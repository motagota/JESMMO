//! `crafting.toml` — deposits, and later stations and recipes (mine epic #164).
//!
//! Issue #166 fills in the deposit half. Stations and recipes land in #167/#168
//! and share this file, so the mine's whole tuning surface is one place.
//!
//! ## Why a deposit isn't a resource node
//!
//! `ResourceNode` already exists: a fixed spawn with a quantity, harvested one
//! unit per swing, refilling after ~10s. A deposit reuses that shape — and the
//! whole Pick-ability swing path with it — but differs where it matters:
//!
//! * **Yields are probabilistic and multi-entry.** A node gives one item per
//!   swing. A deposit rolls each line independently, so starter iron misses on
//!   nearly half the swings. That miss chance is one of the three levers holding
//!   the mine's coin faucet down (#170).
//! * **Charges, not quantity.** A deposit is worked a fixed number of times
//!   regardless of what it produces.
//! * **Respawn jitter**, so the mine never re-blooms in a synchronised wave and
//!   nobody learns to camp a clock.
//! * **XP falloff**, so the tutorial mine stops being worth grinding long before
//!   it stops being usable.
//!
//! The contention model is the load-bearing part, and it isn't in this file
//! because it's an absence: **there is no lock.** Several players work one
//! deposit at once, each swing resolving independently, and a late arrival still
//! gets value from a partly-worked seam. The resource is contested; the
//! interaction is not. That is what makes shared nodes survivable in a starter
//! area, and why per-party instancing was ruled out.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// One line of a deposit's loot table, rolled independently of the others.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Yield {
    pub item: String,
    #[serde(default = "one")]
    pub qty_min: i64,
    #[serde(default = "one")]
    pub qty_max: i64,
    /// 0.0-1.0. Below 1.0 the swing can come up empty, which is deliberate:
    /// a guaranteed yield turns a mine into a faucet with a fixed rate.
    #[serde(default = "one_f")]
    pub chance: f64,
}

fn one() -> i64 {
    1
}
fn one_f() -> f64 {
    1.0
}

/// A kind of deposit. Placement lives in `zones.toml`; this is the behaviour.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepositType {
    pub display_name: String,
    /// The skill trained, and gated on.
    pub skill: String,
    #[serde(default = "one")]
    pub required_level: i64,
    /// The equipped tool this needs — matched against the item id, so a future
    /// better pickaxe is a config change rather than a code one.
    pub required_tool: String,
    /// Swings before the seam is spent. Shared across everyone working it.
    pub charges: i64,
    pub swing_time_ms: i64,
    #[serde(default = "one")]
    pub durability_per_swing: i64,
    pub respawn_seconds: i64,
    /// Spread on the respawn, so a cleared mine doesn't come back all at once.
    #[serde(default)]
    pub respawn_jitter_seconds: i64,
    pub xp_per_swing: i64,
    /// Above this level the XP decays toward zero — see
    /// `world::xp_with_falloff`.
    #[serde(default)]
    pub xp_falloff_level: i64,
    pub yields: Vec<Yield>,
}

impl DepositType {
    /// Roll the whole loot table for one swing. Each line is independent, so a
    /// swing can produce nothing, one thing, or several.
    pub fn roll(&self, rng: &mut impl rand::Rng) -> Vec<(String, i64)> {
        let mut out = Vec::new();
        for y in &self.yields {
            if y.chance < 1.0 && !rng.gen_bool(y.chance.clamp(0.0, 1.0)) {
                continue;
            }
            let qty = if y.qty_max > y.qty_min {
                rng.gen_range(y.qty_min..=y.qty_max)
            } else {
                y.qty_min
            };
            if qty > 0 {
                out.push((y.item.clone(), qty));
            }
        }
        out
    }

    /// How long this seam stays spent, given a roll in `0.0..1.0`. Jitter is
    /// symmetric around `respawn_seconds` and never pushes it below a second.
    pub fn respawn_after(&self, roll: f64) -> i64 {
        if self.respawn_jitter_seconds <= 0 {
            return self.respawn_seconds.max(1);
        }
        let spread = self.respawn_jitter_seconds as f64;
        let offset = (roll.clamp(0.0, 1.0) * 2.0 - 1.0) * spread;
        ((self.respawn_seconds as f64 + offset).round() as i64).max(1)
    }
}

/// What levelling a gathering skill buys (#166).
///
/// Both bonuses are capped. An uncapped per-level bonus eventually makes a
/// high-level miner strictly better at everything, which quietly removes the
/// reason for anyone else to mine — and at that point the cap is the only thing
/// standing between "progression" and "the only correct answer".
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillCurve {
    /// Percent off the swing time per level.
    #[serde(default)]
    pub speed_bonus_pct_per_level: f64,
    #[serde(default)]
    pub max_speed_bonus_pct: f64,
    /// Percentage-point chance per level of one extra roll of the whole loot
    /// table — so the bonus pays out in the same proportions the seam does,
    /// rather than favouring whichever line happens to be listed first.
    #[serde(default)]
    pub bonus_yield_chance_per_level: f64,
    #[serde(default)]
    pub max_bonus_yield_chance: f64,
    /// Percentage points of `failure_chance` removed per level (#168).
    #[serde(default)]
    pub failure_reduction_per_level: f64,
    /// The lowest failure chance this skill can ever reach, as a percentage.
    ///
    /// NEVER ZERO for a skill that has one. The floor is a permanent material
    /// sink — it is the reason pottery keeps consuming clay after everyone has
    /// levelled it, and the reason the clay seams stay worth working. A mastery
    /// curve that ends in "no failure ever" ends the demand with it.
    #[serde(default)]
    pub min_failure_chance: f64,
}

impl Default for SkillCurve {
    fn default() -> Self {
        SkillCurve {
            speed_bonus_pct_per_level: 0.0,
            max_speed_bonus_pct: 0.0,
            bonus_yield_chance_per_level: 0.0,
            max_bonus_yield_chance: 0.0,
            failure_reduction_per_level: 0.0,
            min_failure_chance: 0.0,
        }
    }
}

impl SkillCurve {
    /// Swing time at `level`, never below a quarter of the base however the
    /// numbers are tuned — a swing that rounds to nothing stops being an action.
    pub fn swing_time_ms(&self, base: i64, level: i64) -> i64 {
        let pct = (self.speed_bonus_pct_per_level * level.max(0) as f64)
            .min(self.max_speed_bonus_pct.max(0.0));
        let scaled = (base as f64 * (1.0 - pct / 100.0)).round() as i64;
        scaled.max((base as f64 * 0.25).round() as i64).max(1)
    }

    /// Chance in 0.0..1.0 of an extra roll of the loot table at `level`.
    pub fn bonus_yield_chance(&self, level: i64) -> f64 {
        let pct = (self.bonus_yield_chance_per_level * level.max(0) as f64)
            .min(self.max_bonus_yield_chance.max(0.0));
        (pct / 100.0).clamp(0.0, 1.0)
    }

    /// A recipe's failure chance at this level, as a fraction 0.0-1.0.
    ///
    /// Skill drives it DOWN toward `min_failure_chance` and no further. A
    /// recipe that declares no failure has none at any level — levelling can't
    /// invent a risk that isn't there.
    pub fn failure_chance(&self, base_pct: f64, level: i64) -> f64 {
        if base_pct <= 0.0 {
            return 0.0;
        }
        let reduced = base_pct - self.failure_reduction_per_level * level.max(0) as f64;
        // The floor applies even when it is above the base: a recipe easier
        // than the floor stays as easy as it was written.
        let floored = reduced.max(self.min_failure_chance.min(base_pct));
        (floored / 100.0).clamp(0.0, 1.0)
    }
}

/// What a station *is*, which decides only one thing: whether it burns fuel.
///
/// Furnace, kiln, campfire and potter's wheel are deliberately ONE entity with a
/// recipe filter rather than four station implementations, so the next station is
/// a config entry instead of new code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StationKind {
    /// Burns fuel to do its work: furnace, kiln, campfire.
    Heat,
    /// Needs no fuel, only hands: potter's wheel, workbench.
    Shaping,
}

/// One ingredient of a station recipe.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ingredient {
    pub item: String,
    #[serde(default = "one")]
    pub qty: i64,
}

/// A kind of station. Placement lives in `zones.toml`; this is the behaviour —
/// exactly the split deposits use.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StationType {
    pub display_name: String,
    pub kind: StationKind,
    /// A recipe is usable here if any of its tags appears in this list. Tags
    /// rather than a recipe list so adding a recipe doesn't mean editing every
    /// station that should accept it.
    pub recipe_tags: Vec<String>,
    /// Accepted fuel items, and what one of each is worth in fuel units. Empty
    /// for a `shaping` station, which is why the two kinds exist at all.
    #[serde(default)]
    pub fuels: BTreeMap<String, i64>,
    /// Concurrent jobs **per player**, not in total.
    ///
    /// This is the load-bearing number on a public station. A queue shared
    /// across everyone is a griefing surface and a miserable wait; per-player
    /// slots are what make a public station usable at all.
    #[serde(default = "one")]
    pub job_slots: i64,
    /// Charged in gold when a job starts, and BURNED — see `station_fee` on the
    /// gold ledger. A fee that quietly vanished would break the supply identity
    /// #154 established.
    #[serde(default)]
    pub usage_fee_gold: i64,
    /// How close a player must be to use it, in world units.
    #[serde(default = "default_station_radius")]
    pub radius: i64,
    /// When false, jobs run on while the player is offline and the output waits
    /// in the slot. That is the default because the alternative — a job that
    /// silently stops when you walk away — is indistinguishable from a bug.
    #[serde(default)]
    pub requires_presence: bool,
}

fn default_station_radius() -> i64 {
    40
}

/// An optional catalyst a recipe can consume the wear of (#168).
///
/// GENERIC ON PURPOSE. The clay crucible is the first, but nothing here knows
/// what a crucible is: any recipe may name any durable item, so the next
/// catalyst is a config entry.
///
/// OPTIONAL, NOT REQUIRED, and that is the load-bearing part. A required
/// crucible would mean a clay shortage stalls the entire iron economy, and
/// every new player hits a wall between "I mined ore" and "I have metal".
/// Optional keeps the chain unbroken while still giving potters permanent
/// demand — every crucible in the world is on its way to being used up.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalyst {
    /// The item that must be equipped in the `catalyst` slot to apply.
    pub item: String,
    /// Durability spent per job.
    #[serde(default = "one")]
    pub wear: i64,
    /// Percentage chance of one extra output, on top of any skill bonus.
    #[serde(default)]
    pub bonus_chance: f64,
    /// Percentage cut to the job's duration.
    #[serde(default)]
    pub speed_bonus_pct: f64,
}

/// A timed recipe made at a station, as opposed to the instant `world::Recipe`
/// made at a home crafting structure.
///
/// The two registries are deliberately separate. An instant recipe is a
/// compile-time constant in `world.rs`; a station recipe is data, because the
/// mine's tuning surface is meant to be editable without a rebuild. Merging them
/// would mean either making the instant ones data (a refactor this issue doesn't
/// need) or making these code (which defeats the point).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StationRecipe {
    pub display_name: String,
    /// Matched against a station's `recipe_tags`.
    pub tags: Vec<String>,
    pub skill: String,
    #[serde(default = "one")]
    pub required_level: i64,
    pub inputs: Vec<Ingredient>,
    pub output_item: String,
    #[serde(default = "one")]
    pub output_qty: i64,
    /// Fuel units burned by the whole job, taken from the station's buffer at
    /// START. Zero for a `shaping` recipe.
    #[serde(default)]
    pub fuel_units: i64,
    pub duration_ms: i64,
    #[serde(default)]
    pub xp: i64,
    /// Percentage chance the job FAILS, consuming its inputs and producing
    /// nothing (#168). Zero for everything but shaping.
    ///
    /// This is on the recipe rather than the station because difficulty belongs
    /// to what you are making, not to what you are making it on — a crucible is
    /// harder to throw than a jar at the same wheel.
    #[serde(default)]
    pub failure_chance: f64,
    /// What fraction of the XP a FAILED attempt still grants, 0.0-1.0.
    ///
    /// Nonzero by default, because failure that teaches nothing is just a tax:
    /// the player spent the clay and the time either way, and a skill whose
    /// early levels are pure loss is a skill nobody levels.
    #[serde(default = "half")]
    pub failure_xp_fraction: f64,
    /// An optional catalyst that improves the job if the player has one.
    #[serde(default)]
    pub catalyst: Option<Catalyst>,
    /// How many station fees this job costs (#170).
    ///
    /// A bulk recipe MUST set this to its multiple. The station's fee is charged
    /// per job, so a x4 recipe paying one fee would be a quarter of the cost per
    /// ingot — and a bulk recipe that is cheaper per unit is not a convenience,
    /// it is a discount for knowing which button to press. Same anti-split
    /// property the market fees needed in #141, pointing the other way.
    ///
    /// Bulk exists to spend fewer clicks, never fewer resources.
    #[serde(default = "one")]
    pub fee_multiplier: i64,
}

fn half() -> f64 {
    0.5
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CraftingConfig {
    #[serde(default)]
    pub deposit: BTreeMap<String, DepositType>,
    /// Per-skill progression curves, keyed by skill id.
    #[serde(default)]
    pub skill: BTreeMap<String, SkillCurve>,
    /// Station types, keyed by type id. `zones.toml` places instances of these.
    #[serde(default)]
    pub station: BTreeMap<String, StationType>,
    /// Timed station recipes, keyed by recipe id.
    #[serde(default)]
    pub recipe: BTreeMap<String, StationRecipe>,
}

impl CraftingConfig {
    pub fn deposit(&self, id: &str) -> Option<&DepositType> {
        self.deposit.get(id)
    }

    pub fn station(&self, id: &str) -> Option<&StationType> {
        self.station.get(id)
    }

    pub fn recipe(&self, id: &str) -> Option<&StationRecipe> {
        self.recipe.get(id)
    }

    /// Whether `recipe` may be made at `station` — the tag intersection.
    pub fn station_accepts(&self, station: &StationType, recipe: &StationRecipe) -> bool {
        recipe.tags.iter().any(|t| station.recipe_tags.contains(t))
    }

    /// Every recipe this station type accepts, in id order. What the client's
    /// station panel is populated from, so the two can never disagree about
    /// what is makeable where.
    pub fn recipes_for(&self, station: &StationType) -> Vec<(&String, &StationRecipe)> {
        self.recipe
            .iter()
            .filter(|(_, r)| self.station_accepts(station, r))
            .collect()
    }

    /// The curve for `skill`, or a flat one — an unconfigured skill simply
    /// grants no bonuses rather than failing.
    pub fn skill(&self, skill: &str) -> SkillCurve {
        self.skill.get(skill).cloned().unwrap_or_default()
    }

    pub fn parse(text: &str) -> Result<CraftingConfig, CraftingConfigError> {
        let cfg: CraftingConfig = toml::from_str(text).map_err(CraftingConfigError::Toml)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// A missing file means no deposits, which is the world before #166. A
    /// malformed one refuses to boot, same contract as `market.toml` (#152).
    pub fn load(path: &Path) -> Result<CraftingConfig, CraftingConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => CraftingConfig::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CraftingConfig::default()),
            Err(e) => Err(CraftingConfigError::Io(e)),
        }
    }

    fn validate(&self) -> Result<(), CraftingConfigError> {
        for (id, d) in &self.deposit {
            let bad = |why: &str| {
                Err(CraftingConfigError::Invalid { what: format!("deposit.{id}"), why: why.to_string() })
            };
            if d.charges <= 0 {
                return bad("charges must be positive — a seam with none can never be worked");
            }
            if d.swing_time_ms <= 0 {
                return bad("swing_time_ms must be positive");
            }
            if d.respawn_seconds <= 0 {
                return bad("respawn_seconds must be positive");
            }
            if d.respawn_jitter_seconds < 0 {
                return bad("respawn_jitter_seconds must not be negative");
            }
            if d.respawn_jitter_seconds >= d.respawn_seconds {
                return bad(
                    "respawn_jitter_seconds must be smaller than respawn_seconds, or a seam \
                     could come back instantly",
                );
            }
            if d.durability_per_swing < 0 {
                return bad("durability_per_swing must not be negative");
            }
            if d.xp_per_swing < 0 {
                return bad("xp_per_swing must not be negative");
            }
            if crate::world::item(&d.required_tool).is_none() {
                return bad("required_tool is not a real item");
            }
            if crate::world::tool_max_durability(&d.required_tool).is_none() {
                return bad("required_tool is not a tool — it has no durability to spend");
            }
            if d.yields.is_empty() {
                return bad("has no yields — working it would produce nothing");
            }
            for y in &d.yields {
                if crate::world::item(&y.item).is_none() {
                    return Err(CraftingConfigError::Invalid {
                        what: format!("deposit.{id}"),
                        why: format!("yields `{}`, which is not a real item", y.item),
                    });
                }
                if !(0.0..=1.0).contains(&y.chance) {
                    return Err(CraftingConfigError::Invalid {
                        what: format!("deposit.{id}"),
                        why: format!("`{}` has a chance outside 0.0-1.0", y.item),
                    });
                }
                if y.qty_min <= 0 || y.qty_max < y.qty_min {
                    return Err(CraftingConfigError::Invalid {
                        what: format!("deposit.{id}"),
                        why: format!("`{}` has a nonsense quantity range", y.item),
                    });
                }
            }
        }
        for (id, st) in &self.station {
            let bad = |why: &str| {
                Err(CraftingConfigError::Invalid { what: format!("station.{id}"), why: why.to_string() })
            };
            if st.job_slots <= 0 {
                return bad("job_slots must be positive — a station nobody can queue at is furniture");
            }
            if st.usage_fee_gold < 0 {
                return bad("usage_fee_gold must not be negative — a station that PAYS you is a gold faucet");
            }
            if st.radius <= 0 {
                return bad("radius must be positive");
            }
            if st.recipe_tags.is_empty() {
                return bad("has no recipe_tags, so nothing could ever be made at it");
            }
            match st.kind {
                StationKind::Heat if st.fuels.is_empty() => {
                    return bad("is a heat station with no accepted fuels, so it could never be lit");
                }
                StationKind::Shaping if !st.fuels.is_empty() => {
                    return bad("is a shaping station but accepts fuel — it has nothing to burn it for");
                }
                _ => {}
            }
            for (item, units) in &st.fuels {
                if crate::world::item(item).is_none() {
                    return Err(CraftingConfigError::Invalid {
                        what: format!("station.{id}"),
                        why: format!("accepts `{item}` as fuel, which is not a real item"),
                    });
                }
                if *units <= 0 {
                    return Err(CraftingConfigError::Invalid {
                        what: format!("station.{id}"),
                        why: format!("`{item}` is worth {units} fuel units — loading it would burn nothing"),
                    });
                }
            }
        }
        for (id, r) in &self.recipe {
            let bad = |why: &str| {
                Err(CraftingConfigError::Invalid { what: format!("recipe.{id}"), why: why.to_string() })
            };
            if r.duration_ms <= 0 {
                return bad("duration_ms must be positive — an instant job belongs in the world.rs registry");
            }
            if r.tags.is_empty() {
                return bad("has no tags, so no station would accept it");
            }
            if r.inputs.is_empty() {
                return bad("has no inputs — it would make something from nothing");
            }
            if r.output_qty <= 0 {
                return bad("output_qty must be positive");
            }
            if r.fuel_units < 0 {
                return bad("fuel_units must not be negative");
            }
            if r.xp < 0 {
                return bad("xp must not be negative");
            }
            if !(0.0..=100.0).contains(&r.failure_chance) {
                return bad("failure_chance must be a percentage between 0 and 100");
            }
            if !(0.0..=1.0).contains(&r.failure_xp_fraction) {
                return bad("failure_xp_fraction must be between 0.0 and 1.0");
            }
            if r.fee_multiplier < 1 {
                return bad("fee_multiplier must be at least 1 — a job cannot cost less than one fee");
            }
            if let Some(c) = &r.catalyst {
                if crate::world::item(&c.item).is_none() {
                    return Err(CraftingConfigError::Invalid {
                        what: format!("recipe.{id}"),
                        why: format!("names catalyst `{}`, which is not a real item", c.item),
                    });
                }
                // A catalyst is spent by USE, so it needs durability to spend.
                // Without this a config could name a stackable item and the
                // wear would silently do nothing.
                if crate::world::tool_max_durability(&c.item).is_none() {
                    return Err(CraftingConfigError::Invalid {
                        what: format!("recipe.{id}"),
                        why: format!("catalyst `{}` has no durability, so it could never be consumed", c.item),
                    });
                }
                if c.wear <= 0 {
                    return bad("a catalyst that never wears is a permanent upgrade, not a catalyst");
                }
                if c.bonus_chance < 0.0 || c.speed_bonus_pct < 0.0 {
                    return bad("catalyst bonuses must not be negative");
                }
                if c.bonus_chance <= 0.0 && c.speed_bonus_pct <= 0.0 {
                    return bad("a catalyst that grants nothing would only cost the player a crucible");
                }
            }
            if crate::world::item(&r.output_item).is_none() {
                return Err(CraftingConfigError::Invalid {
                    what: format!("recipe.{id}"),
                    why: format!("outputs `{}`, which is not a real item", r.output_item),
                });
            }
            for i in &r.inputs {
                if crate::world::item(&i.item).is_none() {
                    return Err(CraftingConfigError::Invalid {
                        what: format!("recipe.{id}"),
                        why: format!("takes `{}`, which is not a real item", i.item),
                    });
                }
                if i.qty <= 0 {
                    return Err(CraftingConfigError::Invalid {
                        what: format!("recipe.{id}"),
                        why: format!("takes {} of `{}`", i.qty, i.item),
                    });
                }
            }
            // A recipe no station accepts is unmakeable, and silently so — it
            // shows up as an empty panel rather than an error, which is exactly
            // the kind of thing that survives a review.
            if !self.station.is_empty()
                && !self.station.values().any(|st| self.station_accepts(st, r))
            {
                return bad("no configured station accepts any of its tags, so it could never be made");
            }
            // Fuel is charged at start against the station's buffer. A fuelled
            // recipe on a shaping station could never start.
            if r.fuel_units > 0 {
                let heat_accepts = self
                    .station
                    .values()
                    .any(|st| st.kind == StationKind::Heat && self.station_accepts(st, r));
                if !self.station.is_empty() && !heat_accepts {
                    return bad("needs fuel but only shaping stations accept it, so it could never start");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum CraftingConfigError {
    Toml(toml::de::Error),
    Io(std::io::Error),
    Invalid { what: String, why: String },
}

impl std::fmt::Display for CraftingConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CraftingConfigError::Toml(e) => write!(f, "crafting.toml parse error: {e}"),
            CraftingConfigError::Io(e) => write!(f, "crafting.toml read error: {e}"),
            CraftingConfigError::Invalid { what, why } => {
                write!(f, "crafting.toml [{what}]: {why}")
            }
        }
    }
}
impl std::error::Error for CraftingConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    fn iron() -> &'static str {
        r#"
        [deposit.iron_starter]
        display_name = "Iron Deposit"
        skill = "mining"
        required_level = 1
        required_tool = "pickaxe"
        charges = 8
        swing_time_ms = 3000
        respawn_seconds = 75
        respawn_jitter_seconds = 25
        xp_per_swing = 6
        xp_falloff_level = 15

        [[deposit.iron_starter.yields]]
        item = "iron_ore"
        chance = 0.55

        [[deposit.iron_starter.yields]]
        item = "stone"
        qty_min = 1
        qty_max = 2
        chance = 0.30
        "#
    }

    /// Both bonuses are capped, and the cap is what stops a high-level miner
    /// becoming the only correct answer.
    /// Skill drives shaping failure DOWN toward the floor and stops there.
    ///
    /// The floor never reaching zero is not a rounding detail — it is the
    /// economic point of the whole skill. It is what keeps clay worth mining
    /// after everyone has levelled Pottery, and what gives potters a reason to
    /// keep buying it. A curve that ended in "no failure ever" would end the
    /// demand with it, and pottery would become a thing you finish rather than
    /// a thing you do.
    #[test]
    fn shaping_failure_falls_with_level_but_never_reaches_zero() {
        let curve = SkillCurve {
            failure_reduction_per_level: 1.2,
            min_failure_chance: 8.0,
            ..SkillCurve::default()
        };

        assert!((curve.failure_chance(35.0, 0) - 0.35).abs() < 1e-9, "untrained: as written");
        assert!((curve.failure_chance(35.0, 10) - 0.23).abs() < 1e-9, "35 - 12 = 23%");

        // The floor, and everything past it.
        assert!((curve.failure_chance(35.0, 25) - 0.08).abs() < 1e-9, "floored at 8%");
        for level in [30, 60, 200, 10_000] {
            assert!(
                (curve.failure_chance(35.0, level) - 0.08).abs() < 1e-9,
                "level {level} must not fall through the floor"
            );
            assert!(curve.failure_chance(35.0, level) > 0.0, "and must never reach zero");
        }

        // A recipe that declares no failure has none at any level — levelling
        // cannot invent a risk that isn't in the recipe.
        assert_eq!(curve.failure_chance(0.0, 0), 0.0);
        assert_eq!(curve.failure_chance(0.0, 99), 0.0);

        // A recipe EASIER than the floor stays as easy as it was written, rather
        // than being dragged up to it.
        assert!((curve.failure_chance(5.0, 0) - 0.05).abs() < 1e-9);
        assert!((curve.failure_chance(5.0, 99) - 0.05).abs() < 1e-9);

        // An unconfigured skill has no failure curve at all, so a recipe's own
        // chance stands unmodified rather than silently becoming zero.
        let flat = SkillCurve::default();
        assert!((flat.failure_chance(35.0, 50) - 0.35).abs() < 1e-9);
    }

    /// The shipped Pottery curve actually reaches its floor within a level
    /// range a player will see, and the shipped recipes agree with it.
    #[test]
    fn the_shipped_pottery_curve_is_reachable_and_bites() {
        let cfg = CraftingConfig::load(std::path::Path::new(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../crafting.toml"),
        ))
        .expect("the shipped crafting.toml should load");
        let curve = cfg.skill("pottery");
        assert!(curve.min_failure_chance > 0.0, "pottery's floor must not be zero");

        let crucible = cfg.recipe("greenware_crucible").expect("the crucible recipe");
        assert!(crucible.failure_chance > 0.0, "shaping should be able to fail");
        assert!(crucible.failure_xp_fraction > 0.0, "a failed throw should still teach");

        let at_0 = curve.failure_chance(crucible.failure_chance, 0);
        let at_50 = curve.failure_chance(crucible.failure_chance, 50);
        assert!(at_50 < at_0, "levelling should visibly help");
        assert!(at_50 > 0.0, "but never all the way");
        assert!(
            (at_50 - curve.min_failure_chance / 100.0).abs() < 1e-9,
            "a level-50 potter should be sitting exactly on the floor"
        );

        // Firing must NOT be able to fail: the player is not present for it, and
        // punishing them for something they cannot influence is just a tax.
        let fired = cfg.recipe("clay_crucible").expect("the firing recipe");
        assert_eq!(fired.failure_chance, 0.0, "firing is safe by design");
    }

    #[test]
    fn skill_bonuses_scale_with_level_and_stop_at_their_cap() {
        let cfg = CraftingConfig::parse(
            r#"
            [skill.mining]
            speed_bonus_pct_per_level = 0.4
            max_speed_bonus_pct = 25
            bonus_yield_chance_per_level = 0.3
            max_bonus_yield_chance = 20
            "#,
        )
        .unwrap();
        let m = cfg.skill("mining");

        assert_eq!(m.swing_time_ms(3000, 0), 3000, "level 0 gets nothing");
        assert_eq!(m.swing_time_ms(3000, 10), 2880, "0.4%/level -> 4% off at 10");
        // Capped: level 100 and level 1000 are the same swing.
        assert_eq!(m.swing_time_ms(3000, 100), m.swing_time_ms(3000, 1000));
        assert_eq!(m.swing_time_ms(3000, 100), 2250, "25% is the ceiling");

        assert!(m.bonus_yield_chance(0).abs() < 1e-9);
        assert!((m.bonus_yield_chance(10) - 0.03).abs() < 1e-9);
        assert!((m.bonus_yield_chance(1000) - 0.20).abs() < 1e-9, "capped at 20%");

        // A swing can never round away to nothing, however it is tuned.
        let silly = CraftingConfig::parse(
            "[skill.mining]
speed_bonus_pct_per_level = 50
max_speed_bonus_pct = 99",
        )
        .unwrap();
        assert!(silly.skill("mining").swing_time_ms(3000, 50) >= 750);

        // An unconfigured skill grants nothing rather than failing.
        let flat = cfg.skill("smelting");
        assert_eq!(flat.swing_time_ms(3000, 99), 3000);
        assert!(flat.bonus_yield_chance(99).abs() < 1e-9);
    }

    #[test]
    fn a_missing_file_means_no_deposits() {
        let cfg = CraftingConfig::load(Path::new("no_such_crafting.toml")).unwrap();
        assert!(cfg.deposit.is_empty());
    }

    #[test]
    fn a_deposit_type_parses_with_its_loot_table() {
        let cfg = CraftingConfig::parse(iron()).unwrap();
        let d = cfg.deposit("iron_starter").expect("iron");
        assert_eq!(d.charges, 8);
        assert_eq!(d.required_tool, "pickaxe");
        assert_eq!(d.durability_per_swing, 1, "defaults to a point a swing");
        assert_eq!(d.yields.len(), 2);
        assert_eq!(d.yields[0].qty_min, 1, "a single-quantity line needs no range");
    }

    /// Each line rolls independently, so a swing produces nothing, one thing or
    /// several — and over a large sample the rates match what was configured.
    /// The miss chance is a balance lever (#170), so it has to actually miss.
    #[test]
    fn yields_roll_independently_at_their_configured_rates() {
        let cfg = CraftingConfig::parse(iron()).unwrap();
        let d = cfg.deposit("iron_starter").unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);

        let (mut ore, mut stone, mut empty, n) = (0, 0, 0, 20_000);
        for _ in 0..n {
            let got = d.roll(&mut rng);
            if got.is_empty() {
                empty += 1;
            }
            for (item, qty) in got {
                assert!(qty >= 1);
                match item.as_str() {
                    "iron_ore" => ore += 1,
                    "stone" => stone += 1,
                    other => panic!("rolled something not in the table: {other}"),
                }
            }
        }
        let rate = |k: i32| k as f64 / n as f64;
        assert!((rate(ore) - 0.55).abs() < 0.02, "ore rate {:.3}", rate(ore));
        assert!((rate(stone) - 0.30).abs() < 0.02, "stone rate {:.3}", rate(stone));
        assert!(empty > 0, "a sub-1.0 chance must actually miss sometimes");
    }

    /// Jitter is what stops a cleared mine re-blooming in a wave and players
    /// learning to camp a clock.
    #[test]
    fn respawn_jitter_spreads_around_the_configured_window() {
        let cfg = CraftingConfig::parse(iron()).unwrap();
        let d = cfg.deposit("iron_starter").unwrap();
        assert_eq!(d.respawn_after(0.5), 75, "the midpoint is the configured window");
        assert_eq!(d.respawn_after(0.0), 50, "fully early");
        assert_eq!(d.respawn_after(1.0), 100, "fully late");
        // Never instant, however the dice fall.
        for i in 0..=20 {
            assert!(d.respawn_after(i as f64 / 20.0) >= 1);
        }
    }

    /// Every one of these ships a mine that misbehaves in a way somebody would
    /// have to debug from player reports. Refused at boot instead.
    #[test]
    fn nonsense_deposits_refuse_to_boot() {
        // The tool is parameterised rather than overridden: TOML rejects a
        // duplicate key before validation ever runs, so an "override" line would
        // test the parser instead of the rule.
        let base = |tool: &str, body: &str| {
            format!(
                r#"[deposit.d]
                   display_name = "D"
                   skill = "mining"
                   required_tool = "{tool}"
                   charges = 8
                   swing_time_ms = 1000
                   respawn_seconds = 60
                   xp_per_swing = 1
                   {body}
                   [[deposit.d.yields]]
                   item = "iron_ore""#
            )
        };
        let cases = [
            ("pickaxe", "charges = 0", "charges"),
            ("pickaxe", "swing_time_ms = 0", "swing_time_ms"),
            ("pickaxe", "respawn_seconds = 0", "respawn_seconds"),
            // Jitter as wide as the window could bring a seam back instantly.
            ("pickaxe", "respawn_jitter_seconds = 60", "respawn_jitter_seconds"),
            // A stackable good is not something you can wear down.
            ("wood", "", "not a tool"),
            ("nonsense", "", "not a real item"),
        ];
        for (tool, line, want) in cases {
            let text = base(tool, line);
            let err = match CraftingConfig::parse(&text) {
                Ok(_) => panic!("`{tool}` / `{line}` should have been refused"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(want), "error for `{tool}`/`{line}` should mention `{want}`: {err}");
        }

        // A loot table that can't pay out at all.
        let err = CraftingConfig::parse(
            r#"[deposit.d]
               display_name = "D"
               skill = "mining"
               required_tool = "pickaxe"
               charges = 1
               swing_time_ms = 1
               respawn_seconds = 1
               xp_per_swing = 0
               yields = []"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no yields"), "{err}");

        // A yield that names something that doesn't exist.
        let err = CraftingConfig::parse(
            r#"[deposit.d]
               display_name = "D"
               skill = "mining"
               required_tool = "pickaxe"
               charges = 1
               swing_time_ms = 1
               respawn_seconds = 1
               xp_per_swing = 0
               [[deposit.d.yields]]
               item = "mithril""#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("mithril"), "{err}");
    }
}
