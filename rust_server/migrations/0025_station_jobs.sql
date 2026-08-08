-- Stations: shared fuel buffers and per-player timed jobs (mine epic #164, #167).
--
-- Two tables, and the split between them IS the design.
--
-- `station_fuel` is keyed by station alone: fuel is a SHARED resource on a
-- public furnace. Whoever loads charcoal is heating the fire for whoever is
-- standing there. That is a deliberate choice, not an oversight — per-player
-- fuel buffers would turn a communal fire into a row of private ones and delete
-- the only cooperative surface the mine has.
--
-- `station_job` is keyed by (station, character, slot): SLOTS ARE PER PLAYER.
-- A queue shared across everyone on a public station is a griefing surface and
-- a miserable wait, so each player gets their own slots on the same physical
-- object. This is what makes a public station usable at all.
--
-- Fuel shared, slots private. The two answers differ because the two problems
-- do: fuel is a consumable anyone can replace, a slot is time you can't.

-- How much fuel each station is holding, in fuel UNITS rather than items.
-- Units, because what a fuel item is worth is a config number
-- (`[station.furnace.fuels] charcoal = 2`) and storing items would bake today's
-- exchange rate into the database.
CREATE TABLE IF NOT EXISTS station_fuel (
    -- The authored id from `zones.toml`, e.g. `furnace_mine_yard`.
    station_id TEXT PRIMARY KEY,
    units      INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);

-- One row per running or finished-but-uncollected job.
--
-- THE INPUTS ARE ESCROWED HERE. They are taken out of the player's inventory at
-- START and held in this row until collect — the same custody model the market
-- warehouse runs on (#138), and copied on purpose rather than reinvented. A
-- crash, a logout or a restart between start and collect must neither duplicate
-- the materials nor void them, and the only way to guarantee that is for
-- exactly one place to own them at any instant.
--
-- Fuel is escrowed the same way: `fuel_units` records what this job took from
-- the shared buffer, so a job that fails refunds precisely what it reserved
-- rather than what its recipe currently claims to cost.
CREATE TABLE IF NOT EXISTS station_job (
    id           TEXT PRIMARY KEY,
    station_id   TEXT NOT NULL,
    character_id TEXT NOT NULL,
    -- 0-based, bounded by the station type's `job_slots`. Part of the unique
    -- key below, which is what actually enforces "no two jobs in one slot"
    -- rather than a read-then-write in application code.
    slot         INTEGER NOT NULL,
    -- A key into `crafting.toml`'s `[recipe.*]`. It can VANISH: the file is
    -- edited between restarts and this row outlives it. A job whose recipe no
    -- longer exists is failed and its escrow refunded, never panicked over.
    recipe_id    TEXT NOT NULL,
    -- The escrowed inputs as JSON `[[item_id, qty], ...]`, captured at start
    -- from what was actually taken. Stored rather than re-derived from the
    -- recipe for the same reason `fuel_units` is: the recipe may have changed
    -- or gone, and a refund must return what was TAKEN, not what a later
    -- version of the config says it should have been.
    inputs_json  TEXT NOT NULL,
    fuel_units   INTEGER NOT NULL DEFAULT 0,
    -- What the job will produce, also captured at start. Same reasoning.
    output_item  TEXT NOT NULL,
    output_qty   INTEGER NOT NULL,
    xp           INTEGER NOT NULL DEFAULT 0,
    skill        TEXT NOT NULL DEFAULT '',
    started_at   INTEGER NOT NULL,
    -- Unix seconds when it is done. Absolute rather than a remaining duration,
    -- so the clock runs while the server is down — a job started before a
    -- restart finishes on time rather than being paused by an outage the player
    -- had nothing to do with.
    ready_at     INTEGER NOT NULL,
    -- 'running' | 'ready' | 'failed'. 'ready' and 'failed' both mean there is
    -- something to collect: the output, or the refunded escrow.
    state        TEXT NOT NULL DEFAULT 'running',
    -- Why a failed job failed, shown to the player. A refund with no
    -- explanation reads as a bug.
    fail_reason  TEXT
);

-- One job per slot per player per station, enforced by the schema. Two collect
-- or start commands racing then lose to the database rather than to whichever
-- happened to read first.
CREATE UNIQUE INDEX IF NOT EXISTS idx_station_job_slot
    ON station_job (station_id, character_id, slot);

-- The sweep asks "what is due?" every tick; the panel asks "what are mine?".
CREATE INDEX IF NOT EXISTS idx_station_job_due ON station_job (state, ready_at);
CREATE INDEX IF NOT EXISTS idx_station_job_owner ON station_job (character_id, station_id);
