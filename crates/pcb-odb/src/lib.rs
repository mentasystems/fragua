//! ODB++ exporter — minimum viable subset of the v8.0 "Job Outline"
//! tree that JLCPCB's uploader accepts.
//!
//! ODB++ is the industry-standard PCB fab interchange format. Unlike
//! Gerber (one file per layer, no netlist, no stackup) a single
//! ODB++ tarball carries the whole job: layer stackup, drill data,
//! BOM, netlist, dimensions. We emit a deliberately small subset —
//! enough for JLCPCB to ingest, not enough to claim full v8.0
//! compliance.
//!
//! Tree shape (under `<board_name>/`):
//!
//! ```text
//! misc/info
//! misc/matrix
//! steps/pcb/eda/data
//! steps/pcb/layers/comp_+_top/features
//! steps/pcb/layers/comp_+_bot/features
//! steps/pcb/layers/drill/features
//! steps/pcb/layers/outline/features
//! steps/pcb/layers/silk_top/features
//! steps/pcb/layers/silk_bot/features
//! steps/pcb/layers/soldermask_top/features
//! steps/pcb/layers/soldermask_bot/features
//! steps/pcb/profile
//! steps/pcb/stephdr
//! ```
//!
//! The `features` text format is plain-text, one feature per line. We
//! implement the symbol-level subset: `L` (line / trace), `P` (pad
//! flash) with positional aperture references, and `S ... SR ... SE`
//! (surface / region). The `LP` directive switches polarity (`P`
//! positive / `N` negative). Apertures are declared via top-level
//! `$<id> standard <shape> <dim>` lines per ODB++ convention.

use std::io;

use flate2::write::GzEncoder;
use flate2::Compression;

use pcb_core::{Board, Length, Pad, Point};

pub mod features;
pub mod tar;

/// Pack a Board into an ODB++ `.tgz`. Returns the gzip-compressed
/// tar archive bytes.
///
/// `board_name` is used as both the top-level directory inside the
/// archive and the suggested filename stem.
pub fn write_odb_tgz(board: &Board, board_name: &str) -> io::Result<Vec<u8>> {
    let tree = build_tree(board, board_name);
    let mut out = Vec::with_capacity(64 * 1024);
    {
        let mut gz = GzEncoder::new(&mut out, Compression::default());
        tar::write_archive(&mut gz, &tree)?;
        gz.finish()?;
    }
    Ok(out)
}

/// Build the in-memory list of `(path, contents)` pairs that make up
/// the ODB++ tree. Public so consumers (the Tauri command, tests)
/// can inspect or post-process the tree without re-deriving it.
#[must_use]
pub fn build_tree(board: &Board, board_name: &str) -> Vec<(String, Vec<u8>)> {
    let stem = sanitize_name(board_name);
    let prefix = format!("{stem}/");
    let mut tree: Vec<(String, Vec<u8>)> = Vec::new();

    // misc/info: job header (ODB++ "INFO" file).
    let info = format!(
        "JOB_NAME={stem}\nUNITS=MM\nVERSION_MAJOR=8\nVERSION_MINOR=0\nGENERATED_BY=pcb-odb {ver}\n",
        ver = env!("CARGO_PKG_VERSION"),
    );
    tree.push((format!("{prefix}misc/info"), info.into_bytes()));

    // misc/matrix: lists the steps and per-step layer map. Single
    // step `pcb` with our layer set.
    tree.push((format!("{prefix}misc/matrix"), build_matrix().into_bytes()));

    // steps/pcb/stephdr — minimal step header.
    let stephdr =
        "UNITS=MM\nX_DATUM=0\nY_DATUM=0\nX_ORIGIN=0\nY_ORIGIN=0\nTOP_ACTIVE=0\nBOTTOM_ACTIVE=0\nRIGHT_ACTIVE=0\nLEFT_ACTIVE=0\nAFFECTING_BOM=\nAFFECTING_BOM_CHANGED=0\n";
    tree.push((
        format!("{prefix}steps/pcb/stephdr"),
        stephdr.as_bytes().to_vec(),
    ));

    // steps/pcb/profile — board outline polygon. ODB++ "profile" is
    // the cut line.
    tree.push((
        format!("{prefix}steps/pcb/profile"),
        build_profile(board).into_bytes(),
    ));

    // steps/pcb/eda/data — net list + components. Minimum form.
    tree.push((
        format!("{prefix}steps/pcb/eda/data"),
        build_eda_data(board).into_bytes(),
    ));

    // Per-layer features files.
    let layers: &[(&str, LayerKind)] = &[
        ("comp_+_top", LayerKind::CopperTop),
        ("comp_+_bot", LayerKind::CopperBottom),
        ("drill", LayerKind::Drill),
        ("outline", LayerKind::Outline),
        ("silk_top", LayerKind::SilkTop),
        ("silk_bot", LayerKind::SilkBottom),
        ("soldermask_top", LayerKind::SoldermaskTop),
        ("soldermask_bot", LayerKind::SoldermaskBottom),
    ];
    for (name, kind) in layers {
        let features = features::build_layer(board, *kind);
        tree.push((
            format!("{prefix}steps/pcb/layers/{name}/features"),
            features.into_bytes(),
        ));
    }

    tree
}

/// Layer-kind selector used by the features writer.
#[derive(Debug, Clone, Copy)]
pub enum LayerKind {
    CopperTop,
    CopperBottom,
    Drill,
    Outline,
    SilkTop,
    SilkBottom,
    SoldermaskTop,
    SoldermaskBottom,
}

fn build_matrix() -> String {
    // ODB++ matrix file: a list of `STEP {...}` entries followed by
    // a list of `LAYER {...}` entries. We have one step (`pcb`) and
    // the layer set declared in `build_tree`.
    let mut out = String::new();
    out.push_str("STEP {\n  COL=1\n  NAME=pcb\n}\n");
    let layers = [
        ("comp_+_top", "COMPONENT", "TOP"),
        ("comp_+_bot", "COMPONENT", "BOTTOM"),
        ("drill", "DRILL", "ALL"),
        ("outline", "DOCUMENT", "ALL"),
        ("silk_top", "SILK_SCREEN", "TOP"),
        ("silk_bot", "SILK_SCREEN", "BOTTOM"),
        ("soldermask_top", "SOLDER_MASK", "TOP"),
        ("soldermask_bot", "SOLDER_MASK", "BOTTOM"),
    ];
    for (i, (name, ty, side)) in layers.iter().enumerate() {
        out.push_str(&format!(
            "LAYER {{\n  ROW={row}\n  CONTEXT=BOARD\n  TYPE={ty}\n  NAME={name}\n  POLARITY=POSITIVE\n  START_NAME=\n  END_NAME=\n  ADD_TYPE=\n  SIDE={side}\n}}\n",
            row = i + 1,
        ));
    }
    out
}

fn build_profile(board: &Board) -> String {
    let mut out = String::new();
    let Some(rect) = board.outline else {
        return out;
    };
    // Surface form: S ... SR <surface> SE
    out.push_str("S 0 0\n");
    let xs = rect.min.x.to_mm();
    let ys = rect.min.y.to_mm();
    let xe = rect.max.x.to_mm();
    let ye = rect.max.y.to_mm();
    out.push_str(&format!("OB {xs:.4} {ys:.4} I\n"));
    out.push_str(&format!("OS {xe:.4} {ys:.4}\n"));
    out.push_str(&format!("OS {xe:.4} {ye:.4}\n"));
    out.push_str(&format!("OS {xs:.4} {ye:.4}\n"));
    out.push_str(&format!("OS {xs:.4} {ys:.4}\n"));
    out.push_str("OE\n");
    out.push_str("SE\n");
    out
}

fn build_eda_data(board: &Board) -> String {
    let mut out = String::new();
    out.push_str("HDR UNITS=MM\n");
    // Net list — collect unique pad nets.
    let mut nets: Vec<&str> = Vec::new();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            if let Some(n) = pad.net.as_deref() {
                if !nets.contains(&n) {
                    nets.push(n);
                }
            }
        }
    }
    for (i, net) in nets.iter().enumerate() {
        out.push_str(&format!("NET {i} {net}\n"));
    }
    // Components (PRP / TOP form, one per footprint).
    for (i, fp) in board.footprints_in_order().enumerate() {
        out.push_str(&format!(
            "CMP {i} {x:.4} {y:.4} {rot:.2} {layer} {ref_} {val}\n",
            x = fp.position.x.to_mm(),
            y = fp.position.y.to_mm(),
            rot = fp.rotation,
            layer = if fp.layer.is_top() { "T" } else { "B" },
            ref_ = sanitize_token(&fp.reference),
            val = sanitize_token(&fp.value),
        ));
    }
    out
}

/// Convenience for the Tauri command and tests.
#[must_use]
pub fn sanitize_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("board");
    }
    out
}

fn sanitize_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_whitespace() || c == '\n' {
            out.push('_');
        } else {
            out.push(c);
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Re-exports for testing convenience.
#[doc(hidden)]
pub fn pad_world_center_mm(fp: &pcb_core::Footprint, pad: &Pad) -> (f64, f64) {
    let c = fp.pad_world_center(pad);
    (c.x.to_mm(), c.y.to_mm())
}

#[allow(dead_code)]
fn _length_anchor(_l: Length) {}

#[allow(dead_code)]
fn _point_anchor(_p: Point) {}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_core::{Board, CopperLayer, Footprint, Id, Pad, Point, Rect, Trace};

    fn minimal_board() -> Board {
        let mut b = Board::new();
        b.outline = Some(Rect::from_corners(
            Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
            Point::new(Length::from_mm(20.0), Length::from_mm(15.0)),
        ));
        b.add_footprint(Footprint {
            id: Id::new(),
            reference: "R1".into(),
            value: "10k".into(),
            library: "test".into(),
            position: Point::new(Length::from_mm(5.0), Length::from_mm(5.0)),
            rotation: 0.0,
            layer: CopperLayer::Top,
            pads: vec![Pad {
                number: "1".into(),
                name: String::new(),
                offset: Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
                size: (Length::from_mm(1.0), Length::from_mm(1.0)),
                layer: CopperLayer::Top,
                net: Some("FOO".into()),
                drill: None,
            }],
            key: String::new(),
            description: String::new(),
            edge_mounted: false,
            silk: Vec::new(),
        });
        b.add_trace(Trace {
            id: Id::new(),
            layer: CopperLayer::Top,
            start: Point::new(Length::from_mm(5.0), Length::from_mm(5.0)),
            end: Point::new(Length::from_mm(10.0), Length::from_mm(5.0)),
            width: Length::from_mm(0.25),
            net: "FOO".into(),
        });
        b
    }

    #[test]
    fn odb_emits_basic_tree() {
        let b = minimal_board();
        let tgz = write_odb_tgz(&b, "demo").expect("write_odb_tgz");
        assert!(!tgz.is_empty(), "expected non-empty tarball");
        // gzip magic.
        assert_eq!(&tgz[0..2], &[0x1f, 0x8b]);
        // The tree definitions should mention every required path.
        let tree = build_tree(&b, "demo");
        let names: Vec<&str> = tree.iter().map(|(n, _)| n.as_str()).collect();
        for expected in [
            "demo/misc/info",
            "demo/misc/matrix",
            "demo/steps/pcb/eda/data",
            "demo/steps/pcb/layers/comp_+_top/features",
            "demo/steps/pcb/layers/comp_+_bot/features",
            "demo/steps/pcb/layers/drill/features",
            "demo/steps/pcb/layers/outline/features",
            "demo/steps/pcb/layers/silk_top/features",
            "demo/steps/pcb/layers/silk_bot/features",
            "demo/steps/pcb/layers/soldermask_top/features",
            "demo/steps/pcb/layers/soldermask_bot/features",
            "demo/steps/pcb/profile",
            "demo/steps/pcb/stephdr",
        ] {
            assert!(
                names.contains(&expected),
                "expected `{expected}` in tree, got {:?}",
                names
            );
        }
    }

    #[test]
    fn odb_features_format_round_trips() {
        let b = minimal_board();
        let tree = build_tree(&b, "demo");
        let (_, features) = tree
            .iter()
            .find(|(n, _)| n == "demo/steps/pcb/layers/comp_+_top/features")
            .expect("comp_+_top features");
        let text = std::str::from_utf8(features).expect("utf8");
        // The trace from (5,5) to (10,5) on the top layer should
        // emit an `L` line with both endpoints + net id.
        let line = text
            .lines()
            .find(|l| l.starts_with("L "))
            .expect("expected at least one L line for the trace");
        let parsed = features::parse_l_line(line).expect("parse L line");
        assert!(
            (parsed.x1 - 5.0).abs() < 1e-3 && (parsed.x2 - 10.0).abs() < 1e-3,
            "endpoints: {:?}",
            parsed
        );
        assert_eq!(parsed.net, "FOO");
    }
}
