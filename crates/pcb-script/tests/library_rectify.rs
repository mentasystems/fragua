//! Integration tests for the `rectify-photo` script verb. They cover:
//!
//! - rectify-photo on a confirmed, calibrated entry produces a new
//!   `photo-rectified` attachment at the fixed px/mm, and auto-remaps the
//!   calibration axis-aligned (small residual).
//! - the degenerate-quad (collinear corners) case is refused and no new
//!   attachment is added.
//! - rectify-photo on a still-PENDING entry is refused with a "confirm it
//!   first" message (same gate as calibrate-photo / body-rect).
//!
//! Mirrors the `library_calibrate_body.rs` harness (hand-rolled executor
//! + fresh temp-HOME project), but synthesises a real decodable PNG since
//! rectification actually decodes/warps the pixels.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll, Wake, Waker};

use pcb_core::Project;
use serde_json::{json, Value};

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
        &json!({ "script": script }),
    ))
    .unwrap_or_else(|e| {
        panic!(
            "script failed: {} ({})\n--script--\n{script}",
            e.message, e.code
        )
    })
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

fn extract_results(reply: &Value) -> Vec<Value> {
    reply
        .get("structuredContent")
        .and_then(|d| d.get("results"))
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Write a real (decodable) grey PNG of the given size into the temp dir
/// and return its absolute path.
fn write_png(name: &str, w: u32, h: u32) -> String {
    let img = image::RgbImage::from_pixel(w, h, image::Rgb([128, 128, 128]));
    let path = std::env::temp_dir().join(format!(
        "pcb-rectphoto-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    img.save_with_format(&path, image::ImageFormat::Png)
        .expect("write png");
    path.to_str().expect("utf8 path").to_string()
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .expect("basename")
        .to_string()
}

/// Confirmed two-pad entry with one 400×400 photo attached. Pads sit at
/// x = ±4 mm; the board is 10×10 mm filling the image (40 px/mm), so the
/// pads land at image (40,200) and (360,200).
fn make_calibrated_part(project: &Project, key: &str) -> String {
    let photo = write_png("top.png", 400, 400);
    let script = format!(
        "lib {key}\n  pad 1 -4 0 0.5 0.5\n  pad 2 4 0 0.5 0.5\nattach {key} photo {photo}\n"
    );
    run_script(project, &script);
    project
        .confirm_pending_library_entry(key)
        .expect("confirm library entry");
    // Calibrate the plain photo: 8 mm ↔ 320 px → 0.025 mm/px.
    run_script(
        project,
        &format!(
            "calibrate-photo {key} {} 40 200 360 200 1 2\n",
            basename(&photo)
        ),
    );
    basename(&photo)
}

#[test]
fn rectify_photo_produces_rectified_attachment_and_remaps_calibration() {
    let _g = test_lock();
    let project = fresh_project("rect-ok");
    let att = make_calibrated_part(&project, "cam");

    // Full-image quad in TL,TR,BR,BL order (already axis aligned).
    let reply = run_script(
        &project,
        &format!("rectify-photo cam {att} 0 0 400 0 400 400 0 400 10 10\n"),
    );
    let results = extract_results(&reply);
    assert_eq!(results.len(), 1, "one rectify attempt: {reply:#?}");
    assert_eq!(results[0]["ok"], true, "rectify should succeed: {reply:#?}");
    let data = &results[0]["result"]["structuredContent"];
    assert_eq!(data["width_px"], json!(400));
    assert_eq!(data["height_px"], json!(400));
    assert!((data["px_per_mm"].as_f64().unwrap() - 40.0).abs() < 1e-9);
    let cal = &data["calibration"];
    assert_eq!(
        cal["remapped"],
        json!(true),
        "calibration remapped: {data:#?}"
    );
    let scale = cal["scale_mm_per_px"].as_f64().expect("scale");
    assert!((scale - 0.025).abs() < 1e-3, "rectified scale, got {scale}");
    let residual = cal["residual_deg"].as_f64().expect("residual");
    assert!(
        residual < 0.5,
        "residual should be near-zero, got {residual}"
    );

    // The new attachment lands on the entry as "photo-rectified", calibrated.
    let entry = project.library().find("cam").expect("entry");
    let new_id = data["attachment_id"].as_str().expect("id");
    let new = entry
        .attachments
        .iter()
        .find(|a| a.id == new_id)
        .expect("rectified attachment present");
    assert_eq!(new.kind, "photo-rectified");
    assert!(
        new.filename.ends_with("-rect.jpg"),
        "filename: {}",
        new.filename
    );
    assert!(
        new.calibration.is_some(),
        "rectified attachment carries calibration"
    );
}

#[test]
fn rectify_photo_rejects_degenerate_quad() {
    let _g = test_lock();
    let project = fresh_project("rect-bad");
    let att = make_calibrated_part(&project, "cam");
    let before = project.library().find("cam").unwrap().attachments.len();

    // Collinear top edge → degenerate quad.
    let reply = run_script(
        &project,
        &format!("rectify-photo cam {att} 0 0 200 0 400 0 200 100 10 10\n"),
    );
    let results = extract_results(&reply);
    assert_eq!(
        results[0]["ok"], false,
        "degenerate quad must fail: {reply:#?}"
    );
    let err = results[0]["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("rectify"),
        "error should mention rectify: {err}"
    );
    // No attachment was added.
    assert_eq!(
        project.library().find("cam").unwrap().attachments.len(),
        before
    );
}

#[test]
fn rectify_photo_on_pending_entry_is_refused() {
    let _g = test_lock();
    let project = fresh_project("rect-pending");
    let photo = write_png("top.png", 400, 400);
    // Queue + attach, but do NOT confirm.
    run_script(
        &project,
        &format!("lib cam\n  pad 1 -4 0 0.5 0.5\n  pad 2 4 0 0.5 0.5\nattach cam photo {photo}\n"),
    );
    let reply = run_script(
        &project,
        &format!(
            "rectify-photo cam {} 0 0 400 0 400 400 0 400 10 10\n",
            basename(&photo)
        ),
    );
    let results = extract_results(&reply);
    assert_eq!(
        results[0]["ok"], false,
        "pending entry must be refused: {reply:#?}"
    );
    let err = results[0]["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("pending") && err.contains("confirm"),
        "error should tell the agent to confirm first, got: {err}"
    );
}
