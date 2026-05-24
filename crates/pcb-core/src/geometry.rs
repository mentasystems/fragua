//! 2D geometry primitives over `Length`.
//!
//! Everything stays in nanometres so comparisons, hashing, and bounding
//! boxes are exact. Floating-point only appears in `to_mm` conversions
//! at the user/render boundary.

use serde::{Deserialize, Serialize};

use crate::units::Length;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Point {
    pub x: Length,
    pub y: Length,
}

impl Point {
    pub const ORIGIN: Self = Self {
        x: Length::ZERO,
        y: Length::ZERO,
    };

    #[must_use]
    pub fn new(x: Length, y: Length) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub fn translate(self, dx: Length, dy: Length) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
        }
    }
}

/// Axis-aligned rectangle in board coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub min: Point,
    pub max: Point,
}

impl Rect {
    #[must_use]
    pub fn from_corners(a: Point, b: Point) -> Self {
        Self {
            min: Point {
                x: a.x.min(b.x),
                y: a.y.min(b.y),
            },
            max: Point {
                x: a.x.max(b.x),
                y: a.y.max(b.y),
            },
        }
    }

    #[must_use]
    pub fn from_center(center: Point, w: Length, h: Length) -> Self {
        Self {
            min: Point {
                x: center.x - w / 2,
                y: center.y - h / 2,
            },
            max: Point {
                x: center.x + w / 2,
                y: center.y + h / 2,
            },
        }
    }

    #[must_use]
    pub fn width(self) -> Length {
        self.max.x - self.min.x
    }

    #[must_use]
    pub fn height(self) -> Length {
        self.max.y - self.min.y
    }

    /// Rectangle that contains both `self` and `other`.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self {
            min: Point {
                x: self.min.x.min(other.min.x),
                y: self.min.y.min(other.min.y),
            },
            max: Point {
                x: self.max.x.max(other.max.x),
                y: self.max.y.max(other.max.y),
            },
        }
    }

    /// Expand outward by `margin` on every side.
    #[must_use]
    pub fn expand(self, margin: Length) -> Self {
        Self {
            min: self.min.translate(-margin, -margin),
            max: self.max.translate(margin, margin),
        }
    }

    /// True if the two rectangles share any interior area. Touching
    /// edges (zero-area overlap) counts as non-intersecting so a tight
    /// edge-to-edge layout passes the check.
    #[must_use]
    pub fn intersects(&self, other: &Self) -> bool {
        self.min.x.0 < other.max.x.0
            && self.max.x.0 > other.min.x.0
            && self.min.y.0 < other.max.y.0
            && self.max.y.0 > other.min.y.0
    }
}
