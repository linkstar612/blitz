use crate::BlitzShellProvider;
use crate::convert_events::{
    button_source_to_blitz, button_to_mouse_event_button, color_scheme_to_theme,
    pointer_kind_to_blitz, pointer_source_to_blitz,
    pointer_source_to_blitz_details, theme_to_color_scheme, winit_ime_to_blitz,
    winit_key_event_to_blitz, winit_modifiers_to_kbt_modifiers,
};
use crate::event::{BlitzShellEvent, BlitzShellProxy, create_waker};
use anyrender::WindowRenderer;
use blitz_dom::Document;
use blitz_paint::paint_scene;
use blitz_traits::events::{
    BlitzPointerEvent, BlitzPointerId, BlitzWheelDelta, BlitzWheelEvent, MouseEventButton,
    MouseEventButtons, PointerCoords, PointerDetails, UiEvent,
};
use blitz_traits::shell::Viewport;
use winit::dpi::{LogicalPosition, PhysicalInsets, PhysicalPosition};
use winit::keyboard::PhysicalKey;

use atomic_refcell::AtomicRefCell;
use std::any::Any;
use std::sync::Arc;
use std::task::Waker;
use web_time::Instant;
use winit::event::ElementState;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Theme, WindowAttributes, WindowId};
use winit::{event::Modifiers, event::WindowEvent, keyboard::KeyCode, window::Window};

#[cfg(feature = "accessibility")]
use crate::accessibility::AccessibilityState;


/// rmboot8 TRACE (temporary): reveal-path trace to the file named by
/// `BLITZ_REVEAL_TRACE`, one line per call.
pub(crate) fn rtrace(msg: impl FnOnce() -> String) {
    use std::io::Write;
    static FILE: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = *START.get_or_init(std::time::Instant::now);
    let f = FILE.get_or_init(|| {
        let path = std::env::var_os("BLITZ_REVEAL_TRACE")?;
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(std::sync::Mutex::new)
    });
    if let Some(f) = f {
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "{:>10.3} {}", start.elapsed().as_secs_f64() * 1000.0, msg());
        }
    }
}

// Ignore safe_area_insets on macOS because we don't want to avoid
// drawing in the titlebar.
#[cfg(target_os = "macos")]
fn get_safe_area_insets(_window: &dyn Window) -> PhysicalInsets<u32> {
    Default::default()
}
#[cfg(not(target_os = "macos"))]
fn get_safe_area_insets(window: &dyn Window) -> PhysicalInsets<u32> {
    window.safe_area()
}

pub struct WindowConfig<Rend: WindowRenderer> {
    doc: Box<dyn Document>,
    pub(crate) attributes: WindowAttributes,
    renderer: Rend,
    defer_renderer: bool,
    paint_first: bool,
}

impl<Rend: WindowRenderer> WindowConfig<Rend> {
    pub fn new(doc: Box<dyn Document>, renderer: Rend) -> Self {
        Self::with_attributes(doc, renderer, WindowAttributes::default())
    }

    pub fn with_attributes(
        doc: Box<dyn Document>,
        renderer: Rend,
        attributes: WindowAttributes,
    ) -> Self {
        WindowConfig {
            doc,
            attributes,
            renderer,
            defer_renderer: false,
            paint_first: false,
        }
    }

    /// Build no renderer for this window until it is first shown through
    /// [`View::show_after_frame`] (polyvox-vrc R-OIS.6: a desktop overlay that
    /// is switched off holds no surface, atlas or scratch). The document is
    /// built and polled from the start, as a window released by
    /// [`View::hide_and_release`] is. Ask for a hidden window with it: a
    /// visible one would reveal nothing, since its first frame never comes.
    pub fn with_deferred_renderer(mut self, defer: bool) -> Self {
        self.defer_renderer = defer;
        self
    }

    /// Paint this window's changed document right after the next frame any
    /// other window paints, instead of waiting for its own `RedrawRequested`
    /// (polyvox-vrc R-OMQ.13). All windows share one event loop and Windows
    /// hands out paint messages in its own order: a desktop overlay waited
    /// behind both frames the main window painted for a new line (about 11 ms
    /// shown against 2.3 ms with the main window hidden), though the line was
    /// already in the first. The other window's first frame still goes first,
    /// and the window's own paint request stays as the fallback when no other
    /// window paints. What either window draws is unchanged; only the order of
    /// the frames is.
    pub fn with_paint_first(mut self, paint_first: bool) -> Self {
        self.paint_first = paint_first;
        self
    }
}

/// polyvox-vrc R-OMQ.13: how long after a paint-first window painted its owed
/// frame early its own paint message counts as the repeat of that frame. The
/// message trails the other window's frames by a few milliseconds; a later one
/// is not that message and always paints.
const OWED_PAINT_SKIP_WINDOW: std::time::Duration = std::time::Duration::from_millis(100);

/// polyvox-vrc R-OMQ.1: an animation's next frame (smooth scroll, the overlay
/// scrollbar fade, a CSS animation, a custom widget asking for frames) waits
/// for the display's next refresh instead of being asked for the moment the
/// last one presented. The renderer's present does not block on vsync here,
/// so a scroll animation presented about 590 frames a second on a 360 Hz
/// display: work on a GPU VRChat shares, for frames nobody sees. Frames an
/// event or a document change asks for are not paced and paint at once.
///
/// `POLYVOX_FRAME_PACING=0` turns it off (A/B and diagnosis only).
fn frame_pacing_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("POLYVOX_FRAME_PACING").map_or(true, |v| v != "0" && v != "off")
    })
}

/// How often a window re-reads its monitor's refresh rate (it can move to
/// another monitor, or the mode can change).
const FRAME_INTERVAL_REFRESH: std::time::Duration = std::time::Duration::from_secs(1);

/// A deferred animation frame due within this much of now is asked for now:
/// a timer wake that short costs more than it saves.
const ANIMATION_FRAME_SLACK: std::time::Duration = std::time::Duration::from_micros(250);

/// The display refresh period for a refresh rate in millihertz, or `None` for
/// a rate outside 20 to 1000 Hz (unknown or nonsense: no pacing).
pub(crate) fn refresh_period(millihertz: u32) -> Option<std::time::Duration> {
    if !(20_000..=1_000_000).contains(&millihertz) {
        return None;
    }
    Some(std::time::Duration::from_secs_f64(1000.0 / f64::from(millihertz)))
}

/// When an animating window's next frame should be asked for: `None` for now
/// (no known refresh period, or a frame period has already passed since the
/// last frame started), else the instant one display period after
/// `frame_start`.
pub(crate) fn next_animation_frame(
    frame_start: Instant,
    now: Instant,
    period: Option<std::time::Duration>,
) -> Option<Instant> {
    let due = frame_start + period?;
    (due > now + ANIMATION_FRAME_SLACK).then_some(due)
}

/// What the next animation frame is timed from: the due time of the deferred
/// frame this one answers, when it started within a period of it, so the timer
/// wake's own delay does not stretch every interval past the display period
/// (a 360 Hz display drew about 330 frames a second when timed from each
/// frame's start); else the frame's start.
pub(crate) fn pacing_anchor(
    frame_start: Instant,
    fired_due: Option<Instant>,
    period: Option<std::time::Duration>,
) -> Instant {
    match (fired_due, period) {
        (Some(due), Some(period))
            if frame_start >= due && frame_start.duration_since(due) < period =>
        {
            due
        }
        _ => frame_start,
    }
}

pub struct View<Rend: WindowRenderer> {
    pub doc: Box<dyn Document>,

    pub renderer: Rend,
    pub waker: Option<Waker>,

    pub proxy: BlitzShellProxy,
    pub window: Arc<dyn Window>,

    /// The state of the keyboard modifiers (ctrl, shift, etc). Winit/Tao don't track these for us so we
    /// need to store them in order to have access to them when processing keypress events
    pub theme_override: Option<Theme>,
    pub keyboard_modifiers: Modifiers,
    pub buttons: MouseEventButtons,
    pub pointer_pos: PhysicalPosition<f64>,
    /// The non-mouse pointers (touch/pen) that are currently pressed, in the
    /// order they were pressed.
    ///
    /// This serves two purposes:
    /// - Multi-touch: it is cloned (cheaply, via [`Arc`]) into every dispatched
    ///   [`BlitzPointerEvent`] so that touch events can report all concurrent
    ///   touches via their `touches` list.
    /// - Cancellation detection: winit signals a cancelled touch with a
    ///   [`WindowEvent::PointerLeft`] that is *not* preceded by a
    ///   [`WindowEvent::PointerButton`] with [`ElementState::Released`]. If a
    ///   pointer is still in this list when it leaves, it was cancelled.
    ///
    /// The events stored here always have an empty `active_pointers` list to
    /// avoid a reference cycle.
    pub active_events: Arc<AtomicRefCell<Vec<BlitzPointerEvent>>>,
    pub animation_timer: Option<Instant>,
    pub is_visible: bool,
    pub safe_area_insets: PhysicalInsets<u32>,

    /// The embedder asked for a visible window and it has not been shown yet.
    /// The window is created hidden and revealed by `reveal_after_frame` once
    /// a frame painted with no critical resource pending is on its surface, so
    /// the first thing on screen is the laid-out document: never the OS window
    /// background, never a configured-but-unpresented swapchain.
    reveal_pending: bool,
    /// The document has reported a change from `poll` at least once. Before
    /// that a dioxus document is an empty `<html>`: `complete_resume` paints
    /// it, and revealing on that frame shows a black window for as long as
    /// the first real render takes (measured at about 500 ms).
    doc_polled: bool,
    /// The first `resume` installs the poll waker and builds no renderer
    /// ([`WindowConfig::with_deferred_renderer`]); taken once.
    defer_first_resume: bool,
    /// The viewport size changed since the renderer's surface was last sized.
    /// `set_size` reconfigures the swapchain and a reconfigured swapchain shows
    /// nothing until the next present, so it is applied right before `render`
    /// instead of inside the resize event that produced it.
    surface_size_dirty: bool,
    /// [`WindowConfig::with_paint_first`].
    paint_first: bool,
    /// A paint-first window polled a change it has not painted yet.
    paint_owed: bool,
    /// When a paint-first window painted its owed frame ahead of its own
    /// paint message (`paint_owed_now`), with nothing left to draw. The
    /// paint message the poll asked for is then a copy of that frame, and
    /// the next `RedrawRequested` within [`OWED_PAINT_SKIP_WINDOW`] skips it
    /// unless anything asked for a frame since.
    owed_paint_done_at: Option<Instant>,
    /// Set by every frame request, this view's and the document's
    /// (`BlitzShellProvider`): a paint message that carries one of those is
    /// never skipped.
    redraw_asked: Arc<std::sync::atomic::AtomicBool>,
    /// The last `redraw` presented a frame and asked for no next one.
    last_frame_settled: bool,
    /// polyvox-vrc R-OMQ.1: an animation frame waiting for the display's next
    /// refresh ([`next_animation_frame`]); the event loop asks for it in
    /// `about_to_wait` once due. Any frame that paints first clears it.
    anim_frame_due: Option<Instant>,
    /// The due time of the deferred animation frame last asked for, which the
    /// next `redraw` times its successor from ([`pacing_anchor`]).
    anim_frame_fired: Option<Instant>,
    /// This window's display refresh period and when it was read.
    frame_period: Option<(Instant, Option<std::time::Duration>)>,

    #[cfg(target_arch = "wasm32")]
    pending_resize: Option<winit::dpi::PhysicalSize<u32>>,
    #[cfg(target_arch = "wasm32")]
    last_resize_at: Option<web_time::Instant>,
    /// True iff a setTimeout has been scheduled and not yet observed by
    /// `apply_pending_resize_if_settled`. Prevents the timer storm that would
    /// otherwise allocate a fresh `Closure` per resize event during a drag.
    #[cfg(target_arch = "wasm32")]
    resize_timer_scheduled: bool,

    #[cfg(feature = "accessibility")]
    /// Accessibility adapter for `accesskit`.
    pub accessibility: AccessibilityState,

    // Calling request_redraw within a WindowEvent doesn't work on iOS. So on iOS we track the state
    // with a boolean and call request_redraw in about_to_wait
    //
    // See https://github.com/rust-windowing/winit/issues/3406
    #[cfg(target_os = "ios")]
    pub ios_request_redraw: std::cell::Cell<bool>,
}

impl<Rend: WindowRenderer> Drop for View<Rend> {
    fn drop(&mut self) {
        // Release the renderer's window surface before the window is dropped.
        // The renderer may be shared (e.g. provided as a context to user code),
        // in which case it can outlive the `View`. A GPU surface must not
        // outlive the window/display it is attached to: dropping it after the
        // event loop has shut down segfaults on Wayland.
        self.renderer.suspend();
    }
}

impl<Rend: WindowRenderer> View<Rend> {
    pub fn init(
        config: WindowConfig<Rend>,
        event_loop: &dyn ActiveEventLoop,
        proxy: &BlitzShellProxy,
    ) -> Self {
        // The window is created invisible. It used to be shown right after
        // AccessKit initialized (a hidden window avoids AccessKit panics); it
        // now stays hidden until the first frame has been painted, which is
        // later still, so that ordering is kept. See `reveal_after_frame`.
        let reveal_pending = config.attributes.visible;
        // Capture the requested surface size before consuming `attributes`, so we can
        // seed the viewport on platforms (winit-web) that report `surface_size() == 0×0`
        // until a layout pass fires.
        let requested_surface_size = config.attributes.surface_size;
        let attrs = config.attributes.with_visible(false);

        let winit_window: Arc<dyn Window> = Arc::from(event_loop.create_window(attrs).unwrap());
        #[cfg(feature = "accessibility")]
        let accessibility = AccessibilityState::new(&*winit_window, proxy.clone());

        // Create viewport
        let scale = winit_window.scale_factor() as f32;
        let mut size = winit_window.surface_size();
        if (size.width == 0 || size.height == 0)
            && let Some(requested) = requested_surface_size
        {
            size = requested.to_physical(scale as f64);
        }
        // On wasm, when the embedder didn't call `with_surface_size`, winit-web's
        // initial `surface_size()` is 0×0 — its ResizeObserver hasn't fired yet.
        // Resuming the renderer at 0×0 trips a wgpu swapchain-size-0 error, so
        // seed from the canvas element's CSS layout box (host-stylesheet result).
        #[cfg(target_arch = "wasm32")]
        if size.width == 0 || size.height == 0 {
            use winit::platform::web::WindowExtWeb;
            if let Some(canvas) = winit_window.canvas() {
                let css_w = canvas.offset_width().max(0) as u32;
                let css_h = canvas.offset_height().max(0) as u32;
                if css_w > 0 && css_h > 0 {
                    size = winit::dpi::LogicalSize::new(css_w, css_h).to_physical(scale as f64);
                }
            }
        }
        let safe_area_insets = get_safe_area_insets(&*winit_window);
        let theme = winit_window.theme().unwrap_or(Theme::Light);
        let color_scheme = theme_to_color_scheme(theme);
        // The viewport is the window surface minus the safe area insets
        let viewport_width = size
            .width
            .saturating_sub(safe_area_insets.left + safe_area_insets.right);
        let viewport_height = size
            .height
            .saturating_sub(safe_area_insets.top + safe_area_insets.bottom);
        let viewport = Viewport::new(viewport_width, viewport_height, scale, color_scheme);

        // Create shell provider
        let redraw_asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shell_provider = BlitzShellProvider::with_redraw_flag(
            winit_window.clone(),
            proxy.clone(),
            Arc::clone(&redraw_asked),
        );

        let defer_first_resume = config.defer_renderer;
        let mut doc = config.doc;
        let mut inner = doc.inner_mut();
        inner.set_viewport(viewport);
        inner.set_shell_provider(Arc::new(shell_provider));

        // If the document title is set prior to the window being created then it will
        // have been sent to a dummy ShellProvider and won't get picked up.
        // So we look for it here and set it if present.
        let title = inner.find_title_node().map(|node| node.text_content());
        if let Some(title) = title {
            winit_window.set_title(&title);
        }

        drop(inner);

        Self {
            renderer: config.renderer,
            waker: None,
            animation_timer: None,
            keyboard_modifiers: Default::default(),
            proxy: proxy.clone(),
            window: winit_window.clone(),
            doc,
            theme_override: None,
            buttons: MouseEventButtons::None,
            active_events: Arc::new(AtomicRefCell::new(Vec::new())),
            safe_area_insets,
            #[cfg(target_arch = "wasm32")]
            pending_resize: None,
            #[cfg(target_arch = "wasm32")]
            last_resize_at: None,
            #[cfg(target_arch = "wasm32")]
            resize_timer_scheduled: false,
            pointer_pos: Default::default(),
            // A window awaiting its reveal paints while hidden: that first
            // frame is what the reveal waits for. Windows never reports
            // `Occluded`, so nothing else would ever flip this to true.
            is_visible: reveal_pending || winit_window.is_visible().unwrap_or(true),
            reveal_pending,
            doc_polled: false,
            defer_first_resume,
            surface_size_dirty: false,
            paint_first: config.paint_first,
            paint_owed: false,
            owed_paint_done_at: None,
            redraw_asked,
            last_frame_settled: false,
            anim_frame_due: None,
            anim_frame_fired: None,
            frame_period: None,
            #[cfg(feature = "accessibility")]
            accessibility,

            #[cfg(target_os = "ios")]
            ios_request_redraw: std::cell::Cell::new(false),
        }
    }

    pub fn replace_document(&mut self, new_doc: Box<dyn Document>, retain_scroll_position: bool) {
        let inner = self.doc.inner();
        let scroll = inner.viewport_scroll();
        let viewport = inner.viewport().clone();
        let shell_provider = inner.shell_provider.clone();
        drop(inner);

        self.doc = new_doc;

        let mut inner = self.doc.inner_mut();
        inner.set_viewport(viewport);
        inner.set_shell_provider(shell_provider);
        drop(inner);

        self.poll();
        self.request_redraw();

        if retain_scroll_position {
            self.doc.inner_mut().set_viewport_scroll(scroll);
        }
    }

    pub fn theme_override(&self) -> Option<Theme> {
        self.theme_override
    }

    pub fn current_theme(&self) -> Theme {
        color_scheme_to_theme(self.doc.inner().viewport().color_scheme)
    }

    pub fn set_theme_override(&mut self, theme: Option<Theme>) {
        self.theme_override = theme;
        let theme = theme.or(self.window.theme()).unwrap_or(Theme::Light);
        self.with_viewport(|v| v.color_scheme = theme_to_color_scheme(theme));
    }

    pub fn downcast_doc_mut<T: 'static>(&mut self) -> &mut T {
        (&mut *self.doc as &mut dyn Any)
            .downcast_mut::<T>()
            .unwrap()
    }

    pub fn current_animation_time(&mut self) -> f64 {
        match &self.animation_timer {
            Some(start) => Instant::now().duration_since(*start).as_secs_f64(),
            None => {
                self.animation_timer = Some(Instant::now());
                0.0
            }
        }
    }
}

impl<Rend: WindowRenderer> View<Rend> {
    /// Start resuming the renderer. Dispatches [`BlitzShellEvent::ResumeReady`]
    /// when initialization completes — synchronously on native, asynchronously
    /// on wasm32. The embedder must call [`complete_resume`](Self::complete_resume)
    /// in response.
    pub fn resume(&mut self) {
        let window_id = self.window_id();
        rtrace(|| format!("resume {window_id:?} defer={} waker={}", self.defer_first_resume, self.waker.is_some()));
        // A deferred window (polyvox-vrc R-OIS.6) starts out like a released
        // one: its document polls, its renderer waits for `show_after_frame`.
        if std::mem::take(&mut self.defer_first_resume) {
            if self.waker.is_none() {
                self.waker = Some(create_waker(&self.proxy, window_id));
            }
            return;
        }
        let animation_time = self.current_animation_time();

        let (width, height) = {
            let mut inner = self.doc.inner_mut();
            inner.resolve(animation_time);
            inner.viewport().window_size
        };

        // The render surface covers the entire window, including the safe area
        let insets = self.safe_area_insets;
        let width = width + insets.left + insets.right;
        let height = height + insets.top + insets.bottom;

        let proxy = self.proxy.clone();
        self.renderer
            .resume(Arc::new(self.window.clone()), width, height, move || {
                proxy.send_event(BlitzShellEvent::ResumeReady { window_id });
            });
    }

    /// Finalize a previously-started resume. Should be called in response to a
    /// [`BlitzShellEvent::ResumeReady`] event. Paints the first frame and
    /// installs the doc poll waker. Returns `true` if the renderer is now active.
    pub fn complete_resume(&mut self) -> bool {
        let pre_active = self.renderer.is_active();
        let pre_pending = self.renderer.is_pending();
        rtrace(|| format!("complete_resume-enter {:?} pre_active={pre_active} pre_pending={pre_pending} doc_polled={} waker={}", self.window_id(), self.doc_polled, self.waker.is_some()));
        // polyvox-vrc R-OMQ.13: a renderer wake (a frame retried after the
        // GPU was busy, a readback shown, a rebuild) may post the paint
        // message a skip would eat. Ahead of the check: a retry answers false.
        self.owed_paint_done_at = None;
        if !self.renderer.complete_resume() {
            rtrace(|| format!("complete_resume {:?} -> false pre_active={pre_active}", self.window_id()));
            return false;
        }

        let window_id = self.window_id();

        // Resync the renderer to the current viewport. Resize/scale events that
        // arrived while the renderer was Pending were no-ops on the renderer
        // (its `set_size` only matches Active), so the surface created during
        // resume could be at a stale size by the time we get here.
        let animation_time = self.current_animation_time();
        let mut inner = self.doc.inner_mut();
        inner.resolve(animation_time);
        let (width, height) = inner.viewport().window_size;
        let scale = inner.viewport().scale_f64();
        let insets = self.safe_area_insets;

        #[cfg(feature = "custom-widget")]
        inner.can_create_surfaces(&mut self.renderer as _);

        // The render surface covers the entire window, including the safe area
        self.renderer.set_size(
            width + insets.left + insets.right,
            height + insets.top + insets.bottom,
        );
        self.surface_size_dirty = false;
        let is_blocked = inner.has_pending_critical_resources();

        self.renderer.render(|scene| {
            paint_scene(
                scene,
                &mut inner,
                scale,
                width,
                height,
                insets.left,
                insets.top,
            )
        });
        drop(inner);

        rtrace(|| format!("complete_resume {window_id:?} painted pre_active={pre_active} blocked={is_blocked} doc_polled={} reveal_pending={} vis={} had_waker={}", self.doc_polled, self.reveal_pending, self.is_visible, self.waker.is_some()));
        self.waker = Some(create_waker(&self.proxy, window_id));
        // A frame painted with a stylesheet or font still loading is not the
        // document; leave the reveal to the first unblocked `redraw`.
        if !is_blocked {
            self.reveal_after_frame();
        }
        true
    }

    /// Record that the document already holds its first render (polyvox-vrc
    /// R-OIM.3). An embedder that built the document synchronously before
    /// `resume` (dioxus-native's runtime `CreateWindow`) calls this so the
    /// frame `complete_resume` paints can reveal the window. Without it the
    /// reveal waits for a `poll` that finds work, and a document with no
    /// dirty scope after its first build (effects run without marking one)
    /// never reports any: the window stayed hidden forever.
    pub fn mark_document_built(&mut self) {
        self.doc_polled = true;
    }

    /// `request_redraw`, except that a window still awaiting its reveal is
    /// painted now (polyvox-vrc R-OIM.3). A hidden window gets no WM_PAINT on
    /// Windows, so a redraw requested for it never arrives; a frame first
    /// blocked on a critical resource would then never be followed by the
    /// unblocked one the reveal waits for.
    pub fn request_redraw_or_paint(&mut self) {
        rtrace(|| format!("request_redraw_or_paint {:?} reveal_pending={} active={}", self.window.id(), self.reveal_pending, self.renderer.is_active()));
        if self.reveal_pending && self.renderer.is_active() {
            self.redraw();
        } else {
            self.request_redraw();
        }
    }

    /// Show a window that was created hidden, now that a frame is on its
    /// surface. The renderer waits for the GPU before returning from `render`,
    /// so the swapchain already holds that frame when the window appears.
    /// A no-op once the window has been shown, and a wait while the document
    /// has not been polled yet (that frame is an empty document).
    fn reveal_after_frame(&mut self) {
        rtrace(|| format!("reveal_after_frame {:?} reveal_pending={} doc_polled={}", self.window.id(), self.reveal_pending, self.doc_polled));
        if !self.reveal_pending || !self.doc_polled {
            return;
        }
        self.reveal_pending = false;
        self.window.set_visible(true);
        rtrace(|| format!("revealed {:?} os_visible={:?}", self.window.id(), self.window.is_visible()));
        self.is_visible = true;
    }

    /// Hide the window and drop its renderer's GPU state: the surface, the
    /// swapchain, the atlases and the scratch buffers of a window nobody can
    /// see (polyvox-vrc R-OIS.6, a shell closed to the tray). The document,
    /// its layout and the winit window stay, and so does the poll waker: the
    /// document keeps running its tasks while hidden. Hidden through winit, so
    /// winit's visibility flag agrees with the OS. Brought back by
    /// [`show_after_frame`](Self::show_after_frame).
    pub fn hide_and_release(&mut self) {
        rtrace(|| format!("hide_and_release {:?}", self.window.id()));
        self.reveal_pending = false;
        self.is_visible = false;
        self.window.set_visible(false);
        self.renderer.suspend();

        #[cfg(feature = "custom-widget")]
        self.doc.inner_mut().destroy_surfaces();
    }

    /// Show a window hidden by [`hide_and_release`](Self::hide_and_release)
    /// (polyvox-vrc R-OIS.6). The renderer is rebuilt and the window stays
    /// hidden until a frame is on its surface, the same reveal a new window
    /// gets, so the first frame on screen is the document, never an empty or
    /// stale swapchain. Also safe for a window hidden some other way: its
    /// renderer is still active, so it paints and shows at once.
    pub fn show_after_frame(&mut self) {
        rtrace(|| format!("show_after_frame {:?}", self.window.id()));
        // A window hidden behind winit's back (a raw `ShowWindow`) still has
        // the visible flag, and the reveal's `set_visible(true)` would then
        // be a no-op. Clearing it first makes that reveal a real show; a
        // window already on screen keeps its flag, or it would blink.
        if self.window.is_visible() == Some(false) {
            self.window.set_visible(false);
        }
        self.reveal_pending = true;
        self.is_visible = true;
        if self.renderer.is_active() {
            self.redraw();
        } else if !self.renderer.is_pending() {
            // `complete_resume`, on the `ResumeReady` this sends, paints the
            // first frame and reveals the window off it.
            self.resume();
        }
    }

    pub fn suspend(&mut self) {
        self.waker = None;
        self.renderer.suspend();

        #[cfg(feature = "custom-widget")]
        self.doc.inner_mut().destroy_surfaces();
    }

    pub fn poll(&mut self) -> bool {
        if self.waker.is_none() {
            rtrace(|| format!("poll {:?} no-waker", self.window_id()));
        }
        if let Some(waker) = &self.waker {
            let cx = std::task::Context::from_waker(waker);
            let got = self.doc.poll(Some(cx));
            rtrace(|| format!("poll {:?} got={got} reveal_pending={} active={}", self.window.id(), self.reveal_pending, self.renderer.is_active()));
            if got {
                self.doc_polled = true;
                #[cfg(feature = "accessibility")]
                {
                    let inner = self.doc.inner();
                    if inner.has_changes() {
                        self.accessibility.update_tree(&inner);
                    }
                }

                if self.reveal_pending {
                    // A hidden window gets no WM_PAINT on Windows, so a
                    // requested redraw would never arrive. Paint now: this is
                    // the first frame with the document in it, and the
                    // reveal happens off it.
                    self.redraw();
                } else {
                    // polyvox-vrc R-OMQ.13: owed to the next frame of another
                    // window (`take_paint_owed`), with this request as the
                    // fallback when no other window paints.
                    if self.paint_first {
                        self.paint_owed = true;
                    }
                    self.request_redraw();
                }
                return true;
            }
        }

        false
    }

    /// Whether this window was asked to paint first
    /// ([`WindowConfig::with_paint_first`]).
    pub fn paints_first(&self) -> bool {
        self.paint_first
    }

    /// Whether a paint-first window has a changed document it has not painted
    /// yet, and can paint it now (shown, with a live renderer). Clears the
    /// flag; the caller paints with [`redraw`](Self::redraw).
    pub fn take_paint_owed(&mut self) -> bool {
        let owed = std::mem::take(&mut self.paint_owed);
        owed && self.is_visible && self.renderer.is_active()
    }

    /// Paint a paint-first window's owed frame now, ahead of the paint
    /// message its poll asked for (polyvox-vrc R-OMQ.13). When the frame
    /// presented with nothing left to draw, that message only repeats it, so
    /// the next `RedrawRequested` skips it unless anything asked for a frame
    /// in between: the document, an event, or the renderer retrying a frame
    /// the GPU was too busy for (its wake comes through `complete_resume`).
    pub fn paint_owed_now(&mut self) {
        // The poll's own request is the one this frame answers. One made
        // while it paints (a widget wanting another frame) stays set.
        self.redraw_asked
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.redraw();
        if self.last_frame_settled {
            self.owed_paint_done_at = Some(Instant::now());
        }
    }

    pub fn request_redraw(&self) {
        if self.renderer.is_active() {
            self.redraw_asked
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.window.request_redraw();
            #[cfg(target_os = "ios")]
            self.ios_request_redraw.set(true);
        }
    }

    pub fn redraw(&mut self) {
        #[cfg(target_os = "ios")]
        self.ios_request_redraw.set(false);
        self.paint_owed = false;
        let frame_start = Instant::now();
        self.anim_frame_due = None;
        let fired_due = self.anim_frame_fired.take();
        let animation_time = self.current_animation_time();
        let is_visible = self.is_visible;

        let mut inner = self.doc.inner_mut();
        inner.resolve(animation_time);

        // Unregister resources (e.g. textures) from dropped custom widget nodes
        #[cfg(feature = "custom-widget")]
        for id in inner.take_pending_resource_deallocations() {
            self.renderer.unregister_resource(id);
        }

        let (width, height) = inner.viewport().window_size;
        let scale = inner.viewport().scale_f64();
        let is_animating = inner.is_animating();
        let is_blocked = inner.has_pending_critical_resources();
        let insets = self.safe_area_insets;
        let painted = !is_blocked && is_visible && self.renderer.is_active();
        rtrace(|| format!("redraw {:?} painted={painted} blocked={is_blocked} vis={is_visible} active={} reveal_pending={} doc_polled={}", self.window.id(), self.renderer.is_active(), self.reveal_pending, self.doc_polled));

        if !is_blocked && is_visible {
            // The document is laid out at the new size by now (`resolve`
            // above), so the surface can take that size and be painted in
            // one go: no blank frame between the two. Field access, not a
            // helper: `inner` still borrows `self.doc` here.
            if self.surface_size_dirty {
                self.surface_size_dirty = false;
                let sa = self.safe_area_insets;
                self.renderer
                    .set_size(width + sa.left + sa.right, height + sa.top + sa.bottom);
            }
            self.renderer.render(|scene| {
                paint_scene(
                    scene,
                    &mut inner,
                    scale,
                    width,
                    height,
                    insets.left,
                    insets.top,
                )
            });
        }

        drop(inner);

        if painted {
            self.reveal_after_frame();
        }

        self.last_frame_settled = painted && !is_animating && !self.surface_size_dirty;
        if !is_blocked && is_visible && is_animating {
            self.request_animation_frame(frame_start, fired_due);
        }
    }

    /// The display refresh period of the monitor this window is on, re-read
    /// at most once a second.
    fn display_period(&mut self) -> Option<std::time::Duration> {
        let now = Instant::now();
        if let Some((read_at, period)) = self.frame_period
            && now.duration_since(read_at) < FRAME_INTERVAL_REFRESH
        {
            return period;
        }
        let period = self
            .window
            .current_monitor()
            .and_then(|m| m.current_video_mode())
            .and_then(|mode| mode.refresh_rate_millihertz())
            .and_then(|mhz| refresh_period(mhz.get()));
        self.frame_period = Some((now, period));
        period
    }

    /// Ask for an animation's next frame, one display period after the frame
    /// that started at `frame_start` (polyvox-vrc R-OMQ.1).
    fn request_animation_frame(&mut self, frame_start: Instant, fired_due: Option<Instant>) {
        let period = if frame_pacing_enabled() {
            self.display_period()
        } else {
            None
        };
        let anchor = pacing_anchor(frame_start, fired_due, period);
        match next_animation_frame(anchor, Instant::now(), period) {
            None => self.request_redraw(),
            Some(due) => self.anim_frame_due = Some(due),
        }
    }

    /// Ask for a deferred animation frame that is due by `now`. Returns when
    /// the one still waiting is due, for the event loop's wake.
    pub fn fire_due_animation_frame(&mut self, now: Instant) -> Option<Instant> {
        let due = self.anim_frame_due?;
        if due > now + ANIMATION_FRAME_SLACK {
            return Some(due);
        }
        self.anim_frame_due = None;
        self.anim_frame_fired = Some(due);
        self.request_redraw();
        None
    }

    pub fn pointer_coords(&self, position: PhysicalPosition<f64>) -> PointerCoords {
        let inner = self.doc.inner();
        let scale = inner.viewport().scale_f64();
        let LogicalPosition::<f32> {
            x: screen_x,
            y: screen_y,
        } = position.to_logical(scale);
        let viewport_scroll_offset = inner.viewport_scroll();
        let client_x = screen_x - (self.safe_area_insets.left as f64 / scale) as f32;
        let client_y = screen_y - (self.safe_area_insets.top as f64 / scale) as f32;
        let page_x = client_x + viewport_scroll_offset.x as f32;
        let page_y = client_y + viewport_scroll_offset.y as f32;

        PointerCoords {
            screen_x,
            screen_y,
            client_x,
            client_y,
            page_x,
            page_y,
        }
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    /// Store `event` as an active pointer, replacing any existing entry with the
    /// same id. The stored event has an empty `active_pointers` list to avoid a
    /// reference cycle.
    fn set_active_pointer(&self, event: &BlitzPointerEvent) {
        let mut stored = event.clone();
        stored.active_pointers = Default::default();

        let mut active = self.active_events.borrow_mut();
        if let Some(existing) = active.iter_mut().find(|e| e.id == stored.id) {
            *existing = stored;
        } else {
            active.push(stored);
        }
    }

    /// Update the stored position/state of an already-active pointer. Does
    /// nothing if the pointer is not currently active (e.g. a hovering pen).
    fn update_active_pointer(&self, event: &BlitzPointerEvent) {
        let mut active = self.active_events.borrow_mut();
        if let Some(existing) = active.iter_mut().find(|e| e.id == event.id) {
            let mut stored = event.clone();
            stored.active_pointers = Default::default();
            *existing = stored;
        }
    }

    /// Remove an active pointer by id. Returns `true` if it was present.
    fn remove_active_pointer(&self, id: BlitzPointerId) -> bool {
        let mut active = self.active_events.borrow_mut();
        let len_before = active.len();
        active.retain(|e| e.id != id);
        active.len() != len_before
    }

    #[inline]
    pub fn with_viewport(&mut self, cb: impl FnOnce(&mut Viewport)) {
        let mut inner = self.doc.inner_mut();
        let mut viewport = inner.viewport_mut();
        cb(&mut viewport);
        let (width, height) = viewport.window_size;
        drop(viewport);
        drop(inner);
        if width > 0 && height > 0 {
            // Not `renderer.set_size` here: that reconfigures the swapchain,
            // and between the reconfigure and the next present the window
            // shows nothing (black on DXGI flip model). `redraw` applies the
            // size right before it renders, so the surface is never resized
            // without a frame following it in the same call.
            self.surface_size_dirty = true;
            self.request_redraw();
        }
    }

    #[cfg(feature = "accessibility")]
    pub fn build_accessibility_tree(&mut self) {
        let inner = self.doc.inner();
        self.accessibility.update_tree(&inner);
    }

    #[cfg(target_arch = "wasm32")]
    const RESIZE_DEBOUNCE_MS: u32 = 100;

    #[cfg(target_arch = "wasm32")]
    fn schedule_resize_settle_check(&mut self, delay_ms: u32) {
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;

        let proxy = self.proxy.clone();
        let window_id = self.window_id();
        let cb = Closure::once_into_js(move || {
            proxy.send_event(BlitzShellEvent::ResizeSettleCheck { window_id });
        });
        if let Some(win) = web_sys::window() {
            let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.unchecked_ref(),
                delay_ms as i32,
            );
            self.resize_timer_scheduled = true;
        }
    }

    /// Applies the pending resize iff motion has been quiet for the debounce
    /// window; otherwise re-arms the timer for the remaining time. Called
    /// when a previously scheduled timer fires.
    #[cfg(target_arch = "wasm32")]
    pub fn apply_pending_resize_if_settled(&mut self) {
        self.resize_timer_scheduled = false;
        let Some(last) = self.last_resize_at else {
            return;
        };
        let debounce = std::time::Duration::from_millis(Self::RESIZE_DEBOUNCE_MS as u64);
        let elapsed = web_time::Instant::now().saturating_duration_since(last);
        if elapsed < debounce {
            // Motion ongoing — wait out the rest of the window before re-checking.
            let remaining_ms = (debounce - elapsed).as_millis() as u32;
            self.schedule_resize_settle_check(remaining_ms);
            return;
        }
        let Some(size) = self.pending_resize.take() else {
            return;
        };
        self.last_resize_at = None;

        let insets = self.safe_area_insets;
        let width = size.width.saturating_sub(insets.left + insets.right);
        let height = size.height.saturating_sub(insets.top + insets.bottom);
        self.with_viewport(|v| v.window_size = (width, height));
        self.request_redraw();
    }

    #[cfg(target_os = "macos")]
    pub fn handle_apple_standard_keybinding(&mut self, command: &str) {
        use blitz_traits::SmolStr;
        let event = UiEvent::AppleStandardKeybinding(SmolStr::new(command));
        self.doc.handle_ui_event(event);
    }

    pub fn handle_winit_event(&mut self, event: WindowEvent) {
        rtrace(|| { let d = format!("{event:?}"); format!("ev {:?} os_visible={:?} {}", self.window.id(), self.window.is_visible(), d.chars().take(90).collect::<String>()) });
        // Update accessibility focus and window size state in response to a Winit WindowEvent
        #[cfg(feature = "accessibility")]
        self.accessibility
            .process_window_event(&*self.window, &event);

        match event {
            WindowEvent::Destroyed => {}
            WindowEvent::ActivationTokenDone { .. } => {},
            WindowEvent::CloseRequested => {
                // Currently handled at the level above in application.rs
            }
            WindowEvent::RedrawRequested => {
                rtrace(|| format!("RedrawRequested {:?}", self.window.id()));
                // polyvox-vrc R-OMQ.13: the paint message after an owed frame
                // that already went out (`paint_owed_now`).
                let done_at = self.owed_paint_done_at.take();
                let asked = self
                    .redraw_asked
                    .swap(false, std::sync::atomic::Ordering::Relaxed);
                let repeat = done_at.is_some_and(|at| at.elapsed() < OWED_PAINT_SKIP_WINDOW)
                    && !asked
                    && !self.paint_owed
                    && !self.surface_size_dirty
                    && !self.reveal_pending;
                if !repeat {
                    self.redraw();
                }
            }
            WindowEvent::Moved(_) => {}
            WindowEvent::Occluded(is_occluded) => {
                rtrace(|| format!("Occluded {:?} {is_occluded}", self.window.id()));
                self.is_visible = !is_occluded;
                if self.is_visible {
                    self.request_redraw();
                }
            },
            WindowEvent::SurfaceResized(physical_size) => {
                self.safe_area_insets = get_safe_area_insets(&*self.window);
                // On WASM, defer the apply: wgpu's surface.configure clears the canvas,
                // so running it every frame flickers during a drag. The browser stretches
                // the stale backing store until the debounce timer settles.
                #[cfg(target_arch = "wasm32")]
                {
                    self.pending_resize = Some(physical_size);
                    self.last_resize_at = Some(web_time::Instant::now());
                    if !self.resize_timer_scheduled {
                        self.schedule_resize_settle_check(Self::RESIZE_DEBOUNCE_MS);
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let insets = self.safe_area_insets;
                    let width = physical_size.width - insets.left - insets.right;
                    let height = physical_size.height - insets.top - insets.bottom;
                    self.with_viewport(|v| v.window_size = (width, height));
                    // Relayout and present inside the resize event, not on a
                    // later RedrawRequested: the compositor shows the window at
                    // its new size as soon as this handler returns, and the
                    // frame it shows should be the one laid out for that size.
                    self.redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.with_viewport(|v| v.set_hidpi_scale(scale_factor as f32));
                self.request_redraw();
            }
            WindowEvent::ThemeChanged(theme) => {
                let color_scheme = theme_to_color_scheme(self.theme_override.unwrap_or(theme));
                let mut inner = self.doc.inner_mut();
                inner.viewport_mut().color_scheme = color_scheme;
            }
            WindowEvent::Ime(ime_event) => {
                self.doc.handle_ui_event(UiEvent::Ime(winit_ime_to_blitz(ime_event)));
                self.request_redraw();
            },
            WindowEvent::ModifiersChanged(new_state) => {
                // Store new keyboard modifier (ctrl, shift, etc) state for later use
                self.keyboard_modifiers = new_state;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(key_code) = event.physical_key && event.state.is_pressed() {
                        let ctrl = self.keyboard_modifiers.state().control_key();
                        let meta = self.keyboard_modifiers.state().meta_key();
                        let alt = self.keyboard_modifiers.state().alt_key();

                        // Ctrl/Super keyboard shortcuts
                        if ctrl | meta {
                            match key_code {
                                KeyCode::Equal => {
                                    self.doc.inner_mut().viewport_mut().zoom_by(0.1);
                                },
                                KeyCode::Minus => {
                                    self.doc.inner_mut().viewport_mut().zoom_by(-0.1);
                                },
                                KeyCode::Digit0 => {
                                    self.doc.inner_mut().viewport_mut().set_zoom(1.0);
                                }
                                _ => {}
                            };
                        }

                        // Alt keyboard shortcuts
                        if alt {
                            match key_code {
                                KeyCode::KeyD => {
                                    let mut inner = self.doc.inner_mut();
                                    inner.devtools_mut().toggle_show_layout();
                                    drop(inner);
                                    self.request_redraw();
                                }
                                KeyCode::KeyH => {
                                    let mut inner = self.doc.inner_mut();
                                    inner.devtools_mut().toggle_highlight_hover();
                                    drop(inner);
                                    self.request_redraw();
                                }
                                KeyCode::KeyT => self.doc.inner_mut().print_taffy_tree(),
                                _ => {}
                            };
                        }

                }

                // Unmodified keypresses
                let key_event_data = winit_key_event_to_blitz(&event, self.keyboard_modifiers.state());
                let event = if event.state.is_pressed() {
                    UiEvent::KeyDown(key_event_data)
                } else {
                    UiEvent::KeyUp(key_event_data)
                };

                self.doc.handle_ui_event(event);
            }
            WindowEvent::PointerEntered { /*device_id*/.. } => {}
            WindowEvent::PointerLeft { position, primary, kind, .. } => {
                let id = pointer_kind_to_blitz(&kind);

                // A `PointerLeft` for a non-mouse pointer that is still pressed
                // (i.e. we never saw a `PointerButton` with `Released` for it)
                // means the system cancelled tracking of this touch/pen. Emit a
                // pointercancel in that case. A mouse simply leaving the window,
                // or a touch that was already released, is not a cancellation.
                // Remove from the active list first so the cancelled pointer is
                // excluded from this event's `touches`. `remove_active_pointer`
                // reports whether the pointer was actually active.
                if id != BlitzPointerId::Mouse && self.remove_active_pointer(id) {
                    let position = position.unwrap_or(self.pointer_pos);
                    self.pointer_pos = position;

                    // The pointer is no longer pressed.
                    self.buttons ^= MouseEventButton::Main.into();

                    let event = BlitzPointerEvent {
                        id,
                        is_primary: primary,
                        coords: self.pointer_coords(position),
                        button: MouseEventButton::Main,
                        buttons: self.buttons,
                        mods: winit_modifiers_to_kbt_modifiers(self.keyboard_modifiers.state()),
                        details: PointerDetails::default(),
                        element: Default::default(),
                        active_pointers: Arc::clone(&self.active_events),
                    };

                    self.doc.handle_ui_event(UiEvent::PointerCancel(event));
                    self.request_redraw();
                } else if id == BlitzPointerId::Mouse {
                    // polyvox R-OJL.2: a mouse leaving the window ends hover,
                    // as it does in a browser. Blitz only re-resolved hover on
                    // a move, so the last hovered element kept `:hover` and
                    // never saw `mouseleave` until the pointer came back.
                    let position = position.unwrap_or(self.pointer_pos);
                    let event = BlitzPointerEvent {
                        id,
                        is_primary: primary,
                        coords: self.pointer_coords(position),
                        button: Default::default(),
                        buttons: self.buttons,
                        mods: winit_modifiers_to_kbt_modifiers(self.keyboard_modifiers.state()),
                        details: PointerDetails::default(),
                        element: Default::default(),
                        active_pointers: Arc::clone(&self.active_events),
                    };
                    self.doc.handle_ui_event(UiEvent::PointerLeave(event));
                    self.request_redraw();
                }
            }
            WindowEvent::PointerMoved { position, source, primary, .. } => {
                self.pointer_pos = position;
                let id = pointer_source_to_blitz(&source);
                let event = BlitzPointerEvent {
                    id,
                    is_primary: primary,
                    coords: self.pointer_coords(position),
                    button: Default::default(),
                    buttons: self.buttons,
                    mods: winit_modifiers_to_kbt_modifiers(self.keyboard_modifiers.state()),
                    details: pointer_source_to_blitz_details(&source),
                    element: Default::default(),
                    active_pointers: Arc::clone(&self.active_events),
                };
                // Keep multi-touch positions current (no-op for non-active pointers).
                if id != BlitzPointerId::Mouse {
                    self.update_active_pointer(&event);
                }
                self.doc.handle_ui_event(UiEvent::PointerMove(event));
            }
            WindowEvent::PointerButton { button, state, primary, position, .. } => {
                let id = button_source_to_blitz(&button);
                let coords = self.pointer_coords(position);
                self.pointer_pos = position;
                let button = button_to_mouse_event_button(&button);

                match state {
                    ElementState::Pressed => self.buttons |= button.into(),
                    ElementState::Released => self.buttons ^= button.into(),
                }

                let pointer_event = BlitzPointerEvent {
                    id,
                    is_primary: primary,
                    coords,
                    button,
                    buttons: self.buttons,
                    mods: winit_modifiers_to_kbt_modifiers(self.keyboard_modifiers.state()),

                    // TODO: details for pointer up/down events
                    details: PointerDetails::default(),
                    element: Default::default(),
                    active_pointers: Arc::clone(&self.active_events),
                };

                // Maintain the list of active (pressed) non-mouse pointers. A
                // press adds the pointer *before* dispatch (so touchstart's
                // `touches` includes it). A release is handled after the
                // synthetic move below so the move still sees it, but before the
                // pointerup so touchend's `touches` excludes it.
                if id != BlitzPointerId::Mouse && state == ElementState::Pressed {
                    self.set_active_pointer(&pointer_event);
                }

                // Touch input doesn't emit a `PointerMoved` before the button
                // event the way a mouse does, so synthesise a move to update the
                // hover/hit position to the touch location.
                if id != BlitzPointerId::Mouse {
                    let event = BlitzPointerEvent {
                        id,
                        is_primary: primary,
                        coords,
                        button: Default::default(),
                        buttons: self.buttons,
                        mods: winit_modifiers_to_kbt_modifiers(self.keyboard_modifiers.state()),
                        details: PointerDetails::default(),
                        element: Default::default(),
                        active_pointers: Arc::clone(&self.active_events),
                    };
                    self.doc.handle_ui_event(UiEvent::PointerMove(event));
                }

                if id != BlitzPointerId::Mouse && state == ElementState::Released {
                    self.remove_active_pointer(id);
                }

                let event = pointer_event;

                let event = match state {
                    ElementState::Pressed => UiEvent::PointerDown(event),
                    ElementState::Released => UiEvent::PointerUp(event),
                };

                self.doc.handle_ui_event(event);
                self.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let blitz_delta = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => BlitzWheelDelta::Lines(x as f64, y as f64),
                    winit::event::MouseScrollDelta::PixelDelta(pos) => BlitzWheelDelta::Pixels(pos.x, pos.y),
                    _ =>  {
                        #[cfg(feature = "tracing")]
                        tracing::error!("unsupported MouseScrollDelta variant: {delta:?}");
                        BlitzWheelDelta::Pixels(0., 0.)
                    }
                };

                let event = BlitzWheelEvent {
                    delta: blitz_delta,
                    coords: self.pointer_coords(self.pointer_pos),
                    buttons: self.buttons,
                    mods: winit_modifiers_to_kbt_modifiers(self.keyboard_modifiers.state()),
                    element: Default::default()
                };

                self.doc.handle_ui_event(UiEvent::Wheel(event));
            }
            WindowEvent::Focused(_) => {}
            WindowEvent::TouchpadPressure { .. } => {}
            WindowEvent::PinchGesture { .. } => {},
            WindowEvent::PanGesture { .. } => {},
            WindowEvent::DoubleTapGesture { .. } => {},
            WindowEvent::RotationGesture { .. } => {},
            WindowEvent::DragEntered { .. } => {},
            WindowEvent::DragDropped { .. } => {},
            WindowEvent::DragLeft { .. } => {},
            _ => {}
        }
    }
}

#[cfg(test)]
mod frame_pacing_tests {
    use super::{next_animation_frame, pacing_anchor, refresh_period};
    use std::time::Duration;
    use web_time::Instant;

    #[test]
    fn refresh_period_follows_the_rate_and_drops_nonsense() {
        let p360 = refresh_period(360_000).unwrap();
        assert!((p360.as_secs_f64() - 1.0 / 360.0).abs() < 1e-9);
        let p60 = refresh_period(59_940).unwrap();
        assert!((p60.as_secs_f64() * 1000.0 - 16.683).abs() < 0.01);
        assert_eq!(refresh_period(0), None);
        assert_eq!(refresh_period(19_999), None);
        assert_eq!(refresh_period(1_000_001), None);
    }

    #[test]
    fn an_animation_frame_waits_one_display_period_after_the_last_start() {
        let start = Instant::now();
        let period = Some(Duration::from_micros(2778));
        // Asked right after a 0.4 ms frame: due one period after its start.
        let now = start + Duration::from_micros(400);
        assert_eq!(
            next_animation_frame(start, now, period),
            Some(start + Duration::from_micros(2778))
        );
    }

    #[test]
    fn a_frame_late_past_its_period_or_with_no_known_rate_is_asked_for_now() {
        let start = Instant::now();
        let period = Some(Duration::from_micros(2778));
        assert_eq!(
            next_animation_frame(start, start + Duration::from_millis(3), period),
            None
        );
        // Within the slack of the due time: now, not a timer wake.
        assert_eq!(
            next_animation_frame(start, start + Duration::from_micros(2600), period),
            None
        );
        assert_eq!(next_animation_frame(start, start, None), None);
    }

    #[test]
    fn a_paced_frame_times_its_successor_from_its_due_time_not_its_late_start() {
        let due = Instant::now();
        let period = Some(Duration::from_micros(2778));
        // The timer woke 0.3 ms late: the next frame is still due one period
        // after the last due time, so the cadence holds the display rate.
        let late_start = due + Duration::from_micros(300);
        assert_eq!(pacing_anchor(late_start, Some(due), period), due);
        // A frame that started a full period or more after it (the loop was
        // busy), or one no deferred frame asked for, times from its start.
        let much_later = due + Duration::from_millis(5);
        assert_eq!(pacing_anchor(much_later, Some(due), period), much_later);
        assert_eq!(pacing_anchor(late_start, None, period), late_start);
        assert_eq!(pacing_anchor(late_start, Some(due), None), late_start);
    }
}
