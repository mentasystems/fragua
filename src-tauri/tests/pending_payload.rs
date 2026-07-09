//! Regression test for the confirmation-modal payload
//! (`pending_entries_json`). Reproduces the shape the frontend modal
//! renders from for an entry that stresses the historically fragile
//! bits: staged photo attachments, non-numeric pad numbers (`MH1`), a
//! large silk body, and mixed drilled / SMD pads. Guards against a
//! serialization regression where an optional field (drill, mpn,
//! calibration, body_rect) or an odd pad number would produce a payload
//! the modal cannot render.

use std::future::Future;
use std::task::{Context, Poll, Waker};

use pcb_core::Project;
use serde_json::{json, Value};

fn block_on<F: Future>(fut: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut fut = Box::pin(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("test future stalled — handler should be sync"),
    }
}

#[test]
fn pending_payload_renders_photos_nonnumeric_pads_and_silk() {
    // Sandbox HOME so the library store lands in a temp dir.
    let tmp = std::env::temp_dir().join(format!("pcb-test-pending-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("mkdir tmp HOME");
    std::env::set_var("HOME", &tmp);

    // A tiny on-disk "photo" the `attach` verb can read (mime derives
    // from the .jpg extension, not the bytes).
    let photo = tmp.join("part.jpg");
    std::fs::write(&photo, b"\xff\xd8\xff\xe0not-a-real-jpeg").expect("write photo");
    let photo_path = photo.to_str().unwrap();

    let project = Project::new("pending-test");

    // Mirrors the real agent flow: create a library entry with a mix of
    // numbered + non-numeric (MH) pads, drills, and a big silk body,
    // then stage a photo on the pending entry.
    let script = format!(
        "lib oled_test edge=false desc=\"0.96in OLED, 4 header pads + 4 mounting holes\"\n  \
         pad 1 -3.81 0 1.7 1.7 name=GND drill=1.0\n  \
         pad 2 -1.27 0 1.7 1.7 name=VDD drill=1.0\n  \
         pad 3 1.27 0 1.7 1.7 name=SCK drill=1.0\n  \
         pad 4 3.81 0 1.7 1.7 name=SDA drill=1.0\n  \
         pad MH1 -10.15 -0.2 3.2 3.2 name=MTG drill=2.2\n  \
         pad MH2 10.15 -0.2 3.2 3.2 name=MTG drill=2.2\n  \
         pad MH3 -10.15 -24.0 3.2 3.2 name=MTG drill=2.2\n  \
         pad MH4 10.15 -24.0 3.2 3.2 name=MTG drill=2.2\n  \
         silk-line top -12.35 -25.88 12.35 -25.88\n  \
         silk-line top 12.35 -25.88 12.35 1.12\n  \
         silk-text top 0 -13 \"{{REF}}\" size=1.4\n\
         attach oled_test photo \"{photo_path}\"\n",
    );

    block_on(pcb_script::tools::dispatch(
        &project,
        "script",
        &json!({ "script": script }),
    ))
    .map_err(|e| e.message)
    .expect("run script");

    let payload: Value = fragua_lib::pending_entries_json(&project);

    // Dump for eyeball diagnosis (visible with `cargo test -- --nocapture`),
    // with the base64 photo bytes trimmed so the output stays readable.
    let mut dump = payload.clone();
    if let Some(atts) = dump["entries"][0]["attachments"].as_array_mut() {
        for a in atts {
            if let Some(uri) = a["data_uri"].as_str() {
                let head: String = uri.chars().take(40).collect();
                a["data_uri"] = json!(format!("{head}…<trimmed>"));
            }
        }
    }
    println!(
        "PENDING PAYLOAD:\n{}",
        serde_json::to_string_pretty(&dump).unwrap()
    );

    let entries = payload["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1, "one pending entry");
    let e = &entries[0];

    assert_eq!(e["key"], "oled_test");
    assert_eq!(e["pad_count"], 8, "4 header + 4 MH pads");

    // Every pad must serialize with the shape the modal reads.
    let pads = e["pads"].as_array().expect("pads array");
    assert_eq!(pads.len(), 8);
    for p in pads {
        assert!(p["number"].is_string(), "pad number is a string");
        assert!(p["name"].is_string());
        assert!(p["x_mm"].is_number());
        assert!(p["y_mm"].is_number());
        assert!(p["w_mm"].is_number());
        assert!(p["h_mm"].is_number());
        assert!(p["is_ground"].is_boolean());
        // drill is present (either a number or explicit null) — never absent.
        assert!(p.get("drill_mm").is_some(), "drill_mm key present");
    }
    // Non-numeric pad number survived verbatim.
    assert!(
        pads.iter().any(|p| p["number"] == "MH1"),
        "non-numeric pad number MH1 present"
    );
    // Drills applied.
    assert!(
        pads.iter().any(|p| p["drill_mm"] == json!(2.2)),
        "MH drill 2.2 present"
    );
    assert!(
        pads.iter().any(|p| p["drill_mm"] == json!(1.0)),
        "header drill 1.0 present"
    );

    // Attachment shape the modal's `attachments.find(a => a.mime...)` reads.
    let atts = e["attachments"].as_array().expect("attachments array");
    assert_eq!(atts.len(), 1, "one staged photo");
    let a = &atts[0];
    assert!(
        a["mime"].as_str().unwrap().starts_with("image/"),
        "photo mime is image/*"
    );
    assert!(a["data_uri"].as_str().unwrap().starts_with("data:image/"));
    assert!(a["filename"].is_string());
    assert!(a["kind"].is_string());

    // review_svg must be a non-empty SVG that actually draws all pads.
    let svg = e["review_svg"].as_str().expect("review_svg string");
    assert!(svg.starts_with("<svg"), "review_svg is an svg");
    let rect_count = svg.matches("<rect").count();
    assert!(
        rect_count >= 8,
        "review_svg should draw at least one rect per pad (got {rect_count})"
    );
    // The non-numeric pad label must be present in the SVG text.
    assert!(svg.contains(">MH1<"), "MH1 pad label rendered in svg");

    // Optional top-level fields the modal reads must at least be present
    // as keys (null is fine; absent is what breaks `a.foo.bar`).
    for k in [
        "description",
        "edge_mounted",
        "ground_pad_count",
        "lcsc_id",
        "mpn",
    ] {
        assert!(e.get(k).is_some(), "top-level key {k} present");
    }
}
