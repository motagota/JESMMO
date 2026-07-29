-- Market order book + trade ledger (market epic #136, issue #139).
--
-- `market_order` holds only OPEN orders — the book is rebuilt from it on boot,
-- so a filled or cancelled order is deleted rather than kept in a terminal
-- state. Nothing is lost by that: `market_trade` below is the permanent record.
--
-- `created_seq` is a monotonic SEQUENCE number, not a timestamp. Price-time
-- priority (#140) needs a total order over orders placed in the same
-- millisecond, and it must not be reorderable by a clock adjustment.
--
-- `side` carries 'sell' | 'buy' from day one though #139 only ever rests
-- sells (buys execute immediately against the book and never rest); #140 adds
-- resting buys with no schema change.
--
-- The commodity key is `item_id` ALONE — there is no quality system in this
-- game (epic #136's agreed adaptation), and only stackable items are
-- commodities at all. Unique items (tools, which carry per-instance
-- durability) go to the listing board instead (#142).
CREATE TABLE IF NOT EXISTS market_order (
    id            TEXT PRIMARY KEY,
    market_id     TEXT NOT NULL REFERENCES build_order(id),
    character_id  TEXT NOT NULL REFERENCES character(id),
    side          TEXT NOT NULL,
    item_id       TEXT NOT NULL,
    unit_price    INTEGER NOT NULL,
    qty_total     INTEGER NOT NULL,
    qty_remaining INTEGER NOT NULL,
    created_seq   INTEGER NOT NULL,
    created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_market_order_book
    ON market_order(market_id, item_id, side, unit_price, created_seq);

-- APPEND-ONLY. Never updated, never deleted. This is the audit log, the proof
-- that goods and gold were conserved, and the sole source for price history
-- (#143) — so treat any UPDATE or DELETE against it as a bug.
--
-- `unit_price` is the EXECUTION price (the resting order's), not the
-- aggressor's limit: price improvement goes to whoever crossed the spread.
-- `sale_tax_gold`/`listing_fee_gold` are reserved for #141 and stay 0 until
-- fees exist, so fee revenue is auditable from the same ledger.
CREATE TABLE IF NOT EXISTS market_trade (
    id               TEXT PRIMARY KEY,
    market_id        TEXT NOT NULL,
    item_id          TEXT NOT NULL,
    unit_price       INTEGER NOT NULL,
    qty              INTEGER NOT NULL,
    seller_id        TEXT NOT NULL,
    buyer_id         TEXT NOT NULL,
    sale_tax_gold    INTEGER NOT NULL DEFAULT 0,
    listing_fee_gold INTEGER NOT NULL DEFAULT 0,
    created_at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_market_trade_history
    ON market_trade(market_id, item_id, created_at);

-- Client-generated command ids, so a reconnect-and-resend can't place the same
-- order twice. Insert-or-ignore: a second sighting of an id is a no-op, not an
-- error, because the honest case (the ack was lost, the client retried) and the
-- dishonest one are indistinguishable and both want the same answer.
CREATE TABLE IF NOT EXISTS market_command (
    command_id   TEXT PRIMARY KEY,
    character_id TEXT NOT NULL,
    created_at   INTEGER NOT NULL
);
