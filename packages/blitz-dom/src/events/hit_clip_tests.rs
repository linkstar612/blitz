//! polyvox R-OMH.1: a scrolled list never takes the presses meant for what
//! sits above it.
//!
//! A 40 px toolbar over a 100 px scroller of ten 40 px rows. Scrolled, the
//! list used to claim a strip above its top edge as tall as its scroll
//! offset, so a press on the toolbar reached a row nobody could see.

use blitz_traits::node_id::NodeId;
use blitz_traits::shell::{ColorScheme, Viewport};

use crate::{Attribute, BaseDocument, DocumentConfig, qual_name};

fn make_doc() -> (BaseDocument, NodeId, NodeId, Vec<NodeId>) {
    let mut doc = BaseDocument::new(DocumentConfig {
        viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
        ..Default::default()
    });
    doc.add_user_agent_stylesheet("html, body, div { display: block; }");
    let root_id = doc.root_node().id;
    let style = |value: &str| Attribute {
        name: qual_name!("style"),
        value: value.to_string(),
    };
    let mut mutator = doc.mutate();
    let html = mutator.create_element(qual_name!("html"), vec![]);
    let body = mutator.create_element(qual_name!("body"), vec![style("margin:0")]);
    let toolbar = mutator.create_element(qual_name!("div"), vec![style("width:300px;height:40px")]);
    let list = mutator.create_element(
        qual_name!("div"),
        vec![style("width:300px;height:100px;overflow-y:auto")],
    );
    let rows: Vec<NodeId> = (0..10)
        .map(|_| mutator.create_element(qual_name!("div"), vec![style("height:40px")]))
        .collect();
    mutator.append_children(list, &rows);
    mutator.append_children(body, &[toolbar, list]);
    mutator.append_children(html, &[body]);
    mutator.append_children(root_id, &[html]);
    drop(mutator);
    doc.resolve(0.0);
    (doc, toolbar, list, rows)
}

fn hit(doc: &BaseDocument, x: f32, y: f32) -> Option<NodeId> {
    doc.hit(x, y).map(|h| h.node_id)
}

#[test]
fn a_scrolled_list_leaves_the_toolbar_above_it_alone() {
    let (mut doc, toolbar, list, rows) = make_doc();
    assert_eq!(hit(&doc, 150.0, 20.0), Some(toolbar), "unscrolled, the toolbar is hit");
    doc.scroll_node_by(list, 0.0, -200.0, |_| {});
    doc.resolve(0.0);
    assert!(doc.get_node(list).unwrap().scroll_offset().y > 0.0, "the list scrolled");
    assert_eq!(hit(&doc, 150.0, 20.0), Some(toolbar), "a press on the toolbar reaches the toolbar");
    // Inside the list the rows still take presses, scrolled.
    let inside = hit(&doc, 150.0, 60.0).expect("a row");
    assert!(rows.contains(&inside), "the list's top row takes a press inside the list");
}

#[test]
fn a_scrolled_list_takes_no_press_below_its_box() {
    let (mut doc, _toolbar, list, rows) = make_doc();
    doc.scroll_node_by(list, 0.0, -40.0, |_| {});
    doc.resolve(0.0);
    // Below the list's 140 px bottom there is nothing but the body.
    let below = hit(&doc, 150.0, 200.0);
    assert!(below.is_none_or(|id| !rows.contains(&id)), "a row below the list took a press");
}
