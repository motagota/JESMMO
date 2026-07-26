-- Tool durability & instancing (mining/abilities epic #123 backlog, #128):
-- tools stop being simple stack counts and become individually tracked
-- instances — two pickaxes can be at different wear, so "the pickaxe" is
-- no longer a well-defined thing once you own more than one.
--
-- `inventory_item.durability` is NULL for an ordinary stackable item (wood,
-- stone, ...) and an integer 0..max for a tool instance. A tool row's `qty`
-- is always 1 — instances never merge/stack, unlike ordinary items.
--
-- `equipment.instance_id` names exactly which owned instance is worn, since
-- `item_id` alone ("a pickaxe") is now ambiguous. NULL until something's
-- equipped.
ALTER TABLE inventory_item ADD COLUMN durability INTEGER;
ALTER TABLE equipment ADD COLUMN instance_id TEXT REFERENCES inventory_item(id);
