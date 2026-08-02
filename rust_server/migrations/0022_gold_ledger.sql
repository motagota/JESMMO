-- Every gold that has ever existed, and why (market phase 2 epic #151, issue
-- #154).
--
-- APPEND-ONLY. Rows are never updated or deleted.
--
-- WHY THIS HAD TO EXIST BEFORE THE PROVISIONER
-- Gold was being created by a bare `UPDATE character SET gold = gold + ?` that
-- recorded nothing. Characters mint 500 on creation, and #145's build wages
-- mint more on every contribution — so the honest answer to "how much gold
-- exists in this world, and where did it come from" was: nobody knows. The
-- conservation test only held because no wages were paid during it.
--
-- #154 adds a SECOND, higher-volume faucet: an NPC that buys at a price floor
-- must be able to buy without limit, or the floor stops being a floor exactly
-- when it is needed. Stacking an unmeasured faucet on an unmeasured faucet is
-- how an economy gets away from you invisibly, so the measurement comes first.
--
-- SIGNED, so one table answers the whole question. Positive rows create gold,
-- negative rows destroy it, and the sum IS the money supply:
--
--     SUM(gold_ledger.amount) == SUM(character.gold) + escrowed gold
--
-- with escrow being the open buy book (gold deducted from a purse and held by
-- an unfilled order — absent from purses, but not destroyed). That identity is
-- asserted by `gold_is_conserved_against_the_ledger`, and it is the thing that
-- makes "the economy is balanced" a checkable claim rather than a hope.
--
-- RELATIONSHIP TO `market_fee` (#141). That table stays: it is the market's own
-- detail — which market, which order or trade, which fee kind — and answers
-- "what did this market take from whom". This table answers only "how much gold
-- exists". A burned fee writes BOTH, in the same transaction, and
-- `fee_ledgers_agree` pins them together so the pair cannot drift.
--
-- `reason` is a stable, low-cardinality string rather than an enum column so a
-- new source needs no migration:
--   character_start  +  the flat balance a new character is created with
--   build_wage       +  the city paying for construction (#145)
--   provisioner      +  float minted for the NPC's standing bid (#154)
--   market_fee       -  a listing fee or sale tax burned (#141)
--   rent             -  rent paid to the city and destroyed (#14)
CREATE TABLE IF NOT EXISTS gold_ledger (
    id           TEXT PRIMARY KEY,
    character_id TEXT NOT NULL,
    -- Positive = created, negative = destroyed. Never zero.
    amount       INTEGER NOT NULL,
    reason       TEXT NOT NULL,
    created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_gold_ledger_reason ON gold_ledger(reason, created_at);
CREATE INDEX IF NOT EXISTS idx_gold_ledger_character ON gold_ledger(character_id, created_at);
