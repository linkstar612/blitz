//! The cursor over text and fields, and Ctrl+A inside a focused field
//! (polyvox R-OJF.3, R-OJF.4).
//!
//! The reference is a browser: the I-beam over selectable text and over a text
//! field, the arrow over a block's own padding and over `user-select: none`
//! text, and Ctrl+A in a focused `<input>` or `<textarea>` selecting that
//! field's text and nothing on the page.

use blitz_dom::{CursorIcon, Document, DocumentConfig};
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_traits::{
    events::{
        BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, KeyState, MouseEventButton,
        MouseEventButtons, PointerCoords, PointerDetails, UiEvent,
    },
    shell::{ColorScheme, Viewport},
};
use keyboard_types::{Code, Key, Location, Modifiers};
use std::sync::Arc;

fn test_font() -> Vec<u8> {
    ["C:/Windows/Fonts/segoeui.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"]
        .iter()
        .find_map(|p| std::fs::read(p).ok())
        .expect("a system TrueType font to shape the fixture with")
}

fn doc(html: &str) -> HtmlDocument {
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            font_ctx: Some(blitz_dom::build_single_font_ctx(&test_font())),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    doc
}

fn hover(doc: &mut HtmlDocument, x: f32, y: f32) -> Option<CursorIcon> {
    doc.handle_ui_event(UiEvent::PointerMove(BlitzPointerEvent {
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
        button: MouseEventButton::Main,
        buttons: MouseEventButtons::None,
        mods: Default::default(),
        details: PointerDetails::default(),
        element: Default::default(),
        active_pointers: Default::default(),
    }));
    doc.get_cursor()
}

/// A short word in a block padded by 40px on the left: x 10 is the block's own
/// box, x 50 is on the first glyph.
const PADDED: &str = r#"<html><body style="margin:0; font: 16px/20px sans-serif">
    <p id="t" style="margin:0; padding-left:40px; height:20px">Wide words</p>
    <p id="n" style="margin:0; padding-left:40px; height:20px; user-select:none">Wide label</p>
    <input id="f" value="field" style="display:block; margin:0; width:200px; height:20px">
    </body></html>"#;

#[test]
fn selectable_text_shows_the_i_beam() {
    let mut doc = doc(PADDED);
    assert_eq!(hover(&mut doc, 50.0, 10.0), Some(CursorIcon::Text));
}

/// Entering the paragraph on its padding and then moving onto its text stays
/// on the same hovered element, which used to keep the arrow over the text.
#[test]
fn moving_from_a_blocks_padding_onto_its_text_switches_to_the_i_beam() {
    let mut doc = doc(PADDED);
    assert_eq!(hover(&mut doc, 10.0, 10.0), Some(CursorIcon::Default));
    assert_eq!(hover(&mut doc, 50.0, 10.0), Some(CursorIcon::Text));
    assert_eq!(hover(&mut doc, 10.0, 10.0), Some(CursorIcon::Default));
}

#[test]
fn user_select_none_text_keeps_the_arrow() {
    let mut doc = doc(PADDED);
    assert_eq!(hover(&mut doc, 50.0, 30.0), Some(CursorIcon::Default));
}

/// `user-select: none` on a container reaches the text in its children, the
/// way a Tailwind `select-none` row does in the web UI.
#[test]
fn user_select_none_on_an_ancestor_keeps_the_arrow() {
    let mut doc = doc(r#"<html><body style="margin:0; font: 16px/20px sans-serif">
        <div style="user-select:none"><span>Wide label</span></div>
        </body></html>"#);
    assert_eq!(hover(&mut doc, 5.0, 10.0), Some(CursorIcon::Default));
}

#[test]
fn a_text_field_shows_the_i_beam() {
    let mut doc = doc(PADDED);
    // Past the end of the value: the field's box, not a glyph.
    assert_eq!(hover(&mut doc, 180.0, 50.0), Some(CursorIcon::Text));
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

fn field_selection(doc: &HtmlDocument, id: usize) -> String {
    doc.get_node(id)
        .and_then(|n| n.element_data())
        .and_then(|e| e.text_input_data())
        .and_then(|t| t.editor.selected_text().map(str::to_owned))
        .unwrap_or_default()
}

fn ctrl_a_selects_the_field_only(tag: &str) {
    let html = format!(
        r#"<html><body style="margin:0">
        <p id="p">page text</p>
        {tag}
        <p>more page text</p>
        </body></html>"#
    );
    let mut doc = doc(&html);
    let field = doc.query_selector("#f").unwrap().expect("#f");
    doc.set_focus_to(field);
    doc.handle_ui_event(ctrl_a());
    assert_eq!(field_selection(&doc, field), "secret value", "{tag}");
    assert_eq!(doc.get_selected_text().unwrap_or_default(), "", "{tag}: the page was selected");
}

#[test]
fn ctrl_a_in_a_focused_input_selects_its_text_only() {
    ctrl_a_selects_the_field_only(r#"<input id="f" type="password" value="secret value">"#);
}

#[test]
fn ctrl_a_in_a_focused_textarea_selects_its_text_only() {
    // The editor seeds from the `value` attribute (`create_text_editor`), which
    // is how dioxus-native sets a textarea's value; child text is not read.
    ctrl_a_selects_the_field_only(r#"<textarea id="f" value="secret value"></textarea>"#);
}
