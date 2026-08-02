//! `market.toml` — boot-loaded market tuning (market phase 2 epic #151, issue
//! #152), replacing the compile-time consts that #136 shipped with.
//!
//! **Why this exists.** #129 (balance pass) is open and waiting on playtest
//! data. While every market number was a `const`, trying a different sale tax
//! meant a rebuild and a restart — exactly the friction that stops a balance
//! pass from ever happening. `terrain.toml` already establishes this pattern in
//! the repo (see `terrain-bake::config`), so this follows a convention rather
//! than inventing one.
//!
//! **Overrides key on DISTRICT, not market id.** A market's id is a
//! `Uuid::new_v4()` minted by `insert_build_order`, so it is neither authorable
//! in a file nor stable across a DB reseed — a fee rate pinned to one would
//! silently revert to defaults the next time the world was reseeded, which is
//! the worst possible failure mode for an economy tunable. Districts are
//! authored in `world::capital()` and stable forever, and a district holds at
//! most one market, so `[districts.<id>]` is both writable and durable.
//!
//! **A missing file is not an error.** Every default here equals the const it
//! replaced (asserted by `defaults_match_the_shipped_constants`), so a fresh
//! clone with no `market.toml` behaves exactly like the pre-config server.
//! Config is an override mechanism, not a required input.
//!
//! **A malformed file refuses to boot.** `deny_unknown_fields` turns a typo
//! into a named parse error, and [`MarketConfig::validate`] rejects nonsense
//! values with the key in the message. Silently falling back to defaults on a
//! bad key is how a server ends up running numbers nobody chose — and an
//! economy misconfigured for a week can't be fixed by editing the file
//! afterwards, because the trades already happened.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::world::OrderReject;

/// Fully-resolved tuning for **one** market: defaults with that district's
/// overrides already layered on. Everything the market subsystem used to read
/// from a `const` lives here, so there is exactly one place a value can come
/// from and no path that reads a stale compile-time copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketConfig {
    /// How close a player must stand to trade. Enforced server-side by
    /// `market_at`; the client mirrors it only to decide when to show the panel.
    pub range: i32,
    /// Warehouse slots one player gets at one market.
    pub warehouse_slots: i64,
    /// Order prices are whole gold and must be a multiple of this. A tick keeps
    /// the book's price levels countable instead of letting a thousand orders
    /// sit one copper apart.
    pub price_tick_gold: i64,
    /// Bounds on a single order's size — below the floor is noise, above the cap
    /// is a mis-click or an attempt to wedge the book.
    pub min_order_qty: i64,
    pub max_order_qty: i64,
    /// Durations (hours) a client may ask for before the sweep releases a
    /// resting order's escrow. A resting order holds goods or gold hostage, so
    /// "forever" isn't on the list.
    pub order_durations_hours: Vec<i64>,
    pub default_order_hours: i64,
    /// Resting orders one player may hold at one market. Caps the cost of
    /// rebuilding a book on boot and stops one player papering it with hundreds
    /// of tiny orders.
    pub max_open_orders: i64,
    /// Sliding-window rate limit on market commands, per player.
    pub commands_per_minute: i64,
    /// Cap on one listing-board page.
    pub listing_page_limit: i64,
    /// Listing fee: `max(min_gold, ceil(num/den * notional))`, charged to both
    /// sides at placement and never refunded.
    pub listing_fee_min_gold: i64,
    pub listing_fee_num: i64,
    pub listing_fee_den: i64,
    /// Sale tax on a fill's value, charged to the seller out of their proceeds.
    pub sale_tax_num: i64,
    pub sale_tax_den: i64,
    /// Candle resolution the rollup materialises.
    pub candle_interval_secs: i64,
    /// How long candles are kept. The ledger they derive from is never pruned.
    pub history_retain_days: i64,
    /// Daily holding cost per OCCUPIED WAREHOUSE SLOT (#155). **0 by default,
    /// so storage is free unless an operator turns it on.**
    ///
    /// Per slot rather than per item because the slot is the scarce resource
    /// (`warehouse_slots`); charging per item would punish stacking a commodity,
    /// which is the opposite of the intent. Charged on locked stock as well as
    /// available, or "list it at an absurd price" would be a free-storage
    /// loophole.
    pub storage_fee_per_slot_per_day: i64,
    /// How many days of storage fees may accumulate as unpaid debt before the
    /// meter stops (#155). Bounded on purpose: a player returning to a bill they
    /// can never clear has effectively lost the goods, which is the outcome the
    /// whole arrears design exists to avoid.
    pub storage_arrears_cap_days: i64,
    /// Standing price bounds per commodity (#154), empty by default — the
    /// provisioner is opt-in, so a server that configures nothing gets no NPC
    /// orders and no second gold faucet.
    pub provisioner: BTreeMap<String, ProvisionerBounds>,
}

impl Default for MarketConfig {
    /// Exactly the values #136-#143 shipped as consts. Changing one of these
    /// changes the behaviour of a server with no `market.toml`, which is most of
    /// them — treat it as a balance change, not a refactor.
    fn default() -> Self {
        MarketConfig {
            range: 60,
            warehouse_slots: 60,
            price_tick_gold: 1,
            min_order_qty: 1,
            max_order_qty: 10_000,
            order_durations_hours: vec![12, 24, 72, 168],
            default_order_hours: 24,
            max_open_orders: 40,
            commands_per_minute: 60,
            listing_page_limit: 100,
            listing_fee_min_gold: 1,
            listing_fee_num: 1,
            listing_fee_den: 100,
            sale_tax_num: 3,
            sale_tax_den: 100,
            candle_interval_secs: 3600,
            history_retain_days: 30,
            // Off. The mechanism ships; the policy does not (#155).
            storage_fee_per_slot_per_day: 0,
            storage_arrears_cap_days: 7,
            provisioner: BTreeMap::new(),
        }
    }
}

/// Integer ceiling division, for fees that must round toward the house.
fn div_ceil(n: i64, d: i64) -> i64 {
    if n <= 0 {
        0
    } else {
        (n + d - 1) / d
    }
}

impl MarketConfig {
    /// The fee to place an order of `notional` (= `unit_price * qty`) gold,
    /// charged to **both** sides at placement and **never refunded** — that's
    /// what makes posting an order you don't mean to honour cost something.
    ///
    /// Rounds UP and is never zero on a nonzero notional. Rounding in the
    /// house's favour is not greed, it's the anti-exploit: a fee that rounded to
    /// zero on small trades would make a free lane, and splitting one big order
    /// into a hundred tiny ones would dodge the sink entirely.
    pub fn listing_fee(&self, notional: i64) -> i64 {
        if notional <= 0 {
            return 0;
        }
        div_ceil(notional * self.listing_fee_num, self.listing_fee_den)
            .max(self.listing_fee_min_gold)
    }

    /// Tax on one fill's value (`execution_price * qty`), charged to the seller
    /// out of their proceeds. Rounds up, never zero on a nonzero fill.
    pub fn sale_tax(&self, value: i64) -> i64 {
        if value <= 0 {
            return 0;
        }
        div_ceil(value * self.sale_tax_num, self.sale_tax_den).max(1)
    }

    /// Clamp a requested order duration to one this market actually offers,
    /// falling back to the default for anything unrecognised.
    pub fn order_duration_hours(&self, requested: i64) -> i64 {
        if self.order_durations_hours.contains(&requested) {
            requested
        } else {
            self.default_order_hours
        }
    }

    /// Shared validation for every order placement (#139): the commodity gate,
    /// the price tick, and the size bounds. One place, so a sell and a buy can
    /// never disagree about what's tradable.
    pub fn validate_order(
        &self,
        item_id: &str,
        unit_price: i64,
        qty: i64,
    ) -> Result<(), OrderReject> {
        if !crate::world::is_commodity(item_id) {
            return Err(OrderReject::NotACommodity);
        }
        if unit_price <= 0 || unit_price % self.price_tick_gold != 0 {
            return Err(OrderReject::BadPrice);
        }
        if !(self.min_order_qty..=self.max_order_qty).contains(&qty) {
            return Err(OrderReject::BadQty);
        }
        Ok(())
    }

    /// The rules a client needs to preview a cost before committing. Sent in
    /// `market.opened` (#152) because the client can no longer hardcode them:
    /// the moment these became data, a mirrored copy in `Protocol.gd` was a
    /// *lie*, and a quiet one — the panel would preview a 3% tax while the
    /// server charged whatever the file said, and the player would find out by
    /// being short-changed.
    pub fn wire_rules(&self) -> serde_json::Value {
        serde_json::json!({
            "range": self.range,
            "warehouse_slots": self.warehouse_slots,
            "price_tick_gold": self.price_tick_gold,
            "min_order_qty": self.min_order_qty,
            "max_order_qty": self.max_order_qty,
            "order_durations_hours": self.order_durations_hours,
            "default_order_hours": self.default_order_hours,
            "max_open_orders": self.max_open_orders,
            "listing_fee_min_gold": self.listing_fee_min_gold,
            "listing_fee_num": self.listing_fee_num,
            "listing_fee_den": self.listing_fee_den,
            "sale_tax_num": self.sale_tax_num,
            "sale_tax_den": self.sale_tax_den,
            "candle_interval_secs": self.candle_interval_secs,
            // The provisioner's standing bounds (#154), as
            // `{item: {floor, ceiling}}`. Sent so the panel can SAY what the
            // band is instead of leaving a player to infer it from a
            // suspiciously large resting order — the bounds are the most
            // useful thing a newcomer can know about a commodity, and they are
            // public information by construction (the orders are right there in
            // the book).
            //
            // Sizes are deliberately omitted: how much the NPC will buy is a
            // depth question the book already answers, and quoting a number
            // that goes stale between refreshes would be worse than silence.
            "provisioner": self
                .provisioner
                .iter()
                .map(|(k, b)| {
                    (k.clone(), serde_json::json!({"floor": b.floor, "ceiling": b.ceiling}))
                })
                .collect::<serde_json::Map<String, serde_json::Value>>(),
        })
    }

    /// Reject values that would break the market rather than merely tune it.
    ///
    /// Checked at LOAD, not at use. These are the same shapes `validate_order`
    /// already refuses at runtime, and catching them at boot is strictly better
    /// than discovering them on a player's order — a zero denominator is a
    /// divide-by-zero in the fee path, and `min > max` is a market where no
    /// order is placeable at all.
    ///
    /// `where` names the section so the operator knows which table to fix.
    fn validate(&self, whence: &str) -> Result<(), ConfigError> {
        let bad = |key: &str, why: &str| {
            Err(ConfigError::Invalid {
                whence: whence.to_string(),
                key: key.to_string(),
                why: why.to_string(),
            })
        };
        if self.range <= 0 {
            return bad("range", "must be positive");
        }
        if self.warehouse_slots <= 0 {
            return bad("warehouse_slots", "must be positive");
        }
        if self.price_tick_gold <= 0 {
            return bad("price_tick_gold", "must be positive (it is a divisor)");
        }
        if self.min_order_qty <= 0 {
            return bad("min_order_qty", "must be positive");
        }
        if self.min_order_qty > self.max_order_qty {
            return bad(
                "min_order_qty",
                "must not exceed max_order_qty, or no order is placeable",
            );
        }
        if self.order_durations_hours.is_empty() {
            return bad("order_durations_hours", "must offer at least one duration");
        }
        if self.order_durations_hours.iter().any(|h| *h <= 0) {
            return bad("order_durations_hours", "durations must be positive");
        }
        if !self.order_durations_hours.contains(&self.default_order_hours) {
            return bad(
                "default_order_hours",
                "must be one of order_durations_hours, since unrecognised requests fall back to it",
            );
        }
        if self.max_open_orders <= 0 {
            return bad("max_open_orders", "must be positive");
        }
        if self.commands_per_minute <= 0 {
            return bad("commands_per_minute", "must be positive");
        }
        if self.listing_page_limit <= 0 {
            return bad("listing_page_limit", "must be positive");
        }
        if self.listing_fee_min_gold < 0 {
            return bad("listing_fee_min_gold", "must not be negative");
        }
        if self.listing_fee_num < 0 {
            return bad("listing_fee_num", "must not be negative");
        }
        if self.listing_fee_den <= 0 {
            return bad("listing_fee_den", "must be positive (it is a divisor)");
        }
        if self.sale_tax_num < 0 {
            return bad("sale_tax_num", "must not be negative");
        }
        if self.sale_tax_den <= 0 {
            return bad("sale_tax_den", "must be positive (it is a divisor)");
        }
        if self.candle_interval_secs <= 0 {
            return bad("candle_interval_secs", "must be positive (it is a divisor)");
        }
        if self.history_retain_days <= 0 {
            return bad("history_retain_days", "must be positive");
        }
        if self.storage_fee_per_slot_per_day < 0 {
            return bad("storage_fee_per_slot_per_day", "must not be negative — 0 disables it");
        }
        if self.storage_arrears_cap_days <= 0 {
            return bad(
                "storage_arrears_cap_days",
                "must be positive, or unpaid storage debt would be unbounded and                  a returning player could never clear it",
            );
        }
        for (item, bounds) in &self.provisioner {
            // Only real commodities: a typo'd item id would otherwise sit in the
            // file looking like a price floor while backing nothing, and uniques
            // are sold on the listing board and have no book to stand orders in.
            if !crate::world::is_commodity(item) {
                return Err(ConfigError::Invalid {
                    whence: whence.to_string(),
                    key: format!("provisioner.{item}"),
                    why: "is not a tradable commodity — check the spelling, and note that                           unique items are sold on the listing board and have no order book"
                        .to_string(),
                });
            }
            bounds.validate(whence, item)?;
        }
        Ok(())
    }
}

/// The NPC provisioner's standing bounds for one commodity (#154).
///
/// **Opt-in.** A commodity with no entry gets no provisioner orders at all, so
/// adding an item to the registry never silently creates a gold faucet for it.
///
/// The floor and ceiling are deliberately far apart. The provisioner should be
/// the worst counterparty in the market and still better than nobody: it exists
/// so a fresh server's book isn't dead content and so a commodity can't be
/// cornered without limit, not to be a place anyone chooses to trade. A test
/// asserts round-tripping goods through it LOSES money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionerBounds {
    /// What it will always pay. The bid is refreshed back to `bid_qty` forever,
    /// minting the gold to do it — a floor with a finite budget stops being a
    /// floor exactly when it is needed, during a crash.
    pub floor: i64,
    /// What it will sell at, **from stock only**. Unbounded selling would be an
    /// infinite item faucet, and destroying scarcity is worse than an uncapped
    /// price.
    pub ceiling: i64,
    /// How much the standing bid is topped back up to on each refresh.
    pub bid_qty: i64,
    /// Stock the provisioner is seeded with at each market the first time it
    /// appears, so the ceiling exists on a brand-new server before it has
    /// bought anything. Everything after that comes from what it buys.
    #[serde(default)]
    pub seed_stock: i64,
}

impl ProvisionerBounds {
    fn validate(&self, whence: &str, item: &str) -> Result<(), ConfigError> {
        let bad = |key: &str, why: &str| {
            Err(ConfigError::Invalid {
                whence: format!("{whence}.provisioner.{item}"),
                key: key.to_string(),
                why: why.to_string(),
            })
        };
        if self.floor <= 0 {
            return bad("floor", "must be positive");
        }
        if self.ceiling <= 0 {
            return bad("ceiling", "must be positive");
        }
        // An inverted or touching spread would let a player buy from the
        // provisioner and immediately sell back to it at a profit — an infinite
        // gold fountain limited only by how fast they can click.
        if self.ceiling <= self.floor {
            return bad(
                "ceiling",
                "must be strictly above floor, or buying from the provisioner and selling \
                 straight back to it prints money",
            );
        }
        if self.bid_qty <= 0 {
            return bad("bid_qty", "must be positive");
        }
        if self.seed_stock < 0 {
            return bad("seed_stock", "must not be negative");
        }
        Ok(())
    }
}

/// One TOML table's worth of overrides: every field optional, so `[defaults]`
/// and `[districts.<id>]` share a shape and a district only states what it
/// changes.
///
/// `deny_unknown_fields` is load-bearing: a misspelled key must be a boot
/// failure, not a silently ignored line that leaves the operator believing they
/// changed a rate they didn't.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MarketPatch {
    pub range: Option<i32>,
    pub warehouse_slots: Option<i64>,
    pub price_tick_gold: Option<i64>,
    pub min_order_qty: Option<i64>,
    pub max_order_qty: Option<i64>,
    pub order_durations_hours: Option<Vec<i64>>,
    pub default_order_hours: Option<i64>,
    pub max_open_orders: Option<i64>,
    pub commands_per_minute: Option<i64>,
    pub listing_page_limit: Option<i64>,
    pub listing_fee_min_gold: Option<i64>,
    pub listing_fee_num: Option<i64>,
    pub listing_fee_den: Option<i64>,
    pub sale_tax_num: Option<i64>,
    pub sale_tax_den: Option<i64>,
    pub candle_interval_secs: Option<i64>,
    pub history_retain_days: Option<i64>,
    pub storage_fee_per_slot_per_day: Option<i64>,
    pub storage_arrears_cap_days: Option<i64>,
    /// Per-commodity provisioner bounds. Stated in a district table, this
    /// REPLACES the defaults' map wholesale rather than merging key by key: a
    /// market that names its own commodities means exactly those, and a
    /// half-inherited set of price bounds would be very hard to reason about.
    pub provisioner: Option<BTreeMap<String, ProvisionerBounds>>,
}

impl MarketPatch {
    /// Layer this patch over `base`, field by field.
    fn apply_to(&self, base: &MarketConfig) -> MarketConfig {
        MarketConfig {
            range: self.range.unwrap_or(base.range),
            warehouse_slots: self.warehouse_slots.unwrap_or(base.warehouse_slots),
            price_tick_gold: self.price_tick_gold.unwrap_or(base.price_tick_gold),
            min_order_qty: self.min_order_qty.unwrap_or(base.min_order_qty),
            max_order_qty: self.max_order_qty.unwrap_or(base.max_order_qty),
            order_durations_hours: self
                .order_durations_hours
                .clone()
                .unwrap_or_else(|| base.order_durations_hours.clone()),
            default_order_hours: self.default_order_hours.unwrap_or(base.default_order_hours),
            max_open_orders: self.max_open_orders.unwrap_or(base.max_open_orders),
            commands_per_minute: self.commands_per_minute.unwrap_or(base.commands_per_minute),
            listing_page_limit: self.listing_page_limit.unwrap_or(base.listing_page_limit),
            listing_fee_min_gold: self
                .listing_fee_min_gold
                .unwrap_or(base.listing_fee_min_gold),
            listing_fee_num: self.listing_fee_num.unwrap_or(base.listing_fee_num),
            listing_fee_den: self.listing_fee_den.unwrap_or(base.listing_fee_den),
            sale_tax_num: self.sale_tax_num.unwrap_or(base.sale_tax_num),
            sale_tax_den: self.sale_tax_den.unwrap_or(base.sale_tax_den),
            candle_interval_secs: self
                .candle_interval_secs
                .unwrap_or(base.candle_interval_secs),
            history_retain_days: self.history_retain_days.unwrap_or(base.history_retain_days),
            storage_fee_per_slot_per_day: self
                .storage_fee_per_slot_per_day
                .unwrap_or(base.storage_fee_per_slot_per_day),
            storage_arrears_cap_days: self
                .storage_arrears_cap_days
                .unwrap_or(base.storage_arrears_cap_days),
            provisioner: self
                .provisioner
                .clone()
                .unwrap_or_else(|| base.provisioner.clone()),
        }
    }
}

/// The parsed file: `[defaults]` plus optional `[districts.<id>]` tables.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketToml {
    #[serde(default)]
    defaults: MarketPatch,
    /// Keyed by the district ids authored in `world::capital()` — `civic`,
    /// `market`, `suburbs`, `craftworks`, `old_quarter`.
    #[serde(default)]
    districts: BTreeMap<String, MarketPatch>,
}

/// Every market's resolved tuning: the defaults, plus a per-district table for
/// the districts that override something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketConfigSet {
    defaults: MarketConfig,
    districts: BTreeMap<String, MarketConfig>,
}

impl Default for MarketConfigSet {
    fn default() -> Self {
        MarketConfigSet {
            defaults: MarketConfig::default(),
            districts: BTreeMap::new(),
        }
    }
}

impl MarketConfigSet {
    /// The tuning in force at a market in `district`. Falls back to the
    /// defaults, so a district with no table needs no entry.
    pub fn for_district(&self, district: &str) -> &MarketConfig {
        self.districts.get(district).unwrap_or(&self.defaults)
    }

    /// The defaults, for the background jobs that aren't standing at a market
    /// (candle rollup, expiry sweep) and for tests.
    pub fn defaults(&self) -> &MarketConfig {
        &self.defaults
    }

    /// Every district-specific config, for boot logging — an operator should be
    /// able to see what the server actually resolved, not just what they wrote.
    pub fn overrides(&self) -> impl Iterator<Item = (&String, &MarketConfig)> {
        self.districts.iter()
    }

    pub fn parse(toml_text: &str) -> Result<MarketConfigSet, ConfigError> {
        let raw: MarketToml = toml::from_str(toml_text).map_err(ConfigError::Toml)?;
        let defaults = raw.defaults.apply_to(&MarketConfig::default());
        defaults.validate("defaults")?;

        let known = crate::world::capital_district_ids();
        let mut districts = BTreeMap::new();
        for (id, patch) in &raw.districts {
            // A table for a district that doesn't exist is almost always a typo
            // ("civics", "old-quarter"), and it would otherwise sit in the file
            // looking effective forever while changing nothing.
            if !known.contains(&id.as_str()) {
                return Err(ConfigError::UnknownDistrict {
                    id: id.clone(),
                    known: known.join(", "),
                });
            }
            // The candle rollup and the retention prune are ONE background job
            // that sweeps every market in a single pass, so there is exactly one
            // interval and one retention window the server can honour. A
            // district setting these would parse, validate, and then do nothing
            // — the precise failure this file's strictness exists to prevent —
            // so it's refused with the reason rather than accepted as a lie.
            if patch.candle_interval_secs.is_some() {
                return Err(ConfigError::NotPerDistrict {
                    district: id.clone(),
                    key: "candle_interval_secs".to_string(),
                });
            }
            if patch.history_retain_days.is_some() {
                return Err(ConfigError::NotPerDistrict {
                    district: id.clone(),
                    key: "history_retain_days".to_string(),
                });
            }
            let resolved = patch.apply_to(&defaults);
            resolved.validate(&format!("districts.{id}"))?;
            districts.insert(id.clone(), resolved);
        }
        Ok(MarketConfigSet { defaults, districts })
    }

    /// Load from `path`. **A missing file is not an error** — it resolves to the
    /// shipped defaults, so a fresh clone runs the same economy #136 shipped.
    /// Anything else (unreadable, malformed, out of range) fails, because those
    /// mean the operator wrote something they expected to take effect.
    pub fn load(path: &Path) -> Result<MarketConfigSet, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => MarketConfigSet::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(MarketConfigSet::default())
            }
            Err(e) => Err(ConfigError::Io(e)),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Toml(toml::de::Error),
    Io(std::io::Error),
    /// A value parsed but would break the market. Carries the table and key so
    /// the operator is pointed at the line to fix.
    Invalid {
        whence: String,
        key: String,
        why: String,
    },
    UnknownDistrict {
        id: String,
        known: String,
    },
    /// A key that parses and validates but that the server can only honour
    /// globally. Refused rather than silently ignored.
    NotPerDistrict {
        district: String,
        key: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Toml(e) => write!(f, "market.toml parse error: {e}"),
            ConfigError::Io(e) => write!(f, "market.toml read error: {e}"),
            ConfigError::Invalid { whence, key, why } => {
                write!(f, "market.toml [{whence}]: `{key}` {why}")
            }
            ConfigError::UnknownDistrict { id, known } => write!(
                f,
                "market.toml [districts.{id}]: no such district — known districts are: {known}"
            ),
            ConfigError::NotPerDistrict { district, key } => write!(
                f,
                "market.toml [districts.{district}]: `{key}` can only be set in [defaults] —                  the candle rollup sweeps every market in one pass, so a per-district value                  would silently do nothing"
            ),
        }
    }
}
impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole promise of "config is an override mechanism, not a required
    /// input": a server with no `market.toml` must behave exactly like the
    /// pre-config one. These literals are the consts #136-#143 shipped, spelled
    /// out again on purpose — if someone edits a default, this test makes them
    /// notice they changed live behaviour rather than refactored.
    #[test]
    fn defaults_match_the_shipped_constants() {
        let c = MarketConfig::default();
        assert_eq!(c.range, 60);
        assert_eq!(c.warehouse_slots, 60);
        assert_eq!(c.price_tick_gold, 1);
        assert_eq!(c.min_order_qty, 1);
        assert_eq!(c.max_order_qty, 10_000);
        assert_eq!(c.order_durations_hours, vec![12, 24, 72, 168]);
        assert_eq!(c.default_order_hours, 24);
        assert_eq!(c.max_open_orders, 40);
        assert_eq!(c.commands_per_minute, 60);
        assert_eq!(c.listing_page_limit, 100);
        assert_eq!(c.listing_fee_min_gold, 1);
        assert_eq!((c.listing_fee_num, c.listing_fee_den), (1, 100));
        assert_eq!((c.sale_tax_num, c.sale_tax_den), (3, 100));
        assert_eq!(c.candle_interval_secs, 3600);
        assert_eq!(c.history_retain_days, 30);
    }

    #[test]
    fn a_missing_file_loads_the_defaults() {
        let set = MarketConfigSet::load(Path::new("no_such_market_config.toml")).unwrap();
        assert_eq!(set, MarketConfigSet::default());
        assert_eq!(set.for_district("civic"), &MarketConfig::default());
    }

    /// An empty file is the same as no file — worth pinning separately, because
    /// `[defaults]` being absent has to mean "take them all" rather than
    /// deserialising to zeroes.
    #[test]
    fn an_empty_file_loads_the_defaults() {
        let set = MarketConfigSet::parse("").unwrap();
        assert_eq!(set, MarketConfigSet::default());
    }

    #[test]
    fn defaults_can_be_overridden_wholesale() {
        let set = MarketConfigSet::parse(
            r#"
            [defaults]
            sale_tax_num = 5
            max_order_qty = 500
            "#,
        )
        .unwrap();
        let c = set.for_district("civic");
        assert_eq!(c.sale_tax_num, 5);
        assert_eq!(c.max_order_qty, 500);
        // Everything unstated still comes from the shipped defaults.
        assert_eq!(c.sale_tax_den, 100);
        assert_eq!(c.listing_fee_num, 1);
    }

    /// The epic's whole point: two markets that are economically distinct. A
    /// district's table must change that district and nothing else.
    #[test]
    fn a_district_override_touches_only_that_district() {
        let set = MarketConfigSet::parse(
            r#"
            [defaults]
            sale_tax_num = 3

            [districts.market]
            sale_tax_num = 1
            warehouse_slots = 120
            "#,
        )
        .unwrap();
        assert_eq!(set.for_district("market").sale_tax_num, 1);
        assert_eq!(set.for_district("market").warehouse_slots, 120);
        assert_eq!(set.for_district("civic").sale_tax_num, 3);
        assert_eq!(set.for_district("civic").warehouse_slots, 60);
        // And an unmentioned district gets the defaults, not the override.
        assert_eq!(set.for_district("suburbs").sale_tax_num, 3);
    }

    /// A district table layers over the *resolved* defaults, not the shipped
    /// ones — otherwise a `[defaults]` change would mysteriously skip any
    /// district that happened to override an unrelated key.
    #[test]
    fn district_overrides_layer_over_resolved_defaults() {
        let set = MarketConfigSet::parse(
            r#"
            [defaults]
            warehouse_slots = 80

            [districts.market]
            sale_tax_num = 1
            "#,
        )
        .unwrap();
        assert_eq!(set.for_district("market").warehouse_slots, 80);
    }

    #[test]
    fn a_misspelled_key_refuses_to_boot_and_names_it() {
        let err = MarketConfigSet::parse(
            r#"
            [defaults]
            sale_tax_numm = 5
            "#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("sale_tax_numm"), "unhelpful error: {msg}");
    }

    #[test]
    fn a_misspelled_district_refuses_to_boot_and_lists_the_real_ones() {
        let err = MarketConfigSet::parse(
            r#"
            [districts.civics]
            sale_tax_num = 1
            "#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("civics"), "should name the bad district: {msg}");
        assert!(msg.contains("civic"), "should list the real ones: {msg}");
    }

    /// Every one of these is a live footgun, not a style preference: a zero
    /// denominator divides by zero in the fee path, `min > max` makes every
    /// order unplaceable, and a default duration absent from the offered list
    /// means the fallback itself is invalid.
    #[test]
    fn out_of_range_values_refuse_to_boot_and_name_the_key() {
        let cases = [
            ("sale_tax_den = 0", "sale_tax_den"),
            ("listing_fee_den = 0", "listing_fee_den"),
            ("price_tick_gold = 0", "price_tick_gold"),
            ("listing_fee_num = -1", "listing_fee_num"),
            ("sale_tax_num = -3", "sale_tax_num"),
            ("min_order_qty = 0", "min_order_qty"),
            ("max_order_qty = 0", "min_order_qty"),
            ("order_durations_hours = []", "order_durations_hours"),
            ("order_durations_hours = [0, 24]", "order_durations_hours"),
            ("default_order_hours = 99", "default_order_hours"),
            ("max_open_orders = 0", "max_open_orders"),
            ("commands_per_minute = 0", "commands_per_minute"),
            ("listing_page_limit = 0", "listing_page_limit"),
            ("candle_interval_secs = 0", "candle_interval_secs"),
            ("history_retain_days = 0", "history_retain_days"),
            ("range = 0", "range"),
            ("warehouse_slots = -1", "warehouse_slots"),
        ];
        for (line, key) in cases {
            let text = format!("[defaults]\n{line}\n");
            let err = MarketConfigSet::parse(&text)
                .unwrap_err_or_panic(&format!("`{line}` should have been refused"));
            let msg = err.to_string();
            assert!(msg.contains(key), "error for `{line}` should name `{key}`: {msg}");
            assert!(msg.contains("defaults"), "error for `{line}` should name the table: {msg}");
        }
    }

    /// A knob the server can only honour globally must be REFUSED in a district
    /// table, not accepted and ignored. An override that parses, validates, and
    /// then does nothing is worse than a parse error: the operator has no way to
    /// find out it never took effect.
    #[test]
    fn globally_scoped_keys_are_refused_in_a_district_table() {
        for key in ["candle_interval_secs", "history_retain_days"] {
            let text = format!("[districts.market]
{key} = 7200
");
            let err = MarketConfigSet::parse(&text)
                .unwrap_err_or_panic(&format!("`{key}` should be refused per-district"));
            let msg = err.to_string();
            assert!(msg.contains(key), "error should name `{key}`: {msg}");
            assert!(msg.contains("defaults"), "error should say where it belongs: {msg}");
        }
        // ...and are of course fine in [defaults].
        let set = MarketConfigSet::parse("[defaults]
candle_interval_secs = 7200
").unwrap();
        assert_eq!(set.defaults().candle_interval_secs, 7200);
    }

    /// The provisioner is OPT-IN and validated: a typo'd item id, an inverted
    /// spread, or a nonsense size must all refuse the boot. An inverted spread
    /// especially — buying from the provisioner and selling straight back to it
    /// at a profit would be an infinite gold fountain limited only by clicking.
    #[test]
    fn provisioner_bounds_are_validated() {
        let ok = MarketConfigSet::parse(
            "[defaults.provisioner.wood]
floor = 1
ceiling = 40
bid_qty = 200
seed_stock = 10
",
        )
        .unwrap();
        let b = ok.defaults().provisioner.get("wood").unwrap();
        assert_eq!((b.floor, b.ceiling, b.bid_qty, b.seed_stock), (1, 40, 200, 10));
        // Absent by default: no configuration, no faucet.
        assert!(MarketConfig::default().provisioner.is_empty());

        let cases = [
            ("floor = 0
ceiling = 40
bid_qty = 5", "floor"),
            ("floor = 5
ceiling = 0
bid_qty = 5", "ceiling"),
            ("floor = 40
ceiling = 40
bid_qty = 5", "ceiling"), // touching = free money
            ("floor = 40
ceiling = 10
bid_qty = 5", "ceiling"), // inverted
            ("floor = 1
ceiling = 40
bid_qty = 0", "bid_qty"),
            ("floor = 1
ceiling = 40
bid_qty = 5
seed_stock = -1", "seed_stock"),
        ];
        for (body, key) in cases {
            let text = format!("[defaults.provisioner.wood]
{body}
");
            let err = MarketConfigSet::parse(&text)
                .unwrap_err_or_panic(&format!("`{body}` should have been refused"));
            assert!(err.to_string().contains(key), "error for `{body}` should name `{key}`: {err}");
        }

        // A misspelled or non-commodity item is refused rather than silently
        // backing nothing.
        for item in ["wodo", "pickaxe"] {
            let text = format!("[defaults.provisioner.{item}]
floor = 1
ceiling = 9
bid_qty = 5
");
            let err = MarketConfigSet::parse(&text).unwrap_err_or_panic("should be refused");
            assert!(err.to_string().contains(item), "{err}");
        }
    }

    /// A district stating its own provisioner REPLACES the defaults' map rather
    /// than merging key by key — a half-inherited set of price bounds would be
    /// very hard to reason about, and "this market backs exactly these goods" is
    /// the useful reading.
    #[test]
    fn a_district_provisioner_replaces_rather_than_merges() {
        let set = MarketConfigSet::parse(
            "[defaults.provisioner.wood]
floor = 1
ceiling = 40
bid_qty = 200

             [defaults.provisioner.stone]
floor = 1
ceiling = 60
bid_qty = 100

             [districts.market.provisioner.wood]
floor = 2
ceiling = 30
bid_qty = 50
",
        )
        .unwrap();
        let remote = set.for_district("market");
        assert_eq!(remote.provisioner.len(), 1, "the district's map should stand alone");
        assert_eq!(remote.provisioner.get("wood").unwrap().ceiling, 30);
        assert!(remote.provisioner.get("stone").is_none());
        // The capital keeps both.
        assert_eq!(set.for_district("civic").provisioner.len(), 2);
    }

    /// A bad value inside a district table must be caught too, and blamed on
    /// that table rather than on `[defaults]`.
    #[test]
    fn a_bad_district_value_is_blamed_on_its_own_table() {
        let err = MarketConfigSet::parse(
            r#"
            [districts.market]
            sale_tax_den = 0
            "#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("districts.market"), "wrong blame: {msg}");
        assert!(msg.contains("sale_tax_den"), "unhelpful error: {msg}");
    }

    // --- fee arithmetic, now a method instead of a free function ------------
    //
    // These properties are anti-abuse, not cosmetics, and they must hold for
    // ANY configured rate — a rate an operator can edit is exactly how they'd
    // otherwise be broken.

    #[test]
    fn fees_round_toward_the_house_and_are_never_zero() {
        let c = MarketConfig::default();
        // Nothing to charge on nothing.
        assert_eq!(c.listing_fee(0), 0);
        assert_eq!(c.listing_fee(-5), 0);
        assert_eq!(c.sale_tax(0), 0);

        // Below the percentage's resolution, the floor holds — this is the
        // anti-exploit: a fee rounding to zero on small orders would make a
        // free lane, and splitting one order into a hundred tiny ones would
        // dodge the sink entirely.
        for notional in 1..=100 {
            assert!(
                c.listing_fee(notional) >= c.listing_fee_min_gold,
                "notional {notional} charged nothing"
            );
        }
        for value in 1..=100 {
            assert!(c.sale_tax(value) >= 1, "fill worth {value} taxed nothing");
        }

        // Rounds UP, never down.
        assert_eq!(c.listing_fee(100), 1); // exactly 1%
        assert_eq!(c.listing_fee(101), 2); // 1.01 -> 2
        assert_eq!(c.listing_fee(250), 3); // 2.5 -> 3
        assert_eq!(c.sale_tax(100), 3); // exactly 3%
        assert_eq!(c.sale_tax(101), 4); // 3.03 -> 4
        assert_eq!(c.sale_tax(1), 1); // 0.03 -> floored up to 1

        // Monotonic: a bigger order never costs less to list.
        let mut last = 0;
        for notional in (0..5_000).step_by(7) {
            let f = c.listing_fee(notional);
            assert!(f >= last, "listing fee dipped at {notional}");
            last = f;
        }
    }

    /// Splitting one order into many must never be cheaper than placing it
    /// whole — otherwise the fee is a suggestion. (It's strictly *more*
    /// expensive here, because every slice pays at least the floor.)
    ///
    /// Extended in #152 to run against ARBITRARY configured rates, not just the
    /// shipped ones: the rates are now operator-editable, so "did someone tune
    /// the fee into a free lane?" became a real question a test has to answer.
    #[test]
    fn splitting_an_order_never_dodges_the_fee() {
        let shipped = MarketConfig::default();
        for (price, qty) in [(5, 20), (8, 50), (13, 7), (100, 3)] {
            let whole = shipped.listing_fee(price * qty);
            let split: i64 = (0..qty).map(|_| shipped.listing_fee(price)).sum();
            assert!(
                split >= whole,
                "splitting {qty}x{price} cost {split} vs {whole} whole — that's a free lane"
            );
        }
        // Same for the sale tax across partial fills.
        for (price, qty) in [(7, 30), (11, 9)] {
            let whole = shipped.sale_tax(price * qty);
            let split: i64 = (0..qty).map(|_| shipped.sale_tax(price)).sum();
            assert!(split >= whole, "splitting fills dodged tax: {split} < {whole}");
        }
        // And for rates an operator might plausibly write, including a zeroed
        // percentage (where the floor is the only thing holding the line).
        for (num, den, min_gold) in [(1i64, 100i64, 1i64), (5, 1000, 2), (0, 100, 1), (7, 50, 3)] {
            let c = MarketConfig {
                listing_fee_num: num,
                listing_fee_den: den,
                listing_fee_min_gold: min_gold,
                ..MarketConfig::default()
            };
            for (price, qty) in [(5i64, 20i64), (13, 7), (100, 3)] {
                let whole = c.listing_fee(price * qty);
                let split: i64 = (0..qty).map(|_| c.listing_fee(price)).sum();
                assert!(
                    split >= whole,
                    "{num}/{den} min {min_gold}: splitting {qty}x{price} paid {split} vs {whole}"
                );
            }
        }
    }

    /// A zero-rate fee still charges the floor, so "free" has to be spelled out
    /// as a zero floor as well — worth pinning so #155's rate-0 default and any
    /// operator trying to disable fees get the behaviour they expect.
    #[test]
    fn a_fee_is_only_truly_off_when_the_floor_is_zero_too() {
        let floored = MarketConfig { listing_fee_num: 0, ..MarketConfig::default() };
        assert_eq!(floored.listing_fee(1000), 1, "the floor still applies");
        let off = MarketConfig {
            listing_fee_num: 0,
            listing_fee_min_gold: 0,
            ..MarketConfig::default()
        };
        assert_eq!(off.listing_fee(1000), 0);
    }

    #[test]
    fn an_unoffered_duration_falls_back_to_the_default() {
        let c = MarketConfig::default();
        assert_eq!(c.order_duration_hours(72), 72);
        assert_eq!(c.order_duration_hours(9999), c.default_order_hours);
        assert_eq!(c.order_duration_hours(0), c.default_order_hours);
        assert_eq!(c.order_duration_hours(-5), c.default_order_hours);
    }

    /// The price tick is configurable, so validation has to actually use it
    /// rather than assuming 1.
    #[test]
    fn validation_honours_a_configured_price_tick_and_size_bounds() {
        let c = MarketConfig { price_tick_gold: 5, max_order_qty: 100, ..MarketConfig::default() };
        assert!(c.validate_order("wood", 10, 1).is_ok());
        assert_eq!(c.validate_order("wood", 7, 1), Err(OrderReject::BadPrice));
        assert_eq!(c.validate_order("wood", 10, 101), Err(OrderReject::BadQty));
        assert_eq!(c.validate_order("wood", 10, 0), Err(OrderReject::BadQty));
        // Uniques stay off the book whatever the rates say.
        assert_eq!(c.validate_order("pickaxe", 10, 1), Err(OrderReject::NotACommodity));
    }

    /// Everything the client previews a number from must be on the wire. A key
    /// missing here is a silent trust bug: the panel would fall back to its
    /// compile-time default and quote a fee the server won't charge.
    #[test]
    fn wire_rules_carry_every_value_the_client_previews() {
        let c = MarketConfig::default();
        let v = c.wire_rules();
        for key in [
            "range",
            "warehouse_slots",
            "price_tick_gold",
            "min_order_qty",
            "max_order_qty",
            "order_durations_hours",
            "default_order_hours",
            "max_open_orders",
            "listing_fee_min_gold",
            "listing_fee_num",
            "listing_fee_den",
            "sale_tax_num",
            "sale_tax_den",
            "candle_interval_secs",
        ] {
            assert!(!v[key].is_null(), "market.opened rules missing `{key}`");
        }
        assert_eq!(v["sale_tax_num"], 3);
        assert_eq!(v["order_durations_hours"], serde_json::json!([12, 24, 72, 168]));
    }

    /// Tiny helper so the table-driven validation test reads as a list of cases
    /// rather than a wall of `match`.
    trait UnwrapErrOrPanic<T> {
        fn unwrap_err_or_panic(self, msg: &str) -> ConfigError;
    }
    impl UnwrapErrOrPanic<MarketConfigSet> for Result<MarketConfigSet, ConfigError> {
        fn unwrap_err_or_panic(self, msg: &str) -> ConfigError {
            match self {
                Ok(_) => panic!("{msg}"),
                Err(e) => e,
            }
        }
    }
}
