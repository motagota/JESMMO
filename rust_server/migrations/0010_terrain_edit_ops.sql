-- Undo for terrain editing (epic #72): an append-only log of accepted edit
-- ops, each carrying the PRE-edit content of every block it touched. Revert
-- = write those blocks back wholesale (a whole 512-byte block is cheaper and
-- simpler than computing/replaying inverses -- the design doc's chosen
-- tradeoff). Reverting out of stroke order can therefore clobber a later
-- overlapping op's changes; the v1 client only offers undo-last (LIFO),
-- which is always exact.
CREATE TABLE terrain_edit_op (
    id         TEXT PRIMARY KEY,      -- uuid, minted per accepted op
    author     TEXT NOT NULL,         -- AuthorId string form ("editor:<id>")
    brush      TEXT NOT NULL,         -- freeform label from the op, for history UI
    created_at INTEGER NOT NULL,      -- unix seconds
    reverted   INTEGER NOT NULL DEFAULT 0
);

-- One row per (chunk, block) the op touched. prev_block is the block's raw
-- LE-i16 content (terrain_common::HeightBlock::to_bytes) BEFORE the op;
-- NULL means the block did not exist -- revert deletes it.
CREATE TABLE terrain_edit_op_block (
    op_id      TEXT NOT NULL REFERENCES terrain_edit_op(id),
    chunk_tx   INTEGER NOT NULL,
    chunk_ty   INTEGER NOT NULL,
    block_idx  INTEGER NOT NULL,
    prev_block BLOB,
    PRIMARY KEY (op_id, chunk_tx, chunk_ty, block_idx)
);
