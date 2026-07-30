-- Market warehouse (market epic #136, issue #138): custody of goods held AT a
-- market, per player. Goods are local in this design — you deposit stock to
-- sell it and you collect purchases where you bought them, so a warehouse is
-- scoped to `(market_id, character_id)` and nothing teleports between markets.
--
-- Deliberately its own table rather than a reuse of `storage_item`: that one is
-- the character's single safe home stash with no market scoping, no lock
-- concept, and no per-instance state. This one needs all three.
--
-- `state` is 'available' | 'locked'. Locked stock is spoken for by an open sell
-- order (#139) and can't be withdrawn; escrow lives here rather than in a
-- separate table so a deposit that's been half-committed to an order is still
-- one row, and the conservation invariant (carried + available + locked is
-- constant) is checkable with one query.
--
-- `durability` mirrors `inventory_item.durability` (#128): NULL for an ordinary
-- stackable, 0..max for one tool instance (qty always 1, never merged). `id` is
-- carried over UNCHANGED from the `inventory_item` row on deposit and restored
-- on withdraw, so a tool is literally the same instance the whole way through —
-- which is what lets the listing board (#142) sell a specific worn pickaxe.
CREATE TABLE IF NOT EXISTS market_warehouse_item (
    id           TEXT PRIMARY KEY,
    market_id    TEXT NOT NULL REFERENCES build_order(id),
    character_id TEXT NOT NULL REFERENCES character(id),
    item_id      TEXT NOT NULL,
    qty          INTEGER NOT NULL,
    state        TEXT NOT NULL DEFAULT 'available',
    durability   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_warehouse_owner
    ON market_warehouse_item(market_id, character_id);
