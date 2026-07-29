-- Order expiry (market epic #136, issue #140).
--
-- A resting order holds escrow — goods for a sell, gold for a buy — so an
-- order nobody ever cancels would strand it forever. Expiry is the backstop:
-- at `expires_at` a sweep releases the escrow exactly as a cancel would.
--
-- Existing rows (placed by #139, before buys could rest) get 0, which the
-- sweep reads as "no expiry" rather than "expired long ago" — retro-expiring
-- orders that were placed under different rules would be a nasty surprise.
-- New orders always carry a real timestamp.
ALTER TABLE market_order ADD COLUMN expires_at INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_market_order_expiry ON market_order(expires_at);
