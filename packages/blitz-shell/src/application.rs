use crate::event::{BlitzShellEvent, BlitzShellProxy};

use anyrender::WindowRenderer;
use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::WindowId;

#[cfg(target_os = "macos")]
use winit::platform::macos::ApplicationHandlerExtMacOS;

use crate::{View, WindowConfig};

thread_local! {
    static CLOSE_VETOES: std::cell::RefCell<Vec<WindowId>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Keep `window_id` open through the `CloseRequested` being dispatched now.
///
/// Call it from an embedder's window-event hook (dioxus-native's
/// `use_window_event`), which runs before [`BlitzApplication::window_event`]
/// on the same thread. The veto covers that one event: the shell takes it back
/// when it sees the close, so a later close drops the window as before. On
/// Windows an embedder can swallow `WM_CLOSE` in a subclass instead; Linux and
/// macOS have no such seam (polyvox-vrc R-OLM.4).
pub fn veto_close(window_id: WindowId) {
    CLOSE_VETOES.with(|v| {
        let mut v = v.borrow_mut();
        if !v.contains(&window_id) {
            v.push(window_id);
        }
    });
}

fn take_close_veto(window_id: WindowId) -> bool {
    CLOSE_VETOES.with(|v| {
        let mut v = v.borrow_mut();
        let before = v.len();
        v.retain(|w| *w != window_id);
        v.len() != before
    })
}

pub struct BlitzApplication<Rend: WindowRenderer> {
    pub windows: HashMap<WindowId, View<Rend>>,
    pub pending_windows: Vec<WindowConfig<Rend>>,
    pub proxy: BlitzShellProxy,
    pub event_queue: Receiver<BlitzShellEvent>,
}

impl<Rend: WindowRenderer> BlitzApplication<Rend> {
    pub fn new(proxy: BlitzShellProxy, event_queue: Receiver<BlitzShellEvent>) -> Self {
        BlitzApplication {
            windows: HashMap::new(),
            pending_windows: Vec::new(),
            proxy,
            event_queue,
        }
    }

    pub fn add_window(&mut self, window_config: WindowConfig<Rend>) {
        self.pending_windows.push(window_config);
    }

    fn window_mut_by_doc_id(&mut self, doc_id: usize) -> Option<&mut View<Rend>> {
        self.windows.values_mut().find(|w| w.doc.id() == doc_id)
    }

    pub fn handle_blitz_shell_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        event: BlitzShellEvent,
    ) {
        match event {
            BlitzShellEvent::Poll { window_id } => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.poll();
                };
            }
            BlitzShellEvent::CloseWindow { window_id } => {
                // Drop window before exiting event loop
                // See https://github.com/rust-windowing/winit/issues/4135
                let window = self.windows.remove(&window_id);
                drop(window);
                if self.windows.is_empty() {
                    event_loop.exit();
                }
            }
            BlitzShellEvent::ResumeReady { window_id } => {
                // The renderer fires `on_ready` after it has sent on the
                // channel, so `complete_resume` should always succeed here.
                // If a stale event survives a suspend, dropping it is safe.
                // A window released between its resume and this event
                // (polyvox-vrc R-OIS.6, `HideAndRelease`) answers false.
                if let Some(window) = self.windows.get_mut(&window_id) {
                    let _ = window.complete_resume();
                }
            }
            BlitzShellEvent::HideAndRelease { window_id } => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.hide_and_release();
                }
            }
            BlitzShellEvent::ShowAfterFrame { window_id } => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.show_after_frame();
                }
            }
            BlitzShellEvent::RequestRedraw { doc_id } => {
                // TODO: Handle multiple documents per window
                if let Some(window) = self.window_mut_by_doc_id(doc_id) {
                    // A resource finishing while a window awaits its reveal
                    // must paint now: a hidden window gets no WM_PAINT
                    // (polyvox-vrc R-OIM.3).
                    window.request_redraw_or_paint();
                }
            }

            #[cfg(feature = "accessibility")]
            BlitzShellEvent::Accessibility { window_id, data } => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    match &*data {
                        accesskit_xplat::WindowEvent::InitialTreeRequested => {
                            window.build_accessibility_tree();
                        }
                        accesskit_xplat::WindowEvent::AccessibilityDeactivated => {
                            // TODO
                        }
                        accesskit_xplat::WindowEvent::ActionRequested(_req) => {
                            // TODO
                        }
                    }
                }
            }
            BlitzShellEvent::Embedder(_) => {
                // Do nothing. Should be handled by embedders (if required).
            }
            BlitzShellEvent::Navigate(_opts) => {
                // Do nothing. Should be handled by embedders (if required).
            }
            BlitzShellEvent::NavigationLoad { .. } => {
                // Do nothing. Should be handled by embedders (if required).
            }
            #[cfg(target_arch = "wasm32")]
            BlitzShellEvent::ResizeSettleCheck { window_id } => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.apply_pending_resize_if_settled();
                }
            }
        }
    }
}

impl<Rend: WindowRenderer> ApplicationHandler for BlitzApplication<Rend> {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        // Resume existing windows
        for view in self.windows.values_mut() {
            view.resume();
        }

        // Initialise pending windows. The renderer's resume is non-blocking —
        // on native it finishes inline, on wasm32 it spawns a future that will
        // dispatch BlitzShellEvent::ResumeReady when init completes. Either way
        // we insert the view immediately so the event handler can find it.
        for window_config in self.pending_windows.drain(..) {
            let mut view = View::init(window_config, event_loop, &self.proxy);
            view.resume();
            self.windows.insert(view.window_id(), view);
        }
    }

    fn destroy_surfaces(&mut self, _event_loop: &dyn ActiveEventLoop) {
        for view in self.windows.values_mut() {
            view.suspend();
        }
    }

    fn resumed(&mut self, _event_loop: &dyn ActiveEventLoop) {
        // TODO
    }

    fn suspended(&mut self, _event_loop: &dyn ActiveEventLoop) {
        // TODO
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        // Exit the app when window close is requested.
        if matches!(event, WindowEvent::CloseRequested) {
            // An embedder hook that ran before this one kept the window
            // (polyvox-vrc R-OLM.4, `veto_close`).
            if take_close_veto(window_id) {
                return;
            }
            // Drop window before exiting event loop
            // See https://github.com/rust-windowing/winit/issues/4135
            let window = self.windows.remove(&window_id);
            drop(window);
            if self.windows.is_empty() {
                event_loop.exit();
            }
            return;
        }

        if let Some(window) = self.windows.get_mut(&window_id) {
            window.handle_winit_event(event);
        }
        self.proxy.send_event(BlitzShellEvent::Poll { window_id });
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        while let Ok(event) = self.event_queue.try_recv() {
            self.handle_blitz_shell_event(event_loop, event);
        }
    }

    #[cfg(target_os = "macos")]
    fn macos_handler(&mut self) -> Option<&mut dyn ApplicationHandlerExtMacOS> {
        Some(self)
    }

    #[cfg(target_os = "ios")]
    fn about_to_wait(&mut self, _event_loop: &dyn ActiveEventLoop) {
        for view in self.windows.values_mut() {
            if view.ios_request_redraw.get() {
                view.window.request_redraw();
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl<Rend: WindowRenderer> ApplicationHandlerExtMacOS for BlitzApplication<Rend> {
    fn standard_key_binding(
        &mut self,
        _event_loop: &dyn ActiveEventLoop,
        window_id: WindowId,
        action: &str,
    ) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.handle_apple_standard_keybinding(action);
            self.proxy.send_event(BlitzShellEvent::Poll { window_id });
        }
    }
}
