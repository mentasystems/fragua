//! Fab capability profiles.
//!
//! A `FabProfile` is a single fab house's published minimums — trace
//! width, drill, annular ring, etc. `pcb-drc` consumes it via
//! `DrcOptions::fab_profile`. When set, every minimum-style check
//! gates against the profile alongside the project-side defaults, and
//! emits `ViolationKind::FabProfileMin` on a hit.
//!
//! The presets here are sourced from each fab's published capability
//! page (2024-2025). When in doubt we pick the looser side of any
//! quoted range so a board passing the check also passes the fab's
//! intake review without an exception.

// Re-export the canonical type from `pcb_drc` so callers only need to
// `use pcb_fab::FabProfile` regardless of which crate owns the struct.
pub use pcb_drc::FabProfile;

/// JLCPCB standard 2-layer capability. Published at
/// jlcpcb.com/capabilities/pcb-capabilities (numbers as of 2024-2025).
/// 0.127 mm = 5 mil for trace width and clearance, the standard tier
/// minimums. Annular ring 0.13 mm matches their published "minimum
/// drill-to-copper" of 0.13 mm. Standard board size 100×100 mm at the
/// no-extra-cost tier; bigger boards are accepted at higher prices.
#[must_use]
pub fn jlcpcb_2layer() -> FabProfile {
    FabProfile {
        name: "jlcpcb_2layer".into(),
        min_trace_width_mm: 0.127,
        min_clearance_mm: 0.127,
        min_drill_mm: 0.20,
        min_annular_ring_mm: 0.13,
        min_via_diameter_mm: 0.45,
        min_edge_clearance_mm: 0.20,
        max_board_size_mm: (100.0, 100.0),
    }
}

/// PCBWay standard 2-layer capability. Published at
/// pcbway.com/capabilities.html. Default tier is 6/6 mil
/// (0.152 mm) for trace / clearance; 4/4 mil is available with a
/// surcharge. We encode the cheap tier since boards passing here also
/// pass the more permissive tier. Larger panels are accepted up to
/// 600 mm but priced as oversized — we cap at 200 mm.
#[must_use]
pub fn pcbway_standard() -> FabProfile {
    FabProfile {
        name: "pcbway_standard".into(),
        min_trace_width_mm: 0.152,
        min_clearance_mm: 0.152,
        min_drill_mm: 0.30,
        min_annular_ring_mm: 0.15,
        min_via_diameter_mm: 0.60,
        min_edge_clearance_mm: 0.20,
        max_board_size_mm: (200.0, 200.0),
    }
}

/// OSH Park 4-layer "Super Swift Service". Published at
/// docs.oshpark.com under "4 Layer". Their headline minimum is
/// 5 mil / 5 mil = 0.127 mm and a 10 mil (0.254 mm) minimum drill;
/// max board size is the standard 5 × 10 inch panel (127 × 254 mm).
/// Annular ring 5 mil = 0.127 mm. We use these conservatively — for
/// 2-layer the same numbers apply.
#[must_use]
pub fn oshpark_4layer() -> FabProfile {
    FabProfile {
        name: "oshpark_4layer".into(),
        min_trace_width_mm: 0.127,
        min_clearance_mm: 0.127,
        min_drill_mm: 0.254,
        min_annular_ring_mm: 0.127,
        min_via_diameter_mm: 0.508,
        min_edge_clearance_mm: 0.381,
        max_board_size_mm: (127.0, 254.0),
    }
}

/// Look up a profile by lowercase name. Returns `None` for unknown
/// keys so the caller can surface "supported: ..." in errors.
#[must_use]
pub fn by_name(s: &str) -> Option<FabProfile> {
    match s.to_ascii_lowercase().as_str() {
        "jlcpcb" | "jlc" | "jlcpcb_2layer" => Some(jlcpcb_2layer()),
        "pcbway" | "pcbway_standard" => Some(pcbway_standard()),
        "oshpark" | "oshpark_4layer" => Some(oshpark_4layer()),
        _ => None,
    }
}
