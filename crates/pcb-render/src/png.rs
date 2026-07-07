//! SVG -> PNG rasterisation for the headless screenshot endpoint.
//!
//! The agents driving Fragua from the local HTTP API need a way to
//! verify what the UI is showing without depending on OS-level screen
//! capture (which on macOS requires accessibility permissions for the
//! Tauri-spawned `fragua` binary). Rather than capturing the actual
//! webview pixels we re-render the same SVG the webview consumes, into
//! a PNG, in the same Rust process. The result is identical to what an
//! operator sees in the canvas — just minus any human pan/zoom state.
//!
//! This module is deliberately small: SVG string in, PNG bytes out. Any
//! caller that already has an SVG (board, schematic, library entry)
//! can rasterise it through `svg_to_png`.

use pcb_core::{schematic::Schematic, Board, LibraryEntry};
use resvg::tiny_skia;
use usvg::{Options, Tree};

/// Errors that can happen while rasterising an SVG. Kept as a plain
/// `String` so the HTTP layer can stuff it straight into a 500 body
/// without a custom error type.
pub type RenderError = String;

/// Default width (in pixels) for screenshot output when the caller
/// doesn't specify a size. 1600 is wide enough to read pad labels and
/// silk text without being absurdly large for a thumbnail download.
pub const DEFAULT_PNG_WIDTH: u32 = 1600;

/// Maximum PNG dimension we accept from the API. Caps memory usage so
/// a hostile / clumsy caller can't ask for an 80000x80000 frame.
pub const MAX_PNG_DIMENSION: u32 = 8192;

/// Rasterise an SVG string into a PNG byte vector. `target_width_px`
/// drives the scale; the height is computed from the SVG's intrinsic
/// aspect ratio. The background is whatever the SVG paints — the board
/// SVG paints its own dark substrate, the schematic SVG paints white.
pub fn svg_to_png(svg: &str, target_width_px: u32) -> Result<Vec<u8>, RenderError> {
    let width = target_width_px.clamp(64, MAX_PNG_DIMENSION);

    let mut opts = Options::default();
    opts.fontdb_mut().load_system_fonts();
    let tree = Tree::from_str(svg, &opts).map_err(|e| format!("parse svg: {e}"))?;

    let svg_size = tree.size();
    if svg_size.width() <= 0.0 || svg_size.height() <= 0.0 {
        return Err("svg has zero size".to_string());
    }
    let scale = f32::from(u16::try_from(width).unwrap_or(u16::MAX)) / svg_size.width();
    let pixmap_w = width;
    let pixmap_h = ((svg_size.height() * scale).round() as u32).clamp(1, MAX_PNG_DIMENSION);

    let mut pixmap = tiny_skia::Pixmap::new(pixmap_w, pixmap_h).ok_or("allocate pixmap failed")?;
    // Most of our SVGs paint a background rectangle of their own, but
    // a few (library entry thumbnails) are transparent — fill with
    // white so the resulting PNG is readable on dark terminal viewers.
    pixmap.fill(tiny_skia::Color::WHITE);

    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    pixmap.encode_png().map_err(|e| format!("encode png: {e}"))
}

/// Convenience: render a board to a PNG at the requested width.
pub fn render_board_png(board: &Board, width_px: u32) -> Result<Vec<u8>, RenderError> {
    svg_to_png(&crate::render_svg(board), width_px)
}

/// Same as `render_board_png` but consults `margins` for library-keyed
/// placement-margin body outlines (see `render_svg_with_margins`).
pub fn render_board_png_with_margins(
    board: &Board,
    margins: &crate::PlacementMarginMap,
    width_px: u32,
) -> Result<Vec<u8>, RenderError> {
    svg_to_png(&crate::render_svg_with_margins(board, margins), width_px)
}

/// Convenience: render a schematic to a PNG at the requested width.
pub fn render_schematic_png(schematic: &Schematic, width_px: u32) -> Result<Vec<u8>, RenderError> {
    svg_to_png(&crate::render_schematic_svg(schematic), width_px)
}

/// Convenience: render a single library entry's review SVG to PNG.
pub fn render_library_entry_png(
    entry: &LibraryEntry,
    width_px: u32,
) -> Result<Vec<u8>, RenderError> {
    svg_to_png(&crate::render_library_entry_svg(entry), width_px)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_core::Board;

    #[test]
    fn empty_board_renders_to_valid_png() {
        let board = Board::default();
        let png = render_board_png(&board, 400).expect("render");
        // PNG magic number: 89 50 4E 47 0D 0A 1A 0A
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "valid PNG header");
        assert!(png.len() > 100, "non-empty image");
    }

    #[test]
    fn small_width_is_clamped_to_minimum() {
        let board = Board::default();
        let png = render_board_png(&board, 10).expect("render");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
