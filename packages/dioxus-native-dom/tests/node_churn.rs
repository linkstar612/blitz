//! Removed nodes are dropped once their element id is reused (polyvox R-OQP.1).
//!
//! The mutation writer re-points a reused `ElementId` at a new node. Only
//! `assign_node_id` used to drop the detached node the id had named, so every
//! text node, placeholder and template root reused through `map_new_node` left
//! its predecessor orphaned in the node tree for the life of the document, and
//! a replaced template placeholder was never dropped at all. A VR session grew
//! the tree by about 320 nodes a second, 400 MiB of node storage in an hour.

use std::{cell::Cell, rc::Rc};

use blitz_dom::{Document as _, DocumentConfig};
use blitz_traits::shell::{ColorScheme, Viewport};
use dioxus::prelude::*;
use dioxus_native_dom::DioxusDocument;

/// Every render flips a branch, shifts a keyed list by one row, changes a text
/// node and toggles an optional element: each of Dioxus's remove and replace
/// paths runs on every pass.
fn app() -> Element {
    let tick = use_hook(|| Rc::new(Cell::new(0u32)));
    let i = tick.get();
    tick.set(i + 1);
    rsx! {
        div {
            if i % 2 == 0 {
                span { "even {i}" }
            } else {
                b { "odd" }
            }
            for k in i..i + 3 {
                p { key: "{k}", "{k}" }
            }
            "{i}"
            if i % 3 == 0 {
                em { "third" }
            }
        }
    }
}

/// One render, then a layout pass. In debug builds the pass checks that every
/// layout child it caches is still a live node, which is how a node dropped in
/// the same batch that detached it was caught (the native settings sweep hit
/// "layout child ... is not in the slab").
fn rerender(doc: &mut DioxusDocument) {
    doc.vdom.mark_dirty(ScopeId::APP);
    doc.poll(None);
    doc.inner_mut().resolve(0.0);
}

#[test]
fn churning_renders_keep_the_node_count_flat() {
    let mut doc = DioxusDocument::new(
        VirtualDom::new(app),
        DocumentConfig {
            viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
            ..Default::default()
        },
    );
    doc.initial_build();
    doc.inner_mut().resolve(0.0);
    for _ in 0..20 {
        rerender(&mut doc);
    }
    let settled = doc.inner().tree().len();
    let (_, created_before) = blitz_dom::node_counters();
    for _ in 0..500 {
        rerender(&mut doc);
    }
    let after = doc.inner().tree().len();
    let (_, created_after) = blitz_dom::node_counters();
    assert!(
        created_after > created_before + 500,
        "the renders did not create nodes, so the test churned nothing"
    );
    assert!(
        after <= settled + 16,
        "the node tree grew from {settled} to {after} nodes over 500 renders"
    );
}
