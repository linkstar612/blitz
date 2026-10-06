use anyrender::PaintScene;
use blitz_dom::{BaseDocument, node::TextBrush, util::ToColorColor};
use kurbo::{Affine, Rect, Stroke};
use parley::{Affinity, Cursor, Layout, Line, PositionedLayoutItem, Selection};
use peniko::Fill;
use style::values::computed::TextDecorationLine;

use blitz_traits::text_raster::{STEM_DARKEN_ENABLED, TextRaster};

use crate::color::{ToColorColor as _, is_invisible};
use crate::{FONT_EMBOLDEN_ENABLED, SELECTION_COLOR};

/// The raster settings DOM text uses (polyvox R-OIP.3): the one shared
/// definition in `blitz_traits::text_raster`, which embedders read for their own
/// glyph renderers too, with one carve-out. The macOS embolden feature trades
/// hinting for a size-scaled embolden, and an unhinted run has no reason to be
/// snapped to the pixel grid, so that path turns both off. Same precedence the
/// hint flag always had here: `!FONT_EMBOLDEN_ENABLED || STEM_DARKEN_ENABLED`.
pub(crate) fn dom_text_raster() -> TextRaster {
    let shared = TextRaster::get();
    if FONT_EMBOLDEN_ENABLED && !STEM_DARKEN_ENABLED {
        TextRaster {
            hint: false,
            snap_run_origin: false,
            ..shared
        }
    } else {
        shared
    }
}

/// Device-space x shift that puts a glyph run's origin on the device grid.
///
/// polyvox R-OIP.3: parley leaves a run at whatever fraction layout produced
/// (a centered label, a fractional flex share, a 0.5 px border), and nothing
/// downstream rounds it: glifo snaps the baseline row when it hints but keeps
/// the run's x as it arrives, so every vertical stem in the run straddled two
/// device columns. One function computes the shift so the glyphs, their
/// decoration lines, the inline backgrounds behind them, the selection
/// highlight and the caret all move by the same amount.
pub(crate) fn run_snap_dx(raster: &TextRaster, transform: Affine, run_x: f64, baseline: f64) -> f64 {
    let c = transform.as_coeffs();
    let origin = transform * kurbo::Point::new(run_x, baseline);
    raster.run_origin_dx([c[0], c[1], c[2], c[3]], origin.x)
}

/// The run-origin shifts of one laid-out paragraph, in layout units, per line,
/// so geometry parley computes from the unsnapped layout (selection rects, the
/// caret) can be moved onto the snapped glyphs.
pub(crate) struct RunSnaps {
    lines: Vec<LineSnaps>,
}

struct LineSnaps {
    y0: f64,
    y1: f64,
    /// `(x0, x1, dx)`: a run's extent and its shift, layout units, in order.
    runs: Vec<(f64, f64, f64)>,
}

impl RunSnaps {
    pub(crate) fn new(layout: &Layout<TextBrush>, transform: Affine, raster: &TextRaster) -> Self {
        // Device px to layout units. `run_origin_dx` is zero unless the linear
        // part is an upright axis-aligned scale, so `a` is positive whenever
        // a shift is non-zero.
        let a = transform.as_coeffs()[0];
        let lines = layout
            .lines()
            .map(|line| {
                let m = line.metrics();
                let runs = line
                    .items()
                    .filter_map(|item| match item {
                        PositionedLayoutItem::GlyphRun(run) => {
                            let x0 = run.offset() as f64;
                            let dx = run_snap_dx(raster, transform, x0, run.baseline() as f64);
                            let dx = if dx == 0.0 { 0.0 } else { dx / a };
                            Some((x0, x0 + run.advance() as f64, dx))
                        }
                        _ => None,
                    })
                    .collect();
                LineSnaps {
                    y0: m.block_min_coord as f64,
                    y1: m.block_max_coord as f64,
                    runs,
                }
            })
            .collect();
        RunSnaps { lines }
    }

    /// The shift for an x on `line`, layout units. `right_edge` picks the run
    /// that ENDS at a boundary rather than the one that starts there, so a
    /// selection rect's two edges each follow the glyphs they touch. An x past
    /// either end of the line takes the nearest run's shift.
    pub(crate) fn dx_at(&self, line: usize, x: f64, right_edge: bool) -> f64 {
        let Some(l) = self.lines.get(line) else {
            return 0.0;
        };
        let (Some(first), Some(last)) = (l.runs.first(), l.runs.last()) else {
            return 0.0;
        };
        if x <= first.0 {
            return first.2;
        }
        if x >= last.1 {
            return last.2;
        }
        l.runs
            .iter()
            .find(|&&(x0, x1, _)| {
                if right_edge {
                    x > x0 && x <= x1
                } else {
                    x >= x0 && x < x1
                }
            })
            .map_or(0.0, |r| r.2)
    }

    /// The line a y falls on, or the last line for a y past the end.
    pub(crate) fn line_at(&self, y: f64) -> usize {
        self.lines
            .iter()
            .position(|l| y >= l.y0 && y < l.y1)
            .unwrap_or_else(|| self.lines.len().saturating_sub(1))
    }

    /// Move a rect on `line` onto the snapped glyphs: each edge by the shift of
    /// the run it touches.
    pub(crate) fn snap_rect(&self, line: usize, rect: kurbo::Rect) -> kurbo::Rect {
        kurbo::Rect::new(
            rect.x0 + self.dx_at(line, rect.x0, false),
            rect.y0,
            rect.x1 + self.dx_at(line, rect.x1, true),
            rect.y1,
        )
    }
}

/// Draw the backgrounds of inline elements (e.g. `<span style="background: ...">`).
///
/// Each glyph run carries the node id of the innermost inline element it belongs to
/// (via its brush). We look up that node's `background-color` and, if non-transparent,
/// fill a rectangle covering the run's advance and its font's ascent/descent so that the
/// background sits behind the text.
///
/// The inline root's own background is painted separately (as a normal block box), so
/// runs belonging to the root are skipped to avoid drawing it twice.
pub(crate) fn draw_inline_backgrounds<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    inline_root_id: usize,
) {
    let raster = dom_text_raster();
    for line in lines {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };

            let node_id = glyph_run.style().brush.id;
            if node_id == inline_root_id {
                continue;
            }

            let Some(styles) = doc.get_node(node_id).and_then(|node| node.primary_styles()) else {
                continue;
            };

            let current_color = styles.clone_color();
            let bg_color = styles
                .get_background()
                .background_color
                .resolve_to_absolute(&current_color)
                .as_srgb_color();
            if is_invisible(bg_color) {
                continue;
            }

            let metrics = glyph_run.run().metrics();
            let x = glyph_run.offset() as f64;
            let w = glyph_run.advance() as f64;
            let baseline = glyph_run.baseline() as f64;
            let y0 = baseline - metrics.ascent as f64;
            let y1 = baseline + metrics.descent as f64;
            let rect = Rect::new(x, y0, x + w, y1);
            // Behind the run's snapped glyphs, not its unsnapped layout box.
            let dx = run_snap_dx(&raster, transform, x, baseline);
            let transform = transform.then_translate((dx, 0.0).into());

            scene.fill(Fill::NonZero, transform, bg_color, None, &rect);
        }
    }
}

// Synthetic stem darkening (R-OCZ.9) and its `BLITZ_TEXT_STEM_DARKEN`
// override live in `blitz_traits::text_raster` since polyvox R-OIP.3, so the
// embedder's own glyph renderers read the same amount. Why it exists: DirectWrite
// gamma-corrects and stem-darkens coverage before blending and `vello_hybrid`
// blends area coverage directly, which left native stems at 1 px where Chromium
// put them at 2. Expanding the outline is the closest lever this stack has, a
// mitigation for a renderer difference, not a root-cause fix. The amount is in
// device pixels because the hinted path caches its outline at the draw size
// with a unit draw scale (`glifo::GlyphScaleProperties::new`).

pub(crate) fn stroke_text<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    scale: f64,
) {
    let raster = dom_text_raster();
    for line in lines {
        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                // Snap this run's origin to the device grid (R-OIP.3). The
                // decoration lines below draw with the same transform.
                let dx = run_snap_dx(
                    &raster,
                    transform,
                    glyph_run.offset() as f64,
                    glyph_run.baseline() as f64,
                );
                let transform = transform.then_translate((dx, 0.0).into());
                let run = glyph_run.run();
                let font = run.font();
                let font_size = run.font_size();
                let metrics = run.metrics();
                let style = glyph_run.style();
                let synthesis = run.synthesis();
                let glyph_xform = synthesis
                    .skew()
                    .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));

                // Styles
                let styles = doc
                    .get_node(style.brush.id)
                    .unwrap()
                    .primary_styles()
                    .unwrap();
                let itext_styles = styles.get_inherited_text();
                let text_styles = styles.get_text();
                let text_color = itext_styles.color.as_color_color();
                let text_decoration_color = text_styles
                    .text_decoration_color
                    .as_absolute()
                    .map(ToColorColor::as_color_color)
                    .unwrap_or(text_color);
                let text_decoration_brush = anyrender::Paint::from(text_decoration_color);
                let text_decoration_line = text_styles.text_decoration_line;
                let has_underline = text_decoration_line.contains(TextDecorationLine::UNDERLINE);
                let has_strikethrough =
                    text_decoration_line.contains(TextDecorationLine::LINE_THROUGH);

                let embolden = if FONT_EMBOLDEN_ENABLED {
                    let fs = font_size as f64 / scale;
                    kurbo::Vec2::new((0.015125 * fs).min(0.3), (0.0121 * fs).min(0.3))
                } else {
                    let (x, y) = raster.stem_darken();
                    kurbo::Vec2::new(x, y)
                };

                scene.draw_glyphs(
                    font,
                    font_size,
                    // Hinting and embolden are independent in glifo, so keeping
                    // both on is safe; only the macOS embolden path trades one
                    // for the other (`dom_text_raster`).
                    raster.hint,
                    run.normalized_coords(),
                    embolden,
                    Fill::NonZero,
                    &anyrender::Paint::from(text_color),
                    1.0, // alpha
                    transform,
                    glyph_xform,
                    glyph_run.positioned_glyphs().map(|glyph| anyrender::Glyph {
                        id: glyph.id as _,
                        x: glyph.x,
                        y: glyph.y,
                    }),
                );

                let mut draw_decoration_line =
                    |offset: f32, size: f32, brush: &anyrender::Paint| {
                        let x = glyph_run.offset() as f64;
                        let w = glyph_run.advance() as f64;
                        let y = (glyph_run.baseline() - offset + size / 2.0) as f64;
                        let line = kurbo::Line::new((x, y), (x + w, y));
                        scene.stroke(&Stroke::new(size as f64), transform, brush, None, &line)
                    };

                if has_underline {
                    let offset = metrics.underline_offset;
                    let size = metrics.underline_size;

                    // TODO: intercept line when crossing an descending character like "gqy"
                    draw_decoration_line(offset, size, &text_decoration_brush);
                }
                if has_strikethrough {
                    let offset = metrics.strikethrough_offset;
                    let size = metrics.strikethrough_size;

                    draw_decoration_line(offset, size, &text_decoration_brush);
                }
            }
        }
    }
}

/// Draw selection highlight rectangles for the given byte range in a layout.
/// Uses Parley's Selection type for accurate geometry calculation.
pub(crate) fn draw_text_selection(
    scene: &mut impl PaintScene,
    layout: &Layout<TextBrush>,
    transform: Affine,
    selection_start: usize,
    selection_end: usize,
) {
    let anchor = Cursor::from_byte_index(layout, selection_start, Affinity::Downstream);
    let focus = Cursor::from_byte_index(layout, selection_end, Affinity::Downstream);
    let selection = Selection::new(anchor, focus);
    // The highlight follows the snapped glyphs, edge by edge (R-OIP.3).
    let snaps = RunSnaps::new(layout, transform, &dom_text_raster());

    selection.geometry_with(layout, |rect, line_idx| {
        let rect = kurbo::Rect::new(rect.x0, rect.y0, rect.x1, rect.y1);
        let rect = snaps.snap_rect(line_idx, rect);
        scene.fill(Fill::NonZero, transform, SELECTION_COLOR, None, &rect);
    });
}

#[cfg(test)]
mod tests {
    //! polyvox R-OIP.3: the run-origin snap on a real parley layout. The glyph
    //! arithmetic below is glifo 0.2's (`DrawProps::positioned_transform`, then
    //! `render_outline_glyph_from_atlas` floors the device x and
    //! `quantize_subpixel` buckets the remainder), so "bucket 0 of the rounded
    //! column" is what the rasterizer will actually do with the run's first
    //! glyph, not an approximation of it.
    use super::*;
    use parley::fontique::Blob;
    use parley::{Alignment, AlignmentOptions, FontContext, LayoutContext, StyleProperty};
    use std::sync::Arc;

    const TEXT: &str = "Minimum time on screen 1.0s";

    /// A layout with two runs (the "1.0s" readout at another size), centered
    /// in a box so the line starts at a fraction. `None` when the machine has
    /// no Segoe UI or DejaVu Sans to shape with.
    fn layout() -> Option<Layout<TextBrush>> {
        let font = ["C:/Windows/Fonts/segoeui.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"]
            .iter()
            .find_map(|p| std::fs::read(p).ok())?;
        let mut fcx = FontContext::new();
        let families = fcx.collection.register_fonts(Blob::new(Arc::new(font)), None);
        let family = fcx.collection.family_name(families.first()?.0)?.to_owned();
        let mut lcx = LayoutContext::<TextBrush>::new();
        let mut b = lcx.ranged_builder(&mut fcx, TEXT, 1.0, true);
        b.push_default(StyleProperty::FontFamily(parley::style::FontFamily::Source(family.into())));
        b.push_default(StyleProperty::FontSize(13.0));
        b.push_default(StyleProperty::Brush(TextBrush { id: 1 }));
        b.push(StyleProperty::FontSize(11.0), 23..TEXT.len());
        b.push(StyleProperty::Brush(TextBrush { id: 2 }), 23..TEXT.len());
        let mut layout = b.build(TEXT);
        layout.break_all_lines(Some(301.3));
        layout.align(Alignment::Center, AlignmentOptions::default());
        Some(layout)
    }

    fn runs(layout: &Layout<TextBrush>) -> Vec<(f64, f64, f64, Vec<f32>)> {
        layout
            .lines()
            .flat_map(|line| {
                line.items()
                    .filter_map(|item| match item {
                        PositionedLayoutItem::GlyphRun(r) => Some((
                            r.offset() as f64,
                            (r.offset() + r.advance()) as f64,
                            r.baseline() as f64,
                            r.positioned_glyphs().map(|g| g.x).collect(),
                        )),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn glifo_column_and_bucket(t: Affine, glyph_x: f32) -> (f64, u8) {
        let c = t.as_coeffs();
        let tx = c[4] + c[0] * glyph_x as f64;
        let frac = tx.fract();
        let frac = if frac < 0.0 { frac + 1.0 } else { frac };
        (tx.floor(), ((frac as f32 * 4.0).round() as u8).min(3))
    }

    fn transforms() -> Vec<Affine> {
        vec![
            Affine::translate((40.37, 20.6)),
            Affine::translate((40.875, 20.6)),
            Affine::translate((7.0, 3.0)) * Affine::scale(1.25) * Affine::translate((31.3, 9.9)),
            Affine::translate((12.6, 0.0)) * Affine::scale(1.5),
        ]
    }

    #[test]
    fn every_run_origin_lands_on_a_whole_device_column() {
        let Some(layout) = layout() else {
            assert!(!cfg!(windows), "Segoe UI must be present on Windows");
            return;
        };
        let raster = TextRaster::resolve(true, None);
        let runs = runs(&layout);
        assert!(runs.len() >= 2, "the readout must be its own run: {}", runs.len());
        assert!(runs[0].0.fract() != 0.0, "the centered line must start at a fraction");
        for t in transforms() {
            for (x0, _, baseline, glyphs) in &runs {
                let dx = run_snap_dx(&raster, t, *x0, *baseline);
                let snapped = t.then_translate((dx, 0.0).into());
                let device = (t * kurbo::Point::new(*x0, *baseline)).x;
                // The first glyph sits on the run origin; it lands in bucket 0
                // of the column the origin rounds to.
                assert_eq!(glyphs[0] as f64, *x0);
                let (col, bucket) = glifo_column_and_bucket(snapped, glyphs[0]);
                assert_eq!((col, bucket), (device.round(), 0), "transform {t:?} run at {x0}");
                // The rest of the run moves rigidly: advances are untouched.
                for g in glyphs {
                    let before = (t * kurbo::Point::new(*g as f64, 0.0)).x;
                    let after = (snapped * kurbo::Point::new(*g as f64, 0.0)).x;
                    assert!((after - before - dx).abs() < 1e-9);
                }
            }
        }
    }

    #[test]
    fn rotated_and_unhinted_runs_are_not_moved() {
        let Some(layout) = layout() else { return };
        let runs = runs(&layout);
        let hinted = TextRaster::resolve(true, None);
        let unhinted = TextRaster { hint: false, ..hinted };
        for (x0, _, baseline, _) in &runs {
            let rot = Affine::translate((40.37, 20.6)) * Affine::rotate(0.3);
            assert_eq!(run_snap_dx(&hinted, rot, *x0, *baseline), 0.0);
            let vert = Affine::translate((40.37, 20.6)) * Affine::rotate(std::f64::consts::FRAC_PI_2);
            assert_eq!(run_snap_dx(&hinted, vert, *x0, *baseline), 0.0);
            let t = Affine::translate((40.37, 20.6));
            assert_eq!(run_snap_dx(&unhinted, t, *x0, *baseline), 0.0);
        }
    }

    #[test]
    fn selection_and_caret_follow_the_snapped_runs() {
        let Some(layout) = layout() else { return };
        let raster = TextRaster::resolve(true, None);
        let runs = runs(&layout);
        for t in transforms() {
            let a = t.as_coeffs()[0];
            let snaps = RunSnaps::new(&layout, t, &raster);
            let dx: Vec<f64> = runs
                .iter()
                .map(|(x0, _, b, _)| run_snap_dx(&raster, t, *x0, *b) / a)
                .collect();
            let (r0, r1) = (&runs[0], &runs[runs.len() - 1]);
            // A highlight over the whole line: its left edge moves with the
            // first run, its right edge with the last.
            let full = snaps.snap_rect(0, kurbo::Rect::new(r0.0, 0.0, r1.1, 10.0));
            assert!((full.x0 - (r0.0 + dx[0])).abs() < 1e-9);
            assert!((full.x1 - (r1.1 + dx[runs.len() - 1])).abs() < 1e-9);
            // At the boundary between two runs a caret (a left edge) follows
            // the run that starts there, and a highlight that ends there
            // follows the run that ends there.
            let boundary = runs[1].0;
            assert_eq!(snaps.dx_at(0, boundary, false), dx[1]);
            assert_eq!(snaps.dx_at(0, boundary, true), dx[0]);
            // The caret's line is found from its y.
            assert_eq!(snaps.line_at(layout.height() as f64 / 2.0), 0);
            // Past the end of the line: the nearest run.
            assert_eq!(snaps.dx_at(0, r1.1 + 50.0, false), dx[runs.len() - 1]);
            assert_eq!(snaps.dx_at(0, -5.0, true), dx[0]);
            assert_eq!(snaps.dx_at(7, 10.0, false), 0.0);
        }
    }
}
