//! Diagnostic: SA-only vs electrostatic-global + SA on the same
//! scattered 10-part board. Run with
//! `cargo run -p pcb-placer --example compare_stages --release`.

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

fn build_board() -> (Board, Vec<String>) {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(85.0), Length::from_mm(52.0)),
    ));
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
    let movable = ["U1", "U2", "DS1", "R1", "R2", "R3", "C1", "C2", "J1", "U3"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    (board, movable)
}

fn main() {
    for seed in [42u64, 7, 1234] {
        for (label, global) in [("sa-only ", false), ("two-stage", true)] {
            let (mut board, movable) = build_board();
            let opts = PlaceOptions {
                seed,
                global_stage: global,
                ..PlaceOptions::default()
            };
            let t0 = std::time::Instant::now();
            let report = place(&mut board, &movable, &opts, &MarginMap::new()).unwrap();
            let dt = t0.elapsed();
            let gap = min_pairwise_gap(&board, &MarginMap::new());
            println!(
                "seed {seed:5} {label}: HPWL {:7.1} → {:6.1} mm ({:5.1} %)  min-gap {gap:5.2} mm  {dt:.2?}{}",
                report.initial_hpwl_mm,
                report.final_hpwl_mm,
                100.0 * report.final_hpwl_mm / report.initial_hpwl_mm,
                report
                    .global
                    .as_ref()
                    .map(|g| format!("  [global: {} iters, τ {:.3}, HPWL {:.1}]", g.iterations, g.overflow, g.hpwl_mm))
                    .unwrap_or_default(),
            );
        }
    }
}
