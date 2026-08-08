-- When each authored deposit was worked out (mine epic #164, issue #166).
--
-- ONE COLUMN OF RUNTIME STATE, and deliberately only one. Charges, yields,
-- respawn window and everything else are derivable from `crafting.toml` the
-- moment the zone loads, so the only thing that cannot be recomputed is *when*
-- a seam was emptied. Persisting that alone means a restart mid-cycle RESUMES
-- the timers rather than either refilling the whole mine (a free reset for
-- anyone who notices) or leaving it barren.
--
-- Absent row = the seam is full. That makes a fresh database, a newly authored
-- deposit, and a seam that has never been touched all the same case, with no
-- backfill to write and nothing to migrate when new seams are added to
-- `zones.toml`.
--
-- Rows are updated in place rather than appended: this is a cache of live world
-- state, not a ledger of what happened. Nothing downstream needs the history of
-- a rock, and an append-only table of every depletion would grow without bound
-- for no reader. (Contrast `gold_ledger`, which IS a ledger precisely because
-- the money supply needs auditing.)
CREATE TABLE IF NOT EXISTS deposit_state (
    -- The authored id from `zones.toml`, which is also the zone's entity id.
    deposit_id  TEXT PRIMARY KEY,
    -- Unix seconds the seam was emptied. The zone adds the type's respawn
    -- window (plus its jitter) to decide when it comes back.
    depleted_at INTEGER NOT NULL
);
