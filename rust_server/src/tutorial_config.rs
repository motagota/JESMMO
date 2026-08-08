//! `tutorial.toml` — conditional NPC handouts and the tutorial track (#169).
//!
//! ## The condition language is deliberately tiny
//!
//! Five forms, and no operators, nesting or arithmetic. That is not a stage on
//! the way to something bigger — it is the whole design. The moment a condition
//! language grows expressions it becomes a thing to debug at runtime, and the
//! failure mode of a broken handout condition is either "nobody can ever get a
//! pickaxe" or "everybody can farm pickaxes forever". Both are worse than not
//! being able to express something.
//!
//! Every condition is parsed at LOAD and refuses the boot naming the offending
//! step, the same contract `market.toml` (#152) and `crafting.toml` (#166) run
//! on. A malformed condition must never reach evaluation, because a condition
//! that fails silently at runtime is exactly the infinite-pickaxe bug.
//!
//! ## Two kinds of condition, and why the split matters
//!
//! * **State** — `has_item`, `no_item`, `inventory_below`. Answerable by looking
//!   at the player right now.
//! * **History** — `gained`, `made`, `loaded_fuel`. Answerable only by having
//!   been watching.
//!
//! The history ones are counted for EVERY persistent character from the moment
//! they log in, whether or not they have ever met Marlow. That is what makes
//! "a player who completed a step before ever talking to him has it already
//! ticked" true rather than aspirational: there is no start event to miss.
//! Only items the track actually mentions are counted, so the cost is a set
//! lookup on the gather path.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

/// One condition, already parsed. The string form is never seen again after
/// load — which is the point.
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// Owns at least one, in inventory or in hand.
    HasItem(String),
    /// Owns none at all, in inventory or in hand. The pickaxe valve's core.
    NoItem(String),
    /// Owns fewer than `n`. Distinct from `NoItem` because "no ore" and "no
    /// pickaxe" want the same shape but the handout wants a threshold.
    InventoryBelow(String, i64),
    /// Has gathered `n` of this item in total, ever.
    Gained(String, i64),
    /// Has produced this item at a station at least once.
    Made(String),
    /// Has loaded fuel into any station at least once.
    LoadedFuel,
}

impl Condition {
    /// Parse one condition. Returns the reason it is bad, for a boot-refusing
    /// error that names the fault rather than the line number.
    pub fn parse(text: &str) -> Result<Condition, String> {
        let parts: Vec<&str> = text.split_whitespace().collect();
        match parts.as_slice() {
            ["has_item", item] => Ok(Condition::HasItem((*item).to_string())),
            ["no_item", item] => Ok(Condition::NoItem((*item).to_string())),
            ["inventory_below", item, n] => n
                .parse::<i64>()
                .map(|n| Condition::InventoryBelow((*item).to_string(), n))
                .map_err(|_| format!("`{n}` is not a number")),
            ["gained", item, n] => n
                .parse::<i64>()
                .map(|n| Condition::Gained((*item).to_string(), n))
                .map_err(|_| format!("`{n}` is not a number")),
            ["made", item] => Ok(Condition::Made((*item).to_string())),
            ["loaded_fuel"] => Ok(Condition::LoadedFuel),
            [] => Err("is empty".to_string()),
            [verb, ..] => Err(format!(
                "`{verb}` is not a condition. Known: has_item, no_item, \
                 inventory_below, gained, made, loaded_fuel"
            )),
        }
    }

    /// The item this condition talks about, if any — so the gather hook can
    /// count only what some condition actually cares about.
    pub fn item(&self) -> Option<&str> {
        match self {
            Condition::HasItem(i)
            | Condition::NoItem(i)
            | Condition::InventoryBelow(i, _)
            | Condition::Gained(i, _)
            | Condition::Made(i) => Some(i),
            Condition::LoadedFuel => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHandout {
    npc: String,
    item: String,
    #[serde(default = "one")]
    qty: i64,
    /// Seconds before this can fire again. Zero means no cooldown, which only
    /// makes sense alongside `once`.
    #[serde(default)]
    cooldown_secs: i64,
    /// Fires at most once ever, for the charcoal bundle.
    #[serde(default)]
    once: bool,
    #[serde(default)]
    when: Vec<String>,
    #[serde(default)]
    line: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    id: String,
    text: String,
    when: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTutorial {
    #[serde(default)]
    handout: Vec<RawHandout>,
    #[serde(default)]
    step: Vec<RawStep>,
    #[serde(default)]
    reward: Vec<RawReward>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReward {
    item: String,
    #[serde(default = "one")]
    qty: i64,
}

fn one() -> i64 {
    1
}

/// A conditional handout: an item an NPC gives when the conditions all hold.
///
/// This is a VALVE, not a gift, and the distinction is the whole reason for the
/// conditions. Bram (#160) set the contract for the sword: hand one over only
/// when the player has none at all, so losing yours is never a dead end and
/// dropping one is never a farm. The conditions generalise that — the moment a
/// player owns a pickaxe or any ore, the handout stops.
#[derive(Debug, Clone, PartialEq)]
pub struct Handout {
    pub npc: String,
    pub item: String,
    pub qty: i64,
    pub cooldown_secs: i64,
    pub once: bool,
    pub when: Vec<Condition>,
    pub line: String,
}

/// One step of the track. Completion is by DOING, never by talking.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub id: String,
    pub text: String,
    pub when: Condition,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TutorialConfig {
    pub handouts: Vec<Handout>,
    pub steps: Vec<Step>,
    pub reward: Vec<(String, i64)>,
}

impl TutorialConfig {
    /// A missing file means no handouts and no track — the world before #169,
    /// and a perfectly playable one, because nothing is gated behind the track.
    pub fn load(path: &Path) -> Result<TutorialConfig, TutorialConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => TutorialConfig::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TutorialConfig::default()),
            Err(e) => Err(TutorialConfigError::Io(e)),
        }
    }

    pub fn parse(text: &str) -> Result<TutorialConfig, TutorialConfigError> {
        let raw: RawTutorial = toml::from_str(text).map_err(TutorialConfigError::Toml)?;

        let mut handouts = Vec::new();
        for h in raw.handout {
            let bad = |why: String| TutorialConfigError::Invalid {
                what: format!("handout `{}` from `{}`", h.item, h.npc),
                why,
            };
            if crate::world::item(&h.item).is_none() {
                return Err(bad("is not a real item".to_string()));
            }
            if crate::world::npc(&h.npc).is_none() {
                return Err(bad(format!("names NPC `{}`, who does not exist", h.npc)));
            }
            if h.qty <= 0 {
                return Err(bad("hands over a non-positive quantity".to_string()));
            }
            if h.when.is_empty() {
                // An unconditional repeatable handout is an item printer. The
                // one thing this file exists to prevent.
                return Err(bad(
                    "has no conditions, so it would hand out forever".to_string(),
                ));
            }
            if !h.once && h.cooldown_secs <= 0 {
                return Err(bad(
                    "is repeatable with no cooldown — set `once` or a `cooldown_secs`".to_string(),
                ));
            }
            let mut when = Vec::new();
            for c in &h.when {
                when.push(Condition::parse(c).map_err(|why| TutorialConfigError::Invalid {
                    what: format!("handout `{}` from `{}`", h.item, h.npc),
                    why: format!("condition `{c}` {why}"),
                })?);
            }
            handouts.push(Handout {
                npc: h.npc,
                item: h.item,
                qty: h.qty,
                cooldown_secs: h.cooldown_secs,
                once: h.once,
                when,
                line: h.line,
            });
        }

        let mut steps = Vec::new();
        let mut seen = BTreeSet::new();
        for st in raw.step {
            if !seen.insert(st.id.clone()) {
                return Err(TutorialConfigError::Invalid {
                    what: format!("step `{}`", st.id),
                    why: "is a duplicate id — progress is keyed by it".to_string(),
                });
            }
            let when = Condition::parse(&st.when).map_err(|why| TutorialConfigError::Invalid {
                what: format!("step `{}`", st.id),
                why: format!("condition `{}` {why}", st.when),
            })?;
            if let Some(item) = when.item() {
                if crate::world::item(item).is_none() {
                    return Err(TutorialConfigError::Invalid {
                        what: format!("step `{}`", st.id),
                        why: format!("condition names `{item}`, which is not a real item"),
                    });
                }
            }
            steps.push(Step { id: st.id, text: st.text, when });
        }

        let mut reward = Vec::new();
        for r in raw.reward {
            if crate::world::item(&r.item).is_none() {
                return Err(TutorialConfigError::Invalid {
                    what: "reward".to_string(),
                    why: format!("`{}` is not a real item", r.item),
                });
            }
            if r.qty <= 0 {
                return Err(TutorialConfigError::Invalid {
                    what: "reward".to_string(),
                    why: format!("`{}` has a non-positive quantity", r.item),
                });
            }
            reward.push((r.item, r.qty));
        }
        if !reward.is_empty() && steps.is_empty() {
            return Err(TutorialConfigError::Invalid {
                what: "reward".to_string(),
                why: "there are no steps to complete, so it could never be earned".to_string(),
            });
        }

        Ok(TutorialConfig { handouts, steps, reward })
    }

    /// Handouts this NPC offers, in order.
    pub fn handouts_from(&self, npc_id: &str) -> Vec<&Handout> {
        self.handouts.iter().filter(|h| h.npc == npc_id).collect()
    }

    /// Every item some `gained` condition counts. The gather hook checks this
    /// set before writing anything, so items nobody is watching cost one lookup.
    pub fn counted_items(&self) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for c in self
            .steps
            .iter()
            .map(|s| &s.when)
            .chain(self.handouts.iter().flat_map(|h| &h.when))
        {
            if let Condition::Gained(item, _) = c {
                set.insert(item.clone());
            }
        }
        set
    }

    /// Every item some `made` condition watches for.
    pub fn made_items(&self) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for c in self
            .steps
            .iter()
            .map(|s| &s.when)
            .chain(self.handouts.iter().flat_map(|h| &h.when))
        {
            if let Condition::Made(item) = c {
                set.insert(item.clone());
            }
        }
        set
    }
}

#[derive(Debug)]
pub enum TutorialConfigError {
    Toml(toml::de::Error),
    Io(std::io::Error),
    Invalid { what: String, why: String },
}

impl std::fmt::Display for TutorialConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TutorialConfigError::Toml(e) => write!(f, "tutorial.toml parse error: {e}"),
            TutorialConfigError::Io(e) => write!(f, "tutorial.toml read error: {e}"),
            TutorialConfigError::Invalid { what, why } => {
                write!(f, "tutorial.toml {what}: {why}")
            }
        }
    }
}
impl std::error::Error for TutorialConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every condition form round-trips, and nonsense is refused with a reason
    /// naming what was wrong — at parse time, never at evaluation.
    #[test]
    fn conditions_parse_or_say_why_not() {
        assert_eq!(Condition::parse("has_item pickaxe"), Ok(Condition::HasItem("pickaxe".into())));
        assert_eq!(Condition::parse("no_item pickaxe"), Ok(Condition::NoItem("pickaxe".into())));
        assert_eq!(
            Condition::parse("inventory_below iron_ore 1"),
            Ok(Condition::InventoryBelow("iron_ore".into(), 1))
        );
        assert_eq!(Condition::parse("gained clay_lump 4"), Ok(Condition::Gained("clay_lump".into(), 4)));
        assert_eq!(Condition::parse("made iron_ingot"), Ok(Condition::Made("iron_ingot".into())));
        assert_eq!(Condition::parse("loaded_fuel"), Ok(Condition::LoadedFuel));

        assert!(Condition::parse("").is_err());
        assert!(Condition::parse("has_item").is_err(), "an arity error is still an error");
        assert!(Condition::parse("gained clay_lump lots").unwrap_err().contains("not a number"));
        assert!(Condition::parse("summon_dragon").unwrap_err().contains("not a condition"));
    }

    /// A repeatable handout with no conditions is an item printer, and a
    /// repeatable one with no cooldown is only slightly slower. Both refuse the
    /// boot rather than being discovered in the economy later.
    #[test]
    fn an_unconditional_or_uncooled_handout_refuses_to_boot() {
        let unconditional = r#"
            [[handout]]
            npc = "npc_quarry_foreman"
            item = "pickaxe"
        "#;
        let e = TutorialConfig::parse(unconditional).unwrap_err().to_string();
        assert!(e.contains("hand out forever"), "{e}");

        let uncooled = r#"
            [[handout]]
            npc = "npc_quarry_foreman"
            item = "pickaxe"
            when = ["no_item pickaxe"]
        "#;
        let e = TutorialConfig::parse(uncooled).unwrap_err().to_string();
        assert!(e.contains("cooldown"), "{e}");

        // ...but `once` is a legitimate reason to have no cooldown.
        let bundle = r#"
            [[handout]]
            npc = "npc_quarry_foreman"
            item = "charcoal"
            qty = 10
            once = true
            when = ["loaded_fuel"]
        "#;
        assert!(TutorialConfig::parse(bundle).is_ok());
    }

    /// A malformed condition names the step it came from. Finding out WHICH
    /// step is broken is the whole value of refusing the boot.
    #[test]
    fn a_malformed_step_condition_names_the_step() {
        let cfg = r#"
            [[step]]
            id = "mine_clay"
            text = "Mine some clay"
            when = "mine 4 clay_lump"
        "#;
        let e = TutorialConfig::parse(cfg).unwrap_err().to_string();
        assert!(e.contains("mine_clay"), "should name the step: {e}");
        assert!(e.contains("not a condition"), "and the fault: {e}");
    }

    /// Conditions are checked against the real item registry, so a typo is
    /// caught at boot rather than becoming a step nobody can ever complete.
    #[test]
    fn a_step_watching_an_imaginary_item_refuses_to_boot() {
        let cfg = r#"
            [[step]]
            id = "mine_clay"
            text = "Mine some clay"
            when = "gained clay_lumps 4"
        "#;
        let e = TutorialConfig::parse(cfg).unwrap_err().to_string();
        assert!(e.contains("clay_lumps") && e.contains("not a real item"), "{e}");
    }

    #[test]
    fn duplicate_step_ids_refuse_to_boot() {
        let cfg = r#"
            [[step]]
            id = "one"
            text = "a"
            when = "loaded_fuel"
            [[step]]
            id = "one"
            text = "b"
            when = "loaded_fuel"
        "#;
        assert!(TutorialConfig::parse(cfg).unwrap_err().to_string().contains("duplicate"));
    }

    /// A missing file is a world with no tutorial, which is a playable world —
    /// nothing is gated behind the track.
    #[test]
    fn a_missing_file_means_no_tutorial_rather_than_no_boot() {
        let cfg = TutorialConfig::load(Path::new("definitely/not/here.toml")).unwrap();
        assert!(cfg.steps.is_empty() && cfg.handouts.is_empty());
    }

    /// Only items some condition actually watches are counted, so the gather
    /// hook stays a set lookup for everything else.
    #[test]
    fn only_watched_items_are_counted() {
        let cfg = TutorialConfig::parse(
            r#"
            [[step]]
            id = "clay"
            text = "Mine 4 clay"
            when = "gained clay_lump 4"
            [[step]]
            id = "smelt"
            text = "Smelt an ingot"
            when = "made iron_ingot"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.counted_items(), ["clay_lump".to_string()].into_iter().collect());
        assert_eq!(cfg.made_items(), ["iron_ingot".to_string()].into_iter().collect());
    }

    /// The shipped file is real: it parses, its steps are ordered, and its
    /// handouts are valves rather than gifts.
    #[test]
    fn the_shipped_tutorial_is_well_formed() {
        let cfg = TutorialConfig::load(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tutorial.toml"
        )))
        .expect("the shipped tutorial.toml should load");
        assert!(!cfg.steps.is_empty(), "there should be a track");
        assert!(!cfg.handouts.is_empty(), "and handouts");

        for h in &cfg.handouts {
            assert!(!h.when.is_empty(), "`{}` must be conditional", h.item);
            assert!(h.once || h.cooldown_secs > 0, "`{}` must be rate-limited", h.item);
        }

        // The pickaxe valve specifically: it has to stop the moment the player
        // owns a pickaxe OR any ore, or it is a farm.
        let pick = cfg
            .handouts
            .iter()
            .find(|h| h.item == "pickaxe")
            .expect("Marlow should hand out a pickaxe");
        assert!(
            pick.when.contains(&Condition::NoItem("pickaxe".into())),
            "the valve must close when they already have one"
        );
        assert!(
            pick.when.iter().any(|c| matches!(c, Condition::InventoryBelow(i, _) if i == "iron_ore")),
            "...and when they already have ore"
        );
    }
}
