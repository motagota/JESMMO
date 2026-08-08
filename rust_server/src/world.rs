//! The authored Capital — Phase 1 world content (issue #4).
//!
//! The capital is **authored data**, not code that runs a simulation. It defines
//! the named districts that tile the world, the starter plot grid, and the
//! town-centre spawn anchor. Crucially this identity is keyed to *regions of the
//! world*, independent of how many zone processes back them (a busy district may be
//! split across several sims, or several districts may share one) — the gateway
//! maps a point/region to its district by geometry.
//!
//! The capital starts **empty**: this module authors the ground (district rects)
//! and the plot grid, but **no buildings and no roads**. Structures — including
//! roads — only appear as players complete build orders (commissioned at runtime
//! by the mayor, see `mayor.build_create`) and build homes (M2/M3). See
//! phase1.md §3.1-3.2.
//!
//! `WORLD_SIZE` lives here; the gateway/zone binaries import it.

/// Edge length of the (square) world, in world units (1 unit = 1 meter).
/// 25600x25600 = ~655 km²: the near-full extent of the real Brisbane DEM
/// (the v3 bake, see the repo-root `terrain.toml`) — exactly 4x the linear
/// size of the original 6400 world, so all authored coordinates scaled by 4.
pub const WORLD_SIZE: i32 = 25600;

/// A half-open rectangle of the world: `[x0, x1) x [y0, y1)`. (Mirror of the
/// gateway's private `Region`, exposed here as authored geometry.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Rect {
    pub const fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Rect { x0, y0, x1, y1 }
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
    pub fn centre(&self) -> (i32, i32) {
        ((self.x0 + self.x1) / 2, (self.y0 + self.y1) / 2)
    }
    pub fn area(&self) -> i64 {
        (self.x1 - self.x0) as i64 * (self.y1 - self.y0) as i64
    }
    /// Whether this rect shares any area with `other` (both half-open). Unlike
    /// [`Capital::district_for_region`] (which picks the *one* district a region's
    /// *centre* falls in, for labeling), this is for "does any part of this zone's
    /// region fall in this district at all" — needed when a single zone spans
    /// multiple districts (e.g. the default whole-world zone before any split).
    pub fn overlaps(&self, other: Rect) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }
}

/// Whether a district is a safe hub (no PvP / mob aggression) or open wilds. The
/// whole Phase 1 capital is `Safe`; the flag is authored here, but its *enforcement*
/// (disabling damage) lands in #5. `Wilds` exists only to reserve the concept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Safety {
    Safe,
    Wilds,
}

/// Parameters for a district's authored plot grid. World coordinates of a plot are
/// derived from these so seeding (which stores grid indices) and rendering (which
/// needs world positions) share one source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlotGrid {
    pub cols: i32,
    pub rows: i32,
    pub margin: i32, // inset from the district's top-left origin
    pub plot_w: i32,
    pub plot_h: i32,
    pub gap: i32, // spacing between adjacent plots
    pub tier: i64,
}

/// One authored plot cell: its grid indices (durably stored) plus the world-space
/// top-left it maps to (derived; handy for the client and for spawn framing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlotCell {
    pub grid_x: i32,
    pub grid_y: i32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub tier: i64,
}

impl PlotCell {
    /// This cell's world-space bounds as a [`Rect`].
    pub fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, self.x + self.w, self.y + self.h)
    }
}

/// A named district: a region of the world with an identity and (optionally) a
/// starter plot grid.
#[derive(Debug, Clone)]
pub struct District {
    pub id: &'static str,
    pub name: &'static str,
    pub region: Rect,
    pub safety: Safety,
    pub plot_grid: Option<PlotGrid>,
}

impl District {
    /// The authored plot cells for this district (empty if it has no grid).
    pub fn plots(&self) -> Vec<PlotCell> {
        let Some(g) = self.plot_grid else { return Vec::new() };
        let mut cells = Vec::with_capacity((g.cols * g.rows) as usize);
        for gy in 0..g.rows {
            for gx in 0..g.cols {
                cells.push(PlotCell {
                    grid_x: gx,
                    grid_y: gy,
                    x: self.region.x0 + g.margin + gx * (g.plot_w + g.gap),
                    y: self.region.y0 + g.margin + gy * (g.plot_h + g.gap),
                    w: g.plot_w,
                    h: g.plot_h,
                    tier: g.tier,
                });
            }
        }
        cells
    }
}

/// A seed build order: the city quests that exist the moment the capital boots.
#[derive(Debug, Clone, Copy)]
pub struct SeedBuildOrder {
    pub district: &'static str,
    pub kind: &'static str,
    pub required_json: &'static str,
    /// The build-order kind that must be `completed` before this one unlocks. `None`
    /// for orders that are open from the start; `Some(kind)` seeds this order `locked`
    /// until `kind` completes (the tech-tree edge).
    pub prereq: Option<&'static str>,
    /// The structure this order spawns on completion, and where it appears (world
    /// coords). City structures are authored here — the completed `build_order` row is
    /// their durable source of truth (no `structure` table row in Phase 1/M2).
    pub structure_kind: &'static str,
    pub structure_x: i32,
    pub structure_y: i32,
    /// Skill gate: a contributor must have levelled `required_skill` to at least
    /// `required_level` before this order accepts their contributions. `None`/0 means
    /// ungated. Distinct from `prereq` (a tech-tree edge): a skill-gated order can be
    /// `open` from boot yet show greyed until the player is skilled enough.
    pub required_skill: Option<&'static str>,
    pub required_level: i64,
}

/// A static item definition (the item registry). Gathered resources, crafted
/// goods, and build-order costs all reference these by `id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub id: &'static str,
    pub name: &'static str,
    pub stack_size: i64,
    pub category: &'static str, // "wood" | "stone" | ...
}

/// The Phase 1 item registry. Look up by id with [`item`].
pub fn items() -> Vec<Item> {
    vec![
        Item { id: "wood", name: "Wood", stack_size: 100, category: "wood" },
        Item { id: "stone", name: "Stone", stack_size: 100, category: "stone" },
        Item { id: "plank", name: "Plank", stack_size: 100, category: "crafted" },
        Item { id: "tool_kit", name: "Tool Kit", stack_size: 100, category: "crafted" },
        // Dropped by wild dogs (#157/#159) and the currency of the bounty
        // (#161). An ORDINARY stackable commodity on purpose: it carries,
        // stores, warehouses and trades like wood does, so someone who hates
        // combat can buy pelts from someone who loves it and still turn them
        // in. That emergent market costs nothing to allow — the book already
        // handles any commodity — and a bounty item that couldn't be sold would
        // be a strange exception in a game that just spent an epic on trading.
        Item { id: "dog_pelt", name: "Dog Pelt", stack_size: 100, category: "trophy" },
        // Mined at the starter mine (#164/#166). Ordinary stackable commodities,
        // so they carry, store, warehouse and trade like wood and stone — the
        // iron chain has to be able to move through the market it was built
        // alongside.
        Item { id: "iron_ore", name: "Iron Ore", stack_size: 100, category: "ore" },
        Item { id: "clay_lump", name: "Clay Lump", stack_size: 100, category: "clay" },
        // Fuel for every heat station (#167). Charcoal exists as an item before
        // the woodcutting-to-charcoal chain does: for now it is made straight
        // from wood at a home crafting structure, and Marlow hands out a bundle
        // (#169). The kiln chain that should really produce it is Phase 2.
        Item { id: "charcoal", name: "Charcoal", stack_size: 100, category: "fuel" },
        Item { id: "iron_ingot", name: "Iron Ingot", stack_size: 100, category: "metal" },
        // The game's first WEAPON (#160). stack_size 1, like the tools: it's an
        // instance with its own wear, not a stack.
        Item { id: "sword", name: "Sword", stack_size: 1, category: "weapon" },
        // Equippable tool (mining/abilities epic #123): arming one in the tool
        // slot puts the Pick ability on the hotbar. stack_size 1 — it's worn,
        // not stacked, though nothing stops carrying spares unequipped.
        Item { id: "pickaxe", name: "Pickaxe", stack_size: 1, category: "tool" },
        // Woodcutting (#122 backlog, #125): same single tool-in-hand slot as
        // the pickaxe — equipping one replaces the other.
        Item { id: "axe", name: "Axe", stack_size: 1, category: "tool" },
    ]
}

/// Look up an item definition by id.
pub fn item(id: &str) -> Option<Item> {
    items().into_iter().find(|i| i.id == id)
}

/// A crafting recipe: `inputs` (each `item_id -> qty`) consumed to produce
/// `output_qty` of `output_item`. Crafting is instant (no timer) — issue #12's
/// "can craft a basic item" acceptance needs only a couple of these.
#[derive(Debug, Clone, Copy)]
pub struct Recipe {
    pub id: &'static str,
    pub name: &'static str,
    pub inputs: &'static [(&'static str, i64)],
    pub output_item: &'static str,
    pub output_qty: i64,
}

/// The Phase 1 recipe registry. Look up by id with [`recipe`].
pub fn recipes() -> Vec<Recipe> {
    vec![
        Recipe { id: "plank", name: "Plank", inputs: &[("wood", 2)], output_item: "plank", output_qty: 2 },
        Recipe {
            id: "tool_kit", name: "Tool Kit", inputs: &[("wood", 1), ("stone", 1)],
            output_item: "tool_kit", output_qty: 1,
        },
        // Tool costs were raised by the #129 balance pass so that upkeep is a
        // real share of what a tool gathers (~27% for the pickaxe, ~17% for the
        // axe) rather than the old ~8%. The pickaxe stays the dearer of the two
        // deliberately: mining is the high-effort track, and it is priced in the
        // resource it produces so a miner funds their own upkeep.
        Recipe {
            id: "pickaxe", name: "Pickaxe", inputs: &[("wood", 3), ("stone", 5)],
            output_item: "pickaxe", output_qty: 1,
        },
        // A sword costs more than either gathering tool (#160). It has to: a
        // tool's upkeep is priced against what it gathers (#129), and a sword
        // gathers nothing — it earns pelts, and through them the bounty (#161).
        // At 30 durability it clears ~30 dogs, so ~9 units of materials stand
        // behind three bounties' worth of gold. That keeps combat's upkeep in
        // the same ~30% band #129 chose for gathering, rather than making the
        // most lucrative activity in the game the only free one.
        Recipe {
            id: "sword", name: "Sword", inputs: &[("wood", 3), ("stone", 6)],
            output_item: "sword", output_qty: 1,
        },
        Recipe {
            id: "axe", name: "Axe", inputs: &[("wood", 3), ("stone", 2)],
            output_item: "axe", output_qty: 1,
        },
        // Charcoal (#167). This is the ONLY source of fuel in the game, so it is
        // deliberately cheap and instant: a furnace whose fuel is hard to get is
        // a furnace nobody lights, and the interesting constraint is meant to be
        // the ore, not the firewood. 3 wood -> 2 charcoal, and one smelt burns 2
        // fuel, so a tree's worth of wood is a handful of ingots.
        //
        // It is an INSTANT recipe rather than a kiln job on purpose — the kiln
        // and the real charcoal chain are Phase 2, and until they exist a timed
        // fuel recipe would just be a second wait in front of the first one.
        Recipe {
            id: "charcoal", name: "Charcoal", inputs: &[("wood", 3)],
            output_item: "charcoal", output_qty: 2,
        },
    ]
}

/// Look up a recipe definition by id.
pub fn recipe(id: &str) -> Option<Recipe> {
    recipes().into_iter().find(|r| r.id == id)
}

// --- Market (epic #136, issue #139) -----------------------------------------

/// Whether an item can be traded as a **commodity** — a fungible good with an
/// order book, keyed by `item_id` alone.
///
/// `stack_size` already draws exactly the line the market needs: anything that
/// stacks is interchangeable unit-for-unit, and anything that doesn't carries
/// per-instance state (a tool's durability, #128) that makes "the price of a
/// pickaxe" meaningless. Unique items are sold individually on the listing
/// board (#142) instead, so this is also the check that keeps them off the book.
pub fn is_commodity(item_id: &str) -> bool {
    items().into_iter().any(|i| i.id == item_id && i.stack_size > 1)
}

// Order prices, size bounds, durations, order caps and **all fee rates** moved
// to `crate::market_config::MarketConfig` in #152, so a balance pass is a file
// edit rather than a rebuild (#129) and two markets can charge different rates.
// They are deliberately **not** re-exported as consts here: a `const` left in
// place would be a path by which some call site kept reading the shipped
// default while the server charged what `market.toml` said.
//
// The fee arithmetic and its anti-abuse properties (round up, never zero,
// splitting never dodges the fee) moved with them, as
// `MarketConfig::listing_fee` / `sale_tax` / `order_duration_hours` /
// `validate_order`. The doc comments there carry the reasoning.
//
// `candle_bucket` stays here: it's pure time arithmetic that takes the interval
// as an argument, so it never read a const in the first place.

/// The bucket a timestamp belongs to: the interval's opening second. Flooring
/// (not rounding) is what makes a trade land in exactly one bucket, including
/// one landing precisely on a boundary — it opens the new interval rather than
/// closing the old one.
pub fn candle_bucket(at: i64, interval_secs: i64) -> i64 {
    if interval_secs <= 0 {
        return at;
    }
    at.div_euclid(interval_secs) * interval_secs
}

/// Why an order was refused, as a stable code for the wire (#139).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderReject {
    NotACommodity,
    BadPrice,
    BadQty,
    TooManyOrders,
    RateLimited,
    CannotAffordFee,
}

impl OrderReject {
    pub fn code(self) -> &'static str {
        match self {
            OrderReject::NotACommodity => "not_a_commodity",
            OrderReject::BadPrice => "bad_price",
            OrderReject::BadQty => "bad_qty",
            OrderReject::TooManyOrders => "too_many_orders",
            OrderReject::RateLimited => "rate_limited",
            OrderReject::CannotAffordFee => "cannot_afford_fee",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            OrderReject::NotACommodity => "that item is sold on the listing board, not the book",
            OrderReject::BadPrice => "price must be a positive whole number of gold",
            OrderReject::BadQty => "order size is out of bounds",
            OrderReject::TooManyOrders => "you have too many orders resting at this market",
            OrderReject::RateLimited => "slow down — too many market commands",
            OrderReject::CannotAffordFee => "not enough gold to cover the listing fee",
        }
    }
}


// --- Equipment & abilities (mining/abilities epic #123) ---------------------
//
// A tiny slice of a bigger future system: one equipment slot ("tool") and one
// ability ("pick"). The `equipment` table is already keyed by slot, and this
// registry is already keyed by item/ability id, so a paper-doll with more
// slots and more abilities is additive later, not a rewrite.

/// The equipment slot an item can be armed into, if any. `None` means the
/// item isn't equippable at all (most items — wood, stone, plank, ...).
pub fn equippable_slot(item_id: &str) -> Option<&'static str> {
    match item_id {
        "pickaxe" | "axe" => Some("tool"),
        // The weapon slot (#160), which migration 0013 anticipated by comment
        // ("tool" | ... future: "weapon", "head") and which `equipment`'s
        // `(character_id, slot)` key has always supported.
        //
        // A SEPARATE slot from the tool on purpose: a player carries a pickaxe
        // and a sword at once. Forcing a choice would make the walk to the
        // quarry a chore rather than a risk — you'd arrive unable to mine, or
        // mine unable to defend yourself.
        "sword" => Some("weapon"),
        _ => None,
    }
}

/// An ability an equipped item grants on the hotbar. `cooldown_ms` here is
/// the item's own base speed; [`ability_cooldown_ms`] applies the wielder's
/// skill level on top before anything is sent to a client.
#[derive(Debug, Clone, Copy)]
pub struct Ability {
    pub id: &'static str,
    pub name: &'static str,
}

/// The abilities granted by having `item_id` equipped (empty for anything
/// not equippable, or equippable but ability-less).
pub fn abilities_for_item(item_id: &str) -> &'static [Ability] {
    match item_id {
        "pickaxe" => &[Ability { id: "pick", name: "Pick" }],
        "axe" => &[Ability { id: "chop", name: "Chop" }],
        _ => &[],
    }
}

/// The skill whose level scales an ability's cooldown, if any. Woodcutting
/// (#122 backlog, #125) is a dedicated skill, not a reuse of the old
/// channel-gathering "gathering" skill — same one-skill-per-ability pattern
/// mining already set.
pub fn governing_skill(ability_id: &str) -> Option<&'static str> {
    match ability_id {
        "pick" => Some("mining"),
        "chop" => Some("woodcutting"),
        _ => None,
    }
}

/// The node item an ability harvests, for abilities that target a resource
/// node at all (some future ability — a heal, say — might not).
pub fn ability_target_item(ability_id: &str) -> Option<&'static str> {
    match ability_id {
        "pick" => Some("stone"),
        "chop" => Some("wood"),
        _ => None,
    }
}

/// An ability's swing/use cooldown (ms) at a given level of its governing
/// skill (0 if ungoverned or the wielder hasn't trained it).
///
/// Each ability owns its curve, and since the #129 balance pass they DIVERGE.
/// This is the single place both the gateway (enforcement) and `equip.update`
/// (display) compute it, so the two can never disagree.
///
/// **Chop is the accessible track, Pick the high-effort one.** Woodcutting is
/// where a new player starts — the foreman is a short walk from spawn and the
/// trees ring the town centre — so it swings faster at every level. Mining
/// means a trek to the Mt Coot-tha quarry for a resource that costs more to
/// tool up for, so it swings slower and pays out in the scarcer good.
///
/// Both reach their floor around level 7 (~409 swings, ~9 minutes of swinging)
/// rather than the old level 10 / 834 swings / ~21 minutes. The old curve's
/// payoff sat beyond a single session, which is a long time to wait to feel a
/// skill improve; the steeper slope puts it inside one.
pub fn ability_cooldown_ms(ability_id: &str, skill_level: i64) -> i64 {
    match ability_id {
        // 1800ms at level 0, -120ms per level, floors at 1000ms (level 7+).
        "chop" => (1800 - 120 * skill_level).max(1000),
        // Slower at every level than Chop, and floors higher: mining is the
        // deliberate high-effort track (#129).
        "pick" => (2200 - 120 * skill_level).max(1300),
        _ => 1000,
    }
}

/// XP for a swing, scaled down once the swinger has outgrown what they are
/// swinging at (#166).
///
/// Above `falloff_level` the award decays toward zero rather than stopping
/// dead. That is the whole mechanism that keeps the tutorial mine from being a
/// viable place to grind Mining to cap **without ever locking a newcomer out of
/// it** — a level gate would do the first thing and the second, and the second
/// is the one that matters for a starter area.
///
/// Decay is linear over the same span again, so a source with a falloff of 15
/// is worthless by level 30. Never negative.
pub fn xp_with_falloff(base: i64, level: i64, falloff_level: i64) -> i64 {
    if falloff_level <= 0 || level <= falloff_level {
        return base.max(0);
    }
    let over = level - falloff_level;
    if over >= falloff_level {
        return 0;
    }
    let remaining = falloff_level - over;
    ((base * remaining) / falloff_level).max(0)
}

/// Mining-skill xp per successful swing per ability (mining/abilities epic
/// #123; generalized in #125 when Chop joined Pick — a per-ability table
/// rather than one hardcoded constant in the zone). Same rate for both:
/// a swing is a swing, instant rather than the old multi-tick channel's
/// per-unit yield (which paid out at 10/unit).
pub fn ability_xp_per_swing(ability_id: &str) -> i64 {
    match ability_id {
        "pick" | "chop" => 12,
        _ => 0,
    }
}

// --- Tool durability & repair (mining/abilities epic #123 backlog, #128) ------

/// Max durability (swings before it breaks) for an equippable tool, or
/// `None` for anything that isn't one — also doubles as "is this item
/// instanced rather than stacked" (see `persistence::add_inventory_in_tx`).
///
/// Cut from 50 to 30 by the #129 balance pass, together with a rise in recipe
/// costs. At 50 durability a tool returned ~92% profit over its life and paid
/// back its own-resource cost in 3 of 50 swings — durability existed
/// mechanically (#128) but had no economic weight at all. Upkeep is now roughly
/// a quarter of what a tool gathers, which makes replacing one a decision and
/// gives the crafting loop and the market something real to trade.
pub fn tool_max_durability(item_id: &str) -> Option<i64> {
    match item_id {
        "pickaxe" | "axe" => Some(30),
        // A sword wears on swings that CONNECT (#160), so 30 is ~30 kills —
        // three bounties' worth. Same number as the tools deliberately: there
        // is no reason for equipment to age at different rates, and one number
        // is one thing to tune.
        "sword" => Some(30),
        _ => None,
    }
}

/// The tool item an ability wears down on a successful swing — the inverse
/// of [`abilities_for_item`]. `None` for an ability with no tool of its own
/// (shouldn't happen for a harvesting ability today, but keeps the mapping
/// honest for whatever comes next).
pub fn governing_tool(ability_id: &str) -> Option<&'static str> {
    match ability_id {
        "pick" => Some("pickaxe"),
        "chop" => Some("axe"),
        _ => None,
    }
}

/// Repair cost for a tool missing `missing` of its `max` durability: each of
/// the craft recipe's ingredients, scaled by the fraction worn away and rounded
/// up, with a minimum of 1 of each so a token repair is never free. `None` if
/// `item_id` has no matching recipe (shouldn't happen for a real tool) or
/// nothing is actually missing.
///
/// **Repair is always cheaper than crafting new, right up until the tool is
/// entirely spent.** It used to bucket `missing` into 10-durability chunks,
/// which made the cost saturate at the full recipe from 40/50 worn onward — so
/// repairing was strictly pointless exactly when a tool most needed it, and the
/// top half of the curve was dead. Scaling directly on `missing / max` is both
/// simpler and monotone: it only reaches the full craft cost at 100% worn,
/// which is the one point where "just make a new one" is genuinely the same
/// deal. That matters more since #129 made upkeep a real cost — repair is the
/// decision the wear system is supposed to pose, and a dominated option is not
/// a decision.
pub fn repair_cost(item_id: &str, missing: i64, max: i64) -> Option<Vec<(&'static str, i64)>> {
    if missing <= 0 || max <= 0 {
        return None;
    }
    let recipe = recipes().into_iter().find(|r| r.output_item == item_id)?;
    let worn = missing.min(max);
    Some(
        recipe
            .inputs
            .iter()
            .map(|(ingredient, full_qty)| {
                // Rounds DOWN, floored at 1. Down rather than up is what makes
                // "repair beats a fresh craft" provable rather than merely
                // usually-true: `floor(q * worn / max) < q` for every `worn <
                // max`, so every ingredient — and therefore the total — is
                // strictly under the full recipe until the tool is completely
                // spent, at which point it lands exactly on it.
                let cost = (full_qty * worn) / max;
                (*ingredient, cost.max(1))
            })
            .collect(),
    )
}

/// Fixed footprint (world units) for a home structure kind, used both by
/// placement validation (bounds/overlap) and the client's ghost preview. `None`
/// for anything that isn't a placeable home structure (#12).
pub fn structure_footprint(kind: &str) -> Option<(i32, i32)> {
    match kind {
        "bed" => Some((20, 20)),
        "storage" => Some((16, 16)),
        "crafting" => Some((20, 20)),
        _ => None,
    }
}

/// An authored hostile creature (wild dogs epic #157, issue #158).
///
/// Mobs were anonymous until now: `spawn_mobs` scattered a fixed count at random
/// points inside whatever region a zone happened to own, every one identical and
/// nameless. That made them pure ambience — and made "count your dog kills"
/// impossible, because there was no such thing as a dog.
///
/// Authored spawns are the other half of that split, and it is a deliberate one:
///
/// * **Authored mobs are CONTENT.** Fixed place, named species, and they come
///   back where they were put — so a pack is a landmark you can return to and a
///   bounty can send you somewhere specific.
/// * **Ambient mobs stay TERRITORY.** Random placement, no species, and they are
///   what the capture bar counts. Authored dogs deliberately do NOT block a zone
///   capture or suppress the ambient top-up, or parking six of them in a region
///   would quietly break territory control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MobSpawn {
    pub id: &'static str,
    pub district: &'static str,
    /// What kind of creature. Rides the wire so a client can name and draw it.
    pub species: &'static str,
    pub x: i32,
    pub y: i32,
}

/// An authored gatherable node: a fixed spawn that yields `item_id` until its
/// `qty` is exhausted, then respawns. Node *runtime* state (current qty, respawn
/// timer) is cache-only in the owning zone; this is just the authored spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceNodeSpawn {
    pub id: &'static str,
    pub district: &'static str,
    pub item_id: &'static str,
    pub x: i32,
    pub y: i32,
    pub qty: i64,
}

/// An authored storage access point — a place a player can stand near to deposit
/// to / withdraw from the safe home stash. For M2 this is a public town storehouse;
/// in M3 (#12) per-plot home `storage` structures become additional storage points
/// using the same protocol and server ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoragePoint {
    pub id: &'static str,
    pub district: &'static str,
    pub x: i32,
    pub y: i32,
}

/// An authored build-order board — a place a player stands near to contribute to the
/// district's city build orders. For M2 there is one at the town centre; more can be
/// authored per district later. Synced to clients as a `build_board` entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildBoard {
    pub id: &'static str,
    pub district: &'static str,
    pub x: i32,
    pub y: i32,
}

/// An authored NPC: a fixed, always-present entity a player can talk to
/// (mining/abilities epic #123, #118). Never moves, never despawns — unlike
/// a resource node there's no runtime state to track, so the zone just
/// spawns this directly with no cache-only wrapper.
///
/// `grants_item`/`lines_granted`/`lines_repeat` (#126) make every NPC's
/// "safety net, not a farm" hand-out fully data-driven — `apply_npc_interact`
/// in proxy.rs reads these instead of hardcoding a specific NPC id/item, so
/// a third NPC later needs zero gateway changes, just a new entry here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NpcSpawn {
    pub id: &'static str,
    pub name: &'static str,
    pub district: &'static str,
    pub x: i32,
    pub y: i32,
    /// The item this NPC hands over the first time (and any time since —
    /// it's a safety net, not a farm) a talker has none at all, owned or
    /// equipped. `None` for an NPC that only ever talks.
    pub grants_item: Option<&'static str>,
    /// Dialogue when a talk actually granted something.
    pub lines_granted: &'static [&'static str],
    /// Dialogue on any other talk (already has one, or nothing to grant).
    pub lines_repeat: &'static [&'static str],
}

/// The Phase 1 NPC registry. Look up by id with [`npc`].
pub fn npc_spawns() -> Vec<NpcSpawn> {
    vec![
        // The quarry foreman: stands just clear of the working face (which
        // spans x 8210-8255, y 13900-13930) so he doesn't collide with the
        // rock nodes themselves, but close enough that "walk up and talk"
        // reads as part of the same site.
        NpcSpawn {
            id: "npc_quarry_foreman", name: "Sten", district: "civic", x: 8232, y: 13945,
            grants_item: Some("pickaxe"),
            lines_granted: &[
                "No pick? Take mine, and mind the edge.",
                "Arm it, stand at the face, and swing — [1] once it's in your hand.",
            ],
            lines_repeat: &[
                "Keep that pick on the rock, not the ground between swings.",
                "Practice sharpens more than the edge — you'll swing faster with time.",
            ],
        },
        // The logging camp foreman: a small starter grove a short walk NE of
        // the town centre (#126) — distinct site and direction from the
        // quarry, so a fresh character has two nearby, unmissable bootstrap
        // loops instead of one.
        NpcSpawn {
            id: "npc_logging_foreman", name: "Elke", district: "civic", x: 14300, y: 11400,
            grants_item: Some("axe"),
            lines_granted: &[
                "No axe? Take this one, and watch your footing on the roots.",
                "Arm it, stand by a trunk, and swing — [1] once it's in your hand.",
            ],
            lines_repeat: &[
                "Let the axe do the work — don't force the swing.",
                "Practice sharpens more than the edge — you'll swing faster with time.",
            ],
        },
        // The weapon master (#160). Same "safety net, not a farm" contract the
        // foremen have: he hands over a blade only when you have none at all, so
        // losing yours is never a dead end and dropping one is never a farm.
        NpcSpawn {
            id: "npc_weapon_master",
            name: "Bram",
            district: "civic",
            x: WEAPON_MASTER_AT.0,
            y: WEAPON_MASTER_AT.1,
            grants_item: Some("sword"),
            lines_granted: &[
                "Unarmed, and headed that way? Take this. It's plain, but it bites.",
                "There's a pack west of here that's been at the herds. Bring me ten pelts.",
            ],
            lines_repeat: &[
                "Ten pelts is the standing price. The dogs keep coming back.",
                "Mind the pack — take them one at a time and you'll keep your skin.",
            ],
        },
    ]
}

/// Look up an authored NPC by id.
pub fn npc(id: &str) -> Option<NpcSpawn> {
    npc_spawns().into_iter().find(|n| n.id == id)
}

/// Grid resolution the server samples the loaded terrain artifact at to
/// build the `terrain.data` wire message — decoupled from the baked
/// artifact's own internal tile/cell resolution (see [`loaded_terrain`]'s
/// doc comment for why that decoupling is the whole point). Mirrored by the
/// client's ground mesh — see `docs/protocol.md`'s `terrain.*` section.
///
/// 384 keeps the backdrop at ~66m per cell on the 25600 world — twice the
/// per-cell fidelity the original 6400 world had at 48 — so distant terrain
/// (everything beyond the streamed fine-tile ring) reads as real hills
/// with ridgelines, and together with the client's distance fog the
/// fine-to-coarse transition stops reading as "leftover placeholder
/// terrain". The one-time `terrain.data` message grows to (384+1)² ≈ 148k
/// height samples (~1.5MB of JSON), still a single push at session start.
pub const TERRAIN_RESOLUTION: i32 = 384;

/// Where the baked terrain artifact (issue #56's terrain pipeline; produced
/// by `terrain-bake`, see the repo-root `terrain.toml`) lives, unless
/// overridden by `TERRAIN_DATA_DIR`. Resolved at compile time relative to
/// this crate's own manifest directory so it doesn't depend on the process's
/// current working directory (which varies: the README's own instructions
/// run the server from inside `rust_server/`, but a workspace-wide `cargo
/// run -p proxy` from the repo root works too).
///
/// `world_v3`: the near-full-extent 25.6km Brisbane bake (1600 tiles) —
/// materially different from `world_v2`'s 6.4km crop (16x the area, plot
/// field moved to the west band), hence the new directory name rather than
/// overwriting `world_v2` in place.
const DEFAULT_TERRAIN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../artifacts/world_v3");

static TERRAIN: std::sync::OnceLock<std::sync::Arc<terrain_common::Terrain>> = std::sync::OnceLock::new();

/// Load the baked terrain artifact once per process (subsequent calls clone
/// the cheap `Arc`, not the underlying tiles) and cache it.
///
/// This *is* the fix for the old client/server terrain mismatch class of bug
/// (#54): `sample_height` here is the exact same code path, reading the
/// exact same artifact, that the bake tool's own tests validate — there's no
/// second, independently-computed heightmap to disagree with. The wire
/// format sent to clients (`terrain.data`, see `proxy.rs::send_terrain`)
/// stays a flat `(TERRAIN_RESOLUTION+1)^2` grid exactly like the old
/// synthetic generator sent — deliberately decoupled from the artifact's own
/// internal resolution, so this flat backdrop grid never has to grow just
/// because the bake gets more detailed; it only changes what `sample_height`
/// returns at the same fixed sampling grid. This backdrop is the permanent,
/// zero-latency fallback terrain now that real high-resolution terrain
/// streaming exists (`terrain.tile_request`/`terrain.tile_data`,
/// `proxy.rs::send_terrain_tile`) — the client renders this coarse grid
/// everywhere, and layers genuinely native-resolution tiles on top near the
/// player, streamed in/out as they move (see
/// `client_godot/world/TerrainStreamer.gd`).
///
/// **Terrain-editing caveat (#72/#80)**: this is the immutable *base*.
/// Hand-authored edits live as deltas in the `terrain_delta` table and are
/// composited separately (`Terrain::sample_height_with_delta`; the client
/// composites onto streamed chunks). If you add a server-side gameplay
/// consumer of ground height, do **not** call `sample_height` directly --
/// go through `proxy.rs::composited_ground_height` (or replicate its
/// base-plus-delta composition), or edited terrain will be invisible to
/// your feature. The #80 audit confirmed no such consumer exists today.
pub fn loaded_terrain() -> std::sync::Arc<terrain_common::Terrain> {
    TERRAIN
        .get_or_init(|| {
            let dir = std::env::var("TERRAIN_DATA_DIR").unwrap_or_else(|_| DEFAULT_TERRAIN_DIR.to_string());
            let terrain = terrain_common::Terrain::load_dir(std::path::Path::new(&dir)).unwrap_or_else(|e| {
                panic!(
                    "failed to load terrain artifact from {dir} ({e}) — run `cargo run -p terrain-bake -- \
                     --config terrain.toml` from the repo root to (re)generate it, or set TERRAIN_DATA_DIR"
                )
            });
            std::sync::Arc::new(terrain)
        })
        .clone()
}

/// The district ids [`capital`] authors, without building one.
///
/// Exists because `market.toml`'s `[districts.<id>]` tables have to be
/// validated at boot (#152) and `capital()` loads the whole baked terrain
/// artifact — far too much work to answer "is `civics` a real district?". Kept
/// honest by `district_ids_match_the_authored_capital`, so this can't drift into
/// rejecting a district that exists.
pub const CAPITAL_DISTRICT_IDS: &[&str] =
    &["market", "suburbs", "civic", "craftworks", "old_quarter"];

/// [`CAPITAL_DISTRICT_IDS`] as a `Vec`, for error messages that list them.
pub fn capital_district_ids() -> Vec<&'static str> {
    CAPITAL_DISTRICT_IDS.to_vec()
}

/// The whole authored capital.
#[derive(Debug, Clone)]
pub struct Capital {
    pub districts: Vec<District>,
    pub town_centre: (i32, i32),
    pub build_orders: Vec<SeedBuildOrder>,
    pub resource_nodes: Vec<ResourceNodeSpawn>,
    pub storage_points: Vec<StoragePoint>,
    pub build_boards: Vec<BuildBoard>,
    pub npcs: Vec<NpcSpawn>,
    /// Authored hostile creatures (#158). See [`MobSpawn`] for why these are
    /// separate from the zone's ambient mob population.
    pub mobs: Vec<MobSpawn>,
    /// Authoritative heights — loaded once from the baked artifact (issue
    /// #63), not generated in-process. See [`loaded_terrain`].
    pub terrain: std::sync::Arc<terrain_common::Terrain>,
}

impl Capital {
    /// The district that owns a world point, if any.
    pub fn district_at(&self, x: i32, y: i32) -> Option<&District> {
        self.districts.iter().find(|d| d.region.contains(x, y))
    }

    /// The district that best owns a region — looked up by the region's centre, so
    /// district identity survives the gateway splitting/merging the sim shards.
    pub fn district_for_region(&self, r: Rect) -> Option<&District> {
        let (cx, cy) = r.centre();
        self.district_at(cx, cy)
    }

    /// Every authored starter plot across all districts, in (district_id, cell) form.
    pub fn starter_plots(&self) -> Vec<(&'static str, PlotCell)> {
        self.districts
            .iter()
            .flat_map(|d| d.plots().into_iter().map(move |c| (d.id, c)))
            .collect()
    }

    /// Authored plot cells whose world-space rect falls inside `r` — the set a
    /// zone owning that region should know about, purely as *geometry* (not
    /// ownership — that's the gateway/DB's job), so it can gate "is this point on
    /// some plot" for home-structure placement/crafting (#12).
    pub fn plots_in(&self, r: Rect) -> Vec<(&'static str, PlotCell)> {
        self.starter_plots()
            .into_iter()
            .filter(|(_, c)| r.contains(c.x, c.y) && r.contains(c.x + c.w - 1, c.y + c.h - 1))
            .collect()
    }

    /// Authored resource nodes whose position falls inside `r` — the set a zone
    /// owning that region should spawn and simulate.
    pub fn resource_nodes_in(&self, r: Rect) -> Vec<ResourceNodeSpawn> {
        self.resource_nodes
            .iter()
            .copied()
            .filter(|n| r.contains(n.x, n.y))
            .collect()
    }

    /// Authored storage points whose position falls inside `r`.
    pub fn storage_points_in(&self, r: Rect) -> Vec<StoragePoint> {
        self.storage_points
            .iter()
            .copied()
            .filter(|s| r.contains(s.x, s.y))
            .collect()
    }

    /// Authored build boards whose position falls inside `r` — the set a zone owning
    /// that region should spawn and gate `build.contribute` proximity against.
    pub fn build_boards_in(&self, r: Rect) -> Vec<BuildBoard> {
        self.build_boards
            .iter()
            .copied()
            .filter(|b| r.contains(b.x, b.y))
            .collect()
    }

    /// Authored NPCs whose position falls inside `r` — the set a zone owning
    /// that region should spawn and gate `npc.talk` proximity against.
    pub fn npcs_in(&self, r: Rect) -> Vec<NpcSpawn> {
        self.npcs.iter().copied().filter(|n| r.contains(n.x, n.y)).collect()
    }

    /// The authored mobs inside `r` (#158). Filtered by region exactly like
    /// [`Capital::resource_nodes_in`], so a zone split re-derives the creatures
    /// it now owns rather than duplicating them into both halves or losing them
    /// from both.
    pub fn mobs_in(&self, r: Rect) -> Vec<MobSpawn> {
        self.mobs.iter().copied().filter(|m| r.contains(m.x, m.y)).collect()
    }
}

/// Where the weapon master stands (#160): 250 units from the town centre, on
/// the way south-west toward the pack.
///
/// Sited so a player meets him BEFORE the dogs — he's a short walk from spawn,
/// and roughly on the line to the pack rather than past it. Crucially he is 506
/// units from the nearest dog, comfortably outside their 430-unit threat radius
/// (`the_pack_cannot_reach_the_town_centre` enforces that for every NPC), so the
/// man who hands out swords is never himself being mauled.
pub const WEAPON_MASTER_AT: (i32, i32) = (12600, 12650);

/// How close a player must stand to the weapon master to claim the bounty
/// (#161).
///
/// Deliberately looser than the zone's 10-unit `NPC_TALK_RANGE`. The gateway
/// gates this from its own position cache, which lags the zone's authoritative
/// position by up to a movement tick — matching the talk radius exactly would
/// randomly refuse a player who is plainly standing there. Still unmistakably
/// "with him" rather than "in the district", and the same shape as the market's
/// own range gate.
pub const BOUNTY_RANGE: i32 = 40;

/// Species id for the wild dogs (#158). A string rather than an enum so the
/// registry, the wire and the client all name the same thing without three
/// definitions to keep in step.
pub const SPECIES_WILD_DOG: &str = "wild_dog";

/// What a creature leaves behind when killed (#159), or `None` for one that
/// drops nothing.
///
/// Only AUTHORED creatures drop. Ambient mobs deliberately don't: the bounty
/// should send you to the authored pack, and a dropping ambient mob would turn
/// every zone in the world into a farm — which is also why they stay
/// speciesless (#158), since this looks up by species.
pub fn creature_drop(species: &str) -> Option<&'static str> {
    match species {
        SPECIES_WILD_DOG => Some("dog_pelt"),
        _ => None,
    }
}

/// How far an authored creature may stray from where it was authored (#158).
///
/// Authored siting is a set of promises — dry ground, level footing, clear of
/// every other interaction, reachable on foot — and **every one of them is about
/// a place, not a creature**. Mob wander is 2 units/tick at 20Hz, so within a
/// minute an unleashed dog drifts over a thousand units and takes none of those
/// promises with it: into the river, onto a resource node, into somebody's
/// gather prompt. (Found live, exactly that way, with the pack scattered up to
/// 1250 units from its anchor.)
///
/// So authored creatures are leashed: past this radius they stop whatever they
/// were doing — including a chase — and walk home. `the_wild_dog_pack_is_sited_
/// where_a_new_player_will_meet_it` requires every clearance to exceed this,
/// which is what makes "a dog can never wander onto something else" true by
/// construction rather than by luck.
pub const AUTHORED_MOB_LEASH: i32 = 250;

/// Melee damage per connecting swing with nothing in hand.
///
/// Unarmed combat stays exactly what it always was — a player between swords
/// still has to be able to defend themselves, and several tests predate weapons
/// entirely.
pub const MELEE_DAMAGE_BARE: i64 = 20;

/// Melee damage for the equipped weapon in the `weapon` slot, or
/// [`MELEE_DAMAGE_BARE`] with an empty slot (#160).
///
/// **A sword one-shots a wild dog** (40 hp), where bare hands need two swings.
/// That is the whole point of the number: at anything between 21 and 39 the
/// swing count against the game's only creature is unchanged, so the weapon
/// would be a stat with no consequence. Halving time-to-kill also halves the
/// damage taken clearing the pack, which is the difference between the sword
/// being a nice-to-have and being the thing that makes the encounter yours.
///
/// Raw damage rather than a combat ABILITY, deliberately. Pick and Chop are
/// abilities because harvesting needed a targeted, cooldown-gated action built
/// from nothing; melee already exists as its own path with its own arc, reach
/// and cooldown. Rebuilding it on the ability system would be a large change to
/// combat, and #160's job is to make the weapon slot exist — not to rewrite how
/// swinging works.
pub fn melee_damage(weapon_item: Option<&str>) -> i64 {
    match weapon_item {
        Some("sword") => 40,
        _ => MELEE_DAMAGE_BARE,
    }
}

/// The authored wild-dog pack (#158): five dogs on the flat ground southwest of
/// the town centre.
///
/// Site (12100, 12400) was surveyed against the bake rather than picked by eye —
/// dry across the pack's whole footprint, level to within ~5m, and reachable
/// from spawn without crossing water (drowning is real, #83). It sits **806
/// units from the town centre**, which is the number that matters most here:
/// aggro is 180, so a fresh spawn is never harassed at the storehouse, but the
/// pack is a short deliberate walk rather than an expedition. It is also 200+
/// units clear of every resource node, NPC, storage point and build site, so a
/// dog fight never happens on top of another interaction's panel.
///
/// Southwest, deliberately: the market is east (12900, 12800), the logging
/// foreman northeast, and the quarry far west — this is the one quadrant near
/// spawn with nothing else in it.
fn mob_spawns() -> Vec<MobSpawn> {
    const PACK_X: i32 = 12100;
    const PACK_Y: i32 = 12400;
    // Loosely clustered so they read as a pack rather than a firing line, and
    // far enough apart that pulling one doesn't necessarily pull all five.
    [(0, 0), (-45, -30), (50, -25), (-30, 45), (40, 40)]
        .iter()
        .enumerate()
        .map(|(i, (dx, dy))| MobSpawn {
            id: match i {
                0 => "mob_dog_0",
                1 => "mob_dog_1",
                2 => "mob_dog_2",
                3 => "mob_dog_3",
                _ => "mob_dog_4",
            },
            district: "civic",
            species: SPECIES_WILD_DOG,
            x: PACK_X + dx,
            y: PACK_Y + dy,
        })
        .collect()
}

/// The Phase 1 capital: five named districts tiling the 25600x25600 (~655 km²)
/// world in a plus/cross layout — a central Civic Centre with Suburbs/Market
/// bands to its west/east and Craftworks/Old Quarter bands to its north/south —
/// a starter plot grid in the suburbs, a town-centre spawn at the world centre,
/// and a civic build board. No roads and no build orders are authored; both
/// start empty and are built at runtime (roads via mayor-issued build orders).
///
/// The suburbs (and their starter-plot field) sit in the WEST band: in the v3
/// full-extent Brisbane bake the east band reaches the river mouth and Moreton
/// Bay (flat sea-filled ground), while the west band is real inland hillside —
/// the bake's `capital_flatten_mask` placement (see `terrain.toml`) matches.
pub fn capital() -> Capital {
    // A plus/cross tiling: west/east bands span the full height; the middle column
    // (between them) splits into north/centre/south. Exact tiling, verified in
    // `districts_tile_the_world_without_gaps_or_overlap`.
    let side = WORLD_SIZE / 4; // 6400 — west/east band width, north/south band height
    let mid0 = side; // 6400
    let mid1 = WORLD_SIZE - side; // 19200

    let market = District {
        id: "market",
        name: "Market District",
        region: Rect::new(mid1, 0, WORLD_SIZE, WORLD_SIZE),
        safety: Safety::Safe,
        plot_grid: None,
    };
    let suburbs = District {
        id: "suburbs",
        name: "Starter Suburbs",
        region: Rect::new(0, 0, side, WORLD_SIZE),
        safety: Safety::Safe,
        // A generous starter grid: 12 columns x 20 rows = 240 plots — plots stay
        // scarce/premium, not an attempt at the design doc's long-term ~100k-plot
        // figure. plot 80 + gap 40 -> 120 per cell; 12 cols span 1400 < 6400,
        // 20 rows span 2360 < 25600. Anchored at the band's top-left (NW corner
        // of the world) — the terrain there is real, bake-flattened hillside.
        plot_grid: Some(PlotGrid {
            cols: 12,
            rows: 20,
            margin: 40,
            plot_w: 80,
            plot_h: 80,
            gap: 40,
            tier: 0,
        }),
    };
    let civic = District {
        id: "civic",
        name: "Civic Centre",
        region: Rect::new(mid0, mid0, mid1, mid1),
        safety: Safety::Safe,
        plot_grid: None,
    };
    let craftworks = District {
        id: "craftworks",
        name: "Craftworks Quarter",
        region: Rect::new(mid0, 0, mid1, side),
        safety: Safety::Safe,
        plot_grid: None,
    };
    let old_quarter = District {
        id: "old_quarter",
        name: "Old Quarter",
        region: Rect::new(mid0, mid1, mid1, WORLD_SIZE),
        safety: Safety::Safe,
        plot_grid: None,
    };

    // Terrain (heights) is loaded from the baked artifact (issue #63), not
    // generated here — including the suburbs plot field's flattening (#55),
    // which is now authored once at bake time via the checked-in
    // `capital_flatten_mask` rather than computed on every boot (see the
    // repo-root `terrain.toml` and `loaded_terrain`'s doc comment).
    let terrain = loaded_terrain();

    // Town centre at the world centre, inside the Civic Centre band. This is the
    // spawn anchor and where the first build-order board lives.
    let town_centre = (WORLD_SIZE / 2, WORLD_SIZE / 2);
    let (tcx, tcy) = town_centre;

    // Authored build orders. The capital had none for a long time — city work
    // (starting with dirt paths) is commissioned at runtime by the mayor via
    // `mayor.build_create`. The Market (market epic #136, issue #137) is the
    // first authored one: it must exist as a fixed, findable place before any
    // trading can hang off it, and it's deliberately player-BUILT rather than
    // pre-placed, so a fresh server's first market is a community effort.
    //
    // Sited 100m east of the town centre: a short walk from spawn, and clear
    // of the storehouse (12830, 12810) and build board (12770, 12810) by more
    // than their interaction ranges, so their panels don't all stack open at
    // once.
    // The second market (market phase 2 epic #151, issue #153) sits in the
    // **Market District** — the east band, which has carried that name since the
    // districts were authored and until now contained no market.
    //
    // Every market table has been keyed by `market_id` since #137, and
    // `market_at` resolves by district + range, so this needed no new
    // machinery. What it adds is *gameplay*: warehouses are per-market, so
    // deposited stock does not follow a player. Two markets means two books,
    // two supplies, two prices — and moving goods between them means carrying
    // them 8.6km. That gap is the arbitrage the design doc treats as core
    // content, and the topology creates it rather than a mechanic.
    //
    // Site (20800, 9600) was chosen by surveying the district against the bake
    // rather than by eye: 4.3m elevation with 0.29m of spread across a 120m
    // footprint (flat, and clear of the water mask — the east band reaches the
    // river mouth and Moreton Bay, so "somewhere east" is not automatically
    // land), 1600 units inside the band's western edge so a trader standing at
    // it is unambiguously in this district, ~2.9km from the nearest resource
    // node, and reachable from spawn without crossing water. Pinned by
    // `the_second_market_is_sited_on_dry_reachable_ground`.
    //
    // `prereq` makes it the REWARD for finishing the capital's market rather
    // than a parallel option that would split a small player base's effort
    // across two 80-unit builds. Cost is deliberately IDENTICAL to the first:
    // the difficulty here is the 8.6km haul (at `MAX_CARRY` 50 that is three
    // round trips), and charging more on top of that would be punishing rather
    // than interesting.
    let build_orders: Vec<SeedBuildOrder> = vec![
        SeedBuildOrder {
            district: "civic",
            kind: "market",
            required_json: r#"{"wood":50,"stone":30}"#,
            prereq: None,
            structure_kind: "market",
            structure_x: tcx + 100,
            structure_y: tcy,
            required_skill: None,
            required_level: 0,
        },
        SeedBuildOrder {
            district: "market",
            // A distinct `kind` — the prereq edge is keyed on kind, and the
            // build board renders it raw, so a second order also called
            // "market" would be ambiguous in both. `structure_kind` stays
            // "market", which is what `market_at` and the client's
            // `nearest_market` actually match on.
            kind: "market_east",
            required_json: r#"{"wood":50,"stone":30}"#,
            prereq: Some("market"),
            structure_kind: "market",
            structure_x: 20800,
            structure_y: 9600,
            required_skill: None,
            required_level: 0,
        },
    ];

    // Gatherable nodes. A grove of trees ringing the town centre (so a fresh
    // spawn finds wood immediately) plus wood/stone spread through every
    // district's now much larger footprint. Ids are stable so a node keeps its
    // identity across respawns.
    let resource_nodes = vec![
        ResourceNodeSpawn { id: "node_civic_tree_0", district: "civic", item_id: "wood", x: tcx - 60, y: tcy - 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_tree_1", district: "civic", item_id: "wood", x: tcx + 60, y: tcy - 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_tree_2", district: "civic", item_id: "wood", x: tcx - 60, y: tcy + 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_tree_3", district: "civic", item_id: "wood", x: tcx + 60, y: tcy + 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_rock_0", district: "civic", item_id: "stone", x: tcx, y: tcy - 110, qty: 5 },
        ResourceNodeSpawn { id: "node_market_tree_0", district: "market", item_id: "wood", x: 20800, y: 2800, qty: 5 },
        ResourceNodeSpawn { id: "node_market_tree_1", district: "market", item_id: "wood", x: 23600, y: 8800, qty: 5 },
        ResourceNodeSpawn { id: "node_market_rock_0", district: "market", item_id: "stone", x: 21600, y: 14400, qty: 5 },
        ResourceNodeSpawn { id: "node_market_tree_2", district: "market", item_id: "wood", x: 24000, y: 20000, qty: 5 },
        // (20400, 24400) drowned when #84's real water mask landed — the map's
        // SE corner is genuinely Moreton Bay. Relocated to the nearest dry
        // market-district ground: the Toohey-forest hillside at the band's
        // west edge (h ~140m — a rock node on a hill reads fine).
        ResourceNodeSpawn { id: "node_market_rock_1", district: "market", item_id: "stone", x: 19300, y: 22600, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_tree_0", district: "suburbs", item_id: "wood", x: 1600, y: 3200, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_tree_1", district: "suburbs", item_id: "wood", x: 4000, y: 9600, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_rock_0", district: "suburbs", item_id: "stone", x: 2400, y: 16000, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_tree_2", district: "suburbs", item_id: "wood", x: 4800, y: 20800, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_rock_1", district: "suburbs", item_id: "stone", x: 1200, y: 24000, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_tree_0", district: "craftworks", item_id: "wood", x: 8000, y: 1600, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_rock_0", district: "craftworks", item_id: "stone", x: 12800, y: 3600, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_tree_1", district: "craftworks", item_id: "wood", x: 17600, y: 2000, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_rock_1", district: "craftworks", item_id: "stone", x: 11200, y: 5200, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_tree_0", district: "old_quarter", item_id: "wood", x: 8000, y: 20800, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_rock_0", district: "old_quarter", item_id: "stone", x: 12800, y: 23600, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_tree_1", district: "old_quarter", item_id: "wood", x: 17600, y: 21600, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_rock_1", district: "old_quarter", item_id: "stone", x: 11200, y: 24400, qty: 5 },
        // The QUARRY (roads & quarry epic #93, #97; relocated in #99): the
        // pen's stone-economy anchor — a working face of eight rich stone
        // nodes on MT COOT-THA's east flank. The bake is real Brisbane, and
        // the 281m summit at world (6800, 14000) IS Mt Coot-tha (real height
        // 287m) — the face sits on its NE bench (probed: h ~150–175, dry),
        // ~300m uphill of the Mt Coot-tha roundabout at the slope's base
        // (~8500, 14250), where Milton Road (#99's inaugural road order)
        // arrives from the town centre. Deliberately 5x the qty of a field
        // rock so hauling for road orders (#94–96) centres here; mining is
        // the ordinary gather verb. The client draws a "Quarry" site marker
        // at the face's centre (~8232, 13915).
        ResourceNodeSpawn { id: "node_quarry_rock_0", district: "civic", item_id: "stone", x: 8210, y: 13900, qty: 25 },
        ResourceNodeSpawn { id: "node_quarry_rock_1", district: "civic", item_id: "stone", x: 8225, y: 13905, qty: 25 },
        ResourceNodeSpawn { id: "node_quarry_rock_2", district: "civic", item_id: "stone", x: 8240, y: 13900, qty: 25 },
        ResourceNodeSpawn { id: "node_quarry_rock_3", district: "civic", item_id: "stone", x: 8255, y: 13905, qty: 25 },
        ResourceNodeSpawn { id: "node_quarry_rock_4", district: "civic", item_id: "stone", x: 8210, y: 13925, qty: 25 },
        ResourceNodeSpawn { id: "node_quarry_rock_5", district: "civic", item_id: "stone", x: 8225, y: 13930, qty: 25 },
        ResourceNodeSpawn { id: "node_quarry_rock_6", district: "civic", item_id: "stone", x: 8240, y: 13925, qty: 25 },
        ResourceNodeSpawn { id: "node_quarry_rock_7", district: "civic", item_id: "stone", x: 8255, y: 13930, qty: 25 },
        // The logging camp (#126): a starter grove clustered around the
        // logging foreman, mirroring the quarry's rich-node treatment —
        // 6 nodes at 4x an ordinary field tree's qty, so hauling/crafting
        // here isn't gated on the same handful of scattered singles near
        // spawn. Probed dry (h ~2.5–5.2m, well above sea level) before
        // authoring.
        ResourceNodeSpawn { id: "node_logging_tree_0", district: "civic", item_id: "wood", x: 14280, y: 11380, qty: 20 },
        ResourceNodeSpawn { id: "node_logging_tree_1", district: "civic", item_id: "wood", x: 14320, y: 11385, qty: 20 },
        ResourceNodeSpawn { id: "node_logging_tree_2", district: "civic", item_id: "wood", x: 14340, y: 11410, qty: 20 },
        ResourceNodeSpawn { id: "node_logging_tree_3", district: "civic", item_id: "wood", x: 14320, y: 11435, qty: 20 },
        ResourceNodeSpawn { id: "node_logging_tree_4", district: "civic", item_id: "wood", x: 14280, y: 11430, qty: 20 },
        ResourceNodeSpawn { id: "node_logging_tree_5", district: "civic", item_id: "wood", x: 14260, y: 11405, qty: 20 },
    ];

    // A public town storehouse beside the town centre (the M2 stash). Per-plot
    // home storage (#12) will add more storage points using the same protocol.
    let storage_points = vec![StoragePoint {
        id: "storehouse_town",
        district: "civic",
        x: tcx + 30,
        y: tcy + 10,
    }];

    // The city build-order board, at the town centre (opposite the storehouse) so a
    // fresh spawn can reach it. Contributions are gated on standing near this.
    let build_boards = vec![BuildBoard {
        id: "board_town",
        district: "civic",
        x: tcx - 30,
        y: tcy + 10,
    }];

    Capital {
        districts: vec![market, civic, suburbs, craftworks, old_quarter],
        town_centre,
        build_orders,
        resource_nodes,
        storage_points,
        build_boards,
        npcs: npc_spawns(),
        mobs: mob_spawns(),
        terrain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CAPITAL_DISTRICT_IDS` exists so `market.toml` can validate a
    /// `[districts.<id>]` table without loading the whole baked terrain
    /// artifact (#152). A hand-maintained list can drift, and drift here means
    /// the server refusing to boot on a district that genuinely exists — so it
    /// is pinned to the real thing in both directions.
    #[test]
    fn district_ids_match_the_authored_capital() {
        let mut authored: Vec<&str> = capital().districts.iter().map(|d| d.id).collect();
        let mut listed = capital_district_ids();
        authored.sort_unstable();
        listed.sort_unstable();
        assert_eq!(
            listed, authored,
            "CAPITAL_DISTRICT_IDS has drifted from capital() — market.toml validation \n             would reject a real district, or accept a typo"
        );
    }

    #[test]
    fn districts_tile_the_world_without_gaps_or_overlap() {
        let c = capital();
        // No two districts overlap.
        for (i, a) in c.districts.iter().enumerate() {
            for b in &c.districts[i + 1..] {
                let overlap_x = a.region.x0 < b.region.x1 && b.region.x0 < a.region.x1;
                let overlap_y = a.region.y0 < b.region.y1 && b.region.y0 < a.region.y1;
                assert!(!(overlap_x && overlap_y), "{} overlaps {}", a.id, b.id);
            }
        }
        // Areas sum to the whole world (full coverage of a clean tiling).
        let total: i64 = c.districts.iter().map(|d| d.region.area()).sum();
        assert_eq!(total, (WORLD_SIZE as i64) * (WORLD_SIZE as i64));
    }

    #[test]
    fn town_centre_is_inside_a_safe_district_and_is_the_spawn() {
        let c = capital();
        let (tx, ty) = c.town_centre;
        let d = c.district_at(tx, ty).expect("town centre lies in a district");
        assert_eq!(d.id, "civic");
        assert_eq!(d.safety, Safety::Safe);
        // The gateway's spawn constant must agree with the authored town centre.
        assert_eq!((tx, ty), (WORLD_SIZE / 2, WORLD_SIZE / 2));
    }

    #[test]
    fn district_lookup_by_point_and_region() {
        let c = capital();
        assert_eq!(c.district_at(10, 10).unwrap().id, "suburbs");
        assert_eq!(c.district_at(12800, 12800).unwrap().id, "civic");
        assert_eq!(c.district_at(22000, 12800).unwrap().id, "market");
        assert_eq!(c.district_at(12800, 3200).unwrap().id, "craftworks");
        assert_eq!(c.district_at(12800, 22400).unwrap().id, "old_quarter");
        assert!(c.district_at(WORLD_SIZE, 0).is_none()); // outside (half-open)
        // Region centre routing survives shard geometry.
        let r = Rect::new(200, 0, 1600, 2400);
        assert_eq!(c.district_for_region(r).unwrap().id, "suburbs");
    }

    #[test]
    fn starter_plots_are_authored_and_inside_the_suburbs() {
        let c = capital();
        let suburbs = c.districts.iter().find(|d| d.id == "suburbs").unwrap();
        let cells = suburbs.plots();
        assert_eq!(cells.len(), 240); // 12 x 20
        for cell in &cells {
            assert!(
                suburbs.region.contains(cell.x, cell.y)
                    && suburbs.region.contains(cell.x + cell.w - 1, cell.y + cell.h - 1),
                "plot {:?} escapes the suburbs band",
                cell
            );
        }
        // Grid indices are unique.
        let mut seen = std::collections::HashSet::new();
        for cell in &cells {
            assert!(seen.insert((cell.grid_x, cell.grid_y)), "duplicate grid cell");
        }
        // Only the suburbs carry a grid; the capital total matches.
        assert_eq!(c.starter_plots().len(), 240);
    }

    /// The quarry (#97, relocated to Mt Coot-tha's east flank in #99): the
    /// pen's stone source is a tight cluster of rich nodes on the mountain
    /// bench above the Milton Road roundabout, inside the civic district.
    /// (Dryness is covered for every node by the water-mask anchor test
    /// below; this pins the quarry's own contract.)
    #[test]
    fn quarry_is_a_rich_stone_cluster_on_mt_coottha() {
        let c = capital();
        let t = loaded_terrain();
        let site = Rect::new(8150, 13850, 8350, 13990);
        let quarry: Vec<_> = c.resource_nodes.iter().filter(|n| n.id.starts_with("node_quarry_")).collect();
        assert!(quarry.len() >= 6, "a quarry needs a working face, not a pebble (got {})", quarry.len());
        for n in &quarry {
            assert_eq!(n.item_id, "stone", "{} must be stone", n.id);
            assert_eq!(n.district, "civic", "{} must sit in the civic district", n.id);
            assert!(site.contains(n.x, n.y), "{} at ({},{}) escapes the authored site", n.id, n.x, n.y);
            assert!(n.qty >= 20, "{} should be rich (5x a field rock), got {}", n.id, n.qty);
            let h = t.sample_height(n.x as f32, n.y as f32);
            assert!(h > 100.0, "{} should sit on the mountain bench (h={h:.1})", n.id);
        }
    }

    /// The wild-dog pack (#158) is authored content, so where it sits is a
    /// design decision rather than an accident — and one a rebake or a re-site
    /// could silently ruin. Every property the survey picked the spot for is
    /// pinned here.
    #[test]
    fn the_wild_dog_pack_is_sited_where_a_new_player_will_meet_it() {
        let c = capital();
        let t = loaded_terrain();
        let dogs: Vec<_> = c.mobs.iter().filter(|m| m.species == SPECIES_WILD_DOG).collect();
        assert!(dogs.len() >= 3, "a pack needs to be a pack, got {}", dogs.len());

        let (tcx, tcy) = c.town_centre;
        for d in &dogs {
            // Inside the district that claims it.
            assert_eq!(
                c.district_at(d.x, d.y).map(|x| x.id),
                Some(d.district),
                "{} is sited outside its own district",
                d.id
            );

            // On dry land, across the space a fight actually moves through —
            // not just the pin. The east band reaches the bay and the river
            // runs near spawn, so "somewhere nearby" is not automatically land.
            for (ox, oy) in [(0, 0), (-60, -60), (60, -60), (-60, 60), (60, 60)] {
                assert!(
                    !t.is_water((d.x + ox) as f32, (d.y + oy) as f32),
                    "{} is in the water at ({},{})",
                    d.id,
                    d.x + ox,
                    d.y + oy
                );
            }

            let dist = (((d.x - tcx) as f64).powi(2) + ((d.y - tcy) as f64).powi(2)).sqrt();
            // Far enough that a fresh spawn is never harassed at the storehouse
            // — mob aggro is 180 — but close enough to be a short deliberate
            // walk rather than an expedition.
            assert!(
                (400.0..1500.0).contains(&dist),
                "{} is {dist:.0} from spawn; the pack should be a short walk, not an ambush \
                 at the door or a trek",
                d.id
            );

            // Clear of every other interaction by MORE THAN THE LEASH, so a dog
            // can never *wander* onto somebody's gather prompt or build panel —
            // not merely start clear of it. Stating it in terms of the leash is
            // what keeps the two from drifting apart: widen the leash and this
            // test demands a roomier site, which is the correct conversation to
            // be forced into.
            let clearance = (AUTHORED_MOB_LEASH + 100) as f64;
            for n in &c.resource_nodes {
                let g = (((n.x - d.x) as f64).powi(2) + ((n.y - d.y) as f64).powi(2)).sqrt();
                assert!(g > clearance, "{} could wander onto resource node {}", d.id, n.id);
            }
            for n in &c.npcs {
                let g = (((n.x - d.x) as f64).powi(2) + ((n.y - d.y) as f64).powi(2)).sqrt();
                assert!(g > clearance, "{} could wander onto NPC {}", d.id, n.id);
            }
            for sp in &c.storage_points {
                let g = (((sp.x - d.x) as f64).powi(2) + ((sp.y - d.y) as f64).powi(2)).sqrt();
                assert!(g > clearance, "{} could wander onto a storage point", d.id);
            }
            for o in &c.build_orders {
                let g = (((o.structure_x - d.x) as f64).powi(2)
                    + ((o.structure_y - d.y) as f64).powi(2))
                .sqrt();
                assert!(g > clearance, "{} could wander onto the {} build site", d.id, o.kind);
            }
            for b in &c.build_boards {
                let g = (((b.x - d.x) as f64).powi(2) + ((b.y - d.y) as f64).powi(2)).sqrt();
                assert!(g > clearance, "{} could wander onto the build board", d.id);
            }
        }

        // Reachable on foot from spawn without swimming — drowning is real
        // (#83), so a pack you can only swim to would be a trap.
        let lead = dogs[0];
        for i in 0..=200 {
            let f = i as f32 / 200.0;
            let x = tcx as f32 + (lead.x - tcx) as f32 * f;
            let y = tcy as f32 + (lead.y - tcy) as f32 * f;
            assert!(!t.is_water(x, y), "the walk to the pack crosses water at ({x:.0},{y:.0})");
        }

        // They're a pack: clustered together, not scattered across a district.
        for d in &dogs {
            let g = (((d.x - lead.x) as f64).powi(2) + ((d.y - lead.y) as f64).powi(2)).sqrt();
            assert!(g < 300.0, "{} has wandered off from the pack ({g:.0} away)", d.id);
        }

        // Ids are unique — they double as entity ids in the zone, so a
        // collision would have two dogs sharing one body.
        let mut ids: Vec<&str> = c.mobs.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate authored mob id");
    }

    /// Wild dogs bite (#159), so their siting stops being merely tidy and starts
    /// being a safety property: a player at the town centre must never be
    /// reachable by the pack.
    ///
    /// The territory is bounded by geometry rather than by a rule that could be
    /// forgotten — a dog aggros within `AGGRO_RADIUS` of itself and is leashed to
    /// `AUTHORED_MOB_LEASH` of its home, so it can threaten a point at most the
    /// sum of the two away. `AGGRO_RADIUS` lives in the zone, so it is restated
    /// here as the value this siting was chosen against; if it ever grows past
    /// this, the pack needs moving and this test is where that conversation
    /// starts.
    #[test]
    fn the_pack_cannot_reach_the_town_centre() {
        const ZONE_AGGRO_RADIUS: i32 = 180;
        let c = capital();
        let (tcx, tcy) = c.town_centre;
        let threat = (AUTHORED_MOB_LEASH + ZONE_AGGRO_RADIUS) as f64;

        for d in c.mobs.iter().filter(|m| m.species == SPECIES_WILD_DOG) {
            let dist = (((d.x - tcx) as f64).powi(2) + ((d.y - tcy) as f64).powi(2)).sqrt();
            assert!(
                dist > threat * 1.5,
                "{} sits {dist:.0} from spawn but can threaten out to {threat:.0} — a fresh                  character would be mauled at the storehouse",
                d.id
            );
        }

        // The same guarantee for everywhere else a player is made to stand: the
        // storehouse, the build board, the markets, the NPCs who hand out tools.
        for sp in &c.storage_points {
            for d in &c.mobs {
                let g = (((sp.x - d.x) as f64).powi(2) + ((sp.y - d.y) as f64).powi(2)).sqrt();
                assert!(g > threat, "{} can reach a storage point ({g:.0})", d.id);
            }
        }
        for n in &c.npcs {
            for d in &c.mobs {
                let g = (((n.x - d.x) as f64).powi(2) + ((n.y - d.y) as f64).powi(2)).sqrt();
                assert!(g > threat, "{} can reach NPC {} ({g:.0})", d.id, n.id);
            }
        }
        for o in &c.build_orders {
            for d in &c.mobs {
                let g = (((o.structure_x - d.x) as f64).powi(2)
                    + ((o.structure_y - d.y) as f64).powi(2))
                .sqrt();
                assert!(g > threat, "{} can reach the {} site ({g:.0})", d.id, o.kind);
            }
        }
    }

    /// The SHIPPED `zones.toml` — not a synthetic fixture — is valid and sited
    /// where it claims (#165).
    ///
    /// `zone_config`'s own tests prove the loader rejects bad layouts; this
    /// proves the layout we actually ship isn't one of them, and that the adit
    /// mouth obeys the same siting rules every other fixture does. A mine you
    /// have to swim to, or that opens inside the dog pack, would be authored
    /// exactly as easily as a good one.
    #[test]
    fn the_shipped_mine_is_valid_and_well_sited() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../zones.toml");
        let cfg = crate::zone_config::ZoneConfig::load(std::path::Path::new(path))
            .expect("the shipped zones.toml must load");
        let Some(mine) = cfg.interior("mine_starter") else {
            // A checkout without the file is legitimate — it just means no
            // interiors — so this is a skip, not a failure.
            return;
        };

        let c = capital();
        let t = loaded_terrain();
        let (tcx, tcy) = c.town_centre;
        let portal = mine.portals.first().expect("a way in");
        let (px, py) = portal.world;

        // On dry land across the whole mouth: the walk to a mine must not be a
        // swim, and drowning is real (#83).
        for (ox, oy) in [(0, 0), (-40, -40), (40, -40), (-40, 40), (40, 40)] {
            assert!(
                !t.is_water((px + ox) as f32, (py + oy) as f32),
                "the adit mouth is in the water at ({}, {})",
                px + ox,
                py + oy
            );
        }
        // Walkable from spawn without crossing water.
        for i in 0..=200 {
            let f = i as f32 / 200.0;
            let x = tcx as f32 + (px - tcx) as f32 * f;
            let y = tcy as f32 + (py - tcy) as f32 * f;
            assert!(!t.is_water(x, y), "the walk to the mine crosses water at ({x:.0},{y:.0})");
        }

        let dist = (((px - tcx) as f64).powi(2) + ((py - tcy) as f64).powi(2)).sqrt();
        assert!(
            (300.0..1200.0).contains(&dist),
            "the mine is {dist:.0} from spawn — a short walk, not an expedition or a doorstep"
        );

        // Outside the wild dogs' reach (#157): aggro 180 plus a 250 leash.
        let threat = (AUTHORED_MOB_LEASH + 180) as f64;
        for d in &c.mobs {
            let g = (((d.x - px) as f64).powi(2) + ((d.y - py) as f64).powi(2)).sqrt();
            assert!(g > threat, "{} can reach the adit mouth ({g:.0})", d.id);
        }
        // Clear of every other interaction, so the mouth is its own place.
        for n in &c.npcs {
            let g = (((n.x - px) as f64).powi(2) + ((n.y - py) as f64).powi(2)).sqrt();
            assert!(g > 200.0, "the adit mouth is on top of NPC {}", n.id);
        }
        for n in &c.resource_nodes {
            let g = (((n.x - px) as f64).powi(2) + ((n.y - py) as f64).powi(2)).sqrt();
            assert!(g > 150.0, "the adit mouth is on top of node {}", n.id);
        }

        // The interior itself hangs together: you arrive on floor, the anchor is
        // floor, and the galleries actually connect to the entrance chamber.
        assert!(mine.contains(portal.inside.0, portal.inside.1));
        assert!(mine.contains(mine.spawn_anchor.0, mine.spawn_anchor.1));
        assert!(mine.volumes.len() >= 3, "three short galleries, so players disperse");
    }

    /// The shipped deposits (#166) are real, reachable, and laid out the way the
    /// design intends: clay near the mouth as the tutorial resource, iron deep
    /// enough to be a walk.
    ///
    /// `zone_config` proves a deposit in the rock is refused; this proves the
    /// mine we actually ship isn't one, and that the two files agree — a
    /// placement naming a type that `crafting.toml` doesn't define would spawn
    /// nothing at all, silently.
    #[test]
    fn the_shipped_deposits_are_reachable_and_sensibly_spread() {
        let zpath = concat!(env!("CARGO_MANIFEST_DIR"), "/../zones.toml");
        let cpath = concat!(env!("CARGO_MANIFEST_DIR"), "/../crafting.toml");
        let zones = crate::zone_config::ZoneConfig::load(std::path::Path::new(zpath)).unwrap();
        let crafting =
            crate::crafting_config::CraftingConfig::load(std::path::Path::new(cpath)).unwrap();
        let Some(mine) = zones.interior("mine_starter") else { return };
        if mine.deposits.is_empty() {
            return;
        }

        let portal = mine.portals.first().expect("a way in");
        let (ex, ey) = portal.inside;
        let (mut clay, mut iron) = (0, 0);
        for d in &mine.deposits {
            // Both files have to agree, or the seam silently never spawns.
            let t = crafting
                .deposit(&d.kind)
                .unwrap_or_else(|| panic!("{} names undefined type `{}`", d.id, d.kind));
            assert!(mine.contains(d.pos.0, d.pos.1), "{} is inside the rock", d.id);
            let walk = (((d.pos.0 - ex) as f64).powi(2) + ((d.pos.1 - ey) as f64).powi(2)).sqrt();
            match d.kind.as_str() {
                "clay_starter" => {
                    clay += 1;
                    assert!(walk < 260.0, "{} is meant to be near the mouth ({walk:.0})", d.id);
                }
                "iron_starter" => {
                    iron += 1;
                    assert!(walk > 260.0, "{} is meant to be a walk in ({walk:.0})", d.id);
                }
                _ => {}
            }
            assert_eq!(t.required_tool, "pickaxe");
        }
        assert!(clay >= 8, "not enough clay to be the tutorial resource ({clay})");
        assert!(iron >= 12, "not enough iron seams to disperse players ({iron})");

        // Seams shouldn't stack on each other, or two of them are one in
        // practice and the density that bounds the faucet is a lie.
        for (i, a) in mine.deposits.iter().enumerate() {
            for b in &mine.deposits[i + 1..] {
                let g = (((a.pos.0 - b.pos.0) as f64).powi(2)
                    + ((a.pos.1 - b.pos.1) as f64).powi(2))
                .sqrt();
                assert!(g > 20.0, "{} and {} are on top of each other ({g:.0})", a.id, b.id);
            }
        }
    }

    /// Region filtering is what makes a zone split safe: each half must derive
    /// exactly the creatures inside it, with none duplicated into both and none
    /// lost from both. Same contract `resource_nodes_in` already honours.
    #[test]
    fn authored_mobs_are_partitioned_cleanly_by_region() {
        let c = capital();
        assert!(!c.mobs.is_empty());

        // The whole world holds every one of them.
        let all = c.mobs_in(Rect::new(0, 0, WORLD_SIZE, WORLD_SIZE));
        assert_eq!(all.len(), c.mobs.len());

        // Split the world in half and the two halves partition it exactly.
        let half = WORLD_SIZE / 2;
        let left = c.mobs_in(Rect::new(0, 0, half, WORLD_SIZE));
        let right = c.mobs_in(Rect::new(half, 0, WORLD_SIZE, WORLD_SIZE));
        assert_eq!(
            left.len() + right.len(),
            c.mobs.len(),
            "a split duplicated or dropped an authored creature"
        );
        for m in &left {
            assert!(!right.contains(m), "{} is owned by both halves of a split", m.id);
        }

        // An empty corner owns none, and that must be fine rather than a panic.
        assert!(c.mobs_in(Rect::new(0, 0, 10, 10)).is_empty());
    }

    /// #84 made the river/bay a real water mask (`sea_level_m = 0`). Two
    /// invariants any future rebake must preserve: the mask is genuinely
    /// non-empty (drowning needs real water), and every authored gameplay
    /// anchor — spawn, plots, resource nodes, storage points — is on dry
    /// land. The v3 crop was *placed* so these hold (see terrain.toml's
    /// header); this asserts it instead of assuming it.
    #[test]
    fn water_mask_is_real_and_authored_gameplay_points_stay_dry() {
        let c = capital();
        let t = loaded_terrain();

        let (tx, ty) = c.town_centre;
        assert!(!t.is_water(tx as f32, ty as f32), "spawn/town centre is underwater");

        for n in &c.resource_nodes {
            assert!(!t.is_water(n.x as f32, n.y as f32), "resource node {} is underwater", n.id);
        }
        for s in &c.storage_points {
            assert!(!t.is_water(s.x as f32, s.y as f32), "storage point {} is underwater", s.id);
        }
        for (_, cell) in c.starter_plots() {
            // Corners and centre — a plot partially in the river is still broken.
            for (px, py) in [
                (cell.x, cell.y),
                (cell.x + cell.w - 1, cell.y),
                (cell.x, cell.y + cell.h - 1),
                (cell.x + cell.w - 1, cell.y + cell.h - 1),
                (cell.x + cell.w / 2, cell.y + cell.h / 2),
            ] {
                assert!(
                    !t.is_water(px as f32, py as f32),
                    "plot ({},{}) is underwater at ({px},{py})",
                    cell.grid_x, cell.grid_y
                );
            }
        }

        // The mask is non-empty: a coarse 100m scan must find the river/bay
        // (~10% of the world). Guards against silently rebaking with the old
        // empty-mask sea level.
        let mut water_samples = 0u32;
        let mut total = 0u32;
        let step = 100;
        let mut y = step / 2;
        while y < WORLD_SIZE {
            let mut x = step / 2;
            while x < WORLD_SIZE {
                total += 1;
                if t.is_water(x as f32, y as f32) {
                    water_samples += 1;
                }
                x += step;
            }
            y += step;
        }
        let frac = water_samples as f64 / total as f64;
        assert!(
            frac > 0.05,
            "water mask looks empty ({water_samples}/{total} coarse samples) — was the bake run with the old sea_level_m = -25?"
        );
    }

    #[test]
    fn item_registry_and_node_spawns_are_consistent() {
        let c = capital();
        // Every node references a real item and sits inside its district.
        assert!(!c.resource_nodes.is_empty());
        for n in &c.resource_nodes {
            assert!(item(n.item_id).is_some(), "node {} -> unknown item {}", n.id, n.item_id);
            let d = c.districts.iter().find(|d| d.id == n.district).expect("node district exists");
            assert!(d.region.contains(n.x, n.y), "node {} escapes {}", n.id, n.district);
            assert!(n.qty > 0);
        }
        // Node ids are unique.
        let mut seen = std::collections::HashSet::new();
        for n in &c.resource_nodes {
            assert!(seen.insert(n.id), "duplicate node id {}", n.id);
        }
        // A fresh spawn at the town centre finds wood nearby.
        let (tcx, tcy) = c.town_centre;
        let near = c.resource_nodes.iter().any(|n| {
            n.item_id == "wood" && ((n.x - tcx).pow(2) + (n.y - tcy).pow(2)) < 200 * 200
        });
        assert!(near, "no wood near the town centre");
    }

    #[test]
    fn resource_nodes_in_filters_by_region() {
        let c = capital();
        let civic = c.districts.iter().find(|d| d.id == "civic").unwrap().region;
        let in_civic = c.resource_nodes_in(civic);
        assert!(!in_civic.is_empty());
        assert!(in_civic.iter().all(|n| n.district == "civic"));
        // The whole world contains every node.
        assert_eq!(c.resource_nodes_in(Rect::new(0, 0, WORLD_SIZE, WORLD_SIZE)).len(), c.resource_nodes.len());
    }

    #[test]
    fn storage_point_is_in_the_civic_centre_near_spawn() {
        let c = capital();
        assert!(!c.storage_points.is_empty());
        let (tcx, tcy) = c.town_centre;
        for s in &c.storage_points {
            assert_eq!(c.district_at(s.x, s.y).map(|d| d.id), Some(s.district));
            // Near the town centre so a fresh spawn can reach it.
            assert!((s.x - tcx).pow(2) + (s.y - tcy).pow(2) < 100 * 100);
        }
        let civic = c.districts.iter().find(|d| d.id == "civic").unwrap().region;
        assert_eq!(c.storage_points_in(civic).len(), c.storage_points.len());
    }

    #[test]
    fn capital_authors_only_the_two_markets() {
        let c = capital();
        // Roads and most city work are commissioned at runtime by the mayor,
        // not authored. The markets (#137, #153) are the deliberate exception:
        // trading needs a fixed, findable place to hang off, so they're authored
        // — but still player-BUILT, seeding `open`/`locked` rather than
        // pre-placed. Anything else appearing here should be a deliberate
        // decision, not a drive-by addition.
        assert_eq!(c.build_orders.len(), 2, "only the two markets are authored");
        for o in &c.build_orders {
            assert_eq!(o.structure_kind, "market", "{} is not a market", o.kind);
            assert_eq!(o.required_level, 0, "no skill gate keeps newcomers out of the economy");
            assert_eq!(
                c.district_at(o.structure_x, o.structure_y).map(|d| d.id),
                Some(o.district),
                "{} is sited outside the district that owns it",
                o.kind
            );
        }

        // The capital's, at the root of the trade tree.
        let (tcx, tcy) = c.town_centre;
        let first = c.build_orders.iter().find(|o| o.kind == "market").unwrap();
        assert_eq!(first.district, "civic");
        assert!(first.prereq.is_none(), "the first market gates on nothing");
        assert!((first.structure_x - tcx).pow(2) + (first.structure_y - tcy).pow(2) < 200 * 200,
            "the first market should be a short walk from spawn");

        // The Market District's (#153), gated behind the first so a small
        // player base finishes one before starting the other.
        let second = c.build_orders.iter().find(|o| o.kind == "market_east").unwrap();
        assert_eq!(second.district, "market", "the Market District finally has a market");
        assert_eq!(second.prereq, Some("market"), "the second market is the reward for the first");
        assert_eq!(
            second.required_json, first.required_json,
            "the second market costs the same — the 8.6km haul is the difficulty, not the bill"
        );
        // Distinct kinds: the prereq edge is keyed on kind, and the build board
        // renders it raw. Two orders both called "market" would be ambiguous in
        // both places.
        assert_ne!(first.kind, second.kind);

        // Far enough that hauling between them is a real journey — that
        // distance IS the arbitrage, so a future re-site must not quietly
        // shorten it.
        let d2 = ((second.structure_x - first.structure_x) as i64).pow(2)
            + ((second.structure_y - first.structure_y) as i64).pow(2);
        assert!(d2 > 5000i64.pow(2), "the two markets are too close to be worth hauling between");
    }

    /// The east band reaches the Brisbane river mouth and Moreton Bay, so
    /// "somewhere east" is emphatically not automatically land. This pins the
    /// survey that picked (20800, 9600) — dry, flat, well inside its district,
    /// clear of the resource nodes, and walkable from spawn without swimming —
    /// so a future rebake or re-site can't silently drop a market in the water
    /// or somewhere a player can't reach on foot.
    #[test]
    fn the_second_market_is_sited_on_dry_reachable_ground() {
        let c = capital();
        let t = loaded_terrain();
        let m = c.build_orders.iter().find(|o| o.kind == "market_east").unwrap();
        let (mx, my) = (m.structure_x, m.structure_y);

        // Dry across the whole footprint a trader stands in, not just the pin.
        for (ox, oy) in [(0, 0), (-60, -60), (60, -60), (-60, 60), (60, 60)] {
            assert!(
                !t.is_water((mx + ox) as f32, (my + oy) as f32),
                "the second market is in the water at ({},{})",
                mx + ox,
                my + oy
            );
        }

        // Flat enough to be a plaza rather than a cliff face.
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for (ox, oy) in [(-60, -60), (60, -60), (-60, 60), (60, 60), (0, 0)] {
            let h = t.sample_height((mx + ox) as f32, (my + oy) as f32);
            lo = lo.min(h);
            hi = hi.max(h);
        }
        assert!(hi - lo < 3.0, "the second market's site is not level (spread {:.2}m)", hi - lo);
        assert!(lo > 1.0, "the second market sits barely above the waterline ({lo:.2}m)");

        // Well inside its district: a trader standing anywhere in range must
        // resolve to `market`, or `market_at` would look up the wrong config.
        let region = c.districts.iter().find(|d| d.id == "market").unwrap().region;
        let margin = 600;
        assert!(
            mx - region.x0 > margin && region.x1 - mx > margin
                && my - region.y0 > margin && region.y1 - my > margin,
            "the second market is too close to its district edge"
        );

        // Clear of every resource node by far more than any interaction range,
        // so the market panel and a gather prompt never fight for the screen.
        for n in &c.resource_nodes {
            let d2 = ((n.x - mx) as i64).pow(2) + ((n.y - my) as i64).pow(2);
            assert!(d2 > 500i64.pow(2), "resource node {} is on top of the second market", n.id);
        }

        // Reachable on foot: the straight line from spawn never crosses water.
        // Drowning is a real hazard (#83), so a market you can only swim to
        // would be a trap rather than a destination.
        let (sx, sy) = c.town_centre;
        for i in 0..=400 {
            let f = i as f32 / 400.0;
            let x = sx as f32 + (mx - sx) as f32 * f;
            let y = sy as f32 + (my - sy) as f32 * f;
            assert!(!t.is_water(x, y), "the walk to the second market crosses water at ({x:.0},{y:.0})");
        }
    }

    #[test]
    fn build_board_is_in_the_civic_centre_near_spawn() {
        let c = capital();
        assert!(!c.build_boards.is_empty());
        let (tcx, tcy) = c.town_centre;
        for b in &c.build_boards {
            assert_eq!(c.district_at(b.x, b.y).map(|d| d.id), Some(b.district));
            assert!((b.x - tcx).pow(2) + (b.y - tcy).pow(2) < 100 * 100,
                "board should be reachable from a fresh spawn");
        }
        let civic = c.districts.iter().find(|d| d.id == "civic").unwrap().region;
        assert_eq!(c.build_boards_in(civic).len(), c.build_boards.len());
    }

    #[test]
    fn rect_overlaps_detects_any_shared_area() {
        let suburbs = Rect::new(800, 0, 1200, 1200);
        // A single whole-world zone overlaps every district, even though its
        // *centre* (600,600) falls only in civic — this is the case `overlaps`
        // exists for (#13).
        let whole_world = Rect::new(0, 0, WORLD_SIZE, WORLD_SIZE);
        assert!(whole_world.overlaps(suburbs));
        // Two districts that only share a boundary edge (half-open) don't overlap.
        let civic = Rect::new(400, 0, 800, 1200);
        assert!(!civic.overlaps(suburbs));
        // A region entirely inside the suburbs overlaps it.
        assert!(Rect::new(900, 100, 1000, 200).overlaps(suburbs));
        // A region entirely elsewhere does not.
        assert!(!Rect::new(0, 0, 100, 100).overlaps(suburbs));
    }

    #[test]
    fn recipe_registry_is_well_formed() {
        let all = recipes();
        assert!(!all.is_empty());
        for r in &all {
            assert!(!r.inputs.is_empty(), "{} has no inputs", r.id);
            for (input_item, qty) in r.inputs {
                assert!(item(input_item).is_some(), "{} needs unknown item {}", r.id, input_item);
                assert!(*qty > 0);
            }
            assert!(item(r.output_item).is_some(), "{} produces unknown item {}", r.id, r.output_item);
            assert!(r.output_qty > 0);
        }
        // Ids are unique and the lookup helper agrees with the list.
        let mut seen = std::collections::HashSet::new();
        for r in &all {
            assert!(seen.insert(r.id), "duplicate recipe id {}", r.id);
            assert_eq!(recipe(r.id).map(|found| found.id), Some(r.id));
        }
        assert!(recipe("nonexistent").is_none());
    }

    #[test]
    fn structure_footprints_cover_every_placeable_home_structure() {
        for kind in ["bed", "storage", "crafting"] {
            let (w, h) = structure_footprint(kind).expect(kind);
            assert!(w > 0 && h > 0, "{kind} has a degenerate footprint");
        }
        assert!(structure_footprint("wall").is_none(), "city structures aren't placeable homes");
    }

    #[test]
    fn terrain_loads_from_the_baked_artifact_deterministically_and_in_bounds() {
        let t1 = capital().terrain;
        let t2 = capital().terrain;
        // Cached via `loaded_terrain`'s `OnceLock` — literally the same `Arc`
        // (loaded from disk once per process), not just equal content.
        assert!(std::sync::Arc::ptr_eq(&t1, &t2), "the terrain artifact should be loaded once and cached");

        let manifest = t1.manifest();
        assert_eq!(manifest.world_size_m, (WORLD_SIZE as f32, WORLD_SIZE as f32));

        // Every sampled corner of the wire-format grid stays within the
        // artifact's own declared height range (a little slack for the u16
        // encoding's quantization).
        let step = WORLD_SIZE as f32 / TERRAIN_RESOLUTION as f32;
        for gy in 0..=TERRAIN_RESOLUTION {
            for gx in 0..=TERRAIN_RESOLUTION {
                let h = t1.sample_height(gx as f32 * step, gy as f32 * step);
                assert!(
                    h >= manifest.height_min_m - 0.05 && h <= manifest.height_max_m + 0.05,
                    "corner ({gx},{gy}) height {h} outside the artifact's declared [{}, {}] range",
                    manifest.height_min_m,
                    manifest.height_max_m
                );
            }
        }
    }

    #[test]
    fn terrain_is_smooth_not_a_jagged_sheet() {
        // Adjacent wire-grid corners should differ by a modest fraction of
        // the artifact's whole height range -- confirms the checked-in bake
        // is genuinely broad rolling hills, not a jagged sheet (a property
        // of the *data*, since the generator that used to live in this crate
        // is now `terrain-bake`'s job — see its own `synth`/`stylize` tests
        // for the generation-time guarantees).
        let t = capital().terrain;
        let manifest = t.manifest();
        let range = (manifest.height_max_m - manifest.height_min_m).max(0.001);
        let max_step = range * 0.5; // generous, well under the whole-range worst case
        let step = WORLD_SIZE as f32 / TERRAIN_RESOLUTION as f32;
        for gy in 0..=TERRAIN_RESOLUTION {
            for gx in 0..TERRAIN_RESOLUTION {
                let a = t.sample_height(gx as f32 * step, gy as f32 * step);
                let b = t.sample_height((gx + 1) as f32 * step, gy as f32 * step);
                assert!(
                    (a - b).abs() <= max_step,
                    "horizontal neighbours at ({gx},{gy}) differ by {} > {max_step}",
                    (a - b).abs()
                );
            }
        }
        for gy in 0..TERRAIN_RESOLUTION {
            for gx in 0..=TERRAIN_RESOLUTION {
                let a = t.sample_height(gx as f32 * step, gy as f32 * step);
                let b = t.sample_height(gx as f32 * step, (gy + 1) as f32 * step);
                assert!(
                    (a - b).abs() <= max_step,
                    "vertical neighbours at ({gx},{gy}) differ by {} > {max_step}",
                    (a - b).abs()
                );
            }
        }
    }

    #[test]
    fn plot_field_is_flattened_so_plots_sit_on_level_ground() {
        // #55/#63: the suburbs starter plot field (240 plots) should be a
        // level plateau in the baked terrain artifact -- entities/markers are
        // placed via `Protocol.w2v`, which follows this same heightmap, so a
        // sloped plot field would clip structures on one side and float them
        // on the other. Flattening now happens once at bake time (the
        // repo-root `terrain.toml`'s `capital_flatten_mask`/
        // `capital_flatten_margin_m`), not in this crate — this test just
        // confirms the checked-in artifact actually has that property.
        const BAKED_FLATTEN_MARGIN: f32 = 100.0; // must match terrain.toml's capital_flatten_margin_m

        let c = capital();
        let suburbs = c.districts.iter().find(|d| d.id == "suburbs").unwrap();
        let cells = suburbs.plots();
        let x0 = cells.iter().map(|cell| cell.x).min().unwrap();
        let y0 = cells.iter().map(|cell| cell.y).min().unwrap();
        let x1 = cells.iter().map(|cell| cell.x + cell.w).max().unwrap();
        let y1 = cells.iter().map(|cell| cell.y + cell.h).max().unwrap();

        let t = &c.terrain;
        let step = WORLD_SIZE as f32 / TERRAIN_RESOLUTION as f32;

        // Every sampled corner well inside the plot field's bounding box (a
        // full margin in from every edge, so we're only sampling points
        // guaranteed to be at full flatten weight) must be level.
        let mut interior_heights = Vec::new();
        for gy in 0..=TERRAIN_RESOLUTION {
            for gx in 0..=TERRAIN_RESOLUTION {
                let wx = gx as f32 * step;
                let wy = gy as f32 * step;
                if wx >= x0 as f32 + BAKED_FLATTEN_MARGIN
                    && wx <= x1 as f32 - BAKED_FLATTEN_MARGIN
                    && wy >= y0 as f32 + BAKED_FLATTEN_MARGIN
                    && wy <= y1 as f32 - BAKED_FLATTEN_MARGIN
                {
                    interior_heights.push(t.sample_height(wx, wy));
                }
            }
        }
        assert!(
            !interior_heights.is_empty(),
            "expected at least one sampled corner well inside the plot field"
        );
        let first = interior_heights[0];
        for h in &interior_heights {
            assert!(
                (h - first).abs() < 0.05,
                "plot field interior isn't flat: {first} vs {h}"
            );
        }
    }

    #[test]
    fn plots_in_filters_by_region_like_the_other_authored_fixtures() {
        let c = capital();
        let suburbs = c.districts.iter().find(|d| d.id == "suburbs").unwrap().region;
        let in_suburbs = c.plots_in(suburbs);
        assert_eq!(in_suburbs.len(), 240, "every starter plot sits in the suburbs");
        // A civic-only region (no plot grid there) has none.
        let civic = c.districts.iter().find(|d| d.id == "civic").unwrap().region;
        assert!(c.plots_in(civic).is_empty());
        // Each returned cell's rect() really is inside the queried region.
        for (_, cell) in &in_suburbs {
            let r = cell.rect();
            assert!(suburbs.contains(r.x0, r.y0) && suburbs.contains(r.x1 - 1, r.y1 - 1));
        }
    }

    // --- Tool durability & repair (mining/abilities epic #123 backlog, #128) ------

    // --- balance model (#129) ------------------------------------------------
    //
    // These pin the DERIVED consequences of the tuning constants, not the
    // constants themselves. Anyone can change a number; the point of this block
    // is that changing one immediately shows what it did to the loop, in the
    // units the design actually cares about — how much of a tool's yield goes on
    // upkeep, how long until a skill stops improving, whether repairing is worth
    // it. Rediscovering those by hand was the expensive part of #129.
    //
    // A failure here is not necessarily a bug. It means a tuning change moved
    // something the balance pass deliberately chose, and the new numbers need a
    // decision rather than a rubber stamp.

    /// One tool's whole life, in the terms the design cares about.
    fn tool_economics(tool: &str) -> (i64, i64, i64, f64) {
        let max = tool_max_durability(tool).expect("a tool");
        let recipe = recipes().into_iter().find(|r| r.output_item == tool).expect("a recipe");
        let cost: i64 = recipe.inputs.iter().map(|(_, q)| *q).sum();
        // One unit harvested per swing, so a tool's lifetime yield is its
        // durability.
        (max, cost, max - cost, cost as f64 / max as f64)
    }

    /// Upkeep is a real share of what a tool gathers. Before #129 it was ~8% —
    /// durability existed mechanically (#128) but had no economic weight, and a
    /// free replacement was available from an NPC anyway. Tools should be a
    /// running cost, not a rounding error.
    #[test]
    fn tool_upkeep_is_a_real_share_of_what_it_gathers() {
        for tool in ["pickaxe", "axe"] {
            let (durability, cost, net, share) = tool_economics(tool);
            assert!(
                (0.12..0.40).contains(&share),
                "{tool}: upkeep is {:.0}% of its {durability}-swing yield ({cost} units) — \
                 outside the 12-40% band #129 chose. Under it, wear is a rounding error; \
                 over it, gathering stops paying for itself.",
                share * 100.0
            );
            assert!(net > 0, "{tool} costs more than it can ever gather — the loop is broken");
        }

        // Mining is deliberately the dearer track (#129), and is priced in the
        // resource it produces so a miner funds their own upkeep.
        let (_, pick_cost, _, pick_share) = tool_economics("pickaxe");
        let (_, axe_cost, _, axe_share) = tool_economics("axe");
        assert!(
            pick_cost > axe_cost && pick_share > axe_share,
            "the pickaxe should cost more than the axe — mining is the high-effort track"
        );
    }

    /// A tool must pay for itself out of the resource it harvests, or the
    /// gathering loop needs an external subsidy to run at all.
    #[test]
    fn each_tool_funds_itself_from_what_it_harvests() {
        for (tool, ability) in [("pickaxe", "pick"), ("axe", "chop")] {
            let harvested = ability_target_item(ability).expect("a harvest target");
            let max = tool_max_durability(tool).unwrap();
            let recipe = recipes().into_iter().find(|r| r.output_item == tool).unwrap();
            let own: i64 = recipe
                .inputs
                .iter()
                .filter(|(i, _)| *i == harvested)
                .map(|(_, q)| *q)
                .sum();
            assert!(own > 0, "{tool} doesn't cost any of the {harvested} it harvests");
            assert!(
                own < max,
                "{tool} costs {own} {harvested} but only harvests {max} — it can't fund itself"
            );
        }
    }

    /// Both curves reach their floor inside a single session. The old shared
    /// curve took 834 swings (~21 minutes of uninterrupted swinging) to stop
    /// improving, which is a long time to wait to feel a skill get better.
    #[test]
    fn a_skill_reaches_its_cooldown_floor_inside_one_session() {
        for ability in ["pick", "chop"] {
            let floor = ability_cooldown_ms(ability, 100);
            let floor_level = (0..=40)
                .find(|l| ability_cooldown_ms(ability, *l) == floor)
                .expect("the curve must reach its floor");
            assert!(
                (5..=8).contains(&floor_level),
                "{ability} floors at level {floor_level}; #129 targeted ~7 so the payoff lands \
                 inside one session"
            );

            // Swings to get there, and the wall-clock they take.
            let xp_needed = 100 * floor_level * floor_level;
            let per_swing = ability_xp_per_swing(ability);
            let mut xp = 0i64;
            let mut swings = 0i64;
            let mut ms = 0i64;
            while xp < xp_needed {
                ms += ability_cooldown_ms(ability, crate::persistence::level_for_xp(xp));
                xp += per_swing;
                swings += 1;
            }
            let minutes = ms as f64 / 60_000.0;
            assert!(
                (4.0..15.0).contains(&minutes),
                "{ability} takes {minutes:.1} min ({swings} swings) to stop improving — \
                 outside the single-session window #129 chose"
            );
        }
    }

    /// Chop is the accessible track and Pick the high-effort one (#129), so
    /// Chop swings faster at every level. Before the balance pass the two were
    /// literally the same curve.
    #[test]
    fn chop_is_faster_than_pick_at_every_level() {
        for level in 0..=20 {
            let chop = ability_cooldown_ms("chop", level);
            let pick = ability_cooldown_ms("pick", level);
            assert!(
                chop < pick,
                "at level {level} chop is {chop}ms and pick {pick}ms — mining should be slower"
            );
        }
        // And both improve monotonically, never getting worse with practice.
        for ability in ["pick", "chop"] {
            for level in 1..=20 {
                assert!(
                    ability_cooldown_ms(ability, level) <= ability_cooldown_ms(ability, level - 1),
                    "{ability} got SLOWER from level {} to {level}",
                    level - 1
                );
            }
        }
    }

    /// Repairing must beat crafting new at every wear level short of totally
    /// spent, or the wear system poses no decision at all. It used to saturate
    /// at the full recipe from 40/50 worn onward, so the top half of the curve
    /// was dominated and dead.
    #[test]
    fn repairing_always_beats_crafting_new_until_the_tool_is_spent() {
        for tool in ["pickaxe", "axe"] {
            let max = tool_max_durability(tool).unwrap();
            let recipe = recipes().into_iter().find(|r| r.output_item == tool).unwrap();
            let full: i64 = recipe.inputs.iter().map(|(_, q)| *q).sum();

            let mut last = 0i64;
            for missing in 1..max {
                let cost: i64 =
                    repair_cost(tool, missing, max).unwrap().iter().map(|(_, q)| *q).sum();
                assert!(
                    cost < full,
                    "{tool} missing {missing}/{max} costs {cost} to repair vs {full} to craft \
                     new — repair is dominated and nobody would ever choose it"
                );
                assert!(
                    cost >= last,
                    "{tool} repair got CHEAPER as it wore further ({last} -> {cost})"
                );
                last = cost;
            }
            // Entirely spent is the one point where rebuilding is the same deal.
            let spent: i64 = repair_cost(tool, max, max).unwrap().iter().map(|(_, q)| *q).sum();
            assert_eq!(spent, full, "a fully spent {tool} should cost exactly a fresh craft");
        }
    }

    /// A swing is worth more than the old multi-tick channel's per-unit rate,
    /// and both abilities pay the same for the same effort — the divergence
    /// #129 chose is in SPEED and TOOL COST, not in xp.
    #[test]
    fn both_abilities_pay_the_same_xp_per_swing() {
        assert_eq!(ability_xp_per_swing("pick"), ability_xp_per_swing("chop"));
        assert!(
            ability_xp_per_swing("pick") >= 10,
            "a swing should be worth at least the old channel's 10/unit"
        );
    }

    #[test]
    fn repair_cost_scales_with_missing_durability_and_floors_at_one() {
        // Pickaxe since #129: 3 wood + 5 stone, max durability 30. Cost is each
        // ingredient scaled by the fraction worn away, rounded DOWN, floored at
        // 1 so a token repair is never free.
        let max = tool_max_durability("pickaxe").unwrap();
        assert_eq!(repair_cost("pickaxe", 0, max), None, "nothing missing -> nothing to repair");
        // Barely worn: the floor, not the proportion (3*1/30 and 5*1/30 are 0).
        assert_eq!(repair_cost("pickaxe", 1, max), Some(vec![("wood", 1), ("stone", 1)]));
        // A third worn: wood 3*10/30 = 1, stone 5*10/30 = 1.
        assert_eq!(repair_cost("pickaxe", 10, max), Some(vec![("wood", 1), ("stone", 1)]));
        // Two thirds: wood 3*20/30 = 2, stone 5*20/30 = 3.
        assert_eq!(repair_cost("pickaxe", 20, max), Some(vec![("wood", 2), ("stone", 3)]));
        // Nearly spent, and still strictly cheaper than the 8-unit fresh craft.
        assert_eq!(repair_cost("pickaxe", 29, max), Some(vec![("wood", 2), ("stone", 4)]));
        // Entirely spent -> exactly the full recipe, never more.
        assert_eq!(repair_cost("pickaxe", max, max), Some(vec![("wood", 3), ("stone", 5)]));
        // Missing far more than max (shouldn't happen, but must not go
        // negative/overflow) clamps to the same as fully spent.
        assert_eq!(repair_cost("pickaxe", 999, max), Some(vec![("wood", 3), ("stone", 5)]));
        // Not a real tool/recipe -> None, not a panic.
        assert_eq!(repair_cost("wood", 5, max), None);
    }

    #[test]
    fn tool_registries_agree_with_each_other() {
        for item_id in ["pickaxe", "axe"] {
            assert!(tool_max_durability(item_id).is_some(), "{item_id} should have a durability cap");
            let abilities = abilities_for_item(item_id);
            assert_eq!(abilities.len(), 1, "{item_id} should grant exactly one ability");
            assert_eq!(governing_tool(abilities[0].id), Some(item_id), "governing_tool must invert abilities_for_item for {item_id}");
        }
        assert_eq!(tool_max_durability("wood"), None);
        assert_eq!(governing_tool("nonexistent"), None);
    }
}

