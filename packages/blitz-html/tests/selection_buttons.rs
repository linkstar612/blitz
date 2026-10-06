//! What a non-primary press and Ctrl+A do to the page's text selection
//! (polyvox R-OIP.5).
//!
//! The reference is Blink, measured through `Input.dispatchMouseEvent` against
//! Edge 141 headless on a two-paragraph page: a right press INSIDE a selection
//! keeps it, a right press on other text or on empty space clears it, and
//! Ctrl+A with nothing editable focused selects every paragraph.

use blitz_dom::{Document, DocumentConfig};
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_traits::{
    events::{
        BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, KeyState, MouseEventButton,
        MouseEventButtons, Point, PointerCoords, PointerDetails, UiEvent,
    },
    shell::{ColorScheme, Viewport},
};
use keyboard_types::{Code, Key, Location, Modifiers};
use std::sync::Arc;

const PAGE: &str = r#"<html><body style="margin:0; font: 16px/20px sans-serif">
    <p id="a" style="margin:0; height:20px">alpha bravo charlie delta echo</p>
    <p id="b" style="margin:0; height:20px">foxtrot golf hotel india juliet</p>
    <div style="height:200px"></div>
    </body></html>"#;

fn doc(html: &str) -> HtmlDocument {
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    doc
}

fn pointer(x: f32, y: f32, button: MouseEventButton, buttons: MouseEventButtons) -> BlitzPointerEvent {
    BlitzPointerEvent {
        id: BlitzPointerId::Mouse,
        is_primary: true,
        coords: PointerCoords {
            page_x: x,
            page_y: y,
            screen_x: x,
            screen_y: y,
            client_x: x,
            client_y: y,
        },
        button,
        buttons,
        mods: Default::default(),
        details: PointerDetails::default(),
        element: Point::default(),
        active_pointers: Default::default(),
    }
}

/// Select "bravo charlie" in `#a` directly and check it took, so no test
/// below can pass on an empty selection. A synthetic left drag selects nothing
/// in this headless harness (the drag never arms `Selecting`), which is a
/// fixture limit, not the behavior under test. Also checks that the point the
/// right presses use resolves to text inside the selection.
fn select_phrase(doc: &mut HtmlDocument) -> String {
    let a = doc.query_selector("#a").unwrap().expect("#a");
    doc.set_text_selection(a, 6, a, 19);
    let text = selected(doc);
    assert_eq!(text, "bravo charlie", "the fixture selection did not take");
    let (node, offset) = doc.find_text_position(INSIDE.0, INSIDE.1).expect("hit test found no text");
    assert!(node == a && (6..=19).contains(&offset), "{INSIDE:?} is not inside the selection: {offset}");
    text
}

/// A point on "charlie", inside the fixture selection.
const INSIDE: (f32, f32) = (110.0, 10.0);

/// A right press at (`x`, `y`), optionally jiggled before the release.
fn right_click(doc: &mut HtmlDocument, x: f32, y: f32, jiggle: f32) {
    let sec = MouseEventButton::Secondary;
    let held = MouseEventButtons::from(sec);
    doc.handle_ui_event(UiEvent::PointerMove(pointer(x, y, sec, MouseEventButtons::None)));
    doc.handle_ui_event(UiEvent::PointerDown(pointer(x, y, sec, held)));
    if jiggle != 0.0 {
        doc.handle_ui_event(UiEvent::PointerMove(pointer(x + jiggle, y, sec, held)));
    }
    doc.handle_ui_event(UiEvent::PointerUp(pointer(x + jiggle, y, sec, MouseEventButtons::None)));
}

fn selected(doc: &HtmlDocument) -> String {
    doc.get_selected_text().unwrap_or_default()
}

#[test]
fn a_right_press_inside_the_selection_keeps_it() {
    let mut doc = doc(PAGE);
    let before = select_phrase(&mut doc);
    right_click(&mut doc, INSIDE.0, INSIDE.1, 0.0);
    assert_eq!(selected(&doc), before);
}

#[test]
fn a_jiggled_right_press_inside_the_selection_keeps_it() {
    // Past the 2px drag threshold. A secondary drag used to arm text
    // selection and move the focus end to the release point.
    let mut doc = doc(PAGE);
    let before = select_phrase(&mut doc);
    right_click(&mut doc, INSIDE.0, INSIDE.1, 6.0);
    assert_eq!(selected(&doc), before);
}

#[test]
fn a_right_press_on_other_text_clears_the_selection() {
    let mut doc = doc(PAGE);
    select_phrase(&mut doc);
    right_click(&mut doc, 40.0, 30.0, 0.0);
    assert_eq!(selected(&doc), "");
}

#[test]
fn a_right_press_on_empty_space_clears_the_selection() {
    let mut doc = doc(PAGE);
    select_phrase(&mut doc);
    right_click(&mut doc, 100.0, 150.0, 0.0);
    assert_eq!(selected(&doc), "");
}

#[test]
fn a_left_press_inside_the_selection_still_collapses_it() {
    // The keep rule is for non-primary buttons only. A left press inside a
    // selection starts a new one, which is what a click to deselect means.
    let mut doc = doc(PAGE);
    select_phrase(&mut doc);
    let main = MouseEventButton::Main;
    doc.handle_ui_event(UiEvent::PointerDown(pointer(INSIDE.0, INSIDE.1, main, main.into())));
    doc.handle_ui_event(UiEvent::PointerUp(pointer(INSIDE.0, INSIDE.1, main, MouseEventButtons::None)));
    assert_eq!(selected(&doc), "");
}

fn ctrl_a() -> UiEvent {
    UiEvent::KeyDown(BlitzKeyEvent {
        key: Key::Character("a".into()),
        code: Code::KeyA,
        modifiers: Modifiers::CONTROL,
        location: Location::Standard,
        is_auto_repeating: false,
        is_composing: false,
        state: KeyState::Pressed,
        text: None,
    })
}

#[test]
fn ctrl_a_with_nothing_focused_selects_every_paragraph() {
    let mut doc = doc(PAGE);
    select_phrase(&mut doc);
    doc.handle_ui_event(ctrl_a());
    let text = selected(&doc);
    assert!(text.contains("alpha"), "{text:?}");
    assert!(text.contains("juliet"), "{text:?}");
}

#[test]
fn ctrl_a_skips_user_select_none_at_the_ends() {
    let mut doc = doc(r#"<html><body style="margin:0">
        <div style="user-select:none">chrome label</div>
        <p>reading text</p>
        <div style="user-select:none">footer label</div>
        </body></html>"#);
    doc.handle_ui_event(ctrl_a());
    let text = selected(&doc);
    assert!(text.contains("reading text"), "{text:?}");
    assert!(!text.contains("chrome label"), "{text:?}");
    assert!(!text.contains("footer label"), "{text:?}");
}

/// With a field focused, Ctrl+A belongs to the field's editor, so the page
/// selection must not move. The field's own select-all is the editor's arm and
/// does not run in this headless harness, so only the page half is asserted.
#[test]
fn ctrl_a_in_a_focused_field_leaves_the_page_selection_alone() {
    let mut doc = doc(r#"<html><body style="margin:0">
        <p id="p">page text</p>
        <input id="f" value="field text" style="width:200px">
        </body></html>"#);
    let p = doc.query_selector("#p").unwrap().expect("#p");
    doc.set_text_selection(p, 0, p, 4);
    assert_eq!(selected(&doc), "page", "the fixture selection did not take");
    let field = doc.query_selector("#f").unwrap().expect("#f");
    doc.set_focus_to(field);
    doc.handle_ui_event(ctrl_a());
    assert_eq!(selected(&doc), "page", "a focused field's Ctrl+A selected the page");
}
