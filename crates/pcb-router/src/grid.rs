//! Uniform-cell occupancy grid used by the Theta* router.
//!
//! N parallel layers (top / bottom plus optional inner layers — set at
//! `Grid::new` time, defaults to 2 to preserve the pre-Phase-4 router
//! contract). Each cell on each layer is one of:
//! - `Free`        — routable.
//! - `Obstacle`    — never enter (foreign pad, board edge).
//! - `NetPad(u32)` — entrance point for the named net; obstacle for
//!   every other net.
//! - `Trace(u32)`  — already routed by another net; obstacle for
//!   everyone else, free for the same net (allows
//!   multi-segment polylines on a star route).
//!
//! Bresenham line rasterisation is shared between `line_of_sight`,
//! `cost_along`, and `stamp_trace` so any-angle segments behave
//! consistently across visibility, cost accumulation, and obstacle
//! stamping.

use pcb_core::{Board, CopperLayer, Keepout, Layer, Length, Point, Rect};

/// Per-cell extra cost layered on top of the grid for negotiated
/// congestion. A* adds `at(p)` to the step cost when entering `p`, so
/// raising the bias on a corridor pushes the next pass's nets to detour
/// around it. Lives across rip-up-and-reroute iterations and accumulates;
/// the grid itself is rebuilt each pass.
#[derive(Debug, Clone)]
pub struct CostMap {
    cols: i32,
    rows: i32,
    layer_count: u8,
    /// Layer-major: index = layer * cols * rows + r * cols + c.
    extra: Vec<u32>,
}

impl CostMap {
    /// Bias for the cell at `p`. Returns 0 for out-of-bounds points so
    /// callers don't need a separate bounds check.
    pub fn at(&self, p: GridPoint) -> u32 {
        if p.col < 0
            || p.row < 0
            || p.col >= self.cols
            || p.row >= self.rows
            || p.layer >= self.layer_count
        {
            return 0;
        }
        let idx = (p.layer as usize) * (self.cols * self.rows) as usize
            + (p.row * self.cols + p.col) as usize;
        self.extra[idx]
    }

    /// Bump every cell inside the inclusive rectangle `[c0..=c1, r0..=r1]`
    /// on every layer by `amount`, capped at `max`. Out-of-range columns
    /// and rows are silently clipped.
    pub fn bump_box(&mut self, c0: i32, r0: i32, c1: i32, r1: i32, amount: u32, max: u32) {
        let c0 = c0.max(0);
        let r0 = r0.max(0);
        let c1 = c1.min(self.cols - 1);
        let r1 = r1.min(self.rows - 1);
        if c1 < c0 || r1 < r0 {
            return;
        }
        let stride = (self.cols * self.rows) as usize;
        for layer in 0..self.layer_count as usize {
            for r in r0..=r1 {
                let row_base = layer * stride + (r * self.cols) as usize;
                for c in c0..=c1 {
                    let i = row_base + c as usize;
                    self.extra[i] = (self.extra[i] + amount).min(max);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cell {
    Free,
    Obstacle,
    /// A pad belonging to net id `u32`. Used by the search both as a
    /// source/destination and as an obstacle for foreign nets.
    NetPad(u32),
    /// A through-hole pad cell. Behaves like `NetPad` for traces, but
    /// the via-safe check rejects vias landing inside it (the existing
    /// PTH drill already connects both layers — a router via on top
    /// would collide with the fab drill).
    DrilledPad(u32),
    /// A previously-laid trace cell belonging to net `u32`.
    Trace(u32),
}

#[derive(Debug, Clone)]
pub struct Grid {
    pub origin_nm: (i64, i64),
    pub cell_nm: i64,
    pub cols: i32,
    pub rows: i32,
    /// Number of copper layers the grid was sized for. Always ≥ 2;
    /// defaults to 2 via `Grid::new` for pre-Phase-4 callers.
    pub layer_count: u8,
    /// `layer_count` layers, row-major — index = layer * cols * rows + r * cols + c.
    cells: Vec<Cell>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GridPoint {
    pub layer: u8, // 0 = top, layer_count - 1 = bottom
    pub col: i32,
    pub row: i32,
}

impl GridPoint {
    /// Convert back to the model `Layer`. Identity mapping.
    pub fn copper_layer(self) -> CopperLayer {
        Layer { index: self.layer }
    }
}

impl Grid {
    /// Build a 2-layer grid (legacy default). Use `with_layers` for an
    /// N-layer routing surface.
    #[allow(dead_code)]
    pub fn new(region: Rect, cell: Length) -> Self {
        Self::with_layers(region, cell, 2)
    }

    /// Build a grid covering the routing region with `layer_count`
    /// copper layers. Caller chooses cell pitch — common choice is
    /// 0.25 mm so that 0.2 mm traces with 0.2 mm clearance comfortably
    /// fit per cell. `layer_count` is clamped to `[2, 8]` and aligned
    /// to an even number (manufacturing constraint).
    pub fn with_layers(region: Rect, cell: Length, layer_count: u8) -> Self {
        let layer_count = layer_count.clamp(2, 8);
        let layer_count = if layer_count.is_multiple_of(2) {
            layer_count
        } else {
            layer_count + 1
        };
        let cell_nm = cell.0.max(1);
        let w_nm = region.width().0;
        let h_nm = region.height().0;
        let cols = (w_nm / cell_nm) as i32 + 1;
        let rows = (h_nm / cell_nm) as i32 + 1;
        Self {
            origin_nm: (region.min.x.0, region.min.y.0),
            cell_nm,
            cols,
            rows,
            layer_count,
            cells: vec![Cell::Free; (cols * rows * i32::from(layer_count)) as usize],
        }
    }

    fn idx(&self, p: GridPoint) -> usize {
        let layer_off = p.layer as usize * (self.cols * self.rows) as usize;
        layer_off + (p.row * self.cols + p.col) as usize
    }

    pub fn in_bounds(&self, p: GridPoint) -> bool {
        p.col >= 0
            && p.col < self.cols
            && p.row >= 0
            && p.row < self.rows
            && p.layer < self.layer_count
    }

    pub fn get(&self, p: GridPoint) -> Cell {
        if !self.in_bounds(p) {
            return Cell::Obstacle;
        }
        self.cells[self.idx(p)]
    }

    pub fn set(&mut self, p: GridPoint, cell: Cell) {
        if self.in_bounds(p) {
            let idx = self.idx(p);
            self.cells[idx] = cell;
        }
    }

    /// Snap a board-coord point to the nearest grid cell on the given layer.
    pub fn snap(&self, p: Point, layer: CopperLayer) -> GridPoint {
        let dx = p.x.0 - self.origin_nm.0;
        let dy = p.y.0 - self.origin_nm.1;
        // Multi-layer mapping: `CopperLayer::Bottom` (index 1) maps to
        // the BOTTOM of the actual stackup, not literal index 1. The
        // router/grid mostly works in terms of `Layer { index }` now;
        // this helper is for legacy 2-layer call sites.
        let raw_idx = layer.index;
        let actual = if raw_idx == 1 && self.layer_count > 2 {
            self.layer_count - 1
        } else {
            raw_idx
        };
        GridPoint {
            layer: actual.min(self.layer_count - 1),
            col: (dx + self.cell_nm / 2) as i32 / self.cell_nm as i32,
            row: (dy + self.cell_nm / 2) as i32 / self.cell_nm as i32,
        }
    }

    /// Convert a grid point back to a board-coord `Point`.
    pub fn unsnap(&self, p: GridPoint) -> Point {
        Point::new(
            Length(self.origin_nm.0 + i64::from(p.col) * self.cell_nm),
            Length(self.origin_nm.1 + i64::from(p.row) * self.cell_nm),
        )
    }

    /// Stamp each footprint's full body bbox as `Obstacle` on its
    /// copper layer. Prevents foreign-net traces from running
    /// underneath component bodies on the same side — some packages
    /// (ESP32-S3-Zero etc.) carry un-modelled pads or thermal slugs
    /// under the body, and a trace there is a shorting risk even when
    /// no pad is declared. Call this BEFORE `stamp_pads` so the pad
    /// cells overwrite the obstacle inside the bbox. For TH footprints
    /// (any pad with `drill`) the body is stamped on both layers
    /// because the package physically straddles both.
    pub fn stamp_bodies(&mut self, board: &Board) {
        let bottom = self.layer_count - 1;
        for fp in board.footprints_in_order() {
            let Some(bounds) = fp.bounds() else { continue };
            let is_th = fp.pads.iter().any(|p| p.drill.is_some());
            // Map the model layer to a grid-layer index. Pre-Phase-4
            // boards only use Top/Bottom; on a 4-layer grid Bottom
            // still means "the very bottom".
            let primary: u8 = if fp.layer.is_top() {
                0
            } else if fp.layer.index == 1 {
                bottom
            } else {
                fp.layer.index.min(bottom)
            };
            // TH pads punch every copper layer they straddle. Since
            // we currently only model through-hole vias top↔bottom,
            // stamp the outer two for TH parts.
            let layers: Vec<u8> = if is_th { vec![0, bottom] } else { vec![primary] };
            let cmin = self.snap(bounds.min, fp.layer);
            let cmax = self.snap(bounds.max, fp.layer);
            for &layer in &layers {
                for r in cmin.row..=cmax.row {
                    for c in cmin.col..=cmax.col {
                        let gp = GridPoint { layer, col: c, row: r };
                        if !self.in_bounds(gp) {
                            continue;
                        }
                        if matches!(self.get(gp), Cell::Free) {
                            self.set(gp, Cell::Obstacle);
                        }
                    }
                }
            }
        }
    }

    /// Stamp obstacles for every pad: the pad rectangle expanded by
    /// `clearance` is marked `Obstacle` for foreign nets and `NetPad`
    /// for its own net (so the search can enter it).
    pub fn stamp_pads(
        &mut self,
        board: &Board,
        net_id_of: &dyn Fn(&str) -> Option<u32>,
        clearance: Length,
    ) {
        let bottom = self.layer_count - 1;
        for fp in board.footprints_in_order() {
            for pad in &fp.pads {
                let is_th = pad.drill.is_some();
                let primary_layer: u8 = if pad.layer.is_top() {
                    0
                } else if pad.layer.index == 1 {
                    bottom
                } else {
                    pad.layer.index.min(bottom)
                };
                let center = fp.pad_world_center(pad);
                let (pw, ph) = fp.pad_world_size(pad);
                let half_w = pw / 2 + clearance;
                let half_h = ph / 2 + clearance;
                let min = center.translate(-half_w, -half_h);
                let max = center.translate(half_w, half_h);
                let cmin = self.snap(min, pad.layer);
                let cmax = self.snap(max, pad.layer);
                let net = pad.net.as_deref().and_then(net_id_of);
                let cell_for_net = match (net, is_th) {
                    (Some(id), true) => Cell::DrilledPad(id),
                    (Some(id), false) => Cell::NetPad(id),
                    (None, _) => Cell::Obstacle,
                };
                // TH pads punch every copper layer — stamp the copper
                // region on the outer two (vias still only go
                // top↔bottom in this iteration; inner-layer landing
                // pads are unmodelled) so the via-safe check sees the
                // drilled cells from either side of a layer flip.
                let layers: Vec<u8> = if is_th { vec![0, bottom] } else { vec![primary_layer] };
                for &layer in &layers {
                    for r in cmin.row..=cmax.row {
                        for c in cmin.col..=cmax.col {
                            let gp = GridPoint { layer, col: c, row: r };
                            if !self.in_bounds(gp) {
                                continue;
                            }
                            self.set(gp, cell_for_net);
                        }
                    }
                }
            }
        }
    }

    /// Rasterise every keepout polygon into `Obstacle` cells on the
    /// applicable layers. Only the "blocks all nets" case is honoured
    /// in this iteration (every `keepout.nets_allowed` is treated as
    /// empty) — the grid's `Cell` enum doesn't yet carry a per-cell
    /// allow-net bitmap. Per-net allow lists stay in the model so a
    /// future grid extension can pick them up without a schema change.
    pub fn stamp_keepouts(&mut self, board: &Board) {
        for kp in &board.keepouts {
            if kp.polygon.len() < 3 {
                continue;
            }
            let bottom = self.layer_count - 1;
            let layers: Vec<u8> = if kp.layers.is_empty() {
                (0..self.layer_count).collect()
            } else {
                kp.layers
                    .iter()
                    .map(|l| {
                        if l.is_top() {
                            0
                        } else if l.index == 1 {
                            bottom
                        } else {
                            l.index.min(bottom)
                        }
                    })
                    .collect()
            };
            // Bounding box of the polygon in grid cells.
            let mut min_c = i32::MAX;
            let mut min_r = i32::MAX;
            let mut max_c = i32::MIN;
            let mut max_r = i32::MIN;
            for p in &kp.polygon {
                let gp = self.snap(*p, CopperLayer::Top);
                min_c = min_c.min(gp.col);
                min_r = min_r.min(gp.row);
                max_c = max_c.max(gp.col);
                max_r = max_r.max(gp.row);
            }
            min_c = min_c.max(0);
            min_r = min_r.max(0);
            max_c = max_c.min(self.cols - 1);
            max_r = max_r.min(self.rows - 1);
            // Standard point-in-polygon scanline test on each cell.
            for r in min_r..=max_r {
                for c in min_c..=max_c {
                    // Use cell-centre coords in mm for the test.
                    let p = self.unsnap(GridPoint { layer: 0, col: c, row: r });
                    let px = p.x.to_mm();
                    let py = p.y.to_mm();
                    if !point_in_polygon(&kp.polygon, px, py) {
                        continue;
                    }
                    for &layer in &layers {
                        let gp = GridPoint { layer, col: c, row: r };
                        if matches!(self.get(gp), Cell::Free) {
                            self.set(gp, Cell::Obstacle);
                        }
                    }
                }
            }
        }
    }

    /// Mark the path of an existing trace as `Trace(net)`, plus a
    /// `halo` of cells around it on the same layer so foreign nets
    /// can't run flush against this one. Works for arbitrary
    /// straight segments (H/V/diagonal/any angle) via Bresenham.
    pub fn stamp_trace(&mut self, a: GridPoint, b: GridPoint, net: u32, halo: i32) {
        debug_assert_eq!(a.layer, b.layer);
        let layer = a.layer;
        for (c, r) in bresenham(a.col, a.row, b.col, b.row) {
            self.stamp_cell_with_halo(layer, c, r, net, halo);
        }
    }

    /// True if the swept halo of every Bresenham cell between `a` and `b`
    /// (inclusive of both endpoints) is walkable for `target_net`. The
    /// `halo` parameter mirrors `stamp_cell_with_halo`: each centre cell
    /// is checked along with the (2*halo+1)² box around it, so the LOS
    /// shortcut never lets a trace pass through cells the trace's own
    /// halo would otherwise overlap with foreign copper.
    /// Requires `a.layer == b.layer`.
    pub fn line_of_sight(&self, a: GridPoint, b: GridPoint, target_net: u32, halo: i32) -> bool {
        debug_assert_eq!(a.layer, b.layer);
        let layer = a.layer;
        for (c, r) in bresenham(a.col, a.row, b.col, b.row) {
            for dr in -halo..=halo {
                for dc in -halo..=halo {
                    let gp = GridPoint { layer, col: c + dc, row: r + dr };
                    match self.get(gp) {
                        Cell::Free => {}
                        Cell::NetPad(n) | Cell::DrilledPad(n) | Cell::Trace(n)
                            if n == target_net => {}
                        _ => return false,
                    }
                }
            }
        }
        true
    }

    /// Sum of `cost_map.at(p)` for each cell on the Bresenham line from
    /// `a` (exclusive) to `b` (inclusive). Used by Theta* to charge
    /// per-cell congestion bias along an any-angle straight segment.
    /// Method on `Grid` to keep the line-rasterisation helpers
    /// co-located, even though it doesn't read the grid itself.
    #[allow(clippy::unused_self)]
    pub fn cost_along(&self, cost_map: &CostMap, a: GridPoint, b: GridPoint) -> u32 {
        debug_assert_eq!(a.layer, b.layer);
        let layer = a.layer;
        let mut sum: u32 = 0;
        let mut first = true;
        for (c, r) in bresenham(a.col, a.row, b.col, b.row) {
            if first {
                first = false;
                continue;
            }
            let gp = GridPoint { layer, col: c, row: r };
            sum = sum.saturating_add(cost_map.at(gp));
        }
        sum
    }

    /// Mark a via and its halo on every copper layer. Vias punch
    /// through (top↔bottom only in this iteration), so they need
    /// clearance on every copper layer; otherwise a trace on the
    /// opposite layer might come right up against the via pad.
    pub fn stamp_via(&mut self, p: GridPoint, net: u32, halo: i32) {
        for layer in 0..self.layer_count {
            self.stamp_cell_with_halo(layer, p.col, p.row, net, halo);
        }
    }

    /// Allocate a same-shape `CostMap` for negotiated-congestion routing.
    /// Identical layer/col/row dims as this grid; all biases start at 0.
    pub fn new_cost_map(&self) -> CostMap {
        CostMap {
            cols: self.cols,
            rows: self.rows,
            layer_count: self.layer_count,
            extra: vec![0; (self.cols * self.rows * i32::from(self.layer_count)) as usize],
        }
    }

    fn stamp_cell_with_halo(&mut self, layer: u8, c: i32, r: i32, net: u32, halo: i32) {
        // Centre cell: Trace(net) — walkable by the same net so star
        // routes can share path along the trunk.
        let centre = GridPoint {
            layer,
            col: c,
            row: r,
        };
        if matches!(self.get(centre), Cell::Free) {
            self.set(centre, Cell::Trace(net));
        }
        // Halo cells: Obstacle — blocked for *every* net, including
        // the trace's own. This stops same-net parallel spokes from
        // running in cells adjacent to the trunk (which used to make
        // pairs of blue lines look glued together).
        for dr in -halo..=halo {
            for dc in -halo..=halo {
                if dr == 0 && dc == 0 {
                    continue;
                }
                let gp = GridPoint {
                    layer,
                    col: c + dc,
                    row: r + dr,
                };
                if matches!(self.get(gp), Cell::Free) {
                    self.set(gp, Cell::Obstacle);
                }
            }
        }
    }
}

/// Ray-cast point-in-polygon test. The polygon is treated as a
/// closed loop (last → first edge implicit). Stable in degenerate
/// cases (point exactly on an edge): the boundary may resolve either
/// way, which is fine for keepout rasterisation — a one-cell error
/// at the boundary is well within router resolution.
fn point_in_polygon(poly: &[pcb_core::Point], x: f64, y: f64) -> bool {
    let mut inside = false;
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let pix = poly[i].x.to_mm();
        let piy = poly[i].y.to_mm();
        let pjx = poly[j].x.to_mm();
        let pjy = poly[j].y.to_mm();
        if (piy > y) != (pjy > y) {
            let t = (pjy - piy).abs();
            if t > 1e-12 {
                let x_intersect = pix + (y - piy) * (pjx - pix) / (pjy - piy);
                if x < x_intersect {
                    inside = !inside;
                }
            }
        }
        j = i;
    }
    inside
}

/// Use `Keepout` parameter so the symbol is alive when callers only
/// reach the impl via `Grid::stamp_keepouts`. Pure type-system hint —
/// never invoked.
#[allow(dead_code)]
fn _keepout_type_anchor(_k: &Keepout) {}

/// Integer Bresenham line from (c0,r0) to (c1,r1) inclusive on both
/// endpoints. Used by `line_of_sight`, `cost_along`, and `stamp_trace`
/// so visibility checks, congestion bias, and obstacle stamping all
/// agree on which cells a straight any-angle segment touches.
fn bresenham(c0: i32, r0: i32, c1: i32, r1: i32) -> Vec<(i32, i32)> {
    let dc = (c1 - c0).abs();
    let dr = (r1 - r0).abs();
    let sc: i32 = if c0 < c1 { 1 } else { -1 };
    let sr: i32 = if r0 < r1 { 1 } else { -1 };
    let mut err = dc - dr;
    let mut c = c0;
    let mut r = r0;
    let mut out = Vec::with_capacity((dc.max(dr) + 1) as usize);
    loop {
        out.push((c, r));
        if c == c1 && r == r1 {
            break;
        }
        let e2 = 2 * err;
        if e2 > -dr {
            err -= dr;
            c += sc;
        }
        if e2 < dc {
            err += dc;
            r += sr;
        }
    }
    out
}
