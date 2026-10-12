//! One definition of how text is rasterized (polyvox R-OIP.3).
//!
//! Every text path in an embedder reads this module instead of carrying its own
//! copy: `blitz-paint` for DOM text, and any renderer the embedder draws glyphs
//! with itself (a vello transcript panel, an overlay cell). Before it existed the
//! stem darkening amount, its vertical ratio and the environment override were
//! written twice, once here in the fork and once in the embedder, and only a
//! comment kept them equal.
//!
//! Three settings, all decided per platform at first use:
//!
//! * `hint`: vertical hinting. glifo snaps each glyph's baseline to a whole
//!   device row and hints the outline at the draw size.
//! * `snap_run_origin`: place each horizontal glyph run's origin on a whole
//!   device column. Without it a run starts at whatever fraction layout left
//!   it at (a centered label, a 0.5 px border, a fractional flex share), and
//!   every vertical stem in it is split across two columns. On only where
//!   hinting is on, because snapping a run whose outline is not hinted to the
//!   pixel grid buys nothing.
//! * stem darkening: a synthetic outline expansion, device pixels per side,
//!   standing in for the gamma and stem-darkening stage DirectWrite has and
//!   `vello_hybrid` does not. A mitigation for a renderer difference, not a
//!   root-cause fix. Windows only, where the reference is DirectWrite.
//!
//! The darkening amount was picked by measurement (the R-OCZ.9 sweep, 0.15 to
//! 0.35): 0.20 is the largest amount whose mean stem-core ink stays inside 0.05
//! of the WebView2 reference. [`STEM_DARKEN_ENV`] re-opens the sweep without a
//! rebuild.

use std::sync::OnceLock;

/// Environment override for the stem darkening amount, device pixels per side,
/// `0.0..=1.0`. Anything else is ignored and the default stands.
pub const STEM_DARKEN_ENV: &str = "BLITZ_TEXT_STEM_DARKEN";

/// Default horizontal expansion per side, device pixels.
pub const STEM_DARKEN_DEFAULT_PX: f64 = 0.20;

/// The vertical share of the horizontal amount, the ratio the macOS embolden
/// path uses (0.0121 / 0.015125).
pub const STEM_DARKEN_Y_RATIO: f64 = 0.8;

/// Whether stem darkening applies on this platform.
pub const STEM_DARKEN_ENABLED: bool = cfg!(target_os = "windows");

/// Where a snapped run origin lands inside its device column: just past the
/// column's left edge, not on it. glifo floors the glyph's device x to pick the
/// column and quantizes the remainder into subpixel buckets, clamping anything
/// at or above 0.875 into the last bucket. A target of exactly `n` can come out
/// of the run transform as `n - 1e-13`, which floors to `n - 1` and renders a
/// quarter pixel left of where it should. 2^-10 is exactly representable, far
/// above that error, and far below the first bucket boundary (0.125).
pub const SNAP_BIAS_PX: f64 = 1.0 / 1024.0;

/// The text raster settings one platform uses. Every renderer reads the same
/// value from [`TextRaster::get`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextRaster {
    /// Vertical hinting on.
    pub hint: bool,
    /// Horizontal run origins snap to the device grid.
    pub snap_run_origin: bool,
    /// Stem darkening, device pixels per side, horizontal.
    pub stem_darken_x: f64,
    /// Stem darkening, device pixels per side, vertical.
    pub stem_darken_y: f64,
}

impl TextRaster {
    /// The settings for this platform, with [`STEM_DARKEN_ENV`] applied. Read
    /// once per process: every frame of every renderer sees the same value.
    pub fn get() -> TextRaster {
        static RASTER: OnceLock<TextRaster> = OnceLock::new();
        *RASTER.get_or_init(|| {
            TextRaster::resolve(STEM_DARKEN_ENABLED, std::env::var(STEM_DARKEN_ENV).ok().as_deref())
        })
    }

    /// The pure derivation behind [`TextRaster::get`], for tests and for an
    /// embedder that needs the value for another platform.
    pub fn resolve(stem_darken_enabled: bool, env_amount: Option<&str>) -> TextRaster {
        let x = if stem_darken_enabled {
            env_amount
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                .unwrap_or(STEM_DARKEN_DEFAULT_PX)
        } else {
            0.0
        };
        TextRaster {
            hint: true,
            snap_run_origin: true,
            stem_darken_x: x,
            stem_darken_y: x * STEM_DARKEN_Y_RATIO,
        }
    }

    /// The stem darkening as an `(x, y)` pair, device pixels per side.
    pub fn stem_darken(&self) -> (f64, f64) {
        (self.stem_darken_x, self.stem_darken_y)
    }

    /// The horizontal shift, device pixels, that puts a run whose origin lands
    /// at `device_x` on the device grid, given the run transform's linear part
    /// `[a, b, c, d]` (kurbo / peniko coefficient order). Zero when snapping
    /// is off, when hinting is off, or when the transform is not an
    /// axis-aligned, upright scale: a rotated or vertically skewed run has no
    /// single horizontal column to snap to, and glifo does not hint it either.
    ///
    /// The result is in device space: apply it after the run transform
    /// (`transform.then_translate((dx, 0.0))`), never before it.
    pub fn run_origin_dx(&self, linear: [f64; 4], device_x: f64) -> f64 {
        let [a, b, c, d] = linear;
        if !(self.snap_run_origin && self.hint) {
            return 0.0;
        }
        if b != 0.0 || c != 0.0 || !(a > 0.0) || !(d > 0.0) || !device_x.is_finite() {
            return 0.0;
        }
        (device_x.round() + SNAP_BIAS_PX) - device_x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_default_is_hinted_snapped_and_darkened() {
        let r = TextRaster::resolve(true, None);
        assert!(r.hint && r.snap_run_origin);
        assert_eq!(r.stem_darken(), (0.20, 0.20 * 0.8));
    }

    #[test]
    fn env_override_applies_inside_range_only() {
        assert_eq!(TextRaster::resolve(true, Some(" 0.3 ")).stem_darken_x, 0.3);
        assert_eq!(TextRaster::resolve(true, Some("1.5")).stem_darken_x, STEM_DARKEN_DEFAULT_PX);
        assert_eq!(TextRaster::resolve(true, Some("-0.1")).stem_darken_x, STEM_DARKEN_DEFAULT_PX);
        assert_eq!(TextRaster::resolve(true, Some("NaN")).stem_darken_x, STEM_DARKEN_DEFAULT_PX);
        assert_eq!(TextRaster::resolve(true, Some("bold")).stem_darken_x, STEM_DARKEN_DEFAULT_PX);
        assert_eq!(TextRaster::resolve(false, Some("0.3")).stem_darken(), (0.0, 0.0));
    }

    /// Mirrors glifo 0.2's placement: the glyph's device x is the run
    /// transform's translation plus the scaled glyph x, the column is its floor,
    /// and the subpixel bucket is the rounded quarter of the remainder clamped
    /// to 3 (`atlas/key.rs` `quantize_subpixel`, `renderer.rs`
    /// `render_outline_glyph_from_atlas`).
    fn glifo_column_and_bucket(e: f64, a: f64, glyph_x: f32) -> (f64, u8) {
        let tx = e + a * glyph_x as f64;
        let frac = tx.fract();
        let frac = if frac < 0.0 { frac + 1.0 } else { frac };
        (tx.floor(), ((frac as f32 * 4.0).round() as u8).min(3))
    }

    #[test]
    fn snapped_origin_lands_in_bucket_zero_of_the_rounded_column() {
        let r = TextRaster::resolve(true, None);
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0, 2.25] {
            for i in 0..2000 {
                let e = 13.0 + i as f64 * 0.37291;
                let glyph_x = (i as f32) * 0.613_7 + 0.031;
                let device_x = e + scale * glyph_x as f64;
                let dx = r.run_origin_dx([scale, 0.0, 0.0, scale], device_x);
                assert!(dx.abs() <= 0.5 + SNAP_BIAS_PX, "dx {dx}");
                let (col, bucket) = glifo_column_and_bucket(e + dx, scale, glyph_x);
                assert_eq!(col, device_x.round(), "scale {scale} i {i}");
                assert_eq!(bucket, 0, "scale {scale} i {i}");
            }
        }
    }

    #[test]
    fn rotated_skewed_or_unhinted_runs_are_left_alone() {
        let r = TextRaster::resolve(true, None);
        assert_eq!(r.run_origin_dx([0.0, 1.0, -1.0, 0.0], 10.3), 0.0);
        assert_eq!(r.run_origin_dx([1.0, 0.0, 0.2, 1.0], 10.3), 0.0);
        assert_eq!(r.run_origin_dx([1.0, 0.1, 0.0, 1.0], 10.3), 0.0);
        assert_eq!(r.run_origin_dx([-1.0, 0.0, 0.0, 1.0], 10.3), 0.0);
        let off = TextRaster { hint: false, ..r };
        assert_eq!(off.run_origin_dx([1.0, 0.0, 0.0, 1.0], 10.3), 0.0);
        let off = TextRaster { snap_run_origin: false, ..r };
        assert_eq!(off.run_origin_dx([1.0, 0.0, 0.0, 1.0], 10.3), 0.0);
    }
}
