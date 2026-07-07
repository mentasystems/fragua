//! Length-matching tests.

use pcb_core::{Board, CopperLayer, Id, Length, NetClass, Point, Rect, Schematic, Trace};
use pcb_router::length_match_pass;

fn trace(start: (f64, f64), end: (f64, f64), net: &str) -> Trace {
    Trace {
        id: Id::new(),
        layer: CopperLayer::Top,
        start: Point::new(Length::from_mm(start.0), Length::from_mm(start.1)),
        end: Point::new(Length::from_mm(end.0), Length::from_mm(end.1)),
        width: Length::from_mm(0.25),
        net: net.into(),
    }
}

fn net_length(board: &Board, net: &str) -> f64 {
    board
        .traces
        .iter()
        .filter(|t| t.net == net)
        .map(|t| {
            let dx = t.end.x.to_mm() - t.start.x.to_mm();
            let dy = t.end.y.to_mm() - t.start.y.to_mm();
            (dx * dx + dy * dy).sqrt()
        })
        .sum()
}

#[test]
fn length_match_within_tolerance_when_target_set() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(20.0)),
    ));
    // A is 20 mm; B is 25 mm. Target 30 mm — both need extension.
    board.add_trace(trace((0.0, 5.0), (20.0, 5.0), "A"));
    board.add_trace(trace((0.0, 10.0), (25.0, 10.0), "B"));
    let mut sch = Schematic::new();
    sch.set_net_class(NetClass {
        name: "lm".into(),
        trace_width_mm: Some(0.25),
        target_length_mm: Some(30.0),
        length_tolerance_mm: 0.5,
        ..NetClass::default()
    });
    sch.assign_net_to_class("A", "lm");
    sch.assign_net_to_class("B", "lm");
    let _ = length_match_pass(&mut board, &sch);
    let la = net_length(&board, "A");
    let lb = net_length(&board, "B");
    assert!(
        (la - 30.0).abs() <= 0.5,
        "A length {} not within 0.5 of 30",
        la
    );
    assert!(
        (lb - 30.0).abs() <= 0.5,
        "B length {} not within 0.5 of 30",
        lb
    );
}

#[test]
fn diff_pair_length_match_pulls_shorter_to_longer() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(20.0)),
    ));
    // DP=20 mm, DM=25 mm. DP should grow by ~5 mm (serpentine needs
    // a long enough base segment to absorb the extra path).
    board.add_trace(trace((0.0, 5.0), (20.0, 5.0), "USB_DP"));
    board.add_trace(trace((0.0, 10.0), (25.0, 10.0), "USB_DM"));
    let mut sch = Schematic::new();
    sch.set_net_class(NetClass {
        name: "diff_p".into(),
        trace_width_mm: Some(0.25),
        diff_pair_with: Some("USB_DM".into()),
        ..NetClass::default()
    });
    sch.set_net_class(NetClass {
        name: "diff_n".into(),
        trace_width_mm: Some(0.25),
        diff_pair_with: Some("USB_DP".into()),
        ..NetClass::default()
    });
    sch.assign_net_to_class("USB_DP", "diff_p");
    sch.assign_net_to_class("USB_DM", "diff_n");
    let _ = length_match_pass(&mut board, &sch);
    let dp = net_length(&board, "USB_DP");
    let dm = net_length(&board, "USB_DM");
    // DP was shorter; expected to grow to ~25 mm.
    assert!(
        (dp - 25.0).abs() <= 0.5,
        "USB_DP length {} should be ~25 mm",
        dp
    );
    // DM was already the longest in the pair, should be unchanged.
    assert!(
        (dm - 25.0).abs() <= 1e-6,
        "USB_DM length {} should stay 25 mm",
        dm
    );
}
