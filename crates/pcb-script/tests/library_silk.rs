//! End-to-end test for library-authored silk: create a library entry
//! that ships silk geometry, spawn it via `palette.add_from_library`,
//! and verify the resulting palette footprint carries the silk.
//!
//! `dispatch` is `async` for the parts of the catalog that genuinely
//! await (script, batch); the codepath this test exercises is fully
//! synchronous, so we drive the future with a hand-rolled
//! single-step executor instead of pulling tokio in as a dev-dep.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use pcb_core::{Project, SilkLayer};
use serde_json::json;

struct NoopWake;
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn block_on<F: Future>(fut: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    let mut fut = Box::pin(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("test future stalled — handler should be sync"),
    }
}

#[test]
fn library_silk_propagates_to_spawned_footprint() {
    // Sandbox the library to a temp dir so the test does not touch
    // `~/.pcb-library/`.
    let tmp = std::env::temp_dir().join(format!("pcb-test-lib-silk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("mkdir tmp HOME");
    std::env::set_var("HOME", &tmp);
    let project = Project::new("silk-test");

    // 1. library.create with two pads + a silk-line + a silk-text.
    let create_args = json!({
        "key": "test_two_pad",
        "description": "two-pad part with body outline",
        "pads": [
            {"number": "1", "x_mm": -2.0, "y_mm": 0.0, "w_mm": 1.0, "h_mm": 1.5},
            {"number": "2", "x_mm":  2.0, "y_mm": 0.0, "w_mm": 1.0, "h_mm": 1.5},
        ],
        "silk": [
            {"kind": "line", "layer": "top", "x1_mm": -3.0, "y1_mm": -1.5, "x2_mm": 3.0, "y2_mm": -1.5, "width_mm": 0.15},
            {"kind": "text", "layer": "top", "x_mm": 0.0, "y_mm": 1.5, "text": "{REF}", "size_mm": 1.0, "anchor": "middle"},
        ],
    });
    block_on(pcb_script::tools::dispatch(&project, "library.create", &create_args))
        .map_err(|e| e.message)
        .expect("library.create");

    // Library entry should now expose the silk array.
    let entry = project.library().find("test_two_pad").expect("entry stored");
    assert_eq!(entry.silk.len(), 2, "library entry should carry both silk items");

    // 2. Add a schematic symbol so palette.add_from_library can find it.
    block_on(pcb_script::tools::dispatch(
        &project,
        "schematic.add_symbol",
        &json!({"reference": "U1", "kind": "resistor"}),
    ))
    .map_err(|e| e.message)
    .expect("schematic.add_symbol");

    // 3. palette.add_from_library — the silk should follow.
    block_on(pcb_script::tools::dispatch(
        &project,
        "palette.add_from_library",
        &json!({"reference": "U1", "key": "test_two_pad"}),
    ))
    .map_err(|e| e.message)
    .expect("palette.add_from_library");

    let snap = project.read();
    let palette = snap.palette();
    let fp = palette.iter().find(|f| f.reference == "U1").expect("U1 in palette");
    assert!(!fp.silk.is_empty(), "spawned footprint must carry library silk");
    assert_eq!(fp.silk.len(), 2);
    // First entry is the line on the top layer.
    match &fp.silk[0] {
        pcb_core::FootprintSilk::Line { layer, width, .. } => {
            assert_eq!(*layer, SilkLayer::Top);
            assert!((width.to_mm() - 0.15).abs() < 1e-6);
        }
        other => panic!("expected line, got {other:?}"),
    }
    // Second entry is the text with the {REF} placeholder.
    match &fp.silk[1] {
        pcb_core::FootprintSilk::Text { text, layer, .. } => {
            assert_eq!(text, "{REF}");
            assert_eq!(*layer, SilkLayer::Top);
        }
        other => panic!("expected text, got {other:?}"),
    }
}
