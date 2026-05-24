//! Uniform-cell occupancy grid used by the A* router.
//!
//! Two parallel layers (top / bottom). Each cell on each layer is one
//! of:
//! - `Free`        — routable.
//! - `Obstacle`    — never enter (foreign pad, board edge).
//! - `NetPad(u32)` — entrance point for the named net; obstacle for
//!                   every other net.
//! - `Trace(u32)`  — already routed by another net; obstacle for
//!                   everyone else, free for the same net (allows
//!                   multi-segment polylines on a star route).

use pcb_core::{Board, CopperLayer, Length, Point, Rect};

/// Per-cell extra cost layered on top of the grid for negotiated
/// congestion. A* adds `at(p)` to the step cost when entering `p`, so
/// raising the bias on a corridor pushes the next pass's nets to detour
/// around it. Lives across rip-up-and-reroute iterations and accumulates;
/// the grid itself is rebuilt each pass.
#[derive(Debug, Clone)]
pub struct CostMap {
    cols: i32,
    rows: i32,
    /// Layer-major: index = layer * cols * rows + r * cols + c.
    extra: Vec<u32>,
}

impl CostMap {
    /// Bias for the cell at `p`. Returns 0 for out-of-bounds points so
    /// callers don't need a separate bounds check.
    pub fn at(&self, p: GridPoint) -> u32 {
        if p.col < 0 || p.row < 0 || p.col >= self.cols || p.row >= self.rows || p.layer >= 2 {
            return 0;
        }
        let idx = (p.layer as usize) * (self.cols * self.rows) as usize
            + (p.row * self.cols + p.col) as usize;
        self.extra[idx]
    }

    /// Bump every cell inside the inclusive rectangle `[c0..=c1, r0..=r1]`
    /// on both layers by `amount`, capped at `max`. Out-of-range columns
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
        for layer in 0..2usize {
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
    /// A previously-laid trace cell belonging to net `u32`.
    Trace(u32),
}

#[derive(Debug, Clone)]
pub struct Grid {
    pub origin_nm: (i64, i64),
    pub cell_nm: i64,
    pub cols: i32,
    pub rows: i32,
    /// Two layers, row-major — index = layer * cols * rows + r * cols + c.
    cells: Vec<Cell>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GridPoint {
    pub layer: u8, // 0 = top, 1 = bottom
    pub col: i32,
    pub row: i32,
}

impl GridPoint {
    pub fn copper_layer(self) -> CopperLayer {
        match self.layer {
            0 => CopperLayer::Top,
            _ => CopperLayer::Bottom,
        }
    }
}

impl Grid {
    /// Build a grid covering the routing region. Caller chooses cell
    /// pitch — common choice is 0.25 mm so that 0.2 mm traces with
    /// 0.2 mm clearance comfortably fit per cell.
    pub fn new(region: Rect, cell: Length) -> Self {
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
            cells: vec![Cell::Free; (cols * rows * 2) as usize],
        }
    }

    fn idx(&self, p: GridPoint) -> usize {
        let layer_off = p.layer as usize * (self.cols * self.rows) as usize;
        layer_off + (p.row * self.cols + p.col) as usize
    }

    pub fn in_bounds(&self, p: GridPoint) -> bool {
        p.col >= 0 && p.col < self.cols && p.row >= 0 && p.row < self.rows && p.layer < 2
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
        GridPoint {
            layer: match layer {
                CopperLayer::Top => 0,
                CopperLayer::Bottom => 1,
            },
            col: (dx + self.cell_nm / 2) as i32 / self.cell_nm as i32,
            row: (dy + self.cell_nm / 2) as i32 / self.cell_nm as i32,
        }
    }

    /// Convert a grid point back to a board-coord `Point`.
    pub fn unsnap(&self, p: GridPoint) -> Point {
        Point::new(
            Length(self.origin_nm.0 + p.col as i64 * self.cell_nm),
            Length(self.origin_nm.1 + p.row as i64 * self.cell_nm),
        )
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
        for fp in board.footprints_in_order() {
            for pad in &fp.pads {
                let layer = match pad.layer {
                    CopperLayer::Top => 0,
                    CopperLayer::Bottom => 1,
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
                        match net {
                            Some(id) => {
                                // Pad core = NetPad; ring of clearance = Obstacle for others
                                // but we keep it simple: whole expanded box is NetPad on the
                                // same net, which means the search can enter anywhere in it.
                                self.set(gp, Cell::NetPad(id));
                            }
                            None => self.set(gp, Cell::Obstacle),
                        }
                    }
                }
            }
        }
    }

    /// Mark the path of an existing trace as `Trace(net)`, plus a
    /// `halo` of cells around it on the same layer so foreign nets
    /// can't run flush against this one. The halo radius is the
    /// router's clearance converted to grid cells.
    pub fn stamp_trace(&mut self, a: GridPoint, b: GridPoint, net: u32, halo: i32) {
        debug_assert_eq!(a.layer, b.layer);
        let layer = a.layer;
        let (mut c, mut r) = (a.col, a.row);
        let (tc, tr) = (b.col, b.row);
        loop {
            self.stamp_cell_with_halo(layer, c, r, net, halo);
            if c == tc && r == tr {
                break;
            }
            if c != tc {
                c += if tc > c { 1 } else { -1 };
            } else if r != tr {
                r += if tr > r { 1 } else { -1 };
            }
        }
    }

    /// Mark a via and its halo on both layers. Vias punch through, so
    /// they need clearance on copper top *and* bottom; otherwise a
    /// trace on the opposite layer might come right up against the
    /// via pad.
    pub fn stamp_via(&mut self, p: GridPoint, net: u32, halo: i32) {
        for layer in 0..2u8 {
            self.stamp_cell_with_halo(layer, p.col, p.row, net, halo);
        }
    }

    /// Allocate a same-shape `CostMap` for negotiated-congestion routing.
    /// Identical layer/col/row dims as this grid; all biases start at 0.
    pub fn new_cost_map(&self) -> CostMap {
        CostMap {
            cols: self.cols,
            rows: self.rows,
            extra: vec![0; (self.cols * self.rows * 2) as usize],
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
