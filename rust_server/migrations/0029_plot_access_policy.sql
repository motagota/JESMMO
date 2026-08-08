-- Access policy: one player charging another (business epic #179, issue #181).
--
-- Nothing in this game has ever let one player charge another. Every fee that
-- exists — market listing fees, sale tax, warehouse storage, the station usage
-- fee — is charged by the WORLD and burned. This is the first gold that moves
-- from one purse to another, and it is the whole point of the capital layer.
--
-- THE DISTINCTION THAT MUST NOT BE GOT WRONG. A burned fee is recorded on
-- `gold_ledger` because gold leaves the world; a transferred fee is NOT,
-- because none is created or destroyed. Routing a transfer through
-- `mint_gold_in_tx` would inflate the recorded supply against unchanged purses
-- and break the #154 identity silently, in a direction nobody notices until the
-- money supply is visibly wrong. `grant_gold_in_tx` moves without ledgering,
-- and that is exactly why the two were split.
--
-- Columns on `plot` rather than a table of their own: this is 1:1 with a plot,
-- always present, and read on the same query that already fetches the plot for
-- every station proximity check.

-- "closed" | "public" | "fee" | "roster"
--
-- DEFAULT CLOSED, deliberately. A plot that silently became usable by strangers
-- the moment a station went up would be a nasty surprise, and the safe default
-- costs an owner one deliberate action to change.
ALTER TABLE plot ADD COLUMN station_mode TEXT NOT NULL DEFAULT 'closed';

-- Flat gold per station use, paid by the user to the OWNER.
ALTER TABLE plot ADD COLUMN station_fee_gp INTEGER NOT NULL DEFAULT 0;

-- Minimum skill to use the owner's stations. This exists to stop wastage, not
-- to gate content: a spoiled shaping job (#168) consumes the clay, and an owner
-- letting novices burn their fuel has a real cost. Checked BEFORE anything is
-- consumed.
ALTER TABLE plot ADD COLUMN station_skill_floor INTEGER NOT NULL DEFAULT 0;
