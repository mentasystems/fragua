//! Smoke tests: SA placer must reduce HPWL on a contrived bad layout.

use pcb_core::{Board, CopperLayer, Footprint, Id, Length, Pad, Point, Rect};
use pcb_placer::{place, PlaceOptions};

fn pad(num: &str, off_x: f64, off_y: f64, net: Option<&str>) -> Pad {
    Pad {
        number: num.into(),
        name: String::new(),
        offset: Point::new(Length::from_mm(off_x), Length::from_mm(off_y)),
        size: (Length::from_mm(1.0), Length::from_mm(1.2)),
        layer: CopperLayer::Top,
        net: net.map(str::to_string),
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
fn placer_reduces_hpwl_on_two_far_apart_resistors() {
    // 50×30 mm board with two resistors on the same net, placed at
    // diagonally-opposite corners. SA should be able to bring them
    // close together — same net, no other constraints.
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(50.0), Length::from_mm(30.0)),
    ));
    // Pad at offset (-1, 0) and (+1, 0). HPWL of the shared net is
    // basically the centre-to-centre Manhattan distance, minus 2 mm.
    let mk = |reference: &str, x, y| {
        footprint(
            reference,
            x,
            y,
            vec![
                pad("1", -1.0, 0.0, Some("S")),
                pad("2", 1.0, 0.0, Some("OUT")),
            ],
        )
    };
    board.add_footprint(mk("R1", 5.0, 5.0));
    board.add_footprint(mk("R2", 45.0, 25.0));

    let opts = PlaceOptions { seed: 42, ..PlaceOptions::default() };
    let report = place(
        &mut board,
        &["R1".to_string(), "R2".to_string()],
        &opts,
    )
    .expect("placer should succeed");
    assert!(
        report.final_hpwl_mm < report.initial_hpwl_mm,
        "expected HPWL to drop, got {:.2} → {:.2}",
        report.initial_hpwl_mm,
        report.final_hpwl_mm,
    );
    // With only one floating net and a clear board, SA usually gets to
    // within a few mm of zero.
    assert!(
        report.final_hpwl_mm < 10.0,
        "SA didn't converge: HPWL {:.2} mm > 10 mm",
        report.final_hpwl_mm,
    );
    // R1 and R2 are both on the same net so both should have moved.
    assert!(
        report.moved.contains(&"R1".to_string())
            || report.moved.contains(&"R2".to_string()),
        "expected at least one of R1/R2 to move, got {:?}",
        report.moved,
    );
}

#[test]
fn pinned_footprints_do_not_move() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(50.0), Length::from_mm(30.0)),
    ));
    let mk_pos = |reference, x, y| {
        footprint(
            reference,
            x,
            y,
            vec![
                pad("1", -1.0, 0.0, Some("S")),
                pad("2", 1.0, 0.0, Some("OUT")),
            ],
        )
    };
    board.add_footprint(mk_pos("R1", 5.0, 5.0));
    board.add_footprint(mk_pos("R2", 45.0, 25.0));

    let r1_before = board
        .footprints_in_order()
        .find(|fp| fp.reference == "R1")
        .map(|fp| fp.position)
        .unwrap();

    // Only R2 is movable; R1 must stay put.
    let opts = PlaceOptions { seed: 7, max_iterations: 2000, ..PlaceOptions::default() };
    let _report = place(&mut board, &["R2".to_string()], &opts).unwrap();

    let r1_after = board
        .footprints_in_order()
        .find(|fp| fp.reference == "R1")
        .map(|fp| fp.position)
        .unwrap();
    assert_eq!(r1_before.x.0, r1_after.x.0);
    assert_eq!(r1_before.y.0, r1_after.y.0);
}
