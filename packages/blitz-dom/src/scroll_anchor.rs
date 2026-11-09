//! polyvox R-OMS.40: scroll anchoring (css-scroll-anchoring-1, on by default
//! in Chromium and so in WebView2).
//!
//! Before layout, every scrolled element picks an anchor: the first in-flow
//! box in its scrollport, descending into a box that is only partly visible.
//! After layout the scroller's offset moves by however far the anchor moved,
//! so content that grows or shrinks above the view (a settings card filling
//! in after its first paint) no longer shifts what the reader is looking at.
//!
//! Left out against the spec: `overflow-anchor` (stylo's servo build does not
//! parse it), the suppression triggers, and the viewport's own scroll offset.
//! The native shell scrolls inside elements, never the viewport.

use blitz_traits::node_id::NodeId;

use crate::BaseDocument;
use crate::node::{Node, NodeData};

/// One scroller's anchor and where it sat, in CSS px relative to the
/// scroller's border box with its scroll offset not applied.
pub(crate) struct ScrollAnchor {
    scroller: NodeId,
    anchor: NodeId,
    x: f64,
    y: f64,
}

fn has_layout(node: &Node) -> bool {
    matches!(node.data, NodeData::Element(_) | NodeData::AnonymousBlock(_))
}

fn is_scroll_container(node: &Node) -> bool {
    use style::values::computed::Overflow;
    node.primary_styles().is_some_and(|style| {
        let scrolls = |o: Overflow| matches!(o, Overflow::Auto | Overflow::Scroll | Overflow::Hidden);
        scrolls(style.clone_overflow_x()) || scrolls(style.clone_overflow_y())
    })
}

impl BaseDocument {
    /// Pick an anchor in every scrolled element (before layout).
    pub(crate) fn select_scroll_anchors(&self) -> Vec<ScrollAnchor> {
        let mut anchors = Vec::new();
        for (_, node) in self.nodes.iter() {
            if !has_layout(node) {
                continue;
            }
            let offset = *node.scroll_offset();
            if (offset.x == 0.0 && offset.y == 0.0) || !is_scroll_container(node) {
                continue;
            }
            let layout = node.final_layout();
            let window = kurbo::Rect::new(
                offset.x + layout.border.left as f64,
                offset.y + layout.border.top as f64,
                offset.x + (layout.size.width - layout.border.right) as f64,
                offset.y + (layout.size.height - layout.border.bottom) as f64,
            );
            if let Some((anchor, x, y)) = self.find_anchor_in(node.id, 0.0, 0.0, window) {
                anchors.push(ScrollAnchor {
                    scroller: node.id,
                    anchor,
                    x,
                    y,
                });
            }
        }
        anchors
    }

    /// The first anchor candidate among `parent_id`'s layout children, whose
    /// border box origin is `(origin_x, origin_y)` in the scroller's
    /// coordinates.
    fn find_anchor_in(
        &self,
        parent_id: NodeId,
        origin_x: f64,
        origin_y: f64,
        window: kurbo::Rect,
    ) -> Option<(NodeId, f64, f64)> {
        let parent = &self.nodes[parent_id];
        let children = parent.layout_children.borrow();
        for &child_id in children.as_deref().unwrap_or(&[]) {
            let Some(child) = self.nodes.get(child_id) else {
                continue;
            };
            if !has_layout(child) || child.is_out_of_flow() {
                continue;
            }
            let layout = child.final_layout();
            if layout.size.width <= 0.0 || layout.size.height <= 0.0 {
                continue;
            }
            let x = origin_x + layout.location.x as f64;
            let y = origin_y + layout.location.y as f64;
            let rect = kurbo::Rect::new(
                x,
                y,
                x + layout.size.width as f64,
                y + layout.size.height as f64,
            );
            let overlap = rect.intersect(window);
            if overlap.width() <= 0.0 || overlap.height() <= 0.0 {
                continue;
            }
            let fully_visible = overlap == rect;
            // A layout-generated box is never the anchor (its id dies when the
            // box tree is rebuilt), and a nested scroller is anchored by its
            // own pass, so neither is descended past as a box.
            if child.is_anonymous() {
                if let Some(found) = self.find_anchor_in(child_id, x, y, window) {
                    return Some(found);
                }
                continue;
            }
            if fully_visible || is_scroll_container(child) {
                return Some((child_id, x, y));
            }
            return Some(
                self.find_anchor_in(child_id, x, y, window)
                    .unwrap_or((child_id, x, y)),
            );
        }
        None
    }

    /// Where `anchor` sits now relative to `scroller`, walking layout parents
    /// up from the anchor. `None` once it is no longer inside the scroller.
    fn anchor_position(&self, scroller: NodeId, anchor: NodeId) -> Option<(f64, f64)> {
        let (mut x, mut y) = (0.0, 0.0);
        let mut current = anchor;
        while current != scroller {
            let node = self.nodes.get(current)?;
            if !has_layout(node) {
                return None;
            }
            let location = node.final_layout().location;
            x += location.x as f64;
            y += location.y as f64;
            current = node.layout_parent.get()?;
        }
        Some((x, y))
    }

    /// Move each scroller by however far its anchor moved (after layout,
    /// before the scroll offsets are clamped).
    pub(crate) fn apply_scroll_anchors(&mut self, anchors: Vec<ScrollAnchor>) {
        for ScrollAnchor {
            scroller,
            anchor,
            x,
            y,
        } in anchors
        {
            let Some((new_x, new_y)) = self.anchor_position(scroller, anchor) else {
                continue;
            };
            let (dx, dy) = (new_x - x, new_y - y);
            if dx.abs() < 0.01 && dy.abs() < 0.01 {
                continue;
            }
            let Some(data) = self
                .nodes
                .get_mut(scroller)
                .and_then(|node| node.try_layout_data_mut())
            else {
                continue;
            };
            data.scroll_offset.x += dx;
            data.scroll_offset.y += dy;
        }
    }
}
