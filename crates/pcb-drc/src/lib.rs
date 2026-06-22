//! `pcb-drc` — native design rule checker.
//!
//! Phase 4 implementation: five geometric checks over a `Board`.
//! - **`PadPadClearance`**: two pads on the same copper layer that
//!   belong to different nets must keep `min_clearance` of breathing
//!   room between their outer edges.
//! - **`TraceTraceClearance`**: two trace segments on the same layer
//!   from different nets must do likewise (edge-to-edge after
//!   subtracting the trace half-widths).
//! - **`TracePadClearance`**: trace edge vs pad edge across nets.
//! - **`EdgeClearance`**: every copper item (pad, trace, via) must sit
//!   at least `edge_clearance` inside the board outline.
//! - **`UnconnectedPad`**: a pad declares a net but no copper item of
//!   that net touches it — usually means the router gave up.
//! - **`SmallComponentDangling`**: a footprint with fewer than 8 pads
//!   has at least one pad that is either unrouted or carries no net at
//!   all. Surfaces the kind of dangling resistor / cap / 2-pin module
//!   pad that the per-pad `UnconnectedPad` warning would also catch, but
//!   raises it once per component so the agent sees a tight summary.
//!
//! Pads are axis-aligned in world coords because we only model 90°
//! rotations; this lets the geometry collapse to AABB-vs-AABB and
//! AABB-vs-segment distance checks instead of full polygon clipping.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;

use pcb_core::{
    Board, CopperLayer, Footprint, LayerStackup, Length, Pad, PlacementMargin, Point, Pour, Rect,
    Schematic, Trace,
};

#[derive(Debug, Clone)]
pub struct DrcOptions {
    pub min_clearance: Length,
    pub edge_clearance: Length,
    pub min_trace_width: Length,
    pub min_drill: Length,
    /// `actual_wire / HPWL_lower_bound` above which a net is flagged
    /// `RoutingInefficient`. HPWL = the half-perimeter of the net's pad
    /// bounding box, the universal lower bound on tree wire length.
    /// 1.5 means "the routing used 50 % more wire than the geometric
    /// optimum"; below that the detour is usually noise (cell-pitch
    /// rounding, single 90° bend around a footprint).
    pub routing_inefficient_ratio: f32,
    /// Per-net rule overrides (net classes). When two pieces of copper
    /// from different nets are checked for clearance, the required
    /// gap is the strictest of `min_clearance` and either net's
    /// override — so a 0.3 mm power-class clearance is honoured even
    /// when paired with a 0.2 mm signal.
    pub net_overrides: HashMap<String, NetOverride>,
    /// Optional schematic so DRC can consult net-class clearances
    /// without the caller having to mirror them into `net_overrides`.
    /// When both are set, the schematic class wins.
    pub schematic: Option<Arc<Schematic>>,
    /// Library-key → per-side placement margin (mm). When set, DRC
    /// emits `BodyOverlap` warnings for any pair of footprints whose
    /// library-authored body bbox (pads + margin, rotated into the
    /// world frame) overlap, and `BodyOffBoard` ERRORS for any
    /// footprint whose body bbox sticks past the board outline. The
    /// off-board case is a hard error because the physical plastic of
    /// the part cannot occupy space that the board does not have —
    /// no `edge_mounted` flag changes that, since "the pads reach the
    /// edge" does not move the body inward.
    pub placement_margins: HashMap<String, PlacementMargin>,
    /// Fab profile to enforce alongside the project-side minimums.
    /// Every minimum-style check (trace width, drill, annular ring,
    /// edge clearance) also gates against the profile's value, and
    /// reports the worst of the two via `FabProfileMin`. Set via
    /// `Project::set_fab_profile` or directly when constructing
    /// `DrcOptions`.
    pub fab_profile: Option<FabProfile>,
}

/// Capability profile for a specific fab house. Numbers are the
/// minimum the fab will accept; anything stricter than this is fine.
/// Maps to PCB-industry-standard "feature size" published by every
/// major fab. See `pcb_fab::profiles` for built-in presets.
#[derive(Debug, Clone, PartialEq)]
pub struct FabProfile {
    pub name: String,
    pub min_trace_width_mm: f64,
    pub min_clearance_mm: f64,
    pub min_drill_mm: f64,
    pub min_annular_ring_mm: f64,
    pub min_via_diameter_mm: f64,
    pub min_edge_clearance_mm: f64,
    pub max_board_size_mm: (f64, f64),
}

/// Per-net rule overrides — fields default to "use the call-site
/// defaults" when `None`. Mirrors `pcb_router::NetOverride` so the
/// caller can build one map and feed both crates.
#[derive(Debug, Clone, Default)]
pub struct NetOverride {
    pub clearance: Option<Length>,
}

impl Default for DrcOptions {
    fn default() -> Self {
        Self {
            // 0.2 mm matches our router's `clearance` baseline; DRC
            // is the receipt that the router actually honoured it.
            min_clearance: Length::from_mm(0.2),
            edge_clearance: Length::from_mm(0.3),
            min_trace_width: Length::from_mm(0.1),
            min_drill: Length::from_mm(0.2),
            routing_inefficient_ratio: 1.5,
            net_overrides: HashMap::new(),
            schematic: None,
            placement_margins: HashMap::new(),
            fab_profile: None,
        }
    }
}

/// IPC-2141 microstrip impedance (single-ended) for a trace of width
/// `width_mm` sitting over a reference plane separated by `height_mm`
/// of dielectric with relative permittivity `er`. Copper thickness
/// `copper_thickness_mm` adjusts the effective conductor width.
///
/// `Z0 = (87 / sqrt(Er + 1.41)) * ln(5.98 * H / (0.8 * W + T))`
///
/// Accurate to ~10 % within the formula's valid range
/// (0.1 ≤ W/H ≤ 2.0, 1 ≤ Er ≤ 15). Good enough for the "did the agent
/// pick a sane trace width" check; precise impedance work needs a 2D
/// field solver.
#[must_use]
pub fn compute_microstrip_z0(
    width_mm: f64,
    height_mm: f64,
    er: f64,
    copper_thickness_mm: f64,
) -> f64 {
    if width_mm <= 0.0 || height_mm <= 0.0 {
        return 0.0;
    }
    let denom = 0.8 * width_mm + copper_thickness_mm;
    if denom <= 0.0 {
        return 0.0;
    }
    let arg = 5.98 * height_mm / denom;
    if arg <= 0.0 {
        return 0.0;
    }
    let coeff = 87.0 / (er + 1.41).sqrt();
    coeff * arg.ln()
}

/// Trace width that gives `z_target` ohms on the supplied stackup. Uses
/// bisection between 0.05 mm and 5.0 mm — narrower gives high
/// impedance, wider gives low. Returns the upper bound (5.0) if the
/// target is unreachable (e.g. asking for 10 Ω on a 1.5 mm FR-4
/// stackup).
#[must_use]
pub fn suggest_trace_width_for_impedance(z_target: f64, stackup: &LayerStackup) -> f64 {
    let mut lo = 0.05_f64;
    let mut hi = 5.0_f64;
    let z = |w: f64| {
        compute_microstrip_z0(
            w,
            stackup.dielectric_thickness_mm(),
            stackup.dielectric_er(),
            stackup.copper_thickness_mm(),
        )
    };
    // Narrower traces give HIGHER impedance, so the function is
    // monotonically decreasing in width. We bisect to find the width
    // whose impedance equals z_target.
    let z_lo = z(lo);
    let z_hi = z(hi);
    if z_target >= z_lo {
        return lo;
    }
    if z_target <= z_hi {
        return hi;
    }
    for _ in 0..60 {
        let mid = f64::midpoint(lo, hi);
        let zm = z(mid);
        if (zm - z_target).abs() < 1e-3 {
            return mid;
        }
        if zm > z_target {
            // mid is narrower than the answer
            lo = mid;
        } else {
            hi = mid;
        }
    }
    f64::midpoint(lo, hi)
}

/// Effective clearance required between two nets: the strictest of the
/// global default and either net's class override. Used by every
/// pair-wise clearance check (pad-pad, trace-trace, trace-pad).
fn effective_clearance_mm(opts: &DrcOptions, net_a: Option<&str>, net_b: Option<&str>) -> f64 {
    let mut c = opts.min_clearance.to_mm();
    for n in [net_a, net_b].into_iter().flatten() {
        if let Some(o) = opts.net_overrides.get(n) {
            if let Some(over) = o.clearance {
                c = c.max(over.to_mm());
            }
        }
        if let Some(sch) = opts.schematic.as_ref() {
            let class = sch.class_for(n);
            if let Some(mm) = class.clearance_mm {
                c = c.max(mm);
            }
        }
    }
    c
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    PadPadClearance,
    TraceTraceClearance,
    TracePadClearance,
    EdgeClearance,
    UnconnectedPad,
    NarrowTrace,
    SmallDrill,
    SmallComponentDangling,
    /// A net was routed but the actual wire length is much longer than
    /// the HPWL lower bound for its pads — usually a sign that the
    /// router had to take a long detour because some other net was
    /// blocking the obvious corridor (i.e. a placement issue).
    RoutingInefficient,
    /// Two footprints' library-authored body bboxes (pads inflated by
    /// `LibraryEntry::placement_margin`) overlap. Emitted as a warning
    /// — pad-on-pad overlap is still rejected hard by the placement
    /// APIs, but a body keep-out overlap is something the user may
    /// have accepted intentionally (e.g. tucking a 0805 cap under the
    /// shadow of a screw terminal's plastic shroud).
    BodyOverlap,
    /// A footprint's library-authored body bbox extends past the board
    /// outline. Hard ERROR — the part's plastic physically cannot occupy
    /// space the board does not have. `edge_mounted` does NOT exempt
    /// this; it only relaxes the pad-vs-outline clearance.
    BodyOffBoard,
    /// A trace segment crosses a keepout polygon, or a via lands
    /// inside one, on an applicable copper layer. The keepout's
    /// `nets_allowed` list is not honoured in this iteration (see
    /// `Keepout` docs).
    KeepoutViolation,
    /// A net's class declares a `target_impedance_ohms`, but the
    /// trace width assigned to the net (via the class or the global
    /// default) deviates from the target by more than 5 % when
    /// evaluated against the board's stackup. Carries the net name,
    /// target, and actual values in the message.
    ImpedanceMismatch,
    /// A geometric property of a routed item is below the currently
    /// adopted fab profile's minimum (trace width, drill diameter,
    /// annular ring, edge clearance). Distinct from `NarrowTrace` /
    /// `SmallDrill` so the agent can see which limit fired — the
    /// project's own DRC defaults or the fab profile.
    FabProfileMin,
    /// A net's pads do not all reside on one continuous copper island —
    /// the routed copper splits the net into two or more electrically
    /// isolated groups (an *open*). This is the exhaustive form of the
    /// multimeter "these two points should read 0 Ω but read open"
    /// test: `UnconnectedPad` only catches a pad with *no* same-net
    /// copper at all, whereas a net can be fully routed pad-by-pad yet
    /// still fall into disconnected sub-trees that never join. Carries
    /// the net name and a representative pad from each isolated island.
    NetSplit,
    /// Copper belonging to two *different* declared nets physically
    /// touches (gap ≤ 0) on a shared layer — a *short*. The two nets
    /// are now one electrical node. This is the exhaustive form of the
    /// multimeter "these two nets should read open but read 0 Ω" test.
    /// Distinct from the clearance kinds: those fire when different-net
    /// copper is merely *closer than allowed* (a spacing risk); this
    /// fires only when it actually meets (a definite wiring error).
    /// Carries the two net names and the contact location.
    NetShort,
}

#[derive(Debug, Clone, Serialize)]
pub struct Violation {
    pub kind: ViolationKind,
    pub severity: Severity,
    pub message: String,
    /// Centre of the offending region in board mm; the UI draws a
    /// marker here.
    pub x_mm: f64,
    pub y_mm: f64,
    /// References of items involved (e.g. `["R1.2", "C1.1"]`).
    pub involved: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DrcReport {
    pub violations: Vec<Violation>,
    pub error_count: usize,
    pub warning_count: usize,
}

impl DrcReport {
    fn push(&mut self, v: Violation) {
        match v.severity {
            Severity::Error => self.error_count += 1,
            Severity::Warning => self.warning_count += 1,
        }
        self.violations.push(v);
    }
}

#[must_use]
pub fn run(board: &Board, opts: &DrcOptions) -> DrcReport {
    let mut report = DrcReport::default();
    let pads = collect_pad_geometry(board);
    check_pad_pad(&pads, opts, &mut report);
    check_trace_trace(board, opts, &mut report);
    check_trace_pad(board, &pads, opts, &mut report);
    if let Some(outline) = board.outline {
        check_edge(board, &pads, outline, opts, &mut report);
    }
    check_unconnected_pads(board, &pads, &mut report);
    check_small_component_dangling(board, &pads, &mut report);
    check_net_continuity(board, &pads, &mut report);
    check_narrow_traces(board, opts, &mut report);
    check_small_drills(board, opts, &mut report);
    check_routing_inefficient(board, opts, &mut report);
    check_body_overlap(board, opts, &mut report);
    if let Some(outline) = board.outline {
        check_body_off_board(board, outline, opts, &mut report);
    }
    check_keepouts(board, &mut report);
    check_impedance(board, opts, &mut report);
    if let Some(profile) = opts.fab_profile.as_ref() {
        check_fab_profile(board, profile, &mut report);
    }
    report
}

/// For every net whose class declares `target_impedance_ohms`, compute
/// the Z0 the resolved trace width would actually produce on the
/// board's stackup and emit `ImpedanceMismatch` when the deviation
/// exceeds 5 %. Single-ended only — diff-pair impedance is a different
/// formula and isn't checked here.
fn check_impedance(board: &Board, opts: &DrcOptions, report: &mut DrcReport) {
    let Some(sch) = opts.schematic.as_ref() else {
        return;
    };
    let default_width = opts.min_trace_width.to_mm().max(0.10);
    for net_name in sch.nets.keys() {
        let class = sch.class_for(net_name);
        let Some(z_target) = class.target_impedance_ohms else {
            continue;
        };
        let width_mm = class.trace_width_mm.unwrap_or(default_width);
        let z_actual = compute_microstrip_z0(
            width_mm,
            board.stackup.dielectric_thickness_mm(),
            board.stackup.dielectric_er(),
            board.stackup.copper_thickness_mm(),
        );
        if z_actual <= 0.0 {
            continue;
        }
        let dev = (z_actual - z_target).abs() / z_target;
        if dev > 0.05 {
            report.push(Violation {
                kind: ViolationKind::ImpedanceMismatch,
                severity: Severity::Warning,
                message: format!(
                    "net {net_name}: trace width {width_mm:.3} mm produces Z0≈{z_actual:.1} Ω (target {z_target:.1} Ω, deviation {:.1}%)",
                    dev * 100.0,
                ),
                x_mm: 0.0,
                y_mm: 0.0,
                involved: vec![net_name.clone()],
            });
        }
    }
}

/// Gate every minimum-style check against the adopted fab profile.
/// Emits `FabProfileMin` for traces narrower than `min_trace_width_mm`,
/// vias whose drill or annular ring is below the profile, and pads
/// whose drill is below the profile. Board outline area larger than
/// the profile's max gets a single warning.
fn check_fab_profile(board: &Board, profile: &FabProfile, report: &mut DrcReport) {
    for trace in &board.traces {
        let w = trace.width.to_mm();
        if w + 1e-6 < profile.min_trace_width_mm {
            let mx = f64::midpoint(trace.start.x.to_mm(), trace.end.x.to_mm());
            let my = f64::midpoint(trace.start.y.to_mm(), trace.end.y.to_mm());
            report.push(Violation {
                kind: ViolationKind::FabProfileMin,
                severity: Severity::Error,
                message: format!(
                    "trace {} width {w:.3} mm < {} min {:.3} mm",
                    trace.net, profile.name, profile.min_trace_width_mm,
                ),
                x_mm: mx,
                y_mm: my,
                involved: vec![trace.net.clone()],
            });
        }
    }
    for via in &board.vias {
        let d = via.drill.to_mm();
        if d + 1e-6 < profile.min_drill_mm {
            report.push(Violation {
                kind: ViolationKind::FabProfileMin,
                severity: Severity::Error,
                message: format!(
                    "via on {} drilled at {d:.3} mm < {} min {:.3} mm",
                    via.net, profile.name, profile.min_drill_mm,
                ),
                x_mm: via.position.x.to_mm(),
                y_mm: via.position.y.to_mm(),
                involved: vec![via.net.clone()],
            });
        }
        let ring = (via.diameter.to_mm() - d) / 2.0;
        if ring + 1e-6 < profile.min_annular_ring_mm {
            report.push(Violation {
                kind: ViolationKind::FabProfileMin,
                severity: Severity::Error,
                message: format!(
                    "via on {} annular ring {ring:.3} mm < {} min {:.3} mm",
                    via.net, profile.name, profile.min_annular_ring_mm,
                ),
                x_mm: via.position.x.to_mm(),
                y_mm: via.position.y.to_mm(),
                involved: vec![via.net.clone()],
            });
        }
        if via.diameter.to_mm() + 1e-6 < profile.min_via_diameter_mm {
            report.push(Violation {
                kind: ViolationKind::FabProfileMin,
                severity: Severity::Error,
                message: format!(
                    "via on {} diameter {:.3} mm < {} min {:.3} mm",
                    via.net,
                    via.diameter.to_mm(),
                    profile.name,
                    profile.min_via_diameter_mm,
                ),
                x_mm: via.position.x.to_mm(),
                y_mm: via.position.y.to_mm(),
                involved: vec![via.net.clone()],
            });
        }
    }
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            let Some(drill) = pad.drill else { continue };
            let d = drill.to_mm();
            if d + 1e-6 < profile.min_drill_mm {
                let c = fp.pad_world_center(pad);
                report.push(Violation {
                    kind: ViolationKind::FabProfileMin,
                    severity: Severity::Error,
                    message: format!(
                        "pad {}/{} drilled at {d:.3} mm < {} min {:.3} mm",
                        fp.reference, pad.number, profile.name, profile.min_drill_mm,
                    ),
                    x_mm: c.x.to_mm(),
                    y_mm: c.y.to_mm(),
                    involved: vec![fp.reference.clone()],
                });
            }
        }
    }
    if let Some(outline) = board.outline {
        let w = outline.width().to_mm();
        let h = outline.height().to_mm();
        let (mw, mh) = profile.max_board_size_mm;
        if w > mw + 1e-6 || h > mh + 1e-6 {
            report.push(Violation {
                kind: ViolationKind::FabProfileMin,
                severity: Severity::Warning,
                message: format!(
                    "board {w:.1} × {h:.1} mm exceeds {} standard tier {:.0} × {:.0} mm",
                    profile.name, mw, mh,
                ),
                x_mm: f64::midpoint(outline.min.x.to_mm(), outline.max.x.to_mm()),
                y_mm: f64::midpoint(outline.min.y.to_mm(), outline.max.y.to_mm()),
                involved: Vec::new(),
            });
        }
    }
}

/// Every trace and via vs every keepout. A trace violates when any
/// part of its centreline crosses the keepout polygon on an
/// applicable layer; a via violates when its centre sits inside one
/// (vias punch every layer, so layer filtering is skipped for them).
fn check_keepouts(board: &Board, report: &mut DrcReport) {
    for kp in &board.keepouts {
        if kp.polygon.len() < 3 {
            continue;
        }
        for trace in &board.traces {
            if !keepout_applies_to_layer(kp, trace.layer) {
                continue;
            }
            if segment_in_polygon(
                (trace.start.x.to_mm(), trace.start.y.to_mm()),
                (trace.end.x.to_mm(), trace.end.y.to_mm()),
                &kp.polygon,
            ) {
                let mx = f64::midpoint(trace.start.x.to_mm(), trace.end.x.to_mm());
                let my = f64::midpoint(trace.start.y.to_mm(), trace.end.y.to_mm());
                report.push(Violation {
                    kind: ViolationKind::KeepoutViolation,
                    severity: Severity::Error,
                    message: format!(
                        "trace {} crosses keepout `{}`",
                        trace.net,
                        if kp.label.is_empty() {
                            kp.id.0.to_string()
                        } else {
                            kp.label.clone()
                        },
                    ),
                    x_mm: mx,
                    y_mm: my,
                    involved: vec![trace.net.clone(), kp.label.clone()],
                });
            }
        }
        for via in &board.vias {
            let x = via.position.x.to_mm();
            let y = via.position.y.to_mm();
            if point_in_polygon(&kp.polygon, x, y) {
                report.push(Violation {
                    kind: ViolationKind::KeepoutViolation,
                    severity: Severity::Error,
                    message: format!(
                        "via {} sits inside keepout `{}`",
                        via.net,
                        if kp.label.is_empty() {
                            kp.id.0.to_string()
                        } else {
                            kp.label.clone()
                        },
                    ),
                    x_mm: x,
                    y_mm: y,
                    involved: vec![via.net.clone(), kp.label.clone()],
                });
            }
        }
    }
}

fn keepout_applies_to_layer(kp: &pcb_core::Keepout, layer: CopperLayer) -> bool {
    kp.layers.is_empty() || kp.layers.contains(&layer)
}

fn point_in_polygon(poly: &[Point], x: f64, y: f64) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let pix = poly[i].x.to_mm();
        let piy = poly[i].y.to_mm();
        let pjx = poly[j].x.to_mm();
        let pjy = poly[j].y.to_mm();
        if (piy > y) != (pjy > y) {
            let t = pjy - piy;
            if t.abs() > 1e-12 {
                let xi = pix + (y - piy) * (pjx - pix) / t;
                if x < xi {
                    inside = !inside;
                }
            }
        }
        j = i;
    }
    inside
}

/// Cheap test: a segment "crosses" a polygon if either endpoint is
/// inside, or it intersects any polygon edge. Sufficient for the
/// keepout-vs-trace check (a trace endpoint may legitimately sit on
/// the boundary of a same-net pad just outside the keepout, but the
/// keepout doesn't care about pad geometry — only the trace itself).
fn segment_in_polygon(a: (f64, f64), b: (f64, f64), poly: &[Point]) -> bool {
    if point_in_polygon(poly, a.0, a.1) || point_in_polygon(poly, b.0, b.1) {
        return true;
    }
    let n = poly.len();
    for i in 0..n {
        let j = (i + 1) % n;
        let p = (poly[i].x.to_mm(), poly[i].y.to_mm());
        let q = (poly[j].x.to_mm(), poly[j].y.to_mm());
        if segments_intersect(a, b, p, q) {
            return true;
        }
    }
    false
}

/// Resolve a footprint's placement margin from the rules table. Empty
/// key or unknown key → zero margin (the rule is then a no-op for that
/// footprint).
fn margin_for(opts: &DrcOptions, fp: &Footprint) -> PlacementMargin {
    if fp.key.is_empty() {
        return PlacementMargin::default();
    }
    opts.placement_margins
        .get(&fp.key)
        .copied()
        .unwrap_or_default()
}

fn check_body_overlap(board: &Board, opts: &DrcOptions, report: &mut DrcReport) {
    if opts.placement_margins.is_empty() {
        return;
    }
    let fps: Vec<&Footprint> = board.footprints_in_order().collect();
    for i in 0..fps.len() {
        let a = fps[i];
        let Some(ab) = a.inflated_bbox(margin_for(opts, a)) else {
            continue;
        };
        for &b in fps.iter().skip(i + 1) {
            let Some(bb) = b.inflated_bbox(margin_for(opts, b)) else {
                continue;
            };
            if !ab.intersects(&bb) {
                continue;
            }
            // Don't double-report when neither footprint actually
            // declared a margin — that case is already covered by the
            // existing `MIN_FOOTPRINT_GAP_MM` pad-bbox check the
            // placement APIs enforce, and surfacing it as a body-overlap
            // warning here is noise.
            let am = margin_for(opts, a);
            let bm = margin_for(opts, b);
            if am.is_zero() && bm.is_zero() {
                continue;
            }
            let mx = f64::midpoint(
                f64::midpoint(ab.min.x.to_mm(), ab.max.x.to_mm()),
                f64::midpoint(bb.min.x.to_mm(), bb.max.x.to_mm()),
            );
            let my = f64::midpoint(
                f64::midpoint(ab.min.y.to_mm(), ab.max.y.to_mm()),
                f64::midpoint(bb.min.y.to_mm(), bb.max.y.to_mm()),
            );
            report.push(Violation {
                kind: ViolationKind::BodyOverlap,
                severity: Severity::Warning,
                message: format!(
                    "{} body overlaps {} body (placement_margin inflation)",
                    a.reference, b.reference
                ),
                x_mm: mx,
                y_mm: my,
                involved: vec![a.reference.clone(), b.reference.clone()],
            });
        }
    }
}

fn check_body_off_board(
    board: &Board,
    outline: Rect,
    opts: &DrcOptions,
    report: &mut DrcReport,
) {
    if opts.placement_margins.is_empty() {
        return;
    }
    for fp in board.footprints_in_order() {
        // NOTE: `edge_mounted` is NOT an exception here. A connector
        // whose pads touch the outline is fine, but the part's plastic
        // body still has to fit inside the board — there is no scenario
        // where a physical component can hang in mid-air past the cut
        // line.
        let margin = margin_for(opts, fp);
        // No margin → fall back to the existing edge-clearance check on
        // raw pads; no point re-flagging an unannotated footprint as
        // off-board for a margin that's zero.
        if margin.is_zero() {
            continue;
        }
        let Some(bbox) = fp.inflated_bbox(margin) else {
            continue;
        };
        let over_left = outline.min.x.0 - bbox.min.x.0;
        let over_right = bbox.max.x.0 - outline.max.x.0;
        let over_bottom = outline.min.y.0 - bbox.min.y.0;
        let over_top = bbox.max.y.0 - outline.max.y.0;
        let worst = over_left.max(over_right).max(over_bottom).max(over_top);
        // Allow up to 0.5 mm of overhang — same tolerance the placement
        // APIs use so a body that just kisses the outline isn't flagged.
        if worst <= 500_000 {
            continue;
        }
        let side = if worst == over_left {
            "left"
        } else if worst == over_right {
            "right"
        } else if worst == over_bottom {
            "bottom"
        } else {
            "top"
        };
        let mm = worst as f64 / 1_000_000.0;
        let cx = f64::midpoint(bbox.min.x.to_mm(), bbox.max.x.to_mm());
        let cy = f64::midpoint(bbox.min.y.to_mm(), bbox.max.y.to_mm());
        report.push(Violation {
            kind: ViolationKind::BodyOffBoard,
            severity: Severity::Error,
            message: format!(
                "{} body extends {mm:.2} mm past the {side} board outline",
                fp.reference
            ),
            x_mm: cx,
            y_mm: cy,
            involved: vec![fp.reference.clone()],
        });
    }
}

/// World-space pad geometry — flattened so the checks don't have to
/// re-do the rotation maths repeatedly.
struct PadGeom<'a> {
    fp_reference: &'a str,
    pad_number: &'a str,
    /// Mount side. SMD pads only have copper here; PTH pads
    /// (`is_through_hole`) have a copper ring on every copper layer
    /// — use [`PadGeom::occupies_layer`] / [`PadGeom::shares_layer_with`]
    /// instead of comparing `layer` directly.
    layer: CopperLayer,
    /// `Pad::drill.is_some()` — copy carried so the clearance checks
    /// can ask "does this pad have copper on layer L?" without having
    /// to walk back to the original `Pad`.
    is_through_hole: bool,
    rect: Rect,
    net: Option<&'a str>,
    /// Mirrors `Footprint::edge_mounted`. The edge-clearance check
    /// skips these pads — their job is to sit ON the outline so a
    /// USB-C cable, antenna, or screwdriver can reach them.
    edge_mounted: bool,
}

impl PadGeom<'_> {
    fn label(&self) -> String {
        format!("{}.{}", self.fp_reference, self.pad_number)
    }

    /// True iff this pad has copper on `target`. Mirrors
    /// [`pcb_core::Pad::occupies_layer`].
    fn occupies_layer(&self, target: CopperLayer) -> bool {
        self.is_through_hole || self.layer == target
    }

    /// True iff `self` and `other` share at least one copper layer —
    /// the prerequisite for any pad-pad clearance / connectivity check.
    fn shares_layer_with(&self, other: &PadGeom) -> bool {
        self.is_through_hole || other.is_through_hole || self.layer == other.layer
    }
}

fn collect_pad_geometry(board: &Board) -> Vec<PadGeom<'_>> {
    let mut out = Vec::new();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            out.push(PadGeom {
                fp_reference: fp.reference.as_str(),
                pad_number: pad.number.as_str(),
                layer: pad.layer,
                is_through_hole: pad.drill.is_some(),
                rect: pad_world_rect(fp, pad),
                net: pad.net.as_deref(),
                edge_mounted: fp.edge_mounted,
            });
        }
    }
    out
}

fn pad_world_rect(fp: &Footprint, pad: &Pad) -> Rect {
    let center = fp.pad_world_center(pad);
    let (w, h) = fp.pad_world_size(pad);
    Rect::from_center(center, w, h)
}

fn check_pad_pad(pads: &[PadGeom], opts: &DrcOptions, report: &mut DrcReport) {
    for i in 0..pads.len() {
        for j in (i + 1)..pads.len() {
            let a = &pads[i];
            let b = &pads[j];
            if !a.shares_layer_with(b) {
                continue;
            }
            // Same net or both unassigned: not a clearance violation.
            if a.net == b.net && a.net.is_some() {
                continue;
            }
            let clr = effective_clearance_mm(opts, a.net, b.net);
            let gap = aabb_gap_mm(a.rect, b.rect);
            if gap + 1e-6 < clr {
                let mid = midpoint(a.rect, b.rect);
                report.push(Violation {
                    kind: ViolationKind::PadPadClearance,
                    severity: Severity::Error,
                    message: format!(
                        "pad {} – pad {}: {gap:.3} mm < {clr:.3} mm",
                        a.label(),
                        b.label()
                    ),
                    x_mm: mid.0,
                    y_mm: mid.1,
                    involved: vec![a.label(), b.label()],
                });
            }
        }
    }
}

fn check_trace_trace(board: &Board, opts: &DrcOptions, report: &mut DrcReport) {
    let traces: Vec<&Trace> = board.traces.iter().collect();
    for i in 0..traces.len() {
        for j in (i + 1)..traces.len() {
            let a = traces[i];
            let b = traces[j];
            if a.layer != b.layer {
                continue;
            }
            if a.net == b.net {
                continue;
            }
            let clr = effective_clearance_mm(opts, Some(a.net.as_str()), Some(b.net.as_str()));
            let half_a = a.width.to_mm() / 2.0;
            let half_b = b.width.to_mm() / 2.0;
            let centerline_dist = segment_segment_distance(
                (a.start.x.to_mm(), a.start.y.to_mm()),
                (a.end.x.to_mm(), a.end.y.to_mm()),
                (b.start.x.to_mm(), b.start.y.to_mm()),
                (b.end.x.to_mm(), b.end.y.to_mm()),
            );
            let gap = centerline_dist - half_a - half_b;
            if gap + 1e-6 < clr {
                let pa_mid = (
                    f64::midpoint(a.start.x.to_mm(), a.end.x.to_mm()),
                    f64::midpoint(a.start.y.to_mm(), a.end.y.to_mm()),
                );
                let pb_mid = (
                    f64::midpoint(b.start.x.to_mm(), b.end.x.to_mm()),
                    f64::midpoint(b.start.y.to_mm(), b.end.y.to_mm()),
                );
                report.push(Violation {
                    kind: ViolationKind::TraceTraceClearance,
                    severity: Severity::Error,
                    message: format!(
                        "trace {} – trace {}: {gap:.3} mm < {clr:.3} mm",
                        a.net, b.net
                    ),
                    x_mm: f64::midpoint(pa_mid.0, pb_mid.0),
                    y_mm: f64::midpoint(pa_mid.1, pb_mid.1),
                    involved: vec![a.net.clone(), b.net.clone()],
                });
            }
        }
    }
}

fn check_trace_pad(board: &Board, pads: &[PadGeom], opts: &DrcOptions, report: &mut DrcReport) {
    for trace in &board.traces {
        let half = trace.width.to_mm() / 2.0;
        for pad in pads {
            if !pad.occupies_layer(trace.layer) {
                continue;
            }
            if pad.net == Some(trace.net.as_str()) {
                continue;
            }
            let clr = effective_clearance_mm(opts, Some(trace.net.as_str()), pad.net);
            let centerline_dist = segment_aabb_distance(
                (trace.start.x.to_mm(), trace.start.y.to_mm()),
                (trace.end.x.to_mm(), trace.end.y.to_mm()),
                pad.rect,
            );
            let gap = centerline_dist - half;
            if gap + 1e-6 < clr {
                let mid_p = pad_center(pad.rect);
                report.push(Violation {
                    kind: ViolationKind::TracePadClearance,
                    severity: Severity::Error,
                    message: format!(
                        "trace {} – pad {}: {gap:.3} mm < {clr:.3} mm",
                        trace.net,
                        pad.label()
                    ),
                    x_mm: mid_p.0,
                    y_mm: mid_p.1,
                    involved: vec![trace.net.clone(), pad.label()],
                });
            }
        }
    }
}

fn check_edge(
    board: &Board,
    pads: &[PadGeom],
    outline: Rect,
    opts: &DrcOptions,
    report: &mut DrcReport,
) {
    let clr = opts.edge_clearance.to_mm();
    let ox0 = outline.min.x.to_mm();
    let oy0 = outline.min.y.to_mm();
    let ox1 = outline.max.x.to_mm();
    let oy1 = outline.max.y.to_mm();
    let edge_gap = |x0: f64, y0: f64, x1: f64, y1: f64| -> f64 {
        let inside_x0 = x0 - ox0;
        let inside_x1 = ox1 - x1;
        let inside_y0 = y0 - oy0;
        let inside_y1 = oy1 - y1;
        inside_x0.min(inside_x1).min(inside_y0).min(inside_y1)
    };
    for pad in pads {
        if pad.edge_mounted {
            // USB-C connectors, screw terminals, header breakouts and
            // similar parts are deliberately placed flush to the
            // outline; the edge-clearance rule does not apply to
            // them. The placement validator already checks that the
            // pad bbox sits at the outline.
            continue;
        }
        let r = pad.rect;
        let gap = edge_gap(
            r.min.x.to_mm(),
            r.min.y.to_mm(),
            r.max.x.to_mm(),
            r.max.y.to_mm(),
        );
        if gap + 1e-6 < clr {
            let p = pad_center(r);
            report.push(Violation {
                kind: ViolationKind::EdgeClearance,
                severity: Severity::Error,
                message: format!(
                    "pad {} touches edge: {gap:.3} mm < {clr:.3} mm",
                    pad.label()
                ),
                x_mm: p.0,
                y_mm: p.1,
                involved: vec![pad.label()],
            });
        }
    }
    for trace in &board.traces {
        let half = trace.width.to_mm() / 2.0;
        let xmin = trace.start.x.to_mm().min(trace.end.x.to_mm()) - half;
        let xmax = trace.start.x.to_mm().max(trace.end.x.to_mm()) + half;
        let ymin = trace.start.y.to_mm().min(trace.end.y.to_mm()) - half;
        let ymax = trace.start.y.to_mm().max(trace.end.y.to_mm()) + half;
        let gap = edge_gap(xmin, ymin, xmax, ymax);
        if gap + 1e-6 < clr {
            let mx = f64::midpoint(trace.start.x.to_mm(), trace.end.x.to_mm());
            let my = f64::midpoint(trace.start.y.to_mm(), trace.end.y.to_mm());
            report.push(Violation {
                kind: ViolationKind::EdgeClearance,
                severity: Severity::Error,
                message: format!(
                    "trace {} touches edge: {gap:.3} mm < {clr:.3} mm",
                    trace.net
                ),
                x_mm: mx,
                y_mm: my,
                involved: vec![trace.net.clone()],
            });
        }
    }
    for via in &board.vias {
        let r = via.diameter.to_mm() / 2.0;
        let cx = via.position.x.to_mm();
        let cy = via.position.y.to_mm();
        let gap = edge_gap(cx - r, cy - r, cx + r, cy + r);
        if gap + 1e-6 < clr {
            report.push(Violation {
                kind: ViolationKind::EdgeClearance,
                severity: Severity::Error,
                message: format!("via {} touches edge: {gap:.3} mm < {clr:.3} mm", via.net),
                x_mm: cx,
                y_mm: cy,
                involved: vec![via.net.clone()],
            });
        }
    }
}

fn check_unconnected_pads(board: &Board, pads: &[PadGeom], report: &mut DrcReport) {
    for pad in pads {
        let Some(net) = pad.net else {
            continue;
        };
        // Acceptable: another same-net pad on the same layer overlaps
        // (rare, would mean pads butted together) OR a trace endpoint
        // of the same net touches the pad rect on the same layer OR
        // a via on this net sits on top (any layer) OR the pad lies
        // on a copper pour for the same net on the same layer.
        let touched = pad_has_same_net_neighbour(pads, pad)
            || trace_touches_pad(board, pad, net)
            || via_touches_pad(board, pad, net)
            || pour_covers_pad(&board.pours, pad, net);
        if !touched {
            let p = pad_center(pad.rect);
            report.push(Violation {
                kind: ViolationKind::UnconnectedPad,
                severity: Severity::Warning,
                message: format!("pad {} on net {net} has no copper", pad.label()),
                x_mm: p.0,
                y_mm: p.1,
                involved: vec![pad.label()],
            });
        }
    }
}

/// Threshold below which a footprint is considered a "small" component
/// for the dangling-pad heuristic. Two-pin passives, three-pin SSRs,
/// breakout connectors, etc. should have every pad wired up; an ESP32
/// module legitimately has many unused GPIOs and is excluded.
const SMALL_COMPONENT_PAD_LIMIT: usize = 8;

fn check_small_component_dangling(board: &Board, pads: &[PadGeom], report: &mut DrcReport) {
    for fp in board.footprints.values() {
        if fp.pads.len() >= SMALL_COMPONENT_PAD_LIMIT {
            continue;
        }
        let mut dangling: Vec<String> = Vec::new();
        for fp_pad in &fp.pads {
            let connected = match &fp_pad.net {
                None => false,
                Some(net) => pads
                    .iter()
                    .find(|p| {
                        p.fp_reference == fp.reference && p.pad_number == fp_pad.number.as_str()
                    })
                    .is_some_and(|pg| {
                        pad_has_same_net_neighbour(pads, pg)
                            || trace_touches_pad(board, pg, net)
                            || via_touches_pad(board, pg, net)
                            || pour_covers_pad(&board.pours, pg, net)
                    }),
            };
            if !connected {
                dangling.push(format!("{}.{}", fp.reference, fp_pad.number));
            }
        }
        if dangling.is_empty() {
            continue;
        }
        let cx = fp.position.x.to_mm();
        let cy = fp.position.y.to_mm();
        report.push(Violation {
            kind: ViolationKind::SmallComponentDangling,
            severity: Severity::Warning,
            message: format!(
                "{} ({} pads) has dangling pad(s): {}",
                fp.reference,
                fp.pads.len(),
                dangling.join(", ")
            ),
            x_mm: cx,
            y_mm: cy,
            involved: dangling,
        });
    }
}

fn pad_has_same_net_neighbour(pads: &[PadGeom], pad: &PadGeom) -> bool {
    let Some(net) = pad.net else {
        return false;
    };
    for other in pads {
        if std::ptr::eq(other, pad) {
            continue;
        }
        if !other.shares_layer_with(pad) {
            continue;
        }
        if other.net != Some(net) {
            continue;
        }
        if aabb_gap_mm(pad.rect, other.rect) <= 0.0 {
            return true;
        }
    }
    false
}

fn trace_touches_pad(board: &Board, pad: &PadGeom, net: &str) -> bool {
    for trace in &board.traces {
        if !pad.occupies_layer(trace.layer) || trace.net != net {
            continue;
        }
        let d = segment_aabb_distance(
            (trace.start.x.to_mm(), trace.start.y.to_mm()),
            (trace.end.x.to_mm(), trace.end.y.to_mm()),
            pad.rect,
        );
        let half = trace.width.to_mm() / 2.0;
        if d - half <= 1e-6 {
            return true;
        }
    }
    false
}

/// A pour with `net` filling any layer the pad has copper on is
/// treated as electrical contact for any pad on that net. For SMD
/// pads that's just their mount-side layer; for PTH pads the copper
/// ring exists on every copper layer, so a same-net pour on any
/// signal layer counts. Cross-layer SMD pads still need a via to
/// reach the pour — that case is handled by `via_touches_pad`.
fn pour_covers_pad(pours: &[Pour], pad: &PadGeom, net: &str) -> bool {
    pours.iter().any(|p| p.net == net && pad.occupies_layer(p.layer))
}

fn via_touches_pad(board: &Board, pad: &PadGeom, net: &str) -> bool {
    for via in &board.vias {
        if via.net != net {
            continue;
        }
        let r = via.diameter.to_mm() / 2.0;
        let cx = via.position.x.to_mm();
        let cy = via.position.y.to_mm();
        // Distance from circle to AABB.
        let dx = (cx - pad.rect.max.x.to_mm().min(cx.max(pad.rect.min.x.to_mm()))).abs();
        let dy = (cy - pad.rect.max.y.to_mm().min(cy.max(pad.rect.min.y.to_mm()))).abs();
        let d = (dx * dx + dy * dy).sqrt();
        if d <= r + 1e-6 {
            return true;
        }
    }
    false
}

fn check_narrow_traces(board: &Board, opts: &DrcOptions, report: &mut DrcReport) {
    let min_w = opts.min_trace_width.to_mm();
    for trace in &board.traces {
        let w = trace.width.to_mm();
        if w + 1e-6 < min_w {
            let mx = f64::midpoint(trace.start.x.to_mm(), trace.end.x.to_mm());
            let my = f64::midpoint(trace.start.y.to_mm(), trace.end.y.to_mm());
            report.push(Violation {
                kind: ViolationKind::NarrowTrace,
                severity: Severity::Warning,
                message: format!("trace {} is {w:.3} mm < min {min_w:.3} mm", trace.net),
                x_mm: mx,
                y_mm: my,
                involved: vec![trace.net.clone()],
            });
        }
    }
}

/// Flag every routed net whose total wire length exceeds the HPWL
/// (half-perimeter of the pad bounding box) lower bound by more than
/// `opts.routing_inefficient_ratio`. HPWL is the minimum wire any tree
/// connecting the pads can use, so a high ratio means the router took a
/// detour — usually because some other net's traces were blocking the
/// natural corridor. The fix is almost always to move components, not
/// to change the router's parameters.
fn check_routing_inefficient(board: &Board, opts: &DrcOptions, report: &mut DrcReport) {
    let pour_nets: HashSet<&str> = board.pours.iter().map(|p| p.net.as_str()).collect();

    // Per-net pad world-centres (mm). Skip nets that ride a pour — the
    // pour is the connection, no traces are expected.
    let mut net_pads: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    for fp in board.footprints.values() {
        for pad in &fp.pads {
            let Some(net) = pad.net.as_deref() else {
                continue;
            };
            if pour_nets.contains(net) {
                continue;
            }
            let c = fp.pad_world_center(pad);
            net_pads
                .entry(net.to_string())
                .or_default()
                .push((c.x.to_mm(), c.y.to_mm()));
        }
    }

    // Per-net actual wire length, summed across both layers.
    let mut net_length: HashMap<&str, f64> = HashMap::new();
    for trace in &board.traces {
        let dx = trace.end.x.to_mm() - trace.start.x.to_mm();
        let dy = trace.end.y.to_mm() - trace.start.y.to_mm();
        *net_length.entry(trace.net.as_str()).or_insert(0.0) += (dx * dx + dy * dy).sqrt();
    }

    let threshold = f64::from(opts.routing_inefficient_ratio);
    for (net, pads) in &net_pads {
        if pads.len() < 2 {
            continue;
        }
        let actual = net_length.get(net.as_str()).copied().unwrap_or(0.0);
        // Unrouted nets are caught by UnconnectedPad / SmallComponentDangling;
        // we only opine on what the router did lay.
        if actual <= 1e-6 {
            continue;
        }
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for &(x, y) in pads {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        let hpwl = (max_x - min_x) + (max_y - min_y);
        if hpwl < 1e-3 {
            // All pads on top of each other — degenerate, skip.
            continue;
        }
        let ratio = actual / hpwl;
        if ratio > threshold {
            let cx = pads.iter().map(|p| p.0).sum::<f64>() / pads.len() as f64;
            let cy = pads.iter().map(|p| p.1).sum::<f64>() / pads.len() as f64;
            report.push(Violation {
                kind: ViolationKind::RoutingInefficient,
                severity: Severity::Warning,
                message: format!(
                    "net {net}: {actual:.2} mm of wire vs {hpwl:.2} mm lower bound (ratio {ratio:.2}× — placement is forcing a detour)"
                ),
                x_mm: cx,
                y_mm: cy,
                involved: vec![net.clone()],
            });
        }
    }
}

fn check_small_drills(board: &Board, opts: &DrcOptions, report: &mut DrcReport) {
    let min_d = opts.min_drill.to_mm();
    for via in &board.vias {
        let d = via.drill.to_mm();
        if d + 1e-6 < min_d {
            report.push(Violation {
                kind: ViolationKind::SmallDrill,
                severity: Severity::Warning,
                message: format!(
                    "via on {} drilled at {d:.3} mm < min {min_d:.3} mm",
                    via.net
                ),
                x_mm: via.position.x.to_mm(),
                y_mm: via.position.y.to_mm(),
                involved: vec![via.net.clone()],
            });
        }
    }
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            let Some(drill) = pad.drill else { continue };
            let d = drill.to_mm();
            if d + 1e-6 < min_d {
                let c = fp.pad_world_center(pad);
                let net = pad.net.clone().unwrap_or_default();
                report.push(Violation {
                    kind: ViolationKind::SmallDrill,
                    severity: Severity::Warning,
                    message: format!(
                        "pad {}/{} drilled at {d:.3} mm < min {min_d:.3} mm",
                        fp.reference, pad.number,
                    ),
                    x_mm: c.x.to_mm(),
                    y_mm: c.y.to_mm(),
                    involved: vec![net],
                });
            }
        }
    }
}

// -- Geometry helpers ---------------------------------------------

fn aabb_gap_mm(a: Rect, b: Rect) -> f64 {
    let ax0 = a.min.x.to_mm();
    let ay0 = a.min.y.to_mm();
    let ax1 = a.max.x.to_mm();
    let ay1 = a.max.y.to_mm();
    let bx0 = b.min.x.to_mm();
    let by0 = b.min.y.to_mm();
    let bx1 = b.max.x.to_mm();
    let by1 = b.max.y.to_mm();
    let gap_x = (bx0 - ax1).max(ax0 - bx1).max(0.0);
    let gap_y = (by0 - ay1).max(ay0 - by1).max(0.0);
    (gap_x * gap_x + gap_y * gap_y).sqrt()
}

fn pad_center(r: Rect) -> (f64, f64) {
    (
        f64::midpoint(r.min.x.to_mm(), r.max.x.to_mm()),
        f64::midpoint(r.min.y.to_mm(), r.max.y.to_mm()),
    )
}

fn midpoint(a: Rect, b: Rect) -> (f64, f64) {
    let pa = pad_center(a);
    let pb = pad_center(b);
    (f64::midpoint(pa.0, pb.0), f64::midpoint(pa.1, pb.1))
}

fn point_segment_distance(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-12 {
        let ex = p.0 - a.0;
        let ey = p.1 - a.1;
        return (ex * ex + ey * ey).sqrt();
    }
    let t = ((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2;
    let t = t.clamp(0.0, 1.0);
    let cx = a.0 + t * dx;
    let cy = a.1 + t * dy;
    let ex = p.0 - cx;
    let ey = p.1 - cy;
    (ex * ex + ey * ey).sqrt()
}

fn segment_segment_distance(a0: (f64, f64), a1: (f64, f64), b0: (f64, f64), b1: (f64, f64)) -> f64 {
    if segments_intersect(a0, a1, b0, b1) {
        return 0.0;
    }
    point_segment_distance(a0, b0, b1)
        .min(point_segment_distance(a1, b0, b1))
        .min(point_segment_distance(b0, a0, a1))
        .min(point_segment_distance(b1, a0, a1))
}

fn segments_intersect(a0: (f64, f64), a1: (f64, f64), b0: (f64, f64), b1: (f64, f64)) -> bool {
    fn orient(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> f64 {
        (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
    }
    let o1 = orient(a0, a1, b0);
    let o2 = orient(a0, a1, b1);
    let o3 = orient(b0, b1, a0);
    let o4 = orient(b0, b1, a1);
    (o1 * o2 < 0.0) && (o3 * o4 < 0.0)
}

fn segment_aabb_distance(a: (f64, f64), b: (f64, f64), rect: Rect) -> f64 {
    let rx0 = rect.min.x.to_mm();
    let ry0 = rect.min.y.to_mm();
    let rx1 = rect.max.x.to_mm();
    let ry1 = rect.max.y.to_mm();
    // If either endpoint is inside the rect → 0.
    let endpoint_inside =
        |p: (f64, f64)| -> bool { p.0 >= rx0 && p.0 <= rx1 && p.1 >= ry0 && p.1 <= ry1 };
    if endpoint_inside(a) || endpoint_inside(b) {
        return 0.0;
    }
    // Distance from segment to each rect edge.
    let edges = [
        ((rx0, ry0), (rx1, ry0)),
        ((rx1, ry0), (rx1, ry1)),
        ((rx1, ry1), (rx0, ry1)),
        ((rx0, ry1), (rx0, ry0)),
    ];
    let mut best = f64::INFINITY;
    for (e0, e1) in edges {
        let d = segment_segment_distance(a, b, e0, e1);
        if d < best {
            best = d;
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Net continuity — the exhaustive multimeter.
//
// The clearance/`UnconnectedPad` checks above answer "is the geometry legal"
// and "does each pad touch *some* copper of its net". They do NOT answer the
// two questions a bench multimeter answers in seconds and that account for the
// most embarrassing fabbed-board failures:
//
//   * NetSplit (open): two pads that the schematic says are one node end up on
//     two disconnected copper islands. Each pad may be locally routed, so
//     `UnconnectedPad` stays silent, yet the net is physically broken.
//   * NetShort: copper of two different nets actually meets. The clearance
//     checks flag "too close"; they do not single out the gap == 0 case as the
//     definite wiring error it is, nor phrase it as "net A == net B".
//
// Both fall out of one union-find over the board's copper. O(n²) touch tests —
// fine for the element counts of a hand-designed board; revisit with a grid
// index if a generated board ever pushes this into the thousands.
// ---------------------------------------------------------------------------

/// Touch tolerance (mm). Copper meant to connect overlaps (gap ≈ 0); we treat
/// anything within this of contact as touching. Mirrors the `1e-6` slack the
/// surrounding geometry checks already use.
const TOUCH_TOL_MM: f64 = 1e-6;

/// One piece of copper, flattened into the connectivity graph.
enum CopperShape {
    Pad(Rect),
    Trace { a: (f64, f64), b: (f64, f64), half: f64 },
    Via { c: (f64, f64), r: f64 },
    /// A pour fills its whole layer for its net — the model carries no
    /// polygon yet (see [`pcb_core::Pour`]), so it has no geometry of its
    /// own and instead binds every same-net item that shares its layer.
    Pour,
}

struct CopperElem {
    net: String,
    /// `None` ⇒ present on every copper layer (vias, PTH pads). `Some(l)` ⇒
    /// only layer `l` (SMD pads, traces, pours).
    layer: Option<CopperLayer>,
    shape: CopperShape,
    is_pad: bool,
    /// Label for messages — pad `ref.num`, or `"trace"`/`"via"`/`"pour"`.
    label: String,
    /// Representative point for the violation marker (unused for pours).
    center: (f64, f64),
}

/// Disjoint-set with path halving + union by rank.
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self { parent: (0..n).collect(), rank: vec![0; n] }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
}

/// Two layers overlap if either spans all layers (`None`) or they are equal.
fn layers_overlap(a: Option<CopperLayer>, b: Option<CopperLayer>) -> bool {
    matches!((a, b), (None, _) | (_, None)) || a == b
}

/// Distance (mm) from a point to an axis-aligned rect (0 if inside).
fn point_rect_distance(p: (f64, f64), rect: Rect) -> f64 {
    let dx = (rect.min.x.to_mm() - p.0).max(p.0 - rect.max.x.to_mm()).max(0.0);
    let dy = (rect.min.y.to_mm() - p.1).max(p.1 - rect.max.y.to_mm()).max(0.0);
    (dx * dx + dy * dy).sqrt()
}

/// True iff two copper elements physically meet on a shared layer.
fn elems_touch(a: &CopperElem, b: &CopperElem) -> bool {
    use CopperShape::{Pad, Pour, Trace, Via};
    if !layers_overlap(a.layer, b.layer) {
        return false;
    }
    match (&a.shape, &b.shape) {
        // A pour binds anything sharing its layer (geometry-less by model).
        (Pour, _) | (_, Pour) => true,
        (Pad(ra), Pad(rb)) => aabb_gap_mm(*ra, *rb) <= TOUCH_TOL_MM,
        (Pad(rect), Trace { a, b, half }) | (Trace { a, b, half }, Pad(rect)) => {
            segment_aabb_distance(*a, *b, *rect) - *half <= TOUCH_TOL_MM
        }
        (Pad(rect), Via { c, r }) | (Via { c, r }, Pad(rect)) => {
            point_rect_distance(*c, *rect) - *r <= TOUCH_TOL_MM
        }
        (Trace { a: a0, b: a1, half: h0 }, Trace { a: b0, b: b1, half: h1 }) => {
            segment_segment_distance(*a0, *a1, *b0, *b1) - (*h0 + *h1) <= TOUCH_TOL_MM
        }
        (Trace { a, b, half }, Via { c, r }) | (Via { c, r }, Trace { a, b, half }) => {
            point_segment_distance(*c, *a, *b) - (*half + *r) <= TOUCH_TOL_MM
        }
        (Via { c: c0, r: r0 }, Via { c: c1, r: r1 }) => {
            let (dx, dy) = (c0.0 - c1.0, c0.1 - c1.1);
            (dx * dx + dy * dy).sqrt() - (*r0 + *r1) <= TOUCH_TOL_MM
        }
    }
}

/// Flatten every net-bearing copper item on the board into graph nodes.
fn build_copper_elems(board: &Board, pads: &[PadGeom]) -> Vec<CopperElem> {
    let mut elems = Vec::new();
    for p in pads {
        let Some(net) = p.net else { continue };
        elems.push(CopperElem {
            net: net.to_string(),
            layer: if p.is_through_hole { None } else { Some(p.layer) },
            shape: CopperShape::Pad(p.rect),
            is_pad: true,
            label: p.label(),
            center: pad_center(p.rect),
        });
    }
    for t in &board.traces {
        let a = (t.start.x.to_mm(), t.start.y.to_mm());
        let b = (t.end.x.to_mm(), t.end.y.to_mm());
        elems.push(CopperElem {
            net: t.net.clone(),
            layer: Some(t.layer),
            shape: CopperShape::Trace { a, b, half: t.width.to_mm() / 2.0 },
            is_pad: false,
            label: "trace".into(),
            center: (f64::midpoint(a.0, b.0), f64::midpoint(a.1, b.1)),
        });
    }
    for v in &board.vias {
        let c = (v.position.x.to_mm(), v.position.y.to_mm());
        elems.push(CopperElem {
            net: v.net.clone(),
            layer: None,
            shape: CopperShape::Via { c, r: v.diameter.to_mm() / 2.0 },
            is_pad: false,
            label: "via".into(),
            center: c,
        });
    }
    for pr in &board.pours {
        elems.push(CopperElem {
            net: pr.net.clone(),
            layer: Some(pr.layer),
            shape: CopperShape::Pour,
            is_pad: false,
            label: "pour".into(),
            center: (0.0, 0.0),
        });
    }
    elems
}

fn check_net_continuity(board: &Board, pads: &[PadGeom], report: &mut DrcReport) {
    let elems = build_copper_elems(board, pads);
    let n = elems.len();
    if n == 0 {
        return;
    }

    let mut by_net: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, e) in elems.iter().enumerate() {
        by_net.entry(e.net.as_str()).or_default().push(i);
    }

    // --- Pass A: intra-net islands (opens). ---
    for (net, idxs) in &by_net {
        let net = *net;
        // A net needs ≥2 pads before "they must be common" can be violated.
        let pad_idxs: Vec<usize> = idxs.iter().copied().filter(|&i| elems[i].is_pad).collect();
        if pad_idxs.len() < 2 {
            continue;
        }

        let mut uf = UnionFind::new(n);
        // Track which pads got joined to *something* same-net; a pad joined to
        // nothing is `UnconnectedPad`'s job, not a split.
        let mut touched = vec![false; n];
        for a in 0..idxs.len() {
            for b in (a + 1)..idxs.len() {
                let (ia, ib) = (idxs[a], idxs[b]);
                if elems_touch(&elems[ia], &elems[ib]) {
                    uf.union(ia, ib);
                    touched[ia] = true;
                    touched[ib] = true;
                }
            }
        }

        let mut islands: HashMap<usize, Vec<usize>> = HashMap::new();
        for &pi in &pad_idxs {
            if !touched[pi] {
                continue;
            }
            let root = uf.find(pi);
            islands.entry(root).or_default().push(pi);
        }
        if islands.len() < 2 {
            continue;
        }

        let mut groups: Vec<Vec<usize>> = islands.into_values().collect();
        // Smallest island first — the likely culprit, and where we drop the marker.
        groups.sort_by_key(Vec::len);
        let group_strs: Vec<String> = groups
            .iter()
            .map(|g| {
                let mut names: Vec<&str> = g.iter().map(|&i| elems[i].label.as_str()).collect();
                names.sort_unstable();
                format!("{{{}}}", names.join(", "))
            })
            .collect();
        let marker = elems[groups[0][0]].center;
        let mut involved: Vec<String> =
            pad_idxs.iter().map(|&i| elems[i].label.clone()).collect();
        involved.sort();
        report.push(Violation {
            kind: ViolationKind::NetSplit,
            severity: Severity::Error,
            message: format!(
                "net \"{net}\" is split into {} isolated copper islands: {}",
                groups.len(),
                group_strs.join(" | "),
            ),
            x_mm: marker.0,
            y_mm: marker.1,
            involved,
        });
    }

    // --- Pass B: inter-net shorts. ---
    // Pours are excluded: the model floods the whole layer with no polygon
    // clipping, so a pour overlaps every other net on its layer — a model
    // artefact, not a real short. Different-net spacing stays the clearance
    // checks' job; this fires only on actual contact.
    let mut shorted: HashSet<(&str, &str)> = HashSet::new();
    for i in 0..n {
        if matches!(elems[i].shape, CopperShape::Pour) {
            continue;
        }
        for j in (i + 1)..n {
            if matches!(elems[j].shape, CopperShape::Pour) || elems[i].net == elems[j].net {
                continue;
            }
            if !elems_touch(&elems[i], &elems[j]) {
                continue;
            }
            let (a, b) = if elems[i].net <= elems[j].net {
                (elems[i].net.as_str(), elems[j].net.as_str())
            } else {
                (elems[j].net.as_str(), elems[i].net.as_str())
            };
            if !shorted.insert((a, b)) {
                continue;
            }
            let marker = (
                f64::midpoint(elems[i].center.0, elems[j].center.0),
                f64::midpoint(elems[i].center.1, elems[j].center.1),
            );
            report.push(Violation {
                kind: ViolationKind::NetShort,
                severity: Severity::Error,
                message: format!(
                    "nets \"{a}\" and \"{b}\" are shorted — {} and {} copper touch",
                    elems[i].label, elems[j].label,
                ),
                x_mm: marker.0,
                y_mm: marker.1,
                involved: vec![a.to_string(), b.to_string()],
            });
        }
    }
}

#[cfg(test)]
mod feature3_tests {
    use super::*;
    use pcb_core::{Board, Footprint, Id, NetClass, Pad, Point, Schematic, Trace};
    use std::sync::Arc;

    fn fr4_stackup() -> LayerStackup {
        LayerStackup::default()
    }

    #[test]
    fn impedance_within_tolerance_passes_drc() {
        // Standard FR-4 1.5 mm 1 oz: 50 Ω microstrip ≈ 2.85 mm wide.
        // Compute the actual width that matches first, then set it on
        // the class so the test is robust to formula constants.
        let stackup = fr4_stackup();
        let w_50 = suggest_trace_width_for_impedance(50.0, &stackup);
        let mut sch = Schematic::new();
        sch.set_net_class(NetClass {
            name: "rf50".into(),
            trace_width_mm: Some(w_50),
            target_impedance_ohms: Some(50.0),
            ..NetClass::default()
        });
        sch.assign_net_to_class("RFOUT", "rf50");
        sch.set_net(pcb_core::Net {
            name: "RFOUT".into(),
            connections: vec![],
            class: Some("rf50".into()),
        });

        let mut board = Board::new();
        board.stackup = stackup;
        let opts = DrcOptions {
            schematic: Some(Arc::new(sch)),
            ..DrcOptions::default()
        };
        let report = run(&board, &opts);
        assert!(
            !report.violations.iter().any(|v| v.kind == ViolationKind::ImpedanceMismatch),
            "expected no impedance mismatch, got {:?}",
            report.violations.iter().map(|v| (v.kind, v.message.clone())).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn impedance_too_wide_flags_drc() {
        // 0.25 mm trace on 1.5 mm FR-4 → ~110 Ω, far from 50 Ω target.
        let stackup = fr4_stackup();
        let mut sch = Schematic::new();
        sch.set_net_class(NetClass {
            name: "rf50".into(),
            trace_width_mm: Some(0.25),
            target_impedance_ohms: Some(50.0),
            ..NetClass::default()
        });
        sch.assign_net_to_class("RFOUT", "rf50");
        sch.set_net(pcb_core::Net {
            name: "RFOUT".into(),
            connections: vec![],
            class: Some("rf50".into()),
        });

        let mut board = Board::new();
        board.stackup = stackup;
        let opts = DrcOptions {
            schematic: Some(Arc::new(sch)),
            ..DrcOptions::default()
        };
        let report = run(&board, &opts);
        assert!(
            report.violations.iter().any(|v| v.kind == ViolationKind::ImpedanceMismatch),
            "expected impedance mismatch for narrow trace, got {:?}",
            report.violations.iter().map(|v| (v.kind, v.message.clone())).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn suggest_width_matches_target_after_recompute() {
        let stackup = fr4_stackup();
        let target = 50.0;
        let w = suggest_trace_width_for_impedance(target, &stackup);
        let z = compute_microstrip_z0(
            w,
            stackup.dielectric_thickness_mm(),
            stackup.dielectric_er(),
            stackup.copper_thickness_mm(),
        );
        assert!(
            (z - target).abs() < 0.5,
            "suggested width {w:.4} mm gives {z:.2} Ω, expected {target} Ω",
        );
    }

    fn make_pad_fp(net: &str) -> Footprint {
        Footprint {
            id: Id::new(),
            reference: "U1".into(),
            value: String::new(),
            library: "lib".into(),
            position: Point::ORIGIN,
            rotation: 0.0,
            layer: pcb_core::CopperLayer::Top,
            pads: vec![Pad {
                number: "1".into(),
                name: String::new(),
                offset: Point::ORIGIN,
                size: (pcb_core::Length::from_mm(1.0), pcb_core::Length::from_mm(1.0)),
                layer: pcb_core::CopperLayer::Top,
                net: Some(net.into()),
                drill: None,
            }],
            key: String::new(),
            description: String::new(),
            edge_mounted: false,
            silk: vec![],
        }
    }

    fn jlc_profile() -> FabProfile {
        FabProfile {
            name: "jlcpcb".into(),
            min_trace_width_mm: 0.127,
            min_clearance_mm: 0.127,
            min_drill_mm: 0.20,
            min_annular_ring_mm: 0.13,
            min_via_diameter_mm: 0.45,
            min_edge_clearance_mm: 0.20,
            max_board_size_mm: (100.0, 100.0),
        }
    }

    #[test]
    fn jlcpcb_profile_flags_subminimum_trace() {
        let mut board = Board::new();
        board.add_footprint(make_pad_fp("SIG"));
        board.add_trace(Trace {
            id: Id::new(),
            layer: pcb_core::CopperLayer::Top,
            start: Point::new(pcb_core::Length::from_mm(0.0), pcb_core::Length::from_mm(0.0)),
            end: Point::new(pcb_core::Length::from_mm(2.0), pcb_core::Length::from_mm(0.0)),
            // 0.10 mm is below JLCPCB's 0.127 mm minimum.
            width: pcb_core::Length::from_mm(0.10),
            net: "SIG".into(),
        });
        let opts = DrcOptions {
            fab_profile: Some(jlc_profile()),
            ..DrcOptions::default()
        };
        let report = run(&board, &opts);
        assert!(
            report.violations.iter().any(|v| v.kind == ViolationKind::FabProfileMin),
            "expected FabProfileMin, got {:?}",
            report.violations.iter().map(|v| (v.kind, v.message.clone())).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn jlcpcb_profile_accepts_default_trace() {
        let mut board = Board::new();
        board.add_footprint(make_pad_fp("SIG"));
        // 0.25 mm — well above the 0.127 mm JLCPCB minimum.
        board.add_trace(Trace {
            id: Id::new(),
            layer: pcb_core::CopperLayer::Top,
            start: Point::new(pcb_core::Length::from_mm(0.0), pcb_core::Length::from_mm(0.0)),
            end: Point::new(pcb_core::Length::from_mm(2.0), pcb_core::Length::from_mm(0.0)),
            width: pcb_core::Length::from_mm(0.25),
            net: "SIG".into(),
        });
        let opts = DrcOptions {
            fab_profile: Some(jlc_profile()),
            ..DrcOptions::default()
        };
        let report = run(&board, &opts);
        assert!(
            !report.violations.iter().any(|v| v.kind == ViolationKind::FabProfileMin),
            "should not flag a 0.25 mm trace; got {:?}",
            report.violations.iter().map(|v| (v.kind, v.message.clone())).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn clear_profile_drops_violations() {
        let mut board = Board::new();
        board.add_footprint(make_pad_fp("SIG"));
        board.add_trace(Trace {
            id: Id::new(),
            layer: pcb_core::CopperLayer::Top,
            start: Point::new(pcb_core::Length::from_mm(0.0), pcb_core::Length::from_mm(0.0)),
            end: Point::new(pcb_core::Length::from_mm(2.0), pcb_core::Length::from_mm(0.0)),
            // Below JLCPCB minimum but above the default min_trace_width.
            width: pcb_core::Length::from_mm(0.10),
            net: "SIG".into(),
        });
        // With the profile set we expect a FabProfileMin.
        let with_profile = run(
            &board,
            &DrcOptions {
                fab_profile: Some(jlc_profile()),
                ..DrcOptions::default()
            },
        );
        assert!(with_profile.violations.iter().any(|v| v.kind == ViolationKind::FabProfileMin));
        // Without the profile, no FabProfileMin should appear (NarrowTrace may, with default).
        let without = run(&board, &DrcOptions::default());
        assert!(
            !without.violations.iter().any(|v| v.kind == ViolationKind::FabProfileMin),
            "clearing the profile should drop FabProfileMin violations",
        );
    }

    // --- Net continuity (the multimeter) ---------------------------------

    /// One single-pad footprint of `net` centred at (`x_mm`, `y_mm`),
    /// 1×1 mm pad on Top.
    fn pad_fp_at(reference: &str, net: &str, x_mm: f64, y_mm: f64) -> Footprint {
        Footprint {
            id: Id::new(),
            reference: reference.into(),
            value: String::new(),
            library: "lib".into(),
            position: Point::new(
                pcb_core::Length::from_mm(x_mm),
                pcb_core::Length::from_mm(y_mm),
            ),
            rotation: 0.0,
            layer: pcb_core::CopperLayer::Top,
            pads: vec![Pad {
                number: "1".into(),
                name: String::new(),
                offset: Point::ORIGIN,
                size: (pcb_core::Length::from_mm(1.0), pcb_core::Length::from_mm(1.0)),
                layer: pcb_core::CopperLayer::Top,
                net: Some(net.into()),
                drill: None,
            }],
            key: String::new(),
            description: String::new(),
            edge_mounted: false,
            silk: vec![],
        }
    }

    fn trace(net: &str, x0: f64, y0: f64, x1: f64, y1: f64) -> Trace {
        Trace {
            id: Id::new(),
            layer: pcb_core::CopperLayer::Top,
            start: Point::new(pcb_core::Length::from_mm(x0), pcb_core::Length::from_mm(y0)),
            end: Point::new(pcb_core::Length::from_mm(x1), pcb_core::Length::from_mm(y1)),
            width: pcb_core::Length::from_mm(0.25),
            net: net.into(),
        }
    }

    #[test]
    fn net_split_into_two_islands_flags_open() {
        // Two GND pads 10 mm apart, each locally routed by its own stub, but
        // the two stubs never meet — a classic "reads open on the multimeter"
        // bug that pad-by-pad routing hides.
        let mut board = Board::new();
        board.add_footprint(pad_fp_at("R1", "GND", 0.0, 0.0));
        board.add_footprint(pad_fp_at("R2", "GND", 10.0, 0.0));
        board.add_trace(trace("GND", 0.0, 0.0, 4.0, 0.0)); // touches R1 only
        board.add_trace(trace("GND", 6.0, 0.0, 10.0, 0.0)); // touches R2 only

        let report = run(&board, &DrcOptions::default());
        assert!(
            report.violations.iter().any(|v| v.kind == ViolationKind::NetSplit),
            "expected NetSplit, got {:?}",
            report.violations.iter().map(|v| (v.kind, v.message.clone())).collect::<Vec<_>>(),
        );
        // Both pads are locally connected, so this is NOT an UnconnectedPad.
        assert!(
            !report.violations.iter().any(|v| v.kind == ViolationKind::UnconnectedPad),
            "split with local copper must not double-report as UnconnectedPad",
        );
    }

    #[test]
    fn fully_routed_net_has_no_split() {
        // Same two pads, one trace spanning both → one island.
        let mut board = Board::new();
        board.add_footprint(pad_fp_at("R1", "GND", 0.0, 0.0));
        board.add_footprint(pad_fp_at("R2", "GND", 10.0, 0.0));
        board.add_trace(trace("GND", 0.0, 0.0, 10.0, 0.0));

        let report = run(&board, &DrcOptions::default());
        assert!(
            !report.violations.iter().any(|v| v.kind == ViolationKind::NetSplit),
            "a continuously routed net must not flag NetSplit, got {:?}",
            report.violations.iter().map(|v| (v.kind, v.message.clone())).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn crossing_nets_flag_short() {
        // VCC runs horizontally, GND vertically; they cross at (2.5, 0).
        let mut board = Board::new();
        board.add_footprint(pad_fp_at("U1", "VCC", -5.0, 0.0));
        board.add_footprint(pad_fp_at("U2", "GND", 2.5, -5.0));
        board.add_trace(trace("VCC", 0.0, 0.0, 5.0, 0.0));
        board.add_trace(trace("GND", 2.5, -2.0, 2.5, 2.0));

        let report = run(&board, &DrcOptions::default());
        let short = report.violations.iter().find(|v| v.kind == ViolationKind::NetShort);
        let short = short.expect("expected a NetShort for crossing VCC/GND traces");
        assert!(
            short.involved.contains(&"VCC".to_string())
                && short.involved.contains(&"GND".to_string()),
            "short should name both nets, got {:?}",
            short.involved,
        );
    }

    #[test]
    fn parallel_clear_nets_have_no_short() {
        // Same two nets, 3 mm apart, never touching.
        let mut board = Board::new();
        board.add_footprint(pad_fp_at("U1", "VCC", -5.0, 0.0));
        board.add_footprint(pad_fp_at("U2", "GND", -5.0, 3.0));
        board.add_trace(trace("VCC", 0.0, 0.0, 5.0, 0.0));
        board.add_trace(trace("GND", 0.0, 3.0, 5.0, 3.0));

        let report = run(&board, &DrcOptions::default());
        assert!(
            !report.violations.iter().any(|v| v.kind == ViolationKind::NetShort),
            "well-separated nets must not flag NetShort, got {:?}",
            report.violations.iter().map(|v| (v.kind, v.message.clone())).collect::<Vec<_>>(),
        );
    }
}
