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
    // within a few mm of zero. HPWL is *weighted* (2-pin nets ×4), so
    // a 2 mm residual is reported as ~8 weighted-mm — allow headroom.
    assert!(
        report.final_hpwl_mm < 40.0,
        "SA didn't converge: weighted HPWL {:.2} mm > 40",
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

/// A 2-pin series resistor between two distant chips must be pulled
/// toward the segment joining them. Regression for door-board R3
/// (SSR LED series) freezing far from both ends of its net.
#[test]
fn series_resistor_pulled_toward_its_nets() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(100.0), Length::from_mm(40.0)),
    ));

    // U1 left drives DRV; U3 right sinks LED; R3 series between them.
    board.add_footprint(footprint(
        "U1",
        10.0,
        20.0,
        vec![
            pad("1", -2.0, 0.0, Some("GND")),
            pad("2", 2.0, 0.0, Some("DRV")),
        ],
    ));
    board.add_footprint(footprint(
        "U3",
        90.0,
        20.0,
        vec![
            pad("1", -2.0, 0.0, Some("LED")),
            pad("2", 2.0, 0.0, Some("GND")),
        ],
    ));
    // Stranded at the top, far from the U1–U3 line at y=20.
    board.add_footprint(footprint(
        "R3",
        50.0,
        35.0,
        vec![
            pad("1", -1.6, 0.0, Some("DRV")),
            pad("2", 1.6, 0.0, Some("LED")),
        ],
    ));

    let r3_before_y = board
        .footprints_in_order()
        .find(|f| f.reference == "R3")
        .unwrap()
        .position
        .y
        .to_mm();

    let opts = PlaceOptions {
        max_iterations: 15000,
        seed: 7,
        min_gap_mm: 1.5,
        gap_penalty_factor: 1.0,
        congestion_penalty_factor: 0.0,
        congestion_resolution: 0,
        max_step_mm: 25.0,
        ..PlaceOptions::default()
    };
    let report = place(&mut board, &["R3".to_string()], &opts, &MarginMap::new())
        .expect("placer should succeed");

    assert!(
        report.final_hpwl_mm < report.initial_hpwl_mm - 5.0,
        "expected clear HPWL drop, got {:.1} → {:.1}",
        report.initial_hpwl_mm,
        report.final_hpwl_mm,
    );

    let r3_after = board
        .footprints_in_order()
        .find(|f| f.reference == "R3")
        .unwrap()
        .position;
    assert!(
        r3_after.y.to_mm() < r3_before_y - 5.0,
        "R3 should move toward the U1–U3 segment at y=20: before y={r3_before_y:.1}, after y={:.1}",
        r3_after.y.to_mm()
    );
    assert!(
        report.moved.contains(&"R3".to_string()),
        "R3 must be reported as moved, got {:?}",
        report.moved
    );
}

/// Realistic IoT-board shape: two multi-pin modules, an OLED, an
/// edge-mounted screw terminal and five passives, all scattered to the
/// worst corners of an 85×52 board. The two-stage placer (electrostatic
/// global + SA legalisation) must recover a tight cluster: big raw-HPWL
/// cut, hard solder floor honoured, everything inside the outline, and
/// the screw terminal still on the outline edge.
#[test]
fn two_stage_placer_untangles_scattered_iot_board() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(85.0), Length::from_mm(52.0)),
    ));

    // "ESP module": 8 pads down two sides, 12×8 mm.
    let module = |reference: &str, x, y, nets: [&str; 8]| {
        let mut pads = Vec::new();
        for (i, n) in nets.iter().enumerate() {
            let (col, row) = (i / 4, i % 4);
            pads.push(pad(
                &format!("{}", i + 1),
                if col == 0 { -5.0 } else { 5.0 },
                row as f64 * 2.0 - 3.0,
                Some(n),
            ));
        }
        footprint(reference, x, y, pads)
    };
    // Passive: 2 pads, 3.2 mm apart.
    let passive = |reference: &str, x, y, a: &str, b: &str| {
        footprint(
            reference,
            x,
            y,
            vec![pad("1", -1.6, 0.0, Some(a)), pad("2", 1.6, 0.0, Some(b))],
        )
    };

    board.add_footprint(module(
        "U1",
        8.0,
        45.0,
        ["+3V3", "GND", "SCK", "MOSI", "MISO", "NSS", "SDA", "SCL"],
    ));
    board.add_footprint(module(
        "U2",
        78.0,
        6.0,
        ["+3V3", "GND", "SCK", "MOSI", "MISO", "NSS", "BUSY", "DIO1"],
    ));
    board.add_footprint(module(
        "DS1",
        78.0,
        46.0,
        ["+3V3", "GND", "SDA", "SCL", "NC1", "NC2", "NC3", "NC4"],
    ));
    board.add_footprint(passive("R1", 6.0, 4.0, "SDA", "+3V3"));
    board.add_footprint(passive("R2", 40.0, 50.0, "SCL", "+3V3"));
    board.add_footprint(passive("R3", 42.0, 4.0, "SSR_LED", "GND"));
    board.add_footprint(passive("C1", 6.0, 26.0, "+3V3", "GND"));
    board.add_footprint(passive("C2", 80.0, 26.0, "+3V3", "GND"));
    let mut term = passive("J1", 44.0, 26.0, "LOCK_A", "LOCK_B");
    term.edge_mounted = true;
    board.add_footprint(term);
    board.add_footprint(passive("U3", 20.0, 20.0, "SSR_LED", "LOCK_A"));

    let movable: Vec<String> = ["U1", "U2", "DS1", "R1", "R2", "R3", "C1", "C2", "J1", "U3"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let opts = PlaceOptions {
        seed: 42,
        ..PlaceOptions::default()
    };
    let report = place(&mut board, &movable, &opts, &MarginMap::new()).expect("place");

    // Global stage must have run and produced the bulk of the gain.
    let global = report.global.as_ref().expect("global stage should run");
    assert!(
        global.hpwl_mm < report.initial_hpwl_mm,
        "global stage should cut HPWL: {:.1} → {:.1}",
        report.initial_hpwl_mm,
        global.hpwl_mm,
    );
    // End-to-end: at least a 55 % raw-HPWL reduction on this layout.
    assert!(
        report.final_hpwl_mm < report.initial_hpwl_mm * 0.45,
        "expected ≥55 % HPWL cut, got {:.1} → {:.1} mm",
        report.initial_hpwl_mm,
        report.final_hpwl_mm,
    );
    // Hard solder floor: no two bodies closer than 1 mm (small epsilon
    // for the nm→mm rounding).
    let gap = min_pairwise_gap(&board, &MarginMap::new());
    assert!(
        gap >= 1.0 - 0.02,
        "solder floor violated: min gap {gap:.3} mm"
    );
    // Everything inside the outline.
    let outline = board.outline.unwrap();
    for fp in board.footprints_in_order() {
        let b = fp.bounds().unwrap();
        assert!(
            b.min.x.0 >= outline.min.x.0
                && b.min.y.0 >= outline.min.y.0
                && b.max.x.0 <= outline.max.x.0
                && b.max.y.0 <= outline.max.y.0,
            "{} left the outline",
            fp.reference,
        );
    }
    // Edge-mounted terminal still touches an edge.
    let j1 = board
        .footprints_in_order()
        .find(|fp| fp.reference == "J1")
        .unwrap()
        .clone();
    assert!(
        board.edge_mount_violation(&j1).is_none(),
        "J1 must stay on the outline edge"
    );
}
