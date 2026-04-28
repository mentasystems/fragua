//! Smoke tests for the DRC checks.

use pcb_core::{Board, CopperLayer, Footprint, Id, Length, Pad, Point, Rect, Trace};
use pcb_drc::{run, DrcOptions, ViolationKind};

fn pad(num: &str, off_x: f64, off_y: f64, net: Option<&str>) -> Pad {
    Pad {
        number: num.into(),
        offset: Point::new(Length::from_mm(off_x), Length::from_mm(off_y)),
        size: (Length::from_mm(1.0), Length::from_mm(1.2)),
        layer: CopperLayer::Top,
        net: net.map(str::to_string),
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
    board.add_footprint(fp(
        "R1",
        10.0,
        10.0,
        vec![pad("1", 0.0, 0.0, Some("VCC"))],
    ));
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
    board.add_footprint(fp(
        "R1",
        10.0,
        10.0,
        vec![pad("1", 0.0, 0.0, Some("VCC"))],
    ));
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
