-- Build-order contributions (issue #9).
--
-- Build orders (0002) pool item costs in build_order.progress_json. This migration
-- records *who* contributed and how much, so that on completion the building XP can
-- be granted lump-sum to each contributor, split by units. One row per
-- (order, character); `units` is the running total of items that character has
-- contributed to that order. Written through by persistence::contribute.
CREATE TABLE IF NOT EXISTS build_contribution (
    order_id     TEXT NOT NULL REFERENCES build_order(id),
    character_id TEXT NOT NULL REFERENCES character(id),
    units        INTEGER NOT NULL DEFAULT 0,   -- total items this char contributed
    PRIMARY KEY (order_id, character_id)
);
CREATE INDEX IF NOT EXISTS idx_build_contribution_order ON build_contribution(order_id);
