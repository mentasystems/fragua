//! Smoke tests: SA placer must reduce HPWL on a contrived bad layout.

use pcb_core::{Board, CopperLayer, Footprint, Id, Length, Pad, Point, Rect};
use pcb_placer::{min_pairwise_gap, place, MarginMap, PlaceOptions};

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

    let opts = PlaceOptions {
        seed: 42,
        ..PlaceOptions::default()
    };
    let report = place(
        &mut board,
        &["R1".to_string(), "R2".to_string()],
        &opts,
        &MarginMap::new(),
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
        report.moved.contains(&"R1".to_string()) || report.moved.contains(&"R2".to_string()),
        "expected at least one of R1/R2 to move, got {:?}",
        report.moved,
    );
}

/// The solder-access hard floor: two parts on the same net want to pack
/// as tight as HPWL allows. With the soft gap preference OFF, only the
/// hard clearance governs their spacing — so the finished layout must
/// leave >= `solder_gap_mm` between bodies (default 1.0 mm) so the user
/// can get a soldering iron between them, and `solder_gap=0` must degrade
/// to the old 0.5 mm `min_clearance` floor (packing tighter).
#[test]
fn solder_gap_is_a_hard_floor_by_default() {
    // Same-net pair at opposite corners of a narrow board → HPWL strongly
    // rewards bringing them together, exercising the hard floor.
    let gap_after_place = |solder_gap_mm: f64| -> f64 {
        let mut board = Board::new();
        board.outline = Some(Rect::from_corners(
            Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
            Point::new(Length::from_mm(40.0), Length::from_mm(12.0)),
        ));
        let mk = |reference: &str, x, y| {
            footprint(
                reference,
                x,
                y,
                vec![
                    pad("1", -1.0, 0.0, Some("S")),
                    pad("2", 1.0, 0.0, Some("S")),
                ],
            )
        };
        board.add_footprint(mk("R1", 4.0, 6.0));
        board.add_footprint(mk("R2", 36.0, 6.0));
        let opts = PlaceOptions {
            seed: 12345,
            max_iterations: 10000,
            // Turn the soft preference off so ONLY the hard floor governs.
            min_gap_mm: 0.0,
            gap_penalty_factor: 0.0,
            solder_gap_mm,
            ..PlaceOptions::default()
        };
        place(
            &mut board,
            &["R1".to_string(), "R2".to_string()],
            &opts,
            &MarginMap::new(),
        )
        .expect("placer should succeed");
        min_pairwise_gap(&board, &MarginMap::new())
    };

    // Default 1.0 mm floor: never violated, no matter how much HPWL wants
    // the parts touching.
    let default_gap = gap_after_place(1.0);
    assert!(
        default_gap >= 1.0 - 0.02,
        "default solder-access floor violated: min pairwise gap {default_gap:.3} mm < 1.0",
    );

    // solder_gap=0 degrades to the old behaviour: the 0.5 mm min_clearance
    // is the only hard floor, so SA packs tighter than the 1.0 mm default.
    let old_gap = gap_after_place(0.0);
    assert!(
        old_gap >= 0.5 - 0.02,
        "min_clearance floor violated: min pairwise gap {old_gap:.3} mm < 0.5",
    );
    assert!(
        old_gap < 1.0,
        "solder_gap=0 should reproduce sub-1mm packing, got {old_gap:.3} mm",
    );
    assert!(
        old_gap < default_gap + 1e-9,
        "solder_gap=0 ({old_gap:.3}) should pack at least as tight as default ({default_gap:.3})",
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
    let opts = PlaceOptions {
        seed: 7,
        max_iterations: 2000,
        ..PlaceOptions::default()
    };
    let _report = place(&mut board, &["R2".to_string()], &opts, &MarginMap::new()).unwrap();

    let r1_after = board
        .footprints_in_order()
        .find(|fp| fp.reference == "R1")
        .map(|fp| fp.position)
        .unwrap();
    assert_eq!(r1_before.x.0, r1_after.x.0);
    assert_eq!(r1_before.y.0, r1_after.y.0);
}
