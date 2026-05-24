//! Tiny stroke font for silkscreen text.
//!
//! Why a custom stroke font and not a TTF/Hershey-simplex import?
//!  * The "no third-party dep" rule rules out `ttf-parser`, `ab_glyph`,
//!    and friends.
//!  * Transcribing the full Hershey-simplex tables (≈900 glyphs) would
//!    inflate the binary with data we don't need yet — V1 silk only
//!    has to render the ASCII subset 0x20..=0x7E at ~1.5 mm so the
//!    fab pen-plotter strokes are legible.
//!  * Silkscreen on a real PCB is intentionally chunky: the fab applies
//!    it through a screen with a finite line width, so the visual
//!    gain from spline-perfect glyphs is zero.
//!
//! Approach: every glyph is a list of stroke polylines in a 32-unit
//! tall design grid. `CAP_HEIGHT_UNITS = 32` is the cap height; every
//! glyph advances by `ADVANCE_UNITS = 24` (a fixed-pitch font keeps
//! the layout code trivial — silkscreen text rarely needs fine
//! kerning). Coordinates are in (x, y) pairs where +y points up.
//!
//! Each glyph is hand-authored to be readable as a plotter trace, NOT
//! to look like a serif typeface. Diagonals are drawn with single
//! strokes; round letters (O, C, G, S) are approximated with 4-8
//! straight segments — the same visual style KiCad uses for its
//! built-in stroke font.

use crate::board::SilkAnchor;
use crate::geometry::Point;
use crate::units::Length;

/// Cap height of every glyph in design-grid units.
pub const CAP_HEIGHT_UNITS: f64 = 32.0;
/// Horizontal advance — the distance from one glyph origin to the
/// next, including a baked-in side bearing. Fixed-pitch keeps the
/// layout maths trivial.
pub const ADVANCE_UNITS: i32 = 24;

/// One glyph's strokes.
///
/// `strokes[i]` is a polyline; each glyph has one or more polylines
/// (multi-stroke glyphs like `i` and `=` need disjoint strokes).
pub struct Glyph {
    pub strokes: &'static [&'static [(i8, i8)]],
}

/// Total stroke width in design-grid units, summing every line in
/// the string. Useful only for lower-bound width comparisons; the
/// REAL pen path width is `text_advance_units(s)`.
#[must_use]
pub fn text_width_units(s: &str) -> i32 {
    text_advance_units(s)
}

/// Sum of advance widths — the width of the baseline ribbon the text
/// occupies before any anchoring offset is applied.
#[must_use]
pub fn text_advance_units(s: &str) -> i32 {
    let n: i32 = s.chars().count().try_into().unwrap_or(0);
    n * ADVANCE_UNITS
}

/// Convert a string into world-space line segments.
///
/// `origin` is the anchor point on the baseline; `size` is the cap
/// height in millimetres. The resulting segments respect the requested
/// horizontal `anchor` and the CCW `rotation_deg` (in degrees).
#[must_use]
pub fn text_segments(
    text: &str,
    origin: Point,
    size: Length,
    rotation_deg: f32,
    anchor: SilkAnchor,
) -> Vec<(Point, Point)> {
    if text.is_empty() {
        return Vec::new();
    }
    let scale = size.to_mm() / CAP_HEIGHT_UNITS;
    let total_w = f64::from(text_advance_units(text)) * scale;
    let anchor_dx = match anchor {
        SilkAnchor::Start => 0.0,
        SilkAnchor::Middle => -total_w / 2.0,
        SilkAnchor::End => -total_w,
    };
    let theta = f64::from(rotation_deg).to_radians();
    let (sin_t, cos_t) = (theta.sin(), theta.cos());
    let ox = origin.x.to_mm();
    let oy = origin.y.to_mm();

    let mut out: Vec<(Point, Point)> = Vec::new();
    let to_world = |xu: f64, yu: f64| -> Point {
        // 1. Local glyph-space coordinates (in mm).
        let lx = anchor_dx + xu * scale;
        let ly = yu * scale;
        // 2. Rotate and translate into world space.
        let wx = lx * cos_t - ly * sin_t + ox;
        let wy = lx * sin_t + ly * cos_t + oy;
        Point::new(Length::from_mm(wx), Length::from_mm(wy))
    };

    let mut pen_x: f64 = 0.0;
    for ch in text.chars() {
        let g = glyph(ch);
        for stroke in g.strokes {
            for pair in stroke.windows(2) {
                let (x0, y0) = pair[0];
                let (x1, y1) = pair[1];
                let p0 = to_world(pen_x + f64::from(x0), f64::from(y0));
                let p1 = to_world(pen_x + f64::from(x1), f64::from(y1));
                out.push((p0, p1));
            }
        }
        pen_x += f64::from(ADVANCE_UNITS);
    }
    out
}

/// Same vectorisation as `text_segments` but returned as a list of
/// polylines — each inner `Vec<Point>` is one continuous pen-down
/// stroke, ready for a `D02 ...; D01 ...; D01 ...` Gerber run. Used
/// by the Gerber writer to emit one move per polyline instead of one
/// move per segment, which shrinks long silk text by a large factor.
///
/// The SVG renderer keeps using `text_segments` because pad-overlap
/// suppression operates on individual segments and re-grouping them
/// after clipping wouldn't save anything.
#[must_use]
pub fn text_polylines(
    text: &str,
    origin: Point,
    size: Length,
    rotation_deg: f32,
    anchor: SilkAnchor,
) -> Vec<Vec<Point>> {
    if text.is_empty() {
        return Vec::new();
    }
    let scale = size.to_mm() / CAP_HEIGHT_UNITS;
    let total_w = f64::from(text_advance_units(text)) * scale;
    let anchor_dx = match anchor {
        SilkAnchor::Start => 0.0,
        SilkAnchor::Middle => -total_w / 2.0,
        SilkAnchor::End => -total_w,
    };
    let theta = f64::from(rotation_deg).to_radians();
    let (sin_t, cos_t) = (theta.sin(), theta.cos());
    let ox = origin.x.to_mm();
    let oy = origin.y.to_mm();

    let to_world = |xu: f64, yu: f64| -> Point {
        let lx = anchor_dx + xu * scale;
        let ly = yu * scale;
        let wx = lx * cos_t - ly * sin_t + ox;
        let wy = lx * sin_t + ly * cos_t + oy;
        Point::new(Length::from_mm(wx), Length::from_mm(wy))
    };

    let mut out: Vec<Vec<Point>> = Vec::new();
    let mut pen_x: f64 = 0.0;
    for ch in text.chars() {
        let g = glyph(ch);
        for stroke in g.strokes {
            if stroke.len() < 2 {
                continue;
            }
            let mut poly = Vec::with_capacity(stroke.len());
            for &(xu, yu) in stroke.iter() {
                poly.push(to_world(pen_x + f64::from(xu), f64::from(yu)));
            }
            out.push(poly);
        }
        pen_x += f64::from(ADVANCE_UNITS);
    }
    out
}

/// Glyph lookup. Unknown characters return a 12×24 rectangle as a
/// placeholder so missing chars are visually obvious without breaking
/// the layout.
#[must_use]
pub fn glyph(c: char) -> &'static Glyph {
    // ASCII fast path — every glyph in 0x20..=0x7E lives in `ASCII`.
    let code = c as u32;
    if (0x20..=0x7E).contains(&code) {
        let idx = (code - 0x20) as usize;
        if let Some(g) = ASCII[idx] {
            return g;
        }
    }
    &PLACEHOLDER
}

// Glyph data follows. Every glyph is laid out with the baseline at
// y=0, x-height around y=18, cap top at y=28, and ascenders/descenders
// near y=32 / y=-8. Glyph pen origin is at x=2 to leave a small left
// side-bearing within the 24-unit advance.

const PLACEHOLDER: Glyph = Glyph {
    strokes: &[&[(4, 0), (20, 0), (20, 28), (4, 28), (4, 0)]],
};

// SPACE — no strokes.
const G_SPACE: Glyph = Glyph { strokes: &[] };

// Punctuation, common ASCII.
const G_EXCL: Glyph = Glyph {
    strokes: &[&[(12, 28), (12, 8)], &[(12, 4), (12, 0)]],
};
const G_QUOTE: Glyph = Glyph {
    strokes: &[&[(8, 28), (8, 22)], &[(16, 28), (16, 22)]],
};
const G_HASH: Glyph = Glyph {
    strokes: &[
        &[(8, 28), (8, 0)],
        &[(16, 28), (16, 0)],
        &[(4, 20), (20, 20)],
        &[(4, 8), (20, 8)],
    ],
};
const G_DOLLAR: Glyph = Glyph {
    strokes: &[
        &[(20, 24), (4, 24), (4, 16), (20, 12), (20, 4), (4, 4)],
        &[(12, 30), (12, -2)],
    ],
};
const G_PERCENT: Glyph = Glyph {
    strokes: &[
        &[(4, 0), (20, 28)],
        &[(6, 28), (10, 28), (10, 24), (6, 24), (6, 28)],
        &[(14, 4), (18, 4), (18, 0), (14, 0), (14, 4)],
    ],
};
const G_AMP: Glyph = Glyph {
    strokes: &[&[
        (20, 0),
        (4, 16),
        (4, 24),
        (12, 28),
        (16, 24),
        (16, 18),
        (4, 8),
        (4, 4),
        (8, 0),
        (16, 0),
        (20, 4),
    ]],
};
const G_APOS: Glyph = Glyph {
    strokes: &[&[(12, 28), (12, 22)]],
};
const G_LPAREN: Glyph = Glyph {
    strokes: &[&[(16, 32), (8, 24), (8, 4), (16, -4)]],
};
const G_RPAREN: Glyph = Glyph {
    strokes: &[&[(8, 32), (16, 24), (16, 4), (8, -4)]],
};
const G_STAR: Glyph = Glyph {
    strokes: &[
        &[(12, 24), (12, 8)],
        &[(4, 20), (20, 12)],
        &[(20, 20), (4, 12)],
    ],
};
const G_PLUS: Glyph = Glyph {
    strokes: &[&[(12, 22), (12, 6)], &[(4, 14), (20, 14)]],
};
const G_COMMA: Glyph = Glyph {
    strokes: &[&[(12, 4), (12, 0), (8, -4)]],
};
const G_MINUS: Glyph = Glyph {
    strokes: &[&[(4, 14), (20, 14)]],
};
const G_DOT: Glyph = Glyph {
    strokes: &[&[(12, 4), (12, 0)]],
};
const G_SLASH: Glyph = Glyph {
    strokes: &[&[(4, 0), (20, 28)]],
};

// Digits.
const G_0: Glyph = Glyph {
    strokes: &[
        &[
            (8, 0),
            (4, 4),
            (4, 24),
            (8, 28),
            (16, 28),
            (20, 24),
            (20, 4),
            (16, 0),
            (8, 0),
        ],
        &[(4, 4), (20, 24)],
    ],
};
const G_1: Glyph = Glyph {
    strokes: &[&[(8, 22), (12, 28), (12, 0)], &[(6, 0), (18, 0)]],
};
const G_2: Glyph = Glyph {
    strokes: &[&[
        (4, 24),
        (8, 28),
        (16, 28),
        (20, 24),
        (20, 18),
        (4, 0),
        (20, 0),
    ]],
};
const G_3: Glyph = Glyph {
    strokes: &[
        &[
            (4, 24),
            (8, 28),
            (16, 28),
            (20, 24),
            (20, 18),
            (16, 14),
            (8, 14),
        ],
        &[(16, 14), (20, 10), (20, 4), (16, 0), (8, 0), (4, 4)],
    ],
};
const G_4: Glyph = Glyph {
    strokes: &[&[(16, 0), (16, 28), (4, 10), (20, 10)]],
};
const G_5: Glyph = Glyph {
    strokes: &[&[
        (20, 28),
        (4, 28),
        (4, 16),
        (16, 16),
        (20, 12),
        (20, 4),
        (16, 0),
        (8, 0),
        (4, 4),
    ]],
};
const G_6: Glyph = Glyph {
    strokes: &[&[
        (20, 24),
        (16, 28),
        (8, 28),
        (4, 24),
        (4, 4),
        (8, 0),
        (16, 0),
        (20, 4),
        (20, 12),
        (16, 16),
        (4, 16),
    ]],
};
const G_7: Glyph = Glyph {
    strokes: &[&[(4, 28), (20, 28), (8, 0)]],
};
const G_8: Glyph = Glyph {
    strokes: &[
        &[
            (8, 28),
            (4, 24),
            (4, 18),
            (8, 14),
            (16, 14),
            (20, 18),
            (20, 24),
            (16, 28),
            (8, 28),
        ],
        &[
            (8, 14),
            (4, 10),
            (4, 4),
            (8, 0),
            (16, 0),
            (20, 4),
            (20, 10),
            (16, 14),
        ],
    ],
};
const G_9: Glyph = Glyph {
    strokes: &[&[
        (4, 4),
        (8, 0),
        (16, 0),
        (20, 4),
        (20, 24),
        (16, 28),
        (8, 28),
        (4, 24),
        (4, 16),
        (8, 12),
        (20, 12),
    ]],
};

const G_COLON: Glyph = Glyph {
    strokes: &[&[(12, 18), (12, 14)], &[(12, 4), (12, 0)]],
};
const G_SEMI: Glyph = Glyph {
    strokes: &[&[(12, 18), (12, 14)], &[(12, 4), (12, 0), (8, -4)]],
};
const G_LT: Glyph = Glyph {
    strokes: &[&[(20, 22), (4, 14), (20, 6)]],
};
const G_EQ: Glyph = Glyph {
    strokes: &[&[(4, 18), (20, 18)], &[(4, 10), (20, 10)]],
};
const G_GT: Glyph = Glyph {
    strokes: &[&[(4, 22), (20, 14), (4, 6)]],
};
const G_QUESTION: Glyph = Glyph {
    strokes: &[
        &[
            (4, 24),
            (8, 28),
            (16, 28),
            (20, 24),
            (20, 18),
            (12, 12),
            (12, 8),
        ],
        &[(12, 4), (12, 0)],
    ],
};
const G_AT: Glyph = Glyph {
    strokes: &[&[
        (20, 8),
        (16, 4),
        (8, 4),
        (4, 8),
        (4, 20),
        (8, 24),
        (16, 24),
        (20, 20),
        (20, 8),
        (16, 8),
        (14, 12),
        (14, 18),
        (12, 20),
        (10, 18),
        (10, 12),
        (12, 10),
        (16, 10),
    ]],
};

// Uppercase A-Z.
const G_A: Glyph = Glyph {
    strokes: &[
        &[(4, 0), (4, 22), (12, 28), (20, 22), (20, 0)],
        &[(4, 12), (20, 12)],
    ],
};
const G_B: Glyph = Glyph {
    strokes: &[
        &[
            (4, 0),
            (4, 28),
            (16, 28),
            (20, 24),
            (20, 18),
            (16, 14),
            (4, 14),
        ],
        &[(16, 14), (20, 10), (20, 4), (16, 0), (4, 0)],
    ],
};
const G_C: Glyph = Glyph {
    strokes: &[&[
        (20, 24),
        (16, 28),
        (8, 28),
        (4, 24),
        (4, 4),
        (8, 0),
        (16, 0),
        (20, 4),
    ]],
};
const G_D: Glyph = Glyph {
    strokes: &[&[
        (4, 0),
        (4, 28),
        (14, 28),
        (20, 22),
        (20, 6),
        (14, 0),
        (4, 0),
    ]],
};
const G_E: Glyph = Glyph {
    strokes: &[&[(20, 28), (4, 28), (4, 0), (20, 0)], &[(4, 14), (16, 14)]],
};
const G_F: Glyph = Glyph {
    strokes: &[&[(20, 28), (4, 28), (4, 0)], &[(4, 14), (16, 14)]],
};
const G_G: Glyph = Glyph {
    strokes: &[&[
        (20, 24),
        (16, 28),
        (8, 28),
        (4, 24),
        (4, 4),
        (8, 0),
        (16, 0),
        (20, 4),
        (20, 12),
        (12, 12),
    ]],
};
const G_H: Glyph = Glyph {
    strokes: &[
        &[(4, 0), (4, 28)],
        &[(20, 0), (20, 28)],
        &[(4, 14), (20, 14)],
    ],
};
const G_I: Glyph = Glyph {
    strokes: &[
        &[(8, 0), (16, 0)],
        &[(12, 0), (12, 28)],
        &[(8, 28), (16, 28)],
    ],
};
const G_J: Glyph = Glyph {
    strokes: &[&[(20, 28), (20, 4), (16, 0), (8, 0), (4, 4)]],
};
const G_K: Glyph = Glyph {
    strokes: &[&[(4, 0), (4, 28)], &[(20, 28), (4, 14), (20, 0)]],
};
const G_L: Glyph = Glyph {
    strokes: &[&[(4, 28), (4, 0), (20, 0)]],
};
const G_M: Glyph = Glyph {
    strokes: &[&[(4, 0), (4, 28), (12, 14), (20, 28), (20, 0)]],
};
const G_N: Glyph = Glyph {
    strokes: &[&[(4, 0), (4, 28), (20, 0), (20, 28)]],
};
const G_O: Glyph = Glyph {
    strokes: &[&[
        (8, 0),
        (4, 4),
        (4, 24),
        (8, 28),
        (16, 28),
        (20, 24),
        (20, 4),
        (16, 0),
        (8, 0),
    ]],
};
const G_P: Glyph = Glyph {
    strokes: &[&[
        (4, 0),
        (4, 28),
        (16, 28),
        (20, 24),
        (20, 18),
        (16, 14),
        (4, 14),
    ]],
};
const G_Q: Glyph = Glyph {
    strokes: &[
        &[
            (8, 0),
            (4, 4),
            (4, 24),
            (8, 28),
            (16, 28),
            (20, 24),
            (20, 4),
            (16, 0),
            (8, 0),
        ],
        &[(14, 6), (22, -2)],
    ],
};
const G_R: Glyph = Glyph {
    strokes: &[
        &[
            (4, 0),
            (4, 28),
            (16, 28),
            (20, 24),
            (20, 18),
            (16, 14),
            (4, 14),
        ],
        &[(12, 14), (20, 0)],
    ],
};
const G_S: Glyph = Glyph {
    strokes: &[&[
        (20, 24),
        (16, 28),
        (8, 28),
        (4, 24),
        (4, 18),
        (8, 14),
        (16, 14),
        (20, 10),
        (20, 4),
        (16, 0),
        (8, 0),
        (4, 4),
    ]],
};
const G_T: Glyph = Glyph {
    strokes: &[&[(4, 28), (20, 28)], &[(12, 28), (12, 0)]],
};
const G_U: Glyph = Glyph {
    strokes: &[&[(4, 28), (4, 4), (8, 0), (16, 0), (20, 4), (20, 28)]],
};
const G_V: Glyph = Glyph {
    strokes: &[&[(4, 28), (12, 0), (20, 28)]],
};
const G_W: Glyph = Glyph {
    strokes: &[&[(4, 28), (8, 0), (12, 14), (16, 0), (20, 28)]],
};
const G_X: Glyph = Glyph {
    strokes: &[&[(4, 28), (20, 0)], &[(4, 0), (20, 28)]],
};
const G_Y: Glyph = Glyph {
    strokes: &[&[(4, 28), (12, 14), (20, 28)], &[(12, 14), (12, 0)]],
};
const G_Z: Glyph = Glyph {
    strokes: &[&[(4, 28), (20, 28), (4, 0), (20, 0)]],
};

const G_LBRACK: Glyph = Glyph {
    strokes: &[&[(16, 32), (8, 32), (8, -4), (16, -4)]],
};
const G_BACKSLASH: Glyph = Glyph {
    strokes: &[&[(4, 28), (20, 0)]],
};
const G_RBRACK: Glyph = Glyph {
    strokes: &[&[(8, 32), (16, 32), (16, -4), (8, -4)]],
};
const G_CARET: Glyph = Glyph {
    strokes: &[&[(4, 22), (12, 30), (20, 22)]],
};
const G_UNDER: Glyph = Glyph {
    strokes: &[&[(0, -2), (24, -2)]],
};
const G_BACKTICK: Glyph = Glyph {
    strokes: &[&[(8, 30), (14, 24)]],
};

// Lowercase a-z. We implement them as visually distinct simplified
// glyphs that share the uppercase advance — silkscreen at 1.5 mm
// reads case more from height than from glyph identity. Lowercase
// glyphs are drawn at ~22 units cap (x-height) so they sit lower
// than capitals on the same baseline.
const G_a: Glyph = Glyph {
    strokes: &[
        &[(4, 4), (4, 14), (8, 18), (16, 18), (20, 14), (20, 0)],
        &[(20, 8), (4, 8)],
    ],
};
const G_b: Glyph = Glyph {
    strokes: &[&[
        (4, 28),
        (4, 0),
        (16, 0),
        (20, 4),
        (20, 14),
        (16, 18),
        (8, 18),
        (4, 14),
    ]],
};
const G_c: Glyph = Glyph {
    strokes: &[&[
        (20, 14),
        (16, 18),
        (8, 18),
        (4, 14),
        (4, 4),
        (8, 0),
        (16, 0),
        (20, 4),
    ]],
};
const G_d: Glyph = Glyph {
    strokes: &[&[
        (20, 28),
        (20, 0),
        (8, 0),
        (4, 4),
        (4, 14),
        (8, 18),
        (16, 18),
        (20, 14),
    ]],
};
const G_e: Glyph = Glyph {
    strokes: &[&[
        (4, 8),
        (20, 8),
        (20, 14),
        (16, 18),
        (8, 18),
        (4, 14),
        (4, 4),
        (8, 0),
        (16, 0),
        (20, 4),
    ]],
};
const G_f: Glyph = Glyph {
    strokes: &[&[(20, 28), (12, 28), (8, 24), (8, 0)], &[(4, 14), (16, 14)]],
};
const G_g: Glyph = Glyph {
    strokes: &[
        &[(20, 18), (4, 18), (4, 8), (20, 8)],
        &[(20, 18), (20, -4), (16, -8), (8, -8), (4, -4)],
    ],
};
const G_h: Glyph = Glyph {
    strokes: &[
        &[(4, 28), (4, 0)],
        &[(4, 14), (8, 18), (16, 18), (20, 14), (20, 0)],
    ],
};
const G_i: Glyph = Glyph {
    strokes: &[&[(12, 24), (12, 22)], &[(12, 18), (12, 0)]],
};
const G_j: Glyph = Glyph {
    strokes: &[
        &[(16, 24), (16, 22)],
        &[(16, 18), (16, -4), (12, -8), (4, -8)],
    ],
};
const G_k: Glyph = Glyph {
    strokes: &[&[(4, 28), (4, 0)], &[(20, 18), (4, 8), (20, 0)]],
};
const G_l: Glyph = Glyph {
    strokes: &[&[(8, 28), (12, 28), (12, 0), (16, 0)]],
};
const G_m: Glyph = Glyph {
    strokes: &[
        &[(4, 0), (4, 18), (10, 18), (12, 14), (12, 0)],
        &[(12, 14), (14, 18), (20, 18), (20, 0)],
    ],
};
const G_n: Glyph = Glyph {
    strokes: &[&[(4, 0), (4, 18), (8, 18), (20, 4), (20, 0)]],
};
const G_o: Glyph = Glyph {
    strokes: &[&[
        (8, 0),
        (4, 4),
        (4, 14),
        (8, 18),
        (16, 18),
        (20, 14),
        (20, 4),
        (16, 0),
        (8, 0),
    ]],
};
const G_p: Glyph = Glyph {
    strokes: &[&[
        (4, -8),
        (4, 18),
        (16, 18),
        (20, 14),
        (20, 4),
        (16, 0),
        (8, 0),
        (4, 4),
    ]],
};
const G_q: Glyph = Glyph {
    strokes: &[&[
        (20, -8),
        (20, 18),
        (8, 18),
        (4, 14),
        (4, 4),
        (8, 0),
        (16, 0),
        (20, 4),
    ]],
};
const G_r: Glyph = Glyph {
    strokes: &[&[(4, 0), (4, 18)], &[(4, 14), (8, 18), (20, 18)]],
};
const G_s: Glyph = Glyph {
    strokes: &[&[
        (20, 14),
        (16, 18),
        (8, 18),
        (4, 14),
        (8, 8),
        (16, 8),
        (20, 4),
        (16, 0),
        (8, 0),
        (4, 4),
    ]],
};
const G_t: Glyph = Glyph {
    strokes: &[&[(8, 28), (8, 4), (12, 0), (20, 0)], &[(4, 18), (16, 18)]],
};
const G_u: Glyph = Glyph {
    strokes: &[
        &[(4, 18), (4, 4), (8, 0), (16, 0), (20, 4), (20, 18)],
        &[(20, 4), (20, 0)],
    ],
};
const G_v: Glyph = Glyph {
    strokes: &[&[(4, 18), (12, 0), (20, 18)]],
};
const G_w: Glyph = Glyph {
    strokes: &[&[(4, 18), (8, 0), (12, 10), (16, 0), (20, 18)]],
};
const G_x: Glyph = Glyph {
    strokes: &[&[(4, 18), (20, 0)], &[(4, 0), (20, 18)]],
};
const G_y: Glyph = Glyph {
    strokes: &[&[(4, 18), (12, 4)], &[(20, 18), (8, -8), (4, -8)]],
};
const G_z: Glyph = Glyph {
    strokes: &[&[(4, 18), (20, 18), (4, 0), (20, 0)]],
};

const G_LBRACE: Glyph = Glyph {
    strokes: &[&[
        (20, 32),
        (16, 28),
        (16, 18),
        (12, 14),
        (16, 10),
        (16, 0),
        (20, -4),
    ]],
};
const G_PIPE: Glyph = Glyph {
    strokes: &[&[(12, 32), (12, -4)]],
};
const G_RBRACE: Glyph = Glyph {
    strokes: &[&[
        (4, 32),
        (8, 28),
        (8, 18),
        (12, 14),
        (8, 10),
        (8, 0),
        (4, -4),
    ]],
};
const G_TILDE: Glyph = Glyph {
    strokes: &[&[(4, 14), (8, 18), (16, 10), (20, 14)]],
};

/// 0x20..=0x7E indexed glyph table. `None` falls back to the
/// placeholder rect.
#[rustfmt::skip]
const ASCII: [Option<&'static Glyph>; 0x7E - 0x20 + 1] = [
    /* 0x20 ' ' */ Some(&G_SPACE),
    /* 0x21 '!' */ Some(&G_EXCL),
    /* 0x22 '"' */ Some(&G_QUOTE),
    /* 0x23 '#' */ Some(&G_HASH),
    /* 0x24 '$' */ Some(&G_DOLLAR),
    /* 0x25 '%' */ Some(&G_PERCENT),
    /* 0x26 '&' */ Some(&G_AMP),
    /* 0x27 '\'' */ Some(&G_APOS),
    /* 0x28 '(' */ Some(&G_LPAREN),
    /* 0x29 ')' */ Some(&G_RPAREN),
    /* 0x2A '*' */ Some(&G_STAR),
    /* 0x2B '+' */ Some(&G_PLUS),
    /* 0x2C ',' */ Some(&G_COMMA),
    /* 0x2D '-' */ Some(&G_MINUS),
    /* 0x2E '.' */ Some(&G_DOT),
    /* 0x2F '/' */ Some(&G_SLASH),
    /* 0x30 '0' */ Some(&G_0),
    /* 0x31 '1' */ Some(&G_1),
    /* 0x32 '2' */ Some(&G_2),
    /* 0x33 '3' */ Some(&G_3),
    /* 0x34 '4' */ Some(&G_4),
    /* 0x35 '5' */ Some(&G_5),
    /* 0x36 '6' */ Some(&G_6),
    /* 0x37 '7' */ Some(&G_7),
    /* 0x38 '8' */ Some(&G_8),
    /* 0x39 '9' */ Some(&G_9),
    /* 0x3A ':' */ Some(&G_COLON),
    /* 0x3B ';' */ Some(&G_SEMI),
    /* 0x3C '<' */ Some(&G_LT),
    /* 0x3D '=' */ Some(&G_EQ),
    /* 0x3E '>' */ Some(&G_GT),
    /* 0x3F '?' */ Some(&G_QUESTION),
    /* 0x40 '@' */ Some(&G_AT),
    /* 0x41 'A' */ Some(&G_A),
    /* 0x42 'B' */ Some(&G_B),
    /* 0x43 'C' */ Some(&G_C),
    /* 0x44 'D' */ Some(&G_D),
    /* 0x45 'E' */ Some(&G_E),
    /* 0x46 'F' */ Some(&G_F),
    /* 0x47 'G' */ Some(&G_G),
    /* 0x48 'H' */ Some(&G_H),
    /* 0x49 'I' */ Some(&G_I),
    /* 0x4A 'J' */ Some(&G_J),
    /* 0x4B 'K' */ Some(&G_K),
    /* 0x4C 'L' */ Some(&G_L),
    /* 0x4D 'M' */ Some(&G_M),
    /* 0x4E 'N' */ Some(&G_N),
    /* 0x4F 'O' */ Some(&G_O),
    /* 0x50 'P' */ Some(&G_P),
    /* 0x51 'Q' */ Some(&G_Q),
    /* 0x52 'R' */ Some(&G_R),
    /* 0x53 'S' */ Some(&G_S),
    /* 0x54 'T' */ Some(&G_T),
    /* 0x55 'U' */ Some(&G_U),
    /* 0x56 'V' */ Some(&G_V),
    /* 0x57 'W' */ Some(&G_W),
    /* 0x58 'X' */ Some(&G_X),
    /* 0x59 'Y' */ Some(&G_Y),
    /* 0x5A 'Z' */ Some(&G_Z),
    /* 0x5B '[' */ Some(&G_LBRACK),
    /* 0x5C '\\' */ Some(&G_BACKSLASH),
    /* 0x5D ']' */ Some(&G_RBRACK),
    /* 0x5E '^' */ Some(&G_CARET),
    /* 0x5F '_' */ Some(&G_UNDER),
    /* 0x60 '`' */ Some(&G_BACKTICK),
    /* 0x61 'a' */ Some(&G_a),
    /* 0x62 'b' */ Some(&G_b),
    /* 0x63 'c' */ Some(&G_c),
    /* 0x64 'd' */ Some(&G_d),
    /* 0x65 'e' */ Some(&G_e),
    /* 0x66 'f' */ Some(&G_f),
    /* 0x67 'g' */ Some(&G_g),
    /* 0x68 'h' */ Some(&G_h),
    /* 0x69 'i' */ Some(&G_i),
    /* 0x6A 'j' */ Some(&G_j),
    /* 0x6B 'k' */ Some(&G_k),
    /* 0x6C 'l' */ Some(&G_l),
    /* 0x6D 'm' */ Some(&G_m),
    /* 0x6E 'n' */ Some(&G_n),
    /* 0x6F 'o' */ Some(&G_o),
    /* 0x70 'p' */ Some(&G_p),
    /* 0x71 'q' */ Some(&G_q),
    /* 0x72 'r' */ Some(&G_r),
    /* 0x73 's' */ Some(&G_s),
    /* 0x74 't' */ Some(&G_t),
    /* 0x75 'u' */ Some(&G_u),
    /* 0x76 'v' */ Some(&G_v),
    /* 0x77 'w' */ Some(&G_w),
    /* 0x78 'x' */ Some(&G_x),
    /* 0x79 'y' */ Some(&G_y),
    /* 0x7A 'z' */ Some(&G_z),
    /* 0x7B '{' */ Some(&G_LBRACE),
    /* 0x7C '|' */ Some(&G_PIPE),
    /* 0x7D '}' */ Some(&G_RBRACE),
    /* 0x7E '~' */ Some(&G_TILDE),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcb_renders_some_segments() {
        let segs = text_segments(
            "PCB",
            Point::ORIGIN,
            Length::from_mm(2.0),
            0.0,
            SilkAnchor::Start,
        );
        assert!(!segs.is_empty(), "PCB should render at least one segment");
    }

    #[test]
    fn ab_wider_than_a() {
        assert!(text_width_units("AB") > text_width_units("A"));
    }

    #[test]
    fn unknown_char_falls_back() {
        // Box-drawing char outside ASCII -> placeholder rect.
        let segs = text_segments(
            "═",
            Point::ORIGIN,
            Length::from_mm(2.0),
            0.0,
            SilkAnchor::Start,
        );
        assert!(!segs.is_empty());
    }
}
