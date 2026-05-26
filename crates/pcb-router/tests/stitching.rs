//! Stitching-via tests.

use pcb_core::{
    Board, CopperLayer, Footprint, Id, Length, Pad, Point, Pour, Rect, StitchPolicy, Trace,
};
use pcb_router::{add_stitching_vias, RouteOptions};

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
fn stitching_vias_added_when_both_layers_have_gnd_pour() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(30.0), Length::from_mm(20.0)),
    ));
    // Place a tiny footprint so the board isn't completely empty.
    board.add_footprint(fp(
        "U1",
        15.0,
        10.0,
        vec![Pad {
            number: "1".into(),
            name: String::new(),
            offset: Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
            size: (Length::from_mm(1.0), Length::from_mm(1.0)),
            layer: CopperLayer::Top,
            net: Some("GND".into()),
            drill: None,
        }],
    ));
    board.add_pour(Pour {
        net: "GND".into(),
        layer: CopperLayer::Top,
        thermal_relief: pcb_core::ThermalRelief::default(),
        stitching: StitchPolicy::Grid {
            pitch_mm: 5.0,
            clearance_mm: 0.5,
        },
    });
    board.add_pour(Pour {
        net: "GND".into(),
        layer: CopperLayer::Bottom,
        thermal_relief: pcb_core::ThermalRelief::default(),
        stitching: StitchPolicy::Grid {
            pitch_mm: 5.0,
            clearance_mm: 0.5,
        },
    });
    let added = add_stitching_vias(&mut board, &RouteOptions::default());
    assert!(added > 0, "expected at least one stitching via, got 0");
    // All new vias should be on GND and inside the outline.
    for v in &board.vias {
        assert_eq!(v.net, "GND");
        let x = v.position.x.to_mm();
        let y = v.position.y.to_mm();
        assert!(x >= 0.0 && x <= 30.0 && y >= 0.0 && y <= 20.0);
    }
}

#[test]
fn stitching_respects_trace_clearance() {
    let mut board = Board::new();
    board.outline = Some(Rect::from_corners(
        Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
        Point::new(Length::from_mm(30.0), Length::from_mm(20.0)),
    ));
    // A trace through the middle of the pour.
    board.add_trace(Trace {
        id: Id::new(),
        layer: CopperLayer::Top,
        start: Point::new(Length::from_mm(2.0), Length::from_mm(10.0)),
        end: Point::new(Length::from_mm(28.0), Length::from_mm(10.0)),
        width: Length::from_mm(0.30),
        net: "SIG".into(),
    });
    board.add_pour(Pour {
        net: "GND".into(),
        layer: CopperLayer::Top,
        thermal_relief: pcb_core::ThermalRelief::default(),
        stitching: StitchPolicy::Grid {
            pitch_mm: 2.0,
            clearance_mm: 0.6,
        },
    });
    board.add_pour(Pour {
        net: "GND".into(),
        layer: CopperLayer::Bottom,
        thermal_relief: pcb_core::ThermalRelief::default(),
        stitching: StitchPolicy::Grid {
            pitch_mm: 2.0,
            clearance_mm: 0.6,
        },
    });
    let opts = RouteOptions::default();
    let _ = add_stitching_vias(&mut board, &opts);
    let via_r = opts.via_diameter.to_mm() / 2.0;
    let trace_half = 0.30 / 2.0;
    let min_dist = via_r + 0.6 + trace_half;
    for v in &board.vias {
        // Distance from via centre to the horizontal trace y=10.
        let dy = (v.position.y.to_mm() - 10.0).abs();
        let x = v.position.x.to_mm();
        if x >= 2.0 && x <= 28.0 {
            assert!(
                dy >= min_dist - 1e-6,
                "via at ({}, {}) is {} mm from trace, expected >= {}",
                x,
                v.position.y.to_mm(),
                dy,
                min_dist
            );
        }
    }
}
