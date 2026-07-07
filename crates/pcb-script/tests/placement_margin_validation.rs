//! Integration tests for `LibraryEntry::placement_margin` being honoured
//! in the manual `place` / `move` flow and the board renderer.
//!
//! These tests cover the three regression risks fixed by the
//! placement-margin feature:
//!   1. A `place` whose inflated body extends past the outline must
//!      FAIL with a clear message.
//!   2. A `place` (or `move`) whose inflated body overlaps another
//!      placed footprint's inflated body must FAIL.
//!   3. The board SVG must include a `data-body-outline` rect for
//!      every footprint with a non-zero placement margin.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll, Wake, Waker};

use pcb_core::{PlacementMargin, Project};
use serde_json::{json, Value};

/// Project::new opens `$HOME/.pcb-library/` and caches the handle, so
/// every test that calls `fresh_project` mutates the shared `HOME` env
/// var. Cargo's parallel test runner would otherwise interleave the
/// HOME writes and a Project would end up looking at a sibling test's
/// (now-deleted) library dir. Serialise tests behind a single mutex
/// instead of forcing `--test-threads=1` from the command line.
fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

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

fn run_script_raw(project: &Project, script: &str) -> Result<Value, String> {
    block_on(pcb_script::tools::dispatch(
        project,
        "script",
        &json!({"script": script}),
    ))
    .map_err(|e| format!("{} ({})", e.message, e.code))
}

fn run_script(project: &Project, script: &str) -> Value {
    run_script_raw(project, script)
        .unwrap_or_else(|e| panic!("script failed: {e}\n--script--\n{script}"))
}

fn fresh_project(name: &str) -> Project {
    let tmp = std::env::temp_dir().join(format!(
        "pcb-test-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("mkdir tmp HOME");
    std::env::set_var("HOME", &tmp);
    Project::new(name)
}

/// Spawn a tiny pad-only two-pad library entry, confirm it, and set a
/// `placement_margin` on its disk-backed entry — same shape the review
/// pane writes into the library after the user dials it in.
fn make_part_with_margin(project: &Project, key: &str, margin: PlacementMargin) {
    run_script(
        project,
        &format!("lib {key}\n  pad 1 -1 0 0.5 0.5\n  pad 2  1 0 0.5 0.5\n"),
    );
    project
        .confirm_pending_library_entry(key)
        .expect("confirm library entry");
    project
        .library()
        .set_placement_margin(key, margin)
        .expect("set margin");
}

/// Drill into a script result to find the per-line outcome list. The
/// `script` tool returns `{ results: [{line, ok, ...}] }`.
fn extract_results(reply: &Value) -> Vec<Value> {
    reply
        .get("structuredContent")
        .and_then(|d| d.get("results"))
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
}

#[test]
fn place_with_margin_extending_past_outline_fails() {
    let _g = test_lock();
    let project = fresh_project("margin-edge");
    // 20×20 mm outline so we can position right against an edge.
    run_script(&project, "outline 20 20");
    make_part_with_margin(
        &project,
        "fat_part",
        PlacementMargin {
            top_mm: 0.0,
            right_mm: 5.0, // 5 mm overhang to the right beyond the pads
            bottom_mm: 0.0,
            left_mm: 0.0,
        },
    );
    run_script(
        &project,
        "sym J1 ic key=fat_part\n  pin 1 L\n  pin 2 R\npalette J1 fat_part\n",
    );

    // Pad bbox: x ∈ [-1.25, 1.25] before placement. Place at x = 17:
    // pads land x ∈ [15.75, 18.25] (inside outline 0..20) but the right
    // margin pushes inflated bbox to 23.25 → 3.25 mm past right edge.
    let reply = run_script(&project, "place J1 17 10\n");
    let results = extract_results(&reply);
    assert_eq!(results.len(), 1, "one place attempt: {reply:#?}");
    assert_eq!(
        results[0]["ok"], false,
        "expected place to fail; reply={reply:#?}"
    );
    let err = results[0]["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("right") && err.contains("body"),
        "error should mention right-side body overhang, got: {err}"
    );

    // The pad-bbox-only check still passes (the pads themselves fit),
    // so without the margin enforcement nothing landed — confirm.
    assert!(
        project.read().board().footprints.is_empty(),
        "no footprint should have been placed"
    );
}

#[test]
fn place_two_parts_whose_bodies_overlap_fails() {
    let _g = test_lock();
    let project = fresh_project("margin-overlap");
    run_script(&project, "outline 60 30");
    make_part_with_margin(
        &project,
        "wide_part",
        // 3 mm halo all around — small pads, but the body is huge.
        PlacementMargin {
            top_mm: 3.0,
            right_mm: 3.0,
            bottom_mm: 3.0,
            left_mm: 3.0,
        },
    );
    run_script(
        &project,
        "sym U1 ic key=wide_part\n  pin 1 L\n  pin 2 R\nsym U2 ic key=wide_part\n  pin 1 L\n  pin 2 R\npalette U1 wide_part\npalette U2 wide_part\n",
    );

    // U1 inflated bbox: x ∈ [-4.25, 4.25] + 15 = [10.75, 19.25].
    // U2 at x = 22 → pads x ∈ [20.75, 23.25] (gap ≥ 0.5 from U1 pads),
    // inflated to [17.75, 26.25] → overlaps U1's [10.75, 19.25] body.
    let reply = run_script(&project, "place U1 15 15\nplace U2 22 15\n");
    let results = extract_results(&reply);
    assert_eq!(results.len(), 2, "two place attempts: {reply:#?}");
    assert_eq!(results[0]["ok"], true, "U1 should place fine: {reply:#?}");
    assert_eq!(
        results[1]["ok"], false,
        "U2 must be rejected for body overlap: {reply:#?}"
    );
    let err = results[1]["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("body") && err.contains("U1"),
        "error should mention body overlap with U1, got: {err}"
    );
    assert_eq!(
        project.read().board().footprints.len(),
        1,
        "only U1 should have landed"
    );
}

#[test]
fn move_into_body_overlap_fails() {
    let _g = test_lock();
    let project = fresh_project("margin-move");
    run_script(&project, "outline 60 30");
    make_part_with_margin(
        &project,
        "wide_part",
        PlacementMargin {
            top_mm: 2.0,
            right_mm: 2.0,
            bottom_mm: 2.0,
            left_mm: 2.0,
        },
    );
    run_script(
        &project,
        "sym U1 ic key=wide_part\n  pin 1 L\n  pin 2 R\nsym U2 ic key=wide_part\n  pin 1 L\n  pin 2 R\npalette U1 wide_part\npalette U2 wide_part\nplace U1 10 15\nplace U2 25 15\n",
    );
    assert_eq!(project.read().board().footprints.len(), 2);

    // U1 body x ∈ [6.75, 13.25]. Move U2 to x = 16 → body x ∈ [12.75, 19.25] → overlap.
    let reply = run_script(&project, "move U2 16 15\n");
    let results = extract_results(&reply);
    assert_eq!(results.len(), 1, "one move attempt: {reply:#?}");
    assert_eq!(results[0]["ok"], false, "move should fail: {reply:#?}");
    let err = results[0]["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("body") && err.contains("U1"),
        "error should mention body overlap, got: {err}"
    );
    // U2 should still be where it was placed.
    let snap = project.read();
    let u2 = snap
        .board()
        .footprints
        .values()
        .find(|f| f.reference == "U2")
        .expect("U2");
    assert!(
        (u2.position.x.to_mm() - 25.0).abs() < 1e-6,
        "U2 must not have moved"
    );
}

#[test]
fn edge_mounted_does_not_exempt_body_off_board() {
    // An `edge_mounted=true` part still has a plastic body that has
    // to physically fit on the board. Touching the cut line is only
    // legal for the pads; if the inflated body bbox sticks past the
    // outline, placement must FAIL (no warning fallback).
    let _g = test_lock();
    let project = fresh_project("margin-edge-mounted-body");
    run_script(&project, "outline 20 20");
    // `edge=true` library entry with a 3 mm right-side body halo.
    run_script(
        &project,
        "lib usb_c edge=true\n  pad 1 -1 0 0.5 0.5\n  pad 2  1 0 0.5 0.5\n",
    );
    project
        .confirm_pending_library_entry("usb_c")
        .expect("confirm library entry");
    project
        .library()
        .set_placement_margin(
            "usb_c",
            PlacementMargin {
                top_mm: 0.0,
                right_mm: 3.0,
                bottom_mm: 0.0,
                left_mm: 0.0,
            },
        )
        .expect("set margin");
    run_script(
        &project,
        "sym J9 ic key=usb_c\n  pin 1 L\n  pin 2 R\npalette J9 usb_c\n",
    );

    // Place at x=18.75 so pad bbox right edge sits at the outline
    // (18.75 + 1.25 = 20.0). Pads themselves are fine and the part is
    // "touching the edge" — but the 3 mm body margin pushes the
    // inflated bbox to x=23.0, 3 mm past the right outline.
    let reply = run_script(&project, "place J9 18.75 10\n");
    let results = extract_results(&reply);
    assert_eq!(results.len(), 1, "one place attempt: {reply:#?}");
    assert_eq!(
        results[0]["ok"], false,
        "edge-mounted part with body overhang must still be rejected; reply={reply:#?}"
    );
    let err = results[0]["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("body") && err.contains("right"),
        "error should mention body overhanging the right edge, got: {err}"
    );
    assert!(
        project.read().board().footprints.is_empty(),
        "no footprint should have been placed"
    );
}

#[test]
fn view_snapshot_emits_body_outline_rect_for_margined_parts() {
    let _g = test_lock();
    let project = fresh_project("margin-render");
    run_script(&project, "outline 40 40");
    make_part_with_margin(
        &project,
        "screw_term",
        PlacementMargin {
            top_mm: 1.5,
            right_mm: 2.0,
            bottom_mm: 1.5,
            left_mm: 2.0,
        },
    );
    run_script(
        &project,
        "sym J1 ic key=screw_term\n  pin 1 L\n  pin 2 R\npalette J1 screw_term\nplace J1 15 20\n",
    );

    let reply = block_on(pcb_script::tools::dispatch(
        &project,
        "view.snapshot",
        &json!({}),
    ))
    .unwrap_or_else(|e| panic!("snapshot failed: {} ({})", e.message, e.code));
    let svg = reply
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .expect("snapshot text content");
    assert!(
        svg.contains(r#"data-body-outline="J1""#),
        "expected a body-outline rect attribute for J1 — svg snippet:\n{}",
        &svg[..svg.len().min(800)]
    );
}
