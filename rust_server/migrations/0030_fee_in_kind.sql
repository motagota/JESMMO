-- Payment in goods rather than gold (business epic #179, issue #182).
--
-- The kiln takes one pot in five. It does three things a gold fee cannot:
--
--   * A player with no coin can still use a station, paying in the thing they
--     are making. That matters enormously in the first hour.
--   * It pays the owner in GOODS they must then sell, which pushes them into
--     the market rather than letting rent accrue passively.
--   * "One in five" needs no explanation. "4.5% of assessed output value"
--     needs a wiki.

-- 0.0-1.0. The share of a job's output the owner takes.
ALTER TABLE plot ADD COLUMN station_fee_in_kind REAL NOT NULL DEFAULT 0;

-- The value ceiling, in gold, on a single job's in-kind take. Zero means no
-- ceiling.
--
-- THIS IS THE PART #168's DESIGN DOC ASKED FOR AND DID NOT ANSWER. One pot in
-- five is a modest tax; one SWORD in five is confiscation. The same fraction
-- applied to `clay_lump` and to a high-value output are wildly different taxes,
-- and an owner setting 20% has no way to know which they have configured.
--
-- The issue proposed capping at "the gold equivalent of one unit", which on
-- inspection caps everything at one unit and makes the fraction meaningless for
-- any job bigger than five. A configured ceiling is what actually works: "one
-- in five, up to 50 gold". It is legible, it is the owner's choice, and it
-- degrades gracefully — an item with no reference price falls back to the plain
-- count rather than to a free station or an unbounded take.
--
-- The reference price is the PROVISIONER FLOOR, not the last trade. A cap keyed
-- to recent trades could be moved by making one absurd trade, which is a wash
-- trade with a purpose. The floor is configured (#170 set iron ore to 5),
-- stable, and cannot be manipulated by anyone playing.
ALTER TABLE plot ADD COLUMN station_fee_in_kind_max_gp INTEGER NOT NULL DEFAULT 0;
