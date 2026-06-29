-- Schema v1, gameplay slice (issue #1).
--
-- The M0 migration (0001) covered durable identity: account + character. This
-- migration adds the rest of the phase1.md §2.1 schema so every later milestone
-- has a durable home for its state from day one: skills, inventory, storage,
-- plots, structures, flair, build orders, and resource nodes.
--
-- These tables are written through by the typed repository functions in
-- persistence/mod.rs. They are intentionally landed now (ahead of the gameplay
-- systems that fill them) so the schema is reviewable and stable before features
-- depend on it.

-- Use-based progression. One row per (character, skill); level is derived from xp
-- via a fixed curve (see persistence::level_for_xp) and cached here for cheap reads.
CREATE TABLE IF NOT EXISTS skill (
    character_id  TEXT NOT NULL REFERENCES character(id),
    skill_id      TEXT NOT NULL,              -- "gathering" | "crafting" | "building" | ...
    xp            INTEGER NOT NULL DEFAULT 0,
    level         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (character_id, skill_id)
);

-- Carried items (finite slots). `slot` is the inventory grid position (nullable
-- until the gameplay layer assigns one).
CREATE TABLE IF NOT EXISTS inventory_item (
    id            TEXT PRIMARY KEY,
    character_id  TEXT NOT NULL REFERENCES character(id),
    item_id       TEXT NOT NULL,
    qty           INTEGER NOT NULL,
    slot          INTEGER
);
CREATE INDEX IF NOT EXISTS idx_inventory_character ON inventory_item(character_id);

-- Safe home stash (large, unslotted). One row per (character, item) — quantities
-- stack rather than occupying slots.
CREATE TABLE IF NOT EXISTS storage_item (
    id            TEXT PRIMARY KEY,
    character_id  TEXT NOT NULL REFERENCES character(id),
    item_id       TEXT NOT NULL,
    qty           INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_storage_character ON storage_item(character_id);

-- Rented land. Pre-seeded as unowned (owner_character_id NULL, state 'unowned')
-- by world authoring; claimed on first login. Rent timers drive the land sink.
CREATE TABLE IF NOT EXISTS plot (
    id                  TEXT PRIMARY KEY,
    owner_character_id  TEXT REFERENCES character(id),   -- NULL while unowned
    district            TEXT NOT NULL,
    grid_x              INTEGER NOT NULL,
    grid_y              INTEGER NOT NULL,
    w                   INTEGER NOT NULL,
    h                   INTEGER NOT NULL,
    tier                INTEGER NOT NULL DEFAULT 0,
    rent_due_at         INTEGER,                          -- unix seconds; NULL while unowned
    rent_paid_through   INTEGER,
    state               TEXT NOT NULL DEFAULT 'unowned'   -- unowned|active|lapsed|reclaimed
);
CREATE INDEX IF NOT EXISTS idx_plot_owner ON plot(owner_character_id);
CREATE INDEX IF NOT EXISTS idx_plot_district_state ON plot(district, state);

-- Player-built structures, owned via their plot. bed|storage|crafting|wall|...
-- `data` is an opaque JSON blob for kind-specific fields.
CREATE TABLE IF NOT EXISTS structure (
    id        TEXT PRIMARY KEY,
    plot_id   TEXT NOT NULL REFERENCES plot(id),
    kind      TEXT NOT NULL,
    x         INTEGER NOT NULL,
    y         INTEGER NOT NULL,
    rot       INTEGER NOT NULL DEFAULT 0,
    hp        INTEGER NOT NULL DEFAULT 0,
    built_by  TEXT REFERENCES character(id),
    data      TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_structure_plot ON structure(plot_id);

-- Décor. Always owned by the character (never destroyed on rent lapse), placed on
-- a plot. Tracked separately from `structure` precisely so reclaim can preserve it.
CREATE TABLE IF NOT EXISTS flair (
    id                  TEXT PRIMARY KEY,
    owner_character_id  TEXT NOT NULL REFERENCES character(id),
    plot_id             TEXT REFERENCES plot(id),
    item_id             TEXT NOT NULL,
    x                   INTEGER NOT NULL,
    y                   INTEGER NOT NULL,
    rot                 INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_flair_owner ON flair(owner_character_id);

-- District-scoped city build quests. Contributions are pooled (progress_json), and
-- completion spawns structures and unlocks dependents. Owned by the city authority,
-- not a single zone process.
CREATE TABLE IF NOT EXISTS build_order (
    id            TEXT PRIMARY KEY,
    district      TEXT NOT NULL,
    kind          TEXT NOT NULL,
    required_json TEXT NOT NULL DEFAULT '{}',  -- item costs
    progress_json TEXT NOT NULL DEFAULT '{}',  -- contributed so far
    state         TEXT NOT NULL DEFAULT 'open', -- open|completed|locked
    issued_at     INTEGER NOT NULL,
    completed_at  INTEGER
);
CREATE INDEX IF NOT EXISTS idx_build_order_district ON build_order(district);

-- Gatherable nodes (trees, stone, ore). May be cache-only at runtime, but the
-- authored spawns and respawn timers live here.
CREATE TABLE IF NOT EXISTS resource_node (
    id          TEXT PRIMARY KEY,
    district    TEXT NOT NULL,
    item_id     TEXT NOT NULL,
    x           INTEGER NOT NULL,
    y           INTEGER NOT NULL,
    qty         INTEGER NOT NULL,
    respawn_at  INTEGER                        -- unix seconds; NULL when full/available
);
CREATE INDEX IF NOT EXISTS idx_resource_node_district ON resource_node(district);
