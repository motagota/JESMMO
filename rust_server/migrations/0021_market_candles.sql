-- Price history (market epic #136, issue #143): OHLCV candles per
-- (market, commodity, time bucket).
--
-- This table is a DERIVED CACHE, not a source of truth. `market_trade` is the
-- append-only record of what actually happened; every candle here can be
-- recomputed from it, and a test asserts a from-scratch rebuild reproduces
-- byte-identical rows. Nothing should ever write a candle that isn't derivable
-- from the ledger, and losing this table entirely should cost only CPU.
--
-- Keeping it materialised rather than querying the ledger live is what keeps
-- the rollup off the matching path: it's a periodic background job, so a trade
-- never waits on aggregation.
--
-- `bucket_start` is the UNIX second the interval opens, always a multiple of
-- the interval — so a bucket has exactly one identity and a trade can only
-- land in one. `interval_secs` is part of the key so several resolutions can
-- coexist later without a migration.
--
-- Absent buckets mean NO TRADES, and must render as a gap. Writing a flat
-- carried-forward candle would invent a price nobody paid.
CREATE TABLE IF NOT EXISTS market_candle (
    market_id     TEXT NOT NULL,
    item_id       TEXT NOT NULL,
    interval_secs INTEGER NOT NULL,
    bucket_start  INTEGER NOT NULL,
    open          INTEGER NOT NULL,
    high          INTEGER NOT NULL,
    low           INTEGER NOT NULL,
    close         INTEGER NOT NULL,
    volume        INTEGER NOT NULL,
    trades        INTEGER NOT NULL,
    PRIMARY KEY (market_id, item_id, interval_secs, bucket_start)
);
CREATE INDEX IF NOT EXISTS idx_market_candle_read
    ON market_candle(market_id, item_id, interval_secs, bucket_start);
