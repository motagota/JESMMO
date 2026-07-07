-- Build orders can now carry their own placement, so runtime-commissioned work
-- (mayor.build_create) can spawn a structure wherever it was created instead of
-- only at an authored (district, kind) -> location mapping. `x`/`y` place a point
-- structure; `x1`/`y1` are additionally set for a segment-shaped structure (e.g. a
-- road), with `x`/`y` as its start point.
ALTER TABLE build_order ADD COLUMN structure_kind TEXT;
ALTER TABLE build_order ADD COLUMN x INTEGER;
ALTER TABLE build_order ADD COLUMN y INTEGER;
ALTER TABLE build_order ADD COLUMN x1 INTEGER;
ALTER TABLE build_order ADD COLUMN y1 INTEGER;
