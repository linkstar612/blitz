//! Paint a [`blitz_dom::BaseDocument`] by pushing [`anyrender`] drawing commands into
//! an impl [`anyrender::PaintScene`].

#![allow(clippy::collapsible_if)]

mod color;
mod debug_overlay;
mod filters;
mod gradient;
mod kurbo_css;
mod layers;
mod render;
mod sizing;
mod text;

use std::collections::HashMap;

use anyrender::{PaintScene, Scene};
use blitz_dom::{BaseDocument, NodeId, util::Color};
use render::BlitzDomPainter;

const FONT_EMBOLDEN_ENABLED: bool = cfg!(any(
    feature = "font-embolden",
    all(feature = "apple-font-embolden", target_os = "macos"),
    all(feature = "apple-font-embolden", target_os = "ios"),
));

/// The default color for text selection highlights
const SELECTION_COLOR: Color = Color::from_rgb8(180, 213, 255);

/// A custom widget's scene plus the sub-pixel remainder of its device origin
/// that the widget was told to fold into its own content (polyvox R-OIP.1).
/// `draw_custom_widget` places `scene` at a whole device pixel derived from
/// `fraction`, so a texture the widget returns is composited texel for texel.
pub(crate) struct CustomWidgetPaint {
    pub(crate) scene: Scene,
    pub(crate) fraction: (f64, f64),
}

/// The device-pixel x (or y) of a widget's content box: the paint
/// traversal's `initial` translate less the viewport scroll, plus the box's
/// document position times the window scale (polyvox R-OIP.1, R-OIP.15).
#[cfg(feature = "custom-widget")]
fn widget_device_origin(initial: f64, viewport_scroll: f64, css_origin: f64, scale: f64) -> f64 {
    initial - viewport_scroll * scale + css_origin * scale
}

/// The texture extent a widget paints into along one axis: the rounded box
/// size in device pixels, one column wider when the fold shifts the content
/// right, so the last device column the content reaches is not clipped
/// (polyvox R-OIP.15). A fold of 0 or less keeps the old extent.
#[cfg(feature = "custom-widget")]
fn widget_texture_extent(device_size: f64, fraction: f64) -> u32 {
    if fraction > 0.0 && fraction.is_finite() {
        (device_size + fraction).ceil() as u32
    } else {
        device_size as u32
    }
}

/// Pre-computed scenes for each CustomWidget, keyed by `(document id, node id)`
type CustomWidgetSceneMap = HashMap<(usize, NodeId), CustomWidgetPaint>;

/// Paint a [`blitz_dom::BaseDocument`] by pushing drawing commands into
/// an impl [`anyrender::PaintScene`].
///
/// This function assumes that the styles and layout in the [`BaseDocument`] are already
/// resolved. Please ensure that this is the case before trying to paint.
///
/// The implementation of [`PaintScene`] is responsible for handling the commands that are pushed into it.
/// Generally this will involve executing them to draw a rasterized image/texture. But in some cases it may choose to
/// transform them to a vector format (e.g. SVG/PDF) or serialize them in raw form for later use.
pub fn paint_scene(
    scene: &mut impl PaintScene,
    doc: &mut BaseDocument,
    scale: f64,
    width: u32,
    height: u32,
    x_offset: u32,
    y_offset: u32,
) {
    // Run `.paint()` on every custom widget in the document (and all subdocuments) ahead of time.
    // This helps us avoid borrow-checker issues as we recurse down the tree (`.paint()` require `&mut self`).
    //
    // TODO: Take widget and sub-document visibility into account
    #[allow(unused_mut)]
    let mut custom_widget_scenes: CustomWidgetSceneMap = HashMap::new();
    #[cfg(feature = "custom-widget")]
    build_custom_widget_scenes(
        &mut custom_widget_scenes,
        doc,
        scene,
        scale,
        (x_offset as f64, y_offset as f64),
    );

    let generator = BlitzDomPainter::new(
        doc,
        scale,
        width,
        height,
        x_offset as f64,
        y_offset as f64,
        &custom_widget_scenes,
    );
    generator.paint_scene(scene);

    // println!(
    //     "Rendered using {} clips (depth: {}) (wanted: {})",
    //     CLIPS_USED.load(atomic::Ordering::SeqCst),
    //     CLIP_DEPTH_USED.load(atomic::Ordering::SeqCst),
    //     CLIPS_WANTED.load(atomic::Ordering::SeqCst)
    // );
}

#[cfg(feature = "custom-widget")]
fn build_custom_widget_scenes(
    custom_widget_scenes: &mut CustomWidgetSceneMap,
    doc: &mut BaseDocument,
    render_ctx: &mut impl anyrender::RenderContext,
    scale: f64,
    initial: (f64, f64),
) {
    let doc_id = doc.id();

    // Process scenes for every custom widget in the document
    let custom_widget_node_ids = doc.custom_widget_node_ids();
    for node_id in custom_widget_node_ids.into_iter() {
        if let Some(scene) = process_custom_widget_node(doc, render_ctx, node_id, scale, initial) {
            custom_widget_scenes.insert((doc_id, node_id), scene);
        }
    }

    // Recurse into sub documents
    let sub_document_node_ids = doc.sub_document_node_ids();
    for node_id in sub_document_node_ids.into_iter() {
        if let Some(sub_doc) = doc.get_node_mut(node_id).and_then(|node| node.subdoc_mut()) {
            let mut inner = sub_doc.inner_mut();
            // The host element's own offset is not known here, so a widget in a
            // sub-document is told the remainder of its in-document origin only.
            // Its draw is still snapped to a whole pixel (see `CustomWidgetPaint`).
            build_custom_widget_scenes(custom_widget_scenes, &mut inner, render_ctx, scale, (0.0, 0.0));
        }
    }
}

#[cfg(feature = "custom-widget")]
fn process_custom_widget_node(
    doc: &mut BaseDocument,
    render_ctx: &mut impl anyrender::RenderContext,
    node_id: NodeId,
    scale: f64,
    initial: (f64, f64),
) -> Option<CustomWidgetPaint> {
    use blitz_dom::node::{CustomWidgetStatus, ProxyRenderContext, split_device_origin};

    // polyvox R-OIP.1: the device-pixel content-box origin the paint traversal
    // will place this widget at (`render.rs` `render_element`: the viewport
    // translate, then every layout ancestor's `location * scale` less its
    // scroll offset, then `create_css_rect`'s scaled border and padding),
    // predicted here because `paint` runs before that traversal.
    //
    // polyvox R-OIP.15: the origin is summed from the unrounded layouts, so
    // the remainder is the box's true sub-pixel offset from the whole pixel
    // the traversal places it at. From the rounded layouts it was always 0 at
    // DPR 1, and a widget drew at taffy's rounded line box (a ruby cell at
    // x 58.000 where the browser draws 57.656).
    let viewport_scroll = doc.viewport_scroll();
    let node = doc.get_node(node_id)?;
    let abs = node.absolute_position_unrounded();
    let l = node.unrounded_layout();
    let origin_x = widget_device_origin(
        initial.0,
        viewport_scroll.x,
        abs.x + f64::from(l.border.left) + f64::from(l.padding.left),
        scale,
    );
    let origin_y = widget_device_origin(
        initial.1,
        viewport_scroll.y,
        abs.y + f64::from(l.border.top) + f64::from(l.padding.top),
        scale,
    );
    let fraction = (
        split_device_origin(origin_x).1,
        split_device_origin(origin_y).1,
    );

    let node = doc.get_node_mut(node_id)?;
    let width = widget_texture_extent(f64::from(node.final_layout().size.width) * scale, fraction.0);
    let height = widget_texture_extent(f64::from(node.final_layout().size.height) * scale, fraction.1);

    if width == 0 || height == 0 {
        return None;
    }

    let style = (*node.stylo_element_data().primary_styles()?).clone();
    let element = node.data.downcast_element_mut()?;
    let widget_data = element.custom_widget_data_mut()?;

    let mut render_ctx = ProxyRenderContext {
        inner: render_ctx,
        resource_ids: &mut widget_data.active_resource_ids,
    };

    if widget_data.status == CustomWidgetStatus::Suspended {
        widget_data.widget.can_create_surfaces(&mut render_ctx);
        widget_data.status = CustomWidgetStatus::Active;
    }

    widget_data
        .widget
        .set_device_origin_fraction(fraction.0, fraction.1);
    let widget_scene = widget_data
        .widget
        .paint(&mut render_ctx, &style, width, height, scale);

    Some(CustomWidgetPaint {
        scene: widget_scene,
        fraction,
    })
}

#[cfg(all(test, feature = "custom-widget"))]
mod widget_origin_tests {
    use super::{widget_device_origin, widget_texture_extent};
    use blitz_dom::node::{snapped_widget_translation, split_device_origin};

    /// The research row's ruby line box: unrounded x 57.656, rounded 58.
    /// The widget is told -0.344, composited at the traversal's whole pixel
    /// 58, and so draws its first column at the browser's 57.656.
    #[test]
    fn unrounded_origin_folds_the_rounding_back_in() {
        let origin = widget_device_origin(0.0, 0.0, 57.656, 1.0);
        let (whole, fraction) = split_device_origin(origin);
        assert_eq!(whole, 58.0);
        assert!((fraction + 0.344).abs() < 1e-9, "fraction {fraction}");
        let placed = snapped_widget_translation(58.0, fraction);
        assert_eq!(placed, 58.0);
        assert!((placed + fraction - 57.656).abs() < 1e-9);
        // A left fold keeps the extent; a right fold covers the overflow.
        assert_eq!(widget_texture_extent(430.0, fraction), 430);
        assert_eq!(widget_texture_extent(430.0, 0.4), 431);
        assert_eq!(widget_texture_extent(430.0, 0.0), 430);
    }
}
