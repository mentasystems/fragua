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

use serde::Serialize;

use pcb_core::{Board, CopperLayer, Footprint, Length, Pad, PlacementMargin, Pour, Rect, Trace};

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
            placement_margins: HashMap::new(),
        }
    }
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
    check_narrow_traces(board, opts, &mut report);
    check_small_drills(board, opts, &mut report);
    check_routing_inefficient(board, opts, &mut report);
    check_body_overlap(board, opts, &mut report);
    if let Some(outline) = board.outline {
        check_body_off_board(board, outline, opts, &mut report);
    }
    report
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
    layer: CopperLayer,
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
}

fn collect_pad_geometry(board: &Board) -> Vec<PadGeom<'_>> {
    let mut out = Vec::new();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            out.push(PadGeom {
                fp_reference: fp.reference.as_str(),
                pad_number: pad.number.as_str(),
                layer: pad.layer,
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
            if a.layer != b.layer {
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
            if pad.layer != trace.layer {
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
        if other.layer != pad.layer {
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
        if trace.layer != pad.layer || trace.net != net {
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

/// A pour with `net` filling `pad.layer` is treated as electrical
/// ground for any pad on that net + layer. Cross-layer pads need a
/// via to reach the pour — that case is handled by `via_touches_pad`.
fn pour_covers_pad(pours: &[Pour], pad: &PadGeom, net: &str) -> bool {
    pours.iter().any(|p| p.net == net && p.layer == pad.layer)
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
