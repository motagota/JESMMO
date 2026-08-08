-- `derelict` and the recovery vault (business epic #179, issue #184).
--
-- Closes #43, which observed that rent reclaim DELETES placed structures and
-- deferred itself with: "Once placement has material costs (likely Phase 2),
-- this needs revisiting: either refund materials on reclaim or move structure
-- items to storage."
--
-- #180 created that cost — a station is 40 stone and 12 ingots. So the
-- condition is met, and the answer is better than either option #43 offered: a
-- vault covers structures, stored goods and station fuel in one mechanism
-- instead of three special cases.
--
-- THE GAME MUST NEVER DELETE PLAYER PROPERTY FOR NON-PAYMENT. Someone who stops
-- playing for a fortnight should come back to a lost lease and an intact pile
-- of goods, not to nothing. That is the difference between rent as a pressure
-- and rent as a punishment, and it is the single most likely thing to make
-- somebody quit permanently.
--
-- It is also the honest counterweight to P4. Ownership being temporary is only
-- acceptable if losing it is survivable.
CREATE TABLE IF NOT EXISTS recovery_vault (
    id           TEXT PRIMARY KEY,
    -- The last owner, who may claim it. Not the plot: the plot goes back to the
    -- pool and may be leased by somebody else within the hour, and the goods
    -- must not follow the land.
    character_id TEXT NOT NULL,
    item_id      TEXT NOT NULL,
    qty          INTEGER NOT NULL,
    -- Where it came from, so the claim screen can say "your furnace" rather
    -- than listing 40 anonymous stone.
    source       TEXT NOT NULL,
    deposited_at INTEGER NOT NULL,
    -- After this the vault is emptied. THE VAULT MUST NOT BECOME STORAGE — it
    -- is a grace period, not a second warehouse, or it becomes a way to hold
    -- goods rent-free forever by deliberately lapsing.
    expires_at   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_recovery_vault_owner ON recovery_vault (character_id);
CREATE INDEX IF NOT EXISTS idx_recovery_vault_expiry ON recovery_vault (expires_at);
