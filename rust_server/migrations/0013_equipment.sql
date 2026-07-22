-- Equipment (mining/abilities epic #123, issue #116): a character's "in
-- hand"/worn items. Only `slot = 'tool'` is used today (arming a pickaxe
-- puts the Pick ability on the hotbar), but the table is keyed by slot from
-- day one so a future paper-doll (weapon/head/chest/...) needs no schema
-- change — just more slot values and item registry entries.
CREATE TABLE IF NOT EXISTS equipment (
    character_id  TEXT NOT NULL REFERENCES character(id),
    slot          TEXT NOT NULL,   -- "tool" | ... (future: "weapon", "head", ...)
    item_id       TEXT NOT NULL,
    PRIMARY KEY (character_id, slot)
);
