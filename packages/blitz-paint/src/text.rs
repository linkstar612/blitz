use anyrender::PaintScene;
use blitz_dom::{BaseDocument, NodeId, node::TextBrush, util::ToColorColor};
use kurbo::{Affine, BezPath, Cap, Circle, Rect, Stroke};
use parley::{Affinity, Cursor, Layout, Line, PositionedLayoutItem, Selection};
use peniko::Fill;
use std::collections::HashMap;
use style::properties::generated::longhands::text_decoration_style::computed_value::T as TextDecorationStyle;
use style::values::computed::{
    Length, LengthPercentage, TextDecorationLength, TextDecorationLine, TextUnderlinePosition,
};
use style::values::generics::text::{GenericTextDecorationInset, GenericTextDecorationLength};

use crate::color::{Color, ToColorColor as _, is_invisible};
use crate::{FONT_EMBOLDEN_ENABLED, SELECTION_COLOR};
use blitz_traits::text_raster::{STEM_DARKEN_ENABLED, TextRaster};

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
    inline_root_id: NodeId,
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

            let metrics = glyph_run.run().font_metrics();
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

/// Per-font-face cache of the OS/2 `usWinAscent / unitsPerEm` ratio, keyed by the font
/// blob's unique id and face index. `None` is cached for fonts without a usable OS/2 or
/// head table so they aren't re-parsed either.
type WinAscentCache = HashMap<(u64, u32), Option<f32>>;

/// The font's OS/2 `usWinAscent`, in the same (device) pixels as `font_size`.
///
/// This is the ascent browsers use as the top of the "em box" when positioning overlines. It
/// is typically taller than the hhea ascent Parley exposes via [`parley::layout::run::RunMetrics`].
/// Returns `None` when the font has no OS/2 table.
fn win_ascent(cache: &mut WinAscentCache, font: &parley::FontData, font_size: f32) -> Option<f32> {
    let ratio = *cache
        .entry((font.data.id(), font.index))
        .or_insert_with(|| win_ascent_ratio(font));
    Some(ratio? * font_size)
}

/// The unitless `usWinAscent / unitsPerEm` ratio for a font face, or `None` when the font
/// has no usable head/OS/2 table.
fn win_ascent_ratio(font: &parley::FontData) -> Option<f32> {
    use skrifa::raw::{FontRef, TableProvider as _};
    let font_ref = FontRef::from_index(font.data.as_ref(), font.index).ok()?;
    let units_per_em = font_ref.head().ok()?.units_per_em();
    let win_ascent = font_ref.os2().ok()?.us_win_ascent();
    Some(win_ascent as f32 / units_per_em as f32)
}

/// Mirrors Blink's `SelectBestDashGap`: choose the gap length (as close as possible to
/// `gap_length`) that fits a whole number of `dash_length` dashes across `stroke_length`,
/// so dashes are evenly distributed and a dash lands at each end of the line.
fn select_best_dash_gap(stroke_length: f64, dash_length: f64, gap_length: f64) -> f64 {
    let available_length = stroke_length + gap_length;
    let min_num_dashes = (available_length / (dash_length + gap_length)).floor();
    let max_num_dashes = min_num_dashes + 1.0;
    let min_num_gaps = min_num_dashes - 1.0;
    let max_num_gaps = max_num_dashes - 1.0;
    if min_num_gaps < 1.0 {
        return gap_length;
    }
    let min_gap = (stroke_length - min_num_dashes * dash_length) / min_num_gaps;
    let max_gap = (stroke_length - max_num_dashes * dash_length) / max_num_gaps;
    if max_gap <= 0.0 || (min_gap - gap_length).abs() < (max_gap - gap_length).abs() {
        min_gap
    } else {
        max_gap
    }
}

/// A text decoration resolved from a single "decorating box" (an element that
/// sets `text-decoration-line`), ready to be drawn over a glyph run. Because the
/// `text-decoration-*` properties do not inherit, decorations are propagated from
/// ancestors rather than inherited, so a run may be covered by several of these.
///
/// Only the run-independent (style-derived) values are stored here so an instance
/// can be cached on the ancestor stack and reused across every run within the same
/// decorating box. The `text-decoration-inset` and `text-underline-offset` values
/// depend on the run's advance/font-size and are resolved lazily when drawing.
#[derive(Clone)]
struct ResolvedDecoration {
    line: TextDecorationLine,
    style: TextDecorationStyle,
    color: Color,
    thickness: TextDecorationLength,
    /// `text-underline-offset`; `None` for `auto`. Resolved per run.
    underline_offset: Option<LengthPercentage>,
    /// Whether `text-underline-position: under` is in effect.
    underline_under: bool,
    /// Whether `text-underline-position: from-font` is in effect.
    underline_from_font: bool,
    /// `text-decoration-inset`. Resolved per run.
    inset: GenericTextDecorationInset<LengthPercentage>,
}

/// An element on the current ancestor path (inline root -> run node), caching the
/// values resolved from its computed style so descending into the same node across
/// consecutive runs doesn't re-resolve them.
struct DecorationStackEntry {
    node_id: NodeId,
    /// This node's (inherited) text colour, used for the glyphs of runs whose
    /// innermost node is this one.
    text_color: Color,
    /// The decoration this node introduces as a decorating box, if any.
    decoration: Option<ResolvedDecoration>,
}

/// Resolve the cached style values for a single node into a [`DecorationStackEntry`].
fn resolve_decoration_entry(doc: &BaseDocument, node_id: NodeId) -> DecorationStackEntry {
    let Some(styles) = doc.get_node(node_id).and_then(|node| node.primary_styles()) else {
        return DecorationStackEntry {
            node_id,
            text_color: Color::BLACK,
            decoration: None,
        };
    };

    let itext = styles.get_inherited_text();
    let text = styles.get_text();
    let text_color = itext.color.as_color_color();

    let drawn_lines = TextDecorationLine::UNDERLINE
        | TextDecorationLine::OVERLINE
        | TextDecorationLine::LINE_THROUGH;
    let line = text.text_decoration_line;
    // Decorations propagate through the box tree, and a `display: contents` element
    // generates no box, so its decorations have no effect on descendants.
    let is_contents = styles.clone_display().is_contents();
    let decoration = (!is_contents && line.intersects(drawn_lines)).then(|| {
        // `text-decoration-color: currentColor` (the initial value) resolves against
        // the decorating box's own colour, not the descendant run's.
        let color = text
            .text_decoration_color
            .as_absolute()
            .map(ToColorColor::as_color_color)
            .unwrap_or(text_color);

        ResolvedDecoration {
            line,
            style: text.text_decoration_style,
            color,
            thickness: text.text_decoration_thickness.clone(),
            underline_offset: itext.text_underline_offset.non_auto(),
            underline_under: itext
                .text_underline_position
                .contains(TextUnderlinePosition::UNDER),
            underline_from_font: itext
                .text_underline_position
                .contains(TextUnderlinePosition::FROM_FONT),
            inset: text.text_decoration_inset.clone(),
        }
    });

    DecorationStackEntry {
        node_id,
        text_color,
        decoration,
    }
}

/// The run-dependent geometry used to position and size a decoration, captured from a
/// single glyph run. Firefox draws a decoration once for the whole decorating box using
/// the box's own font, so we prefer the values from a run whose innermost node *is* the
/// decorating box (its own text), falling back to the first run it covers.
#[derive(Clone)]
struct DecorationRunGeometry {
    /// The baseline the decoration is positioned from (the run's own, `vertical-align`
    /// shifted, baseline).
    baseline: f32,
    ascent: f32,
    descent: f32,
    underline_offset: f32,
    underline_size: f32,
    strikethrough_size: f32,
    font: parley::FontData,
    font_size: f32,
    css_font_size: f64,
}

/// A decoration accumulated across the runs of a single line for one decorating box.
///
/// The `text-decoration-*` properties describe the whole box, so we gather the horizontal
/// extent it covers on the line (`min_x`/`max_x`) and the font geometry to draw with, then
/// paint a single line spanning the box rather than one segment per run.
struct LineDecoration {
    node_id: NodeId,
    deco: ResolvedDecoration,
    min_x: f64,
    max_x: f64,
    /// Geometry from a run whose innermost node is the decorating box itself.
    own: Option<DecorationRunGeometry>,
    /// Geometry from the first run the box covers (fallback when it has no own text on
    /// this line, e.g. it only wraps differently-sized descendants).
    first: Option<DecorationRunGeometry>,
}

/// Reusable scratch storage for drawing text across all inline formatting contexts in a
/// document. The vectors are cleared between uses without releasing their allocations.
#[derive(Default)]
pub(crate) struct DrawTextContext {
    stack: Vec<DecorationStackEntry>,
    path_scratch: Vec<NodeId>,
    deco_boxes: Vec<LineDecoration>,
    win_ascent_ratios: WinAscentCache,
}

/// Resolve the CSS `text-decoration-thickness` to a device-pixel size.
///
/// - Percentages are resolved against the font size,
/// - `from-font` uses the metrics from Parley,
/// - `auto` uses a thickness of `font-size / 10` (minimum: 1px).
///
/// The result is floored to a whole number of device pixels (minimum 1), matching
/// browsers, which snap decoration thickness to whole pixels.
fn decoration_size(
    thickness: &TextDecorationLength,
    metric_size: f32,
    css_font_size: f64,
    scale: f64,
) -> f32 {
    (match thickness {
        GenericTextDecorationLength::LengthPercentage(lp) => {
            lp.resolve(Length::new(css_font_size as f32)).px() as f64 * scale
        }
        GenericTextDecorationLength::FromFont => metric_size as f64,
        GenericTextDecorationLength::Auto => (css_font_size / 10.0).max(1.0) * scale,
    })
    .floor()
    .max(1.0) as f32
}

/// Draws a single decoration line of the given `text-decoration-style` spanning
/// `[x0, x0 + width]`. `baseline` is the line's baseline and `offset` positions the line
/// relative to it (`y = baseline - offset + size / 2`, growing downward). Geometry is built
/// explicitly (rather than relying on backend dash support) so it renders consistently
/// across renderers.
///
/// `double_dir` points away from the text (`+1` = down, `-1` = up) and is used to place the
/// second line of a `double` decoration.
#[allow(clippy::too_many_arguments)]
fn draw_decoration_line(
    scene: &mut impl PaintScene,
    transform: Affine,
    scale: f64,
    x0: f64,
    width: f64,
    baseline: f32,
    offset: f32,
    size: f32,
    brush: &anyrender::Paint,
    double_dir: f64,
    deco_style: TextDecorationStyle,
    inset_start: f64,
    inset_end: f64,
) {
    // Inset the line from the box's start/end edges (assumes LTR: start is the left edge).
    // Negative insets extend the line past the text.
    let x = x0 + inset_start;
    let w = width - inset_start - inset_end;
    if w <= 0.0 {
        return;
    }
    let y = (baseline - offset + size / 2.0) as f64;
    let size = size as f64;
    let butt_stroke = Stroke::new(size).with_caps(Cap::Butt);

    match deco_style {
        TextDecorationStyle::MozNone => {
            // no line. Equivalent to `text-decoration-line: none`
        }
        // `solid`
        TextDecorationStyle::Solid => {
            let line = kurbo::Line::new((x, y), (x + w, y));
            scene.stroke(&butt_stroke, transform, brush, None, &line);
        }
        // Two lines, each `size` thick, separated by a 1px (CSS) gap. This
        // matches Chrome, where the second line is offset by `thickness + 1px`
        // and sits further from the text (below for underline, above for
        // overline).
        TextDecorationStyle::Double => {
            let one_css_px = scale;
            for cy in [y, y + double_dir * (size + one_css_px)] {
                let line = kurbo::Line::new((x, cy), (x + w, cy));
                scene.stroke(&butt_stroke, transform, brush, None, &line);
            }
        }
        // Round dots (diameter = line thickness) spaced one dot apart.
        TextDecorationStyle::Dotted => {
            let radius = size / 2.0;
            let step = 2.0 * size;
            let mut cx = x + radius;
            while cx <= x + w - radius {
                let dot = Circle::new((cx, y), radius);
                scene.fill(Fill::NonZero, transform, brush, None, &dot);
                cx += step;
            }
        }
        // Dashes as filled rectangles. Dash/gap lengths are relative to the
        // thickness (matching Blink): thinner lines use proportionally longer
        // dashes and gaps so they don't read as dotted or solid.
        TextDecorationStyle::Dashed => {
            let (dash_ratio, gap_ratio) = if size >= 3.0 { (2.0, 1.0) } else { (3.0, 2.0) };
            let dash = size * dash_ratio;
            let nominal_gap = size * gap_ratio;
            // Nudge the gap so a whole number of dashes fits the run evenly.
            let gap = if w > 2.0 * dash {
                select_best_dash_gap(w, dash, nominal_gap)
            } else {
                nominal_gap
            };
            let mut dx = x;
            while dx < x + w {
                let end = (dx + dash).min(x + w);
                let rect = Rect::new(dx, y - size / 2.0, end, y + size / 2.0);
                scene.fill(Fill::NonZero, transform, brush, None, &rect);
                dx += dash + gap;
            }
        }
        // A squiggle built from alternating quadratic Béziers.
        TextDecorationStyle::Wavy => {
            let amplitude = size;
            let half_wave = (2.0 * size).max(1.0);
            let mut path = BezPath::new();
            path.move_to((x, y));
            let mut px = x;
            let mut up = true;
            while px < x + w {
                let nx = (px + half_wave).min(x + w);
                // A quad Bézier reaches half the control-point offset at its
                // midpoint, so use `2 * amplitude` to hit the target peak.
                let ctrl_y = if up {
                    y - 2.0 * amplitude
                } else {
                    y + 2.0 * amplitude
                };
                path.quad_to(((px + nx) / 2.0, ctrl_y), (nx, y));
                px = nx;
                up = !up;
            }
            let stroke = Stroke::new(size).with_caps(Cap::Round);
            scene.stroke(&stroke, transform, brush, None, &path);
        }
    }
}

/// Paint the decorations accumulated for one line, one line per decorating box.
fn flush_line_decorations(
    scene: &mut impl PaintScene,
    transform: Affine,
    scale: f64,
    deco_boxes: &[LineDecoration],
    win_ascent_ratios: &mut WinAscentCache,
    inline_root_id: NodeId,
    line_baseline: f32,
) {
    // Draw innermost boxes first so ancestors' decorations paint on top, matching the
    // per-run drawing order this replaced (`stack.iter().rev()`).
    for acc in deco_boxes.iter().rev() {
        let deco = &acc.deco;
        // Prefer the decorating box's own font; fall back to the first run it covers.
        let geom = match (&acc.own, &acc.first) {
            (Some(own), _) => own.clone(),
            (None, Some(first)) => {
                let mut geom = first.clone();
                // A descendant run's baseline may be shifted by `vertical-align`, but the
                // decoration is positioned from the decorating box's own baseline. For the
                // inline root that is the line's baseline.
                if acc.node_id == inline_root_id {
                    geom.baseline = line_baseline;
                }
                geom
            }
            (None, None) => continue,
        };
        let geom = &geom;
        let width = acc.max_x - acc.min_x;
        if width <= 0.0 {
            continue;
        }
        let brush = anyrender::Paint::from(deco.color);

        // `text-decoration-inset` shortens (or, when negative, extends) the line from the
        // inline-start/end edges. Percentages resolve against the decoration line length
        // (the box's total advance on this line); `auto` is no inset.
        let (inset_start, inset_end) = match &deco.inset {
            GenericTextDecorationInset::LengthPercentage { start, end } => {
                // The extent is in device pixels; convert to CSS pixels so the resolved
                // result can be scaled back.
                let line_length = Length::new((width / scale) as f32);
                (
                    start.resolve(line_length).px() as f64 * scale,
                    end.resolve(line_length).px() as f64 * scale,
                )
            }
            GenericTextDecorationInset::Auto => (0.0, 0.0),
        };

        if deco.line.contains(TextDecorationLine::UNDERLINE) {
            let size = decoration_size(
                &deco.thickness,
                geom.underline_size,
                geom.css_font_size,
                scale,
            );

            // `text-underline-offset` moves the underline away from the text. `auto` keeps
            // the font's suggested position; otherwise it resolves against the font size
            // (percentages relative to 1em).
            let extra_underline_offset = deco
                .underline_offset
                .as_ref()
                .map(|lp| lp.resolve(Length::new(geom.css_font_size as f32)).px() as f64 * scale)
                .unwrap_or(0.0);

            // `text-underline-position: under` places the underline below the glyph box
            // (below descenders) rather than at the font's suggested position near the
            // alphabetic baseline. We anchor the top of the line at the descent so it clears
            // descending glyphs like "gqy".
            // An explicit offset under `auto` is taken from the alphabetic
            // baseline, as CSS and Chrome have it, not from the auto gap.
            let base_offset = if deco.underline_under {
                -geom.descent
            } else if deco.underline_offset.is_some() && !deco.underline_from_font {
                0.0
            } else {
                geom.underline_offset
            };
            let offset = base_offset - extra_underline_offset as f32;

            // A `double` underline extends downward, away from the text.
            draw_decoration_line(
                scene,
                transform,
                scale,
                acc.min_x,
                width,
                geom.baseline,
                offset,
                size,
                &brush,
                1.0,
                deco.style,
                inset_start,
                inset_end,
            );
        }
        if deco.line.contains(TextDecorationLine::OVERLINE) {
            // Fonts don't provide a dedicated overline metric, so reuse the underline
            // thickness. The line sits at the top of the "em box": its lower edge rests on
            // the ascent so it clears the glyphs, and it extends upward from there.
            //
            // Browsers use the OS/2 `usWinAscent` for this edge, which is taller than the
            // hhea-based ascent Parley reports; using the smaller value would draw the
            // overline too low. Fall back to Parley's ascent for fonts without a usable
            // OS/2 table.
            let size = decoration_size(
                &deco.thickness,
                geom.underline_size,
                geom.css_font_size,
                scale,
            );
            let ascent =
                win_ascent(win_ascent_ratios, &geom.font, geom.font_size).unwrap_or(geom.ascent);
            let offset = ascent + size;

            // A `double` overline extends upward, away from the text.
            draw_decoration_line(
                scene,
                transform,
                scale,
                acc.min_x,
                width,
                geom.baseline,
                offset,
                size,
                &brush,
                -1.0,
                deco.style,
                inset_start,
                inset_end,
            );
        }
        if deco.line.contains(TextDecorationLine::LINE_THROUGH) {
            let size = decoration_size(
                &deco.thickness,
                geom.strikethrough_size,
                geom.css_font_size,
                scale,
            );

            // Centre the line-through a third of the ascent above the baseline, matching
            // Chrome (which places it `2/3 * ascent` below the text-top). Parley's
            // `strikethrough_offset` (the font's `yStrikeoutPosition`) sits lower and would
            // draw the line too close to the baseline.
            let ascent =
                win_ascent(win_ascent_ratios, &geom.font, geom.font_size).unwrap_or(geom.ascent);
            let offset = ascent / 3.0 + size / 2.0;

            draw_decoration_line(
                scene,
                transform,
                scale,
                acc.min_x,
                width,
                geom.baseline,
                offset,
                size,
                &brush,
                1.0,
                deco.style,
                inset_start,
                inset_end,
            );
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
    inline_root_id: NodeId,
    context: &mut DrawTextContext,
) {
    let DrawTextContext {
        stack,
        path_scratch,
        deco_boxes,
        win_ascent_ratios,
    } = context;
    stack.clear();
    path_scratch.clear();
    deco_boxes.clear();
    let raster = dom_text_raster();
    // Device px to layout units, for moving decoration extents with the glyphs.
    let layout_per_device = transform.as_coeffs()[0];

    // Persistent stack mirroring the ancestor path (inline root -> current run's
    // node) as we walk the runs. The `text-decoration-*` properties are *not*
    // inherited; instead a decoration set on an ancestor is propagated to the
    // descendant text it wraps. Rather than re-resolving styles for the whole
    // ancestor chain on every run, we cache each node's resolved values here and,
    // for each run, only resolve styles for the nodes newly descended into (popping
    // as we ascend). `path_scratch` is a reusable buffer for the run's node path.
    for line in lines {
        // Decorations accumulated for this line, keyed by decorating box, so each box is
        // painted once (spanning all its runs) using its own font — matching Firefox, which
        // draws one decoration per box rather than one stepped segment per differently-sized
        // run. Clearing preserves the allocation for the next line and inline context.
        deco_boxes.clear();

        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                // polyvox R-OIP.3: the run's origin on a whole device column.
                let dx = run_snap_dx(
                    &raster,
                    transform,
                    glyph_run.offset() as f64,
                    glyph_run.baseline() as f64,
                );
                let glyph_transform = transform.then_translate((dx, 0.0).into());
                let run = glyph_run.run();
                let font = run.font();
                let font_size = run.font_size();
                let metrics = run.font_metrics();
                let style = glyph_run.style();
                let synthesis = run.synthesis();
                let glyph_xform = synthesis
                    .skew()
                    .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));

                let css_font_size = font_size as f64 / scale;

                // Reconcile the stack with this run's ancestor path. Build the path
                // (inline root -> run node), keep the shared prefix already on the stack,
                // pop the rest, and resolve styles only for the newly-descended nodes.
                path_scratch.clear();
                let mut walk_id = Some(style.brush.id);
                while let Some(node_id) = walk_id {
                    path_scratch.push(node_id);
                    if node_id == inline_root_id {
                        break;
                    }
                    walk_id = doc.get_node(node_id).and_then(|node| node.parent);
                }
                path_scratch.reverse();

                let shared = stack
                    .iter()
                    .zip(path_scratch.iter())
                    .take_while(|(entry, node_id)| entry.node_id == **node_id)
                    .count();
                stack.truncate(shared);
                for &node_id in &path_scratch[shared..] {
                    stack.push(resolve_decoration_entry(doc, node_id));
                }

                // The glyph colour comes from the run's own node (the stack top): `color`
                // inherits, so the innermost inline element already carries the right value.
                let text_color = stack.last().map(|e| e.text_color).unwrap_or(Color::BLACK);

                let embolden = if FONT_EMBOLDEN_ENABLED {
                    let fs = font_size as f64 / scale;
                    kurbo::Vec2::new((0.015125 * fs).min(0.3), (0.0121 * fs).min(0.3))
                } else {
                    let (x, y) = raster.stem_darken();
                    kurbo::Vec2::new(x, y)
                };

                let normalized_coords: &[i16] = bytemuck::cast_slice(run.normalized_coords());
                scene.draw_glyphs(
                    font,
                    font_size,
                    raster.hint,
                    normalized_coords,
                    embolden,
                    Fill::NonZero,
                    &anyrender::Paint::from(text_color),
                    1.0, // alpha
                    glyph_transform,
                    glyph_xform,
                    glyph_run.positioned_glyphs().map(|glyph| anyrender::Glyph {
                        id: glyph.id as _,
                        x: glyph.x,
                        y: glyph.y,
                    }),
                );

                // Accumulate this run's contribution to each decorating box on its ancestor
                // path. The decoration is drawn once per box after the whole line has been
                // walked (see `flush_line_decorations`), so mixed font sizes within a box
                // produce a single straight line rather than one stepped segment per run.
                let geometry = DecorationRunGeometry {
                    baseline: glyph_run.baseline(),
                    ascent: metrics.ascent,
                    descent: metrics.descent,
                    underline_offset: metrics.underline_offset,
                    underline_size: metrics.underline_size,
                    strikethrough_size: metrics.strikethrough_size,
                    font: font.clone(),
                    font_size,
                    css_font_size,
                };
                let run_node_id = style.brush.id;
                // The decoration follows the snapped glyphs.
                let snap = if dx == 0.0 { 0.0 } else { dx / layout_per_device };
                let run_x0 = glyph_run.offset() as f64 + snap;
                let run_x1 = run_x0 + glyph_run.advance() as f64;

                for entry in stack.iter() {
                    if entry.decoration.is_none() {
                        continue;
                    }
                    let idx = match deco_boxes.iter().position(|d| d.node_id == entry.node_id) {
                        Some(idx) => idx,
                        None => {
                            deco_boxes.push(LineDecoration {
                                node_id: entry.node_id,
                                deco: entry.decoration.clone().unwrap(),
                                min_x: f64::INFINITY,
                                max_x: f64::NEG_INFINITY,
                                own: None,
                                first: None,
                            });
                            deco_boxes.len() - 1
                        }
                    };
                    let acc = &mut deco_boxes[idx];
                    acc.min_x = acc.min_x.min(run_x0);
                    acc.max_x = acc.max_x.max(run_x1);
                    if acc.first.is_none() {
                        acc.first = Some(geometry.clone());
                    }
                    // A run whose innermost node is the box itself is the box's own text, so
                    // its font is the one Firefox positions and sizes the decoration with.
                    if entry.node_id == run_node_id {
                        acc.own = Some(geometry.clone());
                    }
                }
            }
        }

        flush_line_decorations(
            scene,
            transform,
            scale,
            deco_boxes,
            win_ascent_ratios,
            inline_root_id,
            line.metrics().baseline,
        );
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
