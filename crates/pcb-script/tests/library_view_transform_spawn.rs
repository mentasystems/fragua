//! End-to-end test: when a library entry carries a non-identity
//! `footprint_view_transform`, the spawned palette footprint (and the
//! placed footprint) must reflect that transform in its pad / silk
//! geometry. The library entry itself stays unchanged on disk so the
//! review pane can keep driving off the "native" coords with a CSS
//! transform.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll, Wake, Waker};

use pcb_core::{Project, ViewTransform};
use serde_json::json;

// HOME is process-global, so the test sandbox setup must be serialised.
// Tests grab this lock for their full body. Cargo can still parallelise
// across crates, but within this crate the three view-transform tests
// run one at a time.
fn home_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
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

fn sandbox_home(label: &str) {
    let tmp = std::env::temp_dir().join(format!(
        "pcb-test-vt-{}-{}-{}",
        label,
        std::process::id(),
        // Differentiate per-test invocation so tests can run in parallel
        // without trampling each other's ~/.pcb-library/.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("mkdir tmp HOME");
    std::env::set_var("HOME", &tmp);
}

fn create_two_pad_entry(project: &Project, key: &str) {
    let create_args = json!({
        "key": key,
        "description": "two-pad part for view-transform test",
        "pads": [
            // Pad "1" at the +x side, pad "2" at the -x side. A flip_h
            // should swap their x signs.
            {"number": "1", "x_mm":  2.0, "y_mm": 0.0, "w_mm": 1.0, "h_mm": 1.5},
            {"number": "2", "x_mm": -2.0, "y_mm": 0.0, "w_mm": 1.0, "h_mm": 1.5},
        ],
    });
    block_on(pcb_script::tools::dispatch(
        project,
        "library.create",
        &create_args,
    ))
    .map_err(|e| e.message)
    .expect("library.create");
    project
        .confirm_pending_library_entry(key)
        .expect("confirm pending entry");
}

fn add_symbol(project: &Project, reference: &str) {
    block_on(pcb_script::tools::dispatch(
        project,
        "schematic.add_symbol",
        &json!({"reference": reference, "kind": "resistor"}),
    ))
    .map_err(|e| e.message)
    .expect("schematic.add_symbol");
}

fn spawn(project: &Project, reference: &str, key: &str) {
    block_on(pcb_script::tools::dispatch(
        project,
        "palette.add_from_library",
        &json!({"reference": reference, "key": key}),
    ))
    .map_err(|e| e.message)
    .expect("palette.add_from_library");
}

#[test]
fn flip_h_mirrors_pad_x_at_palette_spawn() {
    let _g = home_guard();
    sandbox_home("flip-h");
    let project = Project::new("vt-flip-h");
    create_two_pad_entry(&project, "test_flip_h");

    // Apply a horizontal flip to the entry's footprint_view_transform.
    project
        .library()
        .set_footprint_view_transform(
            "test_flip_h",
            ViewTransform {
                rotation_deg: 0,
                flip_h: true,
                flip_v: false,
            },
        )
        .expect("set view transform");

    add_symbol(&project, "U1");
    spawn(&project, "U1", "test_flip_h");

    // The original library entry should still have its native coords —
    // index.json must not change behind the user's back.
    let entry = project
        .library()
        .find("test_flip_h")
        .expect("entry stored");
    assert_eq!(entry.pads[0].x_mm, 2.0, "library entry stays native");
    assert_eq!(entry.pads[1].x_mm, -2.0, "library entry stays native");

    let snap = project.read();
    let fp = snap
        .palette()
        .iter()
        .find(|f| f.reference == "U1")
        .expect("U1 in palette");

    // Pad "1" was at +x in the library; after flip_h it should sit at -x
    // in the spawned footprint. Pad "2" mirrors the other way.
    let pad1 = fp.pads.iter().find(|p| p.number == "1").expect("pad 1");
    let pad2 = fp.pads.iter().find(|p| p.number == "2").expect("pad 2");
    assert!(
        (pad1.offset.x.to_mm() - (-2.0)).abs() < 1e-6,
        "pad 1 x should be -2.0, got {}",
        pad1.offset.x.to_mm()
    );
    assert!(
        (pad2.offset.x.to_mm() - 2.0).abs() < 1e-6,
        "pad 2 x should be 2.0, got {}",
        pad2.offset.x.to_mm()
    );
    // Y untouched.
    assert!(pad1.offset.y.to_mm().abs() < 1e-6);
    // Sizes unchanged (flip alone doesn't swap w/h).
    assert!((pad1.size.0.to_mm() - 1.0).abs() < 1e-6);
    assert!((pad1.size.1.to_mm() - 1.5).abs() < 1e-6);
}

#[test]
fn rotation_270_swaps_dimensions_and_rotates_pad_offsets() {
    let _g = home_guard();
    sandbox_home("rot270");
    let project = Project::new("vt-rot270");
    create_two_pad_entry(&project, "test_rot270");

    project
        .library()
        .set_footprint_view_transform(
            "test_rot270",
            ViewTransform {
                rotation_deg: 270,
                flip_h: false,
                flip_v: false,
            },
        )
        .expect("set view transform");

    add_symbol(&project, "U1");
    spawn(&project, "U1", "test_rot270");

    let snap = project.read();
    let fp = snap
        .palette()
        .iter()
        .find(|f| f.reference == "U1")
        .expect("U1 in palette");

    // Pad "1" at (2, 0) rotated 270° CCW → (0, -2).
    let pad1 = fp.pads.iter().find(|p| p.number == "1").expect("pad 1");
    assert!((pad1.offset.x.to_mm() - 0.0).abs() < 1e-6);
    assert!((pad1.offset.y.to_mm() - (-2.0)).abs() < 1e-6);
    // 90° / 270° rotation swaps the pad dimensions.
    assert!((pad1.size.0.to_mm() - 1.5).abs() < 1e-6);
    assert!((pad1.size.1.to_mm() - 1.0).abs() < 1e-6);
}

#[test]
fn view_rotation_composes_with_place_rotation() {
    // Edge case: view_transform rotates 270° CCW and place rotates the
    // footprint by another -90° (i.e. 270° CCW). The net effect on the
    // world-coords of pad 1 (native library offset = (2, 0)) should be
    // a 540° CCW rotation == 180° rotation: world pad 1 lands at the
    // -x side of the place position.
    let _g = home_guard();
    sandbox_home("compose");
    let project = Project::new("vt-compose");

    // Outline so placement passes the inside-board check.
    block_on(pcb_script::tools::dispatch(
        &project,
        "board.set_outline",
        &json!({"w_mm": 50.0, "h_mm": 50.0}),
    ))
    .map_err(|e| e.message)
    .expect("board.set_outline");

    create_two_pad_entry(&project, "test_compose");
    project
        .library()
        .set_footprint_view_transform(
            "test_compose",
            ViewTransform {
                rotation_deg: 270,
                flip_h: false,
                flip_v: false,
            },
        )
        .expect("set view transform");

    add_symbol(&project, "U1");
    spawn(&project, "U1", "test_compose");

    // Place at (10, 25) with rotation -90° (CCW in Fragua's convention).
    block_on(pcb_script::tools::dispatch(
        &project,
        "placement.batch",
        &json!({"items": [{"reference": "U1", "x_mm": 10.0, "y_mm": 25.0, "rotation": -90.0}]}),
    ))
    .map_err(|e| e.message)
    .expect("placement.batch");

    let snap = project.read();
    let board = snap.board();
    let fp = board
        .footprints
        .values()
        .find(|f| f.reference == "U1")
        .expect("U1 placed");

    // Local pad after view transform (rot 270° CCW): (2, 0) → (0, -2).
    // Then place rotation -90° CCW (≡ 270° CCW) takes (0, -2) → (-2, 0).
    // World = place position + rotated offset = (10 - 2, 25 + 0) = (8, 25).
    let pad1 = fp.pads.iter().find(|p| p.number == "1").expect("pad 1");
    let world = fp.pad_world_center(pad1);
    assert!(
        (world.x.to_mm() - 8.0).abs() < 1e-3,
        "expected world x=8, got {}",
        world.x.to_mm()
    );
    assert!(
        (world.y.to_mm() - 25.0).abs() < 1e-3,
        "expected world y=25, got {}",
        world.y.to_mm()
    );
}
