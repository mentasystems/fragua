//! Integration test for the `delete REF` script verb. Spawns a tiny
//! two-pad library entry, places one footprint, drops a manual trace
//! landing on its pad, then issues `delete REF` and asserts that both
//! the footprint and the connected trace are gone.
//!
//! `pcb_script::tools::dispatch` is async for the `script` / `batch`
//! entrypoints; the codepath here is sync — same hand-rolled single-
//! step executor pattern as `library_silk.rs`.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll, Wake, Waker};

use pcb_core::Project;
use serde_json::{json, Value};

/// Every test in this file sandboxes the on-disk library by mutating the
/// process-global `HOME` env var. Cargo runs a binary's tests on parallel
/// threads, so without serialisation one test's `set_var("HOME", ...)` can
/// land mid-flight through another's library save and the `index.json`
/// rename resolves to a sibling test's (not-yet-created) `.pcb-library/`.
/// Serialise behind a single mutex instead of forcing `--test-threads=1`.
/// Same guard shape as `placement_margin_validation.rs`.
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

fn run_script(project: &Project, script: &str) -> Value {
    block_on(pcb_script::tools::dispatch(
        project,
        "script",
        &json!({"script": script}),
    ))
    .map_err(|e| format!("{} ({})", e.message, e.code))
    .unwrap_or_else(|e| panic!("script failed: {e}\n--script--\n{script}"))
}

#[test]
fn delete_ref_removes_footprint_and_connected_traces() {
    let _guard = test_lock();
    // Sandbox the on-disk library to a temp HOME so the test does not
    // touch `~/.pcb-library/`.
    let tmp = std::env::temp_dir().join(format!("pcb-test-delete-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("mkdir tmp HOME");
    std::env::set_var("HOME", &tmp);

    let project = Project::new("delete-test");

    // Outline so placement isn't rejected for being off-board.
    run_script(&project, "outline 40 40");

    // Tiny two-pad library entry, then confirm it so palette can use it.
    run_script(
        &project,
        "lib test_two_pad\n  pad 1 -2 0 1 1\n  pad 2  2 0 1 1\n",
    );
    project
        .confirm_pending_library_entry("test_two_pad")
        .expect("confirm library entry");

    // Two symbols on a shared net so the pads carry a net once placed.
    run_script(
        &project,
        "sym U1 ic key=test_two_pad\n  pin 1 L\n  pin 2 R\nsym U2 ic key=test_two_pad\n  pin 1 L\n  pin 2 R\nnet SIG U1.2 U2.1\n",
    );

    // Spawn + place both footprints.
    run_script(
        &project,
        "palette U1 test_two_pad\npalette U2 test_two_pad\nplace U1 10 10\nplace U2 25 10\n",
    );

    // Confirm U1 + U2 actually sit on the board with two pads each.
    {
        let snap = project.read();
        let board = snap.board();
        assert_eq!(board.footprints.len(), 2);
    }

    // Drop a manual trace that lands on U1.pad2 world position (12, 10)
    // and on U2.pad1 world position (23, 10). Net SIG.
    run_script(&project, "trace top SIG 12 10 23 10\n");
    assert_eq!(project.read().board().traces.len(), 1);

    // Now the actual delete.
    run_script(&project, "delete U1\n");

    let snap = project.read();
    let board = snap.board();
    assert_eq!(
        board.footprints.len(),
        1,
        "U1 should be gone, U2 stays — got {:?}",
        board
            .footprints
            .values()
            .map(|f| &f.reference)
            .collect::<Vec<_>>()
    );
    assert!(
        board.footprints.values().all(|f| f.reference != "U1"),
        "U1 must be removed"
    );
    assert!(
        board.traces.is_empty(),
        "the SIG trace landed on a U1 pad — should have been cleaned up"
    );
}

#[test]
fn delete_unknown_ref_errors() {
    let _guard = test_lock();
    let tmp = std::env::temp_dir().join(format!("pcb-test-delete-err-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("mkdir tmp HOME");
    std::env::set_var("HOME", &tmp);

    let project = Project::new("delete-err");
    // Issuing `delete` on an empty board must surface a clear error in
    // the script reply rather than silently succeeding. `dispatch`
    // returns a `ToolError` which our caller turns into a per-line FAIL
    // — but at the API surface we test the Err arm directly.
    let result = block_on(pcb_script::tools::dispatch(
        &project,
        "placement.delete",
        &json!({"refs": ["U99"]}),
    ));
    assert!(result.is_err(), "expected an error for missing ref");
    let err = result.err().unwrap();
    assert!(
        err.message.contains("U99"),
        "error message should name the missing ref, got: {}",
        err.message,
    );
}

#[test]
fn clear_board_drops_every_placement() {
    let _guard = test_lock();
    let tmp = std::env::temp_dir().join(format!("pcb-test-clear-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("mkdir tmp HOME");
    std::env::set_var("HOME", &tmp);

    let project = Project::new("clear-test");
    run_script(&project, "outline 40 40");
    run_script(
        &project,
        "lib test_two_pad\n  pad 1 -2 0 1 1\n  pad 2  2 0 1 1\n",
    );
    project
        .confirm_pending_library_entry("test_two_pad")
        .expect("confirm library entry");
    run_script(
        &project,
        "sym U1 ic key=test_two_pad\n  pin 1 L\n  pin 2 R\nsym U2 ic key=test_two_pad\n  pin 1 L\n  pin 2 R\n",
    );
    run_script(
        &project,
        "palette U1 test_two_pad\npalette U2 test_two_pad\nplace U1 10 10\nplace U2 25 10\ntrace top GND 5 5 6 6\n",
    );
    assert_eq!(project.read().board().footprints.len(), 2);
    assert_eq!(project.read().board().traces.len(), 1);

    run_script(&project, "clear-board\n");
    let snap = project.read();
    let board = snap.board();
    assert!(board.footprints.is_empty(), "footprints should be empty");
    assert!(board.traces.is_empty(), "traces should be empty");
}
