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

use std::collections::BTreeMap;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use uuid::Uuid;

use crate::util::now_secs;

/// All persistence errors surface as `sqlx::Error`; callers that need friendlier
/// semantics (e.g. "email already taken") check before writing.
pub type DbError = sqlx::Error;

/// Total carried quantity a character may hold across all items. Storage (the home
/// stash) is the overflow and does **not** count toward this. Gathering stops
/// yielding into a full inventory; depositing frees it.
pub const MAX_CARRY: i64 = 50;

/// Building-skill XP granted per unit contributed to a build order, paid lump-sum to
/// each contributor when the order completes (see [`Db::contribute`]).
pub const BUILD_XP_PER_UNIT: i64 = 5;

/// Crafting-skill XP granted per successful `craft.make` (a flat amount per
/// action, not per output unit — crafting is instant, not a pooled contribution).
pub const CRAFT_XP_PER_CRAFT: i64 = 15;

/// An account row (the login identity). One human, one account.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub pw_hash: String,
    /// `"player"` (default) or `"mayor"`. The mayor may commission city build orders
    /// on city-owned land via `mayor.build_create`; everyone else cannot.
    pub role: String,
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

// --- In-transaction item helpers ---------------------------------------------
// Shared by the inventory/storage methods so deposit/withdraw move both sides in
// a single transaction. Each treats a character's holdings of an item as one
// collapsed stack (the M2 model is a total-quantity carry cap, not per-slot).

type Tx<'a> = sqlx::Transaction<'a, sqlx::Sqlite>;

/// Parse a build-order cost/progress blob (`{"wood":20,"stone":10}`) into a sorted
/// `item -> qty` map. Malformed or non-integer entries are skipped, so a bad blob
/// degrades to "no cost" rather than erroring the whole transaction.
fn parse_cost(json: &str) -> BTreeMap<String, i64> {
    serde_json::from_str::<BTreeMap<String, i64>>(json).unwrap_or_default()
}

/// Serialize an `item -> qty` map back to a cost blob for storage.
fn dump_cost(cost: &BTreeMap<String, i64>) -> String {
    serde_json::to_string(cost).unwrap_or_else(|_| "{}".to_string())
}

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

/// Advance `p`'s paid-through/due dates by one rent period, restore `active`
/// state (clearing a lapse), and clear the `warned` flag for the new cycle.
/// Shared by [`Db::pay_rent`] (no currency check) and [`Db::pay_rent_with_gold`]
/// (#14), so both extend a plot identically once payment is otherwise settled.
async fn pay_rent_in_tx(tx: &mut Tx<'_>, mut p: Plot, rent_period_secs: i64, now: i64) -> Result<Plot, DbError> {
    // Extend from the later of "now" and the existing paid-through, so paying
    // early stacks time rather than losing it.
    let base = p.rent_paid_through.unwrap_or(now).max(now);
    let paid_through = base;
    let due = base + rent_period_secs;
    sqlx::query(
        "UPDATE plot SET rent_paid_through = ?, rent_due_at = ?, state = 'active', warned = 0 WHERE id = ?",
    )
    .bind(paid_through)
    .bind(due)
    .bind(&p.id)
    .execute(&mut **tx)
    .await?;
    p.rent_paid_through = Some(paid_through);
    p.rent_due_at = Some(due);
    p.state = "active".to_string();
    p.warned = false;
    Ok(p)
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
        sqlx::query_as::<_, Account>("SELECT id, email, pw_hash, role FROM account WHERE email = ?")
            .bind(email)
            .fetch_optional(&self.pool)
            .await
    }

    /// An account's role (`"player"` or `"mayor"`), by its id.
    pub async fn role_for_account(&self, account_id: &str) -> Result<String, DbError> {
        sqlx::query_scalar("SELECT role FROM account WHERE id = ?")
            .bind(account_id)
            .fetch_one(&self.pool)
            .await
    }

    /// Idempotently seed the one mayor account (by email) with `role = 'mayor'`, so
    /// there's always a known login that can commission city build orders. A no-op
    /// if the email is already registered (never overwrites an existing account).
    pub async fn seed_mayor_account(
        &self,
        email: &str,
        pw_hash: &str,
        name: &str,
        x: i64,
        y: i64,
        hp: i64,
        now: i64,
    ) -> Result<(), DbError> {
        self.seed_account_with_role(email, pw_hash, name, x, y, hp, now, "mayor").await
    }

    /// Idempotently seed one account+character with an elevated `role` —
    /// shared by the mayor (city build orders) and editor (terrain editing,
    /// epic #72) boot seeding. A no-op if the email is already registered
    /// (never overwrites an existing account).
    #[allow(clippy::too_many_arguments)]
    pub async fn seed_account_with_role(
        &self,
        email: &str,
        pw_hash: &str,
        name: &str,
        x: i64,
        y: i64,
        hp: i64,
        now: i64,
        role: &str,
    ) -> Result<(), DbError> {
        if self.find_account_by_email(email).await?.is_some() {
            return Ok(());
        }
        let account_id = Uuid::new_v4().to_string();
        let char_id = Uuid::new_v4().to_string();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO account (id, email, pw_hash, role, created_at, last_login) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&account_id)
        .bind(email)
        .bind(pw_hash)
        .bind(role)
        .bind(now)
        .bind(now)
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
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
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
            Account {
                id: account_id.clone(),
                email: email.to_string(),
                pw_hash: pw_hash.to_string(),
                role: "player".to_string(),
            },
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

    /// A character's current gold balance (#14). Not part of [`Character`] since
    /// nothing besides rent reads it yet — kept as a dedicated scalar lookup to
    /// avoid touching every `Character`-constructing call site for a field only
    /// the rent system needs.
    pub async fn character_gold(&self, character_id: &str) -> Result<i64, DbError> {
        let gold: Option<i64> = sqlx::query_scalar("SELECT gold FROM character WHERE id = ?")
            .bind(character_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(gold.unwrap_or(0))
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

/// The outcome of a [`Db::grant_skill_xp`] call: the updated skill and whether the
/// grant crossed a level boundary (so the caller can fire a `skill.levelup`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillGain {
    pub skill: Skill,
    /// True when this grant raised the cached level (a new level was reached).
    pub leveled_up: bool,
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
    /// Whether the ticker should try to auto-deduct gold when rent comes due,
    /// rather than requiring an explicit `rent.pay` (#14; opt-in, default off).
    pub auto_pay: bool,
    /// Whether `rent.warning` has already been sent for the *current* due cycle
    /// (cleared whenever rent is paid) — keeps the ticker from re-warning every
    /// tick within the warning window.
    pub warned: bool,
}

/// One row of a district's plot roster (#18): just enough to place the plot
/// and show who (if anyone) owns it — not the full `Plot` (rent/state detail
/// stays a rent-status/own-plot-only concern, out of scope for a roster
/// everyone in the district can see).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PlotRosterRow {
    pub id: String,
    pub owner_character_id: Option<String>,
    pub owner_name: Option<String>,
    pub grid_x: i64,
    pub grid_y: i64,
    pub w: i64,
    pub h: i64,
    pub tier: i64,
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

/// A placed world prop (player-attributes epic #83, issue #85): editor-authored,
/// world-scoped, with gameplay meaning (first kind: `poison_tree`). Unlike
/// [`Structure`] it belongs to no plot and no owner — `author` is provenance
/// (terrain_delta's AuthorId string form), not ownership.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct WorldObject {
    pub id: String,
    pub kind: String,
    pub x: i32,
    pub y: i32,
    pub author: String,
    pub created_at: i64,
}

/// A décor item. Flair is owned by the *character*, not the plot — `plot_id` is
/// `NULL` while unattached (e.g. after a rent reclaim rehomes it, #14) so it's
/// never destroyed, only detached from land the character no longer holds.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Flair {
    pub id: String,
    pub owner_character_id: String,
    pub plot_id: Option<String>,
    pub item_id: String,
    pub x: i64,
    pub y: i64,
    pub rot: i64,
}

/// The outcome of a [`Db::contribute`] call: what moved, the order's cost/progress
/// after it, and — when this contribution completed the order — the contributors to
/// pay building XP to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContributeResult {
    /// Units actually moved from carried inventory into the order.
    pub moved: i64,
    /// The order's required costs (`item -> qty`).
    pub required: BTreeMap<String, i64>,
    /// The order's progress after this contribution (`item -> qty`).
    pub progress: BTreeMap<String, i64>,
    /// The order's kind (for the gateway's unlock lookup).
    pub kind: String,
    /// The order's district.
    pub district: String,
    /// Whether this contribution completed the order.
    pub completed: bool,
    /// On completion, `(character_id, total_units)` for each contributor (for lump-sum
    /// building XP). Empty otherwise.
    pub contributors: Vec<(String, i64)>,
    /// The completed order's own placement, if it carried one (copied from the row so
    /// the gateway can spawn the structure without a second query).
    pub placement: Option<BuildPlacement>,
}

/// Where a build order's structure appears on completion, and what kind it is.
/// `x1`/`y1` are set only for a segment-shaped structure (e.g. a road), with
/// `x`/`y` as its start point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlacement {
    pub structure_kind: String,
    pub x: i64,
    pub y: i64,
    pub x1: Option<i64>,
    pub y1: Option<i64>,
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
    /// The skill a contributor must have levelled to contribute, e.g. `"building"`.
    /// `None`/level 0 means ungated. Enforcement is per contributor (skills are
    /// per-character); the client greys the order for players below the threshold.
    pub required_skill: Option<String>,
    pub required_level: i64,
    /// This order's own placement (e.g. commissioned at runtime by the mayor), if
    /// any. `None` for orders spawning no structure or relying on authored content.
    pub structure_kind: Option<String>,
    pub x: Option<i64>,
    pub y: Option<i64>,
    pub x1: Option<i64>,
    pub y1: Option<i64>,
}

impl BuildOrder {
    /// This order's placement, if it carries one (`structure_kind` + `x`/`y` all set).
    pub fn placement(&self) -> Option<BuildPlacement> {
        Some(BuildPlacement {
            structure_kind: self.structure_kind.clone()?,
            x: self.x?,
            y: self.y?,
            x1: self.x1,
            y1: self.y1,
        })
    }
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
    ) -> Result<SkillGain, DbError> {
        let mut tx = self.pool.begin().await?;
        let current: i64 = sqlx::query_scalar(
            "SELECT xp FROM skill WHERE character_id = ? AND skill_id = ?",
        )
        .bind(character_id)
        .bind(skill_id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(0);
        let previous_level = level_for_xp(current);
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
        Ok(SkillGain {
            skill: Skill {
                character_id: character_id.to_string(),
                skill_id: skill_id.to_string(),
                xp,
                level,
            },
            leveled_up: level > previous_level,
        })
    }

    /// The current cached level of a character's skill (0 if the skill row is absent).
    pub async fn skill_level(&self, character_id: &str, skill_id: &str) -> Result<i64, DbError> {
        let xp: Option<i64> = sqlx::query_scalar(
            "SELECT xp FROM skill WHERE character_id = ? AND skill_id = ?",
        )
        .bind(character_id)
        .bind(skill_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(level_for_xp(xp.unwrap_or(0)))
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
            auto_pay: false,
            warned: false,
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

    /// Every plot in `district`, owned or not, with the owner's display name
    /// resolved — for showing players a district-wide roster (who owns what,
    /// what's still free) rather than just their own plot (#18). A `LEFT JOIN`
    /// (not `JOIN`) so unclaimed plots still appear, with `owner_name: None`.
    /// Free vs. owned is `owner_character_id IS NULL` vs. not — the same rule
    /// `claim_plot`'s free-plot query already uses; a reclaimed plot's `state`
    /// is `"reclaimed"` (not `"unowned"`) but is equally claimable, so `state`
    /// isn't part of the distinction.
    pub async fn plots_for_district(&self, district: &str) -> Result<Vec<PlotRosterRow>, DbError> {
        sqlx::query_as::<_, PlotRosterRow>(
            "SELECT plot.id, plot.owner_character_id, character.name AS owner_name, \
             plot.grid_x, plot.grid_y, plot.w, plot.h, plot.tier \
             FROM plot LEFT JOIN character ON character.id = plot.owner_character_id \
             WHERE plot.district = ?",
        )
        .bind(district)
        .fetch_all(&self.pool)
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
    /// restore `active` state (clearing a lapse). Returns the updated plot. No
    /// currency involved — used by tests/admin tooling; the real player-facing
    /// path is [`Db::pay_rent_with_gold`] (#14).
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
        let Some(p) = plot else {
            tx.commit().await?;
            return Ok(None);
        };
        let updated = pay_rent_in_tx(&mut tx, p, rent_period_secs, now).await?;
        tx.commit().await?;
        Ok(Some(updated))
    }

    /// Pay rent by deducting `cost` gold from `character_id` — only if they own
    /// `plot_id` and can afford it. Atomic: an ownership mismatch or insufficient
    /// balance mutates nothing and returns `None` (#14).
    pub async fn pay_rent_with_gold(
        &self,
        character_id: &str,
        plot_id: &str,
        cost: i64,
        rent_period_secs: i64,
        now: i64,
    ) -> Result<Option<Plot>, DbError> {
        let mut tx = self.pool.begin().await?;
        let plot = sqlx::query_as::<_, Plot>("SELECT * FROM plot WHERE id = ?")
            .bind(plot_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(p) = plot else {
            tx.commit().await?;
            return Ok(None);
        };
        if p.owner_character_id.as_deref() != Some(character_id) {
            tx.commit().await?;
            return Ok(None);
        }
        let gold: i64 = sqlx::query_scalar("SELECT gold FROM character WHERE id = ?")
            .bind(character_id)
            .fetch_one(&mut *tx)
            .await?;
        if gold < cost {
            tx.commit().await?;
            return Ok(None);
        }
        sqlx::query("UPDATE character SET gold = gold - ? WHERE id = ?")
            .bind(cost)
            .bind(character_id)
            .execute(&mut *tx)
            .await?;
        let updated = pay_rent_in_tx(&mut tx, p, rent_period_secs, now).await?;
        tx.commit().await?;
        Ok(Some(updated))
    }

    /// Toggle whether the rent ticker should try to auto-deduct gold for
    /// `plot_id` when it comes due (#14; opt-in, default off). Ownership-checked;
    /// returns `false` (no-op) if `character_id` doesn't own the plot.
    pub async fn set_auto_pay(
        &self,
        character_id: &str,
        plot_id: &str,
        enabled: bool,
    ) -> Result<bool, DbError> {
        let owner: Option<Option<String>> =
            sqlx::query_scalar("SELECT owner_character_id FROM plot WHERE id = ?")
                .bind(plot_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(owner) = owner else { return Ok(false) };
        if owner.as_deref() != Some(character_id) {
            return Ok(false);
        }
        sqlx::query("UPDATE plot SET auto_pay = ? WHERE id = ?")
            .bind(enabled)
            .bind(plot_id)
            .execute(&self.pool)
            .await?;
        Ok(true)
    }

    /// Mark that `rent.warning` has been sent for a plot's current due cycle, so
    /// the ticker doesn't re-send it every tick within the warning window (#14).
    /// Cleared automatically whenever rent is paid ([`pay_rent_in_tx`]).
    pub async fn mark_rent_warned(&self, plot_id: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE plot SET warned = 1 WHERE id = ?")
            .bind(plot_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Every owned plot still subject to rent (`active` or `lapsed`) — the
    /// ticker's per-tick source of truth (#14). Cheap: Phase 1 has 24 plots total.
    pub async fn rent_active_plots(&self) -> Result<Vec<Plot>, DbError> {
        sqlx::query_as::<_, Plot>(
            "SELECT * FROM plot WHERE owner_character_id IS NOT NULL AND state IN ('active','lapsed')",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// The gameplay side-effects of a plot reclaiming — call right after
    /// [`Db::apply_rent_tick`] reports `"reclaimed"` (that call owns the pure
    /// state transition: `owner_character_id`/`rent_*` cleared, `state =
    /// 'reclaimed'`). Flair on the plot is **preserved**, just unattached
    /// (`plot_id = NULL`) — it's owned by the character, not the land. Structures
    /// are **deleted** — they belong to the land itself, which is what's being
    /// reclaimed. If the former owner's respawn pointed at one of the deleted
    /// beds, that's cleared too (no dangling reference). Returns the deleted
    /// structure ids, so the gateway can despawn them client-side and drop them
    /// from each zone's proximity cache (#13).
    pub async fn reclaim_plot_belongings(
        &self,
        plot_id: &str,
        former_owner: &str,
    ) -> Result<Vec<String>, DbError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE flair SET plot_id = NULL WHERE plot_id = ?")
            .bind(plot_id)
            .execute(&mut *tx)
            .await?;
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM structure WHERE plot_id = ?")
            .bind(plot_id)
            .fetch_all(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE character SET respawn_structure_id = NULL \
             WHERE id = ? AND respawn_structure_id IN (SELECT id FROM structure WHERE plot_id = ?)",
        )
        .bind(former_owner)
        .bind(plot_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM structure WHERE plot_id = ?")
            .bind(plot_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(ids)
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

    /// Every structure placed on any plot in `district` — every home in the
    /// district, not just one character's — for hydrating a just-joined player
    /// with everyone's already-built homes (#12).
    pub async fn structures_in_district(&self, district: &str) -> Result<Vec<Structure>, DbError> {
        sqlx::query_as::<_, Structure>(
            "SELECT structure.* FROM structure \
             JOIN plot ON plot.id = structure.plot_id \
             WHERE plot.district = ? ORDER BY structure.id",
        )
        .bind(district)
        .fetch_all(&self.pool)
        .await
    }

    /// Craft an item from `inputs` (each `(item_id, qty)`), atomically: only if
    /// carried inventory covers *every* input are they all removed and
    /// `output_qty` of `output_item` added (bounded by remaining carry room, same
    /// as [`Db::add_to_inventory`]); otherwise nothing changes. Returns whether
    /// the craft went through.
    pub async fn craft(
        &self,
        character_id: &str,
        inputs: &[(&str, i64)],
        output_item: &str,
        output_qty: i64,
    ) -> Result<bool, DbError> {
        let mut tx = self.pool.begin().await?;
        for (item_id, qty) in inputs {
            let have: Option<i64> = sqlx::query_scalar(
                "SELECT SUM(qty) FROM inventory_item WHERE character_id = ? AND item_id = ?",
            )
            .bind(character_id)
            .bind(*item_id)
            .fetch_one(&mut *tx)
            .await?;
            if have.unwrap_or(0) < *qty {
                tx.commit().await?;
                return Ok(false);
            }
        }
        for (item_id, qty) in inputs {
            remove_inventory_in_tx(&mut tx, character_id, item_id, *qty).await?;
        }
        add_inventory_in_tx(&mut tx, character_id, output_item, output_qty).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Set (or clear) which structure a character respawns at. `structure_id` is
    /// trusted by the caller to be a `bed`-kind structure the character owns
    /// (#12) — persistence just records the pointer.
    pub async fn set_respawn_structure(
        &self,
        character_id: &str,
        structure_id: Option<&str>,
    ) -> Result<(), DbError> {
        sqlx::query("UPDATE character SET respawn_structure_id = ? WHERE id = ?")
            .bind(structure_id)
            .bind(character_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The world position of a character's respawn structure, if one is set (and
    /// still exists). `None` means "fall back to the default spawn."
    pub async fn respawn_point_for_character(
        &self,
        character_id: &str,
    ) -> Result<Option<(i64, i64)>, DbError> {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT structure.x, structure.y FROM character \
             JOIN structure ON structure.id = character.respawn_structure_id \
             WHERE character.id = ?",
        )
        .bind(character_id)
        .fetch_optional(&self.pool)
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

    pub async fn flair_for_plot(&self, plot_id: &str) -> Result<Vec<Flair>, DbError> {
        sqlx::query_as::<_, Flair>("SELECT * FROM flair WHERE plot_id = ? ORDER BY id")
            .bind(plot_id)
            .fetch_all(&self.pool)
            .await
    }

    /// Every flair a character owns, attached or not (`plot_id` is `NULL` while
    /// unattached — e.g. after a rent reclaim rehomes it, #14). Flair is never
    /// destroyed, so this is the character's full décor collection.
    pub async fn flair_for_character(&self, owner_character_id: &str) -> Result<Vec<Flair>, DbError> {
        sqlx::query_as::<_, Flair>("SELECT * FROM flair WHERE owner_character_id = ? ORDER BY id")
            .bind(owner_character_id)
            .fetch_all(&self.pool)
            .await
    }

    // --- Build orders & resource nodes -----------------------------------

    pub async fn insert_build_order(
        &self,
        district: &str,
        kind: &str,
        required_json: &str,
        state: &str,
        now: i64,
        required_skill: Option<&str>,
        required_level: i64,
        placement: Option<BuildPlacement>,
    ) -> Result<BuildOrder, DbError> {
        let id = Uuid::new_v4().to_string();
        let (structure_kind, x, y, x1, y1) = match &placement {
            Some(p) => (Some(p.structure_kind.as_str()), Some(p.x), Some(p.y), p.x1, p.y1),
            None => (None, None, None, None, None),
        };
        sqlx::query(
            "INSERT INTO build_order \
             (id, district, kind, required_json, progress_json, state, issued_at, required_skill, required_level, \
              structure_kind, x, y, x1, y1) \
             VALUES (?, ?, ?, ?, '{}', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(district)
        .bind(kind)
        .bind(required_json)
        .bind(state)
        .bind(now)
        .bind(required_skill)
        .bind(required_level)
        .bind(structure_kind)
        .bind(x)
        .bind(y)
        .bind(x1)
        .bind(y1)
        .execute(&self.pool)
        .await?;
        Ok(BuildOrder {
            id,
            district: district.to_string(),
            kind: kind.to_string(),
            required_json: required_json.to_string(),
            progress_json: "{}".to_string(),
            state: state.to_string(),
            issued_at: now,
            completed_at: None,
            required_skill: required_skill.map(|s| s.to_string()),
            required_level,
            structure_kind: placement.as_ref().map(|p| p.structure_kind.clone()),
            x: placement.as_ref().map(|p| p.x),
            y: placement.as_ref().map(|p| p.y),
            x1: placement.as_ref().and_then(|p| p.x1),
            y1: placement.as_ref().and_then(|p| p.y1),
        })
    }

    /// Unlock a `locked` build order (a tech-tree dependent) by flipping it to `open`.
    /// Idempotent: returns the now-open order, or `None` if there was no locked order
    /// of that `(district, kind)` (already open/completed, or absent).
    pub async fn open_build_order(
        &self,
        district: &str,
        kind: &str,
    ) -> Result<Option<BuildOrder>, DbError> {
        let mut tx = self.pool.begin().await?;
        let order = sqlx::query_as::<_, BuildOrder>(
            "SELECT * FROM build_order WHERE district = ? AND kind = ? AND state = 'locked' LIMIT 1",
        )
        .bind(district)
        .bind(kind)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(mut o) = order else {
            tx.commit().await?;
            return Ok(None);
        };
        sqlx::query("UPDATE build_order SET state = 'open' WHERE id = ?")
            .bind(&o.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        o.state = "open".to_string();
        Ok(Some(o))
    }

    /// Contribute up to `qty` of `item_id` from a character's carried inventory to an
    /// open build order, in one transaction. The moved amount is bounded by the order's
    /// remaining need for that item **and** what the character actually carries; items
    /// the order doesn't require move nothing. Records the per-character contribution
    /// (for lump-sum building XP on completion). When the last required item is met the
    /// order flips to `completed` and its contributors are returned.
    pub async fn contribute(
        &self,
        character_id: &str,
        order_id: &str,
        item_id: &str,
        qty: i64,
    ) -> Result<ContributeResult, DbError> {
        let mut tx = self.pool.begin().await?;
        let order = sqlx::query_as::<_, BuildOrder>("SELECT * FROM build_order WHERE id = ?")
            .bind(order_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(order) = order else {
            tx.commit().await?;
            return Ok(ContributeResult::default());
        };

        let required = parse_cost(&order.required_json);
        let mut progress = parse_cost(&order.progress_json);
        let mut result = ContributeResult {
            moved: 0,
            required: required.clone(),
            progress: progress.clone(),
            kind: order.kind.clone(),
            district: order.district.clone(),
            completed: false,
            contributors: Vec::new(),
            placement: order.placement(),
        };

        // Only open orders accept contributions; locked/completed ones are a no-op
        // (but still report their required/progress so the client can render them).
        if order.state != "open" || qty <= 0 {
            tx.commit().await?;
            return Ok(result);
        }

        // Skill gate: a contributor below the order's required level moves nothing.
        // Skills are per-character, so this is enforced per contributor here and shown
        // greyed ("requires Building N") on the client for players who can't yet build it.
        if order.required_level > 0 {
            let skill_id = order.required_skill.as_deref().unwrap_or("building");
            let have: i64 = sqlx::query_scalar(
                "SELECT xp FROM skill WHERE character_id = ? AND skill_id = ?",
            )
            .bind(character_id)
            .bind(skill_id)
            .fetch_optional(&mut *tx)
            .await?
            .map(level_for_xp)
            .unwrap_or(0);
            if have < order.required_level {
                tx.commit().await?;
                return Ok(result);
            }
        }

        let need = required
            .get(item_id)
            .copied()
            .unwrap_or(0)
            .saturating_sub(progress.get(item_id).copied().unwrap_or(0))
            .max(0);
        let carried: Option<i64> = sqlx::query_scalar(
            "SELECT SUM(qty) FROM inventory_item WHERE character_id = ? AND item_id = ?",
        )
        .bind(character_id)
        .bind(item_id)
        .fetch_one(&mut *tx)
        .await?;
        let moved = qty.min(need).min(carried.unwrap_or(0)).max(0);

        if moved > 0 {
            remove_inventory_in_tx(&mut tx, character_id, item_id, moved).await?;
            *progress.entry(item_id.to_string()).or_insert(0) += moved;
            sqlx::query("UPDATE build_order SET progress_json = ? WHERE id = ?")
                .bind(dump_cost(&progress))
                .bind(order_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO build_contribution (order_id, character_id, units) VALUES (?, ?, ?) \
                 ON CONFLICT(order_id, character_id) DO UPDATE SET units = units + excluded.units",
            )
            .bind(order_id)
            .bind(character_id)
            .bind(moved)
            .execute(&mut *tx)
            .await?;
        }

        // Completion: every required item met (an order with no requirements never
        // auto-completes here — it isn't part of the authored tree).
        let completed = !required.is_empty()
            && required
                .iter()
                .all(|(k, v)| progress.get(k).copied().unwrap_or(0) >= *v);
        if completed {
            sqlx::query("UPDATE build_order SET state = 'completed', completed_at = ? WHERE id = ?")
                .bind(now_secs())
                .bind(order_id)
                .execute(&mut *tx)
                .await?;
            result.contributors = sqlx::query_as::<_, (String, i64)>(
                "SELECT character_id, units FROM build_contribution WHERE order_id = ? ORDER BY character_id",
            )
            .bind(order_id)
            .fetch_all(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        result.moved = moved;
        result.progress = progress;
        result.completed = completed;
        Ok(result)
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

    /// A single build order by id (e.g. to check its placement before gating a
    /// contribution on proximity to it).
    pub async fn build_order_by_id(&self, id: &str) -> Result<Option<BuildOrder>, DbError> {
        sqlx::query_as::<_, BuildOrder>("SELECT * FROM build_order WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
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

    /// Every build order still accepting contributions, across every district
    /// — an ops counter (#16), not gameplay-scoped like `build_orders_for_district`.
    pub async fn count_open_build_orders(&self) -> Result<i64, DbError> {
        sqlx::query_scalar("SELECT COUNT(*) FROM build_order WHERE state = 'open'")
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
                // Root orders (no prereq) open at boot; tech-tree dependents seed
                // `locked` and are opened when their prerequisite completes.
                let state = if o.prereq.is_none() { "open" } else { "locked" };
                let placement = Some(BuildPlacement {
                    structure_kind: o.structure_kind.to_string(),
                    x: o.structure_x as i64,
                    y: o.structure_y as i64,
                    x1: None,
                    y1: None,
                });
                self.insert_build_order(
                    o.district,
                    o.kind,
                    o.required_json,
                    state,
                    now,
                    o.required_skill,
                    o.required_level,
                    placement,
                )
                .await?;
            }
        }
        Ok(())
    }

    // --- Terrain deltas (terrain-editing epic #72) ----------------------------
    // Hand-authored edits composited over the baked artifact. One row per
    // edited chunk; an unedited chunk has no row (load returns `None`, and the
    // sampler treats that as "compose nothing" — zero cost for the whole world
    // until someone paints).

    /// The chunk's delta record, or `None` if it has never been edited.
    /// `side` is the artifact's corner-samples-per-chunk (`tile_size + 1`,
    /// from the loaded manifest) — the blob format doesn't self-describe it,
    /// same convention as `HeightTile::decode`.
    pub async fn load_terrain_delta(
        &self,
        chunk_tx: i32,
        chunk_ty: i32,
        side: usize,
    ) -> Result<Option<terrain_common::TerrainDelta>, DbError> {
        let row: Option<(i64, String, Option<Vec<u8>>, String, i64)> = sqlx::query_as(
            "SELECT revision, bake_hash, height_delta_blob, author, edited_at
             FROM terrain_delta WHERE chunk_tx = ? AND chunk_ty = ?",
        )
        .bind(chunk_tx)
        .bind(chunk_ty)
        .fetch_optional(&self.pool)
        .await?;
        let Some((revision, bake_hash, blob, author, edited_at)) = row else {
            return Ok(None);
        };
        let height_delta = match blob {
            Some(bytes) => Some(
                terrain_common::SparseHeightDelta::decode(&bytes, side)
                    .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            ),
            None => None,
        };
        let author = author
            .parse::<terrain_common::AuthorId>()
            .map_err(|e| sqlx::Error::Decode(e.into()))?;
        Ok(Some(terrain_common::TerrainDelta {
            chunk_tx,
            chunk_ty,
            bake_hash,
            revision: revision as u64,
            height_delta,
            provenance: terrain_common::Provenance { author, edited_at },
        }))
    }

    /// Upsert a chunk's delta and return the new revision: `1` for a chunk's
    /// first-ever edit, `previous + 1` after that. The revision is computed
    /// in the database (single statement, `RETURNING`), not taken from the
    /// input — callers never coordinate revision numbers themselves, which
    /// is what keeps concurrent editors from silently overwriting each
    /// other's bump. `delta.revision` is ignored on save.
    pub async fn save_terrain_delta(
        &self,
        delta: &terrain_common::TerrainDelta,
    ) -> Result<u64, DbError> {
        let blob = delta.height_delta.as_ref().map(|d| d.encode(1));
        let revision: i64 = sqlx::query_scalar(
            "INSERT INTO terrain_delta (chunk_tx, chunk_ty, revision, bake_hash, height_delta_blob, author, edited_at)
             VALUES (?, ?, 1, ?, ?, ?, ?)
             ON CONFLICT(chunk_tx, chunk_ty) DO UPDATE SET
                 revision = terrain_delta.revision + 1,
                 bake_hash = excluded.bake_hash,
                 height_delta_blob = excluded.height_delta_blob,
                 author = excluded.author,
                 edited_at = excluded.edited_at
             RETURNING revision",
        )
        .bind(delta.chunk_tx)
        .bind(delta.chunk_ty)
        .bind(&delta.bake_hash)
        .bind(blob)
        .bind(delta.provenance.author.to_string())
        .bind(delta.provenance.edited_at)
        .fetch_one(&self.pool)
        .await?;
        Ok(revision as u64)
    }

    /// Append one accepted edit op to the undo log: the op row plus, per
    /// touched `(chunk, block)`, the block's pre-edit raw content (`None` =
    /// the block didn't exist — revert deletes it). One transaction, so a
    /// logged op is always complete.
    pub async fn log_terrain_edit_op(
        &self,
        op_id: &str,
        author: &str,
        brush: &str,
        created_at: i64,
        blocks: &[(i32, i32, i64, Option<Vec<u8>>)],
    ) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO terrain_edit_op (id, author, brush, created_at, reverted) VALUES (?, ?, ?, ?, 0)")
            .bind(op_id)
            .bind(author)
            .bind(brush)
            .bind(created_at)
            .execute(&mut *tx)
            .await?;
        for (chunk_tx, chunk_ty, block_idx, prev) in blocks {
            sqlx::query(
                "INSERT INTO terrain_edit_op_block (op_id, chunk_tx, chunk_ty, block_idx, prev_block) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(op_id)
            .bind(chunk_tx)
            .bind(chunk_ty)
            .bind(block_idx)
            .bind(prev)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Claim an op for revert: atomically flips `reverted` 0 → 1 and returns
    /// its pre-edit block rows, or `None` if the op doesn't exist or was
    /// already reverted (the claim is the double-revert guard — two racing
    /// reverts can't both win the UPDATE).
    pub async fn take_revertable_edit_op(
        &self,
        op_id: &str,
    ) -> Result<Option<Vec<(i32, i32, i64, Option<Vec<u8>>)>>, DbError> {
        let claimed = sqlx::query("UPDATE terrain_edit_op SET reverted = 1 WHERE id = ? AND reverted = 0")
            .bind(op_id)
            .execute(&self.pool)
            .await?;
        if claimed.rows_affected() == 0 {
            return Ok(None);
        }
        let rows: Vec<(i32, i32, i64, Option<Vec<u8>>)> = sqlx::query_as(
            "SELECT chunk_tx, chunk_ty, block_idx, prev_block FROM terrain_edit_op_block WHERE op_id = ?",
        )
        .bind(op_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(Some(rows))
    }

    // --- placed world props (player-attributes epic #83, issue #85) ---------

    /// Persist a newly placed world object (editor `object.place`). The id is
    /// minted here so the caller broadcasts exactly what was stored.
    pub async fn insert_world_object(
        &self,
        kind: &str,
        x: i32,
        y: i32,
        author: &str,
        created_at: i64,
    ) -> Result<WorldObject, DbError> {
        let obj = WorldObject {
            id: Uuid::new_v4().to_string(),
            kind: kind.to_string(),
            x,
            y,
            author: author.to_string(),
            created_at,
        };
        sqlx::query("INSERT INTO world_object (id, kind, x, y, author, created_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&obj.id)
            .bind(&obj.kind)
            .bind(obj.x)
            .bind(obj.y)
            .bind(&obj.author)
            .bind(obj.created_at)
            .execute(&self.pool)
            .await?;
        Ok(obj)
    }

    /// Delete a placed world object (editor `object.delete`). Returns whether
    /// a row was actually removed — `false` means the id didn't exist (e.g.
    /// two editors racing to delete the same tree; only one wins and
    /// broadcasts).
    pub async fn delete_world_object(&self, id: &str) -> Result<bool, DbError> {
        let res = sqlx::query("DELETE FROM world_object WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Every placed world object — the gateway's boot-time cache load.
    pub async fn list_world_objects(&self) -> Result<Vec<WorldObject>, DbError> {
        let rows: Vec<(String, String, i32, i32, String, i64)> =
            sqlx::query_as("SELECT id, kind, x, y, author, created_at FROM world_object ORDER BY created_at, id")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(id, kind, x, y, author, created_at)| WorldObject { id, kind, x, y, author, created_at })
            .collect())
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
        let g = db.grant_skill_xp(&cid, "gathering", 60).await.unwrap();
        assert_eq!((g.skill.xp, g.skill.level), (60, 0));
        assert!(!g.leveled_up, "still level 0");
        let g = db.grant_skill_xp(&cid, "gathering", 50).await.unwrap();
        assert_eq!((g.skill.xp, g.skill.level), (110, 1)); // use-based, no decay
        assert!(g.leveled_up, "crossed into level 1");
        // A further grant that stays within the level does not report a level-up.
        let g = db.grant_skill_xp(&cid, "gathering", 10).await.unwrap();
        assert_eq!(g.skill.level, 1);
        assert!(!g.leveled_up, "no boundary crossed");
        // separate skills are independent
        db.grant_skill_xp(&cid, "building", 400).await.unwrap();
        let skills = db.skills_for_character(&cid).await.unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(db.skill_level(&cid, "building").await.unwrap(), 2);
        assert_eq!(db.skill_level(&cid, "absent").await.unwrap(), 0);
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
    async fn pay_rent_with_gold_is_atomic_and_ownership_checked() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        let other = a_character(&db).await;
        db.insert_unowned_plot("suburbs", 0, 0, 8, 8, 0).await.unwrap();
        let plot = db.claim_plot(&cid, "suburbs", 1000, 0).await.unwrap().unwrap();
        let starting_gold = db.character_gold(&cid).await.unwrap();
        assert_eq!(starting_gold, 500, "the migration's starting balance");

        // Someone else can't pay your rent.
        assert!(db.pay_rent_with_gold(&other, &plot.id, 50, 1000, 100).await.unwrap().is_none());
        assert_eq!(db.character_gold(&cid).await.unwrap(), starting_gold, "no mutation on the wrong owner");

        // More than the balance: no-op, no partial deduction.
        assert!(db.pay_rent_with_gold(&cid, &plot.id, starting_gold + 1, 1000, 100).await.unwrap().is_none());
        assert_eq!(db.character_gold(&cid).await.unwrap(), starting_gold);

        // Lapse it first, so paying also has to clear the lapse + the warned flag.
        db.apply_rent_tick(&plot.id, 1500, 500).await.unwrap();
        db.mark_rent_warned(&plot.id).await.unwrap();
        assert_eq!(db.load_plot(&plot.id).await.unwrap().unwrap().state, "lapsed");

        let paid = db.pay_rent_with_gold(&cid, &plot.id, 50, 1000, 2000).await.unwrap().unwrap();
        assert_eq!(paid.state, "active");
        assert!(!paid.warned, "paying resets the warning flag for the new cycle");
        assert_eq!(paid.rent_due_at, Some(3000));
        assert_eq!(db.character_gold(&cid).await.unwrap(), starting_gold - 50, "cost deducted exactly once");
    }

    #[tokio::test]
    async fn set_auto_pay_is_ownership_checked() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        let other = a_character(&db).await;
        db.insert_unowned_plot("suburbs", 0, 0, 8, 8, 0).await.unwrap();
        let plot = db.claim_plot(&cid, "suburbs", 1000, 0).await.unwrap().unwrap();
        assert!(!plot.auto_pay, "off by default");

        assert!(!db.set_auto_pay(&other, &plot.id, true).await.unwrap(), "not the owner");
        assert!(!db.load_plot(&plot.id).await.unwrap().unwrap().auto_pay);

        assert!(db.set_auto_pay(&cid, &plot.id, true).await.unwrap());
        assert!(db.load_plot(&plot.id).await.unwrap().unwrap().auto_pay);
    }

    #[tokio::test]
    async fn rent_active_plots_only_returns_owned_active_or_lapsed_plots() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        db.insert_unowned_plot("suburbs", 0, 0, 8, 8, 0).await.unwrap();
        db.insert_unowned_plot("suburbs", 1, 0, 8, 8, 0).await.unwrap();
        db.insert_unowned_plot("suburbs", 2, 0, 8, 8, 0).await.unwrap();
        let owned = db.claim_plot(&cid, "suburbs", 1000, 0).await.unwrap().unwrap();

        let active = db.rent_active_plots().await.unwrap();
        assert_eq!(active.len(), 1, "unowned plots aren't subject to rent");
        assert_eq!(active[0].id, owned.id);

        db.apply_rent_tick(&owned.id, 1500, 500).await.unwrap(); // -> lapsed
        assert_eq!(db.rent_active_plots().await.unwrap().len(), 1, "lapsed still counts, until reclaimed");

        db.apply_rent_tick(&owned.id, 3000, 500).await.unwrap(); // -> reclaimed
        assert!(db.rent_active_plots().await.unwrap().is_empty(), "reclaimed drops out (no owner)");
    }

    #[tokio::test]
    async fn reclaim_plot_belongings_preserves_flair_and_clears_structures_and_respawn() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        db.insert_unowned_plot("suburbs", 0, 0, 8, 8, 0).await.unwrap();
        let plot = db.claim_plot(&cid, "suburbs", 1000, 0).await.unwrap().unwrap();
        let bed = db.place_structure(&plot.id, "bed", 2, 3, 0, 100, Some(&cid), "{}").await.unwrap();
        db.place_structure(&plot.id, "storage", 4, 4, 0, 100, Some(&cid), "{}").await.unwrap();
        let flair_id = db.add_flair(&cid, Some(&plot.id), "rug", 1, 1, 0).await.unwrap();
        db.set_respawn_structure(&cid, Some(&bed.id)).await.unwrap();
        db.deposit_to_storage(&cid, "wood", 10).await.unwrap();

        let deleted = db.reclaim_plot_belongings(&plot.id, &cid).await.unwrap();
        assert_eq!(deleted.len(), 2, "both structures are reported as deleted");
        assert!(deleted.contains(&bed.id));

        assert!(db.structures_for_plot(&plot.id).await.unwrap().is_empty(), "structures are gone");
        let flair = db.flair_for_plot(&plot.id).await.unwrap();
        assert!(flair.is_empty(), "no longer attached to the (former) plot");
        // But it isn't destroyed — still exists, owned, just unattached.
        let all_flair = db.flair_for_character(&cid).await.unwrap();
        assert_eq!(all_flair.len(), 1, "flair is preserved, not deleted");
        assert_eq!(all_flair[0].id, flair_id);
        assert_eq!(all_flair[0].plot_id, None);
        assert_eq!(all_flair[0].owner_character_id, cid);

        // The respawn bed was demolished — the dangling reference is cleared.
        assert_eq!(db.respawn_point_for_character(&cid).await.unwrap(), None);

        // Storage (character-global, never plot-scoped — #12/#13) was never touched.
        let stash = db.storage_for_character(&cid).await.unwrap();
        assert_eq!(stash.iter().find(|i| i.item_id == "wood").unwrap().qty, 10);
    }

    /// #16: reclaiming one plot must not disturb a *different* character's plot,
    /// structures, or flair — an isolation check the single-plot-focused reclaim
    /// tests above didn't specifically cover.
    #[tokio::test]
    async fn reclaiming_one_plot_does_not_disturb_another_owners_plot() {
        let (db, _t) = TempDb::open().await;
        let alice = a_character(&db).await;
        let bob = a_character(&db).await;
        db.insert_unowned_plot("suburbs", 0, 0, 8, 8, 0).await.unwrap();
        db.insert_unowned_plot("suburbs", 1, 0, 8, 8, 0).await.unwrap();
        let alice_plot = db.claim_plot(&alice, "suburbs", 1000, 0).await.unwrap().unwrap();
        let bob_plot = db.claim_plot(&bob, "suburbs", 1000, 0).await.unwrap().unwrap();
        assert_ne!(alice_plot.id, bob_plot.id);

        let alice_bed = db.place_structure(&alice_plot.id, "bed", 2, 3, 0, 100, Some(&alice), "{}").await.unwrap();
        let bob_bed = db.place_structure(&bob_plot.id, "bed", 2, 3, 0, 100, Some(&bob), "{}").await.unwrap();
        db.add_flair(&bob, Some(&bob_plot.id), "rug", 1, 1, 0).await.unwrap();
        db.set_respawn_structure(&bob, Some(&bob_bed.id)).await.unwrap();

        // Reclaim only Alice's plot: the pure state-machine transition (as the
        // real ticker would drive it) plus the belongings side-effect.
        db.apply_rent_tick(&alice_plot.id, 1500, 500).await.unwrap(); // -> lapsed
        db.apply_rent_tick(&alice_plot.id, 3000, 500).await.unwrap(); // -> reclaimed
        let deleted = db.reclaim_plot_belongings(&alice_plot.id, &alice).await.unwrap();
        assert_eq!(deleted, vec![alice_bed.id]);

        // Bob's plot, structure, flair, and respawn are all completely untouched.
        let bob_plot_after = db.load_plot(&bob_plot.id).await.unwrap().unwrap();
        assert_eq!(bob_plot_after.owner_character_id.as_deref(), Some(bob.as_str()));
        assert_eq!(bob_plot_after.state, "active");
        let bob_structures = db.structures_for_plot(&bob_plot.id).await.unwrap();
        assert_eq!(bob_structures.len(), 1);
        assert_eq!(bob_structures[0].id, bob_bed.id);
        assert_eq!(db.flair_for_plot(&bob_plot.id).await.unwrap().len(), 1);
        assert_eq!(
            db.respawn_point_for_character(&bob).await.unwrap(),
            Some((2, 3)),
            "Bob's respawn bed is untouched"
        );

        // Alice's plot really is reclaimed, and a third character can claim it.
        assert!(db.plot_for_character(&alice).await.unwrap().is_none());
        let carol = a_character(&db).await;
        let claimed = db.claim_plot(&carol, "suburbs", 1000, 100).await.unwrap().unwrap();
        assert_eq!(claimed.id, alice_plot.id);
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
            .insert_build_order("market", "town_well", r#"{"wood":20}"#, "open", 100, None, 0, None)
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
    async fn craft_is_atomic_and_bounded_by_ingredients() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "wood", 3).await.unwrap();

        // Short one stone: the whole craft is a no-op, wood is untouched.
        let ok = db.craft(&cid, &[("wood", 2), ("stone", 1)], "tool_kit", 1).await.unwrap();
        assert!(!ok, "insufficient ingredients should not craft");
        assert_eq!(
            qty_of(&db.inventory_for_character(&cid).await.unwrap(), "wood"),
            3,
            "a failed craft must not consume any input"
        );

        // Enough wood alone: plank only needs wood.
        let ok = db.craft(&cid, &[("wood", 2)], "plank", 2).await.unwrap();
        assert!(ok);
        let items = db.inventory_for_character(&cid).await.unwrap();
        assert_eq!(qty_of(&items, "wood"), 1, "inputs are debited");
        assert_eq!(qty_of(&items, "plank"), 2, "output is credited");
    }

    #[tokio::test]
    async fn structures_in_district_spans_every_owning_plot() {
        let (db, _t) = TempDb::open().await;
        let cid_a = a_character(&db).await;
        let cid_b = a_character(&db).await;
        let plot_a = db.insert_unowned_plot("suburbs", 0, 0, 8, 8, 0).await.unwrap();
        let plot_b = db.insert_unowned_plot("suburbs", 1, 0, 8, 8, 0).await.unwrap();
        let other_district = db.insert_unowned_plot("market", 0, 0, 8, 8, 0).await.unwrap();
        db.place_structure(&plot_a.id, "bed", 2, 3, 0, 100, Some(&cid_a), "{}").await.unwrap();
        db.place_structure(&plot_b.id, "storage", 4, 4, 0, 100, Some(&cid_b), "{}").await.unwrap();
        db.place_structure(&other_district.id, "bed", 1, 1, 0, 100, Some(&cid_a), "{}").await.unwrap();

        let suburbs = db.structures_in_district("suburbs").await.unwrap();
        assert_eq!(suburbs.len(), 2, "every home in the district, not just one character's");
        assert!(suburbs.iter().any(|s| s.plot_id == plot_a.id));
        assert!(suburbs.iter().any(|s| s.plot_id == plot_b.id));
        assert!(!suburbs.iter().any(|s| s.plot_id == other_district.id));
    }

    #[tokio::test]
    async fn plots_for_district_shows_every_plot_with_owner_name_or_none() {
        let (db, _t) = TempDb::open().await;
        let (_a, alice) = db
            .create_account_with_character(&format!("alice_{}@t.test", Uuid::new_v4().simple()), "h", "Alice", 0, 0, 100)
            .await
            .unwrap();
        // Two suburbs plots (claim_plot picks the lowest grid coord first, so
        // this one goes to Alice) and one in a different district as a control.
        let plot_a = db.insert_unowned_plot("suburbs", 0, 0, 8, 8, 0).await.unwrap();
        let plot_b = db.insert_unowned_plot("suburbs", 1, 0, 8, 8, 0).await.unwrap();
        let other_district = db.insert_unowned_plot("market", 0, 0, 8, 8, 0).await.unwrap();
        db.claim_plot(&alice.id, "suburbs", 1000, 500).await.unwrap();

        let roster = db.plots_for_district("suburbs").await.unwrap();
        assert_eq!(roster.len(), 2, "every suburbs plot, claimed or not");
        assert!(!roster.iter().any(|p| p.id == other_district.id), "other districts excluded");

        let mine = roster.iter().find(|p| p.id == plot_a.id).expect("the claimed plot appears");
        assert_eq!(mine.owner_character_id.as_deref(), Some(alice.id.as_str()));
        assert_eq!(mine.owner_name.as_deref(), Some("Alice"), "owner name resolved via the join");

        let free = roster.iter().find(|p| p.id == plot_b.id).expect("the still-free plot appears");
        assert_eq!(free.owner_character_id, None);
        assert_eq!(free.owner_name, None, "unclaimed plots have no owner name");
    }

    #[tokio::test]
    async fn respawn_structure_resolves_to_its_position_or_none() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        assert_eq!(db.respawn_point_for_character(&cid).await.unwrap(), None, "no bed set yet");

        let plot = db.insert_unowned_plot("suburbs", 0, 0, 8, 8, 0).await.unwrap();
        let bed = db.place_structure(&plot.id, "bed", 12, 34, 0, 100, Some(&cid), "{}").await.unwrap();
        db.set_respawn_structure(&cid, Some(&bed.id)).await.unwrap();
        assert_eq!(db.respawn_point_for_character(&cid).await.unwrap(), Some((12, 34)));

        db.set_respawn_structure(&cid, None).await.unwrap();
        assert_eq!(db.respawn_point_for_character(&cid).await.unwrap(), None, "clearing it falls back to no bed");
    }

    #[tokio::test]
    async fn seed_capital_is_idempotent_and_claimable() {
        let (db, _t) = TempDb::open().await;
        let cap = crate::world::capital();

        db.seed_capital(&cap, 100).await.unwrap();
        let plots = db.plot_count().await.unwrap();
        assert_eq!(plots, cap.starter_plots().len() as i64);
        // No build orders are authored — city work is commissioned at runtime.
        assert!(db.build_orders_for_district("civic").await.unwrap().is_empty());

        // Re-seed (simulating a restart): no duplicate plots.
        db.seed_capital(&cap, 200).await.unwrap();
        assert_eq!(db.plot_count().await.unwrap(), plots);

        // A fresh character can claim one of the seeded starter plots.
        let cid = a_character(&db).await;
        let claimed = db.claim_plot(&cid, "suburbs", 3600, 300).await.unwrap();
        assert!(claimed.is_some(), "a seeded starter plot should be claimable");
    }

    /// A build order pools contributions from multiple characters, bounds each move by
    /// the remaining need and what's carried, and completes when the last item is met —
    /// returning every contributor for lump-sum XP.
    #[tokio::test]
    async fn build_order_pools_contributions_and_completes() {
        let (db, _t) = TempDb::open().await;
        let order = db
            .insert_build_order("civic", "town_well", r#"{"wood":20,"stone":10}"#, "open", 0, None, 0, None)
            .await
            .unwrap();

        let a = a_character(&db).await;
        let b = a_character(&db).await;
        db.add_to_inventory(&a, "wood", 30).await.unwrap();
        db.add_to_inventory(&b, "wood", 5).await.unwrap();
        db.add_to_inventory(&b, "stone", 20).await.unwrap();

        // A contributes wood: bounded by the order's need (20), not the 30 carried.
        let r = db.contribute(&a, &order.id, "wood", 30).await.unwrap();
        assert_eq!(r.moved, 20, "capped at the wood requirement");
        assert!(!r.completed, "stone still outstanding");
        assert_eq!(r.progress.get("wood"), Some(&20));
        assert_eq!(db.inventory_total(&a).await.unwrap(), 10, "unspent wood stays carried");

        // Wood is already met: a further wood contribution moves nothing.
        assert_eq!(db.contribute(&b, &order.id, "wood", 5).await.unwrap().moved, 0);

        // B finishes the stone (bounded to the 10 needed) → completes the order.
        let done = db.contribute(&b, &order.id, "stone", 20).await.unwrap();
        assert_eq!(done.moved, 10);
        assert!(done.completed, "the last required item completes the order");
        // Both contributors are reported, keyed for XP, with their total units.
        let by: std::collections::HashMap<_, _> = done.contributors.iter().cloned().collect();
        assert_eq!(by.get(&a), Some(&20));
        assert_eq!(by.get(&b), Some(&10));

        // The order is now completed and no longer accepts contributions.
        let after = db.build_orders_for_district("civic").await.unwrap();
        let well = after.iter().find(|o| o.id == order.id).unwrap();
        assert_eq!(well.state, "completed");
        assert!(well.completed_at.is_some());
        assert_eq!(db.contribute(&a, &order.id, "stone", 1).await.unwrap().moved, 0);
    }

    #[tokio::test]
    async fn open_build_order_unlocks_a_locked_dependent() {
        let (db, _t) = TempDb::open().await;
        db.insert_build_order("civic", "wall_section", r#"{"stone":30}"#, "locked", 0, None, 0, None)
            .await
            .unwrap();
        // Unlock flips it open and returns it; a second call is a no-op.
        let opened = db.open_build_order("civic", "wall_section").await.unwrap().unwrap();
        assert_eq!(opened.state, "open");
        assert!(db.open_build_order("civic", "wall_section").await.unwrap().is_none());
        // A locked order rejects contributions until opened.
        let locked = db
            .insert_build_order("civic", "market_stall", r#"{"wood":40}"#, "locked", 0, None, 0, None)
            .await
            .unwrap();
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "wood", 40).await.unwrap();
        assert_eq!(db.contribute(&cid, &locked.id, "wood", 40).await.unwrap().moved, 0,
            "a locked order accepts nothing");
    }

    #[tokio::test]
    async fn skill_gated_order_rejects_until_the_threshold_is_reached() {
        let (db, _t) = TempDb::open().await;
        // An open order that still requires Building 1 to contribute to.
        let order = db
            .insert_build_order("civic", "watchtower", r#"{"wood":30}"#, "open", 0, Some("building"), 1, None)
            .await
            .unwrap();
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "wood", 30).await.unwrap();

        // Below the threshold (Building 0): the gate rejects, nothing moves, the wood
        // stays carried, and the order does not complete.
        let r = db.contribute(&cid, &order.id, "wood", 30).await.unwrap();
        assert_eq!(r.moved, 0, "greyed order accepts nothing below its skill threshold");
        assert!(!r.completed);
        assert_eq!(db.inventory_total(&cid).await.unwrap(), 30, "wood untouched");

        // Reach Building 1, then the same contribution succeeds and completes it.
        db.grant_skill_xp(&cid, "building", 100).await.unwrap();
        assert_eq!(db.skill_level(&cid, "building").await.unwrap(), 1);
        let r = db.contribute(&cid, &order.id, "wood", 30).await.unwrap();
        assert_eq!(r.moved, 30, "the threshold un-greys the order");
        assert!(r.completed);
    }

    // --- Terrain deltas (#74) --------------------------------------------------

    /// Production-shaped corner-grid side (tile_size 128 + 1).
    const DELTA_SIDE: usize = 129;

    fn a_delta(tx: i32, ty: i32) -> terrain_common::TerrainDelta {
        let mut d = terrain_common::SparseHeightDelta::new(DELTA_SIDE);
        d.set_offset_cm(3, 3, 250);
        d.set_offset_cm(40, 90, -775); // second block, negative offset
        d.set_offset_cm(128, 128, 42); // partial edge block
        terrain_common::TerrainDelta {
            chunk_tx: tx,
            chunk_ty: ty,
            bake_hash: "test-bake-hash".to_string(),
            revision: 0, // ignored on save — the DB assigns
            height_delta: Some(d),
            provenance: terrain_common::Provenance {
                author: terrain_common::AuthorId::Editor("acct-e1".to_string()),
                edited_at: 1_700_000_000,
            },
        }
    }

    #[tokio::test]
    async fn terrain_delta_saves_and_loads_round_trip() {
        let (db, _t) = TempDb::open().await;
        let delta = a_delta(2, 7);
        let rev = db.save_terrain_delta(&delta).await.unwrap();
        assert_eq!(rev, 1, "first-ever save of a chunk is revision 1");

        let loaded = db.load_terrain_delta(2, 7, DELTA_SIDE).await.unwrap().expect("row exists");
        assert_eq!(loaded.revision, 1);
        assert_eq!(loaded.bake_hash, "test-bake-hash");
        assert_eq!(loaded.provenance, delta.provenance);
        let hd = loaded.height_delta.expect("height layer present");
        assert_eq!(hd.offset_cm(3, 3), 250);
        assert_eq!(hd.offset_cm(40, 90), -775);
        assert_eq!(hd.offset_cm(128, 128), 42);
        assert_eq!(hd.offset_cm(0, 0), 0, "untouched corner stays zero");
        assert_eq!(hd.touched_block_count(), 3);
    }

    #[tokio::test]
    async fn terrain_delta_upsert_bumps_revision_per_chunk_independently() {
        let (db, _t) = TempDb::open().await;
        db.save_terrain_delta(&a_delta(0, 0)).await.unwrap();
        let rev2 = db.save_terrain_delta(&a_delta(0, 0)).await.unwrap();
        assert_eq!(rev2, 2, "second save of the same chunk bumps its revision");
        let other = db.save_terrain_delta(&a_delta(5, 5)).await.unwrap();
        assert_eq!(other, 1, "a different chunk starts its own revision sequence");
        assert_eq!(db.load_terrain_delta(0, 0, DELTA_SIDE).await.unwrap().unwrap().revision, 2);
    }

    #[tokio::test]
    async fn terrain_delta_never_edited_chunk_loads_none() {
        let (db, _t) = TempDb::open().await;
        assert!(db.load_terrain_delta(9, 9, DELTA_SIDE).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn terrain_delta_null_blob_round_trips_as_no_height_layer() {
        let (db, _t) = TempDb::open().await;
        let mut delta = a_delta(1, 1);
        delta.height_delta = None;
        db.save_terrain_delta(&delta).await.unwrap();
        let loaded = db.load_terrain_delta(1, 1, DELTA_SIDE).await.unwrap().unwrap();
        assert!(loaded.height_delta.is_none(), "NULL blob must load as None, not an empty delta");
    }

    // --- placed world props (#85) -------------------------------------------

    #[tokio::test]
    async fn world_objects_insert_list_delete_round_trip() {
        let (db, _t) = TempDb::open().await;
        assert!(db.list_world_objects().await.unwrap().is_empty(), "starts empty");

        let a = db.insert_world_object("poison_tree", 100, 200, "editor:e1", 1000).await.unwrap();
        let b = db.insert_world_object("poison_tree", 110, 200, "editor:e1", 1001).await.unwrap();
        assert_ne!(a.id, b.id, "each placement mints its own id");

        let all = db.list_world_objects().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], a, "list returns exactly what insert stored, in placement order");
        assert_eq!(all[1], b);

        assert!(db.delete_world_object(&a.id).await.unwrap(), "deleting an existing object reports true");
        let remaining = db.list_world_objects().await.unwrap();
        assert_eq!(remaining, vec![b], "only the deleted object is gone");
    }

    #[tokio::test]
    async fn world_object_delete_of_missing_id_reports_false() {
        let (db, _t) = TempDb::open().await;
        assert!(
            !db.delete_world_object("no-such-id").await.unwrap(),
            "a losing racer's delete must report false (it must not broadcast)"
        );
    }
}
