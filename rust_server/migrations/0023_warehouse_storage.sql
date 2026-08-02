-- Warehouse storage accounts (market phase 2 epic #151, issue #155): the
-- bookkeeping behind a daily holding cost on stored goods.
--
-- SHIPS DISABLED. `storage_fee_per_slot_per_day` defaults to 0, so on a stock
-- server nothing here is ever charged and no row is ever written. The mechanism
-- exists so the lever can be pulled from `market.toml` if hoarding becomes a
-- problem; nobody is hoarding yet, and taxing players for using a feature they
-- were just given is a bad first impression. This table is the machinery, not
-- the policy.
--
-- One row per (market, character) that has ever been charged. Absent means
-- "never charged", which is the correct starting state and needs no backfill.
--
-- `last_charged_at` is what makes the job IDEMPOTENT: a run inside the same day
-- as the last charge is a no-op, so a restart loop cannot bill anyone twice.
--
-- `arrears` is the debt owed when a purse could not cover the charge, and it is
-- the whole reason this table exists rather than just deducting gold. The
-- tempting alternatives are all bad:
--
--   * CONFISCATING goods to settle a debt is an unrecoverable loss caused by
--     not logging in, and the fastest possible way to make players distrust the
--     warehouse. Never done, at any arrears level.
--   * UNBOUNDED debt is the same thing wearing a different hat: someone
--     returning after a month to a bill they can never clear has effectively
--     lost the goods. Arrears are capped at `storage_arrears_cap_days` worth,
--     so the debt is always payable.
--
-- What happens instead: charge what the purse can cover, and when it can't,
-- stop accruing and LOCK the warehouse — deposits and withdrawals refused with
-- a reason — until the arrears are cleared. The goods stay safe and the player
-- keeps agency. `arrears > 0` IS the lock; there is no separate flag to get out
-- of step with it.
--
-- Charges are BURNED like every other market fee (#141): they land on
-- `market_fee` with kind 'storage' and on `gold_ledger` (#154), so the holding
-- cost is measurable next to the listing fee and sale tax rather than in a
-- parallel universe.
CREATE TABLE IF NOT EXISTS market_warehouse_account (
    market_id       TEXT NOT NULL,
    character_id    TEXT NOT NULL,
    -- Unix seconds of the last charge. Also the anchor for "has a day passed".
    last_charged_at INTEGER NOT NULL,
    -- Unpaid storage debt, capped. Nonzero means the warehouse is locked.
    arrears         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (market_id, character_id)
);
CREATE INDEX IF NOT EXISTS idx_warehouse_account_arrears
    ON market_warehouse_account(arrears);
