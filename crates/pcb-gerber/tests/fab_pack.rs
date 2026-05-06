//! Validate the fab pack on a small synthetic board: every file is
//! created, gerbers are well-formed (header + end), CSVs have headers
//! and one row per group/footprint, and coordinates round-trip through
//! the nm fixed-point representation.

use std::fs;

use pcb_core::{
    Board, CopperLayer, Footprint, Id, Length, Pad, Point, Rect, SilkAnchor, SilkLayer, SilkText,
};
use pcb_gerber::write_fab_pack;

fn footprint(reference: &str, value: &str, x_mm: f64, y_mm: f64) -> Footprint {
    Footprint {
        id: Id::new(),
        reference: reference.into(),
        value: value.into(),
        library: "Resistor_SMD:R_0805".into(),
        position: Point::new(Length::from_mm(x_mm), Length::from_mm(y_mm)),
        rotation: 0.0,
        layer: CopperLayer::Top,
        pads: vec![
            Pad {
                number: "1".into(),
                name: String::new(),
                offset: Point::new(Length::from_mm(-1.0), Length::ZERO),
                size: (Length::from_mm(1.0), Length::from_mm(1.2)),
                layer: CopperLayer::Top,
                net: None,
            },
            Pad {
                number: "2".into(),
                name: String::new(),
                offset: Point::new(Length::from_mm(1.0), Length::ZERO),
                size: (Length::from_mm(1.0), Length::from_mm(1.2)),
                layer: CopperLayer::Top,
                net: None,
            },
        ],
        key: String::new(),
        description: String::new(),
        edge_mounted: false,
        silk: Vec::new(),
    }
}

fn build_board() -> Board {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(40.0), Length::from_mm(30.0)),
    ));
    board.add_footprint(footprint("R1", "10k", 10.0, 15.0));
    board.add_footprint(footprint("R2", "10k", 16.0, 15.0));
    board.add_footprint(footprint("R3", "1k", 22.0, 15.0));
    // Board-level silk text so the silk Gerber test below has
    // something concrete to verify beyond the synthesised footprint
    // labels.
    board.add_silk_text(SilkText {
        layer: SilkLayer::Top,
        position: Point::new(Length::from_mm(20.0), Length::from_mm(3.0)),
        text: "PCB".into(),
        size: Length::from_mm(2.0),
        rotation: 0.0,
        anchor: SilkAnchor::Middle,
        width: SilkText::default_stroke(Length::from_mm(2.0)),
    });
    board
}

#[test]
fn fab_pack_writes_every_expected_file_and_each_is_well_formed() {
    let dir = tempdir();
    let board = build_board();
    let paths = write_fab_pack(&board, "demo", &dir).unwrap();

    let names: Vec<String> = paths
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![
            "demo-F_Cu.gbr",
            "demo-B_Cu.gbr",
            "demo-F_Mask.gbr",
            "demo-B_Mask.gbr",
            "demo-F_SilkS.gbr",
            "demo-B_SilkS.gbr",
            "demo-Edge_Cuts.gbr",
            "demo-PTH.drl",
            "demo-NPTH.drl",
            "demo-bom.csv",
            "demo-pos.csv",
        ]
    );

    // Every gerber must start with a comment + format spec and end with M02*.
    for name in [
        "demo-F_Cu.gbr",
        "demo-B_Cu.gbr",
        "demo-F_Mask.gbr",
        "demo-B_Mask.gbr",
        "demo-F_SilkS.gbr",
        "demo-B_SilkS.gbr",
        "demo-Edge_Cuts.gbr",
    ] {
        let body = fs::read_to_string(dir.join(name)).unwrap();
        assert!(body.starts_with("G04 pcb"), "{name}: missing header comment");
        assert!(body.contains("%FSLAX46Y46*%"), "{name}: missing format spec");
        assert!(body.contains("%MOMM*%"), "{name}: missing units");
        assert!(body.trim_end().ends_with("M02*"), "{name}: missing M02 footer");
    }

    // Both copper sides should have 6 flashes (3 footprints × 2 pads on top side, 0 on bottom).
    let f_cu = fs::read_to_string(dir.join("demo-F_Cu.gbr")).unwrap();
    assert_eq!(f_cu.matches("D03*").count(), 6);
    let b_cu = fs::read_to_string(dir.join("demo-B_Cu.gbr")).unwrap();
    assert_eq!(b_cu.matches("D03*").count(), 0);

    // F.Mask uses one larger aperture covering all six pads.
    let f_mask = fs::read_to_string(dir.join("demo-F_Mask.gbr")).unwrap();
    assert_eq!(f_mask.matches("D03*").count(), 6);
    // 1.0 + 2*0.05 = 1.10 mm wide; 1.2 + 2*0.05 = 1.30 mm tall.
    assert!(
        f_mask.contains("R,1.100000X1.300000"),
        "expected expanded mask aperture, got:\n{f_mask}"
    );

    // F.SilkS must contain at least one D02 (move) and one D01
    // (interpolation): the synthesised `R1` footprint label and the
    // explicit "PCB" board-level text both turn into Hershey
    // segments. B.SilkS has no items here but still must contain
    // valid header + footer.
    let f_silk = fs::read_to_string(dir.join("demo-F_SilkS.gbr")).unwrap();
    assert!(f_silk.contains("Legend,Top"), "expected X2 Legend,Top attribute");
    assert!(f_silk.matches("D01*").count() > 0, "F.SilkS missing D01 lines: {f_silk}");
    assert!(f_silk.matches("D02*").count() > 0, "F.SilkS missing D02 lines: {f_silk}");
    let b_silk = fs::read_to_string(dir.join("demo-B_SilkS.gbr")).unwrap();
    assert!(b_silk.contains("Legend,Bot"));

    // Edge cuts traces the 40x30 outline, four interpolations.
    let edge = fs::read_to_string(dir.join("demo-Edge_Cuts.gbr")).unwrap();
    assert_eq!(edge.matches("D01*").count(), 4);

    // BOM: 1 header + 2 groups (10k × R1+R2; 1k × R3).
    let bom = fs::read_to_string(dir.join("demo-bom.csv")).unwrap();
    let bom_lines: Vec<&str> = bom.lines().collect();
    assert_eq!(bom_lines[0], "Reference,Value,Footprint,Quantity");
    assert_eq!(bom_lines.len(), 3);
    assert!(bom_lines.iter().any(|l| l.contains("R1 R2") && l.ends_with(",2")));
    assert!(bom_lines.iter().any(|l| l.contains("R3") && l.ends_with(",1")));

    // Positions: 1 header + 3 footprints.
    let pos = fs::read_to_string(dir.join("demo-pos.csv")).unwrap();
    let pos_lines: Vec<&str> = pos.lines().collect();
    assert_eq!(pos_lines[0], "Reference,Value,Footprint,X,Y,Rotation,Side");
    assert_eq!(pos_lines.len(), 4);
    assert!(pos_lines[1].starts_with("R1,10k,Resistor_SMD:R_0805,10.0000,15.0000,0.00,top"));

    fs::remove_dir_all(&dir).ok();
}

fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("pcb-gerber-test-{pid}-{nanos}"));
    p
}
