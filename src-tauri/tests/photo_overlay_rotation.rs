//! Acceptance gate for the board-canvas photo overlay (Fix 2).
//!
//! A calibrated module photo is painted into the board SVG's
//! `#photo-underlay` group as an `<image>` whose transform is
//! `translate(x,y) rotate(rot) matrix(M)`, nested inside the board SVG's
//! outer `scale(1,-1)` — the EXACT chain the renderer draws a footprint's
//! pads with (`translate(x,y) rotate(rot)` around placed-local pad
//! offsets, per `pcb_render::render_svg`). `M` is the production overlay
//! matrix (`LibraryEntry::photo_overlay_matrix`) mapping a raw photo pixel
//! to the placed footprint's local mm.
//!
//! Because the overlay and the pads share that outer chain, a calibration
//! pin pixel must land on its pad at ANY footprint rotation. The report
//! was "photos don't rotate / pile up at 90/180/270" — so this test drives
//! a synthetic calibrated part at rotations 0/90/180/270 and asserts, for
//! BOTH calibration pins, that the pixel pushed through the overlay chain
//! coincides (< 1 µm) with the pad pushed through the render's pad chain.
//! A rotation-sign or matrix-compose regression on either side breaks it.
//!
//! The part carries a non-identity `footprint_view_transform` (90° CCW) so
//! the calibration→view compose inside `photo_overlay_matrix` is exercised
//! too, not just the identity fast path.

use pcb_core::{LibraryEntry, PhotoCalibration, ViewTransform};

/// SVG y-up→y-down flip applied once by the board SVG's outer
/// `<g transform="scale(1,-1)">`.
fn flip(p: (f64, f64)) -> (f64, f64) {
    (p.0, -p.1)
}

/// Screen position of a placed-local-mm point under a footprint's
/// `translate(fx,fy) rotate(rot)` group, then the outer flip — the exact
/// chain `pcb_render::render_svg` draws pads with.
fn pad_chain(fx: f64, fy: f64, rot_deg: f64, lx: f64, ly: f64) -> (f64, f64) {
    let th = rot_deg.to_radians();
    let (s, c) = th.sin_cos();
    // SVG rotate(a): x' = x·cos − y·sin, y' = x·sin + y·cos (CCW in the
    // y-up frame the outer scale(1,-1) establishes).
    let rx = lx * c - ly * s;
    let ry = lx * s + ly * c;
    flip((fx + rx, fy + ry))
}

/// Screen position of an image pixel under `translate(fx,fy) rotate(rot)
/// matrix(M)` then the outer flip — the exact chain the frontend paints
/// the overlay `<image>` with. `M` is SVG-order `[a,b,c,d,e,f]`.
fn overlay_chain(fx: f64, fy: f64, rot_deg: f64, m: [f64; 6], px: f64, py: f64) -> (f64, f64) {
    let [a, b, c, d, e, f] = m;
    // matrix(M): (a·px + c·py + e, b·px + d·py + f) — into placed-local mm.
    let lx = a * px + c * py + e;
    let ly = b * px + d * py + f;
    pad_chain(fx, fy, rot_deg, lx, ly)
}

/// Synthetic entry: two asymmetric pads (so rotation SIGN is observable)
/// plus a 90° view transform. Built via JSON to dodge the many-field
/// `LibraryEntry` constructor churn.
fn synthetic_entry() -> LibraryEntry {
    let json = serde_json::json!({
        "key": "probe_part",
        "description": "rotation probe",
        "created_at": 0,
        "footprint_view_transform": { "rotation_deg": 90, "flip_h": false, "flip_v": false },
        "pads": [
            { "number": "1", "name": "A", "x_mm": -6.0, "y_mm": 3.0, "w_mm": 1.0, "h_mm": 1.0 },
            { "number": "2", "name": "B", "x_mm":  4.0, "y_mm": -2.0, "w_mm": 1.0, "h_mm": 1.0 }
        ]
    });
    serde_json::from_value(json).expect("synthetic entry deserializes")
}

#[test]
fn photo_overlay_pins_track_pads_at_every_rotation() {
    let entry = synthetic_entry();
    // Two pin marks in raw image pixels (y-down). Deliberately off-axis so
    // the derived similarity has a real rotation, not a trivial scale.
    let cal = PhotoCalibration {
        a_px: (120.0, 480.0),
        b_px: (560.0, 190.0),
        a_pad: "1".into(),
        b_pad: "2".into(),
    };

    // Production overlay matrix (px → placed-local mm).
    let m = entry
        .photo_overlay_matrix(&cal)
        .expect("overlay matrix derives");

    // Placed-local pad centres = native centre through the view transform,
    // exactly what the spawn pipeline bakes into the board footprint's pads.
    let view: ViewTransform = entry.footprint_view_transform;
    let (n1x, n1y) = entry.pad_center_mm("1").unwrap();
    let (n2x, n2y) = entry.pad_center_mm("2").unwrap();
    let placed1 = view.apply_point_mm(n1x, n1y);
    let placed2 = view.apply_point_mm(n2x, n2y);

    // Placed footprint origin on the board.
    let (fx, fy) = (18.0_f64, 25.0_f64);

    for rot in [0.0_f64, 90.0, 180.0, 270.0] {
        let pad1 = pad_chain(fx, fy, rot, placed1.0, placed1.1);
        let pin1 = overlay_chain(fx, fy, rot, m, cal.a_px.0, cal.a_px.1);
        let pad2 = pad_chain(fx, fy, rot, placed2.0, placed2.1);
        let pin2 = overlay_chain(fx, fy, rot, m, cal.b_px.0, cal.b_px.1);

        let d1 = (pad1.0 - pin1.0).hypot(pad1.1 - pin1.1);
        let d2 = (pad2.0 - pin2.0).hypot(pad2.1 - pin2.1);
        assert!(
            d1 < 1e-6,
            "rot {rot}: pin 1 off pad by {d1:.6} mm (pad {pad1:?} vs pin {pin1:?})",
        );
        assert!(
            d2 < 1e-6,
            "rot {rot}: pin 2 off pad by {d2:.6} mm (pad {pad2:?} vs pin {pin2:?})",
        );
    }
}

/// Bind the pad-chain convention to the real renderer: `pcb_render` must
/// draw a footprint as `translate(x,y) rotate(rot)` (inside the outer
/// `scale(1,-1)`). If it ever flips the rotation sign or drops the group,
/// the overlay chain above no longer matches reality — catch it here.
#[test]
fn renderer_uses_the_assumed_footprint_transform() {
    use pcb_core::{Board, CopperLayer, Footprint, Id, Length, Pad, Point};

    let mut board = Board::new();
    let fp = Footprint {
        id: Id::new(),
        reference: "PRB".into(),
        value: String::new(),
        library: "demo".into(),
        position: Point::new(Length::from_mm(18.0), Length::from_mm(25.0)),
        rotation: 90.0,
        layer: CopperLayer::Top,
        pads: vec![Pad {
            number: "1".into(),
            name: String::new(),
            offset: Point::new(Length::from_mm(-3.0), Length::from_mm(-6.0)),
            size: (Length::from_mm(1.0), Length::from_mm(1.0)),
            layer: CopperLayer::Top,
            net: None,
            drill: None,
        }],
        key: String::new(),
        description: String::new(),
        edge_mounted: false,
        silk: Vec::new(),
    };
    board.add_footprint(fp);
    let svg = pcb_render::render_svg(&board);

    assert!(
        svg.contains(r#"<g transform="scale(1,-1)">"#),
        "board SVG must wrap content in an outer scale(1,-1) flip",
    );
    let marker = "data-board-ref=\"PRB\"";
    let i = svg.find(marker).expect("footprint group present");
    let seg = &svg[i..(i + 160).min(svg.len())];
    assert!(
        seg.contains("translate(18.000,25.000) rotate(90.00)"),
        "footprint transform convention changed: {seg}",
    );
}
