# MMO Design Document

*Working title: TBD*

A player-built MMO where the world starts empty and the players write its history. Safety is a social contract, risk is opt-in, and land is the economy.

---

## 1. Pillars / Vision

- **The world is player-made.** The capital starts as an empty safe zone with nothing in it. Every building, road, and wall exists because players built it via build orders. The server's history is authored by its population.
- **Safety is a contract, risk is opt-in.** Players are protected in the capital and choose when to step into the dangerous wilds. Danger is never forced on new players.
- **Land is the economy.** Plots, rent, scarcity, and territory drive the core loop and the politics.
- **Progression through play.** Skills are gained by doing, permanently. No grind-to-maintain.

---

## 2. Three-Zone Risk Model

| Zone | Safety | Loot risk | Run by | Land |
|------|--------|-----------|--------|------|
| **Capital** | Full safe | None | Admins | Scarce / premium |
| **Player cities** | Safe-ish (protected) | Protected belongings | Guild / player government | Built in the wilds |
| **Wilds** | None | **Full-loot** | Nobody | Abundant |

- **Capital** — Admin-run neutral anchor. Baseline safe hub. Sets the rules everyone plays under. Starts empty; built by players.
- **Player cities** — Founded by guilds/players out in the wilds. Self-governed. Provide their own safety/services in exchange for being part of that city's policy.
- **Wilds** — The PvP ring and beyond. Go out, risk everything, claim land, do as you will.

A clean risk gradient: the further from the capital, the more freedom and the more danger.

---

## 3. Core Loop

```
Gather resources → Fulfil build order (quest) → City/structure grows
        ↑                                              ↓
        └──────── unlocks more advanced builds ────────┘
```

- The **king / city policy** issues **build orders** (walls, barracks, markets, roads, a town well…).
- Build orders require **resources**, which turns resource-gathering into the **quest content**.
- Completing builds grows the city and **unlocks more advanced structures and designs**.
- A good *first* build order is something communal and visible (a well, a wall section, a market stall) so players watch the capital physically grow from their work.

---

## 4. Land & Rent System

**Starter plot**
- Every new player receives a generous starter plot on arrival.
- For MVP the starter plot is **inside / near the safe capital**.
- Players can build a basic home: **bed** (respawn anchor), **storage** (safe stash), and a **crafting station**.

**Rent**
- Land is the rented asset; **your belongings and cosmetics are owned**.
- If rent lapses, the **land is reclaimed**, but:
  - Player belongings are **moved to storage (safe)**.
  - **House flair / décor is protected** (it was purchased).
- This makes rent a sink and keeps land circulating, without ever destroying what a player paid for.

**Plot sizing**
- Plots **start small** for everyone.
- **Larger plots are purchasable** — a prestige/status lever and a wealth sink. Scarcity of big central plots drives the real-estate economy.

**Settled**
- **All flair is saved (always protected).** Décor functions as a safe wealth store outside the loot economy. Not lootable, not destructible.

**Open design question**
- **Where does reclaimed land / rent go?** If it returns to a pool, rent is a pure gold-sink. If cities *collect* rent, that's a revenue stream guilds will fight over — good for politics, but city ownership must then be contestable.

**City growth stages**
- **Stage 1 — Starter suburbs.** The first residential ring. Small plots, basic homes. This is where the city begins.
- **Stage 2+ — High-rise high-density living** unlocks later, once the city is built up. Vertical expansion lets the capital absorb far more residents than its footprint would suggest.

---

## 5. Skills

- **Use-based progression** — skills level by doing them (gather, craft, build, fight).
- **No decay.** Permanent. Specialization is a choice driven by time investment, not a punishment.
- Higher skill **unlocks more advanced building and design** options.

---

## 6. Governance

- **Guilds and player-cities choose their own political style** (monarchy, council, etc.).
- The **capital is admin-run** — the neutral baseline hub and ruleset everyone shares.
- *(Future)* The king/leadership of player settlements may be player-elected, guild-owned, or conquerable — TBD.

---

## 7. Technical Notes

**Client**
- **Godot, 3D.** The client is built in Godot with a 3D world.

**Population**
- Target: **1,000,000 total accounts** (lifetime registered), *not* concurrent.
- Realistic concurrency is in the **thousands** (typically a low single-digit % of total accounts).

**Capital footprint**
- **~40 km²**, split across **multiple zones** (each a simulation/streaming boundary holding a few hundred to low-thousands concurrent).
- 40 km² @ 1M people ≈ 25,000 people/km² — real-world dense-city density, plausible as a *shared hub*, not as personal suburbia.

**Plot math (reality check)**

| Plot size | Plots in 40 km² | Notes |
|-----------|-----------------|-------|
| 10×10 m | ~400,000 | Tiny |
| 20×20 m | ~100,000 | Modest house + yard |
| 32×32 m | ~39,000 | Generous, but only ~39k plots |

- Before roads, walls, communal build-order structures, and the capital's own footprint, ~30–50% of land isn't plottable.
- **Conclusion:** the capital can't give 1M players a personal plot. That's by design — capital plots are **scarce/premium prestige real estate**; the **wilds** are where land is abundant. Rent + scarcity do real economic work.

**Zoning**
- **Gated zones** (brief load between districts) for MVP — much simpler, totally acceptable for an MMO capital.
- **Seamless streaming** (no loading screens, cross-zone handoff for players/entities/line-of-sight) is a later upgrade.

---

## 8. Roadmap

Build the city first. The wilds come later — until then, the wilds can be a literal wall you can't cross.

### Phase 1 — The Capital (MVP)

A complete, playable city-builder MMO core with **zero PvP**.

- [ ] Empty safe capital with a build-order system (king/policy issues orders).
- [ ] Resource gathering feeding build orders (the quest loop).
- [ ] Starter plot on arrival inside/near the capital (starter suburbs).
- [ ] Basic player home: bed (respawn), storage (safe stash), crafting station.
- [ ] Use-based skills, no decay.
- [ ] Rent system: lapse → reclaim land, belongings to storage, flair protected.
- [ ] Gated zone transitions.

### Phase 2 — The Wilds

Open the gates once the capital core is proven.

- [ ] One wilds zone with full-loot PvP and basic land claiming.
- [ ] Player-founded cities with self-governance.

**Deferred (later)**
- Hardcore "start in the wilds" option.
- High-rise high-density living (city growth stage 2+).
- Player-to-player property market (buy/sell/move homes).
- Contestable capital/city ownership and rent-revenue politics.
- Seamless zone streaming.

---

## Open Decisions Tracker

1. Where reclaimed land / collected rent goes (pool vs. city revenue).
2. Bed-as-respawn behaviour if homes are ever raidable.
3. Player-settlement leadership model (elected / owned / conquerable).
4. Working title for the game.