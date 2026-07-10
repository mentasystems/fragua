//! User-driven component library.
//!
//! The agent populates this store at runtime: when the user shows it a
//! component (photo, datasheet, breakout module, …), the agent calls
//! `library.create` with a structured pad list + description, optionally
//! attaches the source photo / datasheet via `library.attach`, and
//! later spawns palette items by key with `palette.add_from_library`.
//!
//! On-disk layout (under `~/.pcb-library/`):
//!
//!   index.json
//!   attachments/<uuid>.<ext>
//!
//! Persistence is best-effort: every mutation writes the index back to
//! disk; errors are surfaced to the caller as `String`. The index lives
//! inside an `RwLock` so reads are cheap and writes serial.
//!
//! The store is process-global — one library is shared across every
//! `Project` opened on the same machine.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::board::{SilkAnchor, SilkLayer};

/// Silk primitive authored by a library entry. Coordinates are in
/// footprint-local millimetres (no rotation has been applied yet) and
/// angles in degrees CCW. Library data stays in plain f64/`String`
/// rather than the canonical `Length` type so `index.json` reads as
/// human-friendly mm — only the runtime `Footprint::silk` projection
/// converts it into the nanometre-fixed-point board model.
///
/// `Text` placeholders (`{REF}` / `{VAL}`) are resolved at render and
/// Gerber time by the host footprint, so a single library line can
/// produce a per-instance label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LibrarySilk {
    Line {
        layer: SilkLayer,
        x1_mm: f64,
        y1_mm: f64,
        x2_mm: f64,
        y2_mm: f64,
        width_mm: f64,
    },
    Text {
        layer: SilkLayer,
        x_mm: f64,
        y_mm: f64,
        text: String,
        size_mm: f64,
        #[serde(default)]
        rotation_deg: f32,
        #[serde(default)]
        anchor: SilkAnchor,
        width_mm: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LibraryPad {
    pub number: String,
    #[serde(default)]
    pub name: String,
    pub x_mm: f64,
    pub y_mm: f64,
    pub w_mm: f64,
    pub h_mm: f64,
    /// Optional plated through-hole drill diameter, mm. `None` = SMD.
    /// `Some(d)` = perforated pad (hybrid SMD + PTH).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drill_mm: Option<f64>,
}

/// Orientation tweak authored in the review UI when the stored image /
/// footprint doesn't quite match how the user wants to see it (e.g. a
/// photo taken upside-down, or a footprint whose native pad geometry
/// faces the "wrong" way relative to how the user expects to drop it
/// on a board).
///
/// For `Attachment::view_transform` this is purely visual: the frontend
/// multiplies a CSS transform onto the `<img>` and that's it.
///
/// For `LibraryEntry::footprint_view_transform` this carries semantic
/// weight: the review pane still uses a CSS transform on the rendered
/// SVG (so the entry's `pads` stay the "native" geometry in
/// `index.json`), AND the palette spawn path (`palette KEY` /
/// `palette.add_from_library`) re-applies the same transform to the
/// pad offsets / sizes / silk of the instantiated `Footprint`. That
/// way, what the user sees in the review pane is what gets placed on
/// the board. The transform composes with the per-placement rotation
/// from `place X Y ROT`: the view transform is the canonical
/// orientation of the part, the place rotation is then layered on top
/// (so `place X Y -90` rotates whatever the view transform produced by
/// a further -90°). Default = identity.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ViewTransform {
    /// 0, 90, 180 or 270. Anything else is treated as modulo 360. The
    /// number matches a visual CCW rotation in the review pane (which
    /// in turn maps to a CCW rotation of the footprint-local pad
    /// coordinates when applied at spawn time, since Fragua's world
    /// uses Y-up).
    #[serde(default)]
    pub rotation_deg: u16,
    #[serde(default)]
    pub flip_h: bool,
    #[serde(default)]
    pub flip_v: bool,
}

impl ViewTransform {
    /// True when this transform is the identity — no flip, no rotation
    /// (modulo 360). Callers can skip the transform pipeline when this
    /// is true to keep the common case allocation-free.
    #[must_use]
    pub fn is_identity(self) -> bool {
        !self.flip_h && !self.flip_v && self.rotation_deg.is_multiple_of(360)
    }

    /// Apply the transform to a footprint-local point in mm, in the
    /// convention "flip first, then rotate" (matching how CSS composes
    /// `rotate(R) scaleX(sx) scaleY(sy)` right-to-left when projecting
    /// the SVG in the review pane). Returns the transformed (x, y).
    ///
    /// Library pad coords are footprint-local Y-up millimetres, so a
    /// `rotation_deg` of 90 rotates the point 90° CCW in Y-up — which
    /// matches the visual CCW rotation the user dialled in via the
    /// review UI (CSS `rotate(R)` looks CW in screen Y-down, which is
    /// CCW once the SVG's outer `scale(1,-1)` flips back to Y-up).
    #[must_use]
    pub fn apply_point_mm(self, x: f64, y: f64) -> (f64, f64) {
        let mut x = x;
        let mut y = y;
        if self.flip_h {
            x = -x;
        }
        if self.flip_v {
            y = -y;
        }
        match self.rotation_deg % 360 {
            0 => (x, y),
            90 => (-y, x),
            180 => (-x, -y),
            270 => (y, -x),
            r => {
                // Off-axis rotations are not produced by the UI but
                // serde won't reject them either — handle them with a
                // proper trig fallback so we never silently drop the
                // rotation.
                let theta = f64::from(r).to_radians();
                let (sin, cos) = (theta.sin(), theta.cos());
                (x * cos - y * sin, x * sin + y * cos)
            }
        }
    }

    /// Apply the transform to a rectangular size in mm. A 90° / 270°
    /// rotation swaps width and height; flips leave it alone (rectangles
    /// are symmetric). Off-axis rotations also fall back to a swap when
    /// they land in the 90° / 270° quadrant, matching
    /// `Footprint::pad_world_size`.
    #[must_use]
    pub fn apply_size_mm(self, w: f64, h: f64) -> (f64, f64) {
        let r = u32::from(self.rotation_deg % 360);
        if (45..135).contains(&r) || (225..315).contains(&r) {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// Apply the transform to an angle in degrees CCW (used by silk
    /// text rotation). Flips conceptually mirror the angle:
    /// `flip_h` negates the angle (a +30° tilt becomes -30°),
    /// `flip_v` also negates the angle (the y-mirror flips handedness
    /// the same way). Applying both is a 180° rotation of the angle
    /// space, which is the identity on angles mod 360. The view's
    /// `rotation_deg` then adds on top.
    #[must_use]
    pub fn apply_angle_deg(self, angle: f32) -> f32 {
        let mut a = angle;
        if self.flip_h {
            a = -a;
        }
        if self.flip_v {
            a = -a;
        }
        a + f32::from(self.rotation_deg % 360)
    }

    /// Flatten to an SVG-style affine `[a, b, c, d, 0, 0]` (no
    /// translation) that maps a footprint-local mm point through this
    /// view transform, matching `apply_point_mm`. Built from the images
    /// of the basis vectors so it stays in lock-step with the tested
    /// `apply_point_mm` logic. Used when composing the view transform
    /// onto a calibrated photo overlay so the photo tracks the placed
    /// (view-transformed) pads.
    #[must_use]
    pub fn to_affine_mm(self) -> [f64; 6] {
        let (a, b) = self.apply_point_mm(1.0, 0.0);
        let (c, d) = self.apply_point_mm(0.0, 1.0);
        [a, b, c, d, 0.0, 0.0]
    }
}

/// Per-side keep-out around a footprint, in mm, used by the placer's
/// gap penalty / overlap check. Pads + silk are NOT moved by this; the
/// margin only inflates the bounding box the placer sees, so adjacent
/// components stay further away. Default = all zeros.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct PlacementMargin {
    #[serde(default)]
    pub top_mm: f64,
    #[serde(default)]
    pub right_mm: f64,
    #[serde(default)]
    pub bottom_mm: f64,
    #[serde(default)]
    pub left_mm: f64,
}

impl PlacementMargin {
    /// True when every side is zero (or negative — treated as no
    /// inflation). Callers can skip the rotated-inflate maths in the
    /// common case.
    #[must_use]
    pub fn is_zero(self) -> bool {
        self.top_mm <= 0.0 && self.right_mm <= 0.0 && self.bottom_mm <= 0.0 && self.left_mm <= 0.0
    }

    /// Pack the margin as the placer's `[top, right, bottom, left]`
    /// array so callers can share the same rotation helper.
    #[must_use]
    pub fn as_trbl_mm(self) -> [f64; 4] {
        [self.top_mm, self.right_mm, self.bottom_mm, self.left_mm]
    }
}

/// Two-point photo calibration recorded in the review UI. The user
/// marks two pin centres on a top-down photo of the module and says
/// which pads they are; from those correspondences (plus the entry's
/// own pad offsets) Fragua derives the similarity transform that maps
/// raw image pixels → footprint-local mm. The correspondences are the
/// source of truth — the transform is always re-derived, never stored —
/// so re-numbering pads or nudging a pad offset keeps the photo aligned.
///
/// `a_px` / `b_px` are in RAW image pixel space (origin top-left, y
/// growing downward), captured with the photo shown unrotated so the
/// stored `Attachment::view_transform` never has to be undone here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PhotoCalibration {
    /// Pixel coords of the first marked pin centre (x, y), y-down.
    pub a_px: (f64, f64),
    /// Pixel coords of the second marked pin centre (x, y), y-down.
    pub b_px: (f64, f64),
    /// Pad number the first mark corresponds to.
    pub a_pad: String,
    /// Pad number the second mark corresponds to.
    pub b_pad: String,
}

/// Physical body extent of a module in footprint-local millimetres
/// (Y-up, same frame as `LibraryPad`). Authored by fitting a rectangle
/// to the calibrated photo; the placement margin is derived from it so
/// the placer / DRC / renderer respect the true module footprint.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct BodyRect {
    pub min_x_mm: f64,
    pub min_y_mm: f64,
    pub max_x_mm: f64,
    pub max_y_mm: f64,
}

/// A derived photo→board similarity transform. Maps a raw image pixel
/// `(px, py)` (y-down) to footprint-local mm via
/// `mm = scale * Rot(rotation_deg) * (px, -py) + (tx, ty)`.
/// The `(px, -py)` term flips the image's downward y into the board's
/// upward y; the full map is therefore orientation-reversing (a
/// reflection), which is exactly right for a top-down photo. Not
/// serialised — always re-derived from `PhotoCalibration`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimilarityTransform {
    pub scale_mm_per_px: f64,
    pub rotation_deg: f64,
    pub tx_mm: f64,
    pub ty_mm: f64,
}

impl SimilarityTransform {
    /// Flatten to an SVG-style affine `[a, b, c, d, e, f]` mapping a raw
    /// image pixel `(px, py)` to footprint-local mm:
    /// `(a·px + c·py + e, b·px + d·py + f)`. The determinant is `-scale²`
    /// (negative) because the y-flip makes this a reflection.
    #[must_use]
    pub fn to_affine(self) -> [f64; 6] {
        let (sin, cos) = self.rotation_deg.to_radians().sin_cos();
        let s = self.scale_mm_per_px;
        [s * cos, s * sin, s * sin, -s * cos, self.tx_mm, self.ty_mm]
    }
}

/// Derive the photo→board similarity transform from two pin
/// correspondences: pad centres `a_mm` / `b_mm` in footprint-local mm
/// (Y-up) and the marked pixel points `a_px` / `b_px` (Y-down). Solves
/// for uniform scale + rotation + translation after flipping pixel y so
/// both frames are Y-up. Rejects degenerate input (coincident marks or
/// coincident pads).
pub fn derive_photo_transform(
    a_mm: (f64, f64),
    b_mm: (f64, f64),
    a_px: (f64, f64),
    b_px: (f64, f64),
) -> Result<SimilarityTransform, String> {
    // Board-space delta.
    let dmx = b_mm.0 - a_mm.0;
    let dmy = b_mm.1 - a_mm.1;
    // Pixel-space delta with y flipped to Y-up (d_px').
    let dpx = b_px.0 - a_px.0;
    let dpy = -(b_px.1 - a_px.1);
    let den = dpx * dpx + dpy * dpy;
    if den < 1e-9 {
        return Err("photo calibration: the two pin marks are at the same point".into());
    }
    if dmx * dmx + dmy * dmy < 1e-12 {
        return Err("photo calibration: the two pads are at the same position".into());
    }
    // c = d_mm / d_px' (complex division) = scale · e^{iθ}.
    let cx = (dmx * dpx + dmy * dpy) / den;
    let cy = (dmy * dpx - dmx * dpy) / den;
    let scale = (cx * cx + cy * cy).sqrt();
    let rotation_deg = cy.atan2(cx).to_degrees();
    // t = a_mm − c · a'  where a' is the y-flipped first pixel point.
    let ax = a_px.0;
    let ay = -a_px.1;
    let tx = a_mm.0 - (cx * ax - cy * ay);
    let ty = a_mm.1 - (cy * ax + cx * ay);
    Ok(SimilarityTransform {
        scale_mm_per_px: scale,
        rotation_deg,
        tx_mm: tx,
        ty_mm: ty,
    })
}

/// Compose two SVG-style affines so the result applies `inner` first,
/// then `outer`: `compose(outer, inner)(p) = outer(inner(p))`.
#[must_use]
pub fn affine_compose(outer: [f64; 6], inner: [f64; 6]) -> [f64; 6] {
    let [a, b, c, d, e, f] = inner;
    let [aa, bb, cc, dd, ee, ff] = outer;
    [
        aa * a + cc * b,
        bb * a + dd * b,
        aa * c + cc * d,
        bb * c + dd * d,
        aa * e + cc * f + ee,
        bb * e + dd * f + ff,
    ]
}

/// Outcome of `Library::rectify_photo`: the new rectified attachment plus
/// the fate of the original photo's calibration under the homography.
#[derive(Debug, Clone, PartialEq)]
pub struct RectifyOutcome {
    /// Id of the freshly-created "photo-rectified" attachment.
    pub attachment_id: String,
    /// Filename of the new attachment (`<orig-stem>-rect.jpg`).
    pub filename: String,
    pub width_px: u32,
    pub height_px: u32,
    /// Pixels-per-mm baked into the rectified image (fixed by construction,
    /// reduced only if the long side hit the size cap).
    pub px_per_mm: f64,
    /// `Some` when the original attachment carried a calibration that we
    /// remapped onto the rectified image. Carries the re-derived scale, the
    /// residual rotation from axis-aligned (nearest multiple of 90°), and
    /// whether the remapped calibration validated (re-derived cleanly).
    pub calibration: Option<RectifyCalibration>,
}

/// The remapped-calibration report inside `RectifyOutcome`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectifyCalibration {
    /// Re-derived photo→board scale of the rectified image; should land at
    /// `1 / px_per_mm` once the corners match the footprint orientation.
    pub scale_mm_per_px: f64,
    /// Re-derived rotation of the rectified calibration (degrees). Near a
    /// multiple of 90° when the corner order matches the footprint.
    pub rotation_deg: f64,
    /// Absolute residual from the nearest multiple of 90° — how far the
    /// rectified photo is from perfectly axis-aligned. Large (> ~2°) means
    /// the detected corners or the corner order are off.
    pub residual_deg: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    /// `UUIDv4`. Also the on-disk basename (extension follows the mime).
    pub id: String,
    /// What the agent thinks this file is — free text, but we suggest
    /// "photo", "datasheet", "note".
    pub kind: String,
    /// Original filename the agent sent. Purely human-facing.
    pub filename: String,
    /// MIME type ("image/jpeg", "application/pdf", "text/plain"…).
    pub mime: String,
    /// Unix seconds — kept simple to avoid a chrono dep for one field.
    pub added_at: u64,
    /// Visual-only orientation tweak applied by the review UI. Does
    /// not change anything in the design pipeline. Default = identity.
    #[serde(default)]
    pub view_transform: ViewTransform,
    /// Two-point photo→board calibration, if the user has calibrated
    /// this photo. `None` for uncalibrated / non-photo attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<PhotoCalibration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LibraryEntry {
    /// Stable identifier (`snake_case`). Used by `palette.add_from_library`.
    pub key: String,
    /// What the part is + any orientation / role intent. The agent
    /// writes this when creating the entry, often after looking at an
    /// attached photo or datasheet.
    pub description: String,
    /// Suggested value for new symbols (e.g. "100nF", "ESP32-S3-Zero").
    #[serde(default)]
    pub default_value: String,
    /// Suggested rotation in degrees CCW when dropped on the board.
    #[serde(default)]
    pub default_rotation_deg: f32,
    /// True if this part should sit flush against a board edge (USB,
    /// screw terminal, antenna). Honoured by placement checks.
    #[serde(default)]
    pub edge_mounted: bool,
    pub pads: Vec<LibraryPad>,
    /// Library-authored silkscreen — body outlines, polarity dots,
    /// `{REF}`/`{VAL}` templates. Empty for legacy entries; the
    /// renderer falls back to a synthesised reference label in that
    /// case (`Footprint::silk` is what the spawn pipeline pushes
    /// these into).
    #[serde(default)]
    pub silk: Vec<LibrarySilk>,
    /// LCSC catalogue number (e.g. "C25804" for a 10 kΩ 0603 chip
    /// resistor). Optional. Used by the JLCPCB BOM writer to populate
    /// the "LCSC Part #" column so JLCPCB SMT assembly knows which
    /// part to load. The agent fills this when picking a real part;
    /// the placement / routing pipeline doesn't read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lcsc_id: Option<String>,
    /// Manufacturer part number (e.g. "RC0603FR-0710KL"). Optional,
    /// fab-agnostic — every assembly house accepts MPN as the
    /// canonical "what to put down here" identifier. Falls back into
    /// generic-format BOMs when an LCSC ID isn't available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mpn: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Unix seconds at creation.
    pub created_at: u64,
    /// Visual-only orientation tweak for the rendered footprint SVG in
    /// the review pane. Independent from `Attachment::view_transform`
    /// (which targets photos). Does NOT alter the routed/placed
    /// footprint geometry. Default = identity.
    #[serde(default)]
    pub footprint_view_transform: ViewTransform,
    /// Extra keep-out around the footprint's pad bounding box, in mm,
    /// applied per side. Honoured by the placer's overlap check and
    /// min-gap penalty so AI-authored pad-only footprints get enough
    /// breathing room for the real component body. Default = all zeros.
    #[serde(default)]
    pub placement_margin: PlacementMargin,
    /// Physical body rectangle in footprint-local mm, authored on a
    /// calibrated photo. When set, `placement_margin` is derived from
    /// it. `None` for entries the user hasn't drawn a body on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_rect: Option<BodyRect>,
}

impl LibraryEntry {
    /// Centre of the pad with the given number, in footprint-local mm.
    #[must_use]
    pub fn pad_center_mm(&self, number: &str) -> Option<(f64, f64)> {
        self.pads
            .iter()
            .find(|p| p.number == number)
            .map(|p| (p.x_mm, p.y_mm))
    }

    /// Axis-aligned bounding box of every pad `(min_x, min_y, max_x,
    /// max_y)` in footprint-local mm. `None` when the entry has no pads.
    #[must_use]
    pub fn pads_bbox_mm(&self) -> Option<(f64, f64, f64, f64)> {
        let mut it = self.pads.iter();
        let first = it.next()?;
        let mut min_x = first.x_mm - first.w_mm / 2.0;
        let mut min_y = first.y_mm - first.h_mm / 2.0;
        let mut max_x = first.x_mm + first.w_mm / 2.0;
        let mut max_y = first.y_mm + first.h_mm / 2.0;
        for p in it {
            min_x = min_x.min(p.x_mm - p.w_mm / 2.0);
            min_y = min_y.min(p.y_mm - p.h_mm / 2.0);
            max_x = max_x.max(p.x_mm + p.w_mm / 2.0);
            max_y = max_y.max(p.y_mm + p.h_mm / 2.0);
        }
        Some((min_x, min_y, max_x, max_y))
    }

    /// Derive the photo→board transform for one calibration against this
    /// entry's pad offsets. Errors if a referenced pad no longer exists.
    pub fn photo_transform(&self, cal: &PhotoCalibration) -> Result<SimilarityTransform, String> {
        let a = self
            .pad_center_mm(&cal.a_pad)
            .ok_or_else(|| format!("photo calibration: pad {} not found", cal.a_pad))?;
        let b = self
            .pad_center_mm(&cal.b_pad)
            .ok_or_else(|| format!("photo calibration: pad {} not found", cal.b_pad))?;
        derive_photo_transform(a, b, cal.a_px, cal.b_px)
    }

    /// SVG-style affine `[a,b,c,d,e,f]` mapping a raw image pixel of the
    /// calibrated photo to the PLACED footprint's local mm frame: the
    /// per-photo calibration (`px → native-local mm`) composed under the
    /// entry's `footprint_view_transform` (`native → placed-local mm`).
    ///
    /// This is the exact matrix the board-canvas photo overlay nests under
    /// the footprint's own `translate(x,y) rotate(rotation)` group, so a
    /// calibration pin pixel lands on the same placed-local point the
    /// footprint's pad does. Because the whole chain (this matrix, then the
    /// shared `rotate`/`translate`, then the board SVG's outer
    /// `scale(1,-1)`) is identical to how pads are drawn, the overlay
    /// tracks the pads at ANY footprint rotation. Kept here — one pure,
    /// unit-tested source of truth — so the Tauri overlay payload and its
    /// regression test can't drift apart.
    pub fn photo_overlay_matrix(&self, cal: &PhotoCalibration) -> Result<[f64; 6], String> {
        let transform = self.photo_transform(cal)?;
        Ok(affine_compose(
            self.footprint_view_transform.to_affine_mm(),
            transform.to_affine(),
        ))
    }

    /// Derive the per-side placement margin implied by `body`: how far
    /// the body extends beyond the pad bounding box on each side (Y-up),
    /// clamped to ≥ 0 so a body smaller than the pads yields no margin.
    #[must_use]
    pub fn margin_from_body_rect(&self, body: &BodyRect) -> PlacementMargin {
        let Some((min_x, min_y, max_x, max_y)) = self.pads_bbox_mm() else {
            return PlacementMargin::default();
        };
        PlacementMargin {
            top_mm: (body.max_y_mm - max_y).max(0.0),
            right_mm: (body.max_x_mm - max_x).max(0.0),
            bottom_mm: (min_y - body.min_y_mm).max(0.0),
            left_mm: (min_x - body.min_x_mm).max(0.0),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LibraryIndex {
    #[serde(default)]
    entries: Vec<LibraryEntry>,
}

#[derive(Debug)]
pub struct Library {
    index_path: PathBuf,
    attachments_dir: PathBuf,
    inner: RwLock<LibraryIndex>,
}

/// Default location: `~/.pcb-library/`. Created on first access.
fn default_root() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".pcb-library")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        _ => "bin",
    }
}

impl Library {
    /// Open (or create) the default-location library.
    pub fn open_default() -> Result<Self, String> {
        Self::open_at(default_root())
    }

    pub fn open_at<P: AsRef<Path>>(root: P) -> Result<Self, String> {
        let root = root.as_ref().to_path_buf();
        let attachments_dir = root.join("attachments");
        let index_path = root.join("index.json");
        fs::create_dir_all(&attachments_dir)
            .map_err(|e| format!("library: create {}: {e}", attachments_dir.display()))?;
        let index = match fs::read(&index_path) {
            Ok(bytes) => serde_json::from_slice::<LibraryIndex>(&bytes)
                .map_err(|e| format!("library: parse {}: {e}", index_path.display()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => LibraryIndex::default(),
            Err(e) => return Err(format!("library: read {}: {e}", index_path.display())),
        };
        Ok(Self {
            index_path,
            attachments_dir,
            inner: RwLock::new(index),
        })
    }

    fn save(&self, index: &LibraryIndex) -> Result<(), String> {
        let bytes =
            serde_json::to_vec_pretty(index).map_err(|e| format!("library: serialise: {e}"))?;
        // Atomic-ish: write to .tmp then rename.
        let tmp = self.index_path.with_extension("json.tmp");
        fs::write(&tmp, &bytes).map_err(|e| format!("library: write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &self.index_path)
            .map_err(|e| format!("library: rename {}: {e}", self.index_path.display()))?;
        Ok(())
    }

    pub fn list(&self) -> Vec<LibraryEntry> {
        let inner = self.inner.read().expect("library lock poisoned");
        inner.entries.clone()
    }

    pub fn find(&self, key: &str) -> Option<LibraryEntry> {
        let inner = self.inner.read().expect("library lock poisoned");
        inner.entries.iter().find(|e| e.key == key).cloned()
    }

    /// Insert or replace an entry by `key`. Replacing preserves any
    /// existing attachments unless the caller explicitly hands a new
    /// list — this lets the agent re-state pads / description without
    /// detaching files.
    pub fn upsert(&self, mut entry: LibraryEntry) -> Result<LibraryEntry, String> {
        if entry.key.is_empty() {
            return Err("library: key must not be empty".into());
        }
        if entry.created_at == 0 {
            entry.created_at = now_secs();
        }
        let mut inner = self.inner.write().expect("library lock poisoned");
        if let Some(existing) = inner.entries.iter().position(|e| e.key == entry.key) {
            // Preserve attachments from the existing entry if the
            // caller didn't override them.
            if entry.attachments.is_empty() {
                entry
                    .attachments
                    .clone_from(&inner.entries[existing].attachments);
            }
            inner.entries[existing] = entry.clone();
        } else {
            inner.entries.push(entry.clone());
        }
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(entry)
    }

    pub fn delete(&self, key: &str) -> Result<bool, String> {
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(pos) = inner.entries.iter().position(|e| e.key == key) else {
            return Ok(false);
        };
        // Drop attachments from disk too.
        for att in inner.entries[pos].attachments.clone() {
            let _ = fs::remove_file(self.attachment_path(&att));
        }
        inner.entries.remove(pos);
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Attach a binary blob to an existing entry. Stores the file under
    /// `attachments/<uuid>.<ext>` and records the metadata.
    pub fn attach(
        &self,
        key: &str,
        kind: String,
        filename: String,
        mime: String,
        data: &[u8],
    ) -> Result<Attachment, String> {
        let id = Uuid::new_v4().to_string();
        let path = self
            .attachments_dir
            .join(format!("{}.{}", id, ext_for_mime(&mime)));
        fs::write(&path, data).map_err(|e| format!("library: write {}: {e}", path.display()))?;
        let att = Attachment {
            id,
            kind,
            filename,
            mime,
            added_at: now_secs(),
            view_transform: ViewTransform::default(),
            calibration: None,
        };
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(entry) = inner.entries.iter_mut().find(|e| e.key == key) else {
            // Roll back the file write so we don't leak orphans.
            let _ = fs::remove_file(&path);
            return Err(format!("library: no entry with key {key}"));
        };
        entry.attachments.push(att.clone());
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(att)
    }

    pub fn delete_attachment(&self, key: &str, attachment_id: &str) -> Result<bool, String> {
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(entry) = inner.entries.iter_mut().find(|e| e.key == key) else {
            return Err(format!("library: no entry with key {key}"));
        };
        let Some(pos) = entry.attachments.iter().position(|a| a.id == attachment_id) else {
            return Ok(false);
        };
        let att = entry.attachments.remove(pos);
        let _ = fs::remove_file(self.attachment_path(&att));
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Resolve an attachment's on-disk path. The file may not exist if
    /// the user manually nuked the attachments dir; callers handle the
    /// missing-file case.
    #[must_use]
    pub fn attachment_path(&self, att: &Attachment) -> PathBuf {
        self.attachments_dir
            .join(format!("{}.{}", att.id, ext_for_mime(&att.mime)))
    }

    /// Read the bytes of an attachment, or an error if it's missing.
    pub fn read_attachment(&self, att: &Attachment) -> Result<Vec<u8>, String> {
        let path = self.attachment_path(att);
        fs::read(&path).map_err(|e| format!("library: read {}: {e}", path.display()))
    }

    /// Overwrite the visual transform on one attachment. Returns `true`
    /// if the attachment was found.
    pub fn set_attachment_view_transform(
        &self,
        key: &str,
        attachment_id: &str,
        transform: ViewTransform,
    ) -> Result<bool, String> {
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(entry) = inner.entries.iter_mut().find(|e| e.key == key) else {
            return Err(format!("library: no entry with key {key}"));
        };
        let Some(att) = entry.attachments.iter_mut().find(|a| a.id == attachment_id) else {
            return Ok(false);
        };
        att.view_transform = transform;
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Overwrite the visual transform on the rendered-footprint cell of
    /// an entry's review card. Returns `true` if the entry was found.
    pub fn set_footprint_view_transform(
        &self,
        key: &str,
        transform: ViewTransform,
    ) -> Result<bool, String> {
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(entry) = inner.entries.iter_mut().find(|e| e.key == key) else {
            return Err(format!("library: no entry with key {key}"));
        };
        entry.footprint_view_transform = transform;
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Overwrite the per-side placement margin on an entry. Returns
    /// `true` if the entry was found.
    pub fn set_placement_margin(&self, key: &str, margin: PlacementMargin) -> Result<bool, String> {
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(entry) = inner.entries.iter_mut().find(|e| e.key == key) else {
            return Err(format!("library: no entry with key {key}"));
        };
        entry.placement_margin = margin;
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Store (or overwrite) the two-point photo calibration on one
    /// attachment. Returns `true` if the attachment was found.
    pub fn set_photo_calibration(
        &self,
        key: &str,
        attachment_id: &str,
        calibration: PhotoCalibration,
    ) -> Result<bool, String> {
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(entry) = inner.entries.iter_mut().find(|e| e.key == key) else {
            return Err(format!("library: no entry with key {key}"));
        };
        let Some(att) = entry.attachments.iter_mut().find(|a| a.id == attachment_id) else {
            return Ok(false);
        };
        att.calibration = Some(calibration);
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Validate + store a two-point photo calibration on one attachment,
    /// returning the derived photo→board transform. Centralises the
    /// checks every caller (Tauri command, script verb) needs so nothing
    /// bad reaches disk: the two pads must be distinct and present, the
    /// two pixel marks must differ, the correspondences must yield a
    /// valid transform, and the attachment must exist on the (confirmed)
    /// entry. Errors describe exactly what failed.
    pub fn calibrate_photo(
        &self,
        key: &str,
        attachment_id: &str,
        calibration: PhotoCalibration,
    ) -> Result<SimilarityTransform, String> {
        if calibration.a_pad == calibration.b_pad {
            return Err("photo calibration: pick two different pads".into());
        }
        let entry = self
            .find(key)
            .ok_or_else(|| format!("no library entry with key {key}"))?;
        // `photo_transform` checks pad existence, coincident marks and
        // coincident pads, so we don't duplicate those guards here.
        let transform = entry.photo_transform(&calibration)?;
        if !entry.attachments.iter().any(|a| a.id == attachment_id) {
            return Err(format!(
                "photo calibration: no attachment {attachment_id} on {key}"
            ));
        }
        self.set_photo_calibration(key, attachment_id, calibration)?;
        Ok(transform)
    }

    /// Drop the photo calibration from one attachment. Returns `true` if
    /// the attachment was found.
    pub fn clear_photo_calibration(&self, key: &str, attachment_id: &str) -> Result<bool, String> {
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(entry) = inner.entries.iter_mut().find(|e| e.key == key) else {
            return Err(format!("library: no entry with key {key}"));
        };
        let Some(att) = entry.attachments.iter_mut().find(|a| a.id == attachment_id) else {
            return Ok(false);
        };
        att.calibration = None;
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Store the physical body rectangle on an entry AND recompute the
    /// derived per-side placement margin from it, in a single atomic
    /// update. Returns `true` if the entry was found.
    pub fn set_body_rect(&self, key: &str, body: BodyRect) -> Result<bool, String> {
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(entry) = inner.entries.iter_mut().find(|e| e.key == key) else {
            return Err(format!("library: no entry with key {key}"));
        };
        entry.placement_margin = entry.margin_from_body_rect(&body);
        entry.body_rect = Some(body);
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Rectify (crop + deskew) one photo attachment into a new
    /// "photo-rectified" attachment, ID-scanner style. `corners` are the
    /// four board corners in the ORIGINAL image's raw pixels (y-down),
    /// given in TL,TR,BR,BL order as they should appear in the OUTPUT —
    /// that order fixes the rectified image's orientation. `quad_w_mm ×
    /// quad_h_mm` is the board's real size, which pins the output to a
    /// fixed px/mm so the rectified photo is metrically exact.
    ///
    /// If the original attachment has a calibration, its two pin marks are
    /// carried through the homography onto the new attachment and the
    /// remap is validated by re-deriving the transform (which should come
    /// out axis-aligned — rotation ≈ a multiple of 90°; the residual is
    /// reported). The new attachment is appended to the entry.
    pub fn rectify_photo(
        &self,
        key: &str,
        attachment_id: &str,
        corners: [(f64, f64); 4],
        quad_w_mm: f64,
        quad_h_mm: f64,
    ) -> Result<RectifyOutcome, String> {
        let entry = self
            .find(key)
            .ok_or_else(|| format!("no library entry with key {key}"))?;
        let src_att = entry
            .attachments
            .iter()
            .find(|a| a.id == attachment_id)
            .ok_or_else(|| format!("rectify: no attachment {attachment_id} on {key}"))?
            .clone();
        if !src_att.mime.starts_with("image/") {
            return Err(format!(
                "rectify: attachment {} is {} — not an image",
                src_att.filename, src_att.mime
            ));
        }
        let bytes = self.read_attachment(&src_att)?;
        let rect = crate::rectify::rectify_image(&bytes, &corners, quad_w_mm, quad_h_mm)?;

        // Derive the new filename from the original stem.
        let stem = Path::new(&src_att.filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("photo");
        let filename = format!("{stem}-rect.jpg");
        let new_att = self.attach(
            key,
            "photo-rectified".into(),
            filename.clone(),
            "image/jpeg".into(),
            &rect.jpeg,
        )?;

        // Carry the original calibration through the homography, if any.
        let calibration = match &src_att.calibration {
            None => None,
            Some(cal) => {
                let map = |(x, y): (f64, f64)| {
                    crate::rectify::apply_homography(&rect.src_to_dst, x, y).ok_or_else(|| {
                        "rectify: calibration mark maps to infinity under the homography"
                            .to_string()
                    })
                };
                let a_px = map(cal.a_px)?;
                let b_px = map(cal.b_px)?;
                let new_cal = PhotoCalibration {
                    a_px,
                    b_px,
                    a_pad: cal.a_pad.clone(),
                    b_pad: cal.b_pad.clone(),
                };
                // Re-derive against the entry's pads to validate + report.
                let transform = entry.photo_transform(&new_cal)?;
                self.set_photo_calibration(key, &new_att.id, new_cal)?;
                // Residual: distance of the derived rotation from the
                // nearest multiple of 90°, folded into [0, 45].
                let r = transform.rotation_deg.rem_euclid(90.0);
                let residual = r.min(90.0 - r);
                Some(RectifyCalibration {
                    scale_mm_per_px: transform.scale_mm_per_px,
                    rotation_deg: transform.rotation_deg,
                    residual_deg: residual,
                })
            }
        };

        Ok(RectifyOutcome {
            attachment_id: new_att.id,
            filename,
            width_px: rect.width,
            height_px: rect.height,
            px_per_mm: rect.px_per_mm,
            calibration,
        })
    }

    /// Drop the body rectangle from an entry. The derived placement
    /// margin is left as-is so any manual override survives. Returns
    /// `true` if the entry was found.
    pub fn clear_body_rect(&self, key: &str) -> Result<bool, String> {
        let mut inner = self.inner.write().expect("library lock poisoned");
        let Some(entry) = inner.entries.iter_mut().find(|e| e.key == key) else {
            return Err(format!("library: no entry with key {key}"));
        };
        entry.body_rect = None;
        let snapshot = inner.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} vs {b}");
    }

    #[test]
    fn view_transform_identity_is_a_noop() {
        let vt = ViewTransform::default();
        assert!(vt.is_identity());
        let (x, y) = vt.apply_point_mm(1.25, -3.5);
        approx(x, 1.25);
        approx(y, -3.5);
        let (w, h) = vt.apply_size_mm(2.0, 0.5);
        approx(w, 2.0);
        approx(h, 0.5);
        assert!((vt.apply_angle_deg(45.0) - 45.0).abs() < 1e-6);
    }

    #[test]
    fn view_transform_flip_h_mirrors_x_only() {
        let vt = ViewTransform {
            rotation_deg: 0,
            flip_h: true,
            flip_v: false,
        };
        let (x, y) = vt.apply_point_mm(2.0, 3.0);
        approx(x, -2.0);
        approx(y, 3.0);
        // Sizes are symmetric — flip alone never swaps w/h.
        let (w, h) = vt.apply_size_mm(2.0, 0.5);
        approx(w, 2.0);
        approx(h, 0.5);
        assert!((vt.apply_angle_deg(30.0) - (-30.0)).abs() < 1e-6);
    }

    #[test]
    fn view_transform_flip_v_mirrors_y_only() {
        let vt = ViewTransform {
            rotation_deg: 0,
            flip_h: false,
            flip_v: true,
        };
        let (x, y) = vt.apply_point_mm(2.0, 3.0);
        approx(x, 2.0);
        approx(y, -3.0);
    }

    #[test]
    fn view_transform_pure_rotations_are_ccw_in_y_up() {
        // (1, 0) rotated 90° CCW → (0, 1).
        let vt90 = ViewTransform {
            rotation_deg: 90,
            ..Default::default()
        };
        let (x, y) = vt90.apply_point_mm(1.0, 0.0);
        approx(x, 0.0);
        approx(y, 1.0);
        // (1, 0) → 180° → (-1, 0).
        let vt180 = ViewTransform {
            rotation_deg: 180,
            ..Default::default()
        };
        let (x, y) = vt180.apply_point_mm(1.0, 0.0);
        approx(x, -1.0);
        approx(y, 0.0);
        // (1, 0) → 270° → (0, -1).
        let vt270 = ViewTransform {
            rotation_deg: 270,
            ..Default::default()
        };
        let (x, y) = vt270.apply_point_mm(1.0, 0.0);
        approx(x, 0.0);
        approx(y, -1.0);
    }

    #[test]
    fn view_transform_90_or_270_swaps_w_h() {
        let vt90 = ViewTransform {
            rotation_deg: 90,
            ..Default::default()
        };
        let (w, h) = vt90.apply_size_mm(2.0, 0.5);
        approx(w, 0.5);
        approx(h, 2.0);
        let vt270 = ViewTransform {
            rotation_deg: 270,
            ..Default::default()
        };
        let (w, h) = vt270.apply_size_mm(2.0, 0.5);
        approx(w, 0.5);
        approx(h, 2.0);
        let vt180 = ViewTransform {
            rotation_deg: 180,
            ..Default::default()
        };
        let (w, h) = vt180.apply_size_mm(2.0, 0.5);
        approx(w, 2.0);
        approx(h, 0.5);
    }

    #[test]
    fn view_transform_flip_then_rotate_order_matches_css() {
        // CSS: rotate(R) scaleX(-1) — applies right-to-left, so scale
        // first, then rotate. Pad (1, 0) with flip_h=true,
        // rotation_deg=90 should go to (-1, 0) after flip, then 90° CCW
        // → (0, -1).
        let vt = ViewTransform {
            rotation_deg: 90,
            flip_h: true,
            flip_v: false,
        };
        let (x, y) = vt.apply_point_mm(1.0, 0.0);
        approx(x, 0.0);
        approx(y, -1.0);
    }

    /// Helper: run a derived transform's affine forward on a pixel point.
    fn apply_affine(m: [f64; 6], px: f64, py: f64) -> (f64, f64) {
        (m[0] * px + m[2] * py + m[4], m[1] * px + m[3] * py + m[5])
    }

    #[test]
    fn photo_transform_round_trips_the_two_anchors() {
        // Two pads 10 mm apart on X; pixel marks 100 px apart on X.
        let t = derive_photo_transform((-5.0, 0.0), (5.0, 0.0), (100.0, 200.0), (200.0, 200.0))
            .expect("derive");
        approx(t.scale_mm_per_px, 10.0 / 100.0);
        let m = t.to_affine();
        let (ax, ay) = apply_affine(m, 100.0, 200.0);
        approx(ax, -5.0);
        approx(ay, 0.0);
        let (bx, by) = apply_affine(m, 200.0, 200.0);
        approx(bx, 5.0);
        approx(by, 0.0);
    }

    #[test]
    fn photo_transform_gets_the_y_flip_right_asymmetric() {
        // Asymmetric triangle so a wrong y sign would not cancel out.
        // Pad A at (0,0), B at (4,3) in mm (Y-up). In the photo, A is at
        // (10,90) and B at (50,30) — B is UP and RIGHT of A on screen
        // (smaller pixel-y = higher up). A correct y-flip must map that
        // screen-up to board-up (+Y).
        let a_mm = (0.0, 0.0);
        let b_mm = (4.0, 3.0);
        let a_px = (10.0, 90.0);
        let b_px = (50.0, 30.0);
        let t = derive_photo_transform(a_mm, b_mm, a_px, b_px).expect("derive");
        let m = t.to_affine();
        // Anchors land exactly.
        let (ax, ay) = apply_affine(m, a_px.0, a_px.1);
        approx(ax, 0.0);
        approx(ay, 0.0);
        let (bx, by) = apply_affine(m, b_px.0, b_px.1);
        approx(bx, 4.0);
        approx(by, 3.0);
        // A pixel BELOW A on screen (larger y) must map to NEGATIVE board
        // Y — proving the flip, not just the anchors, is correct.
        let (_, below_y) = apply_affine(m, 10.0, 140.0);
        assert!(below_y < 0.0, "screen-down should be board-down: {below_y}");
        // Determinant is negative (reflection).
        let det = m[0] * m[3] - m[1] * m[2];
        assert!(det < 0.0, "photo→board map must be a reflection: {det}");
    }

    #[test]
    fn photo_transform_handles_a_rotated_photo() {
        // Photo taken rotated 90°: pads on the board X axis appear along
        // the photo's y axis. A at mm(0,0)→px(50,50), B at mm(10,0)→
        // px(50,150) (B is 100 px DOWN from A on screen). Scale should be
        // 10 mm / 100 px = 0.1, and a board +X step must follow the
        // photo's downward y.
        let t = derive_photo_transform((0.0, 0.0), (10.0, 0.0), (50.0, 50.0), (50.0, 150.0))
            .expect("derive");
        approx(t.scale_mm_per_px, 0.1);
        let m = t.to_affine();
        let (bx, by) = apply_affine(m, 50.0, 150.0);
        approx(bx, 10.0);
        approx(by, 0.0);
    }

    #[test]
    fn photo_transform_rejects_degenerate_input() {
        // Coincident pixel marks.
        assert!(
            derive_photo_transform((0.0, 0.0), (5.0, 0.0), (10.0, 10.0), (10.0, 10.0)).is_err()
        );
        // Coincident pads.
        assert!(derive_photo_transform((1.0, 1.0), (1.0, 1.0), (0.0, 0.0), (30.0, 0.0)).is_err());
    }

    fn entry_with_pads(pads: Vec<LibraryPad>) -> LibraryEntry {
        LibraryEntry {
            key: "e".into(),
            description: String::new(),
            default_value: String::new(),
            default_rotation_deg: 0.0,
            edge_mounted: false,
            pads,
            silk: Vec::new(),
            lcsc_id: None,
            mpn: None,
            attachments: Vec::new(),
            created_at: 0,
            footprint_view_transform: ViewTransform::default(),
            placement_margin: PlacementMargin::default(),
            body_rect: None,
        }
    }

    fn pad(number: &str, x: f64, y: f64, w: f64, h: f64) -> LibraryPad {
        LibraryPad {
            number: number.into(),
            name: String::new(),
            x_mm: x,
            y_mm: y,
            w_mm: w,
            h_mm: h,
            drill_mm: None,
        }
    }

    #[test]
    fn margin_from_body_smaller_than_pads_is_all_zero() {
        // Pads span x∈[-2,2], y∈[-2,2]; body is inside that → no margin.
        let e = entry_with_pads(vec![
            pad("1", -1.5, 0.0, 1.0, 4.0),
            pad("2", 1.5, 0.0, 1.0, 4.0),
        ]);
        let m = e.margin_from_body_rect(&BodyRect {
            min_x_mm: -1.0,
            min_y_mm: -1.0,
            max_x_mm: 1.0,
            max_y_mm: 1.0,
        });
        approx(m.top_mm, 0.0);
        approx(m.right_mm, 0.0);
        approx(m.bottom_mm, 0.0);
        approx(m.left_mm, 0.0);
    }

    #[test]
    fn margin_from_asymmetric_body_is_per_side() {
        // Pad bbox: x∈[-1,1], y∈[-1,1]. Body overhangs 3 right, 0.5 left,
        // 2 top, 0 bottom (bottom body edge inside pads → clamps to 0).
        let e = entry_with_pads(vec![pad("1", 0.0, 0.0, 2.0, 2.0)]);
        let m = e.margin_from_body_rect(&BodyRect {
            min_x_mm: -1.5,
            min_y_mm: 0.0,
            max_x_mm: 4.0,
            max_y_mm: 3.0,
        });
        approx(m.left_mm, 0.5);
        approx(m.right_mm, 3.0);
        approx(m.top_mm, 2.0);
        approx(m.bottom_mm, 0.0);
    }

    #[test]
    fn view_transform_affine_matches_apply_point() {
        for vt in [
            ViewTransform {
                rotation_deg: 90,
                flip_h: false,
                flip_v: false,
            },
            ViewTransform {
                rotation_deg: 0,
                flip_h: true,
                flip_v: false,
            },
            ViewTransform {
                rotation_deg: 270,
                flip_h: false,
                flip_v: true,
            },
        ] {
            let m = vt.to_affine_mm();
            for (x, y) in [(1.0, 0.0), (0.0, 1.0), (2.5, -3.5)] {
                let (ex, ey) = vt.apply_point_mm(x, y);
                let (ax, ay) = apply_affine(m, x, y);
                approx(ax, ex);
                approx(ay, ey);
            }
        }
    }

    #[test]
    fn affine_compose_applies_inner_first() {
        // inner: scale by 2; outer: translate by (10, 20).
        let inner = [2.0, 0.0, 0.0, 2.0, 0.0, 0.0];
        let outer = [1.0, 0.0, 0.0, 1.0, 10.0, 20.0];
        let m = affine_compose(outer, inner);
        let (x, y) = apply_affine(m, 3.0, 4.0);
        approx(x, 16.0); // 3*2 + 10
        approx(y, 28.0); // 4*2 + 20
    }

    #[test]
    fn rectify_photo_remaps_calibration_axis_aligned() {
        use image::ImageFormat;
        // Temp library so we don't touch the real ~/.pcb-library.
        let tmp = std::env::temp_dir().join(format!(
            "pcb-rectify-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&tmp);
        let lib = Library::open_at(&tmp).expect("open temp library");

        // Two pads 8 mm apart on X (±4, 0).
        let mut entry = entry_with_pads(vec![
            pad("1", -4.0, 0.0, 1.0, 1.0),
            pad("2", 4.0, 0.0, 1.0, 1.0),
        ]);
        entry.key = "cam".into();
        lib.upsert(entry).expect("upsert");

        // Synthetic 400×400 photo; the board (10×10 mm) fills it, axis
        // aligned. 40 px/mm. Pads land at image (40,200) and (360,200).
        let img = image::RgbImage::from_pixel(400, 400, image::Rgb([128, 128, 128]));
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
            .expect("encode png");
        let att = lib
            .attach(
                "cam",
                "photo".into(),
                "top.png".into(),
                "image/png".into(),
                &png,
            )
            .expect("attach");

        // Calibrate the original: pads 8 mm ↔ 320 px → 0.025 mm/px, rot 0.
        lib.calibrate_photo(
            "cam",
            &att.id,
            PhotoCalibration {
                a_px: (40.0, 200.0),
                b_px: (360.0, 200.0),
                a_pad: "1".into(),
                b_pad: "2".into(),
            },
        )
        .expect("calibrate original");

        // Rectify using the full-image quad (already axis aligned) → the
        // homography is ~identity, so the remap must stay axis-aligned.
        let outcome = lib
            .rectify_photo(
                "cam",
                &att.id,
                [(0.0, 0.0), (400.0, 0.0), (400.0, 400.0), (0.0, 400.0)],
                10.0,
                10.0,
            )
            .expect("rectify");
        approx(outcome.px_per_mm, 40.0);
        assert_eq!(outcome.width_px, 400);
        let cal = outcome.calibration.expect("calibration remapped");
        assert!(
            (cal.scale_mm_per_px - 0.025).abs() < 1e-4,
            "scale {}",
            cal.scale_mm_per_px
        );
        assert!(cal.residual_deg < 0.5, "residual {}", cal.residual_deg);

        // The new attachment exists, is "photo-rectified", and is calibrated.
        let reloaded = lib.find("cam").expect("find");
        let new = reloaded
            .attachments
            .iter()
            .find(|a| a.id == outcome.attachment_id)
            .expect("new attachment present");
        assert_eq!(new.kind, "photo-rectified");
        assert!(new.calibration.is_some(), "rectified attachment calibrated");
        assert!(new.filename.ends_with("-rect.jpg"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rectify_photo_rejects_degenerate_quad() {
        let tmp = std::env::temp_dir().join(format!(
            "pcb-rectify-bad-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&tmp);
        let lib = Library::open_at(&tmp).expect("open temp library");
        let mut entry = entry_with_pads(vec![
            pad("1", -4.0, 0.0, 1.0, 1.0),
            pad("2", 4.0, 0.0, 1.0, 1.0),
        ]);
        entry.key = "cam".into();
        lib.upsert(entry).expect("upsert");
        let img = image::RgbImage::from_pixel(100, 100, image::Rgb([0, 0, 0]));
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode png");
        let att = lib
            .attach(
                "cam",
                "photo".into(),
                "top.png".into(),
                "image/png".into(),
                &png,
            )
            .expect("attach");
        // Collinear corners → error, and no rectified attachment is added.
        let before = lib.find("cam").unwrap().attachments.len();
        assert!(lib
            .rectify_photo(
                "cam",
                &att.id,
                [(0.0, 0.0), (50.0, 0.0), (100.0, 0.0), (50.0, 10.0)],
                10.0,
                10.0
            )
            .is_err());
        assert_eq!(lib.find("cam").unwrap().attachments.len(), before);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn old_index_json_loads_without_new_fields() {
        // An entry + attachment saved before body_rect / calibration
        // existed must still deserialize (serde defaults fill the gap).
        let json = r#"{
            "entries": [{
                "key": "legacy",
                "description": "old part",
                "pads": [],
                "created_at": 123,
                "attachments": [{
                    "id": "abc",
                    "kind": "photo",
                    "filename": "p.jpg",
                    "mime": "image/jpeg",
                    "added_at": 5
                }]
            }]
        }"#;
        let idx: LibraryIndex = serde_json::from_str(json).expect("parse legacy index");
        assert_eq!(idx.entries.len(), 1);
        assert!(idx.entries[0].body_rect.is_none());
        assert!(idx.entries[0].attachments[0].calibration.is_none());
        assert!(idx.entries[0].placement_margin.is_zero());
    }
}
