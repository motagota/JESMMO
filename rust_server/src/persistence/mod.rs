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
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use uuid::Uuid;

use crate::market_config::MarketConfig;
use crate::util::now_secs;
use crate::world;

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

/// Add `qty` of `item_id` to a character's carried inventory. Tools (#128)
/// are instanced, not stacked — each unit becomes its own fresh-durability
/// row and `qty` here just means "how many separate tools to create" (in
/// practice always 1: every current tool source — craft, NPC grant — hands
/// over exactly one at a time). Ordinary items keep merging onto a single
/// stack row like always.
async fn add_inventory_in_tx(tx: &mut Tx<'_>, character_id: &str, item_id: &str, qty: i64) -> Result<(), DbError> {
    if let Some(max) = world::tool_max_durability(item_id) {
        for _ in 0..qty {
            sqlx::query(
                "INSERT INTO inventory_item (id, character_id, item_id, qty, slot, durability) \
                 VALUES (?, ?, ?, 1, NULL, ?)",
            )
            .bind(Uuid::new_v4().to_string()).bind(character_id).bind(item_id).bind(max)
            .execute(&mut **tx).await?;
        }
        return Ok(());
    }
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

/// Remove up to `qty` of `item_id` from carried inventory. For a tool
/// (#128) this deletes whichever OWNED INSTANCES come first (arbitrary,
/// oldest-first) until `qty` units are gone — fine for the one caller that
/// can reach a tool here (`deposit`, which #128 blocks for tools entirely
/// at the gateway, precisely because "which instance" isn't a meaningful
/// question for a stash slot) — kept generic rather than special-cased so
/// it can't silently corrupt state if that guard is ever bypassed.
/// How many of `item_id` a character holds, inside a caller-owned transaction.
///
/// The in-tx twin of [`Db::inventory_qty`], so a check and the removal it
/// authorises can't be separated by another writer.
async fn inventory_qty_in_tx(
    tx: &mut Tx<'_>,
    character_id: &str,
    item_id: &str,
) -> Result<i64, DbError> {
    let qty: Option<i64> = sqlx::query_scalar(
        "SELECT SUM(qty) FROM inventory_item WHERE character_id = ? AND item_id = ?",
    )
    .bind(character_id)
    .bind(item_id)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    Ok(qty.unwrap_or(0))
}

async fn remove_inventory_in_tx(tx: &mut Tx<'_>, character_id: &str, item_id: &str, qty: i64) -> Result<i64, DbError> {
    if world::tool_max_durability(item_id).is_some() {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM inventory_item WHERE character_id = ? AND item_id = ? ORDER BY id LIMIT ?",
        )
        .bind(character_id).bind(item_id).bind(qty)
        .fetch_all(&mut **tx).await?;
        for id in &ids {
            sqlx::query("DELETE FROM inventory_item WHERE id = ?").bind(id).execute(&mut **tx).await?;
        }
        return Ok(ids.len() as i64);
    }
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

/// Put `qty` of a STACKABLE item into a warehouse as `available`, merging into
/// an existing available stack if there is one and otherwise claiming a fresh
/// slot. Returns how much actually landed — 0 when the warehouse is full and
/// there's nothing to merge into.
///
/// Shared by deposits (#138) and by delivering a purchase (#139), so a bought
/// good obeys exactly the same capacity rule as a deposited one.
async fn warehouse_credit_in_tx(
    tx: &mut Tx<'_>,
    market_id: &str,
    character_id: &str,
    item_id: &str,
    qty: i64,
    slots: i64,
) -> Result<i64, DbError> {
    if qty <= 0 {
        return Ok(0);
    }
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM market_warehouse_item WHERE market_id = ? AND character_id = ? \
         AND item_id = ? AND state = 'available' LIMIT 1",
    )
    .bind(market_id)
    .bind(character_id)
    .bind(item_id)
    .fetch_optional(&mut **tx)
    .await?;
    match existing {
        Some(id) => {
            sqlx::query("UPDATE market_warehouse_item SET qty = qty + ? WHERE id = ?")
                .bind(qty)
                .bind(&id)
                .execute(&mut **tx)
                .await?;
        }
        None => {
            let used: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM market_warehouse_item WHERE market_id = ? AND character_id = ?",
            )
            .bind(market_id)
            .bind(character_id)
            .fetch_one(&mut **tx)
            .await?;
            if used >= slots {
                return Ok(0);
            }
            sqlx::query(
                "INSERT INTO market_warehouse_item \
                 (id, market_id, character_id, item_id, qty, state, durability) \
                 VALUES (?, ?, ?, ?, ?, 'available', NULL)",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(market_id)
            .bind(character_id)
            .bind(item_id)
            .bind(qty)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(qty)
}

/// Move `qty` from `available` to `locked` in a warehouse, inside a
/// caller-owned transaction — escrow against a sell order (#139). Splits a
/// stack when only part of it is being committed; a tool instance is
/// indivisible and moves whole. Returns how much was actually locked, which is
/// bounded by what's available.
async fn warehouse_lock_in_tx(
    tx: &mut Tx<'_>,
    market_id: &str,
    character_id: &str,
    item_id: &str,
    qty: i64,
) -> Result<i64, DbError> {
    if qty <= 0 {
        return Ok(0);
    }
    let rows = sqlx::query_as::<_, WarehouseItem>(
        "SELECT * FROM market_warehouse_item WHERE market_id = ? AND character_id = ? \
         AND item_id = ? AND state = 'available' ORDER BY id",
    )
    .bind(market_id)
    .bind(character_id)
    .bind(item_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut locked = 0i64;
    for r in &rows {
        let want = qty - locked;
        if want <= 0 {
            break;
        }
        if r.durability.is_some() || r.qty <= want {
            sqlx::query("UPDATE market_warehouse_item SET state = 'locked' WHERE id = ?")
                .bind(&r.id)
                .execute(&mut **tx)
                .await?;
            locked += r.qty;
        } else {
            sqlx::query("UPDATE market_warehouse_item SET qty = qty - ? WHERE id = ?")
                .bind(want)
                .bind(&r.id)
                .execute(&mut **tx)
                .await?;
            sqlx::query(
                "INSERT INTO market_warehouse_item \
                 (id, market_id, character_id, item_id, qty, state, durability) \
                 VALUES (?, ?, ?, ?, ?, 'locked', NULL)",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(market_id)
            .bind(character_id)
            .bind(item_id)
            .bind(want)
            .execute(&mut **tx)
            .await?;
            locked += want;
        }
    }
    Ok(locked)
}

/// Take `qty` out of a warehouse's `locked` escrow. With `back_to_available`
/// the goods return to the owner (a cancel or expiry); without it they simply
/// leave — they've been sold, and the buyer was credited separately.
async fn release_locked_in_tx(
    tx: &mut Tx<'_>,
    market_id: &str,
    character_id: &str,
    item_id: &str,
    qty: i64,
    back_to_available: bool,
) -> Result<i64, DbError> {
    if qty <= 0 {
        return Ok(0);
    }
    let rows = sqlx::query_as::<_, WarehouseItem>(
        "SELECT * FROM market_warehouse_item WHERE market_id = ? AND character_id = ? \
         AND item_id = ? AND state = 'locked' ORDER BY id",
    )
    .bind(market_id)
    .bind(character_id)
    .bind(item_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut still = qty;
    for r in &rows {
        if still <= 0 {
            break;
        }
        let take = still.min(r.qty);
        if take == r.qty {
            sqlx::query("DELETE FROM market_warehouse_item WHERE id = ?")
                .bind(&r.id)
                .execute(&mut **tx)
                .await?;
        } else {
            sqlx::query("UPDATE market_warehouse_item SET qty = qty - ? WHERE id = ?")
                .bind(take)
                .bind(&r.id)
                .execute(&mut **tx)
                .await?;
        }
        if back_to_available {
            // i64::MAX slots: returning your own escrow must never fail for
            // want of a slot — it was already yours a moment ago.
            warehouse_credit_in_tx(tx, market_id, character_id, item_id, take, i64::MAX).await?;
        }
        still -= take;
    }
    Ok(qty - still)
}

/// Burn `gold` out of the economy as a market fee (#141) and record it on the
/// append-only fee ledger, inside a caller-owned transaction.
///
/// "Burn" means exactly that: the gold is deducted and credited nowhere. The
/// ledger is therefore the ONLY record of what the sink removed — which is
/// what makes `purses + escrow + burned` checkable, and what a Phase 2 city
/// treasury (#144) would redirect rather than reinvent. Caller must have
/// already confirmed the payer can afford it.
#[allow(clippy::too_many_arguments)]
async fn burn_fee_in_tx(
    tx: &mut Tx<'_>,
    market_id: &str,
    character_id: &str,
    kind: &str,
    gold: i64,
    order_id: Option<&str>,
    trade_id: Option<&str>,
    now: i64,
) -> Result<(), DbError> {
    if gold <= 0 {
        return Ok(());
    }
    sqlx::query("UPDATE character SET gold = gold - ? WHERE id = ?")
        .bind(gold)
        .bind(character_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO market_fee (id, market_id, character_id, kind, gold, order_id, trade_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(market_id)
    .bind(character_id)
    .bind(kind)
    .bind(gold)
    .bind(order_id)
    .bind(trade_id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    // ...and on the supply ledger (#154), because a burned fee is gold leaving
    // the world. `market_fee` keeps the market-side detail; this keeps the
    // count. `fee_ledgers_agree` pins the two together.
    ledger_gold_in_tx(tx, character_id, -gold, "market_fee", now).await?;
    Ok(())
}

/// The balance a just-inserted character was given by the schema's DEFAULT.
///
/// Read back rather than hardcoded: the starting balance lives in migration
/// 0006, and a ledger that assumed 500 would quietly stop matching the moment
/// anyone tuned it. The ledger's job is to record what actually happened.
async fn starting_gold_in_tx(tx: &mut Tx<'_>, character_id: &str) -> Result<i64, DbError> {
    let gold: i64 = sqlx::query_scalar("SELECT gold FROM character WHERE id = ?")
        .bind(character_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(gold)
}

/// Record a change to the MONEY SUPPLY on the append-only `gold_ledger`
/// (#154): positive creates gold, negative destroys it.
///
/// Always called in the same transaction as the balance change it describes, so
/// a mint that isn't recorded cannot commit. That is the whole point — gold was
/// previously created by a bare UPDATE that recorded nothing, which made "how
/// much gold exists and where did it come from" unanswerable.
async fn ledger_gold_in_tx(
    tx: &mut Tx<'_>,
    character_id: &str,
    amount: i64,
    reason: &str,
    now: i64,
) -> Result<(), DbError> {
    if amount == 0 {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO gold_ledger (id, character_id, amount, reason, created_at)          VALUES (?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(character_id)
    .bind(amount)
    .bind(reason)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// CREATE gold and credit it to a purse, recording it on the ledger in the same
/// transaction (#154).
///
/// Distinct from [`grant_gold_in_tx`], and the distinction is the entire reason
/// the ledger works: most credits MOVE gold that already exists (a refund of
/// escrow, a seller's proceeds out of a buyer's escrow) and must NOT be
/// recorded as creation, or the supply would appear to balloon with every
/// trade. Only genuine faucets come through here — a new character's starting
/// balance, build wages (#145), and the provisioner's float (#154).
async fn mint_gold_in_tx(
    tx: &mut Tx<'_>,
    character_id: &str,
    amount: i64,
    reason: &str,
    now: i64,
) -> Result<(), DbError> {
    if amount <= 0 {
        return Ok(());
    }
    grant_gold_in_tx(tx, character_id, amount).await?;
    ledger_gold_in_tx(tx, character_id, amount, reason, now).await
}

/// MOVE gold into a character's purse, inside a caller-owned transaction.
///
/// Deliberately does NOT touch the ledger: every caller here is paying out gold
/// that already exists (escrow being refunded, a seller collecting a buyer's
/// escrow). Use [`mint_gold_in_tx`] when gold is genuinely being created —
/// getting this wrong in either direction breaks the supply identity, which is
/// why they are two functions rather than one with a flag.
///
/// `amount <= 0` is a no-op, so callers can pass zero without branching.
async fn grant_gold_in_tx(tx: &mut Tx<'_>, character_id: &str, amount: i64) -> Result<(), DbError> {
    if amount <= 0 {
        return Ok(());
    }
    sqlx::query("UPDATE character SET gold = gold + ? WHERE id = ?")
        .bind(amount)
        .bind(character_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
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
        // SQLite serialises writers. Without a busy timeout the second
        // concurrent write transaction fails IMMEDIATELY with "database is
        // locked" rather than waiting its turn — which surfaced under the
        // market's write-heavy load (#140) as deposits and trades randomly
        // failing. WAL additionally lets readers run alongside a writer,
        // which is most of this workload.
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        // ONE connection. sqlx opens deferred transactions, so a transaction
        // that reads before it writes (most of them — check capacity, then
        // insert) tries to upgrade a read lock to a write lock while another
        // connection already holds the write lock. SQLite returns
        // "database is locked" for that case *immediately*, and `busy_timeout`
        // deliberately does not retry it, because waiting could deadlock.
        // Surfaced under the market's write-heavy load (#140) as deposits and
        // trades randomly failing.
        //
        // A single connection removes the whole class: every transaction
        // queues on the pool instead of racing for locks, which is what this
        // gateway already is logically — one authoritative writer. Safe here
        // because DB work never nests (helpers that run inside a transaction
        // take `&mut Tx` rather than reaching for a second connection); a
        // nested acquire would deadlock instead. Real concurrency is what the
        // Postgres port (#41) is for.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
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
        // The starting balance is a column DEFAULT, which means it is gold
        // created out of nothing — the game's oldest and largest faucet, and
        // one nothing recorded until #154. Ledger it so the supply identity
        // closes; without this every new character silently widens the gap.
        let start = starting_gold_in_tx(&mut tx, &char_id).await?;
        ledger_gold_in_tx(&mut tx, &char_id, start, "character_start", now).await?;
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
        // See the sibling path above: the DEFAULT starting balance is a mint.
        let start = starting_gold_in_tx(&mut tx, &char_id).await?;
        ledger_gold_in_tx(&mut tx, &char_id, start, "character_start", ts).await?;
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


/// A station job as the database holds it, with `inputs_json` still raw.
#[derive(Debug, Clone, sqlx::FromRow)]
struct StationJobRow {
    id: String,
    station_id: String,
    character_id: String,
    slot: i64,
    recipe_id: String,
    inputs_json: String,
    fuel_units: i64,
    output_item: String,
    output_qty: i64,
    xp: i64,
    skill: String,
    started_at: i64,
    ready_at: i64,
    state: String,
    fail_reason: Option<String>,
}

/// A timed job at a station (#167), with its escrowed inputs decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationJob {
    pub id: String,
    pub station_id: String,
    pub character_id: String,
    pub slot: i64,
    pub recipe_id: String,
    /// What was ACTUALLY taken at start, so a refund returns that rather than
    /// whatever the recipe says today.
    pub inputs: Vec<(String, i64)>,
    pub fuel_units: i64,
    pub output_item: String,
    pub output_qty: i64,
    pub xp: i64,
    pub skill: String,
    pub started_at: i64,
    pub ready_at: i64,
    pub state: String,
    pub fail_reason: Option<String>,
}

impl From<StationJobRow> for StationJob {
    fn from(r: StationJobRow) -> Self {
        StationJob {
            id: r.id,
            station_id: r.station_id,
            character_id: r.character_id,
            slot: r.slot,
            recipe_id: r.recipe_id,
            inputs: serde_json::from_str(&r.inputs_json).unwrap_or_default(),
            fuel_units: r.fuel_units,
            output_item: r.output_item,
            output_qty: r.output_qty,
            xp: r.xp,
            skill: r.skill,
            started_at: r.started_at,
            ready_at: r.ready_at,
            state: r.state,
            fail_reason: r.fail_reason,
        }
    }
}

/// Why a job couldn't start. Each variant carries the numbers so the player is
/// told what they're short of rather than just "no".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartJobError {
    SlotBusy,
    NotEnoughGold { need: i64, have: i64 },
    NotEnoughFuel { need: i64, have: i64 },
    MissingInput { item: String, need: i64, have: i64 },
}

impl StartJobError {
    /// A short reason code for the wire; the client renders the prose.
    pub fn code(&self) -> &'static str {
        match self {
            StartJobError::SlotBusy => "slot_busy",
            StartJobError::NotEnoughGold { .. } => "not_enough_gold",
            StartJobError::NotEnoughFuel { .. } => "not_enough_fuel",
            StartJobError::MissingInput { .. } => "missing_input",
        }
    }
}

/// Why a collect didn't happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectError {
    NoSuchJob,
    NotReady { ready_at: i64 },
    /// The pack is full. The job is untouched and still holding the goods —
    /// this is a refusal, not a loss.
    NoRoom { need: i64, room: i64 },
}

/// What a successful collect handed over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedJob {
    pub station_id: String,
    pub slot: i64,
    pub failed: bool,
    pub fail_reason: Option<String>,
    pub payout: Vec<(String, i64)>,
    pub xp: i64,
    pub skill: String,
}

/// A carried inventory item (finite slots). `durability` is `None` for an
/// ordinary stackable item and `Some(0..=max)` for a tool instance (#128) —
/// a tool row's `qty` is always 1, since instances never stack.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct InventoryItem {
    pub id: String,
    pub character_id: String,
    pub item_id: String,
    pub qty: i64,
    pub slot: Option<i64>,
    pub durability: Option<i64>,
}

/// The tool currently armed in a slot, with its instance identity and live
/// durability (#128) — everything `apply_ability_use`'s wear-down and
/// `equip.update`'s display need in one lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EquippedTool {
    pub instance_id: String,
    pub item_id: String,
    pub durability: i64,
    pub max_durability: i64,
}

/// The result of spending durability on an equipped tool (#128).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WearOutcome {
    pub remaining: i64,
    /// `true` when this swing broke it — the equipment row was cleared
    /// (auto-unequip), leaving the instance in inventory at 0 durability.
    pub broke: bool,
}

/// The result of a successful repair (#128) — what it cost, so the gateway
/// can report it back and knows what to re-push (inventory changed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOutcome {
    pub item_id: String,
    pub cost: Vec<(String, i64)>,
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
    /// Gold paid to the contributor for this contribution (#145), already credited
    /// in the same transaction. `moved * wage_per_unit`; 0 for an unpaid order.
    pub wages: i64,
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

/// One priced chunk of a road's path (progressive road building epic #131,
/// issue #132): the input to [`Db::insert_road_cells`]/[`Db::replan_road_order`],
/// computed by chopping the plan's polyline into fixed-length pieces and
/// distributing the plan's total cost across them proportionally to each
/// piece's length — see `cut_road_cells` in `proxy.rs`, which owns the
/// pricing policy the same way it already owns `parse_road_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoadCellSpec {
    pub x0: i64,
    pub y0: i64,
    pub x1: i64,
    pub y1: i64,
    pub required_json: String,
}

/// A persisted chunk of a road's path, with its own cost/progress
/// (progressive road building epic #131, issue #132).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RoadCell {
    pub order_id: String,
    pub cell_index: i64,
    pub x0: i64,
    pub y0: i64,
    pub x1: i64,
    pub y1: i64,
    pub required_json: String,
    pub progress_json: String,
    pub completed_at: Option<i64>,
}

/// The outcome of a [`Db::contribute_to_road_cell`] call: mirrors
/// [`ContributeResult`], scoped to one cell plus whether that contribution
/// finished the WHOLE road (every cell complete), in which case the caller
/// runs the ordinary order-completion announcements same as `contribute`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellContributeResult {
    /// Units actually moved from carried inventory into the cell.
    pub moved: i64,
    /// The cell's required cost (`item -> qty`).
    pub required: BTreeMap<String, i64>,
    /// The cell's progress after this contribution (`item -> qty`).
    pub progress: BTreeMap<String, i64>,
    /// Whether this contribution completed the CELL.
    pub cell_completed: bool,
    /// Whether this contribution completed the whole road order (every cell
    /// done) — same shape as [`ContributeResult`]'s completion fields so the
    /// caller can reuse `announce_order_completion` unchanged.
    pub order_completed: bool,
    /// The order's aggregate required cost (`item -> qty`) — for the
    /// gateway's ordinary district-wide `build.progress` broadcast, which
    /// keeps firing off the pooled total alongside the new per-cell one
    /// (#133) so nothing reading the aggregate needs to change.
    pub order_required: BTreeMap<String, i64>,
    /// The order's aggregate progress after this contribution.
    pub order_progress: BTreeMap<String, i64>,
    pub kind: String,
    pub district: String,
    pub contributors: Vec<(String, i64)>,
    pub placement: Option<BuildPlacement>,
    /// Gold paid to the contributor for this contribution (#145), already credited
    /// in the same transaction. `moved * wage_per_unit`.
    pub wages: i64,
}

/// The outcome of a [`Db::replan_road_order`] call (#104).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplanOutcome {
    /// Whether the swap landed (`false` = the order wasn't an open road any
    /// more — completed/cancelled under the editor's feet; retry).
    pub applied: bool,
    /// Whether kept progress covered the recomputed cost, completing the
    /// order in the same transaction.
    pub completed: bool,
    /// On completion, `(character_id, units)` per contributor (for XP).
    pub contributors: Vec<(String, i64)>,
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
    /// Road orders only (#94): the full grid path as a JSON `[[x, y], ...]`
    /// polyline of axis-aligned runs. The placement columns still carry the
    /// first run (so segment-based proximity/completion code keeps working);
    /// this is the whole shape. `None` for every non-road order.
    pub path_json: Option<String>,
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

/// One row of a player's warehouse at one market (market epic #136, #138).
/// `state` is `available` or `locked` (locked = escrowed against an open sell
/// order, #139). `durability` mirrors `InventoryItem`'s: `None` for an
/// ordinary stackable, `Some` for a single tool instance.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct WarehouseItem {
    pub id: String,
    pub market_id: String,
    pub character_id: String,
    pub item_id: String,
    pub qty: i64,
    pub state: String,
    pub durability: Option<i64>,
}

/// A resting order on a market's book (market epic #136, issue #139).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MarketOrder {
    pub id: String,
    pub market_id: String,
    pub character_id: String,
    pub side: String,
    pub item_id: String,
    pub unit_price: i64,
    pub qty_total: i64,
    pub qty_remaining: i64,
    pub created_seq: i64,
    pub created_at: i64,
}

/// One executed fill, straight off the append-only ledger (#139).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MarketTrade {
    pub id: String,
    pub market_id: String,
    pub item_id: String,
    pub unit_price: i64,
    pub qty: i64,
    pub seller_id: String,
    pub buyer_id: String,
    pub sale_tax_gold: i64,
    pub listing_fee_gold: i64,
    pub created_at: i64,
}

/// One unique item offered on the listing board (#142). Its
/// `warehouse_item_id` is the escrowed instance, whose id has been the same
/// since the seller's own inventory row (#128).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MarketListing {
    pub id: String,
    pub market_id: String,
    pub seller_id: String,
    pub warehouse_item_id: String,
    pub item_id: String,
    pub durability: Option<i64>,
    pub ask_price: i64,
    pub created_at: i64,
    pub expires_at: i64,
}

/// Why a listing purchase was refused (#142). Distinguishing these matters:
/// "someone beat you to it" and "the price moved" want different reactions
/// from the player, and neither is a failure of theirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingReject {
    /// Bought, cancelled, or expired between the browse and the click.
    Gone,
    /// The ask isn't what the buyer was shown — never charge a surprise price.
    PriceChanged,
    /// Not enough gold for the ask.
    NoFunds,
    /// The buyer's warehouse at this market has no room to receive it.
    NoRoom,
    /// You can't buy your own listing.
    OwnListing,
}

impl ListingReject {
    pub fn code(self) -> &'static str {
        match self {
            ListingReject::Gone => "listing_gone",
            ListingReject::PriceChanged => "price_changed",
            ListingReject::NoFunds => "no_funds",
            ListingReject::NoRoom => "warehouse_full",
            ListingReject::OwnListing => "own_listing",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            ListingReject::Gone => "someone else took it first",
            ListingReject::PriceChanged => "the asking price changed — take another look",
            ListingReject::NoFunds => "not enough gold",
            ListingReject::NoRoom => "no room in your warehouse here to receive it",
            ListingReject::OwnListing => "that's your own listing",
        }
    }
}

/// One OHLCV candle of price history (#143). Derived from the trade ledger;
/// never authoritative.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Candle {
    pub bucket_start: i64,
    pub open: i64,
    pub high: i64,
    pub low: i64,
    pub close: i64,
    pub volume: i64,
    pub trades: i64,
}

/// One aggregated price level of a book — what the client is shown (#139).
/// Individual order ownership is deliberately NOT part of this: it keeps the
/// broadcast small and stops players reading each other's positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookLevel {
    pub unit_price: i64,
    pub qty: i64,
}

/// The outcome of placing an order (#139's aggressive buy, generalised in
/// #140 to either side resting or crossing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuyOutcome {
    /// Units actually traded immediately, across every level crossed.
    pub filled: i64,
    /// Gold the placer spent (a buy) — at EXECUTION prices, which are the
    /// resting orders' own, so crossing above the ask keeps the difference.
    pub spent: i64,
    /// Gold the placer received (a sell).
    pub earned: i64,
    /// Escrowed gold handed back: price improvement on a buy that crossed
    /// below its own limit.
    pub refunded: i64,
    /// The remainder that rested, if any — `None` when the order fully filled
    /// or nothing could be escrowed.
    pub resting_order_id: Option<String>,
    /// Units left resting on the book.
    pub resting_qty: i64,
    /// Each fill, for the ticker and for notifying counterparties.
    pub fills: Vec<MarketTrade>,
    /// Resting orders touched, as `(order_id, owner_id, qty_remaining)` — the
    /// gateway pushes each counterparty their own order update.
    pub touched: Vec<(String, String, i64)>,
    /// This command was a duplicate (`command_id` already seen) and was
    /// deliberately not applied. Distinct from "applied and did nothing":
    /// the caller should stay SILENT rather than report a failure, since the
    /// client already got an answer the first time round.
    pub deduped: bool,
    /// Listing fee burned at placement (#141) — charged whatever happens next,
    /// and never refunded.
    pub listing_fee: i64,
    /// Sale tax burned out of this placer's proceeds, if they sold (#141).
    pub sale_tax: i64,
    /// The order was refused because the placer couldn't cover the listing fee
    /// (#141), as opposed to having no stock or no gold to escrow.
    pub fee_unaffordable: bool,
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

    // --- Market warehouse (market epic #136, issue #138) -------------------

    /// Everything this character holds at this market, available and locked
    /// alike, oldest first so the client's list is stable across refreshes.
    pub async fn warehouse_for_character(
        &self,
        market_id: &str,
        character_id: &str,
    ) -> Result<Vec<WarehouseItem>, DbError> {
        sqlx::query_as::<_, WarehouseItem>(
            "SELECT * FROM market_warehouse_item WHERE market_id = ? AND character_id = ? \
             ORDER BY item_id, id",
        )
        .bind(market_id)
        .bind(character_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Move up to `qty` of `item_id` from carried inventory into this market's
    /// warehouse, in one transaction. Returns the amount moved (0 if nothing
    /// could be).
    ///
    /// **Capacity is in SLOTS, not units** (`slots`, from the caller's const):
    /// one row is one slot, so a stack of 90 planks costs the same slot as a
    /// stack of 2, and every tool instance costs its own. A deposit that would
    /// need a NEW row when already at capacity is refused **outright** — never
    /// partially applied — since a half-landed deposit is exactly the kind of
    /// thing players report as lost goods. Topping up an existing stack is
    /// always allowed, because it consumes no additional slot.
    ///
    /// Tools (#128) move as instances: the `inventory_item` row's own id and
    /// durability are carried over unchanged, so the thing in the warehouse is
    /// the same worn pickaxe you put in, not a fresh one.
    pub async fn warehouse_deposit(
        &self,
        market_id: &str,
        character_id: &str,
        item_id: &str,
        qty: i64,
        slots: i64,
    ) -> Result<i64, DbError> {
        if qty <= 0 {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let used_slots: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM market_warehouse_item WHERE market_id = ? AND character_id = ?",
        )
        .bind(market_id)
        .bind(character_id)
        .fetch_one(&mut *tx)
        .await?;

        let moved = if world::tool_max_durability(item_id).is_some() {
            // Instanced: each tool needs its own slot, so only take as many as
            // there is room for — and none at all if there's no room.
            let room = (slots - used_slots).max(0);
            let take = qty.min(room);
            if take == 0 {
                tx.commit().await?;
                return Ok(0);
            }
            let rows = sqlx::query_as::<_, InventoryItem>(
                "SELECT id, character_id, item_id, qty, slot, durability FROM inventory_item \
                 WHERE character_id = ? AND item_id = ? ORDER BY id LIMIT ?",
            )
            .bind(character_id)
            .bind(item_id)
            .bind(take)
            .fetch_all(&mut *tx)
            .await?;
            for r in &rows {
                // Depositing the tool you're holding takes it out of your hand
                // rather than refusing — same courtesy as #128's break, and
                // required anyway: `equipment.instance_id` references the
                // inventory row we're about to delete.
                sqlx::query("DELETE FROM equipment WHERE character_id = ? AND instance_id = ?")
                    .bind(character_id)
                    .bind(&r.id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM inventory_item WHERE id = ?")
                    .bind(&r.id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "INSERT INTO market_warehouse_item \
                     (id, market_id, character_id, item_id, qty, state, durability) \
                     VALUES (?, ?, ?, ?, 1, 'available', ?)",
                )
                .bind(&r.id) // same id in = same instance out
                .bind(market_id)
                .bind(character_id)
                .bind(item_id)
                .bind(r.durability)
                .execute(&mut *tx)
                .await?;
            }
            rows.len() as i64
        } else {
            // Stackable: merges into this market's existing available stack if
            // there is one, otherwise needs a free slot.
            let existing: Option<String> = sqlx::query_scalar(
                "SELECT id FROM market_warehouse_item WHERE market_id = ? AND character_id = ? \
                 AND item_id = ? AND state = 'available' LIMIT 1",
            )
            .bind(market_id)
            .bind(character_id)
            .bind(item_id)
            .fetch_optional(&mut *tx)
            .await?;
            if existing.is_none() && used_slots >= slots {
                tx.commit().await?;
                return Ok(0); // no room, and nothing to merge into
            }
            let moved = remove_inventory_in_tx(&mut tx, character_id, item_id, qty).await?;
            warehouse_credit_in_tx(&mut tx, market_id, character_id, item_id, moved, slots).await?;
            moved
        };
        tx.commit().await?;
        Ok(moved)
    }

    /// Move up to `qty` of `item_id` from this market's warehouse back into
    /// carried inventory. Draws from **`available` rows only** — locked stock
    /// is escrowed against an open order and is not the player's to take back
    /// until they cancel it. Also bounded by remaining carry capacity, exactly
    /// like [`Db::withdraw`] from home storage.
    pub async fn warehouse_withdraw(
        &self,
        market_id: &str,
        character_id: &str,
        item_id: &str,
        qty: i64,
    ) -> Result<i64, DbError> {
        if qty <= 0 {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let carried: Option<i64> =
            sqlx::query_scalar("SELECT SUM(qty) FROM inventory_item WHERE character_id = ?")
                .bind(character_id)
                .fetch_one(&mut *tx)
                .await?;
        let room = (MAX_CARRY - carried.unwrap_or(0)).max(0);
        if room <= 0 {
            tx.commit().await?;
            return Ok(0);
        }
        let rows = sqlx::query_as::<_, WarehouseItem>(
            "SELECT * FROM market_warehouse_item WHERE market_id = ? AND character_id = ? \
             AND item_id = ? AND state = 'available' ORDER BY id",
        )
        .bind(market_id)
        .bind(character_id)
        .bind(item_id)
        .fetch_all(&mut *tx)
        .await?;

        let mut moved = 0i64;
        for r in &rows {
            let want = qty.min(room) - moved;
            if want <= 0 {
                break;
            }
            if r.durability.is_some() {
                // A tool instance: all or nothing, and it goes back to carried
                // inventory as the SAME row id, keeping its wear.
                sqlx::query("DELETE FROM market_warehouse_item WHERE id = ?")
                    .bind(&r.id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "INSERT INTO inventory_item (id, character_id, item_id, qty, slot, durability) \
                     VALUES (?, ?, ?, 1, NULL, ?)",
                )
                .bind(&r.id)
                .bind(character_id)
                .bind(item_id)
                .bind(r.durability)
                .execute(&mut *tx)
                .await?;
                moved += 1;
            } else {
                let take = want.min(r.qty);
                if take <= 0 {
                    continue;
                }
                if take == r.qty {
                    sqlx::query("DELETE FROM market_warehouse_item WHERE id = ?")
                        .bind(&r.id)
                        .execute(&mut *tx)
                        .await?;
                } else {
                    sqlx::query("UPDATE market_warehouse_item SET qty = qty - ? WHERE id = ?")
                        .bind(take)
                        .bind(&r.id)
                        .execute(&mut *tx)
                        .await?;
                }
                add_inventory_in_tx(&mut tx, character_id, item_id, take).await?;
                moved += take;
            }
        }
        tx.commit().await?;
        Ok(moved)
    }

    /// Move `qty` of an item from `available` to `locked` in this market's
    /// warehouse — escrow against an open sell order (#139 will call this;
    /// #138 ships it so the state actually means something and can be tested).
    /// Returns the amount locked, bounded by what's available.
    pub async fn warehouse_lock(
        &self,
        market_id: &str,
        character_id: &str,
        item_id: &str,
        qty: i64,
    ) -> Result<i64, DbError> {
        let mut tx = self.pool.begin().await?;
        let locked = warehouse_lock_in_tx(&mut tx, market_id, character_id, item_id, qty).await?;
        tx.commit().await?;
        Ok(locked)
    }

    // --- Market order book (market epic #136, issue #139) ------------------

    /// Record a client-generated `command_id`, returning `false` if it has been
    /// seen before (#139). A reconnect-and-resend is indistinguishable from a
    /// duplicate, and both want the same answer: do nothing the second time.
    async fn claim_command_in_tx(
        tx: &mut Tx<'_>,
        command_id: &str,
        character_id: &str,
        now: i64,
    ) -> Result<bool, DbError> {
        if command_id.is_empty() {
            return Ok(true); // unversioned callers (tests, internal) aren't deduped
        }
        let res = sqlx::query(
            "INSERT OR IGNORE INTO market_command (command_id, character_id, created_at) \
             VALUES (?, ?, ?)",
        )
        .bind(command_id)
        .bind(character_id)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Place an order on either side of the book (#140, generalising #139's
    /// sell-rests / buy-crosses split): escrow, match against everything the
    /// order crosses, then rest whatever is left.
    ///
    /// **Escrow first, always.** A sell locks goods out of the seller's
    /// warehouse (`available` → `locked`); a buy deducts `unit_price * qty`
    /// gold from the purse. The order is then sized to what could ACTUALLY be
    /// escrowed — offering 50 planks while holding 20 places an order for 20 —
    /// so the book never advertises a promise its owner can't keep.
    ///
    /// **Price-time priority**: the opposite side is swept best-price-first,
    /// ties broken by `created_seq` (a sequence number, never a clock — two
    /// orders placed in the same millisecond still have a total order, and no
    /// clock adjustment can reorder a book).
    ///
    /// **The resting order's price wins.** A buyer bidding 12 into an 8 ask
    /// pays 8 and is refunded the 4 difference per unit out of escrow; a
    /// seller asking 5 into a resting 9 bid receives 9. Price improvement
    /// always goes to whoever crossed the spread.
    ///
    /// **Self-matching is skipped, not cancelled** — matching continues to the
    /// next order at that level, so a player with resting orders on both sides
    /// can still trade with everyone else and can never wash-trade with
    /// themselves.
    ///
    /// The remainder rests, which by construction cannot cross the opposite
    /// best (everything crossable was just consumed) — the book-never-crosses
    /// invariant holds after every call.
    #[allow(clippy::too_many_arguments)]
    pub async fn place_order(
        &self,
        market_id: &str,
        character_id: &str,
        side: &str,
        item_id: &str,
        unit_price: i64,
        qty: i64,
        expires_at: i64,
        cfg: &MarketConfig,
        command_id: &str,
        now: i64,
    ) -> Result<BuyOutcome, DbError> {
        let mut out = BuyOutcome::default();
        let buying = side == "buy";
        let mut tx = self.pool.begin().await?;
        if !Self::claim_command_in_tx(&mut tx, command_id, character_id, now).await? {
            tx.commit().await?;
            out.deduped = true;
            return Ok(out);
        }
        let open: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM market_order WHERE market_id = ? AND character_id = ?",
        )
        .bind(market_id)
        .bind(character_id)
        .fetch_one(&mut *tx)
        .await?;
        if open >= cfg.max_open_orders {
            tx.commit().await?;
            return Ok(out);
        }

        // --- escrow -------------------------------------------------------
        let purse: i64 = sqlx::query_scalar("SELECT gold FROM character WHERE id = ?")
            .bind(character_id)
            .fetch_one(&mut *tx)
            .await?;
        let escrowed_qty = if buying {
            // A buy must cover BOTH its escrow and its listing fee (#141), and
            // the fee depends on the notional, which depends on the size —
            // so size it, then walk down until the pair fits. Converges in a
            // couple of steps (each pass removes at least the shortfall).
            let mut take = qty.min(if unit_price > 0 { purse / unit_price } else { 0 });
            while take > 0 {
                let notional = take * unit_price;
                let total = notional + cfg.listing_fee(notional);
                if total <= purse {
                    break;
                }
                take -= ((total - purse) / unit_price).max(1);
            }
            if take > 0 {
                sqlx::query("UPDATE character SET gold = gold - ? WHERE id = ?")
                    .bind(take * unit_price)
                    .bind(character_id)
                    .execute(&mut *tx)
                    .await?;
            }
            take
        } else {
            // A sell's size comes from its stock, so the fee is a separate
            // affordability question — checked below, before anything moves.
            let stock: i64 = sqlx::query_scalar(
                "SELECT COALESCE(SUM(qty), 0) FROM market_warehouse_item \
                 WHERE market_id = ? AND character_id = ? AND item_id = ? AND state = 'available'",
            )
            .bind(market_id)
            .bind(character_id)
            .bind(item_id)
            .fetch_one(&mut *tx)
            .await?;
            let would_escrow = qty.min(stock);
            if would_escrow > 0 && cfg.listing_fee(would_escrow * unit_price) > purse {
                // Can't pay to list it: refuse OUTRIGHT rather than escrow
                // goods against an order that was never placed.
                tx.commit().await?;
                out.fee_unaffordable = true;
                return Ok(out);
            }
            warehouse_lock_in_tx(&mut tx, market_id, character_id, item_id, qty).await?
        };
        if escrowed_qty <= 0 {
            tx.commit().await?;
            return Ok(out);
        }

        // The listing fee is charged on what was actually escrowed, and burned
        // whatever happens next — filled, rested, cancelled, or expired. That
        // is precisely what makes posting an order you don't mean to honour
        // cost something (#141).
        out.listing_fee = cfg.listing_fee(escrowed_qty * unit_price);
        burn_fee_in_tx(
            &mut tx, market_id, character_id, "listing", out.listing_fee, None, None, now,
        )
        .await?;

        // --- match --------------------------------------------------------
        // Buys sweep the cheapest asks; sells sweep the richest bids.
        let (opposite, price_order) = if buying { ("sell", "ASC") } else { ("buy", "DESC") };
        let cmp = if buying { "<=" } else { ">=" };
        let sql = format!(
            "SELECT * FROM market_order WHERE market_id = ? AND item_id = ? AND side = ? \
             AND unit_price {cmp} ? ORDER BY unit_price {price_order}, created_seq ASC"
        );
        let resting = sqlx::query_as::<_, MarketOrder>(&sql)
            .bind(market_id)
            .bind(item_id)
            .bind(opposite)
            .bind(unit_price)
            .fetch_all(&mut *tx)
            .await?;

        let mut want = escrowed_qty;
        for o in &resting {
            if want <= 0 {
                break;
            }
            if o.character_id == character_id {
                continue; // self-match: skip it, leave it resting, keep going
            }
            let take = want.min(o.qty_remaining);
            if take <= 0 {
                continue;
            }
            let exec = o.unit_price; // the RESTING price, always
            let (buyer, seller) = if buying {
                (character_id, o.character_id.as_str())
            } else {
                (o.character_id.as_str(), character_id)
            };

            // The buyer must be able to receive before anything moves.
            let landed =
                warehouse_credit_in_tx(&mut tx, market_id, buyer, item_id, take, cfg.warehouse_slots)
                    .await?;
            if landed <= 0 {
                break; // buyer's warehouse is full — stop rather than vanish goods
            }

            // Goods leave the seller's escrow.
            release_locked_in_tx(&mut tx, market_id, seller, item_id, landed, false).await?;

            // Gold. Whoever was RESTING already escrowed at their own price, so
            // only the aggressor settles here. The seller pays sale tax out of
            // their proceeds (#141) — they receive value minus tax, and the tax
            // is burned.
            let value = landed * exec;
            let tax = cfg.sale_tax(value);
            if buying {
                // We escrowed at our limit; we pay `exec` and get the rest back.
                let refund = landed * (unit_price - exec);
                if refund > 0 {
                    grant_gold_in_tx(&mut tx, character_id, refund).await?;
                    out.refunded += refund;
                }
                grant_gold_in_tx(&mut tx, seller, value).await?;
                out.spent += value;
            } else {
                // The resting buyer's escrow covers it exactly; we just collect.
                grant_gold_in_tx(&mut tx, character_id, value).await?;
                out.earned += value;
                out.sale_tax += tax;
            }

            let remaining = o.qty_remaining - landed;
            if remaining > 0 {
                sqlx::query("UPDATE market_order SET qty_remaining = ? WHERE id = ?")
                    .bind(remaining)
                    .bind(&o.id)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query("DELETE FROM market_order WHERE id = ?")
                    .bind(&o.id)
                    .execute(&mut *tx)
                    .await?;
            }
            out.touched.push((o.id.clone(), o.character_id.clone(), remaining));

            let trade = MarketTrade {
                id: Uuid::new_v4().to_string(),
                market_id: market_id.to_string(),
                item_id: item_id.to_string(),
                unit_price: exec,
                qty: landed,
                seller_id: seller.to_string(),
                buyer_id: buyer.to_string(),
                sale_tax_gold: tax,
                listing_fee_gold: 0,
                created_at: now,
            };
            sqlx::query(
                "INSERT INTO market_trade (id, market_id, item_id, unit_price, qty, seller_id, \
                 buyer_id, sale_tax_gold, listing_fee_gold, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?)",
            )
            .bind(&trade.id)
            .bind(market_id)
            .bind(item_id)
            .bind(exec)
            .bind(landed)
            .bind(&trade.seller_id)
            .bind(&trade.buyer_id)
            .bind(tax)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            // The seller pays it, whichever side of the match they were on.
            burn_fee_in_tx(
                &mut tx, market_id, seller, "sale_tax", tax, None, Some(&trade.id), now,
            )
            .await?;
            out.fills.push(trade);
            out.filled += landed;
            want -= landed;
        }

        // --- rest the remainder -------------------------------------------
        if want > 0 {
            let seq: i64 =
                sqlx::query_scalar("SELECT COALESCE(MAX(created_seq), 0) + 1 FROM market_order")
                    .fetch_one(&mut *tx)
                    .await?;
            let id = Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO market_order (id, market_id, character_id, side, item_id, unit_price, \
                 qty_total, qty_remaining, created_seq, created_at, expires_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(market_id)
            .bind(character_id)
            .bind(side)
            .bind(item_id)
            .bind(unit_price)
            .bind(want)
            .bind(want)
            .bind(seq)
            .bind(now)
            .bind(expires_at)
            .execute(&mut *tx)
            .await?;
            out.resting_order_id = Some(id);
            out.resting_qty = want;
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Release one resting order's escrow and remove it, inside a caller-owned
    /// transaction. A sell returns its unsold goods to `available`; a buy
    /// returns `unit_price * qty_remaining` gold to the purse (it was deducted
    /// at the buyer's own limit, and every fill settled at that same rate or
    /// refunded the difference, so the remainder is exactly what's owed).
    ///
    /// Shared by cancel and expiry (#140), because an expired order and a
    /// cancelled one must release identically — anything else is a way to lose
    /// goods by not looking at the game for a day.
    async fn release_order_in_tx(tx: &mut Tx<'_>, order: &MarketOrder) -> Result<(), DbError> {
        sqlx::query("DELETE FROM market_order WHERE id = ?")
            .bind(&order.id)
            .execute(&mut **tx)
            .await?;
        if order.side == "sell" {
            release_locked_in_tx(
                tx, &order.market_id, &order.character_id, &order.item_id,
                order.qty_remaining, true,
            )
            .await?;
        } else {
            grant_gold_in_tx(
                tx, &order.character_id, order.qty_remaining * order.unit_price,
            )
            .await?;
        }
        Ok(())
    }

    /// Cancel a resting order (#139, escrow generalised in #140): returns the
    /// unfilled escrow — goods for a sell, gold for a buy — and removes the
    /// order. Ownership-checked: you can only cancel your own.
    pub async fn cancel_order(
        &self,
        character_id: &str,
        order_id: &str,
    ) -> Result<Option<MarketOrder>, DbError> {
        let mut tx = self.pool.begin().await?;
        let order = sqlx::query_as::<_, MarketOrder>(
            "SELECT * FROM market_order WHERE id = ? AND character_id = ?",
        )
        .bind(order_id)
        .bind(character_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(order) = order else {
            tx.commit().await?;
            return Ok(None);
        };
        Self::release_order_in_tx(&mut tx, &order).await?;
        tx.commit().await?;
        Ok(Some(order))
    }

    /// Release every order whose `expires_at` has passed (#140). Behaves
    /// exactly like a cancel, per order. `expires_at = 0` means "no expiry" —
    /// orders placed before expiry existed (#139) must not be retro-expired.
    /// Returns the released orders so the gateway can tell their owners.
    pub async fn expire_orders(&self, now: i64) -> Result<Vec<MarketOrder>, DbError> {
        let mut tx = self.pool.begin().await?;
        let due = sqlx::query_as::<_, MarketOrder>(
            "SELECT * FROM market_order WHERE expires_at > 0 AND expires_at <= ? ORDER BY id",
        )
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;
        for o in &due {
            Self::release_order_in_tx(&mut tx, o).await?;
        }
        tx.commit().await?;
        Ok(due)
    }

    /// A commodity's book at one market, aggregated by price level (#139).
    /// Sells ascending (best ask first); buys descending (best bid first).
    pub async fn book_for(
        &self,
        market_id: &str,
        item_id: &str,
        side: &str,
    ) -> Result<Vec<BookLevel>, DbError> {
        let order = if side == "sell" { "ASC" } else { "DESC" };
        let sql = format!(
            "SELECT unit_price, SUM(qty_remaining) AS qty FROM market_order \
             WHERE market_id = ? AND item_id = ? AND side = ? \
             GROUP BY unit_price ORDER BY unit_price {order}"
        );
        let rows = sqlx::query_as::<_, (i64, i64)>(&sql)
            .bind(market_id)
            .bind(item_id)
            .bind(side)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|(unit_price, qty)| BookLevel { unit_price, qty }).collect())
    }

    /// This character's own resting orders at a market (#139) — their own
    /// positions only; the aggregated book is what everyone else sees.
    pub async fn open_orders_for_character(
        &self,
        market_id: &str,
        character_id: &str,
    ) -> Result<Vec<MarketOrder>, DbError> {
        sqlx::query_as::<_, MarketOrder>(
            "SELECT * FROM market_order WHERE market_id = ? AND character_id = ? \
             ORDER BY item_id, unit_price, created_seq",
        )
        .bind(market_id)
        .bind(character_id)
        .fetch_all(&self.pool)
        .await
    }

    /// The most recent trades at a market, newest first — the ticker, and the
    /// raw material #143 rolls into candles.
    ///
    /// `created_at` is only second-resolution, so a burst of fills from one
    /// sweep all share a timestamp; `rowid` breaks the tie by true insertion
    /// order. Ordering by the id would sort by a random UUID instead, which
    /// makes the ledger's order meaningless and the trade log
    /// nondeterministic. (`rowid` is SQLite's implicit counter — the Postgres
    /// port, #41, would want an explicit sequence column here.)
    pub async fn recent_trades(
        &self,
        market_id: &str,
        item_id: &str,
        limit: i64,
    ) -> Result<Vec<MarketTrade>, DbError> {
        sqlx::query_as::<_, MarketTrade>(
            "SELECT * FROM market_trade WHERE market_id = ? AND item_id = ? \
             ORDER BY created_at DESC, rowid DESC LIMIT ?",
        )
        .bind(market_id)
        .bind(item_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    // --- Listing board for unique items (epic #136, issue #142) ------------

    /// Offer a unique item for sale at a fixed ask (#142).
    ///
    /// The instance must already be sitting `available` in the seller's
    /// warehouse at this market — the same rule commodities follow (#139): you
    /// bank goods at a market before you can sell them there. Listing then
    /// moves it to `locked`, so it is genuinely escrowed rather than flagged,
    /// and can't be withdrawn out from under a buyer.
    ///
    /// Charges the #141 listing fee on the ask, refused outright if the seller
    /// can't cover it (nothing is escrowed against an order never placed).
    /// Returns `None` if the instance isn't theirs, isn't available, or isn't a
    /// unique item.
    #[allow(clippy::too_many_arguments)]
    pub async fn place_listing(
        &self,
        market_id: &str,
        seller_id: &str,
        warehouse_item_id: &str,
        ask_price: i64,
        expires_at: i64,
        cfg: &MarketConfig,
        command_id: &str,
        now: i64,
    ) -> Result<Option<MarketListing>, DbError> {
        if ask_price <= 0 {
            return Ok(None);
        }
        let mut tx = self.pool.begin().await?;
        if !Self::claim_command_in_tx(&mut tx, command_id, seller_id, now).await? {
            tx.commit().await?;
            return Ok(None);
        }
        let row = sqlx::query_as::<_, WarehouseItem>(
            "SELECT * FROM market_warehouse_item WHERE id = ? AND market_id = ? \
             AND character_id = ? AND state = 'available'",
        )
        .bind(warehouse_item_id)
        .bind(market_id)
        .bind(seller_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        // Commodities belong on the book, not here — the board is for things
        // whose per-instance state makes them individually priced.
        if world::is_commodity(&row.item_id) {
            tx.commit().await?;
            return Ok(None);
        }
        let fee = cfg.listing_fee(ask_price);
        let purse: i64 = sqlx::query_scalar("SELECT gold FROM character WHERE id = ?")
            .bind(seller_id)
            .fetch_one(&mut *tx)
            .await?;
        if fee > purse {
            tx.commit().await?;
            return Ok(None);
        }
        sqlx::query("UPDATE market_warehouse_item SET state = 'locked' WHERE id = ?")
            .bind(&row.id)
            .execute(&mut *tx)
            .await?;
        burn_fee_in_tx(&mut tx, market_id, seller_id, "listing", fee, None, None, now).await?;

        let listing = MarketListing {
            id: Uuid::new_v4().to_string(),
            market_id: market_id.to_string(),
            seller_id: seller_id.to_string(),
            warehouse_item_id: row.id.clone(),
            item_id: row.item_id.clone(),
            durability: row.durability,
            ask_price,
            created_at: now,
            expires_at,
        };
        sqlx::query(
            "INSERT INTO market_listing (id, market_id, seller_id, warehouse_item_id, item_id, \
             durability, ask_price, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&listing.id)
        .bind(market_id)
        .bind(seller_id)
        .bind(&listing.warehouse_item_id)
        .bind(&listing.item_id)
        .bind(listing.durability)
        .bind(ask_price)
        .bind(now)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(listing))
    }

    /// Buy a listing outright (#142). First-come and atomic.
    ///
    /// The race is settled by a **compare-and-clear**: the DELETE is what
    /// claims the listing, and only the caller whose DELETE reports a row
    /// affected proceeds to move gold. Everyone else gets `Gone` having been
    /// charged nothing — there is no window in which two buyers can both be
    /// mid-purchase, and no partial charge for a loser.
    ///
    /// `expected_price` must match the ask, so a listing that changed under the
    /// player is refused rather than silently charged at a new price.
    ///
    /// The escrowed instance is handed over by REASSIGNING its warehouse row to
    /// the buyer, so the item that arrives is provably the one advertised —
    /// same row id, same durability — rather than a freshly minted copy.
    #[allow(clippy::too_many_arguments)]
    pub async fn buy_listing(
        &self,
        buyer_id: &str,
        listing_id: &str,
        expected_price: i64,
        cfg: &MarketConfig,
        command_id: &str,
        now: i64,
    ) -> Result<Result<(MarketListing, i64), ListingReject>, DbError> {
        let mut tx = self.pool.begin().await?;
        if !Self::claim_command_in_tx(&mut tx, command_id, buyer_id, now).await? {
            tx.commit().await?;
            return Ok(Err(ListingReject::Gone));
        }
        let listing = sqlx::query_as::<_, MarketListing>(
            "SELECT * FROM market_listing WHERE id = ?",
        )
        .bind(listing_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(listing) = listing else {
            tx.commit().await?;
            return Ok(Err(ListingReject::Gone));
        };
        if listing.seller_id == buyer_id {
            tx.commit().await?;
            return Ok(Err(ListingReject::OwnListing));
        }
        if listing.ask_price != expected_price {
            tx.commit().await?;
            return Ok(Err(ListingReject::PriceChanged));
        }
        let purse: i64 = sqlx::query_scalar("SELECT gold FROM character WHERE id = ?")
            .bind(buyer_id)
            .fetch_one(&mut *tx)
            .await?;
        if purse < listing.ask_price {
            tx.commit().await?;
            return Ok(Err(ListingReject::NoFunds));
        }
        // The buyer must have somewhere to put it before anything moves.
        let used: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM market_warehouse_item WHERE market_id = ? AND character_id = ?",
        )
        .bind(&listing.market_id)
        .bind(buyer_id)
        .fetch_one(&mut *tx)
        .await?;
        if used >= cfg.warehouse_slots {
            tx.commit().await?;
            return Ok(Err(ListingReject::NoRoom));
        }

        // THE claim. Whoever's delete lands owns the sale; a loser's reports 0
        // rows and stops here, uncharged.
        let claimed = sqlx::query("DELETE FROM market_listing WHERE id = ?")
            .bind(listing_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if claimed == 0 {
            tx.commit().await?;
            return Ok(Err(ListingReject::Gone));
        }

        // Hand the very same escrowed instance to the buyer.
        sqlx::query(
            "UPDATE market_warehouse_item SET character_id = ?, state = 'available' WHERE id = ?",
        )
        .bind(buyer_id)
        .bind(&listing.warehouse_item_id)
        .execute(&mut *tx)
        .await?;

        // Gold: buyer pays the ask, seller receives it net of sale tax (#141).
        let tax = cfg.sale_tax(listing.ask_price);
        sqlx::query("UPDATE character SET gold = gold - ? WHERE id = ?")
            .bind(listing.ask_price)
            .bind(buyer_id)
            .execute(&mut *tx)
            .await?;
        grant_gold_in_tx(&mut tx, &listing.seller_id, listing.ask_price).await?;

        let trade_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO market_trade (id, market_id, item_id, unit_price, qty, seller_id, \
             buyer_id, sale_tax_gold, listing_fee_gold, created_at) \
             VALUES (?, ?, ?, ?, 1, ?, ?, ?, 0, ?)",
        )
        .bind(&trade_id)
        .bind(&listing.market_id)
        .bind(&listing.item_id)
        .bind(listing.ask_price)
        .bind(&listing.seller_id)
        .bind(buyer_id)
        .bind(tax)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        burn_fee_in_tx(
            &mut tx, &listing.market_id, &listing.seller_id, "sale_tax", tax, None,
            Some(&trade_id), now,
        )
        .await?;
        tx.commit().await?;
        Ok(Ok((listing, tax)))
    }

    /// Withdraw a listing (#142): the escrowed instance returns to the seller's
    /// `available` warehouse stock, intact. Ownership-checked. The listing fee
    /// stays spent, exactly as on a cancelled order (#141).
    pub async fn cancel_listing(
        &self,
        seller_id: &str,
        listing_id: &str,
    ) -> Result<Option<MarketListing>, DbError> {
        let mut tx = self.pool.begin().await?;
        let listing = sqlx::query_as::<_, MarketListing>(
            "SELECT * FROM market_listing WHERE id = ? AND seller_id = ?",
        )
        .bind(listing_id)
        .bind(seller_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(listing) = listing else {
            tx.commit().await?;
            return Ok(None);
        };
        sqlx::query("DELETE FROM market_listing WHERE id = ?")
            .bind(listing_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE market_warehouse_item SET state = 'available' WHERE id = ?")
            .bind(&listing.warehouse_item_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(listing))
    }

    /// Release every listing past its expiry (#142) — identical to a cancel, so
    /// forgetting about a listing can never cost you the item.
    pub async fn expire_listings(&self, now: i64) -> Result<Vec<MarketListing>, DbError> {
        let mut tx = self.pool.begin().await?;
        let due = sqlx::query_as::<_, MarketListing>(
            "SELECT * FROM market_listing WHERE expires_at > 0 AND expires_at <= ? ORDER BY id",
        )
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;
        for l in &due {
            sqlx::query("DELETE FROM market_listing WHERE id = ?")
                .bind(&l.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE market_warehouse_item SET state = 'available' WHERE id = ?")
                .bind(&l.warehouse_item_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(due)
    }

    /// Browse a market's listing board (#142), cheapest first. Every filter is
    /// optional: `item_id` narrows to one kind, `min_durability` skips
    /// near-broken tools, `max_price` skips what you can't afford.
    pub async fn listings_for_market(
        &self,
        market_id: &str,
        item_id: Option<&str>,
        min_durability: Option<i64>,
        max_price: Option<i64>,
        limit: i64,
    ) -> Result<Vec<MarketListing>, DbError> {
        sqlx::query_as::<_, MarketListing>(
            "SELECT * FROM market_listing WHERE market_id = ? \
             AND (?2 IS NULL OR item_id = ?2) \
             AND (?3 IS NULL OR COALESCE(durability, 0) >= ?3) \
             AND (?4 IS NULL OR ask_price <= ?4) \
             ORDER BY ask_price ASC, created_at ASC LIMIT ?5",
        )
        .bind(market_id)
        .bind(item_id)
        .bind(min_durability)
        .bind(max_price)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// A single listing by id, for the gateway's own checks.
    pub async fn listing_by_id(&self, id: &str) -> Result<Option<MarketListing>, DbError> {
        sqlx::query_as::<_, MarketListing>("SELECT * FROM market_listing WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    // --- Price history (epic #136, issue #143) -----------------------------

    /// Recompute the OHLCV candles covering `[from, to)` from the trade ledger
    /// (#143), replacing whatever was cached for those buckets.
    ///
    /// The aggregation is done in Rust rather than SQL on purpose: open and
    /// close are the FIRST and LAST trade in a bucket by insertion order, which
    /// in SQL needs window functions or correlated subqueries to express and is
    /// easy to get subtly wrong. Folding an ordered stream is obviously
    /// correct, and this is a background job — it doesn't need to be clever.
    ///
    /// Idempotent: running it twice over the same range produces the same rows,
    /// which is what lets it be both an incremental job and a full rebuild.
    pub async fn roll_up_candles(
        &self,
        interval_secs: i64,
        from: i64,
        to: i64,
    ) -> Result<i64, DbError> {
        if interval_secs <= 0 || to <= from {
            return Ok(0);
        }
        // Ordered by insertion (`rowid`) within a timestamp, so "first" and
        // "last" in a bucket mean what they say — the same reason
        // `recent_trades` can't tie-break on a random UUID (#140).
        let trades = sqlx::query_as::<_, (String, String, i64, i64, i64)>(
            "SELECT market_id, item_id, unit_price, qty, created_at FROM market_trade \
             WHERE created_at >= ? AND created_at < ? ORDER BY created_at ASC, rowid ASC",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;

        // (market, item, bucket) -> candle, folded in ledger order.
        let mut acc: BTreeMap<(String, String, i64), Candle> = BTreeMap::new();
        for (market_id, item_id, price, qty, at) in trades {
            let bucket = world::candle_bucket(at, interval_secs);
            acc.entry((market_id, item_id, bucket))
                .and_modify(|c| {
                    c.high = c.high.max(price);
                    c.low = c.low.min(price);
                    c.close = price; // later trade wins
                    c.volume += qty;
                    c.trades += 1;
                })
                .or_insert(Candle {
                    bucket_start: bucket,
                    open: price,
                    high: price,
                    low: price,
                    close: price,
                    volume: qty,
                    trades: 1,
                });
        }

        let mut tx = self.pool.begin().await?;
        // Clear the range first, so a bucket that no longer has trades (a
        // ledger correction, or a narrowed rebuild) disappears rather than
        // lingering as a stale candle.
        sqlx::query(
            "DELETE FROM market_candle WHERE interval_secs = ? \
             AND bucket_start >= ? AND bucket_start < ?",
        )
        .bind(interval_secs)
        .bind(world::candle_bucket(from, interval_secs))
        .bind(to)
        .execute(&mut *tx)
        .await?;
        let written = acc.len() as i64;
        for ((market_id, item_id, _), c) in &acc {
            sqlx::query(
                "INSERT INTO market_candle (market_id, item_id, interval_secs, bucket_start, \
                 open, high, low, close, volume, trades) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(market_id)
            .bind(item_id)
            .bind(interval_secs)
            .bind(c.bucket_start)
            .bind(c.open)
            .bind(c.high)
            .bind(c.low)
            .bind(c.close)
            .bind(c.volume)
            .bind(c.trades)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(written)
    }

    /// Rebuild every candle from the whole ledger (#143). Exists because the
    /// cache must be provably derived: a from-scratch rebuild has to reproduce
    /// exactly what the incremental job produced, and a test asserts it does.
    pub async fn rebuild_all_candles(&self, interval_secs: i64) -> Result<i64, DbError> {
        let span: Option<(Option<i64>, Option<i64>)> =
            sqlx::query_as("SELECT MIN(created_at), MAX(created_at) FROM market_trade")
                .fetch_optional(&self.pool)
                .await?;
        let Some((Some(min), Some(max))) = span else {
            sqlx::query("DELETE FROM market_candle WHERE interval_secs = ?")
                .bind(interval_secs)
                .execute(&self.pool)
                .await?;
            return Ok(0);
        };
        sqlx::query("DELETE FROM market_candle WHERE interval_secs = ?")
            .bind(interval_secs)
            .execute(&self.pool)
            .await?;
        self.roll_up_candles(interval_secs, min, max + 1).await
    }

    /// One commodity's candles, oldest first (#143). **Absent buckets are
    /// absent** — a quiet hour is a gap, not a flat candle at the last price,
    /// because inventing a price nobody paid is worse than showing nothing.
    pub async fn candles(
        &self,
        market_id: &str,
        item_id: &str,
        interval_secs: i64,
        from: i64,
        to: i64,
    ) -> Result<Vec<Candle>, DbError> {
        sqlx::query_as::<_, Candle>(
            "SELECT bucket_start, open, high, low, close, volume, trades FROM market_candle \
             WHERE market_id = ? AND item_id = ? AND interval_secs = ? \
             AND bucket_start >= ? AND bucket_start < ? ORDER BY bucket_start ASC",
        )
        .bind(market_id)
        .bind(item_id)
        .bind(interval_secs)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await
    }

    /// Drop candles older than the retention window (#143). Touches ONLY the
    /// derived cache — `market_trade` is append-only and is never pruned, so
    /// pruned history can always be rebuilt.
    pub async fn prune_candles(&self, before: i64) -> Result<u64, DbError> {
        let res = sqlx::query("DELETE FROM market_candle WHERE bucket_start < ?")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// Total gold burned as market fees (#141) — listing fees plus sale tax,
    /// across every market. The sink's own record: because burned gold is
    /// credited nowhere, this is the only way to close the books, and
    /// `purses + escrow + burned` must be constant.
    pub async fn total_fees_burned(&self) -> Result<i64, DbError> {
        let total: Option<i64> = sqlx::query_scalar("SELECT SUM(gold) FROM market_fee")
            .fetch_one(&self.pool)
            .await?;
        Ok(total.unwrap_or(0))
    }

    // --- warehouse storage fees (#155) --------------------------------------

    /// Unpaid storage debt at this market, and whether that locks the warehouse.
    /// Zero for everyone unless an operator has turned storage fees on.
    pub async fn warehouse_arrears(
        &self,
        market_id: &str,
        character_id: &str,
    ) -> Result<i64, DbError> {
        let n: Option<i64> = sqlx::query_scalar(
            "SELECT arrears FROM market_warehouse_account WHERE market_id = ? AND character_id = ?",
        )
        .bind(market_id)
        .bind(character_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(n.unwrap_or(0))
    }

    /// Pay down storage arrears from the purse, as far as it stretches, and
    /// return what is still owed.
    ///
    /// Called both by the daily job and at the start of every warehouse
    /// operation, so a player who returns with gold is unlocked the moment they
    /// try to use the warehouse rather than having to wait for the next tick.
    /// Being locked out is a nudge to pay, not a punishment to serve.
    pub async fn settle_warehouse_arrears(
        &self,
        market_id: &str,
        character_id: &str,
        now: i64,
    ) -> Result<i64, DbError> {
        let mut tx = self.pool.begin().await?;
        let owed: Option<i64> = sqlx::query_scalar(
            "SELECT arrears FROM market_warehouse_account WHERE market_id = ? AND character_id = ?",
        )
        .bind(market_id)
        .bind(character_id)
        .fetch_optional(&mut *tx)
        .await?;
        let owed = owed.unwrap_or(0);
        if owed <= 0 {
            tx.commit().await?;
            return Ok(0);
        }
        let purse: i64 = sqlx::query_scalar("SELECT gold FROM character WHERE id = ?")
            .bind(character_id)
            .fetch_one(&mut *tx)
            .await?;
        let pay = owed.min(purse.max(0));
        if pay > 0 {
            burn_fee_in_tx(&mut tx, market_id, character_id, "storage", pay, None, None, now)
                .await?;
            sqlx::query(
                "UPDATE market_warehouse_account SET arrears = arrears - ? \
                 WHERE market_id = ? AND character_id = ?",
            )
            .bind(pay)
            .bind(market_id)
            .bind(character_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(owed - pay)
    }

    /// Charge one day's storage at `market_id` to everyone holding stock there.
    ///
    /// Returns `(charged, arrears_added)` in gold.
    ///
    /// Properties that matter more than the arithmetic:
    ///
    /// * **Rate 0 is a total no-op** — no rows written, nobody locked. That is
    ///   the shipped configuration.
    /// * **Idempotent within a day.** `last_charged_at` gates it, so a restart
    ///   loop cannot bill anyone twice.
    /// * **Offline days are free.** A player is only charged if they have been
    ///   seen since their last charge. Billing for days someone wasn't there
    ///   turns a holding cost into a punishment for having a job.
    /// * **Goods are never confiscated.** What the purse can't cover becomes
    ///   capped arrears and locks the warehouse until paid. Deleting someone's
    ///   stored items to settle a debt is an unrecoverable loss caused by not
    ///   logging in.
    pub async fn charge_storage(
        &self,
        market_id: &str,
        cfg: &MarketConfig,
        now: i64,
    ) -> Result<(i64, i64), DbError> {
        if cfg.storage_fee_per_slot_per_day <= 0 {
            return Ok((0, 0));
        }
        const DAY: i64 = 86_400;
        // One row per slot, both states — locked stock is still occupying the
        // warehouse, and exempting it would make "list it at an absurd price" a
        // free-storage loophole.
        let holders = sqlx::query_as::<_, (String, i64)>(
            "SELECT character_id, COUNT(*) FROM market_warehouse_item \
             WHERE market_id = ? GROUP BY character_id",
        )
        .bind(market_id)
        .fetch_all(&self.pool)
        .await?;

        let cap = cfg.storage_arrears_cap_days * cfg.storage_fee_per_slot_per_day;
        let (mut charged, mut accrued) = (0i64, 0i64);
        for (character_id, slots) in holders {
            let mut tx = self.pool.begin().await?;
            let row = sqlx::query_as::<_, (i64, i64)>(
                "SELECT last_charged_at, arrears FROM market_warehouse_account \
                 WHERE market_id = ? AND character_id = ?",
            )
            .bind(market_id)
            .bind(&character_id)
            .fetch_optional(&mut *tx)
            .await?;
            let (last_charged, arrears) = row.unwrap_or((0, 0));

            // Not a new day yet.
            if last_charged > 0 && now - last_charged < DAY {
                tx.commit().await?;
                continue;
            }
            let last_seen: i64 = sqlx::query_scalar("SELECT last_seen FROM character WHERE id = ?")
                .bind(&character_id)
                .fetch_one(&mut *tx)
                .await?;
            // Never charged before: start the clock rather than billing for
            // however long the goods happened to have been sitting there.
            if last_charged == 0 {
                sqlx::query(
                    "INSERT INTO market_warehouse_account \
                     (market_id, character_id, last_charged_at, arrears) VALUES (?, ?, ?, 0)",
                )
                .bind(market_id)
                .bind(&character_id)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                continue;
            }
            // Offline for the whole period: no charge, and the clock moves on so
            // the unbilled days never pile up into a surprise.
            if last_seen <= last_charged {
                sqlx::query(
                    "UPDATE market_warehouse_account SET last_charged_at = ? \
                     WHERE market_id = ? AND character_id = ?",
                )
                .bind(now)
                .bind(market_id)
                .bind(&character_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                continue;
            }

            // Exactly one day's fee per run, however long the gap — a player
            // returning after a month owes a day, not a month.
            let due = slots * cfg.storage_fee_per_slot_per_day;
            let purse: i64 = sqlx::query_scalar("SELECT gold FROM character WHERE id = ?")
                .bind(&character_id)
                .fetch_one(&mut *tx)
                .await?;
            let pay = due.min(purse.max(0));
            if pay > 0 {
                burn_fee_in_tx(&mut tx, market_id, &character_id, "storage", pay, None, None, now)
                    .await?;
                charged += pay;
            }
            // Whatever the purse couldn't cover becomes debt, capped so it stays
            // payable. Past the cap the meter simply stops: the warehouse is
            // already locked, the point has been made, and a bigger number
            // helps nobody.
            let unpaid = (due - pay).max(0);
            let new_arrears = (arrears + unpaid).min(cap.max(0));
            accrued += (new_arrears - arrears).max(0);
            sqlx::query(
                "UPDATE market_warehouse_account SET last_charged_at = ?, arrears = ? \
                 WHERE market_id = ? AND character_id = ?",
            )
            .bind(now)
            .bind(new_arrears)
            .bind(market_id)
            .bind(&character_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        Ok((charged, accrued))
    }

    // --- NPC provisioner (market phase 2 epic #151, issue #154) -------------
    //
    // Standing bounds on a commodity's price, implemented as ORDINARY resting
    // orders owned by a system character rather than a special case in the
    // matching engine. That choice is the whole design: price-time priority,
    // partial fills, self-match prevention, escrow and the trade ledger all work
    // on them unmodified, and the bounds are VISIBLE in the book — a player can
    // see the floor instead of discovering it. A backstop consulted after a
    // failed match would be more code, less reuse, and invisible.

    /// The system character that owns the provisioner's orders, created on first
    /// use and returned thereafter.
    ///
    /// A real character with a real purse, because everything downstream —
    /// escrow, warehouses, fills, the gold ledger — is keyed on one. It is
    /// reached only by this id: the account's password hash is a fixed
    /// non-verifying placeholder, so nobody can log in as the market.
    pub async fn ensure_provisioner(&self) -> Result<String, DbError> {
        const EMAIL: &str = "provisioner@system.invalid";
        if let Some(acct) = self.find_account_by_email(EMAIL).await? {
            if let Some(c) = self.character_for_account(&acct.id).await? {
                return Ok(c.id);
            }
        }
        let (_, c) = self
            .create_account_with_character(EMAIL, "!no-login!", "Provisioner", 0, 0, 100)
            .await?;
        Ok(c.id)
    }

    /// Re-post the provisioner's standing bid and ask at one market (#154).
    ///
    /// Idempotent by construction: it cancels whatever it had resting for the
    /// commodity and posts fresh orders, so running it twice in a row leaves the
    /// same book rather than doubling the depth.
    ///
    /// Returns how much gold was newly MINTED to fund the bid — the number the
    /// caller logs, because a faucet nobody is watching is how an economy gets
    /// away from you.
    pub async fn refresh_provisioner(
        &self,
        market_id: &str,
        cfg: &MarketConfig,
        now: i64,
    ) -> Result<i64, DbError> {
        if cfg.provisioner.is_empty() {
            return Ok(0);
        }
        let npc = self.ensure_provisioner().await?;
        // The provisioner's own placements are free. It re-posts on a timer, so
        // charging it a listing fee every cycle would churn the ledger with a
        // burn-and-mint cycle that means nothing. (It still pays SALE TAX when a
        // player lifts its ask, because that is charged from the taker's config
        // in the fill path — a real sink, properly recorded, and not worth a
        // special case in the hot loop.)
        let npc_cfg = MarketConfig {
            listing_fee_min_gold: 0,
            listing_fee_num: 0,
            max_open_orders: i64::MAX,
            ..cfg.clone()
        };

        let mut minted = 0i64;
        for (item, bounds) in &cfg.provisioner {
            // Seed stock, once per (market, commodity), so the CEILING exists on
            // a brand-new server before the provisioner has bought anything.
            // Deduped through `market_command`, which already exists to make
            // exactly this kind of operation happen-once — no new table, and it
            // survives restarts.
            if bounds.seed_stock > 0 {
                let mut tx = self.pool.begin().await?;
                let seed_id = format!("provisioner-seed:{market_id}:{item}");
                if Self::claim_command_in_tx(&mut tx, &seed_id, &npc, now).await? {
                    warehouse_credit_in_tx(
                        &mut tx, market_id, &npc, item, bounds.seed_stock, i64::MAX,
                    )
                    .await?;
                }
                tx.commit().await?;
            }

            // Clear what it had resting, so this is a refresh rather than an
            // accumulation. Escrow (gold for the bid, goods for the ask) comes
            // back in the process, which is what makes the sizing below simple.
            for o in self.open_orders_for_character(market_id, &npc).await? {
                if o.item_id == *item {
                    self.cancel_order(&npc, &o.id).await?;
                }
            }

            // --- the floor -------------------------------------------------
            // Unbounded by design: a bid with a finite budget stops being a
            // floor exactly when it is needed, during a crash. So the shortfall
            // is MINTED. This is the game's second faucet, and the reason #154
            // built the ledger first.
            let want = bounds.floor * bounds.bid_qty;
            let purse = self.character_gold(&npc).await?;
            if want > purse {
                let short = want - purse;
                let mut tx = self.pool.begin().await?;
                mint_gold_in_tx(&mut tx, &npc, short, "provisioner", now).await?;
                tx.commit().await?;
                minted += short;
            }
            // `0` = never expires (migration 0018). The provisioner's orders
            // must not be swept: a floor that vanished after 24h would be a
            // floor exactly until someone needed it.
            self.place_order(
                market_id, &npc, "buy", item, bounds.floor, bounds.bid_qty,
                0, &npc_cfg, "", now,
            )
            .await?;

            // --- the ceiling -----------------------------------------------
            // Sells from STOCK ONLY — whatever it has bought, plus the seed.
            // Unbounded selling would be an infinite item faucet, and destroying
            // scarcity is worse than an uncapped price.
            let stock: i64 = self
                .warehouse_for_character(market_id, &npc)
                .await?
                .iter()
                .filter(|r| r.item_id == *item && r.state == "available")
                .map(|r| r.qty)
                .sum();
            if stock > 0 {
                self.place_order(
                    market_id, &npc, "sell", item, bounds.ceiling, stock,
                    0, &npc_cfg, "", now,
                )
                .await?;
            }
        }
        Ok(minted)
    }

    /// Trades at `market_id` since `since` that executed OUTSIDE the configured
    /// bounds, as `(item, price, bound_low, bound_high)`.
    ///
    /// The design doc's §7 balance telemetry. Since the provisioner rests orders
    /// at exactly these prices, a trade beyond them means either the refresh job
    /// is lagging or the bounds are wrong for the economy that has grown around
    /// them — both worth knowing, and both otherwise invisible. This is the data
    /// #129's balance pass has never had.
    pub async fn trades_outside_bounds(
        &self,
        market_id: &str,
        cfg: &MarketConfig,
        since: i64,
    ) -> Result<Vec<(String, i64, i64, i64)>, DbError> {
        let mut out = Vec::new();
        for (item, b) in &cfg.provisioner {
            let rows = sqlx::query_as::<_, (i64,)>(
                "SELECT unit_price FROM market_trade \
                 WHERE market_id = ? AND item_id = ? AND created_at >= ? \
                   AND (unit_price < ? OR unit_price > ?)",
            )
            .bind(market_id)
            .bind(item)
            .bind(since)
            .bind(b.floor)
            .bind(b.ceiling)
            .fetch_all(&self.pool)
            .await?;
            for (price,) in rows {
                out.push((item.clone(), price, b.floor, b.ceiling));
            }
        }
        Ok(out)
    }

    // --- deposit state (mine epic #164, issue #166) --------------------------

    /// Record that a seam was worked out, so a restart resumes its timer rather
    /// than refilling it.
    pub async fn mark_deposit_depleted(&self, deposit_id: &str, at: i64) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO deposit_state (deposit_id, depleted_at) VALUES (?, ?)              ON CONFLICT(deposit_id) DO UPDATE SET depleted_at = excluded.depleted_at",
        )
        .bind(deposit_id)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Forget a seam's depletion — it has come back.
    pub async fn clear_deposit_depleted(&self, deposit_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM deposit_state WHERE deposit_id = ?")
            .bind(deposit_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Every seam currently recorded as worked out, as `(id, depleted_at)`.
    ///
    /// Pushed to a zone when it starts so it can resume mid-cycle. An absent id
    /// means full, which is why nothing is written for an untouched seam.
    pub async fn depleted_deposits(&self) -> Result<Vec<(String, i64)>, DbError> {
        sqlx::query_as::<_, (String, i64)>(
            "SELECT deposit_id, depleted_at FROM deposit_state ORDER BY deposit_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    // --- the creature bounty (wild dogs epic #157, issue #161) --------------

    // --- Stations, fuel and timed jobs (mine epic #164, issue #167) ---------

    /// How many fuel units a station is holding.
    pub async fn station_fuel(&self, station_id: &str) -> Result<i64, DbError> {
        let units: Option<i64> =
            sqlx::query_scalar("SELECT units FROM station_fuel WHERE station_id = ?")
                .bind(station_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(units.unwrap_or(0))
    }

    /// Load fuel into a station's shared buffer, converting items to units at
    /// the station type's rate.
    ///
    /// **The items leave the player and the units appear together**, so a crash
    /// between the two can't burn charcoal for nothing.
    ///
    /// Loading is an explicit player action rather than something a job does
    /// implicitly, because it is the mechanic the tutorial (#169) exists to
    /// teach: a furnace you have to feed is a furnace you understand.
    ///
    /// Returns the new total, or `None` if the player didn't have the items.
    pub async fn load_station_fuel(
        &self,
        station_id: &str,
        character_id: &str,
        item_id: &str,
        qty: i64,
        units_per_item: i64,
        now: i64,
    ) -> Result<Option<i64>, DbError> {
        if qty <= 0 || units_per_item <= 0 {
            return Ok(None);
        }
        let mut tx = self.pool.begin().await?;
        let have: Option<i64> = sqlx::query_scalar(
            "SELECT SUM(qty) FROM inventory_item WHERE character_id = ? AND item_id = ?",
        )
        .bind(character_id)
        .bind(item_id)
        .fetch_one(&mut *tx)
        .await?;
        if have.unwrap_or(0) < qty {
            tx.commit().await?;
            return Ok(None);
        }
        remove_inventory_in_tx(&mut tx, character_id, item_id, qty).await?;
        let units = qty * units_per_item;
        sqlx::query(
            "INSERT INTO station_fuel (station_id, units, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(station_id) DO UPDATE SET units = units + excluded.units, \
             updated_at = excluded.updated_at",
        )
        .bind(station_id)
        .bind(units)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let total: i64 = sqlx::query_scalar("SELECT units FROM station_fuel WHERE station_id = ?")
            .bind(station_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(total))
    }

    /// Start a timed job: charge the fee, escrow the inputs and the fuel, and
    /// write the job row — **all in one transaction**.
    ///
    /// The ordering inside matters. Everything that can refuse is checked
    /// before anything is taken, because a fee charged for a job that then
    /// fails validation is silent theft. The transaction makes that guarantee
    /// total rather than merely likely.
    ///
    /// `slot` is chosen by the caller from what [`Db::station_jobs`] reports
    /// free, but the unique index is what actually enforces it: two starts
    /// racing for one slot lose to the database, not to whichever read first.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_station_job(
        &self,
        station_id: &str,
        character_id: &str,
        slot: i64,
        recipe_id: &str,
        recipe: &crate::crafting_config::StationRecipe,
        fee_gold: i64,
        duration_ms: i64,
        now: i64,
    ) -> Result<Result<StationJob, StartJobError>, DbError> {
        let mut tx = self.pool.begin().await?;

        // Slot free?
        let taken: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM station_job WHERE station_id = ? AND character_id = ? AND slot = ?",
        )
        .bind(station_id)
        .bind(character_id)
        .bind(slot)
        .fetch_optional(&mut *tx)
        .await?;
        if taken.is_some() {
            tx.commit().await?;
            return Ok(Err(StartJobError::SlotBusy));
        }

        // Gold for the fee?
        let gold: i64 = sqlx::query_scalar("SELECT gold FROM character WHERE id = ?")
            .bind(character_id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(0);
        if gold < fee_gold {
            tx.commit().await?;
            return Ok(Err(StartJobError::NotEnoughGold { need: fee_gold, have: gold }));
        }

        // Every ingredient, before taking any of them.
        for i in &recipe.inputs {
            let have: Option<i64> = sqlx::query_scalar(
                "SELECT SUM(qty) FROM inventory_item WHERE character_id = ? AND item_id = ?",
            )
            .bind(character_id)
            .bind(&i.item)
            .fetch_one(&mut *tx)
            .await?;
            if have.unwrap_or(0) < i.qty {
                tx.commit().await?;
                return Ok(Err(StartJobError::MissingInput {
                    item: i.item.clone(),
                    need: i.qty,
                    have: have.unwrap_or(0),
                }));
            }
        }

        // Fuel, taken from the SHARED buffer. Reserved here rather than spent at
        // completion so a second job can't promise itself the same charcoal.
        if recipe.fuel_units > 0 {
            let units: i64 =
                sqlx::query_scalar("SELECT units FROM station_fuel WHERE station_id = ?")
                    .bind(station_id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .unwrap_or(0);
            if units < recipe.fuel_units {
                tx.commit().await?;
                return Ok(Err(StartJobError::NotEnoughFuel {
                    need: recipe.fuel_units,
                    have: units,
                }));
            }
            sqlx::query(
                "UPDATE station_fuel SET units = units - ?, updated_at = ? WHERE station_id = ?",
            )
            .bind(recipe.fuel_units)
            .bind(now)
            .bind(station_id)
            .execute(&mut *tx)
            .await?;
        }

        // Past this point nothing can refuse, so it is safe to start taking.
        for i in &recipe.inputs {
            remove_inventory_in_tx(&mut tx, character_id, &i.item, i.qty).await?;
        }
        if fee_gold > 0 {
            // Burned, not banked. A station fee is gold leaving the world, and
            // it goes through the ledger for the same reason market fees do —
            // `gold_supply_gap()` has to stay 0.
            sqlx::query("UPDATE character SET gold = gold - ? WHERE id = ?")
                .bind(fee_gold)
                .bind(character_id)
                .execute(&mut *tx)
                .await?;
            ledger_gold_in_tx(&mut tx, character_id, -fee_gold, "station_fee", now).await?;
        }

        let inputs: Vec<(String, i64)> =
            recipe.inputs.iter().map(|i| (i.item.clone(), i.qty)).collect();
        let inputs_json = serde_json::to_string(&inputs).unwrap_or_else(|_| "[]".to_string());
        let job = StationJob {
            id: Uuid::new_v4().to_string(),
            station_id: station_id.to_string(),
            character_id: character_id.to_string(),
            slot,
            recipe_id: recipe_id.to_string(),
            inputs,
            fuel_units: recipe.fuel_units,
            output_item: recipe.output_item.clone(),
            output_qty: recipe.output_qty,
            xp: recipe.xp,
            skill: recipe.skill.clone(),
            started_at: now,
            ready_at: now + (duration_ms.max(1) + 999) / 1000,
            state: "running".to_string(),
            fail_reason: None,
        };
        sqlx::query(
            "INSERT INTO station_job (id, station_id, character_id, slot, recipe_id, inputs_json, \
             fuel_units, output_item, output_qty, xp, skill, started_at, ready_at, state) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'running')",
        )
        .bind(&job.id)
        .bind(&job.station_id)
        .bind(&job.character_id)
        .bind(job.slot)
        .bind(&job.recipe_id)
        .bind(&inputs_json)
        .bind(job.fuel_units)
        .bind(&job.output_item)
        .bind(job.output_qty)
        .bind(job.xp)
        .bind(&job.skill)
        .bind(job.started_at)
        .bind(job.ready_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Ok(job))
    }

    /// Every job this player owns at this station, slot order.
    pub async fn station_jobs(
        &self,
        station_id: &str,
        character_id: &str,
    ) -> Result<Vec<StationJob>, DbError> {
        let rows: Vec<StationJobRow> = sqlx::query_as(
            "SELECT id, station_id, character_id, slot, recipe_id, inputs_json, fuel_units, \
             output_item, output_qty, xp, skill, started_at, ready_at, state, fail_reason \
             FROM station_job WHERE station_id = ? AND character_id = ? ORDER BY slot",
        )
        .bind(station_id)
        .bind(character_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(StationJob::from).collect())
    }

    /// Mark every running job whose time has come as `ready`, and report them so
    /// the caller can notify the owners.
    ///
    /// Ripening is separate from collecting on purpose: the output waits in the
    /// slot, so a player who logged out mid-job comes back to a finished one
    /// rather than to a job that stopped when they did.
    pub async fn ripen_station_jobs(&self, now: i64) -> Result<Vec<StationJob>, DbError> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<StationJobRow> = sqlx::query_as(
            "SELECT id, station_id, character_id, slot, recipe_id, inputs_json, fuel_units, \
             output_item, output_qty, xp, skill, started_at, ready_at, state, fail_reason \
             FROM station_job WHERE state = 'running' AND ready_at <= ?",
        )
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;
        if !rows.is_empty() {
            sqlx::query("UPDATE station_job SET state = 'ready' WHERE state = 'running' AND ready_at <= ?")
                .bind(now)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|r| StationJob { state: "ready".to_string(), ..StationJob::from(r) })
            .collect())
    }

    /// Fail a job and refund exactly what it escrowed — the inputs recorded on
    /// the row, not what the recipe currently says they should have been.
    ///
    /// This is the path a job takes when its recipe vanishes from `crafting.toml`
    /// between restarts. The config is edited by hand and the row outlives it,
    /// so "the recipe is gone" is an ordinary Tuesday rather than an invariant
    /// violation, and it must never panic.
    ///
    /// The refund goes back to the slot, not to the inventory: a player whose
    /// pack is full when a job fails would otherwise lose the materials to the
    /// very error that was supposed to protect them.
    pub async fn fail_station_job(&self, job_id: &str, reason: &str) -> Result<bool, DbError> {
        let n = sqlx::query(
            "UPDATE station_job SET state = 'failed', fail_reason = ? \
             WHERE id = ? AND state <> 'failed'",
        )
        .bind(reason)
        .bind(job_id)
        .execute(&self.pool)
        .await?;
        Ok(n.rows_affected() > 0)
    }

    /// Collect a finished job: the output (or, for a failed one, the refunded
    /// inputs and fuel) moves into the player's inventory and the slot frees.
    ///
    /// **Compare-and-clear**, the pattern #142's listing purchase established:
    /// the DELETE carries the state in its WHERE clause, so two collect commands
    /// racing produce one payout and one "nothing to collect" rather than two
    /// payouts. Reading the row first and deleting after would be exactly the
    /// duplication bug that pattern exists to prevent.
    ///
    /// **A full pack does not destroy anything.** If the output doesn't fit, the
    /// job stays exactly where it is and the caller is told — the slot keeps
    /// holding it until there is room.
    pub async fn collect_station_job(
        &self,
        job_id: &str,
        character_id: &str,
        bonus_qty: i64,
        now: i64,
    ) -> Result<Result<CollectedJob, CollectError>, DbError> {
        let mut tx = self.pool.begin().await?;
        let row: Option<StationJobRow> = sqlx::query_as(
            "SELECT id, station_id, character_id, slot, recipe_id, inputs_json, fuel_units, \
             output_item, output_qty, xp, skill, started_at, ready_at, state, fail_reason \
             FROM station_job WHERE id = ? AND character_id = ?",
        )
        .bind(job_id)
        .bind(character_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(Err(CollectError::NoSuchJob));
        };
        let job = StationJob::from(row);
        if job.state == "running" {
            tx.commit().await?;
            return Ok(Err(CollectError::NotReady { ready_at: job.ready_at }));
        }

        // What is actually owed: the output, or the refund of a failed job.
        let failed = job.state == "failed";
        let payout: Vec<(String, i64)> = if failed {
            job.inputs.clone()
        } else {
            vec![(job.output_item.clone(), job.output_qty + bonus_qty.max(0))]
        };

        // Room for all of it, checked before any of it moves. A partial payout
        // would leave the slot half-collected with no way to describe that.
        let carried: Option<i64> =
            sqlx::query_scalar("SELECT SUM(qty) FROM inventory_item WHERE character_id = ?")
                .bind(character_id)
                .fetch_one(&mut *tx)
                .await?;
        let room = (MAX_CARRY - carried.unwrap_or(0)).max(0);
        let want: i64 = payout.iter().map(|(_, q)| *q).sum();
        if want > room {
            tx.commit().await?;
            return Ok(Err(CollectError::NoRoom { need: want, room }));
        }

        // Compare-and-clear: whoever's DELETE lands first owns the payout.
        let cleared = sqlx::query("DELETE FROM station_job WHERE id = ? AND state = ?")
            .bind(job_id)
            .bind(&job.state)
            .execute(&mut *tx)
            .await?;
        if cleared.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(Err(CollectError::NoSuchJob));
        }

        for (item, qty) in &payout {
            add_inventory_in_tx(&mut tx, character_id, item, *qty).await?;
        }
        // A failed job also hands back the fuel it reserved, to the station
        // rather than to the player — it was never theirs, it was the fire's.
        if failed && job.fuel_units > 0 {
            sqlx::query(
                "INSERT INTO station_fuel (station_id, units, updated_at) VALUES (?, ?, ?) \
                 ON CONFLICT(station_id) DO UPDATE SET units = units + excluded.units, \
                 updated_at = excluded.updated_at",
            )
            .bind(&job.station_id)
            .bind(job.fuel_units)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(Ok(CollectedJob {
            station_id: job.station_id,
            slot: job.slot,
            failed,
            fail_reason: job.fail_reason,
            payout,
            xp: if failed { 0 } else { job.xp },
            skill: job.skill,
        }))
    }

    /// Every job in the world that is not yet collected. The gateway uses this
    /// at boot to fail any whose recipe has vanished from config.
    pub async fn all_station_jobs(&self) -> Result<Vec<StationJob>, DbError> {
        let rows: Vec<StationJobRow> = sqlx::query_as(
            "SELECT id, station_id, character_id, slot, recipe_id, inputs_json, fuel_units, \
             output_item, output_qty, xp, skill, started_at, ready_at, state, fail_reason \
             FROM station_job ORDER BY started_at",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(StationJob::from).collect())
    }

    /// Hand in `cfg.required` trophies for `cfg.gold`, repeatably.
    ///
    /// Returns `(paid, held_after)` — the gold minted (0 if refused) and how
    /// many trophies remain, so the caller can tell a player where they are
    /// without a second query.
    ///
    /// **One transaction.** The trophies leave and the gold appears together, or
    /// neither happens: a crash between them would either eat ten pelts for
    /// nothing or pay twice on retry.
    ///
    /// **Deduped** through the same `market_command` table the market uses for
    /// exactly-once (#139). A resent frame must not mint a second hundred gold —
    /// this is a faucet, and a replayable faucet is a duplication bug that prints
    /// money rather than merely repeating an action.
    ///
    /// **All or nothing.** Nine trophies is not a turn-in: it is refused whole,
    /// consuming nothing. Part-paying would be the worst outcome, since the
    /// player would have lost goods and gained no bounty.
    pub async fn turn_in_bounty(
        &self,
        character_id: &str,
        cfg: &crate::market_config::BountyConfig,
        command_id: &str,
        now: i64,
    ) -> Result<(i64, i64), DbError> {
        let mut tx = self.pool.begin().await?;
        if !Self::claim_command_in_tx(&mut tx, command_id, character_id, now).await? {
            // Already paid for this command. Report what they hold so a client
            // that retried still renders the truth, and pay nothing.
            let held = inventory_qty_in_tx(&mut tx, character_id, &cfg.item_id).await?;
            tx.commit().await?;
            return Ok((0, held));
        }
        let held = inventory_qty_in_tx(&mut tx, character_id, &cfg.item_id).await?;
        if held < cfg.required {
            tx.commit().await?;
            return Ok((0, held));
        }
        let removed =
            remove_inventory_in_tx(&mut tx, character_id, &cfg.item_id, cfg.required).await?;
        if removed < cfg.required {
            // Shouldn't happen — we just counted them under the same
            // transaction — but paying for trophies that didn't actually leave
            // would mint gold from nothing. Roll back rather than trust it.
            tx.rollback().await?;
            return Ok((0, held));
        }
        // A genuine faucet: the city creates the coin it pays with, so it goes
        // on the supply ledger (#154) under its own reason. This is the largest
        // tap in the game, and the one most worth being able to see.
        mint_gold_in_tx(&mut tx, character_id, cfg.gold, "bounty", now).await?;
        tx.commit().await?;
        Ok((cfg.gold, held - cfg.required))
    }

    /// The money supply: every gold ever created minus every gold ever
    /// destroyed, straight off the append-only ledger (#154).
    ///
    /// This is the number that makes "is the economy balanced?" answerable. It
    /// must always equal purses plus escrowed gold — see
    /// [`Db::gold_supply_gap`], which is what a boot check and the tests
    /// actually assert.
    pub async fn gold_supply(&self) -> Result<i64, DbError> {
        let total: Option<i64> = sqlx::query_scalar("SELECT SUM(amount) FROM gold_ledger")
            .fetch_one(&self.pool)
            .await?;
        Ok(total.unwrap_or(0))
    }

    /// Gold created, grouped by why — the faucet breakdown a balance pass
    /// (#129) needs and could not previously get at all.
    pub async fn gold_by_reason(&self) -> Result<Vec<(String, i64)>, DbError> {
        sqlx::query_as::<_, (String, i64)>(
            "SELECT reason, SUM(amount) FROM gold_ledger GROUP BY reason ORDER BY reason",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// `ledger - (purses + escrow)`. **Zero, always.**
    ///
    /// Escrowed gold is deducted from a purse and held by an open buy order, so
    /// it is absent from `character.gold` but very much still in the world — the
    /// open buy book is its only record, which is why the book is part of the
    /// accounting rather than outside it.
    ///
    /// A nonzero result means gold was created or destroyed by a path that
    /// didn't tell the ledger. That is exactly the class of bug #154 exists to
    /// make impossible, so it is worth checking rather than assuming.
    pub async fn gold_supply_gap(&self) -> Result<i64, DbError> {
        let purses: Option<i64> = sqlx::query_scalar("SELECT SUM(gold) FROM character")
            .fetch_one(&self.pool)
            .await?;
        let escrow: Option<i64> = sqlx::query_scalar(
            "SELECT SUM(unit_price * qty_remaining) FROM market_order WHERE side = 'buy'",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(self.gold_supply().await? - purses.unwrap_or(0) - escrow.unwrap_or(0))
    }

    /// Fees burned at one market, split by kind — what a Phase 2 city treasury
    /// (#144) would be crediting instead, and the balance telemetry for
    /// whether the sink is sized sanely.
    pub async fn fees_by_kind(&self, market_id: &str) -> Result<Vec<(String, i64)>, DbError> {
        sqlx::query_as::<_, (String, i64)>(
            "SELECT kind, SUM(gold) FROM market_fee WHERE market_id = ? GROUP BY kind ORDER BY kind",
        )
        .bind(market_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Every invariant the book must satisfy on boot (#136 §8.3, issue #140),
    /// as a list of human-readable violations — empty means healthy.
    ///
    /// The caller treats a non-empty result as a **hard startup failure**, not
    /// a warning: every one of these means goods or gold have been duplicated
    /// or destroyed, and a market that has silently minted stock is worse than
    /// a market that won't start.
    ///
    /// Checks, across every market:
    /// - **goods escrow reconciles** — `locked` warehouse stock equals what the
    ///   open sell book promises, per item;
    /// - **no nonsense quantities** — nothing rests at zero or above its own
    ///   original size;
    /// - **the book doesn't cross between DIFFERENT owners** — if A's bid is at
    ///   or above B's ask, one of them should have matched instead of resting.
    ///   Crossing is legitimate when both sides are the SAME player: self-match
    ///   prevention deliberately skips those, so someone holding a bid at 8 and
    ///   an ask at 5 is a crossed book that is nonetheless correct.
    ///
    /// Gold escrow has no equivalent cross-check: escrowed gold is simply
    /// absent from purses, with the open buy book as its only record, so there
    /// is nothing independent to compare it against. Keeping the deduction and
    /// the order row in one transaction is what protects it.
    pub async fn book_health(&self) -> Result<Vec<String>, DbError> {
        let mut problems = Vec::new();

        let escrow = sqlx::query_as::<_, (String, String, i64, i64)>(
            "SELECT w.market_id, w.item_id, \
                COALESCE((SELECT SUM(qty_remaining) FROM market_order o \
                    WHERE o.market_id = w.market_id AND o.item_id = w.item_id AND o.side = 'sell'), 0), \
                COALESCE(SUM(w.qty), 0) \
             FROM market_warehouse_item w WHERE w.state = 'locked' \
             GROUP BY w.market_id, w.item_id",
        )
        .fetch_all(&self.pool)
        .await?;
        for (market, item, book, locked) in escrow {
            if book != locked {
                problems.push(format!(
                    "escrow drift at {market}/{item}: open sells promise {book}, warehouse holds {locked}"
                ));
            }
        }

        let bad = sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT id, qty_remaining, qty_total FROM market_order \
             WHERE qty_remaining <= 0 OR qty_remaining > qty_total",
        )
        .fetch_all(&self.pool)
        .await?;
        for (id, remaining, total) in bad {
            problems.push(format!("order {id} rests at {remaining} of {total}"));
        }

        // Only a cross between DIFFERENT owners is a fault — same-owner
        // crossings are what self-match prevention leaves behind by design.
        let crossed = sqlx::query_as::<_, (String, String, i64, i64)>(
            "SELECT b.market_id, b.item_id, MAX(b.unit_price), MIN(s.unit_price) \
             FROM market_order b JOIN market_order s \
               ON b.market_id = s.market_id AND b.item_id = s.item_id \
             WHERE b.side = 'buy' AND s.side = 'sell' \
               AND b.character_id <> s.character_id \
               AND b.unit_price >= s.unit_price \
             GROUP BY b.market_id, b.item_id",
        )
        .fetch_all(&self.pool)
        .await?;
        for (market, item, bid, ask) in crossed {
            problems.push(format!(
                "crossed book at {market}/{item}: a bid at {bid} rests against another player's ask at {ask}"
            ));
        }
        Ok(problems)
    }

    /// Total goods escrowed against open sell orders at a market, per item —
    /// the boot-time reconciliation (#136 §8.3): it must equal the `locked`
    /// stock in the warehouse, or escrow has drifted and something has been
    /// duplicated. Returned rather than asserted so the caller decides how
    /// loudly to fail.
    pub async fn escrow_reconciliation(
        &self,
        market_id: &str,
    ) -> Result<Vec<(String, i64, i64)>, DbError> {
        sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT item_id, \
                COALESCE((SELECT SUM(qty_remaining) FROM market_order o \
                    WHERE o.market_id = w.market_id AND o.item_id = w.item_id AND o.side = 'sell'), 0), \
                COALESCE(SUM(w.qty), 0) \
             FROM market_warehouse_item w \
             WHERE w.market_id = ? AND w.state = 'locked' GROUP BY w.item_id",
        )
        .bind(market_id)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn inventory_for_character(
        &self,
        character_id: &str,
    ) -> Result<Vec<InventoryItem>, DbError> {
        sqlx::query_as::<_, InventoryItem>(
            "SELECT id, character_id, item_id, qty, slot, durability FROM inventory_item \
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
        // Rent is paid to the city, which is to say to nobody — the gold is
        // destroyed. That made it a silent sink until #154; ledgered here it
        // becomes the counterweight to the faucets, and the supply identity
        // closes over rent as well as fees.
        ledger_gold_in_tx(&mut tx, character_id, -cost, "rent", now).await?;
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

    // --- Equipment (mining/abilities epic #123; instanced in #128) --------

    /// Arm a SPECIFIC owned instance — once tools carry their own durability
    /// (#128), "equip the pickaxe" is ambiguous the moment you own more
    /// than one, so the caller identifies exactly which row (whichever the
    /// player clicked); the slot it belongs in is derived from the
    /// instance's own item, same as the old item-id-based `equip` did.
    /// Returns `(slot, item_id)` on success; `None` if the instance doesn't
    /// exist, isn't owned by this character, or isn't equippable at all.
    pub async fn equip_instance(
        &self,
        character_id: &str,
        instance_id: &str,
    ) -> Result<Option<(&'static str, String)>, DbError> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT item_id, character_id FROM inventory_item WHERE id = ?",
        )
        .bind(instance_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((item_id, owner)) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let Some(slot) = world::equippable_slot(&item_id) else {
            tx.commit().await?;
            return Ok(None);
        };
        if owner != character_id {
            tx.commit().await?;
            return Ok(None);
        }
        sqlx::query(
            "INSERT INTO equipment (character_id, slot, item_id, instance_id) VALUES (?, ?, ?, ?) \
             ON CONFLICT(character_id, slot) DO UPDATE SET item_id = excluded.item_id, instance_id = excluded.instance_id",
        )
        .bind(character_id)
        .bind(slot)
        .bind(&item_id)
        .bind(instance_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some((slot, item_id)))
    }

    /// Clear whatever's armed in `slot`. A no-op (not an error) if nothing was.
    pub async fn unequip(&self, character_id: &str, slot: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM equipment WHERE character_id = ? AND slot = ?")
            .bind(character_id)
            .bind(slot)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// What's currently armed in `slot`, if anything — just the item id
    /// (cheap; used wherever only "which kind of tool" matters, not its
    /// wear). See [`Db::equipped_tool`] for the full instance + durability.
    pub async fn equipped(&self, character_id: &str, slot: &str) -> Result<Option<String>, DbError> {
        sqlx::query_scalar("SELECT item_id FROM equipment WHERE character_id = ? AND slot = ?")
            .bind(character_id)
            .bind(slot)
            .fetch_optional(&self.pool)
            .await
    }

    /// The full equipped tool in `slot` — instance id, item id, and live
    /// durability (#128) — for wear-down on a swing and for `equip.update`'s
    /// display. `None` if nothing's equipped (including a just-broken tool,
    /// which auto-unequips — see [`Db::wear_equipped_tool`]).
    pub async fn equipped_tool(&self, character_id: &str, slot: &str) -> Result<Option<EquippedTool>, DbError> {
        let row: Option<(String, String, i64)> = sqlx::query_as(
            "SELECT e.instance_id, i.item_id, i.durability FROM equipment e \
             JOIN inventory_item i ON i.id = e.instance_id \
             WHERE e.character_id = ? AND e.slot = ? AND e.instance_id IS NOT NULL",
        )
        .bind(character_id)
        .bind(slot)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(instance_id, item_id, durability)| {
            let max = world::tool_max_durability(&item_id).unwrap_or(durability);
            EquippedTool { instance_id, item_id, durability, max_durability: max }
        }))
    }

    /// Spend durability on whatever's equipped in `slot` after a successful
    /// swing (#128) — `loss` is the already-rolled amount (1 normally, 2 on
    /// a "rough" swing; the roll itself lives at the call site, since it's
    /// gameplay tuning, not a persistence concern). Clamped at 0; hitting 0
    /// auto-unequips (clears the equipment row) but leaves the instance in
    /// inventory as a repairable, durability-0 husk. `None` if nothing was
    /// actually equipped (a race with an unequip/reconnect — the caller
    /// already checked before the swing, so this should be rare).
    pub async fn wear_equipped_tool(
        &self,
        character_id: &str,
        slot: &str,
        loss: i64,
    ) -> Result<Option<WearOutcome>, DbError> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, i64)> = sqlx::query_as(
            "SELECT e.instance_id, i.durability FROM equipment e \
             JOIN inventory_item i ON i.id = e.instance_id \
             WHERE e.character_id = ? AND e.slot = ? AND e.instance_id IS NOT NULL",
        )
        .bind(character_id)
        .bind(slot)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((instance_id, durability)) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let remaining = (durability - loss).max(0);
        sqlx::query("UPDATE inventory_item SET durability = ? WHERE id = ?")
            .bind(remaining)
            .bind(&instance_id)
            .execute(&mut *tx)
            .await?;
        let broke = remaining <= 0;
        if broke {
            sqlx::query("DELETE FROM equipment WHERE character_id = ? AND slot = ?")
                .bind(character_id)
                .bind(slot)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(Some(WearOutcome { remaining, broke }))
    }

    /// Repair a specific owned tool instance (#128) to full durability, at a
    /// cost that scales with how worn it is ([`world::repair_cost`]) —
    /// consumed atomically alongside the durability restore, same
    /// check-then-spend shape as [`Db::craft`]. Works whether the instance
    /// is currently equipped or just sitting in the bag (a broken tool is
    /// always the latter, having auto-unequipped on the swing that broke
    /// it). Returns `None` if the instance doesn't exist, isn't owned by
    /// this character, isn't actually a tool, isn't missing any durability,
    /// or the character can't afford the repair.
    pub async fn repair_instance(
        &self,
        character_id: &str,
        instance_id: &str,
    ) -> Result<Option<RepairOutcome>, DbError> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String, i64)> = sqlx::query_as(
            "SELECT item_id, character_id, durability FROM inventory_item WHERE id = ?",
        )
        .bind(instance_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((item_id, owner, durability)) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let Some(max) = world::tool_max_durability(&item_id) else {
            tx.commit().await?;
            return Ok(None);
        };
        if owner != character_id {
            tx.commit().await?;
            return Ok(None);
        }
        let Some(cost) = world::repair_cost(&item_id, max - durability, max) else {
            tx.commit().await?;
            return Ok(None);
        };
        for (ingredient, qty) in &cost {
            let have: Option<i64> = sqlx::query_scalar(
                "SELECT SUM(qty) FROM inventory_item WHERE character_id = ? AND item_id = ?",
            )
            .bind(character_id)
            .bind(*ingredient)
            .fetch_one(&mut *tx)
            .await?;
            if have.unwrap_or(0) < *qty {
                tx.commit().await?;
                return Ok(None);
            }
        }
        for (ingredient, qty) in &cost {
            remove_inventory_in_tx(&mut tx, character_id, ingredient, *qty).await?;
        }
        sqlx::query("UPDATE inventory_item SET durability = ? WHERE id = ?")
            .bind(max)
            .bind(instance_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(RepairOutcome {
            item_id,
            cost: cost.into_iter().map(|(i, q)| (i.to_string(), q)).collect(),
        }))
    }

    /// Total carried quantity of one item (0 if none) — a cheap point
    /// lookup for "do they have any of X at all" without pulling the whole
    /// inventory. Used by the quarry foreman's "already has a pick?" gate
    /// (mining/abilities epic #123, #118).
    pub async fn inventory_qty(&self, character_id: &str, item_id: &str) -> Result<i64, DbError> {
        let qty: Option<i64> = sqlx::query_scalar(
            "SELECT SUM(qty) FROM inventory_item WHERE character_id = ? AND item_id = ?",
        )
        .bind(character_id)
        .bind(item_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(qty.unwrap_or(0))
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
        path_json: Option<&str>,
    ) -> Result<BuildOrder, DbError> {
        let id = Uuid::new_v4().to_string();
        let (structure_kind, x, y, x1, y1) = match &placement {
            Some(p) => (Some(p.structure_kind.as_str()), Some(p.x), Some(p.y), p.x1, p.y1),
            None => (None, None, None, None, None),
        };
        sqlx::query(
            "INSERT INTO build_order \
             (id, district, kind, required_json, progress_json, state, issued_at, required_skill, required_level, \
              structure_kind, x, y, x1, y1, path_json) \
             VALUES (?, ?, ?, ?, '{}', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(path_json)
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
            path_json: path_json.map(|s| s.to_string()),
        })
    }

    /// Persist a road plan's cells (#132), in path order starting at index 0.
    /// Called right after `insert_build_order` for a fresh road plan — the
    /// order's `required_json` is already the sum of every cell's cost (the
    /// caller computed both from the same `cut_road_cells` split), so this
    /// never needs to touch the order row itself.
    pub async fn insert_road_cells(&self, order_id: &str, cells: &[RoadCellSpec]) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        for (i, c) in cells.iter().enumerate() {
            sqlx::query(
                "INSERT INTO road_cell (order_id, cell_index, x0, y0, x1, y1, required_json, progress_json) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, '{}')",
            )
            .bind(order_id)
            .bind(i as i64)
            .bind(c.x0)
            .bind(c.y0)
            .bind(c.x1)
            .bind(c.y1)
            .bind(&c.required_json)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// A road order's cells, in path order (#132).
    pub async fn road_cells_for_order(&self, order_id: &str) -> Result<Vec<RoadCell>, DbError> {
        sqlx::query_as::<_, RoadCell>(
            "SELECT * FROM road_cell WHERE order_id = ? ORDER BY cell_index",
        )
        .bind(order_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Contribute up to `qty` of `item_id` into ONE cell of a road order
    /// (#132) — the cell-scoped sibling of [`Db::contribute`]. The caller
    /// (the gateway, #133) resolves `cell_index` from the contributor's
    /// position; this only trusts that it names an existing, unfinished
    /// cell of an open road order. Keeps the order's own `progress_json`
    /// mirrored in lockstep (same map, summed across cells) so every
    /// existing consumer that reads the order's aggregate — the board list,
    /// `settle_demolition`'s refund — keeps working unchanged.
    pub async fn contribute_to_road_cell(
        &self,
        character_id: &str,
        order_id: &str,
        cell_index: i64,
        item_id: &str,
        qty: i64,
        wage_per_unit: i64,
    ) -> Result<CellContributeResult, DbError> {
        let mut tx = self.pool.begin().await?;
        let Some(order) = sqlx::query_as::<_, BuildOrder>("SELECT * FROM build_order WHERE id = ?")
            .bind(order_id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.commit().await?;
            return Ok(CellContributeResult::default());
        };
        let Some(cell) = sqlx::query_as::<_, RoadCell>(
            "SELECT * FROM road_cell WHERE order_id = ? AND cell_index = ?",
        )
        .bind(order_id)
        .bind(cell_index)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.commit().await?;
            return Ok(CellContributeResult::default());
        };

        let cell_required = parse_cost(&cell.required_json);
        let mut cell_progress = parse_cost(&cell.progress_json);
        let mut result = CellContributeResult {
            required: cell_required.clone(),
            progress: cell_progress.clone(),
            kind: order.kind.clone(),
            district: order.district.clone(),
            placement: order.placement(),
            ..Default::default()
        };

        if order.state != "open" || cell.completed_at.is_some() || qty <= 0 {
            tx.commit().await?;
            return Ok(result);
        }

        let need = cell_required
            .get(item_id)
            .copied()
            .unwrap_or(0)
            .saturating_sub(cell_progress.get(item_id).copied().unwrap_or(0))
            .max(0);
        let carried: Option<i64> = sqlx::query_scalar(
            "SELECT SUM(qty) FROM inventory_item WHERE character_id = ? AND item_id = ?",
        )
        .bind(character_id)
        .bind(item_id)
        .fetch_one(&mut *tx)
        .await?;
        let moved = qty.min(need).min(carried.unwrap_or(0)).max(0);
        if moved == 0 {
            tx.commit().await?;
            return Ok(result);
        }

        remove_inventory_in_tx(&mut tx, character_id, item_id, moved).await?;
        *cell_progress.entry(item_id.to_string()).or_insert(0) += moved;
        let cell_completed = cell_required
            .iter()
            .all(|(k, v)| cell_progress.get(k).copied().unwrap_or(0) >= *v);
        sqlx::query(
            "UPDATE road_cell SET progress_json = ?, completed_at = ? WHERE order_id = ? AND cell_index = ?",
        )
        .bind(dump_cost(&cell_progress))
        .bind(cell_completed.then_some(now_secs()))
        .bind(order_id)
        .bind(cell_index)
        .execute(&mut *tx)
        .await?;

        let mut order_progress = parse_cost(&order.progress_json);
        *order_progress.entry(item_id.to_string()).or_insert(0) += moved;
        sqlx::query("UPDATE build_order SET progress_json = ? WHERE id = ?")
            .bind(dump_cost(&order_progress))
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
        // Wages (#145), in this same transaction — see `mint_gold_in_tx`. This
        // is a genuine faucet: the city creates the coin it pays with, so it
        // goes on the supply ledger (#154), not merely into a purse.
        result.wages = moved * wage_per_unit.max(0);
        mint_gold_in_tx(&mut tx, character_id, result.wages, "build_wage", now_secs()).await?;

        let order_required = parse_cost(&order.required_json);
        let order_completed = !order_required.is_empty()
            && order_required
                .iter()
                .all(|(k, v)| order_progress.get(k).copied().unwrap_or(0) >= *v);
        let mut contributors = Vec::new();
        if order_completed {
            sqlx::query("UPDATE build_order SET state = 'completed', completed_at = ? WHERE id = ?")
                .bind(now_secs())
                .bind(order_id)
                .execute(&mut *tx)
                .await?;
            contributors = sqlx::query_as::<_, (String, i64)>(
                "SELECT character_id, units FROM build_contribution WHERE order_id = ? ORDER BY character_id",
            )
            .bind(order_id)
            .fetch_all(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        result.moved = moved;
        result.progress = cell_progress;
        result.cell_completed = cell_completed;
        result.order_completed = order_completed;
        result.order_required = order_required;
        result.order_progress = order_progress;
        result.contributors = contributors;
        Ok(result)
    }

    /// Whether any build order of this `kind` has been completed, anywhere in
    /// the world. Used by seeding to decide whether a newly-authored dependent's
    /// prerequisite is already satisfied — deliberately world-wide rather than
    /// per-district, because a prereq names a KIND, not a place.
    pub async fn is_build_kind_completed(&self, kind: &str) -> Result<bool, DbError> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM build_order WHERE kind = ? AND state = 'completed'",
        )
        .bind(kind)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
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
    ///
    /// `wage_per_unit` (#145) is paid into the contributor's purse **in this same
    /// transaction**, on the units that actually moved — never on the qty offered,
    /// which is routinely larger. The caller owns the policy: it passes 0 for orders
    /// that shouldn't pay (demolitions), so the rate const stays in `proxy.rs` with
    /// every other tuning value.
    pub async fn contribute(
        &self,
        character_id: &str,
        order_id: &str,
        item_id: &str,
        qty: i64,
        wage_per_unit: i64,
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
            wages: 0,
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
            // Wages (#145), in this same transaction — see `grant_gold_in_tx`.
            result.wages = moved * wage_per_unit.max(0);
            // A genuine faucet — the city creates the coin it pays with — so it
            // goes on the supply ledger (#154), not merely into a purse.
            mint_gold_in_tx(&mut tx, character_id, result.wages, "build_wage", now_secs()).await?;
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
                //
                // ...UNLESS that prerequisite is ALREADY complete. The unlock
                // fires on the completion *event* (`build.completed` walks the
                // authored dependents), so an order added to a world that
                // finished its prerequisite before this order existed would seed
                // `locked` and stay that way forever, with nothing left to
                // trigger it. That is not hypothetical: the second market (#153)
                // was authored into worlds whose capital market was already
                // built. Seeding is the only place with the whole picture, so it
                // resolves the edge here rather than leaving dead content.
                let prereq_done = match o.prereq {
                    None => true,
                    Some(kind) => self.is_build_kind_completed(kind).await?,
                };
                let state = if prereq_done { "open" } else { "locked" };
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
                    None,
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

    /// Re-route an OPEN road order (`road.replan`, #104): transactionally
    /// swap in the new path/placement/district and the recomputed cost,
    /// keeping contributed progress (it's stone in the project, not the old
    /// geometry). If the kept progress already covers the recomputed cost —
    /// a part-built road re-routed much shorter — the order completes right
    /// here (state flip + contributor list), so the caller can run the
    /// ordinary completion announcements; any excess progress beyond the new
    /// cost is absorbed (rare, small, and #106's demolition is the doorway
    /// for getting stone back out of a road).
    ///
    /// The `state = 'open'` guard makes the swap race-safe against a
    /// concurrent completing contribution: `applied == false` means the
    /// order changed under the editor, who just retries.
    pub async fn replan_road_order(
        &self,
        order_id: &str,
        district: &str,
        required_json: &str,
        path_json: &str,
        placement: &BuildPlacement,
        cells: &[RoadCellSpec],
        now: i64,
    ) -> Result<ReplanOutcome, DbError> {
        let mut tx = self.pool.begin().await?;
        let applied = sqlx::query(
            "UPDATE build_order SET district = ?, required_json = ?, path_json = ?, \
             structure_kind = ?, x = ?, y = ?, x1 = ?, y1 = ? \
             WHERE id = ? AND state = 'open' AND path_json IS NOT NULL",
        )
        .bind(district)
        .bind(required_json)
        .bind(path_json)
        .bind(&placement.structure_kind)
        .bind(placement.x)
        .bind(placement.y)
        .bind(placement.x1)
        .bind(placement.y1)
        .bind(order_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !applied {
            tx.commit().await?;
            return Ok(ReplanOutcome::default());
        }

        // Re-cut the cells (#132). A new cell whose span exactly matches an
        // OLD cell's span keeps that cell's progress/completion (capped to
        // the new cost, in case the redistribution shrank it) — cells
        // untouched by the edit (the common case: dragging one waypoint
        // only changes the runs touching it) come out with identical spans
        // and so keep their state for free; anything the edit actually
        // reshaped starts fresh. Progress on a dropped cell isn't lost: the
        // order's own `progress_json` (updated below, unchanged from
        // before) still carries it, same as any other over-collected stone
        // on a replan today.
        let old_cells = sqlx::query_as::<_, RoadCell>(
            "SELECT * FROM road_cell WHERE order_id = ? ORDER BY cell_index",
        )
        .bind(order_id)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM road_cell WHERE order_id = ?")
            .bind(order_id)
            .execute(&mut *tx)
            .await?;
        for (i, c) in cells.iter().enumerate() {
            let carried = old_cells.iter().find(|o| {
                o.x0 == c.x0 && o.y0 == c.y0 && o.x1 == c.x1 && o.y1 == c.y1
            });
            let new_required = parse_cost(&c.required_json);
            let (progress_json, completed_at) = match carried {
                Some(o) => {
                    let mut p = parse_cost(&o.progress_json);
                    for (k, v) in p.iter_mut() {
                        if let Some(cap) = new_required.get(k) {
                            *v = (*v).min(*cap);
                        }
                    }
                    let done = !new_required.is_empty()
                        && new_required.iter().all(|(k, v)| p.get(k).copied().unwrap_or(0) >= *v);
                    (dump_cost(&p), if done { Some(now) } else { None })
                }
                None => ("{}".to_string(), None),
            };
            sqlx::query(
                "INSERT INTO road_cell (order_id, cell_index, x0, y0, x1, y1, required_json, progress_json, completed_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(order_id)
            .bind(i as i64)
            .bind(c.x0)
            .bind(c.y0)
            .bind(c.x1)
            .bind(c.y1)
            .bind(&c.required_json)
            .bind(&progress_json)
            .bind(completed_at)
            .execute(&mut *tx)
            .await?;
        }

        let order = sqlx::query_as::<_, BuildOrder>("SELECT * FROM build_order WHERE id = ?")
            .bind(order_id)
            .fetch_one(&mut *tx)
            .await?;
        let required = parse_cost(&order.required_json);
        let progress = parse_cost(&order.progress_json);
        let completed = !required.is_empty()
            && required.iter().all(|(k, v)| progress.get(k).copied().unwrap_or(0) >= *v);
        let mut contributors = Vec::new();
        if completed {
            sqlx::query("UPDATE build_order SET state = 'completed', completed_at = ? WHERE id = ?")
                .bind(now)
                .bind(order_id)
                .execute(&mut *tx)
                .await?;
            contributors = sqlx::query_as::<_, (String, i64)>(
                "SELECT character_id, units FROM build_contribution WHERE order_id = ? ORDER BY character_id",
            )
            .bind(order_id)
            .fetch_all(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(ReplanOutcome { applied: true, completed, contributors })
    }

    /// Cancel a pristine road plan (`road.cancel`, #106): only an OPEN road
    /// order with ZERO progress goes — anything with contributed stone must
    /// take the demolition route so players' hauling is never vaporised.
    /// Returns whether the row was removed; `false` = wrong state/progress
    /// (the caller rejects with a pointer to demolition).
    pub async fn cancel_road_order(&self, order_id: &str) -> Result<bool, DbError> {
        let mut tx = self.pool.begin().await?;
        let eligible: Option<String> = sqlx::query_scalar(
            "SELECT id FROM build_order WHERE id = ? AND state = 'open' \
             AND path_json IS NOT NULL AND progress_json = '{}'",
        )
        .bind(order_id)
        .fetch_optional(&mut *tx)
        .await?;
        if eligible.is_none() {
            tx.commit().await?;
            return Ok(false);
        }
        // Children before parent — `road_cell.order_id` references
        // `build_order(id)` and this DB enforces foreign keys.
        sqlx::query("DELETE FROM road_cell WHERE order_id = ?")
            .bind(order_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM build_order WHERE id = ?")
            .bind(order_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Create a demolition order for a road (`road.demolish`, #106),
    /// transactionally:
    /// - the target must be a road order that's either COMPLETED (a built
    ///   road) or OPEN WITH PROGRESS (a part-built plan; pristine plans are
    ///   for `road.cancel`);
    /// - an open plan flips to state `demolishing` so contributions stop
    ///   (contribute() only feeds `open` orders) and its stakes drop off the
    ///   boards; a built road keeps rendering until the demolition finishes;
    /// - the demolition order's kind is `demo_<target order id>` — the link
    ///   back — and doubles as the double-demolition guard;
    /// - the demo order carries the road's path (so the ordinary
    ///   contribution proximity gate means the work happens on site) but NO
    ///   placement — completing it must never spawn a structure.
    pub async fn create_demolition(
        &self,
        target_order_id: &str,
        now: i64,
    ) -> Result<Result<BuildOrder, &'static str>, DbError> {
        let mut tx = self.pool.begin().await?;
        let Some(target) = sqlx::query_as::<_, BuildOrder>("SELECT * FROM build_order WHERE id = ?")
            .bind(target_order_id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.commit().await?;
            return Ok(Err("no such order"));
        };
        let Some(path_json) = target.path_json.clone() else {
            tx.commit().await?;
            return Ok(Err("that order is not a road"));
        };
        let demo_kind = format!("demo_{}", target.id);
        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM build_order WHERE kind = ? LIMIT 1")
                .bind(&demo_kind)
                .fetch_optional(&mut *tx)
                .await?;
        if existing.is_some() {
            tx.commit().await?;
            return Ok(Err("a demolition for that road is already posted"));
        }
        match target.state.as_str() {
            "completed" => {} // a built road: keeps rendering until the job's done
            "open" => {
                if target.progress_json == "{}" {
                    tx.commit().await?;
                    return Ok(Err("nothing built there yet — cancel the plan instead"));
                }
                sqlx::query("UPDATE build_order SET state = 'demolishing' WHERE id = ? AND state = 'open'")
                    .bind(&target.id)
                    .execute(&mut *tx)
                    .await?;
            }
            _ => {
                tx.commit().await?;
                return Ok(Err("that road can't be demolished right now"));
            }
        }
        let demo_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO build_order (id, district, kind, required_json, progress_json, state, issued_at, path_json) \
             VALUES (?, ?, ?, ?, '{}', 'open', ?, ?)",
        )
        .bind(&demo_id)
        .bind(&target.district)
        .bind(&demo_kind)
        .bind(r#"{"tool_kit":1}"#)
        .bind(now)
        .bind(&path_json)
        .execute(&mut *tx)
        .await?;
        let demo = sqlx::query_as::<_, BuildOrder>("SELECT * FROM build_order WHERE id = ?")
            .bind(&demo_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Ok(demo))
    }

    /// Finish a completed demolition (#106): compute the refund from the
    /// target (a built road refunds its full required cost; a part-built
    /// plan refunds its contributed progress), delete the target order (and
    /// its contribution rows), and return the refund map. The caller pays it
    /// out and broadcasts the removal.
    pub async fn settle_demolition(
        &self,
        target_order_id: &str,
    ) -> Result<Option<(BuildOrder, BTreeMap<String, i64>)>, DbError> {
        let mut tx = self.pool.begin().await?;
        let Some(target) = sqlx::query_as::<_, BuildOrder>("SELECT * FROM build_order WHERE id = ?")
            .bind(target_order_id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let refund = if target.state == "completed" {
            parse_cost(&target.required_json)
        } else {
            parse_cost(&target.progress_json)
        };
        sqlx::query("DELETE FROM build_contribution WHERE order_id = ?")
            .bind(&target.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM road_cell WHERE order_id = ?")
            .bind(&target.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM build_order WHERE id = ?")
            .bind(&target.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some((target, refund)))
    }

    /// Mint items straight into a character's town storage (#106's refunds
    /// — storage sidesteps the carry cap, and it's the established safe
    /// stash). Not a transfer: demolition returns what the road absorbed.
    pub async fn grant_storage(&self, character_id: &str, item_id: &str, qty: i64) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        add_storage_in_tx(&mut tx, character_id, item_id, qty).await?;
        tx.commit().await?;
        Ok(())
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
            .insert_build_order("market", "town_well", r#"{"wood":20}"#, "open", 100, None, 0, None, None)
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
        // The Market (#137) is the one authored order — seeded `open`, i.e.
        // still to be built. Everything else is mayor-commissioned at runtime.
        let seeded = db.build_orders_for_district("civic").await.unwrap();
        assert_eq!(seeded.len(), 1);
        assert_eq!((seeded[0].kind.as_str(), seeded[0].state.as_str()), ("market", "open"));

        // Re-seed (simulating a restart): no duplicate plots, and no duplicate
        // build orders — each authored kind is created at most once per district.
        db.seed_capital(&cap, 200).await.unwrap();
        assert_eq!(db.plot_count().await.unwrap(), plots);
        assert_eq!(
            db.build_orders_for_district("civic").await.unwrap().len(), 1,
            "re-seeding must not duplicate the market"
        );

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
            .insert_build_order("civic", "town_well", r#"{"wood":20,"stone":10}"#, "open", 0, None, 0, None, None)
            .await
            .unwrap();

        let a = a_character(&db).await;
        let b = a_character(&db).await;
        db.add_to_inventory(&a, "wood", 30).await.unwrap();
        db.add_to_inventory(&b, "wood", 5).await.unwrap();
        db.add_to_inventory(&b, "stone", 20).await.unwrap();

        // A contributes wood: bounded by the order's need (20), not the 30 carried.
        let r = db.contribute(&a, &order.id, "wood", 30, 0).await.unwrap();
        assert_eq!(r.moved, 20, "capped at the wood requirement");
        assert!(!r.completed, "stone still outstanding");
        assert_eq!(r.progress.get("wood"), Some(&20));
        assert_eq!(db.inventory_total(&a).await.unwrap(), 10, "unspent wood stays carried");

        // Wood is already met: a further wood contribution moves nothing.
        assert_eq!(db.contribute(&b, &order.id, "wood", 5, 0).await.unwrap().moved, 0);

        // B finishes the stone (bounded to the 10 needed) → completes the order.
        let done = db.contribute(&b, &order.id, "stone", 20, 0).await.unwrap();
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
        assert_eq!(db.contribute(&a, &order.id, "stone", 1, 0).await.unwrap().moved, 0);
    }

    /// A market to warehouse against — the warehouse is keyed by a completed
    /// market order's id, so tests need a real order row to reference.
    async fn a_market(db: &Db) -> String {
        db.insert_build_order("civic", "market", r#"{"wood":1}"#, "completed", 0, None, 0, None, None)
            .await
            .unwrap()
            .id
    }

    /// A second market, in the Market District (#153). Distinct `market_id`,
    /// which is the only thing separating the two — so these tests are exactly
    /// the ones that decide whether the keying that has existed since #137 is
    /// real or merely untested.
    async fn another_market(db: &Db) -> String {
        db.insert_build_order("market", "market_east", r#"{"wood":1}"#, "completed", 0, None, 0, None, None)
            .await
            .unwrap()
            .id
    }

    /// #139-shaped shims over #140's unified `place_order`, so the tests that
    /// pinned the original sell-rests / buy-crosses behaviour keep guarding it
    /// verbatim. `NO_EXPIRY` keeps them out of the sweep's way.
    const NO_EXPIRY: i64 = i64::MAX;

    /// The tuning these tests run against: the values #136-#143 shipped, never
    /// the repo's `market.toml` (#152). A suite whose expected fees shifted when
    /// somebody tuned a live config file would be worse than no suite —
    /// `market_config`'s own tests cover loading and overriding.
    ///
    /// `max_open_orders` is raised well above the shipped 40 because most of
    /// these tests build deep books on purpose; the cap itself is pinned by
    /// `the_open_order_cap_is_enforced`, which sets its own.
    fn test_cfg() -> MarketConfig {
        MarketConfig { max_open_orders: 1000, ..MarketConfig::default() }
    }

    async fn sell(db: &Db, m: &str, who: &str, item: &str, price: i64, qty: i64) -> BuyOutcome {
        db.place_order(m, who, "sell", item, price, qty, NO_EXPIRY, &test_cfg(), "", 0)
            .await
            .unwrap()
    }

    async fn buy(db: &Db, m: &str, who: &str, item: &str, price: i64, qty: i64) -> BuyOutcome {
        db.place_order(m, who, "buy", item, price, qty, NO_EXPIRY, &test_cfg(), "", 0)
            .await
            .unwrap()
    }

    /// A seller stocked at a market, ready to rest orders.
    async fn a_seller(db: &Db, market: &str, item: &str, qty: i64) -> String {
        let cid = a_character(db).await;
        db.add_to_inventory(&cid, item, qty).await.unwrap();
        db.warehouse_deposit(market, &cid, item, qty, 60).await.unwrap();
        cid
    }

    #[tokio::test]
    async fn a_buy_sweeps_cheapest_first_and_pays_the_resting_price() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        // Two sellers, three price levels — deliberately placed out of price
        // order so "cheapest first" can't pass by accident of insertion order.
        let s1 = a_seller(&db, &m, "wood", 30).await;
        let s2 = a_seller(&db, &m, "wood", 30).await;
        sell(&db, &m, &s1, "wood", 9, 5).await;
        sell(&db, &m, &s2, "wood", 7, 4).await;
        sell(&db, &m, &s1, "wood", 8, 6).await;

        let buyer = a_character(&db).await;
        let buyer_start = db.character_gold(&buyer).await.unwrap();
        let s1_start = db.character_gold(&s1).await.unwrap();
        let s2_start = db.character_gold(&s2).await.unwrap();

        // Bid 10 for 12: sweeps 4@7, then 6@8, then 2@9 — paying each RESTING
        // price, never the 10 bid.
        let out = buy(&db, &m, &buyer, "wood", 10, 12).await;
        assert_eq!(out.filled, 12);
        let expected = 4 * 7 + 6 * 8 + 2 * 9;
        assert_eq!(out.spent, expected, "each fill at the resting price, not the 10 bid");
        assert!(out.spent < 12 * 10, "the aggressor keeps the price improvement");
        // The buyer also paid a listing fee to place the order (#141).
        assert_eq!(out.listing_fee, test_cfg().listing_fee(12 * 10));
        assert_eq!(
            db.character_gold(&buyer).await.unwrap(),
            buyer_start - expected - out.listing_fee
        );
        // Sellers are paid immediately, each for their own fills — net of the
        // sale tax on each fill. (Their listing fees were charged before these
        // balances were captured.)
        assert_eq!(
            db.character_gold(&s2).await.unwrap(),
            s2_start + (4 * 7 - test_cfg().sale_tax(4 * 7)),
            "seller receives the fill minus sale tax"
        );
        assert_eq!(
            db.character_gold(&s1).await.unwrap(),
            s1_start + (6 * 8 - test_cfg().sale_tax(6 * 8)) + (2 * 9 - test_cfg().sale_tax(2 * 9)),
            "taxed per fill, not once on the total"
        );

        // The 9-level order is partially filled and still resting; the others
        // are gone. The book only ever holds live orders.
        let asks = db.book_for(&m, "wood", "sell").await.unwrap();
        assert_eq!(asks, vec![BookLevel { unit_price: 9, qty: 3 }]);

        // Goods landed in the BUYER's warehouse at this market.
        let held: i64 = db.warehouse_for_character(&m, &buyer).await.unwrap()
            .iter().filter(|r| r.item_id == "wood" && r.state == "available").map(|r| r.qty).sum();
        assert_eq!(held, 12);

        // Three fills on the append-only ledger, at execution prices, each
        // carrying the tax that was taken from it (#141).
        let trades = db.recent_trades(&m, "wood", 10).await.unwrap();
        assert_eq!(trades.len(), 3);
        let mut prices: Vec<i64> = trades.iter().map(|t| t.unit_price).collect();
        prices.sort();
        assert_eq!(prices, vec![7, 8, 9]);
        for t in &trades {
            assert_eq!(
                t.sale_tax_gold, test_cfg().sale_tax(t.unit_price * t.qty),
                "each ledger row records its own tax, so fee revenue reconciles"
            );
        }
    }

    #[tokio::test]
    async fn a_buy_below_the_ask_rests_and_escrows_instead_of_filling() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let seller = a_seller(&db, &m, "wood", 40).await;
        sell(&db, &m, &seller, "wood", 5, 10).await;
        sell(&db, &m, &seller, "wood", 50, 10).await;

        // A bid below every ask no longer just evaporates (#139's behaviour) —
        // it RESTS, escrowing the gold it would need at its own limit.
        let buyer = a_character(&db).await;
        let start = db.character_gold(&buyer).await.unwrap();
        let out = buy(&db, &m, &buyer, "wood", 4, 10).await;
        assert_eq!((out.filled, out.spent), (0, 0), "nothing crossed");
        assert_eq!(out.resting_qty, 10);
        let fee = test_cfg().listing_fee(10 * 4);
        assert_eq!(out.listing_fee, fee);
        assert_eq!(
            db.character_gold(&buyer).await.unwrap(), start - 40 - fee,
            "escrowed 10 x 4, plus the listing fee"
        );
        assert_eq!(db.book_for(&m, "wood", "buy").await.unwrap(), vec![BookLevel { unit_price: 4, qty: 10 }]);

        // Cancelling gives every escrowed coin back — but NOT the listing fee
        // (#141). Paying to post an order is the cost of posting it, whether or
        // not you then change your mind.
        let mine = db.open_orders_for_character(&m, &buyer).await.unwrap();
        db.cancel_order(&buyer, &mine[0].id).await.unwrap();
        assert_eq!(
            db.character_gold(&buyer).await.unwrap(), start - fee,
            "escrow returned, listing fee kept"
        );

        // A bid crossing only the cheap level takes that level and rests the
        // rest of its size at its own price.
        let out = buy(&db, &m, &buyer, "wood", 5, 15).await;
        assert_eq!((out.filled, out.spent), (10, 50));
        assert_eq!(out.resting_qty, 5, "the uncrossed remainder rests");

        // Gold bounds the ESCROW, so an order is sized to what can be afforded.
        db.cancel_order(&buyer, &db.open_orders_for_character(&m, &buyer).await.unwrap()[0].id)
            .await
            .unwrap();
        let purse = db.character_gold(&buyer).await.unwrap();
        let out = buy(&db, &m, &buyer, "wood", 50, 100).await;
        assert_eq!(out.filled + out.resting_qty, purse / 50, "sized by the purse, not the ask");
    }

    #[tokio::test]
    async fn you_cannot_trade_with_your_own_resting_order() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cid = a_seller(&db, &m, "wood", 20).await;
        sell(&db, &m, &cid, "wood", 5, 10).await;
        let gold = db.character_gold(&cid).await.unwrap();

        // Crossing your own ask matches nothing — and the ask SURVIVES, rather
        // than being cancelled out from under you.
        let out = buy(&db, &m, &cid, "wood", 99, 10).await;
        assert_eq!((out.filled, out.spent), (0, 0), "self-match is skipped, not executed");
        assert_eq!(db.book_for(&m, "wood", "sell").await.unwrap().len(), 1, "the ask survives");

        // The buy rested instead, so gold moved into escrow — not to yourself.
        // Cancelling it proves nothing was actually spent.
        assert!(out.resting_qty > 0, "the unmatched buy rests");
        let mine = db.open_orders_for_character(&m, &cid).await.unwrap();
        let buy_order = mine.iter().find(|o| o.side == "buy").unwrap();
        db.cancel_order(&cid, &buy_order.id).await.unwrap();
        assert_eq!(
            db.character_gold(&cid).await.unwrap(), gold - out.listing_fee,
            "no wash trade: escrow came back, only the listing fee was spent"
        );
    }

    #[tokio::test]
    async fn a_resting_buy_is_filled_by_a_later_seller_at_the_bid() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;

        // A buyer rests a bid at 9 with nothing to cross.
        let buyer = a_character(&db).await;
        let buyer_start = db.character_gold(&buyer).await.unwrap();
        let out = buy(&db, &m, &buyer, "wood", 9, 10).await;
        assert_eq!(out.resting_qty, 10);
        let bid_fee = out.listing_fee;
        assert_eq!(db.character_gold(&buyer).await.unwrap(), buyer_start - 90 - bid_fee);

        // A seller arrives asking 6 — below the bid, so it crosses. The RESTING
        // order's price wins, so the seller is paid 9, not their own 6.
        let seller = a_seller(&db, &m, "wood", 20).await;
        let seller_start = db.character_gold(&seller).await.unwrap();
        let out = sell(&db, &m, &seller, "wood", 6, 4).await;
        assert_eq!(out.filled, 4);
        assert_eq!(out.earned, 36, "4 x the resting bid of 9, not the 6 asked");
        // Net of the sale tax on that fill, and of what they paid to list.
        assert_eq!(
            db.character_gold(&seller).await.unwrap(),
            seller_start + 36 - test_cfg().sale_tax(36) - out.listing_fee
        );

        // The buyer's escrow covered it exactly — no further charge.
        assert_eq!(db.character_gold(&buyer).await.unwrap(), buyer_start - 90 - bid_fee);
        let held: i64 = db.warehouse_for_character(&m, &buyer).await.unwrap()
            .iter().filter(|r| r.item_id == "wood").map(|r| r.qty).sum();
        assert_eq!(held, 4, "goods landed with the resting buyer");
        assert_eq!(db.book_for(&m, "wood", "buy").await.unwrap(), vec![BookLevel { unit_price: 9, qty: 6 }]);
    }

    #[tokio::test]
    async fn the_book_never_crosses_and_sweeps_in_price_time_order() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let s1 = a_seller(&db, &m, "wood", 30).await;
        let s2 = a_seller(&db, &m, "wood", 30).await;

        // Two asks at the SAME price; s1's is older, so it must fill first.
        sell(&db, &m, &s1, "wood", 7, 5).await;
        sell(&db, &m, &s2, "wood", 7, 5).await;
        let s1_gold = db.character_gold(&s1).await.unwrap();
        let s2_gold = db.character_gold(&s2).await.unwrap();

        let buyer = a_character(&db).await;
        buy(&db, &m, &buyer, "wood", 7, 5).await;
        assert_eq!(
            db.character_gold(&s1).await.unwrap(), s1_gold + 35 - test_cfg().sale_tax(35),
            "oldest at the level fills first"
        );
        assert_eq!(db.character_gold(&s2).await.unwrap(), s2_gold, "the newer one is untouched");

        // After every command the book must not cross between different
        // owners: a resting order can only survive because nothing on the
        // other side was both crossable AND someone else's. (`book_health`
        // encodes exactly that, including the same-owner exemption that
        // self-match prevention deliberately creates.)
        for _ in 0..5 {
            buy(&db, &m, &buyer, "wood", 6, 3).await;
            sell(&db, &m, &s1, "wood", 8, 3).await;
            let bids = db.book_for(&m, "wood", "buy").await.unwrap();
            let asks = db.book_for(&m, "wood", "sell").await.unwrap();
            if let (Some(b), Some(a)) = (bids.first(), asks.first()) {
                assert!(b.unit_price < a.unit_price, "book crossed: bid {} >= ask {}", b.unit_price, a.unit_price);
            }
            assert!(db.book_health().await.unwrap().is_empty(), "invariants hold");
        }

        // And the exemption is real: one player holding both sides of a cross
        // is CORRECT, not a fault — they simply can't trade with themselves.
        let solo = a_seller(&db, &m, "wood", 10).await;
        sell(&db, &m, &solo, "wood", 4, 5).await;
        buy(&db, &m, &solo, "wood", 20, 5).await;
        assert!(
            db.book_health().await.unwrap().is_empty(),
            "a self-crossed book is what self-match prevention leaves behind, not corruption"
        );
    }

    #[tokio::test]
    async fn expiry_releases_escrow_exactly_like_a_cancel() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let seller = a_seller(&db, &m, "wood", 20).await;
        let buyer = a_character(&db).await;
        let buyer_start = db.character_gold(&buyer).await.unwrap();

        // One of each side, both expiring at t=100.
        db.place_order(&m, &seller, "sell", "wood", 9, 20, 100, &test_cfg(), "", 0).await.unwrap();
        let bid = db.place_order(&m, &buyer, "buy", "wood", 4, 10, 100, &test_cfg(), "", 0).await.unwrap();
        let bid_fee = bid.listing_fee;
        assert_eq!(db.character_gold(&buyer).await.unwrap(), buyer_start - 40 - bid_fee);

        // Not due yet: nothing moves.
        assert!(db.expire_orders(99).await.unwrap().is_empty());
        assert_eq!(db.character_gold(&buyer).await.unwrap(), buyer_start - 40 - bid_fee);

        // Due: both release, goods back to available and gold back to purse —
        // but the listing fee stays spent, exactly as on a cancel (#141).
        let expired = db.expire_orders(100).await.unwrap();
        assert_eq!(expired.len(), 2);
        assert_eq!(
            db.character_gold(&buyer).await.unwrap(), buyer_start - bid_fee,
            "escrowed gold returned; expiry refunds no more than a cancel does"
        );
        let held = db.warehouse_for_character(&m, &seller).await.unwrap();
        assert!(held.iter().all(|r| r.state == "available"), "goods un-escrowed");
        assert_eq!(held.iter().map(|r| r.qty).sum::<i64>(), 20);
        assert!(db.book_for(&m, "wood", "sell").await.unwrap().is_empty());

        // An order with no expiry (0 — placed before expiry existed, #139) is
        // never retro-expired.
        db.place_order(&m, &seller, "sell", "wood", 9, 5, 0, &test_cfg(), "", 0).await.unwrap();
        assert!(db.expire_orders(i64::MAX).await.unwrap().is_empty(), "0 means no expiry, not long past");
    }

    #[tokio::test]
    async fn the_open_order_cap_is_enforced() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cid = a_seller(&db, &m, "wood", 30).await;
        // A cap of 3, not the shipped 40 — the point is the boundary, and this
        // is also the one test that must NOT use the shared `test_cfg()`, whose
        // cap is deliberately raised out of the way of deep-book tests.
        let capped = MarketConfig { max_open_orders: 3, ..test_cfg() };
        for i in 0..3 {
            let out = db
                .place_order(&m, &cid, "sell", "wood", 5 + i, 2, NO_EXPIRY, &capped, "", 0)
                .await
                .unwrap();
            assert!(out.resting_order_id.is_some(), "order {i} should rest");
        }
        // At the cap: refused outright, and nothing is escrowed for it.
        let held_before: i64 = db.warehouse_for_character(&m, &cid).await.unwrap()
            .iter().filter(|r| r.state == "locked").map(|r| r.qty).sum();
        let out = db.place_order(&m, &cid, "sell", "wood", 9, 2, NO_EXPIRY, &capped, "", 0).await.unwrap();
        assert!(out.resting_order_id.is_none());
        assert_eq!(out.filled, 0);
        let held_after: i64 = db.warehouse_for_character(&m, &cid).await.unwrap()
            .iter().filter(|r| r.state == "locked").map(|r| r.qty).sum();
        assert_eq!(held_before, held_after, "a refused order escrows nothing");
    }

    /// #141: BOTH sides pay to list, so spoofing — papering the book with
    /// orders you never intend to honour — bleeds gold. That's most of the
    /// defence against it.
    #[tokio::test]
    async fn spoofing_the_book_bleeds_gold() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let spoofer = a_seller(&db, &m, "wood", 40).await;
        let start = db.character_gold(&spoofer).await.unwrap();

        // Post and pull, ten times over, on both sides. Nothing ever trades.
        for i in 0..10 {
            let s = sell(&db, &m, &spoofer, "wood", 20 + i, 4).await;
            let b = buy(&db, &m, &spoofer, "wood", 2, 4).await;
            for id in [s.resting_order_id, b.resting_order_id].into_iter().flatten() {
                db.cancel_order(&spoofer, &id).await.unwrap();
            }
        }
        let end = db.character_gold(&spoofer).await.unwrap();
        assert!(end < start, "spoofing must cost something (was {start}, now {end})");
        assert_eq!(
            db.recent_trades(&m, "wood", 10).await.unwrap().len(), 0,
            "and it never actually traded"
        );
        // Everything lost went to the sink, nowhere else.
        assert_eq!(start - end, db.total_fees_burned().await.unwrap());

        // Stock is untouched — only gold was spent.
        let held: i64 = db.warehouse_for_character(&m, &spoofer).await.unwrap()
            .iter().filter(|r| r.item_id == "wood").map(|r| r.qty).sum();
        assert_eq!(held, 40, "the goods came back every time");
    }

    #[tokio::test]
    async fn an_order_you_cannot_pay_the_fee_on_is_refused_outright() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cid = a_seller(&db, &m, "wood", 20).await;

        // Drain the purse to nothing, so even the 1-gold floor is out of reach.
        let purse = db.character_gold(&cid).await.unwrap();
        sqlx::query("UPDATE character SET gold = 0 WHERE id = ?")
            .bind(&cid)
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(purse > 0);

        let out = sell(&db, &m, &cid, "wood", 5, 20).await;
        assert!(out.fee_unaffordable, "should refuse for want of the fee, not silently");
        assert!(out.resting_order_id.is_none());
        // And critically: nothing was escrowed against an order that was never
        // placed — the goods are all still available.
        let available: i64 = db.warehouse_for_character(&m, &cid).await.unwrap()
            .iter().filter(|r| r.state == "available").map(|r| r.qty).sum();
        assert_eq!(available, 20, "no goods stranded in escrow");
        assert!(db.book_for(&m, "wood", "sell").await.unwrap().is_empty());
        assert_eq!(db.total_fees_burned().await.unwrap(), 0, "and nothing was charged");
    }

    #[tokio::test]
    async fn the_fee_ledger_accounts_for_every_burn_by_kind() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let seller = a_seller(&db, &m, "wood", 20).await;
        let buyer = a_character(&db).await;
        sell(&db, &m, &seller, "wood", 8, 10).await;
        buy(&db, &m, &buyer, "wood", 8, 10).await;

        let by_kind = db.fees_by_kind(&m).await.unwrap();
        let listing: i64 = by_kind.iter().filter(|(k, _)| k == "listing").map(|(_, g)| g).sum();
        let tax: i64 = by_kind.iter().filter(|(k, _)| k == "sale_tax").map(|(_, g)| g).sum();
        assert_eq!(listing, test_cfg().listing_fee(80) * 2, "both sides paid to list");
        assert_eq!(tax, test_cfg().sale_tax(80), "the seller paid tax on the fill");
        assert_eq!(listing + tax, db.total_fees_burned().await.unwrap());
    }

    // --- Listing board for unique items (#142) ------------------------------

    /// A seller with a worn pickaxe banked at the market, ready to list. Returns
    /// `(seller_id, warehouse_item_id, durability)`.
    async fn a_seller_with_a_worn_tool(db: &Db, market: &str) -> (String, String, i64) {
        let cid = a_character(db).await;
        db.add_to_inventory(&cid, "pickaxe", 1).await.unwrap();
        let inv = db.inventory_for_character(&cid).await.unwrap();
        let pick = inv.iter().find(|i| i.item_id == "pickaxe").unwrap().clone();
        // Wear it, so "the same instance" is actually falsifiable.
        db.equip_instance(&cid, &pick.id).await.unwrap();
        db.wear_equipped_tool(&cid, "tool", 13).await.unwrap();
        db.warehouse_deposit(market, &cid, "pickaxe", 1, 60).await.unwrap();
        let held = db.warehouse_for_character(market, &cid).await.unwrap();
        let row = held.iter().find(|r| r.item_id == "pickaxe").unwrap();
        (cid, row.id.clone(), row.durability.unwrap())
    }

    #[tokio::test]
    async fn a_listed_tool_keeps_its_exact_instance_and_wear_through_the_sale() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let (seller, wh_id, wear) = a_seller_with_a_worn_tool(&db, &m).await;
        // Derived from the registry: a balance pass (#129) retunes durability
        // without invalidating what this test is actually about (instance
        // identity surviving a sale).
        let fresh = crate::world::tool_max_durability("pickaxe").unwrap();
        assert_eq!(wear, fresh - 13, "worn by 13 from full");

        let listing = db
            .place_listing(&m, &seller, &wh_id, 60, NO_EXPIRY, &test_cfg(), "", 0)
            .await
            .unwrap()
            .expect("the listing should be accepted");
        assert_eq!(listing.durability, Some(wear), "the board advertises its actual wear");
        // Escrowed, not merely flagged: it can't be withdrawn while listed.
        assert_eq!(db.warehouse_withdraw(&m, &seller, "pickaxe", 1).await.unwrap(), 0);

        let buyer = a_character(&db).await;
        let (sold, tax) = db
            .buy_listing(&buyer, &listing.id, 60, &test_cfg(), "", 0)
            .await
            .unwrap()
            .expect("the purchase should go through");
        assert_eq!(sold.id, listing.id);

        // The SAME row, now the buyer's, still worn — not a fresh tool.
        let buyer_held = db.warehouse_for_character(&m, &buyer).await.unwrap();
        assert_eq!(buyer_held.len(), 1);
        assert_eq!(buyer_held[0].id, wh_id, "the very instance that was advertised");
        assert_eq!(buyer_held[0].durability, Some(wear));
        assert_eq!(buyer_held[0].state, "available", "and collectable");
        assert!(db.warehouse_for_character(&m, &seller).await.unwrap().is_empty());

        // And it withdraws to carried inventory as the same instance again.
        assert_eq!(db.warehouse_withdraw(&m, &buyer, "pickaxe", 1).await.unwrap(), 1);
        let back = db.inventory_for_character(&buyer).await.unwrap()
            .into_iter().find(|i| i.item_id == "pickaxe").unwrap();
        assert_eq!((back.id, back.durability), (wh_id, Some(wear)));
        assert!(tax > 0, "the sale was taxed (#141)");
    }

    /// The listing race (#136 §12): N buyers, one item. Exactly one wins,
    /// exactly one charge happens, and every loser is told it's gone rather
    /// than charged for nothing.
    #[tokio::test]
    async fn concurrent_buyers_race_for_one_listing_and_exactly_one_wins() {
        let (db, _t) = TempDb::open().await;
        let db = std::sync::Arc::new(db);
        let m = a_market(&db).await;
        let (seller, wh_id, _) = a_seller_with_a_worn_tool(&db, &m).await;
        let listing = db.place_listing(&m, &seller, &wh_id, 50, NO_EXPIRY, &test_cfg(), "", 0)
            .await.unwrap().unwrap();

        const BUYERS: usize = 8;
        let mut buyers = Vec::new();
        for _ in 0..BUYERS {
            buyers.push(a_character(&db).await);
        }
        let before: Vec<i64> = {
            let mut v = Vec::new();
            for b in &buyers {
                v.push(db.character_gold(b).await.unwrap());
            }
            v
        };

        // All of them pounce at once.
        let mut tasks = Vec::new();
        for b in buyers.clone() {
            let db = db.clone();
            let id = listing.id.clone();
            tasks.push(tokio::spawn(async move {
                db.buy_listing(&b, &id, 50, &test_cfg(), "", 0).await.unwrap()
            }));
        }
        let mut wins = 0;
        let mut gone = 0;
        for t in tasks {
            match t.await.unwrap() {
                Ok(_) => wins += 1,
                Err(ListingReject::Gone) => gone += 1,
                Err(other) => panic!("unexpected rejection: {other:?}"),
            }
        }
        assert_eq!(wins, 1, "exactly one buyer may win");
        assert_eq!(gone, BUYERS - 1, "everyone else is told it's gone");

        // Exactly one charge, and the losers paid nothing at all.
        let mut charged = 0;
        for (b, was) in buyers.iter().zip(&before) {
            let now = db.character_gold(b).await.unwrap();
            if now != *was {
                charged += 1;
                assert_eq!(*was - now, 50, "the winner paid exactly the ask");
            }
        }
        assert_eq!(charged, 1, "no loser was charged a penny");

        // The item exists exactly once, with exactly one owner.
        let owners: Vec<String> = sqlx::query_scalar(
            "SELECT character_id FROM market_warehouse_item WHERE id = ?",
        )
        .bind(&wh_id)
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(owners.len(), 1, "the instance was never duplicated");
        assert!(buyers.contains(&owners[0]));
        assert_eq!(db.recent_trades(&m, "pickaxe", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn buying_a_listing_refuses_a_changed_price_and_your_own_listing() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let (seller, wh_id, _) = a_seller_with_a_worn_tool(&db, &m).await;
        let listing = db.place_listing(&m, &seller, &wh_id, 60, NO_EXPIRY, &test_cfg(), "", 0)
            .await.unwrap().unwrap();

        // A stale price is refused outright — never charge a surprise price.
        let buyer = a_character(&db).await;
        let purse = db.character_gold(&buyer).await.unwrap();
        assert_eq!(
            db.buy_listing(&buyer, &listing.id, 45, &test_cfg(), "", 0).await.unwrap(),
            Err(ListingReject::PriceChanged)
        );
        assert_eq!(db.character_gold(&buyer).await.unwrap(), purse, "nothing charged");
        assert!(db.listing_by_id(&listing.id).await.unwrap().is_some(), "listing untouched");

        // You can't buy your own.
        assert_eq!(
            db.buy_listing(&seller, &listing.id, 60, &test_cfg(), "", 0).await.unwrap(),
            Err(ListingReject::OwnListing)
        );

        // Nor one you can't afford.
        let broke = a_character(&db).await;
        sqlx::query("UPDATE character SET gold = 0 WHERE id = ?")
            .bind(&broke).execute(&db.pool).await.unwrap();
        assert_eq!(
            db.buy_listing(&broke, &listing.id, 60, &test_cfg(), "", 0).await.unwrap(),
            Err(ListingReject::NoFunds)
        );
        assert!(db.listing_by_id(&listing.id).await.unwrap().is_some(), "still for sale");
    }

    #[tokio::test]
    async fn cancelling_or_expiring_a_listing_returns_the_item_intact() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;

        // Cancel.
        let (seller, wh_id, wear) = a_seller_with_a_worn_tool(&db, &m).await;
        let listing = db.place_listing(&m, &seller, &wh_id, 60, NO_EXPIRY, &test_cfg(), "", 0)
            .await.unwrap().unwrap();
        let other = a_character(&db).await;
        assert!(db.cancel_listing(&other, &listing.id).await.unwrap().is_none(), "not yours");
        assert!(db.cancel_listing(&seller, &listing.id).await.unwrap().is_some());
        let held = db.warehouse_for_character(&m, &seller).await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!((held[0].id.as_str(), held[0].state.as_str(), held[0].durability), (wh_id.as_str(), "available", Some(wear)));

        // Expiry does exactly the same — forgetting a listing can't cost you
        // the item.
        let listing = db.place_listing(&m, &seller, &wh_id, 60, 100, &test_cfg(), "", 0)
            .await.unwrap().unwrap();
        assert!(db.expire_listings(99).await.unwrap().is_empty(), "not due yet");
        let expired = db.expire_listings(100).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, listing.id);
        let held = db.warehouse_for_character(&m, &seller).await.unwrap();
        assert_eq!((held[0].state.as_str(), held[0].durability), ("available", Some(wear)));
        assert!(db.listing_by_id(&listing.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_board_rejects_commodities_and_filters_and_sorts() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;

        // A stackable good can never be listed — it belongs on the book.
        let stacker = a_seller(&db, &m, "wood", 10).await;
        let wood_row = db.warehouse_for_character(&m, &stacker).await.unwrap()[0].id.clone();
        assert!(
            db.place_listing(&m, &stacker, &wood_row, 5, NO_EXPIRY, &test_cfg(), "", 0).await.unwrap().is_none(),
            "commodities go to the order book, not the board"
        );

        // Three tools at different prices and wear.
        let mut made = Vec::new();
        for (ask, extra_wear) in [(90, 0), (40, 20), (65, 5)] {
            let cid = a_character(&db).await;
            db.add_to_inventory(&cid, "axe", 1).await.unwrap();
            let inv = db.inventory_for_character(&cid).await.unwrap();
            let axe = inv.iter().find(|i| i.item_id == "axe").unwrap().clone();
            if extra_wear > 0 {
                db.equip_instance(&cid, &axe.id).await.unwrap();
                db.wear_equipped_tool(&cid, "tool", extra_wear).await.unwrap();
            }
            db.warehouse_deposit(&m, &cid, "axe", 1, 60).await.unwrap();
            let row = db.warehouse_for_character(&m, &cid).await.unwrap()
                .into_iter().find(|r| r.item_id == "axe").unwrap();
            made.push(db.place_listing(&m, &cid, &row.id, ask, NO_EXPIRY, &test_cfg(), "", 0).await.unwrap().unwrap());
        }
        assert_eq!(made.len(), 3);

        // Cheapest first.
        let all = db.listings_for_market(&m, None, None, None, 50).await.unwrap();
        let prices: Vec<i64> = all.iter().map(|l| l.ask_price).collect();
        assert_eq!(prices, vec![40, 65, 90], "board sorts by ask, cheapest first");

        // Filters compose.
        let cheap = db.listings_for_market(&m, Some("axe"), None, Some(70), 50).await.unwrap();
        assert_eq!(cheap.len(), 2);
        let healthy = db.listings_for_market(&m, None, Some(46), None, 50).await.unwrap();
        assert!(
            healthy.iter().all(|l| l.durability.unwrap() >= 46),
            "min durability skips the worn ones: {healthy:?}"
        );
        assert!(db.listings_for_market(&m, Some("pickaxe"), None, None, 50).await.unwrap().is_empty());
    }

    // --- Price history (#143) -----------------------------------------------

    /// Write a trade straight onto the ledger. The rollup only reads
    /// `market_trade`, so a fixture doesn't need the whole matching engine to
    /// exercise it — and driving it directly is what lets a test pin exact
    /// timestamps and orderings.
    async fn a_trade(db: &Db, market: &str, item: &str, price: i64, qty: i64, at: i64) {
        sqlx::query(
            "INSERT INTO market_trade (id, market_id, item_id, unit_price, qty, seller_id, \
             buyer_id, sale_tax_gold, listing_fee_gold, created_at) \
             VALUES (?, ?, ?, ?, ?, 's', 'b', 0, 0, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(market)
        .bind(item)
        .bind(price)
        .bind(qty)
        .bind(at)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn candles_roll_up_ohlcv_and_leave_quiet_hours_as_gaps() {
        let (db, _t) = TempDb::open().await;
        let m = "mkt";
        const H: i64 = 3600;

        // Hour 0: four trades. Open 10, high 14, low 8, close 12, volume 10.
        a_trade(&db, m, "wood", 10, 1, 0).await;
        a_trade(&db, m, "wood", 14, 2, 100).await;
        a_trade(&db, m, "wood", 8, 3, 200).await;
        a_trade(&db, m, "wood", 12, 4, 3599).await;
        // Hour 1: nothing at all.
        // Hour 2: one trade.
        a_trade(&db, m, "wood", 20, 5, 2 * H + 10).await;

        db.roll_up_candles(H, 0, 3 * H).await.unwrap();
        let c = db.candles(m, "wood", H, 0, 3 * H).await.unwrap();
        assert_eq!(c.len(), 2, "the quiet hour is a GAP, not a fabricated flat candle");
        assert_eq!(
            c[0],
            Candle { bucket_start: 0, open: 10, high: 14, low: 8, close: 12, volume: 10, trades: 4 }
        );
        assert_eq!(
            c[1],
            Candle { bucket_start: 2 * H, open: 20, high: 20, low: 20, close: 20, volume: 5, trades: 1 }
        );

        // Open/close follow LEDGER order, not price order — a later cheaper
        // trade still closes the candle.
        assert!(c[0].close < c[0].open || c[0].close != c[0].high);
    }

    #[tokio::test]
    async fn a_trade_on_an_interval_boundary_lands_in_exactly_one_candle() {
        let (db, _t) = TempDb::open().await;
        let m = "mkt";
        const H: i64 = 3600;

        // One second before the boundary, exactly on it, and one after.
        a_trade(&db, m, "wood", 5, 1, H - 1).await;
        a_trade(&db, m, "wood", 6, 1, H).await;
        a_trade(&db, m, "wood", 7, 1, H + 1).await;
        db.roll_up_candles(H, 0, 3 * H).await.unwrap();
        let c = db.candles(m, "wood", H, 0, 3 * H).await.unwrap();

        assert_eq!(c.len(), 2);
        assert_eq!((c[0].bucket_start, c[0].trades), (0, 1), "the earlier hour keeps only its own");
        assert_eq!(
            (c[1].bucket_start, c[1].trades, c[1].open, c[1].close), (H, 2, 6, 7),
            "a trade exactly on the boundary OPENS the new hour"
        );
        let total: i64 = c.iter().map(|x| x.trades).sum();
        assert_eq!(total, 3, "every trade counted exactly once");
    }

    #[tokio::test]
    async fn candles_are_per_market_and_per_commodity() {
        let (db, _t) = TempDb::open().await;
        const H: i64 = 3600;
        a_trade(&db, "m1", "wood", 10, 1, 10).await;
        a_trade(&db, "m1", "stone", 99, 1, 20).await;
        a_trade(&db, "m2", "wood", 50, 1, 30).await;
        db.roll_up_candles(H, 0, H).await.unwrap();

        assert_eq!(db.candles("m1", "wood", H, 0, H).await.unwrap()[0].close, 10);
        assert_eq!(db.candles("m1", "stone", H, 0, H).await.unwrap()[0].close, 99);
        assert_eq!(db.candles("m2", "wood", H, 0, H).await.unwrap()[0].close, 50,
            "the same commodity at another market has its own price");
    }

    /// The candles are a DERIVED CACHE (#143). A from-scratch rebuild must
    /// reproduce exactly what the incremental job produced — otherwise the
    /// cache is secretly authoritative and losing it loses data.
    #[tokio::test]
    async fn rebuilding_candles_from_scratch_reproduces_them_identically() {
        let (db, _t) = TempDb::open().await;
        let m = "mkt";
        const H: i64 = 3600;
        let mut seed: u64 = 0xc0ffee;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as i64
        };
        for i in 0..60 {
            let at = i * 400 + next() % 300;
            let item = if next() % 2 == 0 { "wood" } else { "stone" };
            a_trade(&db, m, item, 1 + next() % 40, 1 + next() % 6, at).await;
        }

        // Incremental: roll up hour by hour, as the background job does.
        for h in 0..8 {
            db.roll_up_candles(H, h * H, (h + 1) * H).await.unwrap();
        }
        let incremental = (
            db.candles(m, "wood", H, 0, 10 * H).await.unwrap(),
            db.candles(m, "stone", H, 0, 10 * H).await.unwrap(),
        );
        assert!(!incremental.0.is_empty() && !incremental.1.is_empty(), "the fixture should produce candles");

        // From scratch: wipe and recompute everything at once.
        db.rebuild_all_candles(H).await.unwrap();
        let rebuilt = (
            db.candles(m, "wood", H, 0, 10 * H).await.unwrap(),
            db.candles(m, "stone", H, 0, 10 * H).await.unwrap(),
        );
        assert_eq!(incremental, rebuilt, "a rebuild must reproduce the cache exactly");

        // And re-running the same range is idempotent, not additive.
        db.roll_up_candles(H, 0, 10 * H).await.unwrap();
        assert_eq!(
            db.candles(m, "wood", H, 0, 10 * H).await.unwrap(), rebuilt.0,
            "rolling up twice must not double-count"
        );
    }

    #[tokio::test]
    async fn pruning_history_never_touches_the_trade_ledger() {
        let (db, _t) = TempDb::open().await;
        let m = "mkt";
        const H: i64 = 3600;
        const DAY: i64 = 86_400;
        a_trade(&db, m, "wood", 10, 1, 0).await; // old
        a_trade(&db, m, "wood", 20, 1, 40 * DAY).await; // recent
        db.roll_up_candles(H, 0, 41 * DAY).await.unwrap();
        assert_eq!(db.candles(m, "wood", H, 0, 41 * DAY).await.unwrap().len(), 2);

        // Prune everything before day 30.
        let pruned = db.prune_candles(30 * DAY).await.unwrap();
        assert_eq!(pruned, 1, "only the out-of-window candle went");
        let left = db.candles(m, "wood", H, 0, 41 * DAY).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].close, 20);

        // The LEDGER is untouched — which is why pruned history is recoverable.
        let ledger: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM market_trade")
            .fetch_one(&db.pool).await.unwrap();
        assert_eq!(ledger, 2, "the append-only ledger must never be pruned");
        db.rebuild_all_candles(H).await.unwrap();
        assert_eq!(
            db.candles(m, "wood", H, 0, 41 * DAY).await.unwrap().len(), 2,
            "and so pruned candles can be rebuilt from it"
        );
    }

    /// Real trades (not hand-written ledger rows) roll up correctly — proves
    /// the engine records what the history needs.
    #[tokio::test]
    async fn real_trades_produce_history() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let seller = a_seller(&db, &m, "wood", 30).await;
        let buyer = a_character(&db).await;
        sell(&db, &m, &seller, "wood", 8, 10).await;
        buy(&db, &m, &buyer, "wood", 8, 6).await;

        let now = now_secs();
        db.roll_up_candles(test_cfg().candle_interval_secs, 0, now + 1).await.unwrap();
        let c = db
            .candles(&m, "wood", test_cfg().candle_interval_secs, 0, now + 1)
            .await
            .unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].open, c[0].close, c[0].volume), (8, 8, 6));
    }

    /// Matching determinism (#136 §12): the same command sequence must produce
    /// a byte-identical trade log, or the engine has hidden nondeterminism and
    /// no bug in it is reproducible.
    #[tokio::test]
    async fn matching_is_deterministic() {
        async fn run() -> String {
            let (db, _t) = TempDb::open().await;
            let m = a_market(&db).await;
            let s = a_seller(&db, &m, "wood", 40).await;
            let b = a_character(&db).await;
            for (side, price, qty) in [
                ("sell", 9, 5), ("sell", 7, 6), ("sell", 8, 4),
                ("buy", 8, 7), ("buy", 12, 6), ("sell", 6, 3), ("buy", 5, 4),
            ] {
                let who = if side == "sell" { &s } else { &b };
                db.place_order(&m, who, side, "wood", price, qty, NO_EXPIRY, &test_cfg(), "", 0)
                    .await
                    .unwrap();
            }
            // The ledger, minus the ids/timestamps that are meant to differ.
            db.recent_trades(&m, "wood", 100)
                .await
                .unwrap()
                .iter()
                .map(|t| format!("{}@{}x{}", t.item_id, t.unit_price, t.qty))
                .collect::<Vec<_>>()
                .join(",")
        }
        let first = run().await;
        assert!(!first.is_empty(), "the fixture should actually trade");
        assert_eq!(first, run().await, "same commands, same trade log");
    }

    #[tokio::test]
    async fn a_sell_order_escrows_only_what_the_seller_actually_holds() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cid = a_seller(&db, &m, "wood", 20).await;

        // Offering 50 while holding 20 rests an order for 20 — never a promise
        // the seller can't keep.
        let order = sell(&db, &m, &cid, "wood", 5, 50).await;
        assert_eq!(order.resting_qty, 20);
        assert!(order.resting_order_id.is_some());
        let held = db.warehouse_for_character(&m, &cid).await.unwrap();
        assert!(held.iter().all(|r| r.state == "locked"), "all of it is escrowed");

        // With nothing left available, a second order rests nothing at all.
        let nothing = sell(&db, &m, &cid, "wood", 5, 5).await;
        assert_eq!((nothing.resting_qty, nothing.filled), (0, 0));
        assert!(nothing.resting_order_id.is_none());
    }

    #[tokio::test]
    async fn cancelling_a_sell_order_returns_the_unsold_escrow() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let seller = a_seller(&db, &m, "wood", 20).await;
        let order_id = sell(&db, &m, &seller, "wood", 5, 20).await.resting_order_id.unwrap();

        // Sell half of it first, so the cancel has to return the REMAINDER.
        let buyer = a_character(&db).await;
        buy(&db, &m, &buyer, "wood", 5, 8).await;

        // Someone else can't cancel it.
        assert!(db.cancel_order(&buyer, &order_id).await.unwrap().is_none());

        assert!(db.cancel_order(&seller, &order_id).await.unwrap().is_some());
        let held = db.warehouse_for_character(&m, &seller).await.unwrap();
        let available: i64 = held.iter().filter(|r| r.state == "available").map(|r| r.qty).sum();
        let locked: i64 = held.iter().filter(|r| r.state == "locked").map(|r| r.qty).sum();
        assert_eq!((available, locked), (12, 0), "the 12 unsold come back, the 8 sold are gone");
        assert!(db.book_for(&m, "wood", "sell").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_repeated_command_id_never_places_or_buys_twice() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let seller = a_seller(&db, &m, "wood", 30).await;

        let place = |who: String, side: &'static str, price: i64, qty: i64, cmd: &'static str| {
            let db = &db;
            let m = m.clone();
            async move {
                db.place_order(&m, &who, side, "wood", price, qty, NO_EXPIRY, &test_cfg(), cmd, 0)
                    .await
                    .unwrap()
            }
        };

        assert!(place(seller.clone(), "sell", 5, 10, "cmd-1").await.resting_order_id.is_some());
        let resent = place(seller.clone(), "sell", 5, 10, "cmd-1").await;
        assert!(resent.resting_order_id.is_none(), "a resent placement is a no-op, not a second order");
        assert_eq!(resent.filled, 0);
        assert_eq!(
            db.book_for(&m, "wood", "sell").await.unwrap(),
            vec![BookLevel { unit_price: 5, qty: 10 }],
            "still exactly one order's worth on the book"
        );

        let buyer = a_character(&db).await;
        let purse = db.character_gold(&buyer).await.unwrap();
        assert_eq!(place(buyer.clone(), "buy", 5, 4, "cmd-2").await.filled, 4);
        let charged = db.character_gold(&buyer).await.unwrap();
        let second = place(buyer.clone(), "buy", 5, 4, "cmd-2").await;
        assert_eq!((second.filled, second.spent), (0, 0), "a resent buy doesn't double-charge");
        assert_eq!(db.character_gold(&buyer).await.unwrap(), charged, "and escrows nothing either");
        assert!(charged < purse);
        assert_eq!(db.recent_trades(&m, "wood", 10).await.unwrap().len(), 1);
    }

    /// The epic's headline invariant (#136 §12) over the whole trading loop:
    /// across sells, buys and cancels, goods and gold are both conserved. This
    // --- warehouse storage fees (#155) --------------------------------------

    const DAY: i64 = 86_400;

    /// A config with storage billing switched on. The shipped one has it OFF —
    /// see `storage_is_off_by_default`.
    fn charged_cfg(per_slot: i64) -> MarketConfig {
        MarketConfig {
            storage_fee_per_slot_per_day: per_slot,
            storage_arrears_cap_days: 3,
            ..test_cfg()
        }
    }

    /// Mark a character as having been active at `when`, which is what makes
    /// them billable — see `offline_days_are_free`.
    async fn seen_at(db: &Db, cid: &str, when: i64) {
        sqlx::query("UPDATE character SET last_seen = ? WHERE id = ?")
            .bind(when)
            .bind(cid)
            .execute(&db.pool)
            .await
            .unwrap();
    }

    /// The shipped configuration. Nobody is hoarding yet, and taxing players for
    /// using a feature they were just given is a bad first impression — so the
    /// mechanism ships and the policy does not. Nothing is charged, no rows are
    /// written, and nobody is locked out.
    #[tokio::test]
    async fn storage_is_off_by_default() {
        assert_eq!(MarketConfig::default().storage_fee_per_slot_per_day, 0);

        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let who = a_seller(&db, &m, "wood", 30).await;
        seen_at(&db, &who, DAY * 10).await;
        let before = db.character_gold(&who).await.unwrap();

        for day in 1..5 {
            assert_eq!(db.charge_storage(&m, &test_cfg(), DAY * day).await.unwrap(), (0, 0));
        }
        assert_eq!(db.character_gold(&who).await.unwrap(), before, "free storage charged rent");
        assert_eq!(db.warehouse_arrears(&m, &who).await.unwrap(), 0);
        assert_eq!(db.total_fees_burned().await.unwrap(), 0, "a disabled fee wrote a ledger row");
    }

    /// Charged per OCCUPIED SLOT, counting locked stock as well as available.
    /// Per slot because the slot is the scarce resource — charging per item
    /// would punish stacking, the opposite of the intent. Locked stock counts
    /// because otherwise "list it at an absurd price" would be free storage.
    #[tokio::test]
    async fn storage_is_charged_per_occupied_slot_including_locked() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cfg = charged_cfg(2);
        let who = a_character(&db).await;
        // Three slots: two commodity rows and a tool instance.
        db.add_to_inventory(&who, "wood", 40).await.unwrap();
        db.warehouse_deposit(&m, &who, "wood", 40, 60).await.unwrap();
        db.add_to_inventory(&who, "stone", 10).await.unwrap();
        db.warehouse_deposit(&m, &who, "stone", 10, 60).await.unwrap();
        // Escrow one row against a resting sell, so it is `locked`.
        sell(&db, &m, &who, "wood", 9, 40).await;
        let rows = db.warehouse_for_character(&m, &who).await.unwrap();
        assert!(rows.iter().any(|r| r.state == "locked"), "precondition: something is escrowed");
        let slots = rows.len() as i64;

        seen_at(&db, &who, 1).await;
        // First run only starts the clock — it must not bill for however long
        // the goods happened to have been sitting there already.
        assert_eq!(db.charge_storage(&m, &cfg, DAY).await.unwrap(), (0, 0));
        let before = db.character_gold(&who).await.unwrap();

        seen_at(&db, &who, DAY + 1).await;
        // Measured as a DELTA: resting the sell above burned a listing fee too,
        // so the running total isn't the storage charge.
        let burned_before = db.total_fees_burned().await.unwrap();
        let (charged, arrears) = db.charge_storage(&m, &cfg, DAY * 2).await.unwrap();
        assert_eq!(charged, slots * 2, "should be per slot, locked rows included");
        assert_eq!(arrears, 0);
        assert_eq!(db.character_gold(&who).await.unwrap(), before - slots * 2);
        assert_eq!(
            db.total_fees_burned().await.unwrap() - burned_before,
            slots * 2,
            "storage should be burned"
        );
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0, "the burn broke the supply identity");
    }

    /// Idempotent within a day. The job wakes far more often than daily, and a
    /// restart loop must not bill anyone twice.
    #[tokio::test]
    async fn charging_twice_in_one_day_charges_once() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cfg = charged_cfg(5);
        let who = a_seller(&db, &m, "wood", 20).await;
        seen_at(&db, &who, 1).await;
        db.charge_storage(&m, &cfg, DAY).await.unwrap(); // starts the clock

        seen_at(&db, &who, DAY + 1).await;
        let (first, _) = db.charge_storage(&m, &cfg, DAY * 2).await.unwrap();
        assert!(first > 0);
        for _ in 0..5 {
            assert_eq!(
                db.charge_storage(&m, &cfg, DAY * 2 + 60).await.unwrap(),
                (0, 0),
                "billed twice in the same day"
            );
        }
    }

    /// A player who wasn't there isn't billed. Charging for days someone was
    /// offline turns a holding cost into a punishment for having a job.
    #[tokio::test]
    async fn offline_days_are_free() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cfg = charged_cfg(5);
        let who = a_seller(&db, &m, "wood", 20).await;
        seen_at(&db, &who, 1).await;
        db.charge_storage(&m, &cfg, DAY).await.unwrap(); // clock starts
        let before = db.character_gold(&who).await.unwrap();

        // A month passes with no logins at all.
        for day in 2..32 {
            assert_eq!(db.charge_storage(&m, &cfg, DAY * day).await.unwrap(), (0, 0));
        }
        assert_eq!(db.character_gold(&who).await.unwrap(), before, "billed while away");
        assert_eq!(db.warehouse_arrears(&m, &who).await.unwrap(), 0, "debt accrued while away");

        // They come back: ONE day's charge, not a month of them.
        seen_at(&db, &who, DAY * 32).await;
        let (charged, _) = db.charge_storage(&m, &cfg, DAY * 33).await.unwrap();
        assert_eq!(charged, 5, "a returning player owes a day, not the whole absence");
    }

    /// **Goods are never confiscated.** An empty purse produces capped arrears
    /// and a locked warehouse, and the stock is untouched — deleting someone's
    /// stored items to settle a debt is an unrecoverable loss caused by not
    /// logging in, and the fastest way to make players distrust the warehouse.
    #[tokio::test]
    async fn an_empty_purse_never_costs_you_your_goods() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cfg = charged_cfg(10);
        let who = a_seller(&db, &m, "wood", 30).await;
        // Spend every coin.
        sqlx::query("UPDATE character SET gold = 0 WHERE id = ?")
            .bind(&who)
            .execute(&db.pool)
            .await
            .unwrap();
        let held: i64 =
            db.warehouse_for_character(&m, &who).await.unwrap().iter().map(|r| r.qty).sum();
        assert!(held > 0);

        seen_at(&db, &who, 1).await;
        db.charge_storage(&m, &cfg, DAY).await.unwrap();
        for day in 2..10 {
            seen_at(&db, &who, DAY * day - 1).await;
            db.charge_storage(&m, &cfg, DAY * day).await.unwrap();
        }

        let still_held: i64 =
            db.warehouse_for_character(&m, &who).await.unwrap().iter().map(|r| r.qty).sum();
        assert_eq!(still_held, held, "goods were confiscated to settle a debt");
        assert!(db.character_gold(&who).await.unwrap() >= 0, "purse went negative");

        // The debt is capped, so it stays payable however long they were away.
        let owed = db.warehouse_arrears(&m, &who).await.unwrap();
        assert!(owed > 0, "nothing accrued at all");
        assert_eq!(
            owed,
            cfg.storage_arrears_cap_days * cfg.storage_fee_per_slot_per_day,
            "arrears must stop at the cap — an unpayable bill is the same as losing the goods"
        );
    }

    /// Arrears lock the warehouse and paying clears the lock, with the payment
    /// burned like any other fee.
    #[tokio::test]
    async fn paying_arrears_unlocks_the_warehouse() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cfg = charged_cfg(10);
        let who = a_seller(&db, &m, "wood", 30).await;
        sqlx::query("UPDATE character SET gold = 0 WHERE id = ?")
            .bind(&who)
            .execute(&db.pool)
            .await
            .unwrap();
        seen_at(&db, &who, 1).await;
        db.charge_storage(&m, &cfg, DAY).await.unwrap();
        seen_at(&db, &who, DAY + 1).await;
        db.charge_storage(&m, &cfg, DAY * 2).await.unwrap();
        let owed = db.warehouse_arrears(&m, &who).await.unwrap();
        assert!(owed > 0, "precondition: they owe something");

        // Broke: settling changes nothing and they stay locked.
        assert_eq!(db.settle_warehouse_arrears(&m, &who, DAY * 2).await.unwrap(), owed);

        // Partially able to pay: the debt shrinks, the lock stays.
        sqlx::query("UPDATE character SET gold = ? WHERE id = ?")
            .bind(owed - 1)
            .bind(&who)
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(db.settle_warehouse_arrears(&m, &who, DAY * 2).await.unwrap(), 1);
        assert_eq!(db.character_gold(&who).await.unwrap(), 0);

        // Paid in full: unlocked, and the payment was burned.
        let burned_before = db.total_fees_burned().await.unwrap();
        sqlx::query("UPDATE character SET gold = 50 WHERE id = ?")
            .bind(&who)
            .execute(&db.pool)
            .await
            .unwrap();
        // This test pokes gold directly to set up a broke player, which is
        // itself an unledgered change — so the supply identity is checked as a
        // DELTA across the settle rather than absolutely. That still catches a
        // settle that creates or destroys gold without recording it.
        let gap_before = db.gold_supply_gap().await.unwrap();
        assert_eq!(db.settle_warehouse_arrears(&m, &who, DAY * 2).await.unwrap(), 0);
        assert_eq!(db.warehouse_arrears(&m, &who).await.unwrap(), 0);
        assert_eq!(db.character_gold(&who).await.unwrap(), 49);
        assert_eq!(db.total_fees_burned().await.unwrap(), burned_before + 1);
        assert_eq!(
            db.gold_supply_gap().await.unwrap(),
            gap_before,
            "settling arrears moved gold without telling the ledger"
        );
    }

    /// Per-market rates charge independently — a remote market can be cheaper to
    /// store at, which with #153's second market is a real trade-off: a long
    /// haul in exchange for cheap storage.
    #[tokio::test]
    async fn storage_rates_are_per_market() {
        let (db, _t) = TempDb::open().await;
        let a = a_market(&db).await;
        let b = another_market(&db).await;
        let dear = charged_cfg(10);
        let cheap = charged_cfg(1);

        let who = a_character(&db).await;
        db.add_to_inventory(&who, "wood", 20).await.unwrap();
        db.warehouse_deposit(&a, &who, "wood", 20, 60).await.unwrap();
        db.add_to_inventory(&who, "wood", 20).await.unwrap();
        db.warehouse_deposit(&b, &who, "wood", 20, 60).await.unwrap();

        seen_at(&db, &who, 1).await;
        db.charge_storage(&a, &dear, DAY).await.unwrap();
        db.charge_storage(&b, &cheap, DAY).await.unwrap();
        seen_at(&db, &who, DAY + 1).await;

        let (dear_charged, _) = db.charge_storage(&a, &dear, DAY * 2).await.unwrap();
        let (cheap_charged, _) = db.charge_storage(&b, &cheap, DAY * 2).await.unwrap();
        assert_eq!(dear_charged, 10, "the capital's rate");
        assert_eq!(cheap_charged, 1, "the remote market's rate");
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0);
    }

    /// Storage lands on the existing fee ledger with its own kind, so the
    /// holding cost is measurable next to the listing fee and the sale tax
    /// rather than in a parallel universe.
    #[tokio::test]
    async fn storage_fees_join_the_existing_fee_ledger() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cfg = charged_cfg(4);
        let who = a_seller(&db, &m, "wood", 30).await;
        sell(&db, &m, &who, "wood", 9, 10).await; // a listing fee, for contrast
        seen_at(&db, &who, 1).await;
        db.charge_storage(&m, &cfg, DAY).await.unwrap();
        seen_at(&db, &who, DAY + 1).await;
        db.charge_storage(&m, &cfg, DAY * 2).await.unwrap();

        let kinds = db.fees_by_kind(&m).await.unwrap();
        let storage: i64 =
            kinds.iter().filter(|(k, _)| k == "storage").map(|(_, g)| *g).sum();
        assert!(storage > 0, "storage should appear as its own fee kind: {kinds:?}");
        assert!(kinds.iter().any(|(k, _)| k == "listing"), "and alongside the others: {kinds:?}");
    }

    // --- NPC provisioner (#154) ---------------------------------------------

    /// A config with the provisioner switched on for wood. Floor well under
    /// ceiling: it should be the worst counterparty available and still better
    /// than nobody.
    fn provisioned_cfg() -> MarketConfig {
        let mut c = test_cfg();
        c.provisioner.insert(
            "wood".to_string(),
            crate::market_config::ProvisionerBounds {
                floor: 2,
                ceiling: 20,
                bid_qty: 100,
                seed_stock: 50,
            },
        );
        c
    }

    /// The bootstrapping problem, which is live right now: on a fresh server
    /// nobody has listed anything, so the first player to reach the market sees
    /// an empty book and the feature looks broken at the moment it makes its
    /// first impression. With the provisioner on, they can sell and buy
    /// immediately.
    #[tokio::test]
    async fn a_fresh_market_is_tradable_the_moment_it_opens() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cfg = provisioned_cfg();
        assert!(db.book_for(&m, "wood", "buy").await.unwrap().is_empty(), "dead book");

        db.refresh_provisioner(&m, &cfg, 0).await.unwrap();

        // There is now a bid to sell into and an ask to buy from.
        let bids = db.book_for(&m, "wood", "buy").await.unwrap();
        let asks = db.book_for(&m, "wood", "sell").await.unwrap();
        assert_eq!(bids[0].unit_price, 2, "the floor should be standing");
        assert_eq!(bids[0].qty, 100);
        assert_eq!(asks[0].unit_price, 20, "the ceiling should be standing");
        assert_eq!(asks[0].qty, 50, "the ceiling sells the seeded stock");

        // A brand-new player can immediately turn goods into gold...
        let who = a_seller(&db, &m, "wood", 10).await;
        let before = db.character_gold(&who).await.unwrap();
        let out = db
            .place_order(&m, &who, "sell", "wood", 2, 10, NO_EXPIRY, &cfg, "", 0)
            .await
            .unwrap();
        assert_eq!(out.filled, 10, "the floor should have absorbed it");
        assert!(db.character_gold(&who).await.unwrap() > before);

        // ...and gold into goods.
        let buyer = a_character(&db).await;
        let out = db
            .place_order(&m, &buyer, "buy", "wood", 20, 5, NO_EXPIRY, &cfg, "", 0)
            .await
            .unwrap();
        assert_eq!(out.filled, 5, "the ceiling should have supplied it");
    }

    /// The floor must hold under a dump far larger than any player's purse —
    /// that is what makes it a floor rather than a bid. It is funded by minting,
    /// which is exactly why #154 built the ledger first.
    #[tokio::test]
    async fn the_floor_holds_under_a_dump_and_every_coin_is_ledgered() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let mut cfg = provisioned_cfg();
        cfg.provisioner.get_mut("wood").unwrap().bid_qty = 500;

        let minted = db.refresh_provisioner(&m, &cfg, 0).await.unwrap();
        assert!(minted > 0, "funding a standing bid should have minted gold");
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0, "minting must be ledgered");

        // A whale dumps 400 wood — far more than any player could have paid for.
        // Stocked in trips, because `MAX_CARRY` caps what one player can hold at
        // once; the warehouse is what accumulates.
        let whale = a_character(&db).await;
        for _ in 0..8 {
            db.add_to_inventory(&whale, "wood", 50).await.unwrap();
            db.warehouse_deposit(&m, &whale, "wood", 50, 60).await.unwrap();
        }
        let before = db.character_gold(&whale).await.unwrap();
        let out = db
            .place_order(&m, &whale, "sell", "wood", 2, 400, NO_EXPIRY, &cfg, "", 0)
            .await
            .unwrap();
        assert_eq!(out.filled, 400, "the floor buckled under a dump");
        assert_eq!(db.character_gold(&whale).await.unwrap(), before + 800 - out.sale_tax - out.listing_fee);
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0, "the dump broke the supply identity");

        // And the provisioner now holds the goods, which become its ceiling
        // stock on the next refresh — it is a conduit, not a black hole.
        db.refresh_provisioner(&m, &cfg, 1).await.unwrap();
        let asks = db.book_for(&m, "wood", "sell").await.unwrap();
        assert_eq!(asks[0].qty, 450, "bought stock should be re-offered at the ceiling");
    }

    /// The ceiling sells from STOCK ONLY. Unbounded selling would be an
    /// infinite item faucet — worse than an uncapped price, because it would
    /// destroy scarcity itself.
    #[tokio::test]
    async fn the_ceiling_is_bounded_by_stock_and_never_goes_negative() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let mut cfg = provisioned_cfg();
        cfg.provisioner.get_mut("wood").unwrap().seed_stock = 10;
        db.refresh_provisioner(&m, &cfg, 0).await.unwrap();

        // Try to buy far more than it holds.
        let buyer = a_character(&db).await;
        let out = db
            .place_order(&m, &buyer, "buy", "wood", 20, 25, NO_EXPIRY, &cfg, "", 0)
            .await
            .unwrap();
        assert_eq!(out.filled, 10, "it sold more than it had");

        // Sold out: no ask at all, rather than an ask it cannot honour.
        db.refresh_provisioner(&m, &cfg, 1).await.unwrap();
        assert!(
            db.book_for(&m, "wood", "sell").await.unwrap().is_empty(),
            "a sold-out provisioner must not advertise stock it doesn't have"
        );
        let npc = db.ensure_provisioner().await.unwrap();
        let held: i64 = db
            .warehouse_for_character(&m, &npc)
            .await
            .unwrap()
            .iter()
            .filter(|r| r.item_id == "wood")
            .map(|r| r.qty)
            .sum();
        assert!(held >= 0, "negative stock");
    }

    /// Round-tripping through the provisioner must LOSE money. This is the
    /// property that makes it a safety net rather than a farm: if buying at the
    /// ceiling and selling back at the floor were profitable, it would be an
    /// infinite gold fountain limited only by clicking speed.
    #[tokio::test]
    async fn round_tripping_through_the_provisioner_loses_money() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cfg = provisioned_cfg();
        db.refresh_provisioner(&m, &cfg, 0).await.unwrap();

        let farmer = a_character(&db).await;
        let start = db.character_gold(&farmer).await.unwrap();
        // Buy 10 at the ceiling...
        let out = db
            .place_order(&m, &farmer, "buy", "wood", 20, 10, NO_EXPIRY, &cfg, "", 0)
            .await
            .unwrap();
        assert_eq!(out.filled, 10);
        // ...and sell the same 10 straight back at the floor.
        let out = db
            .place_order(&m, &farmer, "sell", "wood", 2, 10, NO_EXPIRY, &cfg, "", 0)
            .await
            .unwrap();
        assert_eq!(out.filled, 10);
        assert!(
            db.character_gold(&farmer).await.unwrap() < start,
            "the provisioner is a money printer — round-tripping through it turned a profit"
        );
    }

    /// Refreshing is idempotent: it re-posts, it does not accumulate. A job on a
    /// timer that doubled the book every tick would be a slow-motion outage.
    #[tokio::test]
    async fn refreshing_the_provisioner_is_idempotent() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cfg = provisioned_cfg();

        db.refresh_provisioner(&m, &cfg, 0).await.unwrap();
        let bids = db.book_for(&m, "wood", "buy").await.unwrap();
        let asks = db.book_for(&m, "wood", "sell").await.unwrap();

        for t in 1..4 {
            db.refresh_provisioner(&m, &cfg, t).await.unwrap();
        }
        assert_eq!(db.book_for(&m, "wood", "buy").await.unwrap(), bids, "the bid multiplied");
        assert_eq!(db.book_for(&m, "wood", "sell").await.unwrap(), asks, "the ask multiplied");

        // The seed is granted ONCE, however many times the job runs.
        let npc = db.ensure_provisioner().await.unwrap();
        let held: i64 = db
            .warehouse_for_character(&m, &npc)
            .await
            .unwrap()
            .iter()
            .filter(|r| r.item_id == "wood")
            .map(|r| r.qty)
            .sum();
        assert_eq!(held, 50, "seed stock was granted more than once");
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0);
    }

    /// Opt-in, and only for real commodities. Adding an item to the registry
    /// must never silently create a gold faucet for it.
    #[tokio::test]
    async fn a_commodity_without_bounds_gets_no_provisioner_orders() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;

        // Nothing configured at all: no orders, no minting, no system character
        // conjured for nothing.
        assert_eq!(db.refresh_provisioner(&m, &test_cfg(), 0).await.unwrap(), 0);
        assert!(db.book_for(&m, "wood", "buy").await.unwrap().is_empty());
        assert_eq!(db.gold_supply().await.unwrap(), 0, "an unconfigured provisioner minted gold");

        // Wood configured, stone not: stone stays untouched.
        let cfg = provisioned_cfg();
        db.refresh_provisioner(&m, &cfg, 0).await.unwrap();
        assert!(!db.book_for(&m, "wood", "buy").await.unwrap().is_empty());
        assert!(
            db.book_for(&m, "stone", "buy").await.unwrap().is_empty(),
            "an unconfigured commodity got a floor anyway"
        );
    }

    /// The provisioner never trades with itself. Its own bid at the floor and
    /// ask at the ceiling coexist in one book, and self-match prevention plus a
    /// validated spread both have to hold for that to be safe.
    #[tokio::test]
    async fn the_provisioner_never_matches_its_own_orders() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cfg = provisioned_cfg();
        db.refresh_provisioner(&m, &cfg, 0).await.unwrap();
        db.refresh_provisioner(&m, &cfg, 1).await.unwrap();

        assert!(db.recent_trades(&m, "wood", 10).await.unwrap().is_empty(), "it traded with itself");
        assert!(db.book_health().await.unwrap().is_empty(), "the provisioner broke a book invariant");
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0);
    }

    /// The §7 balance telemetry: a trade outside the bounds means either the
    /// refresh job is lagging or the bounds are wrong for the economy that grew
    /// around them. Both are worth knowing, and both are otherwise invisible.
    #[tokio::test]
    async fn trades_outside_the_bounds_are_reported() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cfg = provisioned_cfg();

        // A trade well inside the bounds: nothing to report.
        let s = a_seller(&db, &m, "wood", 20).await;
        sell(&db, &m, &s, "wood", 9, 5).await;
        let b = a_character(&db).await;
        buy(&db, &m, &b, "wood", 9, 5).await;
        assert!(db.trades_outside_bounds(&m, &cfg, 0).await.unwrap().is_empty());

        // A trade above the ceiling — only possible with the provisioner absent
        // or lagging, which is exactly the condition worth surfacing.
        let s2 = a_seller(&db, &m, "wood", 20).await;
        sell(&db, &m, &s2, "wood", 50, 5).await;
        let b2 = a_character(&db).await;
        buy(&db, &m, &b2, "wood", 50, 5).await;
        let flagged = db.trades_outside_bounds(&m, &cfg, 0).await.unwrap();
        assert_eq!(flagged.len(), 1, "an out-of-bounds trade went unreported: {flagged:?}");
        assert_eq!(flagged[0], ("wood".to_string(), 50, 2, 20));
    }

    // --- deposit state (#166) -----------------------------------------------

    /// Only the one fact that can't be recomputed is stored, and an absent row
    /// means full — so a fresh database, a newly authored seam and an untouched
    /// one are all the same case, with nothing to backfill.
    #[tokio::test]
    async fn deposit_depletion_is_remembered_and_forgotten() {
        let (db, _t) = TempDb::open().await;
        assert!(db.depleted_deposits().await.unwrap().is_empty(), "a fresh mine is full");

        db.mark_deposit_depleted("clay_01", 1_000).await.unwrap();
        db.mark_deposit_depleted("iron_04", 2_000).await.unwrap();
        assert_eq!(
            db.depleted_deposits().await.unwrap(),
            vec![("clay_01".to_string(), 1_000), ("iron_04".to_string(), 2_000)]
        );

        // Working out an already-empty seam updates the timestamp rather than
        // duplicating it — this is live state, not a ledger of every depletion.
        db.mark_deposit_depleted("clay_01", 5_000).await.unwrap();
        let rows = db.depleted_deposits().await.unwrap();
        assert_eq!(rows.len(), 2, "a second depletion added a row: {rows:?}");
        assert_eq!(rows[0], ("clay_01".to_string(), 5_000));

        // Coming back forgets it, so a later restart can't resurrect a
        // depletion that has already been served.
        db.clear_deposit_depleted("clay_01").await.unwrap();
        assert_eq!(
            db.depleted_deposits().await.unwrap(),
            vec![("iron_04".to_string(), 2_000)]
        );
        // Clearing something already full is a no-op, not an error.
        db.clear_deposit_depleted("clay_01").await.unwrap();
    }

    // --- the creature bounty (#161) -----------------------------------------

    fn bounty_cfg() -> crate::market_config::BountyConfig {
        crate::market_config::BountyConfig::default()
    }

    async fn a_hunter_with(db: &Db, pelts: i64) -> String {
        let cid = a_character(db).await;
        db.add_to_inventory(&cid, "dog_pelt", pelts).await.unwrap();
        cid
    }

    /// The loop: trophies in, gold out, trophies gone — and repeatable.
    #[tokio::test]
    async fn the_bounty_pays_and_repeats() {
        let (db, _t) = TempDb::open().await;
        let cfg = bounty_cfg();
        let cid = a_hunter_with(&db, cfg.required * 3).await;
        let purse = db.character_gold(&cid).await.unwrap();

        for turn in 1..=3 {
            let (paid, held) = db.turn_in_bounty(&cid, &cfg, &format!("b{turn}"), 0).await.unwrap();
            assert_eq!(paid, cfg.gold, "turn {turn} paid {paid}");
            assert_eq!(held, cfg.required * (3 - turn), "turn {turn} left {held}");
        }
        assert_eq!(db.character_gold(&cid).await.unwrap(), purse + cfg.gold * 3);
        assert_eq!(db.inventory_qty(&cid, "dog_pelt").await.unwrap(), 0);

        // Out of trophies: refused, and nothing is taken.
        let (paid, held) = db.turn_in_bounty(&cid, &cfg, "b4", 0).await.unwrap();
        assert_eq!((paid, held), (0, 0));
    }

    /// A partial hand-in is refused WHOLE. Part-paying would be the worst
    /// outcome available: the player loses goods and gains no bounty.
    #[tokio::test]
    async fn a_partial_hand_in_is_refused_and_consumes_nothing() {
        let (db, _t) = TempDb::open().await;
        let cfg = bounty_cfg();
        let short = cfg.required - 1;
        let cid = a_hunter_with(&db, short).await;
        let purse = db.character_gold(&cid).await.unwrap();

        let (paid, held) = db.turn_in_bounty(&cid, &cfg, "short", 0).await.unwrap();
        assert_eq!(paid, 0, "a short hand-in must pay nothing");
        assert_eq!(held, short, "and it must report what they actually hold");
        assert_eq!(
            db.inventory_qty(&cid, "dog_pelt").await.unwrap(),
            short,
            "a refused turn-in ate the trophies"
        );
        assert_eq!(db.character_gold(&cid).await.unwrap(), purse);
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0);
    }

    /// A resent frame pays ONCE. This is a faucet, so a replayable one isn't a
    /// repeated action — it's a duplication bug that prints money.
    #[tokio::test]
    async fn a_replayed_turn_in_pays_once() {
        let (db, _t) = TempDb::open().await;
        let cfg = bounty_cfg();
        let cid = a_hunter_with(&db, cfg.required * 2).await;
        let purse = db.character_gold(&cid).await.unwrap();

        let (first, _) = db.turn_in_bounty(&cid, &cfg, "same-id", 0).await.unwrap();
        assert_eq!(first, cfg.gold);
        for _ in 0..4 {
            let (again, held) = db.turn_in_bounty(&cid, &cfg, "same-id", 0).await.unwrap();
            assert_eq!(again, 0, "a resent command minted a second bounty");
            assert_eq!(held, cfg.required, "and it should still report the truth");
        }
        assert_eq!(db.character_gold(&cid).await.unwrap(), purse + cfg.gold);
        assert_eq!(
            db.inventory_qty(&cid, "dog_pelt").await.unwrap(),
            cfg.required,
            "the replay consumed a second batch of trophies"
        );
    }

    /// Every coin is on the supply ledger under its own reason, so the largest
    /// faucet in the game is measurable next to the others rather than showing
    /// up as an unexplained rise in the money supply.
    #[tokio::test]
    async fn the_bounty_is_ledgered_as_its_own_faucet() {
        let (db, _t) = TempDb::open().await;
        let cfg = bounty_cfg();
        let cid = a_hunter_with(&db, cfg.required * 2).await;

        let before = db.gold_supply().await.unwrap();
        db.turn_in_bounty(&cid, &cfg, "led1", 0).await.unwrap();
        db.turn_in_bounty(&cid, &cfg, "led2", 0).await.unwrap();

        assert_eq!(
            db.gold_supply().await.unwrap(),
            before + cfg.gold * 2,
            "bounty gold must show up as newly created"
        );
        let by_reason = db.gold_by_reason().await.unwrap();
        let bounty: i64 =
            by_reason.iter().filter(|(r, _)| r == "bounty").map(|(_, g)| *g).sum();
        assert_eq!(bounty, cfg.gold * 2, "not attributed to the bounty: {by_reason:?}");
        assert_eq!(
            db.gold_supply_gap().await.unwrap(),
            0,
            "the bounty broke the supply identity"
        );
    }

    /// The bounty pays for trophies, however they were obtained — a pelt bought
    /// on the market is indistinguishable from one you killed for, and that's
    /// intended (#159). Nothing here should try to prove provenance.
    #[tokio::test]
    async fn bought_trophies_are_as_good_as_killed_ones() {
        let (db, _t) = TempDb::open().await;
        let cfg = bounty_cfg();
        let m = a_market(&db).await;
        let seller = a_character(&db).await;
        db.add_to_inventory(&seller, "dog_pelt", cfg.required).await.unwrap();
        db.warehouse_deposit(&m, &seller, "dog_pelt", cfg.required, 60).await.unwrap();
        sell(&db, &m, &seller, "dog_pelt", 2, cfg.required).await;

        let buyer = a_character(&db).await;
        let out = db
            .place_order(&m, &buyer, "buy", "dog_pelt", 2, cfg.required, NO_EXPIRY, &test_cfg(), "", 0)
            .await
            .unwrap();
        assert_eq!(out.filled, cfg.required, "the pelts should have traded");
        db.warehouse_withdraw(&m, &buyer, "dog_pelt", cfg.required).await.unwrap();

        let (paid, _) = db.turn_in_bounty(&buyer, &cfg, "bought", 0).await.unwrap();
        assert_eq!(paid, cfg.gold, "a bought pelt should claim the bounty just the same");
    }

    // --- the money supply ledger (#154) --------------------------------------

    /// The identity the whole ledger exists for: everything ever minted, minus
    /// everything ever burned, equals what is in purses plus what is escrowed.
    ///
    /// Driven through every path that touches gold — creating characters,
    /// earning wages, trading, paying fees, resting and cancelling orders, and
    /// paying rent — because a ledger that only balances on the paths someone
    /// remembered to wire up is worth nothing.
    #[tokio::test]
    async fn gold_is_conserved_against_the_ledger() {
        let (db, _t) = TempDb::open().await;
        assert_eq!(db.gold_supply().await.unwrap(), 0, "an empty world has no money");
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0);

        let m = a_market(&db).await;
        let seller = a_seller(&db, &m, "wood", 40).await;
        let buyer = a_character(&db).await;
        assert_eq!(
            db.gold_supply().await.unwrap(),
            db.character_gold(&seller).await.unwrap() + db.character_gold(&buyer).await.unwrap(),
            "creating characters is the game's oldest faucet and must be recorded"
        );
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0);

        // Wages: a genuine mint, so the supply GROWS.
        let before = db.gold_supply().await.unwrap();
        let order = db
            .insert_build_order("civic", "town_well", r#"{"wood":20}"#, "open", 0, None, 0, None, None)
            .await
            .unwrap();
        db.add_to_inventory(&seller, "wood", 20).await.unwrap();
        let res = db.contribute(&seller, &order.id, "wood", 20, 1).await.unwrap();
        assert!(res.wages > 0, "the city should have paid something");
        assert_eq!(
            db.gold_supply().await.unwrap(),
            before + res.wages,
            "wages must show up as newly created gold"
        );
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0);

        // Trading moves gold and burns fees; it must never MINT any.
        let after_wages = db.gold_supply().await.unwrap();
        sell(&db, &m, &seller, "wood", 9, 10).await;
        buy(&db, &m, &buyer, "wood", 12, 10).await;
        buy(&db, &m, &buyer, "wood", 3, 5).await; // rests, escrowing gold
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0, "escrowed gold went missing");
        let burned = db.total_fees_burned().await.unwrap();
        assert!(burned > 0);
        assert_eq!(
            db.gold_supply().await.unwrap(),
            after_wages - burned,
            "trading created gold — only fees should have moved the supply, downward"
        );

        // Cancelling returns escrow without minting.
        let supply = db.gold_supply().await.unwrap();
        let open = db.open_orders_for_character(&m, &buyer).await.unwrap();
        for o in open.iter().filter(|o| o.side == "buy") {
            db.cancel_order(&buyer, &o.id).await.unwrap();
        }
        assert_eq!(db.gold_supply().await.unwrap(), supply, "a refund is not a mint");
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0);

        // Rent destroys gold, and the ledger has to see that too or the
        // faucets would look unopposed.
        db.insert_unowned_plot("suburbs", 0, 0, 80, 80, 0).await.unwrap();
        let plot = db.claim_plot(&buyer, "suburbs", 3600, 0).await.unwrap().unwrap();
        let supply = db.gold_supply().await.unwrap();
        assert!(db.pay_rent_with_gold(&buyer, &plot.id, 25, 3600, 100).await.unwrap().is_some());
        assert_eq!(
            db.gold_supply().await.unwrap(),
            supply - 25,
            "rent paid to the city is gold destroyed, and must be recorded as such"
        );
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0);
    }

    /// `market_fee` and `gold_ledger` are written in the same transaction and
    /// must never disagree. Two tables recording the same burn is a deliberate
    /// redundancy — one answers "what did this market take from whom", the other
    /// "how much gold exists" — and this is what stops them drifting apart.
    #[tokio::test]
    async fn fee_ledgers_agree() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let seller = a_seller(&db, &m, "wood", 40).await;
        let buyer = a_character(&db).await;
        sell(&db, &m, &seller, "wood", 9, 20).await;
        buy(&db, &m, &buyer, "wood", 12, 20).await;

        let by_reason = db.gold_by_reason().await.unwrap();
        let ledger_fees: i64 = by_reason
            .iter()
            .filter(|(r, _)| r == "market_fee")
            .map(|(_, g)| *g)
            .sum();
        assert!(ledger_fees < 0, "burns are negative on the supply ledger");
        assert_eq!(
            -ledger_fees,
            db.total_fees_burned().await.unwrap(),
            "the fee table and the supply ledger disagree about what was burned"
        );
    }

    /// The faucet breakdown a balance pass (#129) needs: which sources created
    /// the gold in this world. Previously unanswerable at any price.
    #[tokio::test]
    async fn the_ledger_names_every_faucet() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let who = a_seller(&db, &m, "wood", 40).await;
        let order = db
            .insert_build_order("civic", "town_well", r#"{"wood":20}"#, "open", 0, None, 0, None, None)
            .await
            .unwrap();
        db.add_to_inventory(&who, "wood", 20).await.unwrap();
        db.contribute(&who, &order.id, "wood", 20, 1).await.unwrap();
        sell(&db, &m, &who, "wood", 9, 10).await;

        let reasons: Vec<String> =
            db.gold_by_reason().await.unwrap().into_iter().map(|(r, _)| r).collect();
        assert!(reasons.contains(&"character_start".to_string()), "{reasons:?}");
        assert!(reasons.contains(&"build_wage".to_string()), "{reasons:?}");
        assert!(reasons.contains(&"market_fee".to_string()), "{reasons:?}");
    }

    /// Two markets, one commodity, two books. Every market table has been keyed
    /// by `market_id` since #137, but until #153 that keying had only ever been
    /// exercised with a single market — and untested generality is not
    /// generality. This is where it either holds or turns out to have had a
    /// hardcoded assumption in it all along.
    #[tokio::test]
    async fn two_markets_keep_separate_books() {
        let (db, _t) = TempDb::open().await;
        let a = a_market(&db).await;
        let b = another_market(&db).await;
        assert_ne!(a, b);

        // A seller stocked and resting at A only.
        let sa = a_seller(&db, &a, "wood", 30).await;
        sell(&db, &a, &sa, "wood", 9, 10).await;
        let sa_gold = db.character_gold(&sa).await.unwrap();

        // A's book has depth; B's is empty. If any query fetched "the" book
        // rather than this market's, B would be showing A's asks.
        assert_eq!(db.book_for(&a, "wood", "sell").await.unwrap().len(), 1);
        assert!(
            db.book_for(&b, "wood", "sell").await.unwrap().is_empty(),
            "market B is showing market A's asks"
        );

        // A buy at B that WOULD cross A's ask must not match it — it rests,
        // because there is nothing here to trade with.
        let buyer = a_character(&db).await;
        let out = buy(&db, &b, &buyer, "wood", 20, 10).await;
        assert_eq!(out.filled, 0, "a bid at B matched an ask at A");
        assert!(out.resting_order_id.is_some(), "it should rest at B instead");
        assert_eq!(
            db.book_for(&a, "wood", "sell").await.unwrap()[0].qty, 10,
            "A's ask was consumed by a trade at B"
        );
        assert_eq!(
            db.character_gold(&sa).await.unwrap(), sa_gold,
            "A's seller was paid for a trade that happened at B"
        );

        // The same bid at A does cross, so the setup was genuinely tradable.
        let out = buy(&db, &a, &buyer, "wood", 20, 10).await;
        assert_eq!(out.filled, 10, "the same bid at A should sweep A's ask");
    }

    /// Warehouses are per-market, and that is the whole gameplay of #153:
    /// deposited stock does not follow a player, so moving goods between markets
    /// means CARRYING them. If stock were visible at both, the haul — and the
    /// arbitrage it exists to create — would evaporate.
    #[tokio::test]
    async fn warehouse_stock_does_not_follow_a_player_between_markets() {
        let (db, _t) = TempDb::open().await;
        let a = a_market(&db).await;
        let b = another_market(&db).await;
        let who = a_character(&db).await;

        db.add_to_inventory(&who, "wood", 20).await.unwrap();
        db.warehouse_deposit(&a, &who, "wood", 20, 60).await.unwrap();

        assert_eq!(
            db.warehouse_for_character(&a, &who).await.unwrap().iter().map(|r| r.qty).sum::<i64>(),
            20
        );
        assert!(
            db.warehouse_for_character(&b, &who).await.unwrap().is_empty(),
            "stock deposited at A is visible at B — the haul would be meaningless"
        );

        // Can't sell at B what is stored at A: there is nothing here to escrow.
        let out = sell(&db, &b, &who, "wood", 9, 20).await;
        assert_eq!(out.resting_order_id, None, "sold stock that is held at another market");

        // The full haul: withdraw at A, carry, deposit at B — and now it sells.
        assert_eq!(db.warehouse_withdraw(&a, &who, "wood", 20).await.unwrap(), 20);
        assert_eq!(qty_of(&db.inventory_for_character(&who).await.unwrap(), "wood"), 20);
        db.warehouse_deposit(&b, &who, "wood", 20, 60).await.unwrap();
        let out = sell(&db, &b, &who, "wood", 9, 20).await;
        assert!(out.resting_order_id.is_some(), "the hauled goods should be sellable at B");
        assert!(
            db.warehouse_for_character(&a, &who).await.unwrap().is_empty(),
            "the goods should have LEFT A"
        );
    }

    /// A bid at one market above an ask at another is NOT a crossed book — it is
    /// precisely the arbitrage #153 exists to create, and the boot invariant has
    /// to say so. `book_health` panics the gateway on any violation, so a false
    /// positive here would mean a server that refuses to start *because* the
    /// feature is working.
    #[tokio::test]
    async fn a_bid_at_one_market_above_an_ask_at_another_is_not_a_crossed_book() {
        let (db, _t) = TempDb::open().await;
        let a = a_market(&db).await;
        let b = another_market(&db).await;

        let seller = a_seller(&db, &b, "wood", 20).await;
        sell(&db, &b, &seller, "wood", 5, 10).await; // cheap ask, remote market
        let buyer = a_character(&db).await;
        buy(&db, &a, &buyer, "wood", 12, 10).await; // rich bid, capital

        let problems = db.book_health().await.unwrap();
        assert!(
            problems.is_empty(),
            "cross-market price divergence reported as a fault: {problems:?}"
        );

        // Negative control: the check must not have simply stopped looking. A
        // genuine same-market cross CANNOT be produced through `place_order` —
        // the engine would match it on the spot — which is the whole reason the
        // invariant exists: it catches corruption, not normal operation. So the
        // corruption is injected directly, the way a real bug would leave it.
        let s2 = a_seller(&db, &a, "wood", 20).await;
        sqlx::query(
            "INSERT INTO market_order (id, market_id, character_id, side, item_id, unit_price,              qty_total, qty_remaining, created_seq, created_at, expires_at)              VALUES (?, ?, ?, 'sell', 'wood', 5, 10, 10, 9999, 0, 0)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&a)
        .bind(&s2)
        .execute(&db.pool)
        .await
        .unwrap();
        let problems = db.book_health().await.unwrap();
        assert!(
            problems.iter().any(|p| p.contains("crossed book")),
            "a genuine same-market cross should still be caught: {problems:?}"
        );
    }

    /// A trade at one market must not show up in another's price history.
    /// Candles are what a player reads a market's prices off, so leakage would
    /// paint the capital's prices onto the remote market's chart and destroy the
    /// very signal that makes hauling a decision worth making.
    #[tokio::test]
    async fn candles_and_trades_do_not_leak_between_markets() {
        let (db, _t) = TempDb::open().await;
        let a = a_market(&db).await;
        let b = another_market(&db).await;
        let interval = test_cfg().candle_interval_secs;

        // One fill at each market, at deliberately different prices.
        let sa = a_seller(&db, &a, "wood", 10).await;
        sell(&db, &a, &sa, "wood", 12, 5).await;
        let ba = a_character(&db).await;
        buy(&db, &a, &ba, "wood", 12, 5).await;

        let sb = a_seller(&db, &b, "wood", 10).await;
        sell(&db, &b, &sb, "wood", 4, 5).await;
        let bb = a_character(&db).await;
        buy(&db, &b, &bb, "wood", 4, 5).await;

        let now = now_secs();
        db.roll_up_candles(interval, 0, now + 1).await.unwrap();

        let ca = db.candles(&a, "wood", interval, 0, now + interval).await.unwrap();
        let cb = db.candles(&b, "wood", interval, 0, now + interval).await.unwrap();
        assert_eq!(ca.len(), 1, "capital should have exactly its own candle");
        assert_eq!(cb.len(), 1, "the remote market should have exactly its own candle");
        assert_eq!((ca[0].close, ca[0].volume), (12, 5), "capital's candle");
        assert_eq!((cb[0].close, cb[0].volume), (4, 5), "remote market's candle");

        // And the trade ledger reads are likewise scoped.
        let ta = db.recent_trades(&a, "wood", 10).await.unwrap();
        let tb = db.recent_trades(&b, "wood", 10).await.unwrap();
        assert_eq!(ta.len(), 1);
        assert_eq!(tb.len(), 1);
        assert_eq!(ta[0].unit_price, 12);
        assert_eq!(tb[0].unit_price, 4);
    }

    /// The open-order cap counts `market_id = ? AND character_id = ?`, so a
    /// second market genuinely doubles a player's resting capacity. That IS the
    /// intent — the cap exists to bound one book's size, not a player's total
    /// ambition — but it only becomes true once a second market exists, so it's
    /// worth pinning deliberately rather than discovering later.
    #[tokio::test]
    async fn the_open_order_cap_is_per_market_not_global() {
        let (db, _t) = TempDb::open().await;
        let a = a_market(&db).await;
        let b = another_market(&db).await;
        let who = a_character(&db).await;
        db.add_to_inventory(&who, "wood", 40).await.unwrap();
        db.warehouse_deposit(&a, &who, "wood", 20, 60).await.unwrap();
        db.warehouse_deposit(&b, &who, "wood", 20, 60).await.unwrap();
        let capped = MarketConfig { max_open_orders: 2, ..test_cfg() };

        for i in 0..2 {
            let o = db
                .place_order(&a, &who, "sell", "wood", 5 + i, 2, NO_EXPIRY, &capped, "", 0)
                .await
                .unwrap();
            assert!(o.resting_order_id.is_some(), "order {i} should rest at A");
        }
        let o = db
            .place_order(&a, &who, "sell", "wood", 9, 2, NO_EXPIRY, &capped, "", 0)
            .await
            .unwrap();
        assert!(o.resting_order_id.is_none(), "the cap should bind at A");
        let o = db
            .place_order(&b, &who, "sell", "wood", 9, 2, NO_EXPIRY, &capped, "", 0)
            .await
            .unwrap();
        assert!(o.resting_order_id.is_some(), "the cap must not be global");
    }

    /// Conservation still holds when the goods and gold are spread across two
    /// markets. The invariant was written against one; with two, a leak would
    /// most plausibly show up as stock or escrow being attributed to the wrong
    /// market, which a single-market sum would never notice.
    #[tokio::test]
    async fn two_markets_conserve_goods_and_gold_between_them() {
        let (db, _t) = TempDb::open().await;
        let a = a_market(&db).await;
        let b = another_market(&db).await;

        let hauler = a_character(&db).await;
        db.add_to_inventory(&hauler, "wood", 40).await.unwrap();
        let buyer_a = a_character(&db).await;
        let buyer_b = a_character(&db).await;
        let who = [hauler.as_str(), buyer_a.as_str(), buyer_b.as_str()];

        let total_wood = 40i64;
        let total_gold: i64 = {
            let mut g = 0;
            for w in who {
                g += db.character_gold(w).await.unwrap();
            }
            g
        };

        async fn wood_everywhere(db: &Db, markets: &[&str], who: &[&str]) -> i64 {
            let mut n = 0i64;
            for w in who {
                n += qty_of(&db.inventory_for_character(w).await.unwrap(), "wood");
                for m in markets {
                    n += db
                        .warehouse_for_character(m, w)
                        .await
                        .unwrap()
                        .iter()
                        .filter(|r| r.item_id == "wood")
                        .map(|r| r.qty)
                        .sum::<i64>();
                }
            }
            n
        }

        async fn gold_everywhere(db: &Db, who: &[&str]) -> i64 {
            let mut g = 0i64;
            for w in who {
                g += db.character_gold(w).await.unwrap();
            }
            g
        }

        /// Escrowed gold is simply absent from purses, with the open buy book
        /// as its only record — so the books of BOTH markets are part of the
        /// accounting, not outside it.
        async fn escrow_everywhere(db: &Db, markets: &[&str], who: &[&str]) -> i64 {
            let mut g = 0i64;
            for m in markets {
                for w in who {
                    g += db
                        .open_orders_for_character(m, w)
                        .await
                        .unwrap()
                        .iter()
                        .filter(|o| o.side == "buy")
                        .map(|o| o.unit_price * o.qty_remaining)
                        .sum::<i64>();
                }
            }
            g
        }

        // Split the stock across both markets, trade at both, and haul between.
        db.warehouse_deposit(&a, &hauler, "wood", 20, 60).await.unwrap();
        db.warehouse_deposit(&b, &hauler, "wood", 20, 60).await.unwrap();
        sell(&db, &a, &hauler, "wood", 10, 10).await;
        sell(&db, &b, &hauler, "wood", 4, 10).await;
        buy(&db, &a, &buyer_a, "wood", 10, 6).await;
        buy(&db, &b, &buyer_b, "wood", 4, 10).await;
        db.warehouse_withdraw(&b, &buyer_b, "wood", 5).await.unwrap();
        db.warehouse_deposit(&a, &buyer_b, "wood", 5, 60).await.unwrap();
        sell(&db, &a, &buyer_b, "wood", 11, 5).await;

        let markets = [a.as_str(), b.as_str()];
        assert_eq!(
            wood_everywhere(&db, &markets, &who).await,
            total_wood,
            "wood was created or destroyed across the two markets"
        );
        assert_eq!(
            gold_everywhere(&db, &who).await
                + escrow_everywhere(&db, &markets, &who).await
                + db.total_fees_burned().await.unwrap(),
            total_gold,
            "gold was created or destroyed across the two markets"
        );
        assert!(db.book_health().await.unwrap().is_empty(), "both books stay healthy");
    }

    /// is the test that matters most — duplication kills economies.
    #[tokio::test]
    async fn trading_conserves_goods_and_gold() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let s = a_seller(&db, &m, "wood", 50).await;
        let b = a_character(&db).await;
        db.add_to_inventory(&b, "wood", 10).await.unwrap();
        db.warehouse_deposit(&m, &b, "wood", 10, 60).await.unwrap();

        let total_wood = 60;
        let total_gold = db.character_gold(&s).await.unwrap() + db.character_gold(&b).await.unwrap();

        async fn wood_now(db: &Db, m: &str, who: &[&str]) -> i64 {
            let mut n = 0i64;
            for w in who {
                n += qty_of(&db.inventory_for_character(w).await.unwrap(), "wood");
                n += db.warehouse_for_character(m, w).await.unwrap()
                    .iter().filter(|r| r.item_id == "wood").map(|r| r.qty).sum::<i64>();
            }
            n
        }

        let mut seed: u64 = 0xfeed;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as i64
        };
        for step in 0..50 {
            match next() % 4 {
                0 => { sell(&db, &m, &s, "wood", 1 + next() % 9, 1 + next() % 7).await; }
                1 => { sell(&db, &m, &b, "wood", 1 + next() % 9, 1 + next() % 5).await; }
                2 => {
                    let who = if next() % 2 == 0 { s.clone() } else { b.clone() };
                    buy(&db, &m, &who, "wood", 1 + next() % 12, 1 + next() % 7).await;
                }
                _ => {
                    let who = if next() % 2 == 0 { s.clone() } else { b.clone() };
                    if let Some(o) = db.open_orders_for_character(&m, &who).await.unwrap().first() {
                        db.cancel_order(&who, &o.id).await.unwrap();
                    }
                }
            }
            assert_eq!(
                wood_now(&db, &m, &[&s, &b]).await, total_wood,
                "wood conserved at step {step}"
            );
            // Gold is conserved across purses PLUS escrow. Escrowed gold is
            // simply absent from purses, with the open buy book as its only
            // record — so the book is part of the accounting, not outside it.
            let escrowed: i64 = db
                .open_orders_for_character(&m, &s)
                .await
                .unwrap()
                .iter()
                .chain(db.open_orders_for_character(&m, &b).await.unwrap().iter())
                .filter(|o| o.side == "buy")
                .map(|o| o.unit_price * o.qty_remaining)
                .sum();
            // Gold is conserved across purses + escrow + everything the fee
            // sink has BURNED (#141). Burned gold is credited nowhere, so the
            // fee ledger is the only thing that closes the books — if this
            // holds, no gold was created or lost, only moved or destroyed on
            // purpose.
            assert_eq!(
                db.character_gold(&s).await.unwrap()
                    + db.character_gold(&b).await.unwrap()
                    + escrowed
                    + db.total_fees_burned().await.unwrap(),
                total_gold,
                "gold conserved at step {step}"
            );
            assert!(db.book_health().await.unwrap().is_empty(), "book invariants hold at step {step}");
        }
    }

    #[tokio::test]
    async fn escrow_reconciles_against_the_open_book() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let s = a_seller(&db, &m, "wood", 30).await;
        sell(&db, &m, &s, "wood", 5, 12).await;
        sell(&db, &m, &s, "wood", 6, 8).await;

        // Escrowed stock must exactly equal what the open book promises — a
        // mismatch means goods were duplicated or lost (#136 §8.3).
        for (item, book_qty, locked_qty) in db.escrow_reconciliation(&m).await.unwrap() {
            assert_eq!(book_qty, locked_qty, "{item}: book says {book_qty}, escrow holds {locked_qty}");
        }

        // Still true after a partial fill and a cancel.
        let buyer = a_character(&db).await;
        buy(&db, &m, &buyer, "wood", 5, 7).await;
        let mine = db.open_orders_for_character(&m, &s).await.unwrap();
        db.cancel_order(&s, &mine[0].id).await.unwrap();
        for (item, book_qty, locked_qty) in db.escrow_reconciliation(&m).await.unwrap() {
            assert_eq!(book_qty, locked_qty, "{item} after fill+cancel");
        }
    }

    #[tokio::test]
    async fn warehouse_deposit_and_withdraw_round_trip_and_respect_carry_capacity() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "wood", 30).await.unwrap();

        // Deposit moves carried -> available.
        assert_eq!(db.warehouse_deposit(&m, &cid, "wood", 20, 60).await.unwrap(), 20);
        let held = db.warehouse_for_character(&m, &cid).await.unwrap();
        assert_eq!(held.len(), 1, "one stack, one slot");
        assert_eq!((held[0].qty, held[0].state.as_str()), (20, "available"));
        assert_eq!(qty_of(&db.inventory_for_character(&cid).await.unwrap(), "wood"), 10);

        // A second deposit of the same item merges into the existing stack
        // rather than eating another slot.
        assert_eq!(db.warehouse_deposit(&m, &cid, "wood", 10, 60).await.unwrap(), 10);
        let held = db.warehouse_for_character(&m, &cid).await.unwrap();
        assert_eq!((held.len(), held[0].qty), (1, 30));

        // Withdraw is bounded by remaining carry capacity (MAX_CARRY = 50).
        db.add_to_inventory(&cid, "stone", 45).await.unwrap();
        assert_eq!(db.warehouse_withdraw(&m, &cid, "wood", 30).await.unwrap(), 5, "only 5 carry slots left");
        assert_eq!(db.warehouse_for_character(&m, &cid).await.unwrap()[0].qty, 25);

        // Depositing an item you don't carry moves nothing.
        assert_eq!(db.warehouse_deposit(&m, &cid, "plank", 5, 60).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn warehouse_locked_stock_is_not_withdrawable() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "wood", 30).await.unwrap();
        db.warehouse_deposit(&m, &cid, "wood", 30, 60).await.unwrap();

        // Lock 10 against a (future) sell order: the stack splits.
        assert_eq!(db.warehouse_lock(&m, &cid, "wood", 10).await.unwrap(), 10);
        let held = db.warehouse_for_character(&m, &cid).await.unwrap();
        let available: i64 = held.iter().filter(|r| r.state == "available").map(|r| r.qty).sum();
        let locked: i64 = held.iter().filter(|r| r.state == "locked").map(|r| r.qty).sum();
        assert_eq!((available, locked), (20, 10));

        // Withdrawing "everything" only takes the available part — escrowed
        // stock isn't the player's to take back until the order is cancelled.
        assert_eq!(db.warehouse_withdraw(&m, &cid, "wood", 999).await.unwrap(), 20);
        let held = db.warehouse_for_character(&m, &cid).await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!((held[0].qty, held[0].state.as_str()), (10, "locked"));
        assert_eq!(db.warehouse_withdraw(&m, &cid, "wood", 10).await.unwrap(), 0, "locked stays put");
    }

    #[tokio::test]
    async fn warehouse_capacity_is_slots_and_a_full_warehouse_refuses_outright() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cid = a_character(&db).await;
        // Two slots only. Fill both with different items.
        db.add_to_inventory(&cid, "wood", 5).await.unwrap();
        db.add_to_inventory(&cid, "stone", 5).await.unwrap();
        db.add_to_inventory(&cid, "plank", 5).await.unwrap();
        assert_eq!(db.warehouse_deposit(&m, &cid, "wood", 5, 2).await.unwrap(), 5);
        assert_eq!(db.warehouse_deposit(&m, &cid, "stone", 5, 2).await.unwrap(), 5);

        // A third distinct item needs a third slot: refused OUTRIGHT, with
        // nothing moved — a half-landed deposit is what players report as
        // lost goods.
        assert_eq!(db.warehouse_deposit(&m, &cid, "plank", 5, 2).await.unwrap(), 0);
        assert_eq!(qty_of(&db.inventory_for_character(&cid).await.unwrap(), "plank"), 5, "nothing taken");
        assert_eq!(db.warehouse_for_character(&m, &cid).await.unwrap().len(), 2);

        // Topping up an item already stored consumes no new slot, so it's
        // still allowed at capacity.
        db.add_to_inventory(&cid, "wood", 5).await.unwrap();
        assert_eq!(db.warehouse_deposit(&m, &cid, "wood", 5, 2).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn warehousing_a_tool_preserves_the_instance_and_its_wear() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "pickaxe", 1).await.unwrap();
        let inv = db.inventory_for_character(&cid).await.unwrap();
        let pick = inv.iter().find(|i| i.item_id == "pickaxe").unwrap().clone();
        assert_eq!(pick.durability, crate::world::tool_max_durability("pickaxe"));

        // Wear it, so "the same instance" is actually falsifiable.
        db.equip_instance(&cid, &pick.id).await.unwrap();
        db.wear_equipped_tool(&cid, "tool", 7).await.unwrap();
        let worn = db.inventory_for_character(&cid).await.unwrap()
            .into_iter().find(|i| i.item_id == "pickaxe").unwrap();
        assert_eq!(
            worn.durability,
            crate::world::tool_max_durability("pickaxe").map(|m| m - 7)
        );

        // Deposit: same row id, same wear, one slot.
        assert_eq!(db.warehouse_deposit(&m, &cid, "pickaxe", 1, 60).await.unwrap(), 1);
        let held = db.warehouse_for_character(&m, &cid).await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].id, worn.id, "the SAME instance, not a fresh tool");
        assert_eq!((held[0].durability, held[0].qty), (worn.durability, 1));
        assert!(db.inventory_for_character(&cid).await.unwrap().iter().all(|i| i.item_id != "pickaxe"));
        // Stowing the tool you're holding takes it out of your hand, rather
        // than refusing the deposit or leaving a dangling equipment row.
        assert!(db.equipped_tool(&cid, "tool").await.unwrap().is_none(), "deposited tool is unequipped");

        // Withdraw: still the same instance, still worn.
        assert_eq!(db.warehouse_withdraw(&m, &cid, "pickaxe", 1).await.unwrap(), 1);
        let back = db.inventory_for_character(&cid).await.unwrap()
            .into_iter().find(|i| i.item_id == "pickaxe").unwrap();
        assert_eq!((back.id, back.durability), (worn.id.clone(), worn.durability));
        assert!(db.warehouse_for_character(&m, &cid).await.unwrap().is_empty());
    }

    /// The epic's headline invariant, first installment (#136 §12): across a
    /// stream of deposits, withdrawals and locks, an item is conserved —
    /// `carried + available + locked` never changes. Duplication kills
    /// economies, so this is the test that matters most here.
    #[tokio::test]
    async fn warehouse_conserves_goods_across_a_random_command_stream() {
        let (db, _t) = TempDb::open().await;
        let m = a_market(&db).await;
        let cid = a_character(&db).await;
        const TOTAL: i64 = 40;
        db.add_to_inventory(&cid, "wood", TOTAL).await.unwrap();

        // Deterministic pseudo-random stream, so a failure is reproducible.
        let mut seed: u64 = 0x5eed;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as i64
        };
        for step in 0..60 {
            let qty = next() % 13; // includes 0 and over-asks
            match next() % 3 {
                0 => { db.warehouse_deposit(&m, &cid, "wood", qty, 60).await.unwrap(); }
                1 => { db.warehouse_withdraw(&m, &cid, "wood", qty).await.unwrap(); }
                _ => { db.warehouse_lock(&m, &cid, "wood", qty).await.unwrap(); }
            }
            let carried = qty_of(&db.inventory_for_character(&cid).await.unwrap(), "wood");
            let held: i64 = db.warehouse_for_character(&m, &cid).await.unwrap()
                .iter().filter(|r| r.item_id == "wood").map(|r| r.qty).sum();
            assert_eq!(carried + held, TOTAL, "wood conserved at step {step}");
        }
    }

    /// Build wages (#145) are paid on the units that actually MOVED, in the
    /// same transaction as the contribution, through both contribute paths —
    /// and a wage of 0 (demolition orders) mints nothing.
    #[tokio::test]
    async fn build_wages_pay_on_moved_units_through_both_contribute_paths() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        let start = db.character_gold(&cid).await.unwrap();
        assert_eq!(start, 500, "the migration's starting balance");
        db.add_to_inventory(&cid, "wood", 30).await.unwrap();
        db.add_to_inventory(&cid, "stone", 30).await.unwrap();

        // Pooled path. The order needs 20 wood; offer 30. Wages must follow
        // the 20 that MOVED, not the 30 offered — the two differ constantly,
        // since contributions are bounded by remaining need and carried stock.
        let well = db
            .insert_build_order("civic", "town_well", r#"{"wood":20,"stone":10}"#, "open", 0, None, 0, None, None)
            .await
            .unwrap();
        let r = db.contribute(&cid, &well.id, "wood", 30, 3).await.unwrap();
        assert_eq!(r.moved, 20, "bounded by the order's need");
        assert_eq!(r.wages, 60, "3 gold x the 20 units that moved, not the 30 offered");
        assert_eq!(db.character_gold(&cid).await.unwrap(), start + 60, "credited in the same tx");

        // Per-cell road path pays on the same rule.
        let road = db
            .insert_build_order("civic", "road_x", r#"{"stone":4}"#, "open", 0, None, 0, None, Some("[[100,100],[110,100]]"))
            .await
            .unwrap();
        db.insert_road_cells(&road.id, &two_road_cells()).await.unwrap();
        let r = db.contribute_to_road_cell(&cid, &road.id, 0, "stone", 5, 3).await.unwrap();
        assert_eq!(r.moved, 2, "bounded by the cell's own cost");
        assert_eq!(r.wages, 6);
        assert_eq!(db.character_gold(&cid).await.unwrap(), start + 66);

        // A zero wage (what the gateway passes for `demo_*` orders) mints
        // nothing, even though the contribution itself still lands.
        let before = db.character_gold(&cid).await.unwrap();
        let r = db.contribute_to_road_cell(&cid, &road.id, 1, "stone", 2, 0).await.unwrap();
        assert_eq!(r.moved, 2, "the contribution still happens");
        assert_eq!(r.wages, 0);
        assert_eq!(db.character_gold(&cid).await.unwrap(), before, "no gold minted at a zero rate");
    }

    /// A contribution that moves nothing pays nothing — the guard that stops
    /// a spam-click on a filled order from minting gold for free.
    #[tokio::test]
    async fn build_wages_pay_nothing_when_no_units_move() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        let start = db.character_gold(&cid).await.unwrap();
        let order = db
            .insert_build_order("civic", "town_well", r#"{"wood":5}"#, "open", 0, None, 0, None, None)
            .await
            .unwrap();

        // Carrying nothing: nothing moves, nothing is paid.
        let r = db.contribute(&cid, &order.id, "wood", 5, 10).await.unwrap();
        assert_eq!((r.moved, r.wages), (0, 0));
        assert_eq!(db.character_gold(&cid).await.unwrap(), start);

        // Fill it, then contribute again into an already-met requirement.
        db.add_to_inventory(&cid, "wood", 10).await.unwrap();
        assert_eq!(db.contribute(&cid, &order.id, "wood", 5, 10).await.unwrap().wages, 50);
        let paid = db.character_gold(&cid).await.unwrap();
        let r = db.contribute(&cid, &order.id, "wood", 5, 10).await.unwrap();
        assert_eq!((r.moved, r.wages), (0, 0), "a completed order pays no more wages");
        assert_eq!(db.character_gold(&cid).await.unwrap(), paid, "no free gold from re-contributing");
    }

    /// Two 5m road cells (progressive road building epic #131, issue #132).
    fn two_road_cells() -> Vec<RoadCellSpec> {
        vec![
            RoadCellSpec { x0: 100, y0: 100, x1: 105, y1: 100, required_json: r#"{"stone":2}"#.to_string() },
            RoadCellSpec { x0: 105, y0: 100, x1: 110, y1: 100, required_json: r#"{"stone":2}"#.to_string() },
        ]
    }

    #[tokio::test]
    async fn road_cell_contribute_updates_the_cell_and_mirrors_into_the_order_aggregate() {
        let (db, _t) = TempDb::open().await;
        let order = db
            .insert_build_order("civic", "road_x", r#"{"stone":4}"#, "open", 0, None, 0, None, Some("[[100,100],[110,100]]"))
            .await
            .unwrap();
        db.insert_road_cells(&order.id, &two_road_cells()).await.unwrap();

        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "stone", 10).await.unwrap();

        // Partial contribution to cell 0: cell progress moves, cell isn't
        // done yet, and the order's own aggregate mirrors the same amount.
        let r = db.contribute_to_road_cell(&cid, &order.id, 0, "stone", 1, 0).await.unwrap();
        assert_eq!(r.moved, 1);
        assert!(!r.cell_completed);
        assert!(!r.order_completed);
        let order_row = db.build_order_by_id(&order.id).await.unwrap().unwrap();
        assert_eq!(order_row.progress_json, r#"{"stone":1}"#, "order aggregate mirrors the cell contribution");
        let cells = db.road_cells_for_order(&order.id).await.unwrap();
        assert_eq!(cells[0].progress_json, r#"{"stone":1}"#);
        assert!(cells[0].completed_at.is_none());

        // Finish cell 0: only THAT cell completes, not the whole road.
        let r = db.contribute_to_road_cell(&cid, &order.id, 0, "stone", 1, 0).await.unwrap();
        assert!(r.cell_completed);
        assert!(!r.order_completed);
        let cells = db.road_cells_for_order(&order.id).await.unwrap();
        assert!(cells[0].completed_at.is_some());
        assert!(cells[1].completed_at.is_none());

        // A contribution bounded past a completed cell's need moves nothing.
        assert_eq!(db.contribute_to_road_cell(&cid, &order.id, 0, "stone", 5, 0).await.unwrap().moved, 0);

        // Finish cell 1 too: the WHOLE order completes, contributors report,
        // exactly like the pooled `contribute` path.
        let r = db.contribute_to_road_cell(&cid, &order.id, 1, "stone", 2, 0).await.unwrap();
        assert!(r.cell_completed);
        assert!(r.order_completed, "every cell done completes the order");
        assert_eq!(r.contributors, vec![(cid.clone(), 4)]);
        let order_row = db.build_order_by_id(&order.id).await.unwrap().unwrap();
        assert_eq!(order_row.state, "completed");
        assert_eq!(order_row.progress_json, r#"{"stone":4}"#);

        // Nothing more moves into a completed order.
        assert_eq!(db.contribute_to_road_cell(&cid, &order.id, 1, "stone", 1, 0).await.unwrap().moved, 0);
    }

    #[tokio::test]
    async fn road_cell_contribute_is_a_noop_for_an_unknown_cell_or_order() {
        let (db, _t) = TempDb::open().await;
        let order = db
            .insert_build_order("civic", "road_x", r#"{"stone":2}"#, "open", 0, None, 0, None, Some("[[100,100],[105,100]]"))
            .await
            .unwrap();
        db.insert_road_cells(&order.id, &two_road_cells()[..1]).await.unwrap();
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "stone", 10).await.unwrap();

        assert_eq!(db.contribute_to_road_cell(&cid, "no-such-order", 0, "stone", 1, 0).await.unwrap().moved, 0);
        assert_eq!(db.contribute_to_road_cell(&cid, &order.id, 9, "stone", 1, 0).await.unwrap().moved, 0, "no cell at that index");
    }

    #[tokio::test]
    async fn replan_road_order_preserves_progress_on_cells_whose_span_is_unchanged() {
        let (db, _t) = TempDb::open().await;
        let order = db
            .insert_build_order("civic", "road_x", r#"{"stone":4}"#, "open", 0, None, 0, None, Some("[[100,100],[110,100]]"))
            .await
            .unwrap();
        db.insert_road_cells(&order.id, &two_road_cells()).await.unwrap();
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "stone", 10).await.unwrap();
        // Finish cell 0 only.
        db.contribute_to_road_cell(&cid, &order.id, 0, "stone", 2, 0).await.unwrap();

        // Replan: keep the first cell's span identical (100,100)-(105,100),
        // extend the road further east so the SECOND cell's span changes.
        let new_cells = vec![
            RoadCellSpec { x0: 100, y0: 100, x1: 105, y1: 100, required_json: r#"{"stone":2}"#.to_string() },
            RoadCellSpec { x0: 105, y0: 100, x1: 115, y1: 100, required_json: r#"{"stone":3}"#.to_string() },
        ];
        let placement = BuildPlacement { structure_kind: "dirt_road".to_string(), x: 100, y: 100, x1: Some(115), y1: Some(100) };
        let outcome = db
            .replan_road_order(&order.id, "civic", r#"{"stone":5}"#, "[[100,100],[115,100]]", &placement, &new_cells, 0)
            .await
            .unwrap();
        assert!(outcome.applied);
        assert!(!outcome.completed, "the recomputed 5-stone cost isn't covered by the kept 2");

        let cells = db.road_cells_for_order(&order.id).await.unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].progress_json, r#"{"stone":2}"#, "unchanged span keeps its progress");
        assert!(cells[0].completed_at.is_some(), "and stays completed");
        assert_eq!(cells[1].progress_json, "{}", "the reshaped cell starts fresh");
        assert!(cells[1].completed_at.is_none());
    }

    #[tokio::test]
    async fn settle_demolition_refunds_the_full_progress_including_a_part_finished_cell() {
        let (db, _t) = TempDb::open().await;
        let order = db
            .insert_build_order("civic", "road_x", r#"{"stone":4}"#, "open", 0, None, 0, None, Some("[[100,100],[110,100]]"))
            .await
            .unwrap();
        db.insert_road_cells(&order.id, &two_road_cells()).await.unwrap();
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "stone", 10).await.unwrap();
        // Cell 0 fully done (2 stone), cell 1 only half done (1 of 2 stone)
        // — the road as a whole is still open, not completed.
        db.contribute_to_road_cell(&cid, &order.id, 0, "stone", 2, 0).await.unwrap();
        db.contribute_to_road_cell(&cid, &order.id, 1, "stone", 1, 0).await.unwrap();

        db.create_demolition(&order.id, 0).await.unwrap().unwrap();
        let (target, refund) = db.settle_demolition(&order.id).await.unwrap().unwrap();
        assert_eq!(target.id, order.id);
        assert_eq!(refund.get("stone"), Some(&3), "the partial cell's stone is refunded too, not lost");

        assert!(db.road_cells_for_order(&order.id).await.unwrap().is_empty(), "cells are cleaned up with the order");
        assert!(db.build_order_by_id(&order.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancel_road_order_removes_its_cells_too() {
        let (db, _t) = TempDb::open().await;
        let order = db
            .insert_build_order("civic", "road_x", r#"{"stone":4}"#, "open", 0, None, 0, None, Some("[[100,100],[110,100]]"))
            .await
            .unwrap();
        db.insert_road_cells(&order.id, &two_road_cells()).await.unwrap();
        assert!(db.cancel_road_order(&order.id).await.unwrap());
        assert!(db.road_cells_for_order(&order.id).await.unwrap().is_empty());
        assert!(db.build_order_by_id(&order.id).await.unwrap().is_none());
    }

    /// Seeding a NEW prereq'd order into a world that already finished its
    /// prerequisite must open it, not lock it forever.
    ///
    /// The unlock normally fires on the completion *event* — `build.completed`
    /// walks the authored dependents. An order authored after that event has
    /// already passed has nothing left to trigger it, so it would seed `locked`
    /// and stay dead content permanently. That is not hypothetical: the second
    /// market (#153) was authored into worlds whose capital market was already
    /// built, including the dev database, where it seeded locked and could never
    /// have opened. Seeding is the only place with the whole picture.
    #[tokio::test]
    async fn seeding_opens_a_dependent_whose_prerequisite_is_already_done() {
        let (db, _t) = TempDb::open().await;

        // A world where the capital's market is already finished.
        db.insert_build_order(
            "civic", "market", r#"{"wood":1}"#, "completed", 0, None, 0, None, None,
        )
        .await
        .unwrap();

        db.seed_capital(&crate::world::capital(), 0).await.unwrap();

        let remote = db
            .build_orders_for_district("market")
            .await
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "market_east")
            .expect("the second market should be seeded");
        assert_eq!(
            remote.state, "open",
            "a dependent whose prerequisite is already complete must seed open, or it is \
             permanently dead content"
        );

        // The converse still holds: on a fresh world it seeds locked, because
        // the completion event is still ahead of it.
        let (fresh, _t2) = TempDb::open().await;
        fresh.seed_capital(&crate::world::capital(), 0).await.unwrap();
        let remote = fresh
            .build_orders_for_district("market")
            .await
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "market_east")
            .unwrap();
        assert_eq!(remote.state, "locked", "on a fresh world the second market is still gated");
        assert!(
            !fresh.is_build_kind_completed("market").await.unwrap(),
            "nothing is built on a fresh world"
        );
    }

    #[tokio::test]
    async fn open_build_order_unlocks_a_locked_dependent() {
        let (db, _t) = TempDb::open().await;
        db.insert_build_order("civic", "wall_section", r#"{"stone":30}"#, "locked", 0, None, 0, None, None)
            .await
            .unwrap();
        // Unlock flips it open and returns it; a second call is a no-op.
        let opened = db.open_build_order("civic", "wall_section").await.unwrap().unwrap();
        assert_eq!(opened.state, "open");
        assert!(db.open_build_order("civic", "wall_section").await.unwrap().is_none());
        // A locked order rejects contributions until opened.
        let locked = db
            .insert_build_order("civic", "market_stall", r#"{"wood":40}"#, "locked", 0, None, 0, None, None)
            .await
            .unwrap();
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "wood", 40).await.unwrap();
        assert_eq!(db.contribute(&cid, &locked.id, "wood", 40, 0).await.unwrap().moved, 0,
            "a locked order accepts nothing");
    }

    #[tokio::test]
    async fn skill_gated_order_rejects_until_the_threshold_is_reached() {
        let (db, _t) = TempDb::open().await;
        // An open order that still requires Building 1 to contribute to.
        let order = db
            .insert_build_order("civic", "watchtower", r#"{"wood":30}"#, "open", 0, Some("building"), 1, None, None)
            .await
            .unwrap();
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "wood", 30).await.unwrap();

        // Below the threshold (Building 0): the gate rejects, nothing moves, the wood
        // stays carried, and the order does not complete.
        let r = db.contribute(&cid, &order.id, "wood", 30, 0).await.unwrap();
        assert_eq!(r.moved, 0, "greyed order accepts nothing below its skill threshold");
        assert!(!r.completed);
        assert_eq!(db.inventory_total(&cid).await.unwrap(), 30, "wood untouched");

        // Reach Building 1, then the same contribution succeeds and completes it.
        db.grant_skill_xp(&cid, "building", 100).await.unwrap();
        assert_eq!(db.skill_level(&cid, "building").await.unwrap(), 1);
        let r = db.contribute(&cid, &order.id, "wood", 30, 0).await.unwrap();
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

    // --- Tool durability & instancing (mining/abilities epic #123 backlog, #128) --

    /// Tools never stack — granting two pickaxes creates two separate
    /// fresh-durability rows, not one row at qty 2. Ordinary items are
    /// untouched (still merge onto one stack row).
    #[tokio::test]
    async fn tools_are_instanced_not_stacked_on_grant() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "pickaxe", 1).await.unwrap();
        db.add_to_inventory(&cid, "pickaxe", 1).await.unwrap();
        db.add_to_inventory(&cid, "wood", 3).await.unwrap();
        db.add_to_inventory(&cid, "wood", 2).await.unwrap();

        let items = db.inventory_for_character(&cid).await.unwrap();
        let picks: Vec<_> = items.iter().filter(|i| i.item_id == "pickaxe").collect();
        assert_eq!(picks.len(), 2, "two grants -> two separate instance rows");
        let fresh = crate::world::tool_max_durability("pickaxe").unwrap();
        assert!(picks.iter().all(|p| p.qty == 1 && p.durability == Some(fresh)), "each a fresh instance: {picks:?}");
        assert_ne!(picks[0].id, picks[1].id, "distinct instance ids");

        let wood: Vec<_> = items.iter().filter(|i| i.item_id == "wood").collect();
        assert_eq!(wood.len(), 1, "ordinary items still merge onto one stack row");
        assert_eq!(wood[0].qty, 5);
        assert_eq!(wood[0].durability, None, "not a tool -> no durability tracked");
    }

    /// Equipping targets a specific instance: wrong owner, unknown id, and a
    /// non-equippable item are all rejected; a legitimate equip returns the
    /// derived slot and item, and is reflected by `equipped`/`equipped_tool`.
    #[tokio::test]
    async fn equip_instance_validates_ownership_and_equippability() {
        let (db, _t) = TempDb::open().await;
        let owner = a_character(&db).await;
        let stranger = a_character(&db).await;
        db.add_to_inventory(&owner, "pickaxe", 1).await.unwrap();
        db.add_to_inventory(&owner, "wood", 1).await.unwrap();
        let items = db.inventory_for_character(&owner).await.unwrap();
        let pick_id = &items.iter().find(|i| i.item_id == "pickaxe").unwrap().id;
        let wood_id = &items.iter().find(|i| i.item_id == "wood").unwrap().id;

        assert_eq!(db.equip_instance(&owner, "no-such-id").await.unwrap(), None, "unknown instance");
        assert_eq!(db.equip_instance(&stranger, pick_id).await.unwrap(), None, "not the owner");
        assert_eq!(db.equip_instance(&owner, wood_id).await.unwrap(), None, "not equippable at all");

        let ok = db.equip_instance(&owner, pick_id).await.unwrap();
        assert_eq!(ok, Some(("tool", "pickaxe".to_string())));
        assert_eq!(db.equipped(&owner, "tool").await.unwrap().as_deref(), Some("pickaxe"));
        let tool = db.equipped_tool(&owner, "tool").await.unwrap().unwrap();
        assert_eq!(tool.instance_id, *pick_id);
        let fresh = crate::world::tool_max_durability("pickaxe").unwrap();
        assert_eq!(tool.durability, fresh);
        assert_eq!(tool.max_durability, fresh);
    }

    /// A swing spends durability on whichever instance is equipped; hitting
    /// 0 auto-unequips (the slot clears) but the instance survives in
    /// inventory as a repairable, durability-0 husk — not deleted.
    #[tokio::test]
    async fn wear_equipped_tool_clamps_at_zero_and_auto_unequips_on_break() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "pickaxe", 1).await.unwrap();
        let instance_id = db.inventory_for_character(&cid).await.unwrap()[0].id.clone();
        db.equip_instance(&cid, &instance_id).await.unwrap();

        let fresh = crate::world::tool_max_durability("pickaxe").unwrap();
        let outcome = db.wear_equipped_tool(&cid, "tool", 1).await.unwrap().unwrap();
        assert_eq!((outcome.remaining, outcome.broke), (fresh - 1, false));
        assert_eq!(db.equipped(&cid, "tool").await.unwrap().as_deref(), Some("pickaxe"), "still equipped");

        // Drive it to the brink, then break it with a loss bigger than what's left.
        for _ in 0..(fresh - 2) {
            db.wear_equipped_tool(&cid, "tool", 1).await.unwrap();
        }
        let broke = db.wear_equipped_tool(&cid, "tool", 5).await.unwrap().unwrap();
        assert_eq!((broke.remaining, broke.broke), (0, true), "clamped at 0, not negative");
        assert_eq!(db.equipped(&cid, "tool").await.unwrap(), None, "auto-unequipped");

        let items = db.inventory_for_character(&cid).await.unwrap();
        let husk = items.iter().find(|i| i.id == instance_id).expect("the broken instance must still exist");
        assert_eq!(husk.durability, Some(0), "a repairable husk, not deleted");

        // Nothing equipped: wearing it down again is a no-op, not an error.
        assert_eq!(db.wear_equipped_tool(&cid, "tool", 1).await.unwrap(), None);
    }

    /// Repair cost scales with missing durability (world::repair_cost's
    /// formula) and is actually consumed; repairing something not missing
    /// any durability, or that can't be afforded, does nothing.
    #[tokio::test]
    async fn repair_instance_costs_scale_and_restore_to_full() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        db.add_to_inventory(&cid, "pickaxe", 1).await.unwrap();
        let instance_id = db.inventory_for_character(&cid).await.unwrap()[0].id.clone();

        // Fully healthy: nothing to repair.
        assert_eq!(db.repair_instance(&cid, &instance_id).await.unwrap(), None);

        // Wear it two thirds of the way down. Derived from the registry rather
        // than hardcoded, so a balance pass (#129) retunes the numbers without
        // silently invalidating what this test is checking.
        let max = crate::world::tool_max_durability("pickaxe").unwrap();
        let missing = max * 2 / 3;
        db.equip_instance(&cid, &instance_id).await.unwrap();
        for _ in 0..missing {
            db.wear_equipped_tool(&cid, "tool", 1).await.unwrap();
        }
        let want: Vec<(String, i64)> = crate::world::repair_cost("pickaxe", missing, max)
            .unwrap()
            .into_iter()
            .map(|(i, q)| (i.to_string(), q))
            .collect();

        // Can't afford it yet.
        assert_eq!(db.repair_instance(&cid, &instance_id).await.unwrap(), None, "no ingredients at all");

        for (item, qty) in &want {
            db.add_to_inventory(&cid, item, *qty).await.unwrap();
        }
        let outcome = db.repair_instance(&cid, &instance_id).await.unwrap().expect("affordable now");
        assert_eq!(outcome.item_id, "pickaxe");
        assert_eq!(
            outcome.cost.into_iter().collect::<std::collections::BTreeMap<_, _>>(),
            want.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
        );
        assert_eq!(db.inventory_qty(&cid, "wood").await.unwrap(), 0, "ingredients consumed");
        assert_eq!(db.inventory_qty(&cid, "stone").await.unwrap(), 0);
        let repaired = db.inventory_for_character(&cid).await.unwrap()
            .into_iter().find(|i| i.id == instance_id).unwrap();
        assert_eq!(repaired.durability, Some(max), "restored to full");

        // A stranger can't repair someone else's instance.
        let stranger = a_character(&db).await;
        db.wear_equipped_tool(&cid, "tool", 10).await.unwrap();
        assert_eq!(db.repair_instance(&stranger, &instance_id).await.unwrap(), None);
    }
    // --- Stations, fuel and timed jobs (#167) -------------------------------

    use crate::crafting_config::{Ingredient, StationRecipe};

    fn a_smelt_recipe() -> StationRecipe {
        StationRecipe {
            display_name: "Smelt Iron Ingot".into(),
            tags: vec!["smelting".into()],
            skill: "smelting".into(),
            required_level: 1,
            inputs: vec![Ingredient { item: "iron_ore".into(), qty: 2 }],
            output_item: "iron_ingot".into(),
            output_qty: 1,
            fuel_units: 2,
            duration_ms: 12_000,
            xp: 10,
        }
    }

    async fn carrying(db: &Db, cid: &str, item: &str, qty: i64) {
        db.add_to_inventory(cid, item, qty).await.unwrap();
    }

    async fn held(db: &Db, cid: &str, item: &str) -> i64 {
        db.inventory_for_character(cid)
            .await
            .unwrap()
            .iter()
            .filter(|i| i.item_id == item)
            .map(|i| i.qty)
            .sum()
    }

    /// The core custody claim: inputs and fuel leave at START, not at collect.
    /// If they left at collect, a job would be free to start and the whole
    /// escrow model would be decorative.
    #[tokio::test]
    async fn a_job_consumes_its_inputs_and_fuel_at_start() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        carrying(&db, &cid, "iron_ore", 5).await;
        db.load_station_fuel("f1", &cid, "charcoal", 0, 2, 100).await.unwrap();
        // Load fuel properly: the player needs the charcoal first.
        carrying(&db, &cid, "charcoal", 3).await;
        let total = db.load_station_fuel("f1", &cid, "charcoal", 3, 2, 100).await.unwrap();
        assert_eq!(total, Some(6), "3 charcoal at 2 units each");
        assert_eq!(held(&db, &cid, "charcoal").await, 0, "the charcoal went into the fire");

        let r = a_smelt_recipe();
        let job = db
            .start_station_job("f1", &cid, 0, "iron_ingot", &r, 2, 12_000, 1_000)
            .await
            .unwrap()
            .expect("should start");

        assert_eq!(held(&db, &cid, "iron_ore").await, 3, "2 ore escrowed at start");
        assert_eq!(db.station_fuel("f1").await.unwrap(), 4, "2 fuel units reserved at start");
        assert_eq!(job.inputs, vec![("iron_ore".to_string(), 2)]);
        assert_eq!(job.ready_at, 1_012, "12s job started at t=1000");
        assert_eq!(held(&db, &cid, "iron_ingot").await, 0, "nothing produced yet");
    }

    /// A restart between start and collect must neither duplicate the materials
    /// nor void them. The job row IS the durable custody, so reopening the same
    /// database is exactly the test.
    #[tokio::test]
    async fn escrow_survives_a_restart_between_start_and_collect() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        carrying(&db, &cid, "iron_ore", 4).await;
        carrying(&db, &cid, "charcoal", 1).await;
        db.load_station_fuel("f1", &cid, "charcoal", 1, 2, 100).await.unwrap();
        let r = a_smelt_recipe();
        let job = db
            .start_station_job("f1", &cid, 0, "iron_ingot", &r, 0, 12_000, 1_000)
            .await
            .unwrap()
            .unwrap();

        // "Restart": a second Db over the same file, as the gateway would open.
        let db2 = Db::connect(&_t.url).await.unwrap();
        let jobs = db2.station_jobs("f1", &cid).await.unwrap();
        assert_eq!(jobs.len(), 1, "the job outlived the process");
        assert_eq!(jobs[0].inputs, vec![("iron_ore".to_string(), 2)], "escrow intact");
        assert_eq!(held(&db2, &cid, "iron_ore").await, 2, "not silently refunded either");

        let ripe = db2.ripen_station_jobs(1_020).await.unwrap();
        assert_eq!(ripe.len(), 1);
        let got = db2.collect_station_job(&job.id, &cid, 0, 1_020).await.unwrap().unwrap();
        assert_eq!(got.payout, vec![("iron_ingot".to_string(), 1)]);
        assert_eq!(held(&db2, &cid, "iron_ingot").await, 1, "exactly one, not two");
    }

    /// Per-player slots are the whole point of a public station: one player
    /// filling theirs must not touch anyone else's.
    #[tokio::test]
    async fn one_players_full_slots_do_not_block_another() {
        let (db, _t) = TempDb::open().await;
        let (a, b) = (a_character(&db).await, a_character(&db).await);
        for c in [&a, &b] {
            carrying(&db, c, "iron_ore", 10).await;
            carrying(&db, c, "charcoal", 5).await;
            db.load_station_fuel("f1", c, "charcoal", 5, 2, 100).await.unwrap();
        }
        let r = a_smelt_recipe();
        // A fills both their slots.
        for slot in 0..2 {
            db.start_station_job("f1", &a, slot, "iron_ingot", &r, 0, 12_000, 1_000)
                .await
                .unwrap()
                .expect("A's own slots are free");
        }
        assert!(
            matches!(
                db.start_station_job("f1", &a, 0, "iron_ingot", &r, 0, 12_000, 1_000).await.unwrap(),
                Err(StartJobError::SlotBusy)
            ),
            "A's slot 0 is taken"
        );
        // B is unaffected — same station, same slot number, different player.
        db.start_station_job("f1", &b, 0, "iron_ingot", &r, 0, 12_000, 1_000)
            .await
            .unwrap()
            .expect("B's slot 0 is their own");
        assert_eq!(db.station_jobs("f1", &a).await.unwrap().len(), 2);
        assert_eq!(db.station_jobs("f1", &b).await.unwrap().len(), 1);
    }

    /// Every refusal consumes nothing. A fee charged for a job that then fails
    /// validation is silent theft, and the ordering inside the transaction is
    /// what prevents it — so each refusal is checked for side effects, not just
    /// for the error.
    #[tokio::test]
    async fn every_refusal_leaves_the_player_exactly_as_they_were() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        let r = a_smelt_recipe();
        let gold0 = db.character_gold(&cid).await.unwrap();

        // No ore, no fuel.
        let e = db.start_station_job("f1", &cid, 0, "iron_ingot", &r, 2, 12_000, 1).await.unwrap();
        assert!(matches!(e, Err(StartJobError::MissingInput { .. })), "{e:?}");

        // Ore but still no fuel.
        carrying(&db, &cid, "iron_ore", 2).await;
        let e = db.start_station_job("f1", &cid, 0, "iron_ingot", &r, 2, 12_000, 1).await.unwrap();
        assert!(matches!(e, Err(StartJobError::NotEnoughFuel { need: 2, have: 0 })), "{e:?}");

        // Fuel, but not enough gold for the fee.
        carrying(&db, &cid, "charcoal", 1).await;
        db.load_station_fuel("f1", &cid, "charcoal", 1, 2, 100).await.unwrap();
        let e = db
            .start_station_job("f1", &cid, 0, "iron_ingot", &r, gold0 + 1, 12_000, 1)
            .await
            .unwrap();
        assert!(matches!(e, Err(StartJobError::NotEnoughGold { .. })), "{e:?}");

        // Nothing was taken by any of the three.
        assert_eq!(held(&db, &cid, "iron_ore").await, 2, "ore untouched");
        assert_eq!(db.station_fuel("f1").await.unwrap(), 2, "fuel untouched");
        assert_eq!(db.character_gold(&cid).await.unwrap(), gold0, "no fee charged");
        assert!(db.station_jobs("f1", &cid).await.unwrap().is_empty(), "no job row");
    }

    /// The station fee is a SINK: it leaves the world through the ledger, and
    /// the supply identity #154 established still holds afterwards.
    #[tokio::test]
    async fn the_station_fee_is_burned_and_the_supply_still_balances() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        carrying(&db, &cid, "iron_ore", 2).await;
        carrying(&db, &cid, "charcoal", 1).await;
        db.load_station_fuel("f1", &cid, "charcoal", 1, 2, 100).await.unwrap();
        let gold0 = db.character_gold(&cid).await.unwrap();

        db.start_station_job("f1", &cid, 0, "iron_ingot", &a_smelt_recipe(), 2, 12_000, 1)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(db.character_gold(&cid).await.unwrap(), gold0 - 2);
        let by_reason = db.gold_by_reason().await.unwrap();
        assert_eq!(
            by_reason.iter().find(|(r, _)| r == "station_fee").map(|(_, a)| *a),
            Some(-2),
            "the burn is on the ledger under its own reason: {by_reason:?}"
        );
        assert_eq!(db.gold_supply_gap().await.unwrap(), 0, "supply identity holds");
    }

    /// A recipe vanishing from `crafting.toml` between restarts fails the job
    /// and refunds exactly what it escrowed — never a panic, and never the
    /// recipe's current cost, which may itself have changed.
    #[tokio::test]
    async fn a_vanished_recipe_fails_the_job_and_refunds_the_escrow() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        carrying(&db, &cid, "iron_ore", 2).await;
        carrying(&db, &cid, "charcoal", 1).await;
        db.load_station_fuel("f1", &cid, "charcoal", 1, 2, 100).await.unwrap();
        let job = db
            .start_station_job("f1", &cid, 0, "iron_ingot", &a_smelt_recipe(), 0, 12_000, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(held(&db, &cid, "iron_ore").await, 0);
        assert_eq!(db.station_fuel("f1").await.unwrap(), 0);

        assert!(db.fail_station_job(&job.id, "recipe_gone").await.unwrap());
        let got = db.collect_station_job(&job.id, &cid, 0, 200).await.unwrap().unwrap();

        assert!(got.failed);
        assert_eq!(got.fail_reason.as_deref(), Some("recipe_gone"));
        assert_eq!(got.payout, vec![("iron_ore".to_string(), 2)], "the ore comes back");
        assert_eq!(got.xp, 0, "a failed job teaches nothing");
        assert_eq!(held(&db, &cid, "iron_ore").await, 2);
        assert_eq!(db.station_fuel("f1").await.unwrap(), 2, "the fuel goes back to the fire");
        assert_eq!(held(&db, &cid, "iron_ingot").await, 0, "and no ingot was made");
    }

    /// Collecting with a full pack holds the output in the slot and says so.
    /// Destroying it would be the worst outcome — the player did the work and
    /// paid the fee.
    #[tokio::test]
    async fn a_full_pack_holds_the_output_rather_than_destroying_it() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        carrying(&db, &cid, "iron_ore", 2).await;
        carrying(&db, &cid, "charcoal", 1).await;
        db.load_station_fuel("f1", &cid, "charcoal", 1, 2, 100).await.unwrap();
        let job = db
            .start_station_job("f1", &cid, 0, "iron_ingot", &a_smelt_recipe(), 0, 12_000, 1)
            .await
            .unwrap()
            .unwrap();
        db.ripen_station_jobs(100).await.unwrap();

        // Fill the pack to the brim while the job runs.
        carrying(&db, &cid, "stone", MAX_CARRY).await;
        let e = db.collect_station_job(&job.id, &cid, 0, 200).await.unwrap();
        assert!(matches!(e, Err(CollectError::NoRoom { .. })), "{e:?}");
        assert_eq!(
            db.station_jobs("f1", &cid).await.unwrap().len(),
            1,
            "the job is still there holding the ingot"
        );

        // Make room; now it collects.
        db.remove_from_inventory(&cid, "stone", 5).await.unwrap();
        let got = db.collect_station_job(&job.id, &cid, 0, 200).await.unwrap().unwrap();
        assert_eq!(got.payout, vec![("iron_ingot".to_string(), 1)]);
        assert!(db.station_jobs("f1", &cid).await.unwrap().is_empty(), "slot freed");
    }

    /// Collecting the same job twice pays out once.
    ///
    /// Note what this does and does NOT prove. Sequentially, the second collect
    /// finds no row at all, so it never reaches the compare-and-clear in the
    /// DELETE — verified by sabotage: removing the state guard leaves this test
    /// green. The guard is still correct and still wanted (it is #142's pattern,
    /// and the pool being one connection today is a tuning decision rather than
    /// a contract), but the honest claim here is idempotence, not a won race.
    /// `concurrent_collects_of_one_job_produce_one_ingot` covers the overlap.
    #[tokio::test]
    async fn collecting_the_same_job_twice_pays_out_once() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        carrying(&db, &cid, "iron_ore", 2).await;
        carrying(&db, &cid, "charcoal", 1).await;
        db.load_station_fuel("f1", &cid, "charcoal", 1, 2, 100).await.unwrap();
        let job = db
            .start_station_job("f1", &cid, 0, "iron_ingot", &a_smelt_recipe(), 0, 12_000, 1)
            .await
            .unwrap()
            .unwrap();
        db.ripen_station_jobs(100).await.unwrap();

        let first = db.collect_station_job(&job.id, &cid, 0, 200).await.unwrap();
        let second = db.collect_station_job(&job.id, &cid, 0, 200).await.unwrap();
        assert!(first.is_ok(), "{first:?}");
        assert!(matches!(second, Err(CollectError::NoSuchJob)), "{second:?}");
        assert_eq!(held(&db, &cid, "iron_ingot").await, 1, "exactly one ingot");
    }

    /// Two collects genuinely in flight at once still produce one ingot.
    ///
    /// The single-connection pool serialises the transactions, so this is not
    /// the hostile interleaving the compare-and-clear was written for — but it
    /// does exercise both calls overlapping in the executor, which the
    /// sequential test above cannot.
    #[tokio::test]
    async fn concurrent_collects_of_one_job_produce_one_ingot() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        carrying(&db, &cid, "iron_ore", 2).await;
        carrying(&db, &cid, "charcoal", 1).await;
        db.load_station_fuel("f1", &cid, "charcoal", 1, 2, 100).await.unwrap();
        let job = db
            .start_station_job("f1", &cid, 0, "iron_ingot", &a_smelt_recipe(), 0, 12_000, 1)
            .await
            .unwrap()
            .unwrap();
        db.ripen_station_jobs(100).await.unwrap();

        let db = std::sync::Arc::new(db);
        let (d1, d2) = (db.clone(), db.clone());
        let (j1, j2) = (job.id.clone(), job.id.clone());
        let (c1, c2) = (cid.clone(), cid.clone());
        let (a, b) = tokio::join!(
            tokio::spawn(async move { d1.collect_station_job(&j1, &c1, 0, 200).await.unwrap() }),
            tokio::spawn(async move { d2.collect_station_job(&j2, &c2, 0, 200).await.unwrap() }),
        );
        let wins = [a.unwrap().is_ok(), b.unwrap().is_ok()];
        assert_eq!(wins.iter().filter(|w| **w).count(), 1, "exactly one collect wins");
        assert_eq!(held(&db, &cid, "iron_ingot").await, 1, "and exactly one ingot exists");
    }

    /// A job can't be collected early, and ripening is what makes it collectable
    /// — not the player asking nicely.
    #[tokio::test]
    async fn a_running_job_refuses_to_be_collected_early() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        carrying(&db, &cid, "iron_ore", 2).await;
        carrying(&db, &cid, "charcoal", 1).await;
        db.load_station_fuel("f1", &cid, "charcoal", 1, 2, 100).await.unwrap();
        let job = db
            .start_station_job("f1", &cid, 0, "iron_ingot", &a_smelt_recipe(), 0, 12_000, 1_000)
            .await
            .unwrap()
            .unwrap();

        let e = db.collect_station_job(&job.id, &cid, 0, 1_005).await.unwrap();
        assert!(matches!(e, Err(CollectError::NotReady { ready_at: 1_012 })), "{e:?}");

        // Not yet due: ripening does nothing.
        assert!(db.ripen_station_jobs(1_005).await.unwrap().is_empty());
        assert_eq!(db.ripen_station_jobs(1_012).await.unwrap().len(), 1, "due on the second");
        assert!(db.collect_station_job(&job.id, &cid, 0, 1_012).await.unwrap().is_ok());
    }

    /// Fuel is SHARED: one player can burn what another loaded. That is the
    /// documented choice for a public furnace, and it deserves a test precisely
    /// because it would otherwise look like a bug.
    #[tokio::test]
    async fn fuel_is_shared_across_players_at_a_public_station() {
        let (db, _t) = TempDb::open().await;
        let (a, b) = (a_character(&db).await, a_character(&db).await);
        carrying(&db, &a, "charcoal", 2).await;
        db.load_station_fuel("f1", &a, "charcoal", 2, 2, 100).await.unwrap();
        assert_eq!(db.station_fuel("f1").await.unwrap(), 4);

        carrying(&db, &b, "iron_ore", 2).await;
        db.start_station_job("f1", &b, 0, "iron_ingot", &a_smelt_recipe(), 0, 12_000, 1)
            .await
            .unwrap()
            .expect("B burns the fire A lit");
        assert_eq!(db.station_fuel("f1").await.unwrap(), 2);
    }

    /// A levelled smelter's bonus output is applied at collect, and it is a
    /// whole extra unit rather than a fraction — an ingot is not divisible.
    #[tokio::test]
    async fn a_bonus_output_adds_a_whole_extra_unit() {
        let (db, _t) = TempDb::open().await;
        let cid = a_character(&db).await;
        carrying(&db, &cid, "iron_ore", 2).await;
        carrying(&db, &cid, "charcoal", 1).await;
        db.load_station_fuel("f1", &cid, "charcoal", 1, 2, 100).await.unwrap();
        let job = db
            .start_station_job("f1", &cid, 0, "iron_ingot", &a_smelt_recipe(), 0, 12_000, 1)
            .await
            .unwrap()
            .unwrap();
        db.ripen_station_jobs(100).await.unwrap();

        let got = db.collect_station_job(&job.id, &cid, 1, 200).await.unwrap().unwrap();
        assert_eq!(got.payout, vec![("iron_ingot".to_string(), 2)], "1 base + 1 bonus");
        assert_eq!(held(&db, &cid, "iron_ingot").await, 2);
    }

}
