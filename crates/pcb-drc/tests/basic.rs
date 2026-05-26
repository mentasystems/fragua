//! Smoke tests for the DRC checks.

use std::collections::HashMap;

use pcb_core::{Board, CopperLayer, Footprint, Id, Length, Pad, PlacementMargin, Point, Rect, Trace};
use pcb_drc::{run, DrcOptions, Severity, ViolationKind};

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

fn fp(reference: &str, x_mm: f64, y_mm: f64, pads: Vec<Pad>) -> Footprint {
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
fn pad_pad_clearance_violation() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(50.0), Length::from_mm(20.0)),
    ));
    // Two pads, 0.05 mm apart at the edges → way under 0.2 mm clearance.
    board.add_footprint(fp("R1", 10.0, 10.0, vec![pad("1", 0.0, 0.0, Some("A"))]));
    board.add_footprint(fp("R2", 11.05, 10.0, vec![pad("1", 0.0, 0.0, Some("B"))]));
    let report = run(&board, &DrcOptions::default());
    assert!(report
        .violations
        .iter()
        .any(|v| v.kind == ViolationKind::PadPadClearance));
}

#[test]
fn unconnected_pad_is_flagged() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(50.0), Length::from_mm(20.0)),
    ));
    board.add_footprint(fp("R1", 10.0, 10.0, vec![pad("1", 0.0, 0.0, Some("VCC"))]));
    let report = run(&board, &DrcOptions::default());
    assert!(report
        .violations
        .iter()
        .any(|v| v.kind == ViolationKind::UnconnectedPad));
}

#[test]
fn trace_touching_pad_marks_pad_as_connected() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(50.0), Length::from_mm(20.0)),
    ));
    board.add_footprint(fp("R1", 10.0, 10.0, vec![pad("1", 0.0, 0.0, Some("VCC"))]));
    board.add_trace(Trace {
        id: Id::new(),
        layer: CopperLayer::Top,
        start: Point::new(Length::from_mm(10.0), Length::from_mm(10.0)),
        end: Point::new(Length::from_mm(20.0), Length::from_mm(10.0)),
        width: Length::from_mm(0.25),
        net: "VCC".into(),
    });
    let report = run(&board, &DrcOptions::default());
    assert!(!report
        .violations
        .iter()
        .any(|v| v.kind == ViolationKind::UnconnectedPad));
}

#[test]
fn routing_inefficient_fires_when_actual_far_exceeds_hpwl() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(50.0), Length::from_mm(50.0)),
    ));
    // Two pads on net "S" 10 mm apart on the X axis. HPWL = 10 mm.
    board.add_footprint(fp("R1", 5.0, 25.0, vec![pad("1", 0.0, 0.0, Some("S"))]));
    board.add_footprint(fp("R2", 15.0, 25.0, vec![pad("1", 0.0, 0.0, Some("S"))]));
    // Snake the trace so the actual length is ~30 mm — 3× HPWL, well
    // above the default 1.5× threshold.
    let seg = |x1, y1, x2, y2| Trace {
        id: Id::new(),
        layer: CopperLayer::Top,
        start: Point::new(Length::from_mm(x1), Length::from_mm(y1)),
        end: Point::new(Length::from_mm(x2), Length::from_mm(y2)),
        width: Length::from_mm(0.25),
        net: "S".into(),
    };
    board.add_trace(seg(5.0, 25.0, 5.0, 35.0));
    board.add_trace(seg(5.0, 35.0, 15.0, 35.0));
    board.add_trace(seg(15.0, 35.0, 15.0, 25.0));
    let report = run(&board, &DrcOptions::default());
    assert!(report
        .violations
        .iter()
        .any(|v| v.kind == ViolationKind::RoutingInefficient));
}

#[test]
fn routing_inefficient_silent_when_close_to_hpwl() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(50.0), Length::from_mm(50.0)),
    ));
    board.add_footprint(fp("R1", 5.0, 25.0, vec![pad("1", 0.0, 0.0, Some("S"))]));
    board.add_footprint(fp("R2", 15.0, 25.0, vec![pad("1", 0.0, 0.0, Some("S"))]));
    // Direct trace; length ≈ HPWL.
    board.add_trace(Trace {
        id: Id::new(),
        layer: CopperLayer::Top,
        start: Point::new(Length::from_mm(5.0), Length::from_mm(25.0)),
        end: Point::new(Length::from_mm(15.0), Length::from_mm(25.0)),
        width: Length::from_mm(0.25),
        net: "S".into(),
    });
    let report = run(&board, &DrcOptions::default());
    assert!(!report
        .violations
        .iter()
        .any(|v| v.kind == ViolationKind::RoutingInefficient));
}

#[test]
fn body_off_board_is_error_even_for_edge_mounted() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(20.0), Length::from_mm(20.0)),
    ));
    // Edge-mounted connector flush against the right edge; the pads
    // fit on the board, but the placement margin pushes the body 3 mm
    // past the right outline.
    let mut connector = fp("J1", 18.75, 10.0, vec![pad("1", 0.0, 0.0, Some("D+"))]);
    connector.key = "usb_c".into();
    connector.edge_mounted = true;
    board.add_footprint(connector);

    let mut margins = HashMap::new();
    margins.insert(
        "usb_c".to_string(),
        PlacementMargin {
            top_mm: 0.0,
            right_mm: 3.0,
            bottom_mm: 0.0,
            left_mm: 0.0,
        },
    );
    let opts = DrcOptions {
        placement_margins: margins,
        ..DrcOptions::default()
    };
    let report = run(&board, &opts);
    let off_board: Vec<_> = report
        .violations
        .iter()
        .filter(|v| v.kind == ViolationKind::BodyOffBoard)
        .collect();
    assert_eq!(
        off_board.len(),
        1,
        "expected exactly one BodyOffBoard violation, got: {:#?}",
        report.violations
    );
    assert_eq!(
        off_board[0].severity,
        Severity::Error,
        "BodyOffBoard must be a hard error, not a warning"
    );
}

#[test]
fn edge_clearance_violation() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(20.0), Length::from_mm(20.0)),
    ));
    // Pad sitting flush on the left edge.
    board.add_footprint(fp("R1", 0.5, 10.0, vec![pad("1", 0.0, 0.0, None)]));
    let report = run(&board, &DrcOptions::default());
    assert!(report
        .violations
        .iter()
        .any(|v| v.kind == ViolationKind::EdgeClearance));
}

#[test]
fn keepout_visible_in_drc_report() {
    use pcb_core::Trace;
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(20.0)),
    ));
    // Keepout in the centre.
    board.keepouts.push(pcb_core::Keepout {
        id: pcb_core::Id::new(),
        polygon: vec![
            Point::new(Length::from_mm(10.0), Length::from_mm(5.0)),
            Point::new(Length::from_mm(30.0), Length::from_mm(5.0)),
            Point::new(Length::from_mm(30.0), Length::from_mm(15.0)),
            Point::new(Length::from_mm(10.0), Length::from_mm(15.0)),
        ],
        layers: vec![],
        nets_allowed: vec![],
        label: "test".into(),
    });
    // A trace running right through the keepout.
    board.traces.push(Trace {
        id: pcb_core::Id::new(),
        layer: pcb_core::CopperLayer::Top,
        start: Point::new(Length::from_mm(5.0), Length::from_mm(10.0)),
        end: Point::new(Length::from_mm(35.0), Length::from_mm(10.0)),
        width: Length::from_mm(0.25),
        net: "FOO".into(),
    });
    let report = run(&board, &DrcOptions::default());
    let kp_violations: Vec<_> = report
        .violations
        .iter()
        .filter(|v| v.kind == ViolationKind::KeepoutViolation)
        .collect();
    assert!(
        !kp_violations.is_empty(),
        "expected at least one KeepoutViolation, got: {:#?}",
        report.violations,
    );
}
