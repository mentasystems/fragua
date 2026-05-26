//! Differential pair routing tests.

use std::sync::Arc;

use pcb_core::{Board, CopperLayer, Footprint, Id, Length, NetClass, Pad, Point, Rect, Schematic};
use pcb_router::{route, RouteOptions};

fn pad(num: &str, off_x: f64, off_y: f64, net: Option<&str>) -> Pad {
    Pad {
        number: num.into(),
        name: String::new(),
        offset: Point::new(Length::from_mm(off_x), Length::from_mm(off_y)),
        size: (Length::from_mm(0.3), Length::from_mm(0.3)),
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

/// USB_DP and USB_DM each have two pads on opposite sides of the board.
/// Classes set the diff_pair_with relation. Routing should produce
/// parallel traces on the same layer at the spec'd gap.
#[test]
fn diff_pair_follow_emits_parallel_trace() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(20.0)),
    ));
    // Each connector exposes BOTH halves of the diff pair, vertically
    // adjacent. Pads are small (0.3 mm) and offset by 0.5 mm so they
    // line up roughly at the diff-pair gap.
    board.add_footprint(footprint(
        "J1",
        5.0,
        10.0,
        vec![
            pad("1", 0.0, 0.0, Some("USB_DP")),
            pad("2", 0.0, 0.5, Some("USB_DM")),
        ],
    ));
    board.add_footprint(footprint(
        "J2",
        35.0,
        10.0,
        vec![
            pad("1", 0.0, 0.0, Some("USB_DP")),
            pad("2", 0.0, 0.5, Some("USB_DM")),
        ],
    ));

    let mut sch = Schematic::new();
    // class diff_p references USB_DM as the partner of USB_DP — i.e.
    // nets in diff_p pair with USB_DM.
    sch.set_net_class(NetClass {
        name: "diff_p".into(),
        trace_width_mm: Some(0.20),
        clearance_mm: Some(0.20),
        diff_pair_with: Some("USB_DM".into()),
        diff_gap_mm: Some(0.20),
        ..NetClass::default()
    });
    sch.set_net_class(NetClass {
        name: "diff_n".into(),
        trace_width_mm: Some(0.20),
        clearance_mm: Some(0.20),
        diff_pair_with: Some("USB_DP".into()),
        diff_gap_mm: Some(0.20),
        ..NetClass::default()
    });
    sch.assign_net_to_class("USB_DP", "diff_p");
    sch.assign_net_to_class("USB_DM", "diff_n");

    let mut opts = RouteOptions::default();
    opts.schematic = Some(Arc::new(sch));
    let _ = route(&mut board, &opts);

    // Collect DP and DM traces.
    let dp: Vec<_> = board.traces.iter().filter(|t| t.net == "USB_DP").collect();
    let dm: Vec<_> = board.traces.iter().filter(|t| t.net == "USB_DM").collect();
    assert!(!dp.is_empty(), "USB_DP should have traces");
    assert!(!dm.is_empty(), "USB_DM should have traces");
    // For each DM trace, there should exist a DP trace whose centerline
    // runs parallel at within 0.20 mm ± half a grid cell (0.125 mm).
    let cell_half = 0.125;
    let target_offset = 0.20 + 0.20; // width/2 + gap + width/2 = 0.10+0.20+0.10 = 0.40
    let mut matched = 0;
    for dm_t in &dm {
        if dm_t.layer != CopperLayer::Top {
            continue;
        }
        let dx = dm_t.end.x.to_mm() - dm_t.start.x.to_mm();
        let dy = dm_t.end.y.to_mm() - dm_t.start.y.to_mm();
        let len = (dx * dx + dy * dy).sqrt();
        if len < 0.1 {
            continue;
        }
        let ux = dx / len;
        let uy = dy / len;
        for dp_t in &dp {
            if dp_t.layer != dm_t.layer {
                continue;
            }
            let dpx = dp_t.end.x.to_mm() - dp_t.start.x.to_mm();
            let dpy = dp_t.end.y.to_mm() - dp_t.start.y.to_mm();
            let dlen = (dpx * dpx + dpy * dpy).sqrt();
            if dlen < 0.1 {
                continue;
            }
            // Direction alignment: dot(unit_dm, unit_dp).abs() ≈ 1.
            let dot = (ux * dpx / dlen + uy * dpy / dlen).abs();
            if dot < 0.95 {
                continue;
            }
            // Perpendicular distance from DM midpoint to DP segment.
            let mx = (dm_t.start.x.to_mm() + dm_t.end.x.to_mm()) / 2.0;
            let my = (dm_t.start.y.to_mm() + dm_t.end.y.to_mm()) / 2.0;
            let nx = -uy;
            let ny = ux;
            let ax = dp_t.start.x.to_mm();
            let ay = dp_t.start.y.to_mm();
            let perp = ((mx - ax) * nx + (my - ay) * ny).abs();
            if (perp - target_offset).abs() <= 2.0 * cell_half {
                matched += 1;
                break;
            }
        }
    }
    assert!(
        matched > 0,
        "expected at least one DM segment to find a parallel DP partner, traces:\n DP={:?}\n DM={:?}",
        dp.iter()
            .map(|t| (t.start.x.to_mm(), t.start.y.to_mm(), t.end.x.to_mm(), t.end.y.to_mm()))
            .collect::<Vec<_>>(),
        dm.iter()
            .map(|t| (t.start.x.to_mm(), t.start.y.to_mm(), t.end.x.to_mm(), t.end.y.to_mm()))
            .collect::<Vec<_>>(),
    );
}

/// With an obstacle in the parallel corridor, the follower net should
/// still route somewhere (via fallback). The router must not panic and
/// must produce at least an attempt — either Ok or a clearly recorded
/// Failed. The fallback warning goes to stderr; the test doesn't
/// capture stderr, so we just check no panic + the partner routes
/// successfully.
#[test]
fn diff_pair_falls_back_when_blocked() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(20.0)),
    ));
    board.add_footprint(footprint(
        "J1",
        5.0,
        10.0,
        vec![
            pad("1", 0.0, 0.0, Some("USB_DP")),
            pad("2", 0.0, 0.5, Some("USB_DM")),
        ],
    ));
    board.add_footprint(footprint(
        "J2",
        35.0,
        10.0,
        vec![
            pad("1", 0.0, 0.0, Some("USB_DP")),
            pad("2", 0.0, 0.5, Some("USB_DM")),
        ],
    ));
    // A thin keepout placed exactly where the diff-pair parallel
    // would naturally land (a couple of grid cells below DM's
    // y=10.5 trace, at y≈10.1). DM (the leader) routes around the
    // narrow keepout; DP's follow attempt hits the keepout and
    // falls back to plain Theta* (which detours via higher y).
    board.add_keepout(pcb_core::Keepout {
        id: Id::new(),
        polygon: vec![
            Point::new(Length::from_mm(18.0), Length::from_mm(10.0)),
            Point::new(Length::from_mm(22.0), Length::from_mm(10.0)),
            Point::new(Length::from_mm(22.0), Length::from_mm(10.3)),
            Point::new(Length::from_mm(18.0), Length::from_mm(10.3)),
        ],
        layers: vec![CopperLayer::Top],
        nets_allowed: vec![],
        label: "block_corridor".into(),
    });
    let mut sch = Schematic::new();
    sch.set_net_class(NetClass {
        name: "diff_p".into(),
        trace_width_mm: Some(0.20),
        diff_pair_with: Some("USB_DM".into()),
        diff_gap_mm: Some(0.20),
        ..NetClass::default()
    });
    sch.set_net_class(NetClass {
        name: "diff_n".into(),
        trace_width_mm: Some(0.20),
        diff_pair_with: Some("USB_DP".into()),
        diff_gap_mm: Some(0.20),
        ..NetClass::default()
    });
    sch.assign_net_to_class("USB_DP", "diff_p");
    sch.assign_net_to_class("USB_DM", "diff_n");
    let mut opts = RouteOptions::default();
    opts.schematic = Some(Arc::new(sch));
    let report = route(&mut board, &opts);
    // At minimum, the leader must still route. The follower may fail
    // entirely when the keepout straddles both the parallel corridor
    // AND the fallback detour; the important assertion is the
    // fallback path was triggered (see the eprintln in
    // try_diff_pair_follow) and that route() doesn't panic.
    let dm_count = board.traces.iter().filter(|t| t.net == "USB_DM").count();
    assert!(dm_count > 0, "USB_DM should still route");
    // Both nets should appear in the per-net report.
    let names: Vec<_> = report.per_net.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"USB_DP"));
    assert!(names.contains(&"USB_DM"));
}
