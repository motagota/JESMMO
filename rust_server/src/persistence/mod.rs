//! Durable persistence layer.
//!
//! SQLite for dev (bundled, zero-setup), but every query is a runtime query
//! against `sqlx`, so swapping to Postgres for staging/prod is a connection-string
//! and driver-feature change — not a rewrite. This module is the *only* place that
//! talks SQL; the rest of the server calls typed repository methods.
//!
//! M0 scope: accounts + a single character per account, with enough character
//! state (position, hp) to demonstrate that logging out and back in — even across
//! a full server restart — restores the player exactly. Gameplay tables (plots,
//! skills, inventory, build orders, rent) land in later milestones.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use uuid::Uuid;

/// All persistence errors surface as `sqlx::Error`; callers that need friendlier
/// semantics (e.g. "email already taken") check before writing.
pub type DbError = sqlx::Error;

/// Total carried quantity a character may hold across all items. Storage (the home
/// stash) is the overflow and does **not** count toward this. Gathering stops
/// yielding into a full inventory; depositing frees it.
pub const MAX_CARRY: i64 = 50;

/// An account row (the login identity). One human, one account.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub pw_hash: String,
}

/// A character row (the in-world entity). One per account in Phase 1. Its `id` is
/// the durable entity id used everywhere the gateway previously used a random UUID.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Character {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub x: i64,
    pub y: i64,
    pub hp: i64,
    pub district: String,
}

pub struct Db {
    pool: SqlitePool,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// --- In-transaction item helpers ---------------------------------------------
// Shared by the inventory/storage methods so deposit/withdraw move both sides in
// a single transaction. Each treats a character's holdings of an item as one
// collapsed stack (the M2 model is a total-quantity carry cap, not per-slot).

type Tx<'a> = sqlx::Transaction<'a, sqlx::Sqlite>;

async fn add_inventory_in_tx(tx: &mut Tx<'_>, character_id: &str, item_id: &str, qty: i64) -> Result<(), DbError> {
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM inventory_item WHERE character_id = ? AND item_id = ? ORDER BY id LIMIT 1",
    )
    .bind(character_id).bind(item_id).fetch_optional(&mut **tx).await?;
    match existing {
        Some(id) => {
            sqlx::query("UPDATE inventory_item SET qty = qty + ? WHERE id = ?")
                .bind(qty).bind(&id).execute(&mut **tx).await?;
        }
        None => {
            sqlx::query("INSERT INTO inventory_item (id, character_id, item_id, qty, slot) VALUES (?, ?, ?, ?, NULL)")
                .bind(Uuid::new_v4().to_string()).bind(character_id).bind(item_id).bind(qty)
                .execute(&mut **tx).await?;
        }
    }
    Ok(())
}

async fn remove_inventory_in_tx(tx: &mut Tx<'_>, character_id: &str, item_id: &str, qty: i64) -> Result<i64, DbError> {
    let cur: Option<i64> = sqlx::query_scalar(
        "SELECT SUM(qty) FROM inventory_item WHERE character_id = ? AND item_id = ?",
    )
    .bind(character_id).bind(item_id).fetch_one(&mut **tx).await?;
    let cur = cur.unwrap_or(0);
    let take = qty.min(cur).max(0);
    if take > 0 {
        sqlx::query("DELETE FROM inventory_item WHERE character_id = ? AND item_id = ?")
            .bind(character_id).bind(item_id).execute(&mut **tx).await?;
        let remaining = cur - take;
        if remaining > 0 {
            sqlx::query("INSERT INTO inventory_item (id, character_id, item_id, qty, slot) VALUES (?, ?, ?, ?, NULL)")
                .bind(Uuid::new_v4().to_string()).bind(character_id).bind(item_id).bind(remaining)
                .execute(&mut **tx).await?;
        }
    }
    Ok(take)
}

async fn add_storage_in_tx(tx: &mut Tx<'_>, character_id: &str, item_id: &str, qty: i64) -> Result<(), DbError> {
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM storage_item WHERE character_id = ? AND item_id = ? LIMIT 1",
    )
    .bind(character_id).bind(item_id).fetch_optional(&mut **tx).await?;
    match existing {
        Some(id) => {
            sqlx::query("UPDATE storage_item SET qty = qty + ? WHERE id = ?")
                .bind(qty).bind(&id).execute(&mut **tx).await?;
        }
        None => {
            sqlx::query("INSERT INTO storage_item (id, character_id, item_id, qty) VALUES (?, ?, ?, ?)")
                .bind(Uuid::new_v4().to_string()).bind(character_id).bind(item_id).bind(qty)
                .execute(&mut **tx).await?;
        }
    }
    Ok(())
}

async fn remove_storage_in_tx(tx: &mut Tx<'_>, character_id: &str, item_id: &str, qty: i64) -> Result<i64, DbError> {
    let cur: Option<i64> = sqlx::query_scalar(
        "SELECT SUM(qty) FROM storage_item WHERE character_id = ? AND item_id = ?",
    )
    .bind(character_id).bind(item_id).fetch_one(&mut **tx).await?;
    let cur = cur.unwrap_or(0);
    let take = qty.min(cur).max(0);
    if take > 0 {
        sqlx::query("DELETE FROM storage_item WHERE character_id = ? AND item_id = ?")
            .bind(character_id).bind(item_id).execute(&mut **tx).await?;
        let remaining = cur - take;
        if remaining > 0 {
            sqlx::query("INSERT INTO storage_item (id, character_id, item_id, qty) VALUES (?, ?, ?, ?)")
                .bind(Uuid::new_v4().to_string()).bind(character_id).bind(item_id).bind(remaining)
                .execute(&mut **tx).await?;
        }
    }
    Ok(take)
}

impl Db {
    /// Open (creating the file if needed) and bring the schema up to date by
    /// running any pending migrations from `./migrations`.
    pub async fn connect(url: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let opts = SqliteConnectOptions::from_str(url)?.create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn find_account_by_email(&self, email: &str) -> Result<Option<Account>, DbError> {
        sqlx::query_as::<_, Account>("SELECT id, email, pw_hash FROM account WHERE email = ?")
            .bind(email)
            .fetch_optional(&self.pool)
            .await
    }

    /// Create an account and its single character in one transaction. Spawn
    /// position is supplied by the caller (the gateway, which owns world geometry).
    pub async fn create_account_with_character(
        &self,
        email: &str,
        pw_hash: &str,
        name: &str,
        x: i64,
        y: i64,
        hp: i64,
    ) -> Result<(Account, Character), DbError> {
        let account_id = Uuid::new_v4().to_string();
        let char_id = Uuid::new_v4().to_string();
        let ts = now_secs();

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO account (id, email, pw_hash, created_at, last_login) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&account_id)
        .bind(email)
        .bind(pw_hash)
        .bind(ts)
        .bind(ts)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO character (id, account_id, name, x, y, hp, district, created_at, last_seen) \
             VALUES (?, ?, ?, ?, ?, ?, '', ?, ?)",
        )
        .bind(&char_id)
        .bind(&account_id)
        .bind(name)
        .bind(x)
        .bind(y)
        .bind(hp)
        .bind(ts)
        .bind(ts)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok((
            Account { id: account_id.clone(), email: email.to_string(), pw_hash: pw_hash.to_string() },
            Character {
                id: char_id,
                account_id,
                name: name.to_string(),
                x,
                y,
                hp,
                district: String::new(),
            },
        ))
    }

    pub async fn character_for_account(&self, account_id: &str) -> Result<Option<Character>, DbError> {
        sqlx::query_as::<_, Character>(
            "SELECT id, account_id, name, x, y, hp, district FROM character WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Look up a character directly by its id (used to resume a session token).
    pub async fn character_by_id(&self, id: &str) -> Result<Option<Character>, DbError> {
        sqlx::query_as::<_, Character>(
            "SELECT id, account_id, name, x, y, hp, district FROM character WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn touch_login(&self, account_id: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE account SET last_login = ? WHERE id = ?")
            .bind(now_secs())
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist a character's latest world state. Called periodically and on logout
    /// so that a restart restores the player where they were.
    pub async fn save_character(
        &self,
        id: &str,
        x: i64,
        y: i64,
        hp: i64,
        district: &str,
    ) -> Result<(), DbError> {
        sqlx::query("UPDATE character SET x = ?, y = ?, hp = ?, district = ?, last_seen = ? WHERE id = ?")
            .bind(x)
            .bind(y)
            .bind(hp)
            .bind(district)
            .bind(now_secs())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Schema v1 gameplay tables (issue #1).
//
// Persistence policy: these repository methods are **write-through** — each
// commits to the DB before returning, so high-value events (claim a plot, place
// a structure, deposit to storage, grant skill xp) are durable the moment they
// succeed. High-frequency state (character position) stays **write-behind** via
// the gateway's periodic flush (see proxy `persistence_flush`). The gameplay
// systems that call these land in later milestones; the durable home for their
// state lands here now (phase1.md §2.1, §6).
// ---------------------------------------------------------------------------

/// A use-based skill row. `level` is derived from `xp` via [`level_for_xp`] and
/// cached for cheap reads.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Skill {
    pub character_id: String,
    pub skill_id: String,
    pub xp: i64,
    pub level: i64,
}

/// A carried inventory item (finite slots).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct InventoryItem {
    pub id: String,
    pub character_id: String,
    pub item_id: String,
    pub qty: i64,
    pub slot: Option<i64>,
}

/// A safe home-stash item (large, unslotted; stacks per item).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StorageItem {
    pub id: String,
    pub character_id: String,
    pub item_id: String,
    pub qty: i64,
}

/// A plot of rented land. `owner_character_id` is `None` while it sits in the
/// pool; `state` is one of `unowned | active | lapsed | reclaimed`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Plot {
    pub id: String,
    pub owner_character_id: Option<String>,
    pub district: String,
    pub grid_x: i64,
    pub grid_y: i64,
    pub w: i64,
    pub h: i64,
    pub tier: i64,
    pub rent_due_at: Option<i64>,
    pub rent_paid_through: Option<i64>,
    pub state: String,
}

/// A player-built structure, owned via its plot.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Structure {
    pub id: String,
    pub plot_id: String,
    pub kind: String,
    pub x: i64,
    pub y: i64,
    pub rot: i64,
    pub hp: i64,
    pub built_by: Option<String>,
    pub data: String,
}

/// A district-scoped city build quest.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BuildOrder {
    pub id: String,
    pub district: String,
    pub kind: String,
    pub required_json: String,
    pub progress_json: String,
    pub state: String,
    pub issued_at: i64,
    pub completed_at: Option<i64>,
}

/// A gatherable resource node.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ResourceNode {
    pub id: String,
    pub district: String,
    pub item_id: String,
    pub x: i64,
    pub y: i64,
    pub qty: i64,
    pub respawn_at: Option<i64>,
}

/// The fixed XP → level curve. Deliberately simple and monotonic (level `n` at
/// `100 * n²` xp): 0 xp = L0, 100 = L1, 400 = L2, 900 = L3. Gameplay can refine
/// the constants later; persistence only needs a single deterministic source of
/// truth so the cached `skill.level` always agrees with `skill.xp`.
pub fn level_for_xp(xp: i64) -> i64 {
    if xp <= 0 {
        return 0;
    }
    ((xp as f64 / 100.0).sqrt()).floor() as i64
}

impl Db {
    // --- Skills -----------------------------------------------------------

    /// Add `amount` xp to a character's skill (creating the row on first use) and
    /// recompute the cached level. Returns the updated skill. Idempotent per call
    /// in the sense that it's a pure increment; callers grant fixed amounts.
    pub async fn grant_skill_xp(
        &self,
        character_id: &str,
        skill_id: &str,
        amount: i64,
    ) -> Result<Skill, DbError> {
        let mut tx = self.pool.begin().await?;
        let current: i64 = sqlx::query_scalar(
            "SELECT xp FROM skill WHERE character_id = ? AND skill_id = ?",
        )
        .bind(character_id)
        .bind(skill_id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(0);
        let xp = (current + amount).max(0);
        let level = level_for_xp(xp);
        sqlx::query(
            "INSERT INTO skill (character_id, skill_id, xp, level) VALUES (?, ?, ?, ?) \
             ON CONFLICT(character_id, skill_id) DO UPDATE SET xp = excluded.xp, level = excluded.level",
        )
        .bind(character_id)
        .bind(skill_id)
        .bind(xp)
        .bind(level)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Skill {
            character_id: character_id.to_string(),
            skill_id: skill_id.to_string(),
            xp,
            level,
        })
    }

    pub async fn skills_for_character(&self, character_id: &str) -> Result<Vec<Skill>, DbError> {
        sqlx::query_as::<_, Skill>(
            "SELECT character_id, skill_id, xp, level FROM skill WHERE character_id = ? ORDER BY skill_id",
        )
        .bind(character_id)
        .fetch_all(&self.pool)
        .await
    }

    // --- Inventory & storage ---------------------------------------------

    /// Total carried quantity for a character (storage does not count toward it).
    pub async fn inventory_total(&self, character_id: &str) -> Result<i64, DbError> {
        let total: Option<i64> =
            sqlx::query_scalar("SELECT SUM(qty) FROM inventory_item WHERE character_id = ?")
                .bind(character_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(total.unwrap_or(0))
    }

    /// Add up to `qty` of an item to a character's carried inventory, **bounded by
    /// the carry capacity** [`MAX_CARRY`] (storage is the overflow). Stacks onto the
    /// existing row if present. Returns how many units were actually added — which
    /// may be less than `qty`, or `0` when the inventory is full.
    pub async fn add_to_inventory(
        &self,
        character_id: &str,
        item_id: &str,
        qty: i64,
    ) -> Result<i64, DbError> {
        if qty <= 0 {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let total: Option<i64> =
            sqlx::query_scalar("SELECT SUM(qty) FROM inventory_item WHERE character_id = ?")
                .bind(character_id)
                .fetch_one(&mut *tx)
                .await?;
        let room = (MAX_CARRY - total.unwrap_or(0)).max(0);
        let add = qty.min(room);
        if add > 0 {
            add_inventory_in_tx(&mut tx, character_id, item_id, add).await?;
        }
        tx.commit().await?;
        Ok(add)
    }

    /// Remove up to `qty` of an item from carried inventory. Returns the amount
    /// actually removed.
    pub async fn remove_from_inventory(
        &self,
        character_id: &str,
        item_id: &str,
        qty: i64,
    ) -> Result<i64, DbError> {
        if qty <= 0 {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let removed = remove_inventory_in_tx(&mut tx, character_id, item_id, qty).await?;
        tx.commit().await?;
        Ok(removed)
    }

    /// Deposit up to `qty` of an item from carried inventory into safe storage, in
    /// one transaction. Returns the amount moved (bounded by what's carried).
    pub async fn deposit(
        &self,
        character_id: &str,
        item_id: &str,
        qty: i64,
    ) -> Result<i64, DbError> {
        let mut tx = self.pool.begin().await?;
        let moved = remove_inventory_in_tx(&mut tx, character_id, item_id, qty).await?;
        if moved > 0 {
            add_storage_in_tx(&mut tx, character_id, item_id, moved).await?;
        }
        tx.commit().await?;
        Ok(moved)
    }

    /// Withdraw up to `qty` of an item from storage back into carried inventory, in
    /// one transaction. Bounded by what's stored **and** the remaining carry
    /// capacity. Returns the amount moved.
    pub async fn withdraw(
        &self,
        character_id: &str,
        item_id: &str,
        qty: i64,
    ) -> Result<i64, DbError> {
        if qty <= 0 {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let stored: Option<i64> = sqlx::query_scalar(
            "SELECT SUM(qty) FROM storage_item WHERE character_id = ? AND item_id = ?",
        )
        .bind(character_id)
        .bind(item_id)
        .fetch_one(&mut *tx)
        .await?;
        let carried: Option<i64> =
            sqlx::query_scalar("SELECT SUM(qty) FROM inventory_item WHERE character_id = ?")
                .bind(character_id)
                .fetch_one(&mut *tx)
                .await?;
        let room = (MAX_CARRY - carried.unwrap_or(0)).max(0);
        let moved = qty.min(stored.unwrap_or(0)).min(room);
        if moved > 0 {
            remove_storage_in_tx(&mut tx, character_id, item_id, moved).await?;
            add_inventory_in_tx(&mut tx, character_id, item_id, moved).await?;
        }
        tx.commit().await?;
        Ok(moved)
    }

    pub async fn inventory_for_character(
        &self,
        character_id: &str,
    ) -> Result<Vec<InventoryItem>, DbError> {
        sqlx::query_as::<_, InventoryItem>(
            "SELECT id, character_id, item_id, qty, slot FROM inventory_item \
             WHERE character_id = ? ORDER BY item_id",
        )
        .bind(character_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Move items into the safe home stash, stacking per item. Returns the stack.
    pub async fn deposit_to_storage(
        &self,
        character_id: &str,
        item_id: &str,
        qty: i64,
    ) -> Result<StorageItem, DbError> {
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query_as::<_, StorageItem>(
            "SELECT id, character_id, item_id, qty FROM storage_item \
             WHERE character_id = ? AND item_id = ? LIMIT 1",
        )
        .bind(character_id)
        .bind(item_id)
        .fetch_optional(&mut *tx)
        .await?;
        let row = match existing {
            Some(mut it) => {
                it.qty += qty;
                sqlx::query("UPDATE storage_item SET qty = ? WHERE id = ?")
                    .bind(it.qty)
                    .bind(&it.id)
                    .execute(&mut *tx)
                    .await?;
                it
            }
            None => {
                let id = Uuid::new_v4().to_string();
                sqlx::query(
                    "INSERT INTO storage_item (id, character_id, item_id, qty) VALUES (?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(character_id)
                .bind(item_id)
                .bind(qty)
                .execute(&mut *tx)
                .await?;
                StorageItem {
                    id,
                    character_id: character_id.to_string(),
                    item_id: item_id.to_string(),
                    qty,
                }
            }
        };
        tx.commit().await?;
        Ok(row)
    }

    pub async fn storage_for_character(
        &self,
        character_id: &str,
    ) -> Result<Vec<StorageItem>, DbError> {
        sqlx::query_as::<_, StorageItem>(
            "SELECT id, character_id, item_id, qty FROM storage_item \
             WHERE character_id = ? ORDER BY item_id",
        )
        .bind(character_id)
        .fetch_all(&self.pool)
        .await
    }

    // --- Plots & rent -----------------------------------------------------

    /// Insert an unowned plot into the pool. World authoring pre-seeds the plot
    /// grid this way; exposed here so seeding and tests share one code path.
    pub async fn insert_unowned_plot(
        &self,
        district: &str,
        grid_x: i64,
        grid_y: i64,
        w: i64,
        h: i64,
        tier: i64,
    ) -> Result<Plot, DbError> {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO plot (id, owner_character_id, district, grid_x, grid_y, w, h, tier, \
             rent_due_at, rent_paid_through, state) \
             VALUES (?, NULL, ?, ?, ?, ?, ?, ?, NULL, NULL, 'unowned')",
        )
        .bind(&id)
        .bind(district)
        .bind(grid_x)
        .bind(grid_y)
        .bind(w)
        .bind(h)
        .bind(tier)
        .execute(&self.pool)
        .await?;
        Ok(Plot {
            id,
            owner_character_id: None,
            district: district.to_string(),
            grid_x,
            grid_y,
            w,
            h,
            tier,
            rent_due_at: None,
            rent_paid_through: None,
            state: "unowned".to_string(),
        })
    }

    pub async fn load_plot(&self, plot_id: &str) -> Result<Option<Plot>, DbError> {
        sqlx::query_as::<_, Plot>("SELECT * FROM plot WHERE id = ?")
            .bind(plot_id)
            .fetch_optional(&self.pool)
            .await
    }

    /// The plot a character currently holds (active or lapsed), if any.
    pub async fn plot_for_character(&self, character_id: &str) -> Result<Option<Plot>, DbError> {
        sqlx::query_as::<_, Plot>(
            "SELECT * FROM plot WHERE owner_character_id = ? AND state IN ('active','lapsed') LIMIT 1",
        )
        .bind(character_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Allocate a free plot in `district` to a character. **Idempotent**: if the
    /// character already holds a plot, that plot is returned and nothing new is
    /// granted (so a reconnect can't hand out a second plot). Returns `None` only
    /// when the pool is exhausted. Rent starts paid through `now`, due at
    /// `now + rent_period_secs`.
    pub async fn claim_plot(
        &self,
        character_id: &str,
        district: &str,
        rent_period_secs: i64,
        now: i64,
    ) -> Result<Option<Plot>, DbError> {
        let mut tx = self.pool.begin().await?;

        if let Some(existing) = sqlx::query_as::<_, Plot>(
            "SELECT * FROM plot WHERE owner_character_id = ? AND state IN ('active','lapsed') LIMIT 1",
        )
        .bind(character_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return Ok(Some(existing));
        }

        let free = sqlx::query_as::<_, Plot>(
            "SELECT * FROM plot WHERE district = ? AND owner_character_id IS NULL \
             ORDER BY grid_y, grid_x LIMIT 1",
        )
        .bind(district)
        .fetch_optional(&mut *tx)
        .await?;

        let plot = match free {
            None => {
                tx.commit().await?;
                return Ok(None);
            }
            Some(mut p) => {
                let due = now + rent_period_secs;
                sqlx::query(
                    "UPDATE plot SET owner_character_id = ?, state = 'active', \
                     rent_paid_through = ?, rent_due_at = ? WHERE id = ?",
                )
                .bind(character_id)
                .bind(now)
                .bind(due)
                .bind(&p.id)
                .execute(&mut *tx)
                .await?;
                p.owner_character_id = Some(character_id.to_string());
                p.state = "active".to_string();
                p.rent_paid_through = Some(now);
                p.rent_due_at = Some(due);
                p
            }
        };
        tx.commit().await?;
        Ok(Some(plot))
    }

    /// Pay rent on a plot: advance the paid-through and due dates by one period and
    /// restore `active` state (clearing a lapse). Returns the updated plot.
    pub async fn pay_rent(
        &self,
        plot_id: &str,
        rent_period_secs: i64,
        now: i64,
    ) -> Result<Option<Plot>, DbError> {
        let mut tx = self.pool.begin().await?;
        let plot = sqlx::query_as::<_, Plot>("SELECT * FROM plot WHERE id = ?")
            .bind(plot_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(mut p) = plot else {
            tx.commit().await?;
            return Ok(None);
        };
        // Extend from the later of "now" and the existing paid-through, so paying
        // early stacks time rather than losing it.
        let base = p.rent_paid_through.unwrap_or(now).max(now);
        let paid_through = base;
        let due = base + rent_period_secs;
        sqlx::query(
            "UPDATE plot SET rent_paid_through = ?, rent_due_at = ?, state = 'active' WHERE id = ?",
        )
        .bind(paid_through)
        .bind(due)
        .bind(plot_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        p.rent_paid_through = Some(paid_through);
        p.rent_due_at = Some(due);
        p.state = "active".to_string();
        Ok(Some(p))
    }

    /// Advance a plot's rent state for the current time. `active` → `lapsed` once
    /// past due; `lapsed` → `reclaimed` once past the grace window, at which point
    /// the owner is cleared and the plot returns to the pool (claimable again).
    /// The belongings-to-storage / flair-preservation move that accompanies a real
    /// reclaim is gameplay (issue #14); this owns only the durable state machine.
    /// Returns the resulting `state`.
    pub async fn apply_rent_tick(
        &self,
        plot_id: &str,
        now: i64,
        grace_secs: i64,
    ) -> Result<Option<String>, DbError> {
        let mut tx = self.pool.begin().await?;
        let plot = sqlx::query_as::<_, Plot>("SELECT * FROM plot WHERE id = ?")
            .bind(plot_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(p) = plot else {
            tx.commit().await?;
            return Ok(None);
        };
        let due = p.rent_due_at.unwrap_or(i64::MAX);
        let new_state = match p.state.as_str() {
            "active" if now > due => Some("lapsed"),
            "lapsed" if now > due + grace_secs => Some("reclaimed"),
            _ => None,
        };
        if let Some(state) = new_state {
            if state == "reclaimed" {
                sqlx::query(
                    "UPDATE plot SET state = 'reclaimed', owner_character_id = NULL, \
                     rent_due_at = NULL, rent_paid_through = NULL WHERE id = ?",
                )
                .bind(plot_id)
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query("UPDATE plot SET state = ? WHERE id = ?")
                    .bind(state)
                    .bind(plot_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(Some(new_state.unwrap_or(&p.state).to_string()))
    }

    // --- Structures & flair ----------------------------------------------

    /// Place (persist) a structure on a plot. Bounds/overlap/ownership validation
    /// is the gameplay layer's job (issue #12); this records the durable row.
    pub async fn place_structure(
        &self,
        plot_id: &str,
        kind: &str,
        x: i64,
        y: i64,
        rot: i64,
        hp: i64,
        built_by: Option<&str>,
        data: &str,
    ) -> Result<Structure, DbError> {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO structure (id, plot_id, kind, x, y, rot, hp, built_by, data) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(plot_id)
        .bind(kind)
        .bind(x)
        .bind(y)
        .bind(rot)
        .bind(hp)
        .bind(built_by)
        .bind(data)
        .execute(&self.pool)
        .await?;
        Ok(Structure {
            id,
            plot_id: plot_id.to_string(),
            kind: kind.to_string(),
            x,
            y,
            rot,
            hp,
            built_by: built_by.map(str::to_string),
            data: data.to_string(),
        })
    }

    pub async fn structures_for_plot(&self, plot_id: &str) -> Result<Vec<Structure>, DbError> {
        sqlx::query_as::<_, Structure>("SELECT * FROM structure WHERE plot_id = ? ORDER BY id")
            .bind(plot_id)
            .fetch_all(&self.pool)
            .await
    }

    /// Add a décor item. Flair is owned by the character and survives rent lapse.
    pub async fn add_flair(
        &self,
        owner_character_id: &str,
        plot_id: Option<&str>,
        item_id: &str,
        x: i64,
        y: i64,
        rot: i64,
    ) -> Result<String, DbError> {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO flair (id, owner_character_id, plot_id, item_id, x, y, rot) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(owner_character_id)
        .bind(plot_id)
        .bind(item_id)
        .bind(x)
        .bind(y)
        .bind(rot)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    // --- Build orders & resource nodes -----------------------------------

    pub async fn insert_build_order(
        &self,
        district: &str,
        kind: &str,
        required_json: &str,
        now: i64,
    ) -> Result<BuildOrder, DbError> {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO build_order (id, district, kind, required_json, progress_json, state, issued_at) \
             VALUES (?, ?, ?, ?, '{}', 'open', ?)",
        )
        .bind(&id)
        .bind(district)
        .bind(kind)
        .bind(required_json)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(BuildOrder {
            id,
            district: district.to_string(),
            kind: kind.to_string(),
            required_json: required_json.to_string(),
            progress_json: "{}".to_string(),
            state: "open".to_string(),
            issued_at: now,
            completed_at: None,
        })
    }

    pub async fn build_orders_for_district(
        &self,
        district: &str,
    ) -> Result<Vec<BuildOrder>, DbError> {
        sqlx::query_as::<_, BuildOrder>(
            "SELECT * FROM build_order WHERE district = ? ORDER BY issued_at",
        )
        .bind(district)
        .fetch_all(&self.pool)
        .await
    }

    /// Persist updated contribution progress (and optionally completion) for an order.
    pub async fn save_build_order_progress(
        &self,
        order_id: &str,
        progress_json: &str,
        state: &str,
        completed_at: Option<i64>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE build_order SET progress_json = ?, state = ?, completed_at = ? WHERE id = ?",
        )
        .bind(progress_json)
        .bind(state)
        .bind(completed_at)
        .bind(order_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_resource_node(
        &self,
        district: &str,
        item_id: &str,
        x: i64,
        y: i64,
        qty: i64,
    ) -> Result<ResourceNode, DbError> {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO resource_node (id, district, item_id, x, y, qty, respawn_at) \
             VALUES (?, ?, ?, ?, ?, ?, NULL)",
        )
        .bind(&id)
        .bind(district)
        .bind(item_id)
        .bind(x)
        .bind(y)
        .bind(qty)
        .execute(&self.pool)
        .await?;
        Ok(ResourceNode {
            id,
            district: district.to_string(),
            item_id: item_id.to_string(),
            x,
            y,
            qty,
            respawn_at: None,
        })
    }

    pub async fn resource_nodes_for_district(
        &self,
        district: &str,
    ) -> Result<Vec<ResourceNode>, DbError> {
        sqlx::query_as::<_, ResourceNode>(
            "SELECT * FROM resource_node WHERE district = ? ORDER BY id",
        )
        .bind(district)
        .fetch_all(&self.pool)
        .await
    }

    /// Decrement a node's quantity by `amount` (floored at 0) and set its respawn
    /// time when it empties. Returns the remaining quantity.
    pub async fn deplete_resource_node(
        &self,
        node_id: &str,
        amount: i64,
        respawn_at: i64,
    ) -> Result<i64, DbError> {
        let mut tx = self.pool.begin().await?;
        let qty: i64 = sqlx::query_scalar("SELECT qty FROM resource_node WHERE id = ?")
            .bind(node_id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(0);
        let remaining = (qty - amount).max(0);
        let respawn = if remaining == 0 { Some(respawn_at) } else { None };
        sqlx::query("UPDATE resource_node SET qty = ?, respawn_at = ? WHERE id = ?")
            .bind(remaining)
            .bind(respawn)
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(remaining)
    }

    // --- World seeding ----------------------------------------------------

    pub async fn plot_count(&self) -> Result<i64, DbError> {
        sqlx::query_scalar("SELECT COUNT(*) FROM plot")
            .fetch_one(&self.pool)
            .await
    }

    /// Seed the authored capital into the database: the starter plot grid (as
    /// unowned plots) and the first build orders. **Idempotent** — safe to call on
    /// every boot. Plots seed only when the pool is empty; each build-order kind is
    /// created at most once per district. `now` stamps newly issued orders.
    pub async fn seed_capital(
        &self,
        capital: &crate::world::Capital,
        now: i64,
    ) -> Result<(), DbError> {
        if self.plot_count().await? == 0 {
            for (district, cell) in capital.starter_plots() {
                self.insert_unowned_plot(
                    district,
                    cell.grid_x as i64,
                    cell.grid_y as i64,
                    cell.w as i64,
                    cell.h as i64,
                    cell.tier,
                )
                .await?;
            }
        }
        for o in &capital.build_orders {
            let existing = self.build_orders_for_district(o.district).await?;
            if !existing.iter().any(|b| b.kind == o.kind) {
                self.insert_build_order(o.district, o.kind, o.required_json, now)
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway on-disk sqlite db (so a single pool's connections share state),
    /// cleaned up on drop.
    struct TempDb {
        url: String,
    }
    impl TempDb {
        async fn open() -> (Db, Self) {
            let path = std::env::temp_dir().join(format!("mmo_persist_{}.db", Uuid::new_v4().simple()));
            let url = format!("sqlite://{}", path.to_string_lossy());
            let db = Db::connect(&url).await.expect("connect");
            (db, TempDb { url })
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            let file = self.url.trim_start_matches("sqlite://");
            let _ = std::fs::remove_file(file);
            let _ = std::fs::remove_file(format!("{file}-wal"));
            let _ = std::fs::remove_file(format!("{file}-shm"));
        }
    }

    async fn a_character(db: &Db) -> String {
        let email = format!("c_{}@t.test", Uuid::new_v4().simple());
        let (_a, c) = db
            .create_account_with_character(&email, "h", "Hero", 0, 0, 100)
            .await
            .unwrap();
        c.id
    }

    #[test]
    fn xp_curve_is_monotonic_and_correct() {
        assert_eq!(level_for_xp(0), 0);
        assert_eq!(level_for_xp(99), 0);
        assert_eq!(level_for_xp(100), 1);
        assert_eq!(level_for_xp(399), 1);
        assert_eq!(level_for_xp(400), 2);
        assert_eq!(level_for_xp(900), 3);
        // never decreases
        let mut last = 0;
        for xp in (0..2000).step_by(7) {
            let l = level_for_xp(xp);
            assert!(l >= last);
            last = l;
        }
    }

    #[tokio::test]
    async fn skill_xp_accumulates_and_levels() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        let s = db.grant_skill_xp(&cid, "gathering", 60).await.unwrap();
        assert_eq!((s.xp, s.level), (60, 0));
        let s = db.grant_skill_xp(&cid, "gathering", 50).await.unwrap();
        assert_eq!((s.xp, s.level), (110, 1)); // use-based, no decay
        // separate skills are independent
        db.grant_skill_xp(&cid, "building", 400).await.unwrap();
        let skills = db.skills_for_character(&cid).await.unwrap();
        assert_eq!(skills.len(), 2);
    }

    fn qty_of(items: &[InventoryItem], item: &str) -> i64 {
        items.iter().filter(|i| i.item_id == item).map(|i| i.qty).sum()
    }

    #[tokio::test]
    async fn inventory_stacks_and_caps_at_carry_limit() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        assert_eq!(db.add_to_inventory(&cid, "wood", 3).await.unwrap(), 3);
        assert_eq!(db.add_to_inventory(&cid, "wood", 2).await.unwrap(), 2); // stacks
        let inv = db.inventory_for_character(&cid).await.unwrap();
        assert_eq!(qty_of(&inv, "wood"), 5);
        assert_eq!(db.inventory_total(&cid).await.unwrap(), 5);

        // Fill to MAX_CARRY; further adds are partially then fully rejected.
        let added = db.add_to_inventory(&cid, "stone", 100).await.unwrap();
        assert_eq!(added, MAX_CARRY - 5, "only the remaining room is added");
        assert_eq!(db.inventory_total(&cid).await.unwrap(), MAX_CARRY);
        assert_eq!(db.add_to_inventory(&cid, "wood", 1).await.unwrap(), 0, "full inventory");
    }

    #[tokio::test]
    async fn deposit_frees_capacity_and_withdraw_respects_it() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "wood", MAX_CARRY).await.unwrap(); // carry full
        assert_eq!(db.add_to_inventory(&cid, "stone", 1).await.unwrap(), 0);

        // Deposit moves carried wood into storage (which is uncapped) and frees carry.
        let moved = db.deposit(&cid, "wood", 30).await.unwrap();
        assert_eq!(moved, 30);
        assert_eq!(db.inventory_total(&cid).await.unwrap(), MAX_CARRY - 30);
        assert_eq!(qty_of(&db.inventory_for_character(&cid).await.unwrap(), "wood"), MAX_CARRY - 30);
        let stored = db.storage_for_character(&cid).await.unwrap();
        assert_eq!(stored.iter().find(|s| s.item_id == "wood").unwrap().qty, 30);
        // Now there is room to carry again.
        assert_eq!(db.add_to_inventory(&cid, "stone", 1).await.unwrap(), 1);

        // Withdraw is bounded by remaining carry room: only fills to MAX_CARRY.
        let room = MAX_CARRY - db.inventory_total(&cid).await.unwrap();
        let got = db.withdraw(&cid, "wood", 999).await.unwrap();
        assert_eq!(got, room);
        assert_eq!(db.inventory_total(&cid).await.unwrap(), MAX_CARRY);
        // The rest stays safely in storage.
        assert_eq!(db.storage_for_character(&cid).await.unwrap().iter()
            .find(|s| s.item_id == "wood").unwrap().qty, 30 - room);

        // Depositing more than carried only moves what's there.
        let inv_stone = qty_of(&db.inventory_for_character(&cid).await.unwrap(), "stone");
        assert_eq!(db.deposit(&cid, "stone", 999).await.unwrap(), inv_stone);
    }

    #[tokio::test]
    async fn claim_plot_is_idempotent_and_respects_the_pool() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        db.insert_unowned_plot("suburbs", 0, 0, 8, 8, 0).await.unwrap();
        db.insert_unowned_plot("suburbs", 1, 0, 8, 8, 0).await.unwrap();

        let p1 = db.claim_plot(&cid, "suburbs", 3600, 1000).await.unwrap().unwrap();
        assert_eq!(p1.owner_character_id.as_deref(), Some(cid.as_str()));
        assert_eq!(p1.state, "active");
        assert_eq!((p1.rent_paid_through, p1.rent_due_at), (Some(1000), Some(4600)));

        // Reconnect: same plot, no second grant.
        let p2 = db.claim_plot(&cid, "suburbs", 3600, 9999).await.unwrap().unwrap();
        assert_eq!(p2.id, p1.id);
        assert_eq!(db.plot_for_character(&cid).await.unwrap().unwrap().id, p1.id);
    }

    #[tokio::test]
    async fn rent_lapses_then_reclaims_and_returns_to_pool() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        db.insert_unowned_plot("suburbs", 0, 0, 8, 8, 0).await.unwrap();
        let plot = db.claim_plot(&cid, "suburbs", 1000, 0).await.unwrap().unwrap();
        // due at 1000. Before due: still active.
        assert_eq!(db.apply_rent_tick(&plot.id, 500, 500).await.unwrap().as_deref(), Some("active"));
        // Past due: lapses (grace window begins).
        assert_eq!(db.apply_rent_tick(&plot.id, 1500, 500).await.unwrap().as_deref(), Some("lapsed"));
        // Paying rescues it.
        let paid = db.pay_rent(&plot.id, 1000, 1500).await.unwrap().unwrap();
        assert_eq!(paid.state, "active");
        assert_eq!(paid.rent_due_at, Some(2500));
        // Let it lapse and exceed grace → reclaimed, owner cleared, back in pool.
        db.apply_rent_tick(&plot.id, 3000, 500).await.unwrap(); // -> lapsed
        let st = db.apply_rent_tick(&plot.id, 4000, 500).await.unwrap();
        assert_eq!(st.as_deref(), Some("reclaimed"));
        let reclaimed = db.load_plot(&plot.id).await.unwrap().unwrap();
        assert_eq!(reclaimed.owner_character_id, None);
        assert!(db.plot_for_character(&cid).await.unwrap().is_none());
        // Another character can claim the reclaimed plot.
        let other = a_character(&db).await;
        let p = db.claim_plot(&other, "suburbs", 1000, 5000).await.unwrap().unwrap();
        assert_eq!(p.id, plot.id);
    }

    #[tokio::test]
    async fn structures_build_orders_and_nodes_round_trip() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        let plot = db.insert_unowned_plot("suburbs", 0, 0, 8, 8, 0).await.unwrap();
        let s = db
            .place_structure(&plot.id, "bed", 2, 3, 90, 50, Some(&cid), "{}")
            .await
            .unwrap();
        assert_eq!(s.kind, "bed");
        assert_eq!(db.structures_for_plot(&plot.id).await.unwrap().len(), 1);
        db.add_flair(&cid, Some(&plot.id), "rug", 1, 1, 0).await.unwrap();

        let order = db
            .insert_build_order("market", "town_well", r#"{"wood":20}"#, 100)
            .await
            .unwrap();
        db.save_build_order_progress(&order.id, r#"{"wood":20}"#, "completed", Some(200))
            .await
            .unwrap();
        let orders = db.build_orders_for_district("market").await.unwrap();
        assert_eq!(orders[0].state, "completed");

        let node = db.insert_resource_node("market", "wood", 10, 10, 5).await.unwrap();
        let remaining = db.deplete_resource_node(&node.id, 5, 9999).await.unwrap();
        assert_eq!(remaining, 0);
        let nodes = db.resource_nodes_for_district("market").await.unwrap();
        assert_eq!(nodes[0].respawn_at, Some(9999)); // respawn scheduled on empty
    }

    #[tokio::test]
    async fn seed_capital_is_idempotent_and_claimable() {
        let (db, _t) = TempDb::open().await;
        let cap = crate::world::capital();

        db.seed_capital(&cap, 100).await.unwrap();
        let plots = db.plot_count().await.unwrap();
        assert_eq!(plots, cap.starter_plots().len() as i64);
        assert_eq!(db.build_orders_for_district("civic").await.unwrap().len(), 1);

        // Re-seed (simulating a restart): no duplicate plots or orders.
        db.seed_capital(&cap, 200).await.unwrap();
        assert_eq!(db.plot_count().await.unwrap(), plots);
        assert_eq!(db.build_orders_for_district("civic").await.unwrap().len(), 1);

        // A fresh character can claim one of the seeded starter plots.
        let cid = a_character(&db).await;
        let claimed = db.claim_plot(&cid, "suburbs", 3600, 300).await.unwrap();
        assert!(claimed.is_some(), "a seeded starter plot should be claimable");
    }
}
