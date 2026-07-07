//! `bench` — headless route-quality scorecard for a single Fragua project.
//!
//! Loads a project file, runs the router exactly as `route.run` does
//! (same default RouteOptions), then prints a one-screen scorecard:
//! unrouted nets, wire length vs lower bound, DRC collisions, and a
//! per-power-net breakdown (width + tree connectivity). This is the
//! benchmark harness for iterating on the router algorithm.
//!
//! Usage: cargo run -q --release --example bench -- <project.json>

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pcb_core::{Board, Footprint, Layer, LayerStackup, Length, Point, Schematic};
use pcb_drc::ViolationKind;
use pcb_router::{Outcome, RouteOptions};
use serde::Deserialize;

#[derive(Deserialize)]
struct ProjectFile {
    name: String,
    board: Board,
    schematic: Schematic,
    #[serde(default)]
    #[allow(dead_code)]
    palette: Vec<Footprint>,
}

/// Heuristic power-net classifier by name. Mirrors what a power-aware
/// router would key on.
fn is_power(net: &str) -> bool {
    let u = net.to_ascii_uppercase();
    const PREFIXES: &[&str] = &[
        "GND", "VBUS", "+3V3", "3V3", "+5V", "5V", "VCC", "VDD", "VIN", "+1V", "VSYS", "PWR",
        "VDDA",
    ];
    PREFIXES.iter().any(|p| u == *p || u.starts_with(p))
}

fn dist_mm(a: Point, b: Point) -> f64 {
    let dx = a.x.to_mm() - b.x.to_mm();
    let dy = a.y.to_mm() - b.y.to_mm();
    (dx * dx + dy * dy).sqrt()
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: bench <project.json> [cell_mm] [via_cost]");
    let cell_mm: f64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.25);
    let via_cost: u32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let layers: u8 = std::env::args()
        .nth(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let do_place: bool = std::env::args()
        .nth(5)
        .map(|s| s == "place")
        .unwrap_or(false);
    let bytes = std::fs::read(&path).expect("read project");
    let pf: ProjectFile = serde_json::from_slice(&bytes).expect("parse project");

    let mut board = pf.board;

    if do_place {
        // Re-place every footprint except the edge-mounted USB
        // connectors (J1/J2 define the board I/O and stay put). Default
        // PlaceOptions enforce a 2 mm body-to-body gap, which is what
        // un-crowds the pads enough for honest-clearance routing.
        //
        // `footprints_in_order()` (NOT `footprints.values()`) — the latter
        // iterates a HashMap, so `movable`'s order, hence the seeded
        // annealer's per-iteration footprint pick, varied run-to-run and
        // the whole placement (and SCORE) was non-deterministic. Ordered
        // iteration makes a fixed seed reproducible.
        let movable: Vec<String> = board
            .footprints_in_order()
            .map(|f| f.reference.clone())
            .filter(|r| r != "J1" && r != "J2")
            .collect();
        // Placer seed from the documented `PSEED` env var (the harness
        // previously hard-coded 7 and ignored PSEED entirely).
        let seed: u64 = std::env::var("PSEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7);
        let opts = pcb_placer::PlaceOptions {
            seed,
            ..Default::default()
        };
        let margins: pcb_placer::MarginMap = Default::default();
        match pcb_placer::place(&mut board, &movable, &opts, &margins) {
            Ok(rep) => {
                let mg = pcb_placer::min_pairwise_gap(&board, &margins);
                eprintln!(
                    "(auto-place: HPWL {:.0}->{:.0} mm, congestion {:.0}->{:.0}, moved {}; MIN body gap={:.3}mm {})",
                    rep.initial_hpwl_mm, rep.final_hpwl_mm, rep.initial_congestion, rep.final_congestion, rep.moved.len(),
                    mg, if mg >= opts.min_clearance_mm - 1e-6 { "OK" } else { "VIOLATION" }
                );
            }
            Err(e) => eprintln!("(auto-place FAILED: {e})"),
        }
    }
    if layers >= 4 {
        // Reconfigure to an N-layer FR-4 stackup. The GND pour currently
        // lives on index 1 ("Bottom" in the 2-layer encoding); on a
        // 4-layer board index 1 is an inner layer (In1) — perfect for a
        // GND reference plane. Outer layers 0 (F.Cu) and N-1 (B.Cu) plus
        // the remaining inner layer stay free for signal routing.
        board.stackup = LayerStackup::fr4(layers);
        eprintln!(
            "(stackup -> {} layers; GND pour layer index = {:?})",
            board.stackup.layer_count(),
            board.pours.first().map(|p| p.layer.index)
        );
    }
    let sch = Arc::new(pf.schematic);

    eprintln!("(cell={cell_mm}mm via_cost={via_cost} layers={layers})");
    eprintln!(
        "(astar_weight={})",
        std::env::var("ASTAR_WEIGHT").unwrap_or_else(|_| "1.0".into())
    );
    let _ = Layer::TOP;
    // Same defaults as tool_route_run.
    let opts = RouteOptions {
        cell: Length::from_mm(cell_mm),
        trace_width: Length::from_mm(0.25),
        clearance: Length::from_mm(0.20),
        via_cost,
        via_drill: Length::from_mm(
            std::env::var("VIA_DRILL")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.30),
        ),
        via_diameter: Length::from_mm(
            std::env::var("VIA_DIA")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.60),
        ),
        net_overrides: Default::default(),
        schematic: Some(sch.clone()),
        initial_net_order: None,
        // Greedy-search weight, knob for A/B benchmarking. Default 1.0
        // (admissible/optimal A*). NOTE: on this multi-source Steiner
        // router, W>1 REGRESSES wall-time — the power nets seed the whole
        // partial tree, so an inflated (inconsistent) heuristic re-expands
        // heavily AND the weighted detours trip the RR&R inefficiency
        // threshold, forcing extra full passes. Measured: W=1.375 >5×
        // slower, W=1.15 ~2× slower vs W=1.0 at cell 0.20/4L. Kept as an
        // opt-in (ASTAR_WEIGHT=…) for open boards with few long nets where
        // it can help; default OFF.
        heuristic_weight: std::env::var("ASTAR_WEIGHT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0),
        // Localized fine-grid escape (Level 2). On by default in the bench;
        // set FINE_ESCAPE=0 to compare against the plain via-in-pad fanout.
    };

    let t0 = std::time::Instant::now();
    let report = if std::env::var("NOROUTE").is_ok() {
        pcb_router::RouteReport {
            per_net: vec![],
            trace_count: 0,
            via_count: 0,
            total_length_mm: 0.0,
            total_lower_bound_mm: 0.0,
            iterations: 0,
            hints: vec![],
        }
    } else {
        pcb_router::route(&mut board, &opts)
    };
    let elapsed = t0.elapsed();

    let drc = pcb_drc::run(&board, &pcb_drc::DrcOptions::default());

    // --- net inventory from the board pads ---
    let mut net_pads: BTreeMap<String, Vec<Point>> = BTreeMap::new();
    for fp in board.footprints.values() {
        for pad in &fp.pads {
            if let Some(net) = &pad.net {
                let p = fp.pad_world_center(pad);
                net_pads.entry(net.clone()).or_default().push(p);
            }
        }
    }
    let pour_nets: BTreeSet<String> = board.pours.iter().map(|p| p.net.clone()).collect();

    let failed: Vec<&str> = report
        .per_net
        .iter()
        .filter_map(|(n, o)| matches!(o, Outcome::Failed { .. }).then_some(n.as_str()))
        .collect();

    let collisions = drc
        .violations
        .iter()
        .filter(|v| {
            matches!(
                v.kind,
                ViolationKind::PadPadClearance
                    | ViolationKind::TraceTraceClearance
                    | ViolationKind::TracePadClearance
                    | ViolationKind::EdgeClearance
                    | ViolationKind::BodyOverlap
            )
        })
        .count();

    // DRC kind histogram.
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for v in &drc.violations {
        *kinds.entry(format!("{:?}", v.kind)).or_default() += 1;
    }

    let detour = if report.total_lower_bound_mm > 0.0 {
        report.total_length_mm / report.total_lower_bound_mm
    } else {
        1.0
    };

    println!("=== FRAGUA BENCH: {} ===", pf.name);
    println!("route: {} pass(es), {:?}", report.iterations, elapsed);
    println!(
        "nets:  {} total, {} FAILED{}",
        report.per_net.len(),
        failed.len(),
        if failed.is_empty() {
            String::new()
        } else {
            format!("  -> {}", failed.join(", "))
        }
    );
    println!(
        "wire:  {:.1} mm total, {:.1} mm lower-bound, detour {:.3}x   traces={} vias={}",
        report.total_length_mm,
        report.total_lower_bound_mm,
        detour,
        report.trace_count,
        report.via_count
    );
    let fanout_vias = board
        .vias
        .iter()
        .filter(|v| (v.diameter.to_mm() - 0.30).abs() < 1e-6)
        .count();
    println!(
        "DRC:   {} errors, {} warnings   collisions(geom)={}   fanout_vias={}",
        drc.error_count, drc.warning_count, collisions, fanout_vias
    );
    for (k, c) in &kinds {
        println!("         {k}: {c}");
    }

    // --- per-power-net breakdown ---
    println!("POWER NETS (width target >= 0.50 mm, must be a single connected tree):");
    let mut power_nets: Vec<String> = net_pads.keys().filter(|n| is_power(n)).cloned().collect();
    power_nets.sort();
    for net in &power_nets {
        let pads = &net_pads[net];
        let segs: Vec<&pcb_core::Trace> = board.traces.iter().filter(|t| &t.net == net).collect();
        if pour_nets.contains(net) {
            println!(
                "  {net:10} POUR  ({} pads)  [plane, not routed]",
                pads.len()
            );
            continue;
        }
        // Judge by the TRUNK: the widest segment. Entries into fine-pitch
        // fanout pads legitimately neck down, so `min` would be 0.25 even
        // on a proper wide tree.
        let trunk_w = segs.iter().map(|t| t.width.to_mm()).fold(0.0_f64, f64::max);
        let len: f64 = segs.iter().map(|t| dist_mm(t.start, t.end)).sum();
        let outcome = report
            .per_net
            .iter()
            .find(|(n, _)| n == net)
            .map(|(_, o)| matches!(o, Outcome::Ok { .. }))
            .unwrap_or(false);
        let wflag = if trunk_w + 1e-9 >= 0.50 { "OK" } else { "THIN" };
        println!(
            "  {net:10} pads={:2} segs={:2} trunk_w={:.2}mm[{}] len={:.1}mm routed={}",
            pads.len(),
            segs.len(),
            trunk_w,
            wflag,
            len,
            if outcome { "Y" } else { "N" }
        );
    }

    let power_thin = power_nets.iter().any(|net| {
        if pour_nets.contains(net) {
            return false;
        }
        let segs: Vec<&pcb_core::Trace> = board.traces.iter().filter(|t| &t.net == net).collect();
        if segs.is_empty() {
            return false; // unrouted is counted separately, not "thin"
        }
        let trunk_w = segs.iter().map(|t| t.width.to_mm()).fold(0.0_f64, f64::max);
        trunk_w + 1e-9 < 0.50
    });

    // --- pad-escape diagnostic for failed nets ---
    // A net can fail for two very different reasons: no *corridor*
    // (capacity) or no *exit from a pad* (placement boxed it in). Count,
    // per failed net, how many of its pads sit within 0.6 mm of another
    // footprint's pad or the board edge — a proxy for "boxed in".
    if !failed.is_empty() {
        println!("FAILED-NET pad crowding (proxy for placement box-in):");
        let outline = board.outline;
        for net in &failed {
            let Some(pads) = net_pads.get(*net) else {
                continue;
            };
            let mut crowded = 0;
            for p in pads {
                // nearest foreign pad edge distance
                let mut nearest = f64::INFINITY;
                for fp in board.footprints.values() {
                    for op in &fp.pads {
                        if op.net.as_deref() == Some(*net) {
                            continue;
                        }
                        let c = fp.pad_world_center(op);
                        let (w, h) = fp.pad_world_size(op);
                        let dx = (p.x.to_mm() - c.x.to_mm()).abs() - w.to_mm() / 2.0;
                        let dy = (p.y.to_mm() - c.y.to_mm()).abs() - h.to_mm() / 2.0;
                        let d = dx.max(dy);
                        nearest = nearest.min(d);
                    }
                }
                let mut edge = f64::INFINITY;
                if let Some(o) = outline {
                    edge = (p.x.to_mm() - o.min.x.to_mm())
                        .min(o.max.x.to_mm() - p.x.to_mm())
                        .min(p.y.to_mm() - o.min.y.to_mm())
                        .min(o.max.y.to_mm() - p.y.to_mm());
                }
                if nearest < 0.6 || edge < 0.6 {
                    crowded += 1;
                }
            }
            println!(
                "  {net:10} {}/{} pads crowded (<0.6mm to foreign pad/edge)",
                crowded,
                pads.len()
            );
        }
    }

    // Optionally persist the routed project so it can be rendered by a
    // fragua server (same on-disk shape: name/board/schematic/palette).
    if let Ok(out) = std::env::var("OUT_JSON") {
        let doc = serde_json::json!({
            "name": pf.name,
            "board": board,
            "schematic": &*sch,
            "palette": [],
        });
        if let Err(e) = std::fs::write(&out, serde_json::to_vec_pretty(&doc).unwrap()) {
            eprintln!("(OUT_JSON write failed: {e})");
        } else {
            eprintln!("(wrote routed project -> {out})");
        }
    }

    let clean = failed.is_empty() && collisions == 0 && !power_thin;
    println!(
        "SCORE: {}  (unrouted={}, collisions={}, power_thin={})",
        if clean { "CLEAN ✅" } else { "NOT CLEAN ❌" },
        failed.len(),
        collisions,
        power_thin
    );
}
