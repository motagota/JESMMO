-- Placed world props (player-attributes epic #83, issue #85): hand-authored
-- objects with gameplay meaning, placed and deleted live by the editor role.
-- First kind: "poison_tree" (the starting-area pen's poison forest, #88).
--
-- Deliberately NOT a terrain delta layer: #81's object-suppress layer is
-- about hiding baked scatter, whereas these are individually authored props
-- with identity -- a plain row each, world-unit coordinates (metres, the
-- same space as structure/resource_node), no chunk keying. The gateway
-- loads the whole table into an in-memory cache at boot (authored-forest
-- counts are tiny) and keeps it write-through.
--
-- `author` follows terrain_delta's AuthorId string form ("editor:<id>") so
-- provenance reads the same across both editing systems.
CREATE TABLE world_object (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    x          INTEGER NOT NULL,
    y          INTEGER NOT NULL,
    author     TEXT NOT NULL,
    created_at INTEGER NOT NULL   -- unix seconds
);
