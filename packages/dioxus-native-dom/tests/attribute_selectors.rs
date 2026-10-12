//! Attribute selectors match attributes Dioxus sets (polyvox R-OJF.9).
//!
//! Stylo's attribute selectors match on the attribute's namespace (upstream
//! #864), and an `[attr]` selector with no prefix matches only the null
//! namespace. dioxus-native-dom filed every attribute under the HTML namespace,
//! so no attribute selector matched a Dioxus-built element. A motion kill
//! switch keyed on one (`#shell[data-anim="0"] *`) then never applied, and an
//! idle window redrew every frame for keyframe animations it was meant to stop.

use blitz_dom::{Document as _, DocumentConfig};
use blitz_traits::shell::{ColorScheme, Viewport};
use dioxus::prelude::*;
use dioxus_native_dom::DioxusDocument;

const CSS: &str = "@keyframes pulse { 0% { opacity: 1; } 50% { opacity: 0.5; } 100% { opacity: 1; } }
.glow { animation: pulse 2s infinite; }
#shell[data-anim=\"0\"] * { animation: none !important; }";

fn app() -> Element {
    // Dynamic values, so the attributes go through `SetAttribute` as well as
    // the template's static ones.
    let anim = use_signal(|| "0");
    let class = use_signal(|| "glow");
    rsx! {
        style { {CSS} }
        div { id: "shell", "data-anim": anim(),
            span { class: class(), "data-static": "1", "x" }
        }
    }
}

fn built() -> DioxusDocument {
    let mut doc = DioxusDocument::new(
        VirtualDom::new(app),
        DocumentConfig {
            viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
            ..Default::default()
        },
    );
    doc.initial_build();
    doc
}

#[test]
fn an_attribute_selector_matches_a_dioxus_attribute() {
    let doc = built();
    let inner = doc.inner();
    assert!(inner.query_selector("[data-anim=\"0\"]").unwrap().is_some(), "dynamic attribute");
    assert!(inner.query_selector("[data-static=\"1\"]").unwrap().is_some(), "static attribute");
}

#[test]
fn a_killed_animation_leaves_the_document_idle() {
    let mut doc = built();
    let mut inner = doc.inner_mut();
    let mut t = 0.0;
    for _ in 0..3 {
        inner.resolve(t);
        t += 0.016;
    }
    assert!(
        !inner.is_animating(),
        "an animation under #shell[data-anim=\"0\"] is still running"
    );
}
