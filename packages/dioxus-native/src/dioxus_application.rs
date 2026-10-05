use blitz_shell::{BlitzApplication, BlitzShellProxy, View};
use dioxus_core::{ScopeId, provide_context};
use dioxus_history::{History, MemoryHistory};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::window::WindowId;

#[cfg(target_os = "macos")]
use winit::platform::macos::ApplicationHandlerExtMacOS;

use crate::DioxusNativeWindowRenderer;
use crate::event_handlers::WindowEventHandlers;
use crate::{BlitzShellEvent, DioxusDocument, WindowConfig, contexts::DioxusNativeDocument};

/// Dioxus-native specific event type
pub enum DioxusNativeEvent {
    /// A hotreload event, basically telling us to update our templates.
    #[cfg(all(feature = "hot-reload", debug_assertions))]
    DevserverEvent(dioxus_devtools::DevserverMsg),

    /// Create a new head element from the Link and Title elements
    ///
    /// todo(jon): these should probabkly be synchronous somehow
    CreateHeadElement {
        window: WindowId,
        name: String,
        attributes: Vec<(String, String)>,
        contents: Option<String>,
    },

    /// polyvox-vrc R-OIM.3: create every window queued by [`open_window`].
    /// A wake only: a `WindowConfig` owns a `VirtualDom`, which is not `Send`,
    /// so the config itself waits in the UI-thread [`RUNTIME_WINDOWS`] queue.
    CreateWindow,

    /// polyvox-vrc R-OIM.3: drop one window. Unlike a close request from the
    /// OS, this never exits the event loop, even when it removes the last
    /// window, so a short-lived picker or flyout cannot end the app.
    CloseWindow(WindowId),
}

thread_local! {
    /// Configs handed to [`open_window`], drained on the next `CreateWindow`.
    static RUNTIME_WINDOWS: RefCell<Vec<WindowConfig<DioxusNativeWindowRenderer>>> =
        const { RefCell::new(Vec::new()) };
    /// The event-loop proxy, stored by [`DioxusNativeApplication::new`] on the
    /// UI thread so [`open_window`] and [`close_window`] can wake the loop
    /// from any component or event handler without a context lookup.
    static RUNTIME_PROXY: RefCell<Option<BlitzShellProxy>> = const { RefCell::new(None) };
}

/// Open a window at runtime (polyvox-vrc R-OIM.3).
///
/// Call on the UI thread (a component, an effect, an event handler). The
/// window is created on the next turn of the event loop and gets the same
/// contexts as a boot window (`use_window()`, `document::*`, history), so its
/// root learns its own id with `use_window().id()`. Returns `false`, dropping
/// the config, when called off the UI thread or before the application exists.
pub fn open_window(config: WindowConfig<DioxusNativeWindowRenderer>) -> bool {
    let Some(proxy) = RUNTIME_PROXY.with(|p| p.borrow().clone()) else {
        return false;
    };
    RUNTIME_WINDOWS.with(|q| q.borrow_mut().push(config));
    proxy.send_event(BlitzShellEvent::embedder_event(
        DioxusNativeEvent::CreateWindow,
    ));
    true
}

/// Close a window opened at boot or by [`open_window`] (polyvox-vrc R-OIM.3).
///
/// The view is dropped on the next turn of the event loop; an unknown id is a
/// no-op. Never exits the app, even for the last window. Returns `false` when
/// called off the UI thread or before the application exists.
pub fn close_window(window_id: WindowId) -> bool {
    let Some(proxy) = RUNTIME_PROXY.with(|p| p.borrow().clone()) else {
        return false;
    };
    proxy.send_event(BlitzShellEvent::embedder_event(
        DioxusNativeEvent::CloseWindow(window_id),
    ));
    true
}

pub struct DioxusNativeApplication {
    pending_window: Option<WindowConfig<DioxusNativeWindowRenderer>>,
    inner: BlitzApplication<DioxusNativeWindowRenderer>,
    event_handlers: Rc<WindowEventHandlers>,
}

impl DioxusNativeApplication {
    pub fn new(
        proxy: BlitzShellProxy,
        event_queue: std::sync::mpsc::Receiver<BlitzShellEvent>,
        config: WindowConfig<DioxusNativeWindowRenderer>,
    ) -> Self {
        RUNTIME_PROXY.with(|p| *p.borrow_mut() = Some(proxy.clone()));
        Self {
            pending_window: Some(config),
            inner: BlitzApplication::new(proxy, event_queue),
            event_handlers: Rc::new(WindowEventHandlers::default()),
        }
    }

    pub fn add_window(&mut self, window_config: WindowConfig<DioxusNativeWindowRenderer>) {
        self.inner.add_window(window_config);
    }

    /// Build one window with the window-bound contexts every window gets, run
    /// its first build and insert it into `windows`, NOT yet resumed. Shared by
    /// the boot path and runtime `CreateWindow` (polyvox-vrc R-OIM.3).
    fn init_window(
        &mut self,
        config: WindowConfig<DioxusNativeWindowRenderer>,
        event_loop: &dyn ActiveEventLoop,
    ) -> WindowId {
        let mut window = View::init(config, event_loop, &self.inner.proxy);
        let winit_window = Arc::clone(&window.window);
        let renderer = window.renderer.clone();
        let window_id = window.window_id();
        let doc = window.downcast_doc_mut::<DioxusDocument>();

        doc.vdom.in_scope(ScopeId::ROOT, || {
            let shared: Rc<dyn dioxus_document::Document> = Rc::new(DioxusNativeDocument::new(
                self.inner.proxy.clone(),
                window_id,
            ));
            provide_context(shared);
            provide_context(self.event_handlers.clone());
        });

        // Add shell provider
        let shell_provider = doc.inner.borrow().shell_provider.clone();
        doc.vdom
            .in_scope(ScopeId::ROOT, move || provide_context(shell_provider));

        // Add history
        let history_provider: Rc<dyn History> = Rc::new(MemoryHistory::default());
        doc.vdom
            .in_scope(ScopeId::ROOT, move || provide_context(history_provider));

        // Add renderer
        doc.vdom
            .in_scope(ScopeId::ROOT, move || provide_context(renderer));

        // Add winit window
        doc.vdom
            .in_scope(ScopeId::ROOT, move || provide_context(winit_window));

        // Queue rebuild
        doc.initial_build();

        // And then request redraw
        window.request_redraw();

        // Inserted directly: the boot drain only resumes what is already in
        // `windows` (pending is empty by now); a runtime caller resumes it.
        self.inner.windows.insert(window_id, window);
        window_id
    }

    fn handle_dioxus_native_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        event: &DioxusNativeEvent,
    ) {
        match event {
            #[cfg(all(feature = "hot-reload", debug_assertions))]
            DioxusNativeEvent::DevserverEvent(event) => match event {
                dioxus_devtools::DevserverMsg::HotReload(hotreload_message) => {
                    for window in self.inner.windows.values_mut() {
                        let doc = window.downcast_doc_mut::<DioxusDocument>();

                        // Apply changes to vdom
                        dioxus_devtools::apply_changes(&doc.vdom, hotreload_message);

                        // Reload changed assets
                        for asset_path in &hotreload_message.assets {
                            if let Some(url) = asset_path.to_str() {
                                doc.inner.borrow_mut().reload_resource_by_href(url);
                            }
                        }

                        window.poll();
                    }
                }
                dioxus_devtools::DevserverMsg::Shutdown => event_loop.exit(),
                dioxus_devtools::DevserverMsg::FullReloadStart => {}
                dioxus_devtools::DevserverMsg::FullReloadFailed => {}
                dioxus_devtools::DevserverMsg::FullReloadCommand => {}
                _ => {}
            },

            DioxusNativeEvent::CreateHeadElement {
                name,
                attributes,
                contents,
                window,
            } => {
                if let Some(window) = self.inner.windows.get_mut(window) {
                    let doc = window.downcast_doc_mut::<DioxusDocument>();
                    doc.create_head_element(name, attributes, contents);
                    window.poll();
                }
            }

            DioxusNativeEvent::CreateWindow => {
                // Drain all of it: two opens before one wake share that wake.
                let configs = RUNTIME_WINDOWS.with(|q| std::mem::take(&mut *q.borrow_mut()));
                for config in configs {
                    let window_id = self.init_window(config, event_loop);
                    // Boot resumes every view in `BlitzApplication::can_create_surfaces`;
                    // a runtime window has no such pass, so resume it here or it
                    // never gets a surface and never paints.
                    if let Some(window) = self.inner.windows.get_mut(&window_id) {
                        window.resume();
                    }
                }
            }

            DioxusNativeEvent::CloseWindow(window_id) => {
                // Drop before anything else touches the loop, as blitz-shell's
                // own close path does (winit#4135). No exit-when-empty check.
                let window = self.inner.windows.remove(window_id);
                drop(window);
            }

            // Suppress unused variable warning
            #[cfg(not(all(feature = "hot-reload", debug_assertions)))]
            #[allow(unreachable_patterns)]
            _ => {
                let _ = event_loop;
                let _ = event;
            }
        }
    }
}

impl ApplicationHandler for DioxusNativeApplication {
    #[cfg(target_os = "macos")]
    fn macos_handler(&mut self) -> Option<&mut dyn ApplicationHandlerExtMacOS> {
        self.inner.macos_handler()
    }

    fn resumed(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.inner.resumed(event_loop);
    }

    fn suspended(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.inner.suspended(event_loop);
    }

    fn destroy_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.inner.destroy_surfaces(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.inner.about_to_wait(event_loop);
    }

    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        #[cfg(feature = "tracing")]
        tracing::debug!("Injecting document provider into all windows");

        // Every window this application creates gets the same window-bound
        // contexts: the primary handed to `new()` and every `add_window`
        // secondary still pending. Before this, only the primary was injected;
        // `BlitzApplication::can_create_surfaces` drained the secondaries bare,
        // so a secondary whose root used `use_window()`, `document::*` or the
        // router panicked on the missing context at its first render
        // (polyvox-vrc R-OCK.10). Secondaries are also built here, after
        // injection: a caller must not `initial_build` them first, since
        // `VirtualDom::rebuild` appends the root a second time.
        let mut configs: Vec<WindowConfig<DioxusNativeWindowRenderer>> = Vec::new();
        configs.extend(self.pending_window.take());
        configs.extend(self.inner.pending_windows.drain(..));

        for config in configs {
            self.init_window(config, event_loop);
        }

        self.inner.can_create_surfaces(event_loop);
    }

    fn new_events(&mut self, event_loop: &dyn ActiveEventLoop, cause: StartCause) {
        self.inner.new_events(event_loop, cause);
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        self.event_handlers
            .apply_event(window_id, &event, event_loop);
        self.inner.window_event(event_loop, window_id, event);
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        while let Ok(event) = self.inner.event_queue.try_recv() {
            match event {
                BlitzShellEvent::Embedder(event) => {
                    if let Some(event) = event.downcast_ref::<DioxusNativeEvent>() {
                        self.handle_dioxus_native_event(event_loop, event);
                    }
                }
                event => self.inner.handle_blitz_shell_event(event_loop, event),
            }
        }
    }
}
