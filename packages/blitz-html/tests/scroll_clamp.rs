//! A scroller whose content shrinks under a scroll offset pulls the offset back
//! inside its new range on the next resolve, the way a browser clamps
//! `scrollTop` when scrollable overflow shrinks (polyvox R-OIP.6). Before this,
//! only a later wheel event clamped it, so the box painted nothing until then.

use std::sync::Arc;

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};

const BLUE: [u8; 3] = [0, 0, 255];

/// A 100 px scroller holding `content` px of blue and then `extra` px of
/// filler, so dropping the filler shrinks the overflow to `content`.
fn doc(content: u32, extra: u32) -> HtmlDocument {
    let mut doc = HtmlDocument::from_html(
        &format!(
            r#"<html><body style="margin:0">
            <div id="scroller" style="width:100px; height:100px; overflow-y:auto; background:#ffffff">
                <div style="height:{content}px; background:#0000ff;"></div>
                <div id="extra" style="height:{extra}px; background:#ffffff;"></div>
            </div>
        </body></html>"#
        ),
        DocumentConfig {
            viewport: Some(Viewport::new(100, 100, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    doc
}

fn pixel(doc: &mut HtmlDocument, x: usize, y: usize) -> [u8; 3] {
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| paint_scene(scene, doc, 1.0, 100, 100, 0, 0),
        100,
        100,
    );
    let idx = (y * 100 + x) * 4;
    [buffer[idx], buffer[idx + 1], buffer[idx + 2]]
}

#[test]
fn shrinking_content_clamps_the_scroll_offset_and_stays_painted() {
    let mut doc = doc(300, 700);
    let scroller = doc.query_selector("#scroller").unwrap().unwrap();
    let extra = doc.query_selector("#extra").unwrap().unwrap();

    // To the end: 1000 of content in a 100 box.
    doc.scroll_by(Some(scroller), 0.0, -10_000.0, &mut |_| {});
    assert_eq!(doc.get_node(scroller).unwrap().scroll_offset.y, 900.0);

    // The filler goes: 300 of content is left, so the new end is 200.
    doc.mutate().remove_and_drop_node(extra);
    doc.resolve(0.0);

    assert_eq!(
        doc.get_node(scroller).unwrap().scroll_offset.y,
        200.0,
        "the offset was left past the new end of the content"
    );
    assert_eq!(pixel(&mut doc, 40, 50), BLUE, "the content is out of view");
}

#[test]
fn content_that_no_longer_overflows_scrolls_back_to_the_top() {
    let mut doc = doc(50, 950);
    let scroller = doc.query_selector("#scroller").unwrap().unwrap();
    let extra = doc.query_selector("#extra").unwrap().unwrap();
    doc.scroll_by(Some(scroller), 0.0, -400.0, &mut |_| {});
    assert_eq!(doc.get_node(scroller).unwrap().scroll_offset.y, 400.0);

    doc.mutate().remove_and_drop_node(extra);
    doc.resolve(0.0);

    assert_eq!(doc.get_node(scroller).unwrap().scroll_offset.y, 0.0);
    assert_eq!(pixel(&mut doc, 40, 20), BLUE);
}

#[test]
fn an_offset_still_in_range_is_left_alone() {
    let mut doc = doc(300, 700);
    let scroller = doc.query_selector("#scroller").unwrap().unwrap();
    doc.scroll_by(Some(scroller), 0.0, -350.0, &mut |_| {});
    doc.resolve(0.0);
    assert_eq!(doc.get_node(scroller).unwrap().scroll_offset.y, 350.0);
}

