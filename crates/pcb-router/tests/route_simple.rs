//! Smoke test: place three pads on two nets and verify the router lays
//! traces between them.

use pcb_core::{Board, CopperLayer, Footprint, Id, Length, Pad, Point, Rect};
use pcb_router::{route, Outcome, RouteOptions};

fn pad(num: &str, off_x: f64, off_y: f64, net: Option<&str>) -> Pad {
    Pad {
        number: num.into(),
        name: String::new(),
        offset: Point::new(Length::from_mm(off_x), Length::from_mm(off_y)),
        size: (Length::from_mm(1.0), Length::from_mm(1.2)),
        layer: CopperLayer::Top,
        net: net.map(str::to_string),
        drill: None,
    }
}

fn footprint(reference: &str, x_mm: f64, y_mm: f64, pads: Vec<Pad>) -> Footprint {
    Footprint {
        id: Id::new(),
        reference: reference.into(),
        value: String::new(),
        library: "demo".into(),
        position: Point::new(Length::from_mm(x_mm), Length::from_mm(y_mm)),
        rotation: 0.0,
        layer: CopperLayer::Top,
        pads,
        key: String::new(),
        description: String::new(),
        edge_mounted: false,
        silk: Vec::new(),
    }
}

#[test]
fn routes_two_two_pin_resistors_sharing_a_net() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(20.0)),
    ));
    board.add_footprint(footprint(
        "R1",
        10.0,
        10.0,
        vec![
            pad("1", -1.0, 0.0, Some("VCC")),
            pad("2", 1.0, 0.0, Some("OUT")),
        ],
    ));
    board.add_footprint(footprint(
        "R2",
        20.0,
        10.0,
        vec![
            pad("1", -1.0, 0.0, Some("OUT")),
            pad("2", 1.0, 0.0, Some("GND")),
        ],
    ));

    let report = route(&mut board, &RouteOptions::default());
    let outcomes: Vec<&Outcome> = report.per_net.iter().map(|(_, o)| o).collect();
    // OUT has two pads; VCC and GND have one each (skipped as no-op).
    assert_eq!(report.per_net.len(), 3);
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, Outcome::Ok { trace_segments, .. } if *trace_segments >= 1)),
        "expected at least one Ok with traces, got {:?}",
        report.per_net
    );
    assert!(report.trace_count >= 1, "report = {report:?}");
    assert!(!board.traces.is_empty());

    // Negotiated congestion can never make a previously-best report
    // worse — the driver keeps the best across iterations. So even on a
    // trivially routable case, RR&R must not "rescue" itself by adding
    // failures: zero failed nets out, period.
    for (name, outcome) in &report.per_net {
        assert!(
            !matches!(outcome, Outcome::Failed { .. }),
            "trivial board should not have failed nets, but {name} did: {outcome:?}",
        );
    }

    // The router should report length metrics on every successfully
    // routed net, and at least one iteration must have run.
    assert!(report.iterations >= 1);
    assert!(report.total_length_mm > 0.0);
    assert!(report.total_lower_bound_mm > 0.0);
    for (name, outcome) in &report.per_net {
        if let Outcome::Ok {
            length_mm,
            lower_bound_mm,
            trace_segments,
            ..
        } = outcome
        {
            if *trace_segments > 0 {
                assert!(
                    *length_mm > 0.0 && *lower_bound_mm > 0.0,
                    "net {name} routed but no length metrics: {outcome:?}",
                );
                // Two-pad nets are routed star-style (1 spoke), so the
                // actual wire is at most a small constant factor above
                // the Manhattan lower bound. Use 1.5× as a safety net
                // against future regressions in `lay_path`.
                assert!(
                    *length_mm <= *lower_bound_mm * 1.5 + 1.0,
                    "net {name}: length {length_mm:.2} > 1.5× lower bound {lower_bound_mm:.2}",
                );
            }
        }
    }
}

#[test]
fn net_override_widens_the_traces_it_lays() {
    // Two pads on net "POWER" 10 mm apart on a fresh board; route
    // with a per-net override that doubles the trace width and verify
    // every emitted trace honours it.
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(20.0)),
    ));
    board.add_footprint(footprint(
        "R1",
        10.0,
        10.0,
        vec![pad("1", 0.0, 0.0, Some("POWER"))],
    ));
    board.add_footprint(footprint(
        "R2",
        20.0,
        10.0,
        vec![pad("1", 0.0, 0.0, Some("POWER"))],
    ));

    let mut opts = pcb_router::RouteOptions::default();
    opts.net_overrides.insert(
        "POWER".into(),
        pcb_router::NetOverride {
            trace_width: Some(Length::from_mm(0.50)),
            clearance: None,
        },
    );
    let _report = pcb_router::route(&mut board, &opts);
    assert!(!board.traces.is_empty(), "expected at least one trace");
    for t in &board.traces {
        assert!(
            (t.width.to_mm() - 0.50).abs() < 1e-6,
            "trace on net `{}` should be 0.50 mm wide, got {} mm",
            t.net,
            t.width.to_mm(),
        );
    }
}

/// Three pads colinear on net N. The optimal Steiner / Prim tree is
/// 2 segments (left-pad → middle-pad → right-pad) of length equal to
/// HPWL (the bounding-box half-perimeter). Star routing through any
/// hub would lay 3 segments and ~1.5× HPWL of wire. This test pins
/// the multi-source A* behaviour: the third spoke must branch off the
/// existing trace, not run all the way back to the seed pad.
#[test]
fn three_colinear_pads_share_trunk_via_steiner() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(20.0)),
    ));
    board.add_footprint(footprint(
        "R1",
        5.0,
        10.0,
        vec![pad("1", 0.0, 0.0, Some("N"))],
    ));
    board.add_footprint(footprint(
        "R2",
        15.0,
        10.0,
        vec![pad("1", 0.0, 0.0, Some("N"))],
    ));
    board.add_footprint(footprint(
        "R3",
        25.0,
        10.0,
        vec![pad("1", 0.0, 0.0, Some("N"))],
    ));

    let report = route(&mut board, &RouteOptions::default());
    let net = report
        .per_net
        .iter()
        .find(|(n, _)| n == "N")
        .expect("net N missing");
    let Outcome::Ok {
        length_mm,
        lower_bound_mm,
        ..
    } = &net.1
    else {
        panic!("net N should route, got {:?}", net.1);
    };
    // HPWL = 20 mm. Optimal tree = 20 mm. A star would be ~30 mm. We
    // require <= 1.3× HPWL so a future regression that breaks Steiner
    // (e.g. mistakenly seeding only the seed pad as a source) gets
    // caught — star routing on this layout would fail the bound.
    let detour = length_mm / lower_bound_mm;
    assert!(
        detour <= 1.3,
        "expected near-optimal Steiner on colinear pads, got detour {detour:.2}×",
    );
}
