//! Integration tests for the `calibrate-photo` and `body-rect` script
//! verbs. They cover:
//!
//! - calibrate-photo on a confirmed entry reports the derived scale and
//!   implied photo width, and list-lib flags the attachment `calibrated`.
//! - calibrate-photo with an ambiguous attachment token errors, listing
//!   the candidates.
//! - calibrate-photo / body-rect on a still-PENDING entry are refused
//!   with a "confirm it first" message (photos have no ids and body rects
//!   don't persist until the entry lands on disk).
//! - body-rect on a confirmed entry stores the rect, derives the per-side
//!   placement margin, and list-lib flags `has_body_rect`;
//!   `body-rect KEY clear` drops it again.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll, Wake, Waker};

use pcb_core::Project;
use serde_json::{json, Value};

/// See `placement_margin_validation.rs`: Project::new mutates the shared
/// `HOME` env var, so serialise the HOME-touching tests behind one mutex.
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
        &json!({ "script": script }),
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

/// Drill into the `script` tool result for the per-line outcome list.
fn extract_results(reply: &Value) -> Vec<Value> {
    reply
        .get("structuredContent")
        .and_then(|d| d.get("results"))
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Minimal 24-bpp BMP header (54 bytes) with the given pixel dimensions.
/// `imagesize` reads dimensions from the header alone (content-sniffed,
/// extension-independent), so no pixel data is needed.
fn bmp(width: i32, height: i32) -> Vec<u8> {
    let mut b = vec![0u8; 54];
    b[0] = b'B';
    b[1] = b'M';
    let size = 54u32 + (width as u32 * height as u32 * 3);
    b[2..6].copy_from_slice(&size.to_le_bytes());
    b[10..14].copy_from_slice(&54u32.to_le_bytes()); // pixel-data offset
    b[14..18].copy_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
    b[18..22].copy_from_slice(&width.to_le_bytes());
    b[22..26].copy_from_slice(&height.to_le_bytes());
    b[26..28].copy_from_slice(&1u16.to_le_bytes()); // planes
    b[28..30].copy_from_slice(&24u16.to_le_bytes()); // bpp
    b
}

/// Write a BMP into the temp dir and return its absolute path (as a
/// space-free string usable as a script token).
fn write_photo(name: &str, width: i32, height: i32) -> String {
    let path = std::env::temp_dir().join(format!(
        "pcb-photo-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&path, bmp(width, height)).expect("write bmp");
    path.to_str().expect("utf8 path").to_string()
}

/// Two-pad entry, queued then confirmed, with `count` copies of a photo
/// attached (all named `top.bmp`). Pads sit at x = ±1 mm, so a 100 px
/// mark span maps to 2 mm → scale 0.02 mm/px.
fn make_confirmed_part_with_photos(project: &Project, key: &str, count: usize) -> String {
    let photo = write_photo("top.bmp", 100, 40);
    let mut script = format!("lib {key}\n  pad 1 -1 0 0.5 0.5\n  pad 2 1 0 0.5 0.5\n");
    for _ in 0..count {
        script.push_str(&format!("attach {key} photo {photo}\n"));
    }
    run_script(project, &script);
    project
        .confirm_pending_library_entry(key)
        .expect("confirm library entry");
    photo
}

/// Basename of a path — the filename `attach` records for the token.
fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .expect("basename")
        .to_string()
}

fn lib_entry<'a>(entries: &'a [Value], key: &str) -> &'a Value {
    entries
        .iter()
        .find(|e| e["key"] == json!(key))
        .unwrap_or_else(|| panic!("no {key} in list-lib: {entries:#?}"))
}

fn list_lib_entries(project: &Project) -> Vec<Value> {
    let reply = block_on(pcb_script::tools::dispatch(
        project,
        "library.list",
        &json!({}),
    ))
    .unwrap_or_else(|e| panic!("library.list failed: {} ({})", e.message, e.code));
    reply
        .get("structuredContent")
        .and_then(|d| d.get("entries"))
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default()
}

#[test]
fn calibrate_photo_on_confirmed_entry_reports_scale_and_width() {
    let _g = test_lock();
    let project = fresh_project("cal-ok");
    let photo = make_confirmed_part_with_photos(&project, "cam", 1);
    let att = basename(&photo);

    // Pads 2 mm apart, marks 100 px apart → 0.02 mm/px; 100 px wide photo
    // → 2.0 mm implied width. Resolve the attachment by filename.
    let reply = run_script(
        &project,
        &format!("calibrate-photo cam {att} 10 20 110 20 1 2\n"),
    );
    let results = extract_results(&reply);
    assert_eq!(results.len(), 1, "one calibrate attempt: {reply:#?}");
    assert_eq!(
        results[0]["ok"], true,
        "calibrate should succeed: {reply:#?}"
    );
    let data = &results[0]["result"]["structuredContent"];
    let scale = data["scale_mm_per_px"].as_f64().expect("scale");
    assert!((scale - 0.02).abs() < 1e-9, "scale mm/px, got {scale}");
    let width_mm = data["photo_width_mm"].as_f64().expect("photo_width_mm");
    assert!(
        (width_mm - 2.0).abs() < 1e-6,
        "implied width mm, got {width_mm}"
    );
    assert_eq!(data["photo_width_px"], json!(100));

    // list-lib now flags the attachment as calibrated.
    let entries = list_lib_entries(&project);
    let cam = lib_entry(&entries, "cam");
    assert_eq!(
        cam["attachments"][0]["calibrated"],
        json!(true),
        "attachment should be flagged calibrated: {cam:#?}"
    );
}

#[test]
fn calibrate_photo_ambiguous_attachment_lists_candidates() {
    let _g = test_lock();
    let project = fresh_project("cal-ambig");
    // Two attachments sharing one filename → the filename token matches
    // both.
    let photo = make_confirmed_part_with_photos(&project, "cam", 2);
    let att = basename(&photo);

    let reply = run_script(
        &project,
        &format!("calibrate-photo cam {att} 10 20 110 20 1 2\n"),
    );
    let results = extract_results(&reply);
    assert_eq!(
        results[0]["ok"], false,
        "ambiguous token must fail: {reply:#?}"
    );
    let err = results[0]["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("ambiguous") && err.contains("candidates") && err.contains(&att),
        "error should list candidates, got: {err}"
    );
}

#[test]
fn calibrate_photo_on_pending_entry_is_refused() {
    let _g = test_lock();
    let project = fresh_project("cal-pending");
    let photo = write_photo("top.bmp", 100, 40);
    // Queue + attach, but do NOT confirm: the staged photo has no id yet.
    run_script(
        &project,
        &format!("lib cam\n  pad 1 -1 0 0.5 0.5\n  pad 2 1 0 0.5 0.5\nattach cam photo {photo}\n"),
    );

    let reply = run_script(&project, "calibrate-photo cam top.bmp 10 20 110 20 1 2\n");
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

#[test]
fn body_rect_sets_margin_and_clear_drops_it() {
    let _g = test_lock();
    let project = fresh_project("body-ok");
    make_confirmed_part_with_photos(&project, "cam", 1);

    // Pad bbox: x ∈ [-1.25, 1.25], y ∈ [-0.25, 0.25]. Body 6×5 mm centred
    // → margin: top/bottom = 2.5 - 0.25 = 2.25, left/right = 3 - 1.25 = 1.75.
    let reply = run_script(&project, "body-rect cam -3 -2.5 3 2.5\n");
    let results = extract_results(&reply);
    assert_eq!(
        results[0]["ok"], true,
        "body-rect should succeed: {reply:#?}"
    );
    let data = &results[0]["result"]["structuredContent"];
    assert!((data["width_mm"].as_f64().unwrap() - 6.0).abs() < 1e-9);
    assert!((data["height_mm"].as_f64().unwrap() - 5.0).abs() < 1e-9);
    let m = &data["placement_margin"];
    assert!(
        (m["top_mm"].as_f64().unwrap() - 2.25).abs() < 1e-9,
        "top margin: {m:#?}"
    );
    assert!(
        (m["right_mm"].as_f64().unwrap() - 1.75).abs() < 1e-9,
        "right margin: {m:#?}"
    );
    assert!(
        (m["bottom_mm"].as_f64().unwrap() - 2.25).abs() < 1e-9,
        "bottom margin: {m:#?}"
    );
    assert!(
        (m["left_mm"].as_f64().unwrap() - 1.75).abs() < 1e-9,
        "left margin: {m:#?}"
    );

    // list-lib flags the entry, and the core derived the same margin.
    let entries = list_lib_entries(&project);
    assert_eq!(lib_entry(&entries, "cam")["has_body_rect"], json!(true));
    let margin = project.library().find("cam").unwrap().placement_margin;
    assert!((margin.top_mm - 2.25).abs() < 1e-9);
    assert!((margin.left_mm - 1.75).abs() < 1e-9);

    // Clear drops the rect (margin is intentionally left as-is by core).
    let reply = run_script(&project, "body-rect cam clear\n");
    assert_eq!(
        extract_results(&reply)[0]["ok"],
        true,
        "clear should succeed: {reply:#?}"
    );
    let entries = list_lib_entries(&project);
    assert_eq!(lib_entry(&entries, "cam")["has_body_rect"], json!(false));
    assert!(lib_entry(&entries, "cam")["body_rect"].is_null());
}

#[test]
fn body_rect_on_pending_entry_is_refused() {
    let _g = test_lock();
    let project = fresh_project("body-pending");
    run_script(
        &project,
        "lib cam\n  pad 1 -1 0 0.5 0.5\n  pad 2 1 0 0.5 0.5\n",
    );

    let reply = run_script(&project, "body-rect cam -3 -2.5 3 2.5\n");
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
