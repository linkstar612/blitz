//! A custom widget and the redraw loop (polyvox be3e2a75, R-OJF.3).
//!
//! A mounted widget used to keep the document animating, which pinned the
//! shell's event loop awake at a full core on an idle window. A static widget
//! must leave the document idle, so a widget that changes its own state in
//! `handle_event` must ask for the redraw itself. The cursor hook lets the
//! widget answer the cursor over its own content.

use blitz_dom::{CursorIcon, Document, DocumentConfig, Widget};
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_traits::{
    events::{
        BlitzPointerEvent, BlitzPointerId, MouseEventButton, MouseEventButtons, PointerCoords,
        PointerDetails, UiEvent,
    },
    shell::{ColorScheme, ShellProvider, Viewport},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct CountingShell {
    redraws: AtomicUsize,
}

impl ShellProvider for CountingShell {
    fn request_redraw(&self) {
        self.redraws.fetch_add(1, Ordering::SeqCst);
    }
}

struct Probe {
    animating: bool,
    cursor: Option<CursorIcon>,
    events: Arc<AtomicUsize>,
}

impl Widget for Probe {
    fn requires_redraw(&self) -> bool {
        self.animating
    }
    fn handle_event(&mut self, _event: &UiEvent) {
        self.events.fetch_add(1, Ordering::SeqCst);
    }
    fn cursor(&self) -> Option<CursorIcon> {
        self.cursor
    }
}

const PAGE: &str = r#"<html><body style="margin:0">
    <object id="widget" style="display:block; width:200px; height:100px"></object>
    </body></html>"#;

fn mounted(probe: Probe, shell: Arc<CountingShell>) -> HtmlDocument {
    let mut doc = HtmlDocument::from_html(
        PAGE,
        DocumentConfig {
            viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            shell_provider: Some(shell as _),
            ..Default::default()
        },
    );
    let node_id = doc
        .query_selector("#widget")
        .unwrap()
        .expect("#widget not found");
    doc.mutate().set_custom_widget(node_id, Box::new(probe));
    doc.resolve(0.0);
    doc
}

fn probe(animating: bool, cursor: Option<CursorIcon>) -> (Probe, Arc<AtomicUsize>) {
    let events = Arc::new(AtomicUsize::new(0));
    let probe = Probe {
        animating,
        cursor,
        events: events.clone(),
    };
    (probe, events)
}

fn pointer_move(doc: &mut HtmlDocument, x: f32, y: f32) {
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
}

#[test]
fn a_static_widget_leaves_the_document_idle() {
    let (p, _) = probe(false, None);
    let doc = mounted(p, Arc::default());
    assert!(!doc.is_animating());
}

#[test]
fn an_animating_widget_keeps_the_document_animating() {
    let (p, _) = probe(true, None);
    let doc = mounted(p, Arc::default());
    assert!(doc.is_animating());
}

#[test]
fn a_widget_event_requests_a_redraw() {
    let shell = Arc::new(CountingShell::default());
    let (p, events) = probe(false, None);
    let mut doc = mounted(p, shell.clone());
    let before = shell.redraws.load(Ordering::SeqCst);
    pointer_move(&mut doc, 50.0, 50.0);
    assert!(events.load(Ordering::SeqCst) > 0, "the widget saw no event");
    assert!(
        shell.redraws.load(Ordering::SeqCst) > before,
        "a handled widget event asked for no redraw"
    );
}

#[test]
fn a_widget_answers_the_cursor_over_itself() {
    let (p, _) = probe(false, Some(CursorIcon::Text));
    let mut doc = mounted(p, Arc::default());
    pointer_move(&mut doc, 50.0, 50.0);
    assert_eq!(doc.get_cursor(), Some(CursorIcon::Text));
}

#[test]
fn a_widget_with_no_answer_keeps_the_arrow() {
    let (p, _) = probe(false, None);
    let mut doc = mounted(p, Arc::default());
    pointer_move(&mut doc, 50.0, 50.0);
    assert_eq!(doc.get_cursor(), Some(CursorIcon::Default));
}
