//! Board model.
//!
//! A `Board` holds the physical layout: copper layer stack, footprints,
//! traces, vias, and outline. The schematic side lives in `schematic.rs`
//! (added in Phase 2) — this is enough for the Phase 1 placement loop.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::geometry::{Point, Rect};
use crate::units::Length;

/// Stable identifier for any item the human or agent can address by name
/// across script-API calls and UI events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Id(pub Uuid);

impl Id {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parse a UUID string. Used by script-tool inputs that accept
    /// ids as strings (e.g. `delete-trace ID`).
    pub fn parse(s: &str) -> Result<Self, String> {
        Uuid::parse_str(s).map(Self).map_err(|e| e.to_string())
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

/// Copper layer slot. Phase 1 only models the two outer layers; inner
/// layers are added when we tackle multi-layer routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CopperLayer {
    Top,
    Bottom,
}

/// Silkscreen side. Mirrors `CopperLayer` but lives in its own enum
/// so the model can't conflate "copper top" with "silk top" (they
/// share a side but are emitted as different Gerber files and
/// rendered with different visual treatments).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SilkLayer {
    Top,
    Bottom,
}

/// Horizontal anchor for a silk text run. Identical semantics to
/// SVG's `text-anchor`: where on the rendered glyph ribbon the
/// `position` point lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum SilkAnchor {
    Start,
    #[default]
    Middle,
    End,
}

/// A straight silkscreen line segment in board coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SilkLine {
    pub layer: SilkLayer,
    pub start: Point,
    pub end: Point,
    pub width: Length,
}

/// A silkscreen text run in board coordinates.
///
/// Stored as a logical text + anchor + size triple so the renderer
/// (and the Gerber writer) can vectorise it through the Hershey
/// stroke font on the fly. Storing the strokes pre-baked would lock
/// the font choice at edit time and bloat the project file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SilkText {
    pub layer: SilkLayer,
    /// Anchor point of the text in board coords (or footprint-local
    /// when this lives inside a `Footprint`).
    pub position: Point,
    pub text: String,
    /// Glyph cap height.
    pub size: Length,
    /// CCW rotation in degrees.
    #[serde(default)]
    pub rotation: f32,
    #[serde(default)]
    pub anchor: SilkAnchor,
    /// Stroke width — defaults to ~size/8 when constructed via
    /// `SilkText::new`.
    pub width: Length,
}

impl SilkText {
    /// Default stroke width for a given cap height — roughly 12.5%
    /// of the cap height, matching the KiCad default.
    #[must_use]
    pub fn default_stroke(size: Length) -> Length {
        Length(size.0 / 8)
    }
}

/// Footprint-local silk primitive. Coordinates are in the
/// footprint's own frame; the renderer transforms them by the
/// footprint's `position` and `rotation`.
///
/// `Text` placeholders `{REF}` and `{VAL}` resolve to the
/// footprint's `reference` / `value` at render time, so a library
/// entry can ship a single `Text` that reads "R1 / 10k" once placed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FootprintSilk {
    Line {
        layer: SilkLayer,
        start: Point,
        end: Point,
        width: Length,
    },
    Text {
        layer: SilkLayer,
        position: Point,
        text: String,
        size: Length,
        #[serde(default)]
        rotation: f32,
        #[serde(default)]
        anchor: SilkAnchor,
        width: Length,
    },
}

/// A pad on a footprint. Rectangular copper. Optionally perforated:
/// `drill = Some(d)` turns the pad into a hybrid SMD+through-hole
/// landing — useful when you want the freedom to populate either the
/// SMD or the through-hole variant of a part on the same footprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pad {
    /// Footprint-local pad number (e.g. "1", "2", "GND").
    pub number: String,
    /// Optional human-readable pad name from the library (e.g. "A"/"K"
    /// on a LED, "VBAT" on a modem header). Carried so the netlist
    /// sync can match a schematic pin labelled with the pad NAME (a
    /// LED's "A"/"K") to a footprint pad addressed by NUMBER.
    #[serde(default)]
    pub name: String,
    /// Position relative to the footprint origin.
    pub offset: Point,
    pub size: (Length, Length),
    pub layer: CopperLayer,
    /// Net this pad belongs to. `None` until the netlist is synced.
    pub net: Option<String>,
    /// Optional plated through-hole drill diameter. `None` = pure SMD
    /// pad. `Some(d)` = perforated pad: copper rectangle on top with a
    /// centred PTH of diameter `d`. The Excellon writer emits these as
    /// PTH drills; the renderer draws the hole over the pad fill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drill: Option<Length>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    pub id: Id,
    /// Reference designator, e.g. "R1", "U3". Unique within a board.
    pub reference: String,
    /// Component value, e.g. "10k", "STM32F103".
    pub value: String,
    /// Library identifier, e.g. "Resistor_SMD:R_0805". Free-form for now.
    pub library: String,
    pub position: Point,
    /// Rotation in degrees, counter-clockwise.
    pub rotation: f32,
    pub layer: CopperLayer,
    pub pads: Vec<Pad>,
    /// Library key copied from the schematic symbol when this footprint
    /// was placed (snake_case, e.g. "esp32_s3_zero"). Empty string if
    /// the symbol had no key. Lets `view.snapshot` and the UI cross-
    /// reference back to the library entry.
    #[serde(default)]
    pub key: String,
    /// Free-form description copied from the schematic symbol — kept on
    /// the footprint so the agent's intent survives even after the
    /// schematic side is reset.
    #[serde(default)]
    pub description: String,
    /// Whether this footprint must touch a board edge (USB-C cables,
    /// screw terminals, antennas). Honoured by `placement` checks.
    #[serde(default)]
    pub edge_mounted: bool,
    /// Silkscreen primitives drawn relative to this footprint's
    /// origin. Empty for now — library-authored silk is V2; the
    /// field exists so the renderer can decide whether to
    /// synthesise a default `{REF}` label or honour the library.
    #[serde(default)]
    pub silk: Vec<FootprintSilk>,
}

impl Footprint {
    /// Bounding box of the footprint in board coordinates, taking the
    /// footprint's rotation into account.
    #[must_use]
    pub fn bounds(&self) -> Option<Rect> {
        let mut iter = self.pads.iter().map(|pad| {
            let center = self.pad_world_center(pad);
            let (w, h) = self.pad_world_size(pad);
            Rect::from_center(center, w, h)
        });
        let first = iter.next()?;
        Some(iter.fold(first, Rect::union))
    }

    /// Absolute board-coord centre of `pad` after applying the
    /// footprint's position and rotation. The pad's offset is treated
    /// as a vector in footprint-local coords and rotated CCW around
    /// the footprint origin.
    #[must_use]
    pub fn pad_world_center(&self, pad: &Pad) -> Point {
        let theta = f64::from(self.rotation).to_radians();
        let (sin, cos) = (theta.sin(), theta.cos());
        let ox = pad.offset.x.to_mm();
        let oy = pad.offset.y.to_mm();
        let rx = ox * cos - oy * sin;
        let ry = ox * sin + oy * cos;
        Point::new(
            crate::units::Length::from_mm(self.position.x.to_mm() + rx),
            crate::units::Length::from_mm(self.position.y.to_mm() + ry),
        )
    }

    /// Pad dimensions on the board after rotation. We only model 90°
    /// increments (the placer and the rotate-by-keypress path both
    /// produce multiples of 90) so this just swaps width ↔ height
    /// when the rotation lands in the 90° / 270° quadrant.
    #[must_use]
    pub fn pad_world_size(&self, pad: &Pad) -> (crate::units::Length, crate::units::Length) {
        let r = f64::from(self.rotation).rem_euclid(360.0);
        if (45.0..135.0).contains(&r) || (225.0..315.0).contains(&r) {
            (pad.size.1, pad.size.0)
        } else {
            (pad.size.0, pad.size.1)
        }
    }

    /// Transform a footprint-local point into world (board) coords
    /// using the footprint's position + rotation. Same maths as
    /// `pad_world_center`, factored out so silk transforms can share
    /// it without smuggling a fake `Pad`.
    #[must_use]
    pub fn local_to_world(&self, p: Point) -> Point {
        let theta = f64::from(self.rotation).to_radians();
        let (sin, cos) = (theta.sin(), theta.cos());
        let lx = p.x.to_mm();
        let ly = p.y.to_mm();
        let rx = lx * cos - ly * sin;
        let ry = lx * sin + ly * cos;
        Point::new(
            crate::units::Length::from_mm(self.position.x.to_mm() + rx),
            crate::units::Length::from_mm(self.position.y.to_mm() + ry),
        )
    }

    /// Resolve `{REF}` / `{VAL}` placeholders in a silk text body.
    /// Library-authored silk uses these so a single template line
    /// reads correctly once the agent fills in `reference` / `value`.
    #[must_use]
    pub fn resolve_silk_text(&self, raw: &str) -> String {
        raw.replace("{REF}", &self.reference)
            .replace("{VAL}", &self.value)
    }

    /// World-frame bounding box of the footprint's pads inflated by
    /// `margin` (per-side in footprint-local mm). Margins are rotated
    /// into the world AABB to match the footprint's `rotation`. Returns
    /// `None` if the footprint has no pads (no base bbox to inflate).
    ///
    /// This is the canonical "what physical area does this component
    /// actually occupy" rectangle — pads + the library-authored body
    /// keep-out — and is shared by the placer, the script tools'
    /// edge/overlap checks, the renderer's body-outline overlay, and
    /// the DRC body-overlap / body-off-board rules so all four agree.
    #[must_use]
    pub fn inflated_bbox(&self, margin: crate::library::PlacementMargin) -> Option<Rect> {
        let base = self.bounds()?;
        if margin.is_zero() {
            return Some(base);
        }
        let world = rotate_margin_trbl(margin.as_trbl_mm(), self.rotation);
        let [t, r, b, l] = world;
        Some(Rect {
            min: Point::new(
                base.min.x - crate::units::Length::from_mm(l),
                base.min.y - crate::units::Length::from_mm(b),
            ),
            max: Point::new(
                base.max.x + crate::units::Length::from_mm(r),
                base.max.y + crate::units::Length::from_mm(t),
            ),
        })
    }
}

/// Rotate a `[top, right, bottom, left]` local-frame per-side margin
/// into the world-aligned `[top, right, bottom, left]` AABB inflation,
/// given a footprint rotation in degrees CCW. Only 90° increments are
/// modelled (matching `pad_world_size`); off-axis rotations snap to the
/// nearest quadrant — for placement keep-out this rounding error is
/// irrelevant compared to the user-set margins (usually ≥ 0.5 mm).
///
/// This is the same maths the SA placer uses for its gap penalty, kept
/// in `pcb-core` so the script tools, renderer, and DRC can share one
/// definition instead of forking it per crate.
#[must_use]
pub fn rotate_margin_trbl(local: [f64; 4], rotation_deg: f32) -> [f64; 4] {
    let r = f64::from(rotation_deg).rem_euclid(360.0);
    let [t, r2, b, l] = local;
    if (45.0..135.0).contains(&r) {
        // +90° CCW: local +Y (top) → world -X (left); local +X (right) → world +Y (top).
        [r2, b, l, t]
    } else if (135.0..225.0).contains(&r) {
        // 180°.
        [b, l, t, r2]
    } else if (225.0..315.0).contains(&r) {
        // +270° CCW.
        [l, t, r2, b]
    } else {
        local
    }
}

/// A copper trace segment. Traces are stored as straight 2-point
/// segments — polylines and arcs become multiple `Trace`s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    pub id: Id,
    pub layer: CopperLayer,
    pub start: Point,
    pub end: Point,
    pub width: Length,
    pub net: String,
}

/// A through-hole via that joins both copper layers. Phase 5 only models
/// pad-on-via vias (no buried/blind layers); `diameter` is the copper
/// pad diameter, `drill` the hole diameter — annular ring is implicit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Via {
    pub id: Id,
    pub position: Point,
    pub drill: Length,
    pub diameter: Length,
    pub net: String,
}

/// A copper pour (a.k.a. "ground plane" / "filled zone"). The minimal
/// model: a net assigned to fill a layer across the entire board
/// outline. Any pad of the pour's net on the pour's layer is treated
/// by the DRC as electrically connected to the pour. Cross-layer
/// connections still need a via — the pour does not auto-stitch top
/// and bottom for you.
///
/// Phase 7: render is a translucent fill of the outline; we do not
/// yet do polygon clipping around foreign-net items, and Gerber
/// export skips the pour. Both arrive when we replace this with a
/// real polygon model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pour {
    pub net: String,
    pub layer: CopperLayer,
    /// Thermal connection between same-net pads and the surrounding
    /// pour. Defaults to `Spokes4` (the KiCad-friendly default) so
    /// existing projects gain hand-solder-friendly thermals on the
    /// next render; set to `Solid` to recover the old flood behaviour.
    #[serde(default)]
    pub thermal_relief: ThermalRelief,
    /// Auto-stitching policy: when set to `Grid`, the router post-pass
    /// sprinkles vias on this pour's net to tie the top and bottom
    /// pours together. Defaults to `None` — historical behaviour.
    #[serde(default)]
    pub stitching: StitchPolicy,
}

/// Automatic stitching-via policy for a `Pour`. Stitching vias tie a
/// same-net pour on the opposite copper layer to this one, so a
/// ground plane spread across both sides behaves as one electrically.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StitchPolicy {
    /// No automatic stitching (current behaviour).
    #[default]
    None,
    /// Sprinkle vias on a `pitch_mm` grid inside the pour, skipping
    /// cells too close to traces, pads, vias, or keepouts. Only fires
    /// when another `Pour` exists on the OPPOSITE layer for the same
    /// net.
    Grid {
        pitch_mm: f64,
        clearance_mm: f64,
    },
}

/// How a copper pour connects to same-net pads sitting inside it.
///
/// The default `Solid` would flood-fill copper around the pad, which
/// is electrically excellent but bakes the pad into a heat sink — hand
/// soldering or rework becomes painful because the iron tip can't lift
/// the pad temperature past the solder melt point. KiCad and other
/// professional tools default to four-spoke thermal reliefs for this
/// reason; we mirror that.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThermalRelief {
    /// Solid copper around the pad — historical behaviour. Hard to
    /// hand-solder; use only on automated-assembly designs where
    /// thermal mass at the joint is not a concern.
    Solid,
    /// Four spokes (N/S/E/W) connecting the pad copper to the pour
    /// copper through a narrow bridge each, with an air gap
    /// everywhere else. The KiCad default.
    Spokes4 {
        /// Width of each spoke, mm.
        spoke_width_mm: f64,
        /// Air-gap thickness between the pad copper and the pour
        /// copper outside the spokes, mm.
        gap_mm: f64,
    },
}

impl Default for ThermalRelief {
    fn default() -> Self {
        Self::Spokes4 {
            spoke_width_mm: 0.4,
            gap_mm: 0.4,
        }
    }
}

/// A polygonal keepout — region of the board where the router (and
/// DRC) refuse to lay copper. The polygon is a simple closed loop in
/// board coordinates; layers default to "all copper layers".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Keepout {
    pub id: Id,
    /// Closed polygon in board coords. Three or more points; the
    /// loop closes implicitly between the last and first points.
    pub polygon: Vec<Point>,
    /// Copper layers the keepout applies to. Empty = all layers.
    #[serde(default)]
    pub layers: Vec<CopperLayer>,
    /// Net names that ARE allowed to traverse this region. Empty =
    /// allow nothing (the keepout blocks every net).
    ///
    /// NOTE: the first cut of router/DRC support only honours the
    /// empty case ("block everything"). Per-net allow lists are
    /// recorded in the model for future work but treated as "block
    /// all" until the grid grows a per-net allow mask.
    #[serde(default)]
    pub nets_allowed: Vec<String>,
    /// Optional human-readable label for UI / docs.
    #[serde(default)]
    pub label: String,
}

/// The board itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Board {
    /// Optional rectangular outline. `None` means "not set yet"; the
    /// agent or the human assigns one before manufacturing.
    pub outline: Option<Rect>,
    /// Corner radius of the outline rectangle, in nm. `0` (the
    /// default) gives sharp corners — the historical behaviour. Any
    /// positive value rounds all four corners by the same radius;
    /// the renderer, the Gerber Edge.Cuts emitter, and the router's
    /// inset all respect it. Capped at half the shorter board side
    /// when the outline is set; values larger than that wouldn't
    /// produce a closed shape.
    #[serde(default, skip_serializing_if = "is_zero_length")]
    pub outline_corner_radius: Length,
    pub footprints: HashMap<Id, Footprint>,
    /// Insertion order for deterministic rendering and serialisation.
    pub footprint_order: Vec<Id>,
    pub traces: Vec<Trace>,
    pub vias: Vec<Via>,
    /// Copper pours (ground/power planes). Order matters only for
    /// rendering precedence — last one drawn wins.
    #[serde(default)]
    pub pours: Vec<Pour>,
    /// Polygonal routing keep-outs (antenna zones, mounting-screw
    /// rings, mechanical clearances). Empty by default — see
    /// `Keepout` for the per-keepout semantics.
    #[serde(default)]
    pub keepouts: Vec<Keepout>,
    /// Board-level silk strokes (frame lines, fiducial labels,
    /// company logo outlines, ...). Footprint-attached silk lives on
    /// each `Footprint::silk` so it follows the footprint when
    /// moved/rotated.
    #[serde(default)]
    pub silk_lines: Vec<SilkLine>,
    /// Board-level silk text. See `silk_lines` for why this is
    /// separate from footprint-attached silk.
    #[serde(default)]
    pub silk_texts: Vec<SilkText>,
}

/// Serde helper: omit the corner-radius field when it's the default
/// zero, so existing on-disk projects (which never serialised it)
/// stay byte-identical and new projects with sharp corners don't
/// gain a noisy `outline_corner_radius: 0` field on disk.
// serde's `skip_serializing_if` calls this with a `&T`, so the
// pass-by-ref-vs-value lint doesn't apply here.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_length(v: &Length) -> bool {
    v.0 == 0
}

/// Minimum body-to-body clearance between two footprints (mm). Anything
/// closer than this can't be hand-soldered or reworked without disturbing
/// the neighbour, so the placement APIs reject overlapping placements
/// down to this gap.
const MIN_FOOTPRINT_GAP_MM: f64 = 0.5;

/// Tolerance (mm) for "this footprint touches the outline" — bigger
/// than the trace clearance default so rounding doesn't reject borderline
/// edge-mounted placements.
const EDGE_TOUCH_TOLERANCE_MM: f64 = 0.5;

impl Board {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_footprint(&mut self, footprint: Footprint) -> Id {
        let id = footprint.id;
        self.footprint_order.push(id);
        self.footprints.insert(id, footprint);
        id
    }

    pub fn move_footprint(&mut self, id: Id, position: Point) -> bool {
        if let Some(fp) = self.footprints.get_mut(&id) {
            fp.position = position;
            true
        } else {
            false
        }
    }

    pub fn remove_footprint(&mut self, id: Id) -> Option<Footprint> {
        self.footprint_order.retain(|i| *i != id);
        self.footprints.remove(&id)
    }

    /// Remove a footprint AND every trace / via whose endpoint lands on
    /// one of its pads. Used by `placement.delete` so the human (or the
    /// agent) doesn't have to manually clear the routing stubs that
    /// would otherwise dangle into empty space and confuse the router.
    ///
    /// Returns `(footprint, traces_removed, vias_removed, orphaned_nets)`.
    /// `orphaned_nets` is the set of net names whose pad count on the
    /// surviving board drops to zero after the removal — the caller can
    /// surface these as a warning so the agent knows a net just lost
    /// its only consumer and a re-route / netlist edit is in order.
    pub fn remove_footprint_and_routing(
        &mut self,
        id: Id,
    ) -> Option<(Footprint, usize, usize, Vec<String>)> {
        // Snapshot the pads (world coords + net) BEFORE removal so we
        // can match trace/via endpoints against them.
        let fp = self.footprints.get(&id)?.clone();
        let pad_hits: Vec<(CopperLayer, String, Point, Length, Length)> = fp
            .pads
            .iter()
            .filter_map(|pad| {
                pad.net.as_ref().map(|net| {
                    let center = fp.pad_world_center(pad);
                    let (w, h) = fp.pad_world_size(pad);
                    (pad.layer, net.clone(), center, w, h)
                })
            })
            .collect();

        // Helper: does an (x, y) point on `layer` for net `net` land on
        // any of the doomed footprint's pads? Same tolerance shape used
        // by `orphan_trace_ids` so a router-grid snap counts as a hit.
        let touches_pad = |layer: CopperLayer, net: &str, x: f64, y: f64, tol: f64| -> bool {
            for (pad_layer, pad_net, center, pw, ph) in &pad_hits {
                if *pad_layer != layer {
                    continue;
                }
                if pad_net.as_str() != net {
                    continue;
                }
                let dx = (x - center.x.to_mm()).abs() - pw.to_mm() / 2.0;
                let dy = (y - center.y.to_mm()).abs() - ph.to_mm() / 2.0;
                if dx <= tol && dy <= tol {
                    return true;
                }
            }
            false
        };

        let traces_before = self.traces.len();
        self.traces.retain(|t| {
            let tol = t.width.to_mm() / 2.0 + 1e-3;
            let a_hit = touches_pad(
                t.layer,
                &t.net,
                t.start.x.to_mm(),
                t.start.y.to_mm(),
                tol,
            );
            let b_hit = touches_pad(t.layer, &t.net, t.end.x.to_mm(), t.end.y.to_mm(), tol);
            !(a_hit || b_hit)
        });
        let traces_removed = traces_before - self.traces.len();

        // Vias join both layers, so a via "touches" the footprint if it
        // sits on a same-net pad on EITHER layer.
        let vias_before = self.vias.len();
        self.vias.retain(|v| {
            let tol = v.diameter.to_mm() / 2.0 + 1e-3;
            let x = v.position.x.to_mm();
            let y = v.position.y.to_mm();
            !(touches_pad(CopperLayer::Top, &v.net, x, y, tol)
                || touches_pad(CopperLayer::Bottom, &v.net, x, y, tol))
        });
        let vias_removed = vias_before - self.vias.len();

        // Finally drop the footprint itself.
        let removed = self.remove_footprint(id)?;

        // Compute the nets whose ONLY remaining pads belonged to the
        // dropped footprint. The caller surfaces these as warnings —
        // without a pad the router can't terminate the net, and the
        // schematic side likely needs an edit.
        let surviving_nets: HashSet<String> = self
            .footprints_in_order()
            .flat_map(|fp| fp.pads.iter().filter_map(|p| p.net.clone()))
            .collect();
        let mut orphaned: Vec<String> = removed
            .pads
            .iter()
            .filter_map(|p| p.net.clone())
            .filter(|n| !surviving_nets.contains(n))
            .collect();
        orphaned.sort();
        orphaned.dedup();

        Some((removed, traces_removed, vias_removed, orphaned))
    }

    pub fn footprints_in_order(&self) -> impl Iterator<Item = &Footprint> {
        self.footprint_order
            .iter()
            .filter_map(|id| self.footprints.get(id))
    }

    /// Tight bounding box covering every placed footprint, or `None` for
    /// an empty board.
    #[must_use]
    pub fn content_bounds(&self) -> Option<Rect> {
        let mut iter = self.footprints_in_order().filter_map(Footprint::bounds);
        let first = iter.next()?;
        Some(iter.fold(first, Rect::union))
    }

    /// Reference of the first board footprint whose bbox (inflated by
    /// `MIN_FOOTPRINT_GAP_MM / 2` on each side) intersects `probe`'s
    /// bbox, or `None` if `probe` is clear. `ignore_id` skips a single
    /// footprint — useful when `probe` is the same physical part at a
    /// new pose. Used by the placement APIs and the auto-placer to
    /// reject moves that would butt two parts together.
    #[must_use]
    pub fn first_overlapper(&self, probe: &Footprint, ignore_id: Option<Id>) -> Option<String> {
        let half_gap = Length::from_mm(MIN_FOOTPRINT_GAP_MM / 2.0);
        let probe_bounds = probe.bounds()?.expand(half_gap);
        for fp in self.footprints_in_order() {
            if Some(fp.id) == ignore_id {
                continue;
            }
            if let Some(b) = fp.bounds() {
                if probe_bounds.intersects(&b.expand(half_gap)) {
                    return Some(fp.reference.clone());
                }
            }
        }
        None
    }

    /// Reference of the first board footprint whose **inflated body
    /// bbox** (pads + the library-authored placement margin) intersects
    /// `probe`'s inflated body bbox, or `None` if clear. `ignore_id`
    /// skips a single footprint (same use as `first_overlapper`).
    /// `margin_for` resolves a footprint's library margin — typically
    /// `|fp| library.find(&fp.key).map(|e| e.placement_margin).unwrap_or_default()`.
    ///
    /// Unlike `first_overlapper`, this does NOT add the
    /// `MIN_FOOTPRINT_GAP_MM` half-gap: the margin itself is the user's
    /// declared keep-out, and stacking the two would double-reject
    /// borderline-but-intended placements. The hard pad-overlap check
    /// from `first_overlapper` is still applied separately by the
    /// placement APIs, so pad-on-pad shorts remain rejected.
    #[must_use]
    pub fn first_body_overlapper<F>(
        &self,
        probe: &Footprint,
        ignore_id: Option<Id>,
        margin_for: &F,
    ) -> Option<String>
    where
        F: Fn(&Footprint) -> crate::library::PlacementMargin,
    {
        let probe_bounds = probe.inflated_bbox(margin_for(probe))?;
        for fp in self.footprints_in_order() {
            if Some(fp.id) == ignore_id {
                continue;
            }
            if let Some(b) = fp.inflated_bbox(margin_for(fp)) {
                if probe_bounds.intersects(&b) {
                    return Some(fp.reference.clone());
                }
            }
        }
        None
    }

    /// If `probe`'s inflated body bbox extends past the board outline
    /// on any side, return a human-readable description. This is the
    /// universal physical-feasibility check: the part's plastic must
    /// fit on the board. The `edge_mounted` flag does NOT exempt a
    /// footprint here — an edge-mounted connector may have its pads
    /// flush with the outline, but the body still has to land on
    /// copper. The `EDGE_TOUCH_TOLERANCE_MM` tolerance (0.5 mm) is
    /// kept so a body that just kisses the outline is not flagged.
    /// Returns `None` when:
    ///   - the board has no outline yet, or
    ///   - the inflated bbox fits within the outline (within tolerance).
    #[must_use]
    pub fn body_outline_violation(
        &self,
        probe: &Footprint,
        margin: crate::library::PlacementMargin,
    ) -> Option<String> {
        let outline = self.outline?;
        let bbox = probe.inflated_bbox(margin)?;
        let tol_nm = (EDGE_TOUCH_TOLERANCE_MM * 1_000_000.0) as i64;
        let over_left = outline.min.x.0 - bbox.min.x.0;
        let over_right = bbox.max.x.0 - outline.max.x.0;
        let over_bottom = outline.min.y.0 - bbox.min.y.0;
        let over_top = bbox.max.y.0 - outline.max.y.0;
        let worst = over_left.max(over_right).max(over_bottom).max(over_top);
        if worst <= tol_nm {
            return None;
        }
        let side = if worst == over_left {
            "left"
        } else if worst == over_right {
            "right"
        } else if worst == over_bottom {
            "bottom"
        } else {
            "top"
        };
        let mm = worst as f64 / 1_000_000.0;
        Some(format!(
            "inflated body extends {mm:.2} mm past the {side} board outline"
        ))
    }

    /// If `probe.edge_mounted` is true, return a human-readable reason
    /// when its bbox does NOT touch any side of the board outline.
    /// Returns `None` if either edge_mounted is false (no constraint),
    /// the board has no outline yet, or at least one bbox side is
    /// within tolerance of the matching outline side.
    #[must_use]
    pub fn edge_mount_violation(&self, probe: &Footprint) -> Option<String> {
        if !probe.edge_mounted {
            return None;
        }
        let outline = self.outline?;
        let bbox = probe.bounds()?;
        let tol_nm = (EDGE_TOUCH_TOLERANCE_MM * 1_000_000.0) as i64;
        let touches_left = (bbox.min.x.0 - outline.min.x.0).abs() <= tol_nm;
        let touches_right = (outline.max.x.0 - bbox.max.x.0).abs() <= tol_nm;
        let touches_top = (bbox.min.y.0 - outline.min.y.0).abs() <= tol_nm;
        let touches_bottom = (outline.max.y.0 - bbox.max.y.0).abs() <= tol_nm;
        if touches_left || touches_right || touches_top || touches_bottom {
            return None;
        }
        let dx_left = (bbox.min.x.0 - outline.min.x.0).abs() as f64 / 1_000_000.0;
        let dx_right = (outline.max.x.0 - bbox.max.x.0).abs() as f64 / 1_000_000.0;
        let dy_top = (bbox.min.y.0 - outline.min.y.0).abs() as f64 / 1_000_000.0;
        let dy_bottom = (outline.max.y.0 - bbox.max.y.0).abs() as f64 / 1_000_000.0;
        let nearest = dx_left.min(dx_right).min(dy_top).min(dy_bottom);
        Some(format!(
            "the bbox is {nearest:.2} mm from the nearest outline edge"
        ))
    }

    pub fn add_trace(&mut self, trace: Trace) -> Id {
        let id = trace.id;
        self.traces.push(trace);
        id
    }

    pub fn add_via(&mut self, via: Via) -> Id {
        let id = via.id;
        self.vias.push(via);
        id
    }

    /// Drop every trace and via on the board. The router uses this
    /// before re-laying the routing — keeps re-routes idempotent.
    pub fn clear_routing(&mut self) {
        self.traces.clear();
        self.vias.clear();
    }

    /// Append a silk line to the board.
    pub fn add_silk_line(&mut self, line: SilkLine) {
        self.silk_lines.push(line);
    }

    /// Append a silk text item to the board.
    pub fn add_silk_text(&mut self, text: SilkText) {
        self.silk_texts.push(text);
    }

    /// Add a pour, replacing any existing pour with the same (net, layer).
    pub fn add_pour(&mut self, pour: Pour) {
        self.pours
            .retain(|p| !(p.net == pour.net && p.layer == pour.layer));
        self.pours.push(pour);
    }

    /// Add a keepout. Returns its id so the caller can later remove
    /// it without scanning by polygon contents.
    pub fn add_keepout(&mut self, keepout: Keepout) -> Id {
        let id = keepout.id;
        self.keepouts.push(keepout);
        id
    }

    /// Remove the keepout with the given id. Returns whether anything
    /// was removed.
    pub fn remove_keepout(&mut self, id: Id) -> bool {
        let before = self.keepouts.len();
        self.keepouts.retain(|k| k.id != id);
        self.keepouts.len() != before
    }

    /// Remove a pour matching this (net, layer). Returns true if one was removed.
    pub fn remove_pour(&mut self, net: &str, layer: CopperLayer) -> bool {
        let before = self.pours.len();
        self.pours.retain(|p| !(p.net == net && p.layer == layer));
        self.pours.len() < before
    }

    /// Trace ids whose neither endpoint touches a pad / via / other-
    /// trace endpoint of the same net. Caller can filter these out
    /// to avoid rendering or exporting half-finished routing
    /// leftovers. Tolerance is half the trace width plus 1 µm so a
    /// router-grid snap offset still counts as connected.
    #[must_use]
    pub fn orphan_trace_ids(&self) -> HashSet<Id> {
        let mut orphans: HashSet<Id> = HashSet::new();
        for trace in &self.traces {
            let connected = |x: f64, y: f64| -> bool {
                let tol = trace.width.to_mm() / 2.0 + 1e-3;
                for fp in self.footprints_in_order() {
                    for pad in &fp.pads {
                        if pad.layer != trace.layer {
                            continue;
                        }
                        if pad.net.as_deref() != Some(trace.net.as_str()) {
                            continue;
                        }
                        let c = fp.pad_world_center(pad);
                        let (pw, ph) = fp.pad_world_size(pad);
                        let dx = (x - c.x.to_mm()).abs() - pw.to_mm() / 2.0;
                        let dy = (y - c.y.to_mm()).abs() - ph.to_mm() / 2.0;
                        if dx <= tol && dy <= tol {
                            return true;
                        }
                    }
                }
                for via in &self.vias {
                    if via.net != trace.net {
                        continue;
                    }
                    let r = via.diameter.to_mm() / 2.0 + tol;
                    let dx = x - via.position.x.to_mm();
                    let dy = y - via.position.y.to_mm();
                    if dx * dx + dy * dy <= r * r {
                        return true;
                    }
                }
                for other in &self.traces {
                    if other.id == trace.id {
                        continue;
                    }
                    if other.net != trace.net || other.layer != trace.layer {
                        continue;
                    }
                    for (ex, ey) in [
                        (other.start.x.to_mm(), other.start.y.to_mm()),
                        (other.end.x.to_mm(), other.end.y.to_mm()),
                    ] {
                        let dx = x - ex;
                        let dy = y - ey;
                        if dx * dx + dy * dy <= tol * tol {
                            return true;
                        }
                    }
                }
                false
            };
            let a_ok = connected(trace.start.x.to_mm(), trace.start.y.to_mm());
            let b_ok = connected(trace.end.x.to_mm(), trace.end.y.to_mm());
            if !a_ok && !b_ok {
                orphans.insert(trace.id);
            }
        }
        orphans
    }

    /// Via ids that no surviving same-net trace endpoint approaches.
    /// Combined with `orphan_trace_ids`, lets exporters drop both
    /// the dangling stubs and their dangling vias in one pass.
    #[must_use]
    pub fn orphan_via_ids(&self) -> HashSet<Id> {
        let dropped_traces = self.orphan_trace_ids();
        let mut orphans: HashSet<Id> = HashSet::new();
        'via: for via in &self.vias {
            let cx = via.position.x.to_mm();
            let cy = via.position.y.to_mm();
            let r = via.diameter.to_mm() / 2.0 + 1e-3;
            for trace in &self.traces {
                if dropped_traces.contains(&trace.id) {
                    continue;
                }
                if trace.net != via.net {
                    continue;
                }
                for (ex, ey) in [
                    (trace.start.x.to_mm(), trace.start.y.to_mm()),
                    (trace.end.x.to_mm(), trace.end.y.to_mm()),
                ] {
                    let dx = cx - ex;
                    let dy = cy - ey;
                    if dx * dx + dy * dy <= r * r {
                        continue 'via;
                    }
                }
            }
            // No surviving trace touches this via — but it might still
            // sit on a same-net pad as a deliberate "test point" via.
            for fp in self.footprints_in_order() {
                for pad in &fp.pads {
                    if pad.net.as_deref() != Some(via.net.as_str()) {
                        continue;
                    }
                    let c = fp.pad_world_center(pad);
                    let (pw, ph) = fp.pad_world_size(pad);
                    let dx = (cx - c.x.to_mm()).abs() - pw.to_mm() / 2.0;
                    let dy = (cy - c.y.to_mm()).abs() - ph.to_mm() / 2.0;
                    if dx <= r && dy <= r {
                        continue 'via;
                    }
                }
            }
            orphans.insert(via.id);
        }
        orphans
    }

    /// Pairs of footprint references whose pad-derived bounding boxes
    /// intersect on the board. Used as a hard postcondition after the
    /// placer settles: any non-empty result means the layout is invalid
    /// and the user must intervene (rotate, resize the board, drop
    /// components). References are returned sorted within each pair so
    /// the caller can format stable error strings.
    #[must_use]
    pub fn footprint_overlaps(&self) -> Vec<(String, String)> {
        let with_bounds: Vec<(&Footprint, Rect)> = self
            .footprints_in_order()
            .filter_map(|fp| fp.bounds().map(|r| (fp, r)))
            .collect();
        let mut out = Vec::new();
        for i in 0..with_bounds.len() {
            for j in (i + 1)..with_bounds.len() {
                let (a, ar) = with_bounds[i];
                let (b, br) = with_bounds[j];
                if ar.intersects(&br) {
                    let (lo, hi) = if a.reference <= b.reference {
                        (a.reference.clone(), b.reference.clone())
                    } else {
                        (b.reference.clone(), a.reference.clone())
                    };
                    out.push((lo, hi));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;
    use crate::units::Length;

    #[test]
    fn silk_round_trips_through_serde() {
        let mut b = Board::new();
        b.add_silk_line(SilkLine {
            layer: SilkLayer::Top,
            start: Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
            end: Point::new(Length::from_mm(10.0), Length::from_mm(5.0)),
            width: Length::from_mm(0.15),
        });
        b.add_silk_text(SilkText {
            layer: SilkLayer::Bottom,
            position: Point::new(Length::from_mm(3.0), Length::from_mm(7.0)),
            text: "PCB v1".into(),
            size: Length::from_mm(1.5),
            rotation: 90.0,
            anchor: SilkAnchor::Middle,
            width: SilkText::default_stroke(Length::from_mm(1.5)),
        });
        let json = serde_json::to_string(&b).expect("serialize");
        let back: Board = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.silk_lines.len(), 1);
        assert_eq!(back.silk_texts.len(), 1);
        assert_eq!(back.silk_texts[0].text, "PCB v1");
        assert_eq!(back.silk_lines[0].layer, SilkLayer::Top);
        assert_eq!(back.silk_texts[0].layer, SilkLayer::Bottom);
    }

    #[test]
    fn remove_footprint_and_routing_drops_connected_traces_and_vias() {
        let mut b = Board::new();
        // Two pads, both on net "SIG", on the top layer, at (0,0) and (5,0).
        let fp_id = Id::new();
        b.add_footprint(Footprint {
            id: fp_id,
            reference: "U1".into(),
            value: "test".into(),
            library: "lib".into(),
            position: Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
            rotation: 0.0,
            layer: CopperLayer::Top,
            pads: vec![
                Pad {
                    number: "1".into(),
                    name: String::new(),
                    offset: Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
                    size: (Length::from_mm(1.0), Length::from_mm(1.0)),
                    layer: CopperLayer::Top,
                    net: Some("SIG".into()),
                    drill: None,
                },
                Pad {
                    number: "2".into(),
                    name: String::new(),
                    offset: Point::new(Length::from_mm(5.0), Length::from_mm(0.0)),
                    size: (Length::from_mm(1.0), Length::from_mm(1.0)),
                    layer: CopperLayer::Top,
                    net: Some("SIG".into()),
                    drill: None,
                },
            ],
            key: String::new(),
            description: String::new(),
            edge_mounted: false,
            silk: vec![],
        });

        // Another footprint not connected to U1, owns net "OTHER".
        let other_id = Id::new();
        b.add_footprint(Footprint {
            id: other_id,
            reference: "R1".into(),
            value: "10k".into(),
            library: "lib".into(),
            position: Point::new(Length::from_mm(20.0), Length::from_mm(20.0)),
            rotation: 0.0,
            layer: CopperLayer::Top,
            pads: vec![Pad {
                number: "1".into(),
                name: String::new(),
                offset: Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
                size: (Length::from_mm(1.0), Length::from_mm(1.0)),
                layer: CopperLayer::Top,
                net: Some("OTHER".into()),
                drill: None,
            }],
            key: String::new(),
            description: String::new(),
            edge_mounted: false,
            silk: vec![],
        });

        // Trace landing on U1.pad1 (gets removed) and another on R1
        // (survives).
        let connected = Trace {
            id: Id::new(),
            layer: CopperLayer::Top,
            start: Point::new(Length::from_mm(0.0), Length::from_mm(0.0)),
            end: Point::new(Length::from_mm(2.5), Length::from_mm(0.0)),
            width: Length::from_mm(0.25),
            net: "SIG".into(),
        };
        let unrelated = Trace {
            id: Id::new(),
            layer: CopperLayer::Top,
            start: Point::new(Length::from_mm(20.0), Length::from_mm(20.0)),
            end: Point::new(Length::from_mm(25.0), Length::from_mm(20.0)),
            width: Length::from_mm(0.25),
            net: "OTHER".into(),
        };
        let connected_id = connected.id;
        let unrelated_id = unrelated.id;
        b.add_trace(connected);
        b.add_trace(unrelated);

        // Via on U1.pad2 (gets removed).
        let via = Via {
            id: Id::new(),
            position: Point::new(Length::from_mm(5.0), Length::from_mm(0.0)),
            drill: Length::from_mm(0.3),
            diameter: Length::from_mm(0.6),
            net: "SIG".into(),
        };
        b.add_via(via);

        let (removed, traces_removed, vias_removed, orphaned) =
            b.remove_footprint_and_routing(fp_id).expect("removed");
        assert_eq!(removed.reference, "U1");
        assert_eq!(traces_removed, 1);
        assert_eq!(vias_removed, 1);
        assert_eq!(orphaned, vec!["SIG".to_string()]);
        assert!(!b.footprints.contains_key(&fp_id));
        assert!(b.footprints.contains_key(&other_id));
        assert!(b.traces.iter().any(|t| t.id == unrelated_id));
        assert!(!b.traces.iter().any(|t| t.id == connected_id));
        assert!(b.vias.is_empty());
    }

    #[test]
    fn footprint_silk_field_roundtrips() {
        let mut fp = Footprint {
            id: Id::new(),
            reference: "R1".into(),
            value: "10k".into(),
            library: "lib".into(),
            position: Point::ORIGIN,
            rotation: 0.0,
            layer: CopperLayer::Top,
            pads: vec![],
            key: String::new(),
            description: String::new(),
            edge_mounted: false,
            silk: vec![FootprintSilk::Line {
                layer: SilkLayer::Top,
                start: Point::ORIGIN,
                end: Point::new(Length::from_mm(1.0), Length::from_mm(0.0)),
                width: Length::from_mm(0.15),
            }],
        };
        fp.value = "22k".into();
        let s = serde_json::to_string(&fp).unwrap();
        let back: Footprint = serde_json::from_str(&s).unwrap();
        assert_eq!(back.silk.len(), 1);
    }

    /// Build a 2-pad footprint centred at `pos` with `rotation`, pads
    /// at local ±1.0 mm on x, 0.5 mm × 0.5 mm — small helper for the
    /// inflated-bbox tests below.
    fn make_two_pad_fp(pos: Point, rotation: f32) -> Footprint {
        Footprint {
            id: Id::new(),
            reference: "J1".into(),
            value: String::new(),
            library: "test".into(),
            position: pos,
            rotation,
            layer: CopperLayer::Top,
            pads: vec![
                Pad {
                    number: "1".into(),
                    name: String::new(),
                    offset: Point::new(Length::from_mm(-1.0), Length::from_mm(0.0)),
                    size: (Length::from_mm(0.5), Length::from_mm(0.5)),
                    layer: CopperLayer::Top,
                    net: None,
                    drill: None,
                },
                Pad {
                    number: "2".into(),
                    name: String::new(),
                    offset: Point::new(Length::from_mm(1.0), Length::from_mm(0.0)),
                    size: (Length::from_mm(0.5), Length::from_mm(0.5)),
                    layer: CopperLayer::Top,
                    net: None,
                    drill: None,
                },
            ],
            key: "test_part".into(),
            description: String::new(),
            edge_mounted: false,
            silk: vec![],
        }
    }

    #[test]
    fn inflated_bbox_asymmetric_margin_at_zero_rotation() {
        // Pad bbox: x ∈ [-1.25, 1.25], y ∈ [-0.25, 0.25].
        let fp = make_two_pad_fp(Point::ORIGIN, 0.0);
        let margin = crate::library::PlacementMargin {
            top_mm: 1.0,
            right_mm: 2.0,
            bottom_mm: 0.5,
            left_mm: 3.0,
        };
        let bb = fp.inflated_bbox(margin).expect("bbox");
        assert!((bb.min.x.to_mm() - (-1.25 - 3.0)).abs() < 1e-6, "left");
        assert!((bb.max.x.to_mm() - (1.25 + 2.0)).abs() < 1e-6, "right");
        assert!((bb.min.y.to_mm() - (-0.25 - 0.5)).abs() < 1e-6, "bottom");
        assert!((bb.max.y.to_mm() - (0.25 + 1.0)).abs() < 1e-6, "top");
    }

    #[test]
    fn inflated_bbox_rotates_margins_with_footprint() {
        // 90° CCW: local +Y (top, 1mm margin) becomes world -X (left).
        // Pad bbox after rotation: x ∈ [-0.25, 0.25], y ∈ [-1.25, 1.25].
        let fp = make_two_pad_fp(Point::ORIGIN, 90.0);
        let margin = crate::library::PlacementMargin {
            top_mm: 1.0,
            right_mm: 2.0,
            bottom_mm: 0.5,
            left_mm: 3.0,
        };
        let bb = fp.inflated_bbox(margin).expect("bbox");
        // After 90° CCW: world [t, r, b, l] = [right, bottom, left, top]
        //              = [2.0, 0.5, 3.0, 1.0].
        assert!((bb.min.x.to_mm() - (-0.25 - 1.0)).abs() < 1e-6, "left");
        assert!((bb.max.x.to_mm() - (0.25 + 0.5)).abs() < 1e-6, "right");
        assert!((bb.min.y.to_mm() - (-1.25 - 3.0)).abs() < 1e-6, "bottom");
        assert!((bb.max.y.to_mm() - (1.25 + 2.0)).abs() < 1e-6, "top");
    }

    #[test]
    fn inflated_bbox_zero_margin_matches_bounds() {
        let fp = make_two_pad_fp(
            Point::new(Length::from_mm(5.0), Length::from_mm(-2.0)),
            0.0,
        );
        let bb = fp
            .inflated_bbox(crate::library::PlacementMargin::default())
            .expect("bbox");
        let raw = fp.bounds().expect("bounds");
        assert_eq!(bb.min.x.0, raw.min.x.0);
        assert_eq!(bb.max.x.0, raw.max.x.0);
        assert_eq!(bb.min.y.0, raw.min.y.0);
        assert_eq!(bb.max.y.0, raw.max.y.0);
    }
}
