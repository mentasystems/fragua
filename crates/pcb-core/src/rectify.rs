//! Document-scanner-style photo rectification.
//!
//! Given four board corners in a handheld (perspective-distorted) photo
//! and the board's real width × height in mm, we compute the homography
//! that maps the corner quad to an axis-aligned rectangle and inverse-warp
//! the source image with bilinear sampling. Because the output resolution
//! is fixed at a known px-per-mm, the rectified image is metrically exact
//! by construction: 1 px always covers the same physical distance.
//!
//! No OpenCV — the 3×3 homography is solved with a tiny hand-rolled DLT
//! (an 8×8 linear system, Gaussian elimination) and inverted directly.
//!
//! Corner order: the caller passes the four corners as they should appear
//! in the OUTPUT, going TL → TR → BR → BL (top-left, top-right,
//! bottom-right, bottom-left of the rectified rectangle). That choice is
//! what fixes the rectified image's orientation — pick the source corner
//! that you want at the output's top-left as the first element, and so on
//! clockwise.

use image::{ImageFormat, RgbImage};

/// Fixed output resolution: 40 pixels per millimetre. A 40 mm board
/// rectifies to 1600 px on that side — plenty for on-screen overlays and
/// review without bloating the attachment.
pub const DEFAULT_PX_PER_MM: f64 = 40.0;

/// Cap the long side of the rectified image so a large module doesn't
/// produce a needlessly huge file; the effective px/mm is reduced to fit.
pub const MAX_LONG_SIDE_PX: f64 = 4000.0;

/// Result of a successful rectification.
pub struct Rectified {
    /// JPEG-encoded rectified image.
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Actual pixels-per-mm used (== `DEFAULT_PX_PER_MM` unless the long
    /// side was capped down).
    pub px_per_mm: f64,
    /// The 3×3 homography (row-major, 9 elements) mapping a SOURCE pixel
    /// `(x, y)` to the OUTPUT/rectified pixel `(u, v)`. Callers use this
    /// to carry annotations (e.g. calibration pin marks) from the source
    /// image into the rectified one.
    pub src_to_dst: [f64; 9],
}

/// Apply a 3×3 homography (row-major) to a point, dividing through by the
/// homogeneous `w`. Returns `None` if the point maps to the line at
/// infinity (`w ≈ 0`).
#[must_use]
pub fn apply_homography(m: &[f64; 9], x: f64, y: f64) -> Option<(f64, f64)> {
    let w = m[6] * x + m[7] * y + m[8];
    if w.abs() < 1e-12 {
        return None;
    }
    Some((
        (m[0] * x + m[1] * y + m[2]) / w,
        (m[3] * x + m[4] * y + m[5]) / w,
    ))
}

/// Invert a 3×3 matrix (row-major). Returns `None` when singular.
#[must_use]
pub fn invert3x3(m: &[f64; 9]) -> Option<[f64; 9]> {
    let det = m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6])
        + m[2] * (m[3] * m[7] - m[4] * m[6]);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv_det = 1.0 / det;
    Some([
        (m[4] * m[8] - m[5] * m[7]) * inv_det,
        (m[2] * m[7] - m[1] * m[8]) * inv_det,
        (m[1] * m[5] - m[2] * m[4]) * inv_det,
        (m[5] * m[6] - m[3] * m[8]) * inv_det,
        (m[0] * m[8] - m[2] * m[6]) * inv_det,
        (m[2] * m[3] - m[0] * m[5]) * inv_det,
        (m[3] * m[7] - m[4] * m[6]) * inv_det,
        (m[1] * m[6] - m[0] * m[7]) * inv_det,
        (m[0] * m[4] - m[1] * m[3]) * inv_det,
    ])
}

/// Solve the 8×8 linear system `A·h = b` by Gaussian elimination with
/// partial pivoting. Returns `None` if the system is singular (degenerate
/// correspondences).
fn solve8(mut a: [[f64; 8]; 8], mut b: [f64; 8]) -> Option<[f64; 8]> {
    for col in 0..8 {
        // Partial pivot: find the largest-magnitude row at/below `col`.
        let mut pivot = col;
        for r in (col + 1)..8 {
            if a[r][col].abs() > a[pivot][col].abs() {
                pivot = r;
            }
        }
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        // Eliminate below.
        let pivot_row = a[col];
        let pivot_b = b[col];
        for r in (col + 1)..8 {
            let factor = a[r][col] / pivot_row[col];
            if factor == 0.0 {
                continue;
            }
            for (ar, pr) in a[r].iter_mut().zip(pivot_row.iter()).skip(col) {
                *ar -= factor * *pr;
            }
            b[r] -= factor * pivot_b;
        }
    }
    // Back-substitute.
    let mut h = [0.0; 8];
    for i in (0..8).rev() {
        let mut sum = b[i];
        for j in (i + 1)..8 {
            sum -= a[i][j] * h[j];
        }
        h[i] = sum / a[i][i];
    }
    Some(h)
}

/// Compute the homography mapping the four `src` points to the four `dst`
/// points (order-matched). Returns the 3×3 row-major matrix with
/// `h[8] = 1`. Errors when the correspondences are degenerate.
pub fn homography_4pt(src: &[[f64; 2]; 4], dst: &[[f64; 2]; 4]) -> Result<[f64; 9], String> {
    // Each correspondence (x,y)->(u,v) yields two rows in the 8-unknown
    // system for h0..h7 (h8 pinned to 1):
    //   h0 x + h1 y + h2 - u x h6 - u y h7 = u
    //   h3 x + h4 y + h5 - v x h6 - v y h7 = v
    let mut a = [[0.0; 8]; 8];
    let mut b = [0.0; 8];
    for i in 0..4 {
        let (x, y) = (src[i][0], src[i][1]);
        let (u, v) = (dst[i][0], dst[i][1]);
        let r0 = 2 * i;
        let r1 = 2 * i + 1;
        a[r0] = [x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y];
        b[r0] = u;
        a[r1] = [0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y];
        b[r1] = v;
    }
    let h = solve8(a, b).ok_or_else(|| "rectify: degenerate corner correspondences".to_string())?;
    Ok([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], 1.0])
}

/// Reject a corner quad that isn't a simple convex quadrilateral: the four
/// points must wind consistently (all cross products the same sign) with
/// no near-collinear vertex. This catches self-intersecting ("bowtie")
/// orders and degenerate (collinear / zero-area) quads before we even try
/// to solve the homography, so the error message is meaningful.
fn validate_quad(corners: &[[f64; 2]; 4]) -> Result<(), String> {
    // Overall polygon area (shoelace) — reject the near-zero case.
    let mut area2 = 0.0;
    for i in 0..4 {
        let j = (i + 1) % 4;
        area2 += corners[i][0] * corners[j][1] - corners[j][0] * corners[i][1];
    }
    if area2.abs() < 1e-6 {
        return Err("rectify: corner quad is degenerate (near-zero area / collinear)".into());
    }
    // Convexity + non-collinearity: cross product at each vertex must
    // share the sign of the polygon's winding and be non-trivial relative
    // to the edge lengths.
    let winding = area2.signum();
    for i in 0..4 {
        let p = corners[(i + 3) % 4];
        let c = corners[i];
        let n = corners[(i + 1) % 4];
        let e0 = [c[0] - p[0], c[1] - p[1]];
        let e1 = [n[0] - c[0], n[1] - c[1]];
        let cross = e0[0] * e1[1] - e0[1] * e1[0];
        let len = (e0[0] * e0[0] + e0[1] * e0[1]).sqrt() * (e1[0] * e1[0] + e1[1] * e1[1]).sqrt();
        if len < 1e-9 {
            return Err("rectify: corner quad has a zero-length edge".into());
        }
        // Normalised turn (|sin| between edges); tiny → collinear vertex.
        if (cross / len).abs() < 1e-4 {
            return Err("rectify: corner quad has a near-collinear vertex".into());
        }
        if cross * winding < 0.0 {
            return Err(
                "rectify: corner quad is not convex / is self-intersecting (check TL,TR,BR,BL order)"
                    .into(),
            );
        }
    }
    Ok(())
}

/// Bilinearly sample an `RgbImage` at fractional `(x, y)` (y-down). Points
/// outside the image clamp to the border pixel, so the warped edges stay
/// solid rather than smearing to black.
fn sample_bilinear(img: &RgbImage, x: f64, y: f64) -> [u8; 3] {
    let (w, h) = (i64::from(img.width()), i64::from(img.height()));
    let x0 = x.floor() as i64;
    let y0 = y.floor() as i64;
    let fx = x - x0 as f64;
    let fy = y - y0 as f64;
    let clamp = |v: i64, hi: i64| v.clamp(0, hi - 1);
    let px = |ix: i64, iy: i64| {
        let p = img.get_pixel(clamp(ix, w) as u32, clamp(iy, h) as u32);
        [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])]
    };
    let c00 = px(x0, y0);
    let c10 = px(x0 + 1, y0);
    let c01 = px(x0, y0 + 1);
    let c11 = px(x0 + 1, y0 + 1);
    let mut out = [0u8; 3];
    for ch in 0..3 {
        let top = c00[ch] * (1.0 - fx) + c10[ch] * fx;
        let bot = c01[ch] * (1.0 - fx) + c11[ch] * fx;
        out[ch] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
    }
    out
}

/// Rectify `src_bytes` (JPEG/PNG) using the four board corners (source
/// pixels, y-down, in TL,TR,BR,BL output order) and the board's real
/// `quad_w_mm × quad_h_mm`. Produces a JPEG at a fixed px/mm (capped so
/// the long side stays ≤ `MAX_LONG_SIDE_PX`).
pub fn rectify_image(
    src_bytes: &[u8],
    corners: &[(f64, f64); 4],
    quad_w_mm: f64,
    quad_h_mm: f64,
) -> Result<Rectified, String> {
    if !(quad_w_mm.is_finite() && quad_h_mm.is_finite()) || quad_w_mm <= 0.0 || quad_h_mm <= 0.0 {
        return Err("rectify: quad width/height must be positive, finite mm".into());
    }
    let src_corners: [[f64; 2]; 4] = [
        [corners[0].0, corners[0].1],
        [corners[1].0, corners[1].1],
        [corners[2].0, corners[2].1],
        [corners[3].0, corners[3].1],
    ];
    validate_quad(&src_corners)?;

    // Fixed resolution, scaled down only if the long side would exceed the
    // cap. Both dims share one px/mm so the image stays isotropic.
    let long_mm = quad_w_mm.max(quad_h_mm);
    let px_per_mm = if long_mm * DEFAULT_PX_PER_MM > MAX_LONG_SIDE_PX {
        MAX_LONG_SIDE_PX / long_mm
    } else {
        DEFAULT_PX_PER_MM
    };
    let out_w = (quad_w_mm * px_per_mm).round().max(1.0) as u32;
    let out_h = (quad_h_mm * px_per_mm).round().max(1.0) as u32;

    // Output-rectangle corners in TL,TR,BR,BL order matching the caller's
    // source corners.
    let dst_corners: [[f64; 2]; 4] = [
        [0.0, 0.0],
        [f64::from(out_w), 0.0],
        [f64::from(out_w), f64::from(out_h)],
        [0.0, f64::from(out_h)],
    ];
    let src_to_dst = homography_4pt(&src_corners, &dst_corners)?;
    let dst_to_src = invert3x3(&src_to_dst)
        .ok_or_else(|| "rectify: homography is singular / non-invertible".to_string())?;

    let dynimg = image::load_from_memory(src_bytes)
        .map_err(|e| format!("rectify: decode source image: {e}"))?;
    let src = dynimg.to_rgb8();

    // Inverse-warp: walk every OUTPUT pixel, map it back to the source and
    // bilinearly sample.
    let mut out = RgbImage::new(out_w, out_h);
    for v in 0..out_h {
        for u in 0..out_w {
            // Sample at the pixel centre to avoid a half-pixel bias.
            let Some((sx, sy)) =
                apply_homography(&dst_to_src, f64::from(u) + 0.5, f64::from(v) + 0.5)
            else {
                continue;
            };
            out.put_pixel(u, v, image::Rgb(sample_bilinear(&src, sx, sy)));
        }
    }

    let mut jpeg = Vec::new();
    out.write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)
        .map_err(|e| format!("rectify: encode JPEG: {e}"))?;

    Ok(Rectified {
        jpeg,
        width: out_w,
        height: out_h,
        px_per_mm,
        src_to_dst,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "{a} vs {b} (tol {tol})");
    }

    #[test]
    fn homography_maps_corners_exactly() {
        // A skewed (perspective) source quad → a 100×80 rectangle.
        let src = [[12.0, 20.0], [180.0, 5.0], [195.0, 160.0], [30.0, 175.0]];
        let dst = [[0.0, 0.0], [100.0, 0.0], [100.0, 80.0], [0.0, 80.0]];
        let m = homography_4pt(&src, &dst).expect("homography");
        for i in 0..4 {
            let (u, v) = apply_homography(&m, src[i][0], src[i][1]).expect("finite");
            approx(u, dst[i][0], 1e-6);
            approx(v, dst[i][1], 1e-6);
        }
    }

    #[test]
    fn inverse_round_trips_a_point() {
        let src = [[12.0, 20.0], [180.0, 5.0], [195.0, 160.0], [30.0, 175.0]];
        let dst = [[0.0, 0.0], [100.0, 0.0], [100.0, 80.0], [0.0, 80.0]];
        let m = homography_4pt(&src, &dst).expect("homography");
        let inv = invert3x3(&m).expect("invertible");
        // A source interior point → dst → back to source.
        let (u, v) = apply_homography(&m, 100.0, 90.0).expect("finite");
        let (x, y) = apply_homography(&inv, u, v).expect("finite");
        approx(x, 100.0, 1e-6);
        approx(y, 90.0, 1e-6);
    }

    /// Build a synthetic image: left half red, right half blue, top-left
    /// quadrant marked green. Warping the full-image quad (axis aligned)
    /// must preserve that layout so we can assert marker regions land where
    /// expected under the fixed px/mm mapping.
    fn synthetic() -> Vec<u8> {
        let mut img = RgbImage::new(200, 160);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = if x < 40 && y < 40 {
                image::Rgb([0, 255, 0]) // green marker, top-left
            } else if x < 100 {
                image::Rgb([255, 0, 0]) // red left
            } else {
                image::Rgb([0, 0, 255]) // blue right
            };
        }
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
            .expect("encode png");
        png
    }

    #[test]
    fn warp_preserves_layout_and_scale() {
        let png = synthetic();
        // Board is 10×8 mm and fills the whole 200×160 image; the corner
        // quad is the full image in TL,TR,BR,BL order.
        let corners = [(0.0, 0.0), (200.0, 0.0), (200.0, 160.0), (0.0, 160.0)];
        let r = rectify_image(&png, &corners, 10.0, 8.0).expect("rectify");
        // 40 px/mm → 400×320 output, isotropic.
        assert_eq!(r.width, 400);
        assert_eq!(r.height, 320);
        approx(r.px_per_mm, 40.0, 1e-9);
        // src_to_dst maps source corners onto the output rectangle.
        let (u, v) = apply_homography(&r.src_to_dst, 200.0, 160.0).expect("finite");
        approx(u, 400.0, 1e-4);
        approx(v, 320.0, 1e-4);

        // Decode the warped JPEG and check region colours (JPEG is lossy,
        // so test dominant channel not exact bytes).
        let out = image::load_from_memory(&r.jpeg)
            .expect("decode out")
            .to_rgb8();
        let is_red = |p: &image::Rgb<u8>| p[0] > 150 && p[2] < 100;
        let is_blue = |p: &image::Rgb<u8>| p[2] > 150 && p[0] < 100;
        let is_green = |p: &image::Rgb<u8>| p[1] > 150 && p[0] < 100 && p[2] < 100;
        // Left-centre (well inside the red half) is red.
        assert!(is_red(out.get_pixel(120, 160)), "left half should be red");
        // Right-centre is blue.
        assert!(
            is_blue(out.get_pixel(280, 160)),
            "right half should be blue"
        );
        // Top-left corner region (green marker: x<40,y<40 of 200×160 →
        // x<80,y<80 of 400×320) is green.
        assert!(
            is_green(out.get_pixel(30, 30)),
            "top-left marker should be green"
        );
    }

    #[test]
    fn long_side_is_capped() {
        let png = synthetic();
        let corners = [(0.0, 0.0), (200.0, 0.0), (200.0, 160.0), (0.0, 160.0)];
        // 200 mm long side × 40 px/mm = 8000 px → capped to 4000.
        let r = rectify_image(&png, &corners, 200.0, 100.0).expect("rectify");
        assert_eq!(r.width, 4000);
        approx(r.px_per_mm, 20.0, 1e-9);
        assert_eq!(r.height, 2000);
    }

    #[test]
    fn degenerate_quads_are_rejected() {
        let png = synthetic();
        // Three collinear points (top edge) + one off — near-collinear
        // vertex.
        let collinear = [(0.0, 0.0), (100.0, 0.0), (200.0, 0.0), (100.0, 100.0)];
        assert!(rectify_image(&png, &collinear, 10.0, 8.0).is_err());
        // Self-intersecting "bowtie" order (TL, BR, TR, BL).
        let bowtie = [(0.0, 0.0), (200.0, 160.0), (200.0, 0.0), (0.0, 160.0)];
        assert!(rectify_image(&png, &bowtie, 10.0, 8.0).is_err());
    }
}
