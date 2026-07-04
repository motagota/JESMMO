-- Rent economy: currency + auto-pay opt-in (issue #14).
--
-- `gold` gets a flat starting balance for every character (there's no earning
-- mechanic yet in Phase 1 — see phase1.md §4.7's open decision on the rent sink).
-- `auto_pay` is per-plot and opt-in: the rent ticker only auto-deducts gold for
-- plots that have explicitly enabled it; otherwise the owner must `rent.pay`
-- manually or the plot lapses on schedule regardless of balance. `warned` tracks
-- whether `rent.warning` has already fired for the current due cycle, so the
-- ticker doesn't re-send it every tick within the warning window.
ALTER TABLE character ADD COLUMN gold INTEGER NOT NULL DEFAULT 500;
ALTER TABLE plot ADD COLUMN auto_pay INTEGER NOT NULL DEFAULT 0;
ALTER TABLE plot ADD COLUMN warned INTEGER NOT NULL DEFAULT 0;
