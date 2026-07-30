-- Market fees (market epic #136, issue #141): the coin sink.
--
-- APPEND-ONLY, like `market_trade`. Never updated, never deleted.
--
-- Fees are BURNED at the capital market — the gold leaves the economy — so
-- there is no account they land in and nothing to reconcile them against
-- except this ledger. That makes it the only record of how much the sink has
-- actually removed, which the conservation invariant needs
-- (purses + escrow + burned is constant) and which is the honest answer to
-- "where did the money go".
--
-- Two kinds:
--   'listing'  — charged to BOTH sides at placement, on notional. Never
--                refunded on cancel or expiry; that's what makes posting an
--                order you don't mean to honour cost something.
--   'sale_tax' — charged to the seller out of each fill's proceeds.
--
-- `order_id`/`trade_id` are loose references, not foreign keys: a listing fee
-- outlives its order (orders are deleted on fill/cancel, #139) and must stay
-- auditable afterwards, which an FK would forbid.
--
-- Phase 2 (#144) wants these credited to a city treasury rather than burned.
-- Recording who paid, where, and why is what makes that a change of
-- destination rather than a rebuild.
CREATE TABLE IF NOT EXISTS market_fee (
    id           TEXT PRIMARY KEY,
    market_id    TEXT NOT NULL,
    character_id TEXT NOT NULL,
    kind         TEXT NOT NULL,
    gold         INTEGER NOT NULL,
    order_id     TEXT,
    trade_id     TEXT,
    created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_market_fee_market ON market_fee(market_id, created_at);
