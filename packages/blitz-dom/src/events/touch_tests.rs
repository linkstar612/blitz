//! polyvox R-OLR.2: a finger that scrolls never selects.
//!
//! Each test drives `EventDriver` the way the shell does (a synthetic move,
//! then the button event) with a handler that cancels `pointerdown`, as every
//! Polyvox kit control does, and records where clicks land.

use std::sync::Arc;

use atomic_refcell::AtomicRefCell;
use blitz_traits::events::{
    BlitzPointerEvent, BlitzPointerId, DomEvent, DomEventData, EventState, MouseEventButton,
    MouseEventButtons, PointerCoords, PointerDetails, UiEvent,
};
use blitz_traits::node_id::NodeId;
use blitz_traits::shell::{ColorScheme, Viewport};
use keyboard_types::Modifiers;

use crate::scrolling::{FlingState, ScrollAnimationState};
use crate::{
    Attribute, BaseDocument, Document, DocumentConfig, EventDriver, EventHandler, qual_name,
};

/// Cancels every `pointerdown` and records the target of every `click`.
struct Recorder<'a> {
    clicks: &'a mut Vec<NodeId>,
}

thread_local! {
    /// Targets of every `pointerleave`, in order.
    static LEAVES: std::cell::RefCell<Vec<NodeId>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Targets of every `pointerenter`, in order.
    static ENTERS: std::cell::RefCell<Vec<NodeId>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl EventHandler for Recorder<'_> {
    fn handle_event(
        &mut self,
        _chain: &[NodeId],
        event: &mut DomEvent,
        _doc: &mut dyn Document,
        event_state: &mut EventState,
    ) {
        match event.data {
            DomEventData::PointerDown(_) => event_state.prevent_default(),
            DomEventData::Click(_) => self.clicks.push(event.target),
            DomEventData::PointerLeave(_) => LEAVES.with(|l| l.borrow_mut().push(event.target)),
            DomEventData::PointerEnter(_) => ENTERS.with(|l| l.borrow_mut().push(event.target)),
            _ => {}
        }
    }
}

/// A 100 px tall scroller with ten 40 px rows, each `touch-action: none` when
/// `pinned`, so a drag over them can never become a pan.
fn make_doc(pinned: bool) -> (BaseDocument, Vec<NodeId>) {
    let mut doc = BaseDocument::new(DocumentConfig {
        viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
        ..Default::default()
    });
    // No user agent sheet is loaded here, so every element is inline.
    doc.add_user_agent_stylesheet("html, body, div { display: block; }");
    let root_id = doc.root_node().id;
    let style = |value: &str| Attribute {
        name: qual_name!("style"),
        value: value.to_string(),
    };
    let row_style = if pinned {
        "height:40px;touch-action:none"
    } else {
        "height:40px"
    };

    let mut mutator = doc.mutate();
    let html = mutator.create_element(qual_name!("html"), vec![]);
    let body = mutator.create_element(qual_name!("body"), vec![style("margin:0")]);
    let list = mutator.create_element(
        qual_name!("div"),
        vec![style("width:300px;height:100px;overflow-y:auto")],
    );
    let rows: Vec<NodeId> = (0..10)
        .map(|_| mutator.create_element(qual_name!("div"), vec![style(row_style)]))
        .collect();
    mutator.append_children(list, &rows);
    mutator.append_children(body, &[list]);
    mutator.append_children(html, &[body]);
    mutator.append_children(root_id, &[html]);
    drop(mutator);

    doc.resolve(0.0);
    (doc, rows)
}

/// The shell's pointer state: pressed buttons and the active fingers.
struct Touch {
    buttons: MouseEventButtons,
    active: Arc<AtomicRefCell<Vec<BlitzPointerEvent>>>,
}

impl Touch {
    fn new() -> Self {
        Self {
            buttons: MouseEventButtons::None,
            active: Arc::new(AtomicRefCell::new(Vec::new())),
        }
    }

    fn event(&self, id: BlitzPointerId, x: f32, y: f32, scroll_y: f32) -> BlitzPointerEvent {
        BlitzPointerEvent {
            id,
            is_primary: true,
            coords: PointerCoords {
                screen_x: x,
                screen_y: y,
                client_x: x,
                client_y: y,
                page_x: x,
                page_y: y + scroll_y,
            },
            button: MouseEventButton::Main,
            buttons: self.buttons,
            mods: Modifiers::empty(),
            details: PointerDetails::default(),
            element: Default::default(),
            active_pointers: Arc::clone(&self.active),
        }
    }
}

fn drive(doc: &mut BaseDocument, clicks: &mut Vec<NodeId>, event: UiEvent) {
    let mut driver = EventDriver::new(doc, Recorder { clicks });
    driver.handle_ui_event(event);
}

fn down(
    doc: &mut BaseDocument,
    clicks: &mut Vec<NodeId>,
    t: &mut Touch,
    id: BlitzPointerId,
    x: f32,
    y: f32,
) {
    t.buttons |= MouseEventButtons::Primary;
    let ev = t.event(id, x, y, 0.0);
    if id != BlitzPointerId::Mouse {
        t.active.borrow_mut().push(ev.clone());
        drive(doc, clicks, UiEvent::PointerMove(ev.clone()));
    }
    drive(doc, clicks, UiEvent::PointerDown(ev));
}

fn move_to(
    doc: &mut BaseDocument,
    clicks: &mut Vec<NodeId>,
    t: &Touch,
    id: BlitzPointerId,
    x: f32,
    y: f32,
    scroll_y: f32,
) {
    drive(
        doc,
        clicks,
        UiEvent::PointerMove(t.event(id, x, y, scroll_y)),
    );
}

fn up(
    doc: &mut BaseDocument,
    clicks: &mut Vec<NodeId>,
    t: &mut Touch,
    id: BlitzPointerId,
    x: f32,
    y: f32,
) {
    // The shell XORs the button out of its mask on every release.
    t.buttons ^= MouseEventButtons::Primary;
    if id != BlitzPointerId::Mouse {
        drive(doc, clicks, UiEvent::PointerMove(t.event(id, x, y, 0.0)));
        t.active.borrow_mut().retain(|p| p.id != id);
    }
    drive(doc, clicks, UiEvent::PointerUp(t.event(id, x, y, 0.0)));
}

const F1: BlitzPointerId = BlitzPointerId::Finger(1);
const F2: BlitzPointerId = BlitzPointerId::Finger(2);

#[test]
fn a_finger_tap_clicks_the_pressed_row() {
    let (mut doc, rows) = make_doc(false);
    let (mut clicks, mut t) = (Vec::new(), Touch::new());
    down(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    move_to(&mut doc, &mut clicks, &t, F1, 53.0, 14.0, 0.0);
    up(&mut doc, &mut clicks, &mut t, F1, 53.0, 14.0);
    assert_eq!(clicks, vec![rows[0]]);
}

#[test]
fn a_finger_drag_where_panning_is_forbidden_clicks_nothing() {
    // touch-action: none keeps the drag from ever becoming a pan, so before
    // R-OLR.2 the release clicked whatever row was under the finger.
    let (mut doc, _rows) = make_doc(true);
    let (mut clicks, mut t) = (Vec::new(), Touch::new());
    down(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    move_to(&mut doc, &mut clicks, &t, F1, 50.0, 30.0, 0.0);
    move_to(&mut doc, &mut clicks, &t, F1, 50.0, 60.0, 0.0);
    up(&mut doc, &mut clicks, &mut t, F1, 50.0, 60.0);
    assert!(clicks.is_empty(), "drag clicked {clicks:?}");
}

#[test]
fn a_drag_measured_on_screen_cancels_even_when_the_page_moves_with_the_finger() {
    // The page scrolls under the finger, so page coordinates stay put while
    // the finger travels 40 px on screen.
    let (mut doc, _rows) = make_doc(true);
    let (mut clicks, mut t) = (Vec::new(), Touch::new());
    down(&mut doc, &mut clicks, &mut t, F1, 50.0, 50.0);
    move_to(&mut doc, &mut clicks, &t, F1, 50.0, 30.0, 20.0);
    move_to(&mut doc, &mut clicks, &t, F1, 50.0, 10.0, 40.0);
    move_to(&mut doc, &mut clicks, &t, F1, 50.0, 50.0, 0.0);
    up(&mut doc, &mut clicks, &mut t, F1, 50.0, 50.0);
    assert!(clicks.is_empty(), "drag clicked {clicks:?}");
}

#[test]
fn a_press_that_stops_a_fling_clicks_nothing() {
    let (mut doc, rows) = make_doc(false);
    let (mut clicks, mut t) = (Vec::new(), Touch::new());
    doc.scroll_animation = ScrollAnimationState::Fling(FlingState {
        target: rows[0],
        last_seen_time: 0.0,
        x_velocity: 0.0,
        y_velocity: 2.0,
    });
    down(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    assert_eq!(
        doc.scroll_animation,
        ScrollAnimationState::None,
        "a cancelled pointerdown still stops the fling"
    );
    up(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    assert!(clicks.is_empty(), "the catch clicked {clicks:?}");

    // The next tap is a plain tap again.
    down(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    up(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    assert_eq!(clicks, vec![rows[0]]);
}

#[test]
fn a_second_finger_cancels_both_clicks() {
    let (mut doc, _rows) = make_doc(false);
    let (mut clicks, mut t) = (Vec::new(), Touch::new());
    down(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    down(&mut doc, &mut clicks, &mut t, F2, 50.0, 50.0);
    up(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    up(&mut doc, &mut clicks, &mut t, F2, 50.0, 50.0);
    assert!(clicks.is_empty(), "two fingers clicked {clicks:?}");

    // Lifting in the other order clicks nothing either.
    down(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    down(&mut doc, &mut clicks, &mut t, F2, 50.0, 50.0);
    up(&mut doc, &mut clicks, &mut t, F2, 50.0, 50.0);
    up(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    assert!(clicks.is_empty(), "two fingers clicked {clicks:?}");
}

#[test]
fn a_finger_click_goes_only_to_the_pressed_row() {
    // A 4 px roll across the row edge stays a tap, and the tap belongs to the
    // row the finger went down on, not the one under the lift.
    let (mut doc, rows) = make_doc(true);
    let (mut clicks, mut t) = (Vec::new(), Touch::new());
    down(&mut doc, &mut clicks, &mut t, F1, 50.0, 38.0);
    move_to(&mut doc, &mut clicks, &t, F1, 50.0, 42.0, 0.0);
    up(&mut doc, &mut clicks, &mut t, F1, 50.0, 42.0);
    assert_eq!(clicks, vec![rows[0]]);
}

#[test]
fn a_finger_lift_clears_hover() {
    let (mut doc, _rows) = make_doc(false);
    let (mut clicks, mut t) = (Vec::new(), Touch::new());
    down(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    down(&mut doc, &mut clicks, &mut t, F2, 50.0, 50.0);
    up(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    assert_eq!(doc.get_hover_node_id(), None);
    // A non-primary lift clears it too.
    let ev = {
        t.buttons ^= MouseEventButtons::Primary;
        let mut ev = t.event(F2, 50.0, 50.0, 0.0);
        ev.is_primary = false;
        ev
    };
    drive(&mut doc, &mut clicks, UiEvent::PointerMove(ev.clone()));
    assert!(doc.get_hover_node_id().is_some());
    t.active.borrow_mut().clear();
    drive(&mut doc, &mut clicks, UiEvent::PointerUp(ev));
    assert_eq!(doc.get_hover_node_id(), None);
}

#[test]
fn the_mouse_still_clicks_the_release_target() {
    // Desktop behavior is unchanged: a mouse press released over another row
    // clicks the release target, and no touch press is recorded.
    let (mut doc, rows) = make_doc(true);
    let (mut clicks, mut t) = (Vec::new(), Touch::new());
    down(
        &mut doc,
        &mut clicks,
        &mut t,
        BlitzPointerId::Mouse,
        50.0,
        10.0,
    );
    assert_eq!(doc.touch_press, None);
    up(
        &mut doc,
        &mut clicks,
        &mut t,
        BlitzPointerId::Mouse,
        50.0,
        50.0,
    );
    assert_eq!(clicks, vec![rows[1]]);
}

#[test]
fn a_finger_lift_sends_pointerleave_to_the_pressed_row() {
    // The settings rail lights a row on pointerenter and darkens it on
    // pointerleave. A silent hover clear left every row a drag ended on lit.
    let (mut doc, rows) = make_doc(false);
    let (mut clicks, mut t) = (Vec::new(), Touch::new());
    down(&mut doc, &mut clicks, &mut t, F1, 50.0, 10.0);
    move_to(&mut doc, &mut clicks, &t, F1, 50.0, 50.0, 0.0);
    LEAVES.with(|l| l.borrow_mut().clear());
    up(&mut doc, &mut clicks, &mut t, F1, 50.0, 50.0);
    assert_eq!(doc.get_hover_node_id(), None);
    assert!(
        LEAVES.with(|l| l.borrow().contains(&rows[0])),
        "the lift sent no pointerleave to the row the finger pressed"
    );
}

/// The rows among `ids`, as indexes into `rows`.
fn row_indexes(rows: &[NodeId], ids: &[NodeId]) -> Vec<usize> {
    ids.iter()
        .filter_map(|id| rows.iter().position(|row| row == id))
        .collect()
}

/// Drags F1 from `(50, y0)` to `(50, y1)` in six moves, one layout pass after
/// each (the pass is where hover used to move silently), and returns the rows
/// that got `pointerenter` and `pointerleave` over the whole gesture.
fn drag_rows(doc: &mut BaseDocument, rows: &[NodeId], y0: f32, y1: f32) -> (Vec<usize>, Vec<usize>) {
    let (mut clicks, mut t) = (Vec::new(), Touch::new());
    ENTERS.with(|l| l.borrow_mut().clear());
    LEAVES.with(|l| l.borrow_mut().clear());
    down(doc, &mut clicks, &mut t, F1, 50.0, y0);
    doc.resolve(0.0);
    for i in 1..=6 {
        let y = y0 + (y1 - y0) * i as f32 / 6.0;
        move_to(doc, &mut clicks, &t, F1, 50.0, y, 0.0);
        doc.resolve(0.0);
    }
    up(doc, &mut clicks, &mut t, F1, 50.0, y1);
    doc.resolve(0.0);
    assert!(clicks.is_empty(), "the drag clicked {clicks:?}");
    assert_eq!(doc.get_hover_node_id(), None, "hover outlived the lift");
    (
        ENTERS.with(|l| row_indexes(rows, &l.borrow())),
        LEAVES.with(|l| row_indexes(rows, &l.borrow())),
    )
}

#[test]
fn a_finger_drag_that_cannot_scroll_enters_and_leaves_only_the_pressed_row() {
    // The emulator's drag over the settings rail: the finger crosses three
    // rows and none of them scrolls. Touch has no hover, so only the pressed
    // row is entered, and the lift leaves it.
    let (mut doc, rows) = make_doc(true);
    let (entered, left) = drag_rows(&mut doc, &rows, 90.0, 10.0);
    assert_eq!(entered, vec![2], "rows entered");
    assert_eq!(left, vec![2], "rows left");
}

#[test]
fn a_finger_drag_that_scrolls_enters_and_leaves_only_the_pressed_row() {
    // Rows scroll under the still finger between moves. The layout pass used
    // to re-resolve hover onto them with no boundary events, so the entered
    // row never saw its leave and stayed lit.
    let (mut doc, rows) = make_doc(false);
    let list = doc.nodes[rows[0]].parent.expect("the list");
    let (entered, left) = drag_rows(&mut doc, &rows, 90.0, 10.0);
    assert!(
        doc.nodes[list].scroll_offset().y > 40.0,
        "the drag did not scroll the list: {:?}",
        doc.nodes[list].scroll_offset()
    );
    assert_eq!(entered, vec![2], "rows entered");
    assert_eq!(left, vec![2], "rows left");
}

#[test]
fn mouse_hover_still_follows_the_pointer() {
    let (mut doc, rows) = make_doc(false);
    let (mut clicks, t) = (Vec::new(), Touch::new());
    let mouse = BlitzPointerId::Mouse;
    ENTERS.with(|l| l.borrow_mut().clear());
    LEAVES.with(|l| l.borrow_mut().clear());
    move_to(&mut doc, &mut clicks, &t, mouse, 50.0, 10.0, 0.0);
    move_to(&mut doc, &mut clicks, &t, mouse, 50.0, 50.0, 0.0);
    move_to(&mut doc, &mut clicks, &t, mouse, 50.0, 90.0, 0.0);
    assert_eq!(doc.get_hover_node_id(), Some(rows[2]));
    assert_eq!(ENTERS.with(|l| row_indexes(&rows, &l.borrow())), vec![0, 1, 2]);
    assert_eq!(LEAVES.with(|l| row_indexes(&rows, &l.borrow())), vec![0, 1]);
}
