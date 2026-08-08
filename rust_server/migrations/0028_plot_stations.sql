-- Player-built crafting stations on plots (business epic #179, issue #180).
--
-- Until now every station in the world was an authored fixture: `zones.toml`
-- placed the mine-yard furnace and wheel, anyone could use them free, and there
-- was no way to attach a station to a plot. Which meant there was nothing for a
-- business to be a business OF.
--
-- WHY A SEPARATE TABLE rather than a column on `structure`. A structure knows
-- where it stands; a station knows what it makes. Those change for different
-- reasons — a structure's row is about placement and hp, a station's is about
-- which `[station.*]` type in `crafting.toml` it realises — and a nullable
-- `station_type` on `structure` would be a column that is meaningless for the
-- other four kinds.
--
-- The row is also the JOIN that makes ownership work: `plot_station` → `plot` →
-- `owner_character_id` is how the fee code in #181 will know whose furnace this
-- is, without stations needing to carry an owner of their own.
CREATE TABLE IF NOT EXISTS plot_station (
    -- Also the station's runtime id. `station_fuel` and `station_job` (#167)
    -- key on a `station_id` TEXT, so an instance slots into the existing state
    -- with no schema change — an authored id like `furnace_mine_yard` and a
    -- uuid here are both just strings to them. That is the whole reason those
    -- tables were keyed on a string rather than a config index.
    id           TEXT PRIMARY KEY,
    -- The structure this station lives in. ON DELETE CASCADE: demolishing the
    -- structure removes the station, and the gateway is responsible for
    -- refunding fuel and failing jobs BEFORE that happens (see #180's demolition
    -- path) — the cascade is a backstop against orphans, not the refund
    -- mechanism.
    structure_id TEXT NOT NULL REFERENCES structure(id) ON DELETE CASCADE,
    plot_id      TEXT NOT NULL,
    -- A key into `crafting.toml`'s `[station.*]`. Can VANISH between restarts,
    -- exactly like a recipe id can (#167): the file is hand-edited and this row
    -- outlives it. A station whose type is gone is inert, not a panic.
    station_type TEXT NOT NULL,
    -- World position, denormalised from the structure so the proximity gate can
    -- resolve a station without a join on every check. Kept in step by the
    -- placement path, which is the only thing that writes either.
    x            INTEGER NOT NULL,
    y            INTEGER NOT NULL,
    built_at     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_plot_station_plot ON plot_station (plot_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_plot_station_structure ON plot_station (structure_id);
