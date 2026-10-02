use crate::BlitzShellProvider;
use crate::convert_events::{
    button_source_to_blitz, color_scheme_to_theme, pointer_kind_to_blitz, pointer_source_to_blitz,
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
use std::time::Duration;
use web_time::Instant;
use winit::event::{ButtonSource, ElementState, MouseButton};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Theme, WindowAttributes, WindowId};
use winit::{event::Modifiers, event::WindowEvent, keyboard::KeyCode, window::Window};

#[cfg(feature = "accessibility")]
use crate::accessibility::AccessibilityState;

// Ignore safe_area_insets on macOS because we don't want to avoid
// drawing in the titlebar.
#[cfg(target_os = "macos")]
fn get_safe_area_insets(_window: &dyn Window) -> PhysicalInsets<u32> {
    Default::default()
}
/// The insets the page is laid out inside: the safe area, or — with
/// `BLITZ_SAFE_AREA_SIDES=0` — only its top and bottom, the page drawn to
/// the left and right edges and minding the sides itself (a phone on its
/// side reports the camera housing's width on both sides, though only one
/// side has it, and only mid-height).
#[cfg(not(target_os = "macos"))]
fn get_safe_area_insets(window: &dyn Window) -> PhysicalInsets<u32> {
    static SIDES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let sides = *SIDES.get_or_init(|| {
        std::env::var("BLITZ_SAFE_AREA_SIDES").map_or(true, |v| v != "0")
    });
    let mut insets = window.safe_area();
    if !sides {
        insets.left = 0;
        insets.right = 0;
    }
    insets
}

pub struct WindowConfig<Rend: WindowRenderer> {
    doc: Box<dyn Document>,
    pub(crate) attributes: WindowAttributes,
    renderer: Rend,
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
        }
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
    /// FTS: redraw pacing for document updates (see `request_paced_redraw`).
    pacing: Pacing,
    /// FTS: the next frame must paint the page, not just its layers: the
    /// DOM changed, or an event may have changed what the page shows.
    paint_page: bool,
    pub is_visible: bool,
    pub safe_area_insets: PhysicalInsets<u32>,
    /// Frames left to re-read the safe area after a resize: on iOS a
    /// rotation resizes the surface before UIKit updates the insets, so
    /// the insets read at the resize are the old orientation's (a phone on
    /// its side kept its upright top and bottom bands). A few frames after
    /// it, the new ones have landed.
    inset_checks: u32,

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

impl<Rend: WindowRenderer> View<Rend> {
    pub fn init(
        config: WindowConfig<Rend>,
        event_loop: &dyn ActiveEventLoop,
        proxy: &BlitzShellProxy,
    ) -> Self {
        // We create window as invisble and then later make window visible
        // after AccessKit has initialised to avoid AccessKit panics
        let is_visible = config.attributes.visible;
        // Capture the requested surface size before consuming `attributes`, so we can
        // seed the viewport on platforms (winit-web) that report `surface_size() == 0×0`
        // until a layout pass fires.
        let requested_surface_size = config.attributes.surface_size;
        let attrs = config.attributes.with_visible(false);

        let winit_window: Arc<dyn Window> = Arc::from(event_loop.create_window(attrs).unwrap());
        #[cfg(feature = "accessibility")]
        let accessibility = AccessibilityState::new(&*winit_window, proxy.clone());

        if is_visible {
            winit_window.set_visible(true);
        }

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
        let shell_provider = BlitzShellProvider::new(winit_window.clone(), proxy.clone());

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
            inset_checks: 0,
            #[cfg(target_arch = "wasm32")]
            pending_resize: None,
            #[cfg(target_arch = "wasm32")]
            last_resize_at: None,
            #[cfg(target_arch = "wasm32")]
            resize_timer_scheduled: false,
            pointer_pos: Default::default(),
            pacing: Pacing::from_env(),
            paint_page: true,
            is_visible: winit_window.is_visible().unwrap_or(true),
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
        if !self.renderer.complete_resume() {
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
        // The painter offsets the scene in physical pixels (it draws at
        // `scale`), so the safe area goes in physical too, as it does
        // everywhere else here. Logical put the picture a third of the way
        // down a 3x phone's notch while input (`pointer_coords`) took off the
        // whole inset: every tap landed below what it pressed.
        let insets = self.safe_area_insets;

        #[cfg(feature = "custom-widget")]
        inner.can_create_surfaces(&mut self.renderer as _);

        // The render surface covers the entire window, including the safe area
        self.renderer.set_size(
            width + insets.left + insets.right,
            height + insets.top + insets.bottom,
        );

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

        self.waker = Some(create_waker(&self.proxy, window_id));
        true
    }

    pub fn suspend(&mut self) {
        self.waker = None;
        self.renderer.suspend();

        #[cfg(feature = "custom-widget")]
        self.doc.inner_mut().destroy_surfaces();
    }

    pub fn poll(&mut self) -> bool {
        let Some(waker) = self.waker.clone() else {
            return false;
        };
        // FTS: poll until the runtime is waiting again. A poll that finds
        // work runs it and returns without leaving a waker behind; only a
        // poll that comes back pending arms the next wake-up. Stopping after
        // one worked only while every redraw's window event polled again
        // (so the window never stopped redrawing); a window that goes quiet
        // would never hear its timers. Bounded, and continued through the
        // event loop, so a runtime that is always busy can't starve it.
        let mut worked = false;
        let mut rounds = 0;
        loop {
            let cx = std::task::Context::from_waker(&waker);
            if !self.doc.poll(Some(cx)) {
                break;
            }
            worked = true;
            rounds += 1;
            if rounds == 8 {
                let window_id = self.window.id();
                self.proxy.send_event(BlitzShellEvent::Poll { window_id });
                break;
            }
        }
        if !worked {
            return false;
        }

        #[cfg(feature = "accessibility")]
        {
            let inner = self.doc.inner();
            if inner.has_changes() {
                self.accessibility.update_tree(&inner);
            }
        }

        // FTS: a poll is often just a timer or a stream waking the runtime
        // and changing nothing; only a mutation or a widget with something
        // new to draw is worth a frame.
        let (mutated, widgets) = {
            let mut inner = self.doc.inner_mut();
            (inner.take_mutated(), inner.widgets_need_redraw())
        };
        self.paint_page |= mutated;
        let wanted = mutated | widgets;
        if wanted {
            self.request_paced_redraw();
        }
        true
    }

    /// A redraw for a document update, paced to `FTS_MAX_FPS` (30 unless
    /// set; 0 = unpaced). Clocks that each update the document at their own
    /// rate (meters, visualisers, lamps) would otherwise interleave into a
    /// redraw at every vsync, each one painting the whole window. Input
    /// still redraws at once: it asks the window directly.
    fn request_paced_redraw(&mut self) {
        let Some(min) = self.pacing.min_frame else {
            self.frame_now();
            return;
        };
        let since = self.pacing.last_frame.elapsed();
        if since >= min {
            self.frame_now();
            return;
        }
        self.send_timer_redraw(min - since);
    }

    /// FTS (iOS): the next frame of an animation, after the pace's wait
    /// (at least a few ms) and always through the event loop.
    #[cfg(target_os = "ios")]
    fn request_timed_redraw(&mut self) {
        let min = self.pacing.min_frame.unwrap_or(Duration::from_millis(16));
        let since = self.pacing.last_frame.elapsed();
        self.send_timer_redraw(min.saturating_sub(since).max(Duration::from_millis(4)));
    }

    /// A `RequestRedraw` sent after `wait` by the pacer thread; one at a time.
    fn send_timer_redraw(&mut self, wait: Duration) {
        if self.pacing.pending.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let doc_id = self.doc.id();
        let tx = self.pacing.timer.get_or_insert_with(|| {
            let (tx, rx) = std::sync::mpsc::channel::<(Duration, usize)>();
            let proxy = self.proxy.clone();
            std::thread::Builder::new()
                .name("blitz-redraw-pacer".into())
                .spawn(move || {
                    while let Ok((wait, doc_id)) = rx.recv() {
                        std::thread::sleep(wait);
                        proxy.send_event(BlitzShellEvent::RequestRedraw { doc_id });
                    }
                })
                .expect("spawn redraw pacer");
            tx
        });
        let _ = tx.send((wait, doc_id));
    }

    /// FTS: a frame for a document update or a paced tick — drawn now on
    /// iOS, asked for elsewhere. winit-uikit decides whether a view is
    /// Metal-backed by asking if the *view* is a `CAMetalLayer`, which a
    /// UIView never is, so every redraw it is asked for becomes
    /// `setNeedsDisplay` — and UIKit only turns that into a frame while it
    /// is handling a touch. A poll or a timer asking for one waited for the
    /// next touch: every tap showed the frame before it, and nothing that
    /// moves on its own (a meter, the tuner) moved.
    pub fn frame_now(&mut self) {
        #[cfg(target_os = "ios")]
        if self.renderer.is_active() {
            self.redraw();
            return;
        }
        self.request_redraw();
    }

    pub fn request_redraw(&self) {
        if self.renderer.is_active() {
            self.window.request_redraw();
            #[cfg(target_os = "ios")]
            self.ios_request_redraw.set(true);
        }
    }

    /// Read the safe area again, and when it has changed since the last
    /// resize, lay the page out in the new one: every frame (it is a
    /// field read), and for a few frames after a resize, asking for them.
    fn recheck_safe_area(&mut self) {
        let now = get_safe_area_insets(&*self.window);
        if now != self.safe_area_insets {
            self.safe_area_insets = now;
            let size = self.window.surface_size();
            let width = size.width.saturating_sub(now.left + now.right);
            let height = size.height.saturating_sub(now.top + now.bottom);
            self.with_viewport(|v| v.window_size = (width, height));
            self.request_redraw();
        }
        if self.inset_checks > 0 {
            self.inset_checks -= 1;
            self.request_redraw();
        }
    }

    pub fn redraw(&mut self) {
        let frame_started = std::time::Instant::now();
        self.pacing.last_frame = frame_started;
        self.pacing
            .pending
            .store(false, std::sync::atomic::Ordering::Release);
        #[cfg(target_os = "ios")]
        self.ios_request_redraw.set(false);
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
        let is_blocked = inner.has_pending_critical_resources();
        // Whether anything animates going in, for painting only the layers
        // (the redraw request below asks again, after painting).
        #[cfg(feature = "custom-widget")]
        let animating_before = inner.is_animating();
        // The painter offsets the scene in physical pixels (it draws at
        // `scale`), so the safe area goes in physical too, as it does
        // everywhere else here. Logical put the picture a third of the way
        // down a 3x phone's notch while input (`pointer_coords`) took off the
        // whole inset: every tap landed below what it pressed.
        let insets = self.safe_area_insets;

        if !is_blocked && is_visible {
            // FTS: paint and present, timed apart from `resolve`, which
            // has a phase timer of its own. Without this the two are one
            // number and a slow frame says nothing about which half.
            let started = std::time::Instant::now();
            let mut encoded = std::time::Duration::ZERO;
            // FTS: nothing changed but widgets drawn as layers: paint just
            // those, and let the renderer put them over the page it kept.
            #[cfg(feature = "custom-widget")]
            {
                let layers_only = anyrender::composite::supported()
                    && !self.paint_page
                    && !animating_before
                    && !inner.resolve_damaged()
                    && !inner.composite.unsettled.get()
                    && !inner.composite.placed.borrow().is_empty()
                    && !inner.page_widgets_need_redraw();
                let reuse = layers_only
                    && blitz_paint::paint_composited_widgets(&mut inner, &mut self.renderer, scale);
                let layers = if reuse { inner.composite.layers.borrow().clone() } else { Vec::new() };
                anyrender::composite::begin(reuse, layers);
            }
            self.paint_page = false;
            self.renderer.render(|scene| {
                let at = std::time::Instant::now();
                paint_scene(
                    scene,
                    &mut inner,
                    scale,
                    width,
                    height,
                    insets.left,
                    insets.top,
                );
                encoded = at.elapsed();
            });
            let whole = started.elapsed();
            // Recorded, not printed: a println on the render path costs
            // more than the frames it is measuring, and only says
            // anything to whoever is watching the terminal. The
            // application reads these beside `LAST_FRAME_MICROS` and can
            // draw them into the frame they describe.
            let micros = |d: std::time::Duration| u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
            blitz_traits::LAST_ENCODE_MICROS
                .store(micros(encoded), core::sync::atomic::Ordering::Relaxed);
            blitz_traits::LAST_PRESENT_MICROS.store(
                micros(whole.saturating_sub(encoded)),
                core::sync::atomic::Ordering::Relaxed,
            );
        }

        // FTS: asked after painting, not before. A widget's paint clears
        // its own "changed" and sets it again when it goes on moving (a
        // fling, a playhead): asked before, the answer is the last frame's.
        let is_animating = inner.is_animating();
        drop(inner);

        blitz_traits::LAST_FRAME_MICROS.store(
            u64::try_from(frame_started.elapsed().as_micros()).unwrap_or(u64::MAX),
            core::sync::atomic::Ordering::Relaxed,
        );
        #[cfg(feature = "custom-widget")]
        let (layers_only, layers) = (
            anyrender::composite::reused(),
            self.doc.inner().composite.placed.borrow().len(),
        );
        #[cfg(not(feature = "custom-widget"))]
        let (layers_only, layers) = (false, 0);
        self.pacing.log_frame(frame_started.elapsed(), layers_only, layers);

        // FTS: `FTS_FORCE_REDRAW=1` keeps asking for the next frame
        // whether or not the document thinks it is animating, which is
        // what makes a window measurable: a render loop that runs at the
        // machine's own pace, rather than one that only advances when
        // somebody moves a mouse over it.
        // FTS: the last paint changed which widgets are layers: once more,
        // so they are drawn as the new set says.
        #[cfg(feature = "custom-widget")]
        if self.doc.inner().composite.unsettled.get() {
            self.paint_page = true;
            #[cfg(target_os = "ios")]
            self.request_timed_redraw();
            #[cfg(not(target_os = "ios"))]
            self.request_paced_redraw();
        }

        let forced = std::env::var_os("FTS_FORCE_REDRAW").is_some();
        if !is_blocked && is_visible && (is_animating || forced) {
            // iOS: from inside a frame the next one comes back through the
            // event loop, always after a wait (`frame_now` draws at once): a
            // frame slower than the pace would otherwise draw the next inside
            // itself, and the run loop — touches with it — never got a turn.
            #[cfg(target_os = "ios")]
            self.request_timed_redraw();
            #[cfg(not(target_os = "ios"))]
            self.request_redraw();
        } else if !is_blocked && is_visible && self.doc.inner().widgets_need_redraw() {
            // FTS: a widget still moving (a face's spring settling, an LFO
            // lamp) asks for the next frame, at the paced rate. A still one
            // doesn't, and the window sleeps. (iOS: never inside this frame —
            // see the branch above.)
            #[cfg(target_os = "ios")]
            self.request_timed_redraw();
            #[cfg(not(target_os = "ios"))]
            self.request_paced_redraw();
        }
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
            let insets = self.safe_area_insets;
            self.renderer.set_size(
                width + insets.left + insets.right,
                height + insets.top + insets.bottom,
            );
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
        // Update accessibility focus and window size state in response to a Winit WindowEvent
        #[cfg(feature = "accessibility")]
        self.accessibility
            .process_window_event(&*self.window, &event);

        // FTS: an event may change what the page shows without the DOM
        // changing (a selection, a scroll, a focus ring): paint the page.
        // Moving the pointer with no button held only restyles (hover),
        // which `resolve` sees; dragging over a layer (a face's knob) only
        // changes the layer.
        match &event {
            WindowEvent::RedrawRequested | WindowEvent::Moved(_) | WindowEvent::ActivationTokenDone { .. } => {}
            WindowEvent::PointerMoved { .. } => {
                if self.buttons != MouseEventButtons::None {
                    #[cfg(feature = "custom-widget")]
                    let over_layer = {
                        let inner = self.doc.inner();
                        inner
                            .hovered_node_id()
                            .is_some_and(|id| inner.composite.placed.borrow().contains_key(&id))
                    };
                    #[cfg(not(feature = "custom-widget"))]
                    let over_layer = false;
                    self.paint_page |= !over_layer;
                }
            }
            _ => self.paint_page = true,
        }

        match event {
            WindowEvent::Destroyed => {}
            WindowEvent::ActivationTokenDone { .. } => {},
            WindowEvent::CloseRequested => {
                // Currently handled at the level above in application.rs
            }
            WindowEvent::RedrawRequested => {
                self.recheck_safe_area();
                self.redraw();
            }
            WindowEvent::Moved(_) => {}
            WindowEvent::Occluded(is_occluded) => {
                self.is_visible = !is_occluded;
                if self.is_visible {
                    self.request_redraw();
                }
            },
            WindowEvent::SurfaceResized(physical_size) => {
                self.safe_area_insets = get_safe_area_insets(&*self.window);
                // Half a second of frames to catch the insets UIKit sets
                // after the resize (see `inset_checks`).
                self.inset_checks = 30;
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
                    self.request_redraw();
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
                                KeyCode::KeyT => self.doc.inner().print_taffy_tree(),
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
                let button = match &button {
                    ButtonSource::Mouse(mouse_button) => match mouse_button {
                        MouseButton::Left => MouseEventButton::Main,
                        MouseButton::Right => MouseEventButton::Secondary,
                        MouseButton::Middle => MouseEventButton::Auxiliary,
                        // TODO: handle other button types
                        _ => MouseEventButton::Auxiliary,
                    }
                    _ => MouseEventButton::Main,
                };

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
                //
                // On a press, the move is the finger arriving, not dragging:
                // it carries no held button. With the button already held,
                // the document measured it from the *previous* press's
                // position, took any tap somewhere new for a pan, and sent
                // no click on release — every other tap was lost.
                if id != BlitzPointerId::Mouse {
                    let buttons = match state {
                        ElementState::Pressed => self.buttons ^ button.into(),
                        ElementState::Released => self.buttons,
                    };
                    let event = BlitzPointerEvent {
                        id,
                        is_primary: primary,
                        coords,
                        button: Default::default(),
                        buttons,
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
            WindowEvent::DragMoved { .. } => {},
            WindowEvent::DragDropped { .. } => {},
            WindowEvent::DragLeft { .. } => {},
        }
    }
}

/// FTS: when the window last drew, and the one deferred redraw queued for
/// a document update that came too soon after it.
struct Pacing {
    min_frame: Option<Duration>,
    last_frame: Instant,
    pending: Arc<std::sync::atomic::AtomicBool>,
    timer: Option<std::sync::mpsc::Sender<(Duration, usize)>>,
    /// `FTS_FPS_LOG=1`: frames and their mean and worst cost, each second.
    log: Option<FrameLog>,
}

struct FrameLog {
    since: Instant,
    frames: u32,
    /// Frames that only drew layers over the kept page.
    layers_only: u32,
    layers: usize,
    busy: Duration,
    worst: Duration,
}

impl Pacing {
    fn from_env() -> Self {
        let fps = std::env::var("FTS_MAX_FPS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(30);
        // No threads to pace with in a browser; it paces itself.
        let fps = if cfg!(target_arch = "wasm32") { 0 } else { fps };
        Self {
            min_frame: (fps > 0).then(|| Duration::from_secs_f64(1.0 / f64::from(fps))),
            last_frame: Instant::now(),
            pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            timer: None,
            log: std::env::var_os("FTS_FPS_LOG").map(|_| FrameLog {
                since: Instant::now(),
                frames: 0,
                layers_only: 0,
                layers: 0,
                busy: Duration::ZERO,
                worst: Duration::ZERO,
            }),
        }
    }

    fn log_frame(&mut self, took: Duration, layers_only: bool, layers: usize) {
        let Some(log) = self.log.as_mut() else { return };
        log.frames += 1;
        log.layers_only += u32::from(layers_only);
        log.layers = layers;
        log.busy += took;
        log.worst = log.worst.max(took);
        if log.since.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "fps {:>3} ({:>3} layers only, {} layers)  frame {:>5.2} ms mean  {:>5.2} ms worst",
                log.frames,
                log.layers_only,
                log.layers,
                log.busy.as_secs_f64() * 1000.0 / f64::from(log.frames.max(1)),
                log.worst.as_secs_f64() * 1000.0,
            );
            *log = FrameLog {
                since: Instant::now(),
                frames: 0,
                layers_only: 0,
                layers: 0,
                busy: Duration::ZERO,
                worst: Duration::ZERO,
            };
        }
    }
}
