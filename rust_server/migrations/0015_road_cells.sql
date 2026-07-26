-- Road cells (progressive per-cell road building epic #131, issue #132):
-- a road's path is chopped into fixed-length chunks at plan time, each with
-- its own cost and progress, so the road pays cell by cell instead of as
-- one pooled total for the whole plan. `build_order.required_json`/
-- `progress_json` stay the road's aggregate (mirrored in lockstep as cells
-- fill), so every existing consumer that reads those columns keeps working
-- unchanged; the cells are the new, finer-grained record underneath.
--
-- `cell_index` orders the cells along the path from the start (0-based).
-- `x0,y0`-`x1,y1` is the cell's own short span (may cross an original
-- waypoint corner — the cut runs on arc length, not on the road's turns),
-- used for per-cell contribution proximity. `required_json`/`progress_json`
-- match `build_order`'s own cost-map convention (currently always
-- `{"stone": N}`, kept generalized for consistency rather than a bespoke
-- integer column). No FK cascade (this codebase deletes explicitly
-- alongside `build_contribution`, see `cancel_road_order`/
-- `settle_demolition`).
CREATE TABLE IF NOT EXISTS road_cell (
    order_id      TEXT NOT NULL REFERENCES build_order(id),
    cell_index    INTEGER NOT NULL,
    x0            INTEGER NOT NULL,
    y0            INTEGER NOT NULL,
    x1            INTEGER NOT NULL,
    y1            INTEGER NOT NULL,
    required_json TEXT NOT NULL,
    progress_json TEXT NOT NULL DEFAULT '{}',
    completed_at  INTEGER,
    PRIMARY KEY (order_id, cell_index)
);
