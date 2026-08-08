-- Foreman Marlow's handouts and the tutorial track (mine epic #164, issue #169).
--
-- Two tables: what a player has DONE, and what they have been GIVEN.

-- Running counts of the things the track watches — clay gathered, ingots made,
-- fuel loaded.
--
-- COUNTED FOR EVERY PERSISTENT CHARACTER FROM LOGIN, whether or not they have
-- ever met Marlow. That is the whole reason this is a counter table rather than
-- a per-player copy of the track: "a player who completed a step before ever
-- talking to him has it already ticked" is only true if there was never a start
-- event to miss. There is no "tutorial accepted" row anywhere, and that absence
-- is deliberate — the track is a set of hints that tick themselves, not a quest
-- you enrol in.
--
-- Nothing is gated behind it. A player who ignores Marlow entirely can mine,
-- smelt, throw pots and sell them exactly as well as one who follows him; the
-- only thing they miss is the reward at the end.
--
-- Only items some condition actually names are counted (`counted_items` in
-- tutorial_config.rs), so the gather path pays a set lookup and nothing more
-- for the overwhelming majority of items.
CREATE TABLE IF NOT EXISTS tutorial_counter (
    character_id TEXT NOT NULL,
    -- 'gained:<item>' | 'made:<item>' | 'loaded_fuel'. A namespaced string
    -- rather than a column per event, because the set of watched events is
    -- config and the schema should not have to change when the track does.
    event        TEXT NOT NULL,
    count        INTEGER NOT NULL DEFAULT 0,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (character_id, event)
);

-- When each handout last fired, which is what enforces both the cooldown and
-- the once-ever bundle.
--
-- A LOG RATHER THAN A FLAG, keyed by (character, npc, item), because the two
-- rules need different things from it: `once` asks whether a row exists at all,
-- the cooldown asks how old it is. One shape answers both, and a boolean would
-- have needed a second column the moment the second handout arrived.
CREATE TABLE IF NOT EXISTS handout_log (
    character_id TEXT NOT NULL,
    npc_id       TEXT NOT NULL,
    item_id      TEXT NOT NULL,
    granted_at   INTEGER NOT NULL,
    times        INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (character_id, npc_id, item_id)
);
