//! An intrinsic-sizing measure must not leave the painted lines broken at the
//! width it measured with (polyvox R-OVP.17).
//!
//! `TextLayout::layout` is the object blitz-paint draws from, and a measure
//! re-breaks it. When taffy measured a text after laying it out, a description
//! laid out 512px wide over two lines was painted as one max-content line
//! running out of its box. The invariant: after a resolve, every inline root
//! is broken at its own content width.

use blitz_dom::{Document, DocumentConfig};
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

const LONG: &str = "A description long enough to wrap at the width of its own box, \
     so a max-content measure would lay it out as one much wider line.";

fn resolved(html: &str) -> HtmlDocument {
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    doc
}

/// Each `.d` paragraph's painted lines fit its content box and wrap.
fn assert_painted_at_box_width(doc: &HtmlDocument) {
    let ids = doc.query_selector_all(".d").unwrap();
    assert!(!ids.is_empty());
    for id in ids {
        let node = doc.get_node(id).unwrap();
        let box_w = node.final_layout().content_box_width();
        let text = node
            .element_data()
            .and_then(|e| e.inline_layout_data.as_ref())
            .expect("an inline root");
        let lines = text.layout.lines().count();
        let widest = text
            .layout
            .lines()
            .map(|l| l.metrics().advance - l.metrics().hanging_advance)
            .fold(0.0_f32, f32::max);
        assert!(lines > 1, "node {id}: {LONG:?} did not wrap in {box_w}px");
        assert!(
            widest <= box_w + 0.5,
            "node {id}: a {widest}px line is painted in a {box_w}px box"
        );
    }
}

#[test]
fn a_flex_row_beside_a_control_keeps_its_wrapped_lines() {
    let doc = resolved(&format!(
        r#"<html><body style="margin:0; font: 16px/20px sans-serif">
        <div style="display:flex; width:512px; gap:12px; align-items:center">
          <div style="display:flex; flex-direction:column; flex:1 1 auto; min-width:0">
            <p class="d" style="margin:0">{LONG}</p>
          </div>
          <div style="width:120px; flex:none">control</div>
        </div></body></html>"#
    ));
    assert_painted_at_box_width(&doc);
}

#[test]
fn a_grid_auto_column_keeps_its_wrapped_lines() {
    let doc = resolved(&format!(
        r#"<html><body style="margin:0; font: 16px/20px sans-serif">
        <div style="display:grid; grid-template-columns:minmax(0,1fr) auto; width:512px">
          <p class="d" style="margin:0">{LONG}</p>
          <div style="width:120px">control</div>
        </div></body></html>"#
    ));
    assert_painted_at_box_width(&doc);
}

#[test]
fn a_fit_content_box_keeps_its_wrapped_lines() {
    let doc = resolved(&format!(
        r#"<html><body style="margin:0; font: 16px/20px sans-serif">
        <div style="width:fit-content; max-width:400px">
          <p class="d" style="margin:0">{LONG}</p>
        </div></body></html>"#
    ));
    assert_painted_at_box_width(&doc);
}
