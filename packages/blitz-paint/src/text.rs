use anyrender::PaintScene;
use blitz_dom::{BaseDocument, node::TextBrush, util::ToColorColor};
use kurbo::{Affine, Rect, Stroke};
use parley::{Affinity, Cursor, Layout, Line, PositionedLayoutItem, Selection};
use peniko::Fill;
use style::values::computed::TextDecorationLine;

use crate::color::{ToColorColor as _, is_invisible};
use crate::{FONT_EMBOLDEN_ENABLED, SELECTION_COLOR};

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

            scene.fill(Fill::NonZero, transform, bg_color, None, &rect);
        }
    }
}

/// Synthetic stem darkening for the platforms whose renderer has no text gamma
/// stage.
///
/// DirectWrite gamma-corrects and stem-darkens glyph coverage before blending;
/// `vello_hybrid` blends area coverage directly. Measured on the same string at
/// the same nominal size, that costs a vertical stem half its columns: Chromium
/// put 137 of 175 stems at 2 device px where this stack left 147 of 182 at 1 px,
/// with the mean core ink within 0.01 of each other. The ink is not weaker, it
/// is spread over half as many columns, which reads as blur.
///
/// There is no coverage curve to fix here: `vello_hybrid` is an upstream crate.
/// Expanding the outline by a fraction of a pixel is the closest lever this
/// stack has, and it is a mitigation for a renderer design difference, not a
/// root-cause fix.
///
/// The amount is in device pixels because the hinted path caches its outline at
/// the draw size with a unit draw scale (`glifo::GlyphScaleProperties::new`), so
/// `kurbo::expand_path` offsets in pixels and a stem gains twice the amount. The
/// default was picked by measurement, not by eye; `BLITZ_TEXT_STEM_DARKEN`
/// re-opens the sweep without a rebuild of the tree.
const STEM_DARKEN_ENABLED: bool = cfg!(target_os = "windows");

/// Default expansion per side, device pixels. Picked from a seven-value sweep
/// on the R-OCZ.9 overlay line (0.15 to 0.35): 0.20 is the largest amount whose
/// mean stem-core ink stays inside the acceptance cap of 0.05 over the WebView2
/// reference, at 0.813 against 0.771. It lifts the 2 px stem share from 0.132
/// to 0.459 at zoom 1.0 and to 0.615 at zoom 1.25, past the reference's 0.561.
const STEM_DARKEN_DEFAULT_PX: f64 = 0.20;

/// The vertical share of the horizontal amount, matching the ratio the macOS
/// embolden path already uses (0.0121 / 0.015125).
const STEM_DARKEN_Y_RATIO: f64 = 0.8;

fn stem_darken_px() -> f64 {
    static AMOUNT: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *AMOUNT.get_or_init(|| {
        std::env::var("BLITZ_TEXT_STEM_DARKEN")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
            .unwrap_or(STEM_DARKEN_DEFAULT_PX)
    })
}

pub(crate) fn stroke_text<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    scale: f64,
) {
    for line in lines {
        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
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
                } else if STEM_DARKEN_ENABLED {
                    let x = stem_darken_px();
                    kurbo::Vec2::new(x, x * STEM_DARKEN_Y_RATIO)
                } else {
                    kurbo::Vec2::default()
                };

                scene.draw_glyphs(
                    font,
                    font_size,
                    // Hinting and embolden are independent in glifo, so keeping
                    // both on is safe; only the macOS embolden path trades one
                    // for the other.
                    !FONT_EMBOLDEN_ENABLED || STEM_DARKEN_ENABLED, // hint
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

    selection.geometry_with(layout, |rect, _line_idx| {
        let rect = kurbo::Rect::new(rect.x0, rect.y0, rect.x1, rect.y1);
        scene.fill(Fill::NonZero, transform, SELECTION_COLOR, None, &rect);
    });
}
