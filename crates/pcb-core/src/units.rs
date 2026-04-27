//! Internal unit system.
//!
//! All coordinates and lengths are stored as `Length`, a fixed-point
//! integer count of nanometres. This avoids floating-point drift across
//! routing and DRC, matches what fab houses ultimately consume (Gerbers
//! are typically expressed at nm resolution), and keeps equality and
//! hashing well-defined.
//!
//! Conversion helpers are provided for the two human-facing units we
//! care about: millimetres (the schematic/board author-facing unit) and
//! mils (1/1000 inch, common in legacy footprints).

use serde::{Deserialize, Serialize};

/// One nanometre, expressed as `Length`.
pub const NM: Length = Length(1);
/// One micrometre.
pub const UM: Length = Length(1_000);
/// One millimetre.
pub const MM: Length = Length(1_000_000);
/// One mil (1/1000 inch = 25.4 µm).
pub const MIL: Length = Length(25_400);

/// A length in nanometres.
///
/// `Length` is the canonical scalar for every coordinate, distance and
/// dimension in the project. It is `i64` so subtraction (delta vectors)
/// and signed comparisons work without surprises.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Length(pub i64);

impl Length {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub fn from_mm(mm: f64) -> Self {
        #[allow(clippy::cast_possible_truncation)]
        Self((mm * 1_000_000.0).round() as i64)
    }

    #[must_use]
    pub fn from_mil(mil: f64) -> Self {
        #[allow(clippy::cast_possible_truncation)]
        Self((mil * 25_400.0).round() as i64)
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn to_mm(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    #[must_use]
    pub fn abs(self) -> Self {
        Self(self.0.abs())
    }
}

impl std::ops::Add for Length {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl std::ops::Sub for Length {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl std::ops::Neg for Length {
    type Output = Self;
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

impl std::ops::Mul<i64> for Length {
    type Output = Self;
    fn mul(self, rhs: i64) -> Self {
        Self(self.0 * rhs)
    }
}

impl std::ops::Div<i64> for Length {
    type Output = Self;
    fn div(self, rhs: i64) -> Self {
        Self(self.0 / rhs)
    }
}
