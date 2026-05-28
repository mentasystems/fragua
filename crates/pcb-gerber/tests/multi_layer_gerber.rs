//! Phase 4: gerber writer emits one copper file per stackup layer.

use std::fs;

use pcb_core::{Board, CopperLayer, Footprint, Id, LayerStackup, Length, Pad, Point, Rect};
use pcb_gerber::write_fab_pack;

fn footprint(reference: &str, x_mm: f64, y_mm: f64) -> Footprint {
    Footprint {
        id: Id::new(),
        reference: reference.into(),
        value: "10k".into(),
        library: "Resistor_SMD:R_0805".into(),
        position: Point::new(Length::from_mm(x_mm), Length::from_mm(y_mm)),
        rotation: 0.0,
        layer: CopperLayer::Top,
        pads: vec![Pad {
            number: "1".into(),
            name: String::new(),
            offset: Point::new(Length::ZERO, Length::ZERO),
            size: (Length::from_mm(1.0), Length::from_mm(1.2)),
            layer: CopperLayer::Top,
            net: None,
            drill: None,
        }],
        key: String::new(),
        description: String::new(),
        edge_mounted: false,
        silk: Vec::new(),
    }
}

fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("pcb-gerber-mlt-{pid}-{nanos}"));
    p
}

#[test]
fn gerber_emits_one_file_per_layer() {
    // A 4-layer board: the fab pack should contain `F_Cu`, `In1_Cu`,
    // `In2_Cu`, `B_Cu` (one copper gerber per stackup layer), plus
    // the usual masks / silks / edge / drills / CSVs.
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(30.0)),
    ));
    board.stackup = LayerStackup::fr4(4);
    board.add_footprint(footprint("R1", 10.0, 15.0));

    let dir = tempdir();
    let paths = write_fab_pack(&board, "demo4", &dir).unwrap();
    let names: Vec<String> = paths
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    // The four copper files come first (one per stackup layer), in
    // top → bottom order.
    assert!(names.contains(&"demo4-F_Cu.gbr".to_string()), "{names:?}");
    assert!(names.contains(&"demo4-In1_Cu.gbr".to_string()), "{names:?}");
    assert!(names.contains(&"demo4-In2_Cu.gbr".to_string()), "{names:?}");
    assert!(names.contains(&"demo4-B_Cu.gbr".to_string()), "{names:?}");

    // Every emitted copper file is a well-formed gerber.
    for name in [
        "demo4-F_Cu.gbr",
        "demo4-In1_Cu.gbr",
        "demo4-In2_Cu.gbr",
        "demo4-B_Cu.gbr",
    ] {
        let body = fs::read_to_string(dir.join(name)).unwrap();
        assert!(body.starts_with("G04 pcb"), "{name}: missing header");
        assert!(body.contains("%FSLAX46Y46*%"), "{name}: missing format");
        assert!(body.trim_end().ends_with("M02*"), "{name}: missing footer");
    }
    fs::remove_dir_all(&dir).ok();
}

/// A PTH pad (`drill = Some`) must export a copper flash on **every**
/// copper layer plus a solder-mask opening on **both** outer sides —
/// otherwise the fabricated board only has the top annular ring and
/// the through-hole connection is broken. Regression test for the bug
/// where `Pad::layer` was treated as the sole copper layer instead of
/// the mount side.
#[test]
fn pth_pad_emits_copper_and_mask_on_every_layer() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(30.0)),
    ));
    // 4 copper layers so we cover top + bottom + inner.
    board.stackup = LayerStackup::fr4(4);

    // A footprint mounted on top with a single PTH pad (drill = 0.8 mm
    // through a 1.6 × 1.6 mm landing) and no other pads. The pad is
    // assigned to Top — that's the mount side — but its copper ring
    // must still appear on every copper layer.
    let mut fp = footprint("U1", 20.0, 15.0);
    fp.pads[0].size = (Length::from_mm(1.6), Length::from_mm(1.6));
    fp.pads[0].drill = Some(Length::from_mm(0.8));
    board.add_footprint(fp);

    let dir = tempdir();
    let _ = pcb_gerber::write_fab_pack(&board, "pth", &dir).unwrap();

    // Every copper gerber (top, both inners, bottom) must contain at
    // least one pad flash (`D03*`).
    for name in ["pth-F_Cu.gbr", "pth-In1_Cu.gbr", "pth-In2_Cu.gbr", "pth-B_Cu.gbr"] {
        let body = fs::read_to_string(dir.join(name)).unwrap();
        assert!(
            body.contains("D03*"),
            "{name}: PTH pad has no copper flash — only the mount side got copper"
        );
    }

    // Both solder masks must have an opening for the PTH pad.
    for name in ["pth-F_Mask.gbr", "pth-B_Mask.gbr"] {
        let body = fs::read_to_string(dir.join(name)).unwrap();
        assert!(
            body.contains("D03*"),
            "{name}: PTH pad has no mask opening — fab would cover the bottom ring"
        );
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn legacy_two_layer_pack_keeps_historical_filenames() {
    // 2-layer board still produces the exact pre-Phase-4 stems:
    // `F_Cu` + `B_Cu`. The fab portals key off these filenames, so
    // any drift here is a user-visible regression.
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(30.0)),
    ));
    board.add_footprint(footprint("R1", 10.0, 15.0));

    let dir = tempdir();
    let paths = write_fab_pack(&board, "demo2", &dir).unwrap();
    let names: Vec<String> = paths
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    assert!(names.contains(&"demo2-F_Cu.gbr".to_string()), "{names:?}");
    assert!(names.contains(&"demo2-B_Cu.gbr".to_string()), "{names:?}");
    // No inner layer files on a 2-layer board.
    for n in &names {
        assert!(
            !n.contains("In1_Cu") && !n.contains("In2_Cu"),
            "unexpected inner-layer file on 2-layer board: {n}"
        );
    }
    fs::remove_dir_all(&dir).ok();
}
