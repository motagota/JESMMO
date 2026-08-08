-- The roster: who else may use your plot's stations (business epic #179, #183).
--
-- Four roles, deliberately few. The permission model is small on purpose and
-- should stay that way until a second thing needs it.
--
--   owner    lease-derived, never a row here (see below)
--   manager  use, deposit, take output, pay rent — everything but policy
--   worker   use and deposit; takes nothing out
--   patron   use only, and pays the fee for it
--
-- THE OWNER IS NOT IN THIS TABLE. Their access comes from the lease, because a
-- grant can expire and an owner locked out of their own storage by a lapsed row
-- would be absurd. It also means transferring a lease transfers control without
-- a roster migration.
--
-- EVERY GRANT EXPIRES. Default 30 days, renewable. This is the same reasoning
-- as P4 — all ownership is a lease — and the reason is concrete: without expiry
-- a guild that stops playing leaves a plot rostered to people who will never
-- log in again, and the owner's own lease lapsing is the only thing that ever
-- clears it.
--
-- Expiry is checked AT USE, not only by a sweep. A sweep is a convenience; the
-- authoritative question is "is this grant live right now", and anything else
-- means a lapsed grant keeps working until a timer happens to fire.
CREATE TABLE IF NOT EXISTS plot_grant (
    plot_id      TEXT NOT NULL,
    character_id TEXT NOT NULL,
    -- 'manager' | 'worker' | 'patron'. Never 'owner'.
    role         TEXT NOT NULL,
    granted_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    PRIMARY KEY (plot_id, character_id)
);

-- The roster panel asks "who is on this plot"; the access gate asks "is this
-- person on it". One row per pair serves both, and the primary key means
-- granting twice updates rather than duplicating.
CREATE INDEX IF NOT EXISTS idx_plot_grant_character ON plot_grant (character_id);
