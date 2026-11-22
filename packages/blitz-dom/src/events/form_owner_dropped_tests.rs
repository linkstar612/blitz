//! polyvox R-OQU.1: a control appended and dropped in the same mutation batch
//! does not panic when the mutator flushes.
//!
//! Appending an input queues it for a form owner reset at flush. A page swap
//! that drops the subtree before the flush left that queue holding a dropped
//! node: "invalid SlotMap key used" at form.rs (the Beta 5 Speaker Identity
//! crash).

use blitz_traits::shell::{ColorScheme, Viewport};

use crate::{BaseDocument, DocumentConfig, qual_name};

#[test]
fn a_control_dropped_before_the_flush_does_not_panic() {
    let mut doc = BaseDocument::new(DocumentConfig {
        viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
        ..Default::default()
    });
    let root_id = doc.root_node().id;
    let mut mutator = doc.mutate();
    let html = mutator.create_element(qual_name!("html"), vec![]);
    let body = mutator.create_element(qual_name!("body"), vec![]);
    mutator.append_children(html, &[body]);
    mutator.append_children(root_id, &[html]);
    drop(mutator);
    doc.resolve(0.0);

    // One batch: the new page arrives with a text field, then is swapped out.
    let mut mutator = doc.mutate();
    let page = mutator.create_element(qual_name!("div"), vec![]);
    let field = mutator.create_element(qual_name!("input"), vec![]);
    let style = mutator.create_element(qual_name!("style"), vec![]);
    mutator.append_children(page, &[field, style]);
    mutator.append_children(body, &[page]);
    mutator.remove_and_drop_node(page);
    drop(mutator);

    doc.resolve(0.0);
}
