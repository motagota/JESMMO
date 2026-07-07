-- The mayor role: one account may be granted city-building authority (issuing
-- build orders on city-owned land, e.g. roads) via `mayor.build_create`.
ALTER TABLE account ADD COLUMN role TEXT NOT NULL DEFAULT 'player';
