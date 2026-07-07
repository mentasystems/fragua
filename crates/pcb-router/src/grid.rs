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

/// Sentinel net id stamped on pads that carry no net (NC pads, mounting
/// holes, etc.). It is never a real net id (those are `0..order.len()`),
/// so the per-trace clearance disk treats this copper as foreign to every
/// net — a trace keeps clearance to a no-net pad — while `walkable` never
/// lets any net enter it (it is `n == target_net` for nobody). This is
/// what replaces the old "no-net pad = Obstacle" stamp, which blocked
/// entry but demanded no clearance.
pub(crate) const FOREIGN_NET: u32 = u32::MAX;

/// True if `c` is copper belonging to a net other than `target` — the
/// only thing the per-trace clearance disk treats as a clearance demand.
/// `Obstacle` is deliberately NOT foreign: component bodies and keepouts
/// block entry (via `walkable`) but impose no edge-to-edge clearance, so
/// traces may still run flush to a body edge exactly as before.
#[inline]
pub(crate) fn is_foreign(c: Cell, target: u32) -> bool {
    matches!(c, Cell::NetPad(n) | Cell::DrilledPad(n) | Cell::Trace(n) if n != target)
}

/// True if `target` may occupy `c`: a free cell or copper of its own net.
#[inline]
pub(crate) fn walkable(c: Cell, target: u32) -> bool {
    match c {
        Cell::Free => true,
        Cell::NetPad(n) | Cell::DrilledPad(n) | Cell::Trace(n) => n == target,
        Cell::Obstacle => false,
    }
}

/// All `(dc, dr)` cell offsets inside the Euclidean disk of radius `r`
/// cells (`dc² + dr² ≤ r²`). Precomputed once per search and reused for
/// every clearance test, so the radius² comparison isn't redone per
/// expansion.
pub(crate) fn disk_offsets(r: i32) -> Vec<(i32, i32)> {
    let r = r.max(0);
    let r2 = r * r;
    let mut v = Vec::new();
    for dr in -r..=r {
        for dc in -r..=r {
            if dc * dc + dr * dr <= r2 {
                v.push((dc, dr));
            }
        }
    }
    v
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

    /// Cell range `(min, max)` (layer 0, only `col`/`row` meaningful)
    /// that fully COVERS the board-coord rectangle `[lo, hi]`: the low
    /// corner is floored and the high corner ceiled to cell boundaries.
    /// Unlike `snap` (nearest), this never under-covers the rectangle, so
    /// a bare pad stamp can't leave a sliver of true copper outside the
    /// stamped cells.
    pub(crate) fn cover_rect_cells(&self, lo: Point, hi: Point) -> (GridPoint, GridPoint) {
        let cell = self.cell_nm.max(1) as f64;
        let fdiv = |num: i64| -> i32 { (num as f64 / cell).floor() as i32 };
        let cdiv = |num: i64| -> i32 { (num as f64 / cell).ceil() as i32 };
        let lo = GridPoint {
            layer: 0,
            col: fdiv(lo.x.0 - self.origin_nm.0),
            row: fdiv(lo.y.0 - self.origin_nm.1),
        };
        let hi = GridPoint {
            layer: 0,
            col: cdiv(hi.x.0 - self.origin_nm.0),
            row: cdiv(hi.y.0 - self.origin_nm.1),
        };
        (lo, hi)
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
            let layers: Vec<u8> = if is_th {
                vec![0, bottom]
            } else {
                vec![primary]
            };
            let cmin = self.snap(bounds.min, fp.layer);
            let cmax = self.snap(bounds.max, fp.layer);
            for &layer in &layers {
                for r in cmin.row..=cmax.row {
                    for c in cmin.col..=cmax.col {
                        let gp = GridPoint {
                            layer,
                            col: c,
                            row: r,
                        };
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
                // Round the stamped rect OUTWARD (floor the min corner,
                // ceil the max corner) so the stamped copper always fully
                // covers the true pad rectangle. Snapping to the NEAREST
                // cell could shave up to half a cell off each edge, which —
                // now that pads are stamped bare and clearance is enforced
                // by the search disk against these cells — silently let a
                // trace sit up to half a cell too close to the real pad
                // edge (the sub-cell TracePadClearance / NetShort the bare
                // stamp would otherwise produce on fine-pitch connectors).
                let (cmin, cmax) = self.cover_rect_cells(min, max);
                let net = pad.net.as_deref().and_then(net_id_of);
                let cell_for_net = match (net, is_th) {
                    (Some(id), true) => Cell::DrilledPad(id),
                    (Some(id), false) => Cell::NetPad(id),
                    // No-net pad: stamp the sentinel so the clearance disk
                    // still keeps traces off it, while nobody may enter it.
                    (None, _) => Cell::NetPad(FOREIGN_NET),
                };
                // TH pads punch EVERY copper layer — their drilled barrel
                // is real copper on all of them, so stamp the copper region
                // on every layer (not just the outer two). Inner-layer
                // routing must keep clearance to a through-hole pad exactly
                // like the outer layers do, or it grazes the barrel and the
                // DRC flags it. (On a 2-layer board `0..layer_count` is just
                // `[0, bottom]`, so this is unchanged there.)
                let layers: Vec<u8> = if is_th {
                    (0..self.layer_count).collect()
                } else {
                    vec![primary_layer]
                };
                for &layer in &layers {
                    for r in cmin.row..=cmax.row {
                        for c in cmin.col..=cmax.col {
                            let gp = GridPoint {
                                layer,
                                col: c,
                                row: r,
                            };
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
                    let p = self.unsnap(GridPoint {
                        layer: 0,
                        col: c,
                        row: r,
                    });
                    let px = p.x.to_mm();
                    let py = p.y.to_mm();
                    if !point_in_polygon(&kp.polygon, px, py) {
                        continue;
                    }
                    for &layer in &layers {
                        let gp = GridPoint {
                            layer,
                            col: c,
                            row: r,
                        };
                        if matches!(self.get(gp), Cell::Free) {
                            self.set(gp, Cell::Obstacle);
                        }
                    }
                }
            }
        }
    }

    /// Stamp a fanout via's landing as `DrilledPad(net)` — a disk of
    /// radius `copper` cells centred on the via, on EVERY copper layer.
    /// Stamps only the via barrel's footprint, NOT the whole SMD pad rect:
    /// on the inner layers the SMD pad does not physically exist, only the
    /// via copper does. Stamping just the via keeps the inner layers'
    /// approach lanes open between neighbouring fine-pitch pins, which a
    /// full-rect stamp would wall off. Overwrites any cell (the via barrel
    /// shorts whatever shares its column/row).
    pub fn stamp_drilled_disk(&mut self, center: Point, copper: i32, net: u32) {
        let gp = self.snap(center, CopperLayer::Top);
        let copper = copper.max(0);
        let r2 = copper * copper;
        for layer in 0..self.layer_count {
            for dr in -copper..=copper {
                for dc in -copper..=copper {
                    if dc * dc + dr * dr > r2 {
                        continue;
                    }
                    let p = GridPoint {
                        layer,
                        col: gp.col + dc,
                        row: gp.row + dr,
                    };
                    if self.in_bounds(p) {
                        self.set(p, Cell::DrilledPad(net));
                    }
                }
            }
        }
    }

    /// The grid cells a straight segment `a..b` (same layer) rasterises
    /// to, via the shared Bresenham line. Used by the router to track a
    /// net's already-laid trace cells incrementally — so the multi-source
    /// search can seed from them without rescanning the whole grid.
    pub fn line_cells(&self, a: GridPoint, b: GridPoint) -> Vec<GridPoint> {
        debug_assert_eq!(a.layer, b.layer);
        let layer = a.layer;
        bresenham(a.col, a.row, b.col, b.row)
            .into_iter()
            .map(|(c, r)| GridPoint {
                layer,
                col: c,
                row: r,
            })
            .collect()
    }

    /// Distinct net ids of `Trace` copper lying in the clearance corridor of
    /// the straight segment `a..b` (the Bresenham line dilated by the disk of
    /// radius `clr_cells`), scanned on EVERY layer, restricted to nets for
    /// which `accept(net)` is true. Returned ascending (deterministic). Only
    /// `Trace` cells are reported — pads/drilled pads are fixed placement and
    /// cannot be ripped, so they are deliberately excluded. Used by the
    /// router to pick rip-up candidates that actually block a failed corridor.
    pub fn corridor_trace_nets(
        &self,
        a: GridPoint,
        b: GridPoint,
        clr_cells: i32,
        accept: impl Fn(u32) -> bool,
    ) -> Vec<u32> {
        let disk = disk_offsets(clr_cells);
        let mut found: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for layer in 0..self.layer_count {
            for (c, r) in bresenham(a.col, a.row, b.col, b.row) {
                for &(dc, dr) in &disk {
                    let gp = GridPoint {
                        layer,
                        col: c + dc,
                        row: r + dr,
                    };
                    if let Cell::Trace(n) = self.get(gp) {
                        if n != FOREIGN_NET && accept(n) {
                            found.insert(n);
                        }
                    }
                }
            }
        }
        found.into_iter().collect()
    }

    /// Stamp the bare copper of a trace `a..b`: each Bresenham cell, plus a
    /// disk of radius `copper` cells around it, becomes `Trace(net)` — the
    /// trace's own half-width and nothing more. No clearance halo is
    /// stamped; edge-to-edge clearance is enforced at search time by the
    /// per-trace clearance disk. Works for any-angle segments via Bresenham.
    pub fn stamp_trace(&mut self, a: GridPoint, b: GridPoint, net: u32, copper: i32) {
        debug_assert_eq!(a.layer, b.layer);
        let layer = a.layer;
        for (c, r) in bresenham(a.col, a.row, b.col, b.row) {
            self.stamp_cell_copper(layer, c, r, net, copper);
        }
    }

    /// Exact inverse of `stamp_trace`: for the same Bresenham cells + copper
    /// disk, free ONLY cells whose current value is `Trace(net)`. Pads,
    /// foreign copper, and obstacles are never touched, so the grid stays
    /// byte-consistent with the board copper that remains. Re-rasterises the
    /// trace's own geometry, never the whole grid — O(trace length), not O(board).
    pub fn unstamp_trace(&mut self, a: GridPoint, b: GridPoint, net: u32, copper: i32) {
        debug_assert_eq!(a.layer, b.layer);
        let layer = a.layer;
        for (c, r) in bresenham(a.col, a.row, b.col, b.row) {
            self.unstamp_cell_copper(layer, c, r, net, copper);
        }
    }

    /// True if no foreign copper sits inside the precomputed clearance
    /// `disk` centred on `p` (same layer). The disk is the set of cell
    /// offsets within the searching net's clearance radius; any foreign
    /// `NetPad`/`DrilledPad`/`Trace` inside it rejects the move. Bodies
    /// (`Obstacle`) are ignored here — they only block entry.
    pub(crate) fn clearance_ok_disk(&self, p: GridPoint, target: u32, disk: &[(i32, i32)]) -> bool {
        for &(dc, dr) in disk {
            let gp = GridPoint {
                layer: p.layer,
                col: p.col + dc,
                row: p.row + dr,
            };
            if is_foreign(self.get(gp), target) {
                return false;
            }
        }
        true
    }

    /// True if every Bresenham cell of the straight segment `a..b`
    /// (inclusive) is enterable by `target_net` AND keeps the per-trace
    /// clearance `disk` clear of foreign copper. This is the LOS test the
    /// Theta* any-angle shortcut uses: the centre cell must be walkable
    /// (catches `Obstacle`, which the disk ignores) and its clearance disk
    /// must hold no foreign copper. Requires `a.layer == b.layer`.
    pub(crate) fn line_of_sight(
        &self,
        a: GridPoint,
        b: GridPoint,
        target_net: u32,
        disk: &[(i32, i32)],
    ) -> bool {
        debug_assert_eq!(a.layer, b.layer);
        let layer = a.layer;
        for (c, r) in bresenham(a.col, a.row, b.col, b.row) {
            let p = GridPoint {
                layer,
                col: c,
                row: r,
            };
            if !walkable(self.get(p), target_net) {
                return false;
            }
            if !self.clearance_ok_disk(p, target_net, disk) {
                return false;
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
            let gp = GridPoint {
                layer,
                col: c,
                row: r,
            };
            sum = sum.saturating_add(cost_map.at(gp));
        }
        sum
    }

    /// Stamp a via's bare copper disk (radius `copper` cells) on every
    /// copper layer — the via barrel shorts all layers, so its copper
    /// blocks later nets' clearance disks on every layer. No clearance
    /// halo: separation to the via is enforced at search time like any
    /// other copper.
    pub fn stamp_via(&mut self, p: GridPoint, net: u32, copper: i32) {
        for layer in 0..self.layer_count {
            self.stamp_cell_copper(layer, p.col, p.row, net, copper);
        }
    }

    /// Inverse of `stamp_via`: free this net's via-barrel copper (cells equal
    /// to `Trace(net)`) on every layer. Via copper is stamped as `Trace(net)`
    /// (see `stamp_via`), so the same free-if-`Trace(net)` rule applies.
    pub fn unstamp_via(&mut self, p: GridPoint, net: u32, copper: i32) {
        for layer in 0..self.layer_count {
            self.unstamp_cell_copper(layer, p.col, p.row, net, copper);
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

    /// Stamp the bare copper of a single trace/via cell: the centre cell
    /// plus a Euclidean disk of radius `copper` cells become `Trace(net)`.
    /// Only `Free` cells are overwritten — pads and foreign copper are
    /// never clobbered. This represents the feature's own copper
    /// half-width; all edge-to-edge clearance is enforced at search time.
    fn stamp_cell_copper(&mut self, layer: u8, c: i32, r: i32, net: u32, copper: i32) {
        let copper = copper.max(0);
        let r2 = copper * copper;
        for dr in -copper..=copper {
            for dc in -copper..=copper {
                if dc * dc + dr * dr > r2 {
                    continue;
                }
                let gp = GridPoint {
                    layer,
                    col: c + dc,
                    row: r + dr,
                };
                if matches!(self.get(gp), Cell::Free) {
                    self.set(gp, Cell::Trace(net));
                }
            }
        }
    }

    /// Inverse of `stamp_cell_copper`: over the same copper disk, set to
    /// `Free` only cells currently equal to `Trace(net)`. Mirror of the
    /// free-only stamp, so unstamping a net leaves every other net's copper
    /// and all pads exactly as they were.
    fn unstamp_cell_copper(&mut self, layer: u8, c: i32, r: i32, net: u32, copper: i32) {
        let copper = copper.max(0);
        let r2 = copper * copper;
        for dr in -copper..=copper {
            for dc in -copper..=copper {
                if dc * dc + dr * dr > r2 {
                    continue;
                }
                let gp = GridPoint {
                    layer,
                    col: c + dc,
                    row: r + dr,
                };
                if matches!(self.get(gp), Cell::Trace(n) if n == net) {
                    self.set(gp, Cell::Free);
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
