//! polyvox R-OQR.1: a hit test between a page swap and the next layout does
//! not panic on the dropped page's hoisted children.
//!
//! A page swap drops the old page's nodes (R-OQP.1). The stacking context
//! that hoisted a z-indexed child still lists it until the next layout, and a
//! pointer move in that window indexed the dropped node: "invalid SlotMap key
//! used" at paint_tree.rs (the Beta 4 Speaker Identity crash).

use blitz_traits::shell::{ColorScheme, Viewport};

use crate::{Attribute, BaseDocument, DocumentConfig, qual_name};

#[test]
fn a_hit_test_after_dropping_a_hoisted_child_does_not_panic() {
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
    let page = mutator.create_element(qual_name!("div"), vec![style("width:300px;height:200px")]);
    let raised = mutator.create_element(
        qual_name!("div"),
        vec![style("position:relative;z-index:2;width:100px;height:100px")],
    );
    mutator.append_children(page, &[raised]);
    mutator.append_children(body, &[page]);
    mutator.append_children(html, &[body]);
    mutator.append_children(root_id, &[html]);
    drop(mutator);
    // No hit test after this layout: the hoisted box is not memoised yet, so
    // the first pointer move after the swap builds it from the stale list.
    doc.resolve(0.0);

    // Swap the page out with no layout after it, then move the pointer.
    let mut mutator = doc.mutate();
    mutator.remove_and_drop_node(page);
    drop(mutator);
    let _ = doc.hit(50.0, 50.0);
    let _ = doc.hit(250.0, 250.0);

    // The next layout settles it.
    doc.resolve(0.0);
    let _ = doc.hit(50.0, 50.0);
}
