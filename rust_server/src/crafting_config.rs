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
}

impl Default for SkillCurve {
    fn default() -> Self {
        SkillCurve {
            speed_bonus_pct_per_level: 0.0,
            max_speed_bonus_pct: 0.0,
            bonus_yield_chance_per_level: 0.0,
            max_bonus_yield_chance: 0.0,
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
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CraftingConfig {
    #[serde(default)]
    pub deposit: BTreeMap<String, DepositType>,
    /// Per-skill progression curves, keyed by skill id.
    #[serde(default)]
    pub skill: BTreeMap<String, SkillCurve>,
}

impl CraftingConfig {
    pub fn deposit(&self, id: &str) -> Option<&DepositType> {
        self.deposit.get(id)
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
                Err(CraftingConfigError::Invalid { what: id.clone(), why: why.to_string() })
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
                        what: id.clone(),
                        why: format!("yields `{}`, which is not a real item", y.item),
                    });
                }
                if !(0.0..=1.0).contains(&y.chance) {
                    return Err(CraftingConfigError::Invalid {
                        what: id.clone(),
                        why: format!("`{}` has a chance outside 0.0-1.0", y.item),
                    });
                }
                if y.qty_min <= 0 || y.qty_max < y.qty_min {
                    return Err(CraftingConfigError::Invalid {
                        what: id.clone(),
                        why: format!("`{}` has a nonsense quantity range", y.item),
                    });
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
                write!(f, "crafting.toml [deposit.{what}]: {why}")
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
