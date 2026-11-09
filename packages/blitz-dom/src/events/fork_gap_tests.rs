//! polyvox R-OMS.40: the four R-UIP.29 fork gaps (D512), one test group
//! each: accessibility names and states, scroll anchoring, double and triple
//! click over page text, and scrollbar track paging.

use blitz_traits::events::MouseEventButton;
use blitz_traits::node_id::NodeId;
use blitz_traits::shell::{ColorScheme, Viewport};
use keyboard_types::Modifiers;

use crate::{Attribute, BaseDocument, DocumentConfig, qual_name};

use super::handle_pointerdown;

fn attr(name: markup5ever::QualName, value: &str) -> Attribute {
    Attribute {
        name,
        value: value.to_string(),
    }
}

fn style(value: &str) -> Attribute {
    attr(qual_name!("style"), value)
}

/// A 400x300 document whose body holds `children` (built by the closure).
fn make_doc(
    build: impl FnOnce(&mut crate::DocumentMutator<'_>) -> Vec<NodeId>,
) -> (BaseDocument, Vec<NodeId>) {
    let mut doc = BaseDocument::new(DocumentConfig {
        viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
        ..Default::default()
    });
    doc.add_user_agent_stylesheet("html, body, div { display: block; }");
    let root_id = doc.root_node().id;
    let mut mutator = doc.mutate();
    let html = mutator.create_element(qual_name!("html"), vec![]);
    let body = mutator.create_element(qual_name!("body"), vec![style("margin:0")]);
    let nodes = build(&mut mutator);
    mutator.append_children(body, &nodes[..1]);
    mutator.append_children(html, &[body]);
    mutator.append_children(root_id, &[html]);
    drop(mutator);
    doc.resolve(0.0);
    (doc, nodes)
}

/// A 300x100 vertical scroller of ten 40 px rows: `[list, row0, .., row9]`.
fn make_list() -> (BaseDocument, Vec<NodeId>) {
    make_doc(|m| {
        let list = m.create_element(
            qual_name!("div"),
            vec![style("width:300px;height:100px;overflow-y:auto")],
        );
        let rows: Vec<NodeId> = (0..10)
            .map(|_| m.create_element(qual_name!("div"), vec![style("height:40px")]))
            .collect();
        m.append_children(list, &rows);
        std::iter::once(list).chain(rows).collect()
    })
}

fn press(doc: &mut BaseDocument, x: f32, y: f32) {
    let target = doc.hit(x, y).map_or(doc.root_element().id, |h| h.node_id);
    handle_pointerdown(
        doc,
        target,
        x,
        y,
        MouseEventButton::Main,
        Modifiers::empty(),
        &mut |_| {},
    );
}

// (11) accessibility names and states

#[cfg(feature = "accessibility")]
#[test]
fn aria_names_and_states_reach_the_accessibility_tree() {
    use accesskit::{NodeId as AkId, Toggled};
    let (doc, nodes) = make_doc(|m| {
        let wrap = m.create_element(qual_name!("div"), vec![]);
        let toggle = m.create_element(
            qual_name!("div"),
            vec![
                attr(qual_name!("role"), "switch"),
                attr(qual_name!("aria-label"), "Mute"),
                attr(qual_name!("aria-checked"), "true"),
            ],
        );
        let slider = m.create_element(
            qual_name!("div"),
            vec![
                attr(qual_name!("role"), "slider"),
                attr(qual_name!("aria-valuenow"), "40"),
                attr(qual_name!("aria-valuemin"), "0"),
                attr(qual_name!("aria-valuemax"), "100"),
                attr(qual_name!("aria-valuetext"), "40 percent"),
            ],
        );
        let disclosure = m.create_element(
            qual_name!("div"),
            vec![
                attr(qual_name!("title"), "More"),
                attr(qual_name!("aria-expanded"), "false"),
                attr(qual_name!("aria-disabled"), "true"),
            ],
        );
        let checkbox = m.create_element(
            qual_name!("input"),
            vec![
                attr(qual_name!("type"), "checkbox"),
                attr(qual_name!("checked"), ""),
            ],
        );
        m.append_children(wrap, &[toggle, slider, disclosure, checkbox]);
        vec![wrap, toggle, slider, disclosure, checkbox]
    });
    let tree = doc.build_accessibility_tree();
    let find = |id: NodeId| {
        tree.nodes
            .iter()
            .find(|(ak, _)| *ak == AkId(id.as_u64()))
            .map(|(_, node)| node.clone())
            .expect("node in the tree")
    };
    let toggle = find(nodes[1]);
    assert_eq!(toggle.label(), Some("Mute"));
    assert_eq!(toggle.toggled(), Some(Toggled::True));
    let slider = find(nodes[2]);
    assert_eq!(slider.numeric_value(), Some(40.0));
    assert_eq!(slider.min_numeric_value(), Some(0.0));
    assert_eq!(slider.max_numeric_value(), Some(100.0));
    assert_eq!(slider.value(), Some("40 percent"));
    let disclosure = find(nodes[3]);
    assert_eq!(disclosure.label(), Some("More"), "title is the name of last resort");
    assert_eq!(disclosure.is_expanded(), Some(false));
    assert!(disclosure.is_disabled());
    let checkbox = find(nodes[4]);
    assert_eq!(checkbox.toggled(), Some(Toggled::True), "a native checkbox reads its checked state");
}

// (12) scroll anchoring

#[test]
fn content_growing_above_the_view_keeps_the_view_on_its_rows() {
    let (mut doc, nodes) = make_list();
    let list = nodes[0];
    doc.scroll_node_by(list, 0.0, -120.0, |_| {});
    doc.resolve(0.0);
    assert_eq!(doc.get_node(list).unwrap().scroll_offset().y, 120.0);

    // Row 0 sits above the view; growing it by 60 px moves every row below.
    doc.mutate()
        .set_attribute(nodes[1], qual_name!("style"), "height:100px");
    doc.resolve(0.0);
    assert_eq!(
        doc.get_node(list).unwrap().scroll_offset().y,
        180.0,
        "the offset follows the anchor row down by the 60 px it moved"
    );

    // Shrinking it back moves the offset back.
    doc.mutate()
        .set_attribute(nodes[1], qual_name!("style"), "height:40px");
    doc.resolve(0.0);
    assert_eq!(doc.get_node(list).unwrap().scroll_offset().y, 120.0);
}

#[test]
fn an_unscrolled_list_is_not_anchored() {
    let (mut doc, nodes) = make_list();
    let list = nodes[0];
    doc.mutate()
        .set_attribute(nodes[1], qual_name!("style"), "height:100px");
    doc.resolve(0.0);
    assert_eq!(doc.get_node(list).unwrap().scroll_offset().y, 0.0);
}

#[test]
fn content_growing_below_the_anchor_leaves_the_offset_alone() {
    let (mut doc, nodes) = make_list();
    let list = nodes[0];
    doc.scroll_node_by(list, 0.0, -120.0, |_| {});
    doc.resolve(0.0);
    doc.mutate()
        .set_attribute(nodes[9], qual_name!("style"), "height:100px");
    doc.resolve(0.0);
    assert_eq!(doc.get_node(list).unwrap().scroll_offset().y, 120.0);
}

// (17) double- and triple-click over page text

fn make_text() -> (BaseDocument, Vec<NodeId>) {
    make_doc(|m| {
        let para = m.create_element(qual_name!("div"), vec![style("width:380px")]);
        let text = m.create_text_node("hello brave world");
        m.append_children(para, &[text]);
        vec![para]
    })
}

/// An x over the middle of "brave" (bytes 6..11), found by probing.
fn x_over_brave(doc: &BaseDocument, y: f32) -> f32 {
    (0..380)
        .map(|x| x as f32)
        .find(|&x| doc.find_text_position(x, y).is_some_and(|(_, at)| at == 8))
        .expect("text under the probe")
}

#[test]
fn a_double_click_selects_the_word_and_a_triple_click_the_line() {
    let (mut doc, _) = make_text();
    let y = 8.0;
    let x = x_over_brave(&doc, y);

    press(&mut doc, x, y);
    assert_eq!(
        doc.get_selected_text().filter(|s| !s.is_empty()),
        None,
        "a single click only places the caret"
    );
    press(&mut doc, x, y);
    // Windows selects a word with its trailing space; either is the word.
    assert_eq!(doc.get_selected_text().as_deref().map(str::trim), Some("brave"));
    press(&mut doc, x, y);
    assert_eq!(
        doc.get_selected_text().as_deref().map(str::trim_end),
        Some("hello brave world")
    );
}

// (22) scrollbar track paging

#[cfg(feature = "scrollbars")]
#[test]
fn a_press_on_the_track_pages_the_scroller_toward_it() {
    let (mut doc, nodes) = make_list();
    let list = nodes[0];
    doc.show_scrollbars(list);
    // The thumb lane is x 288..298; at offset 0 the thumb spans y 0..32.
    press(&mut doc, 293.0, 80.0);
    assert_eq!(
        doc.get_node(list).unwrap().scroll_offset().y,
        87.5,
        "one press below the thumb pages down 87.5% of the 100 px port"
    );
    assert_eq!(doc.drag_mode, super::DragMode::ScrollbarTrack, "its release clicks nothing");
    doc.drag_mode = super::DragMode::None;
    doc.resolve(0.0);

    press(&mut doc, 293.0, 2.0);
    assert_eq!(doc.get_node(list).unwrap().scroll_offset().y, 0.0, "above the thumb pages up");
}

#[cfg(feature = "scrollbars")]
#[test]
fn a_faded_track_does_not_page() {
    let (mut doc, nodes) = make_list();
    let list = nodes[0];
    press(&mut doc, 293.0, 80.0);
    assert_eq!(doc.get_node(list).unwrap().scroll_offset().y, 0.0);
}
