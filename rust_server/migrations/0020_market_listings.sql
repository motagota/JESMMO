-- Listing board for unique items (market epic #136, issue #142).
--
-- Two pickaxes at different durability are not the same good, so a unique item
-- can't have an order book — "the price of a pickaxe" is meaningless. Uniques
-- are instead offered individually at a fixed ask and bought outright.
-- (`world::is_commodity` draws the line: anything that stacks goes to the book,
-- anything that doesn't comes here.)
--
-- The listed item is ESCROWED, not flagged: it moves to `locked` in the
-- seller's warehouse at this market, exactly as a sell order's goods do (#138/
-- #139). `warehouse_item_id` points at that row, whose id was carried over
-- unchanged from the seller's `inventory_item` row when they deposited it
-- (#128) — so the thing on sale is provably the same worn tool the whole way
-- through, and a purchase can simply hand that row to the buyer rather than
-- destroying and recreating it.
--
-- The row EXISTING is what "open" means. A purchase, cancel, or expiry deletes
-- it, which makes first-come purchase a compare-and-clear: whoever's DELETE
-- reports a row affected won, and everyone else lost — no partial charge, no
-- state column to get out of step. Sold listings aren't lost history: the sale
-- lands on the append-only `market_trade` ledger like any other fill.
--
-- `durability` is denormalised from the warehouse row so the board can be
-- filtered and sorted without joining every candidate.
CREATE TABLE IF NOT EXISTS market_listing (
    id                TEXT PRIMARY KEY,
    market_id         TEXT NOT NULL,
    seller_id         TEXT NOT NULL,
    warehouse_item_id TEXT NOT NULL,
    item_id           TEXT NOT NULL,
    durability        INTEGER,
    ask_price         INTEGER NOT NULL,
    created_at        INTEGER NOT NULL,
    expires_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_market_listing_board
    ON market_listing(market_id, item_id, ask_price);
CREATE INDEX IF NOT EXISTS idx_market_listing_expiry ON market_listing(expires_at);
