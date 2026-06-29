-- M0 foundation schema.
--
-- Durable identity + the bare minimum character state needed to demonstrate
-- persistence: log out and back in (even across a server restart) and land at
-- the same position. Gameplay tables (plots, skills, inventory, build orders,
-- rent) arrive in later milestones; this is intentionally the M0 slice.

CREATE TABLE IF NOT EXISTS account (
    id          TEXT PRIMARY KEY,            -- uuid
    email       TEXT NOT NULL UNIQUE,
    pw_hash     TEXT NOT NULL,               -- argon2 PHC string
    created_at  INTEGER NOT NULL,            -- unix seconds
    last_login  INTEGER
);

CREATE TABLE IF NOT EXISTS character (
    id          TEXT PRIMARY KEY,            -- uuid; this is the durable entity id
    account_id  TEXT NOT NULL REFERENCES account(id),
    name        TEXT NOT NULL,
    x           INTEGER NOT NULL,            -- last world position
    y           INTEGER NOT NULL,
    hp          INTEGER NOT NULL,
    district    TEXT NOT NULL DEFAULT '',    -- informational for M0 (routing is by position)
    created_at  INTEGER NOT NULL,
    last_seen   INTEGER
);

-- Phase 1 keeps one active character per account; enforce it now so the
-- assumption can't silently break later.
CREATE UNIQUE INDEX IF NOT EXISTS idx_character_account ON character(account_id);
