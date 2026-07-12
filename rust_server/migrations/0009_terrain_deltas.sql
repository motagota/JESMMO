-- Hand-authored terrain edits (terrain-editing epic #72): one row per edited
-- chunk, keyed by the existing streaming-chunk grid coordinate. An unedited
-- chunk has no row at all -- sparsity is the design's principle #2, so the
-- table is empty until someone actually paints.
--
-- `height_delta_blob` is `terrain_common::SparseHeightDelta::encode` bytes
-- (magic "TRHD"; block bitmap + touched 16x16 blocks). NULL means the row
-- exists without a height layer -- can't happen in v1 (height is the only
-- layer), but later layers (splat/biome/holes/scatter, #81) will add sibling
-- blob columns and any one of them may be the row's reason to exist.
--
-- `bake_hash` is the artifact manifest's bake_hash current at last write --
-- the base-drift detector (a re-bake changes the hash; the delta stays valid
-- because it stores offsets, but the editor can flag the chunk for review).
--
-- `revision` is monotonic per chunk, bumped on every accepted edit op -- the
-- sync/optimistic-concurrency anchor for delta streaming and patch broadcast.
CREATE TABLE terrain_delta (
    chunk_tx          INTEGER NOT NULL,
    chunk_ty          INTEGER NOT NULL,
    revision          INTEGER NOT NULL,
    bake_hash         TEXT NOT NULL,
    height_delta_blob BLOB,
    author            TEXT NOT NULL,     -- AuthorId string form: "system" | "editor:<id>" | "player:<id>"
    edited_at         INTEGER NOT NULL,  -- unix seconds, last write
    PRIMARY KEY (chunk_tx, chunk_ty)
);
