-- Skill-level gating for build orders (issue #10).
--
-- A build order can require a minimum skill level before it accepts contributions
-- ("requires Building 3"). The requirement is authored per order; enforcement is
-- per contributor (skills are per-character), so the order row carries the gate and
-- the gateway checks the contributing character's level. `required_level` 0 (the
-- default) means ungated, so existing rows keep their current behaviour.
ALTER TABLE build_order ADD COLUMN required_skill TEXT;
ALTER TABLE build_order ADD COLUMN required_level INTEGER NOT NULL DEFAULT 0;
