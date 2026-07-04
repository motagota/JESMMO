-- Home structures: bed-based respawn (issue #12).
--
-- A character's active respawn point is whichever `bed`-kind structure they last
-- set via `home.set_respawn`. NULL means "no bed set" — the gateway falls back to
-- the default town-centre spawn.
ALTER TABLE character ADD COLUMN respawn_structure_id TEXT REFERENCES structure(id);
