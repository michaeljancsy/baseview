#![expect(deprecated, reason = "Allow use of NSFilenamesPboardType for now")]

use super::keyboard::{make_modifiers, KeyboardState};
use super::window::WindowSharedState;
use crate::dpi::{LogicalPosition, LogicalSize};
use crate::host::Host;
use crate::platform::macos::cursor::CursorManager;
use crate::platform::macos::timer::TimerManager;
use crate::platform::*;
use crate::tracing::warn;
use crate::utils::SizingStrategy;
use crate::window::WindowInitializer;
use crate::wrappers::appkit::*;
use crate::MouseEvent::{ButtonPressed, ButtonReleased};
use crate::{
    DamageArea, DropData, DropEffect, Event, EventStatus, HandlerError, MouseButton, MouseEvent,
    ScrollDelta, WindowEvent, WindowHandler, WindowSize,
};
use objc2::__framework_prelude::Retained;
use objc2::rc::Weak;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{msg_send, AllocAnyThread, ClassType, MainThreadMarker, Message};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingInfo, NSEvent, NSFilenamesPboardType, NSTrackingArea,
    NSTrackingAreaOptions, NSView, NSWindow, NSWindowDidChangeOcclusionStateNotification,
    NSWindowOcclusionState,
};
use objc2_foundation::{NSArray, NSNotification, NSPoint, NSPointInRect, NSRect, NSSize, NSString};
use objc2_quartz_core::CADisplayLink;
use std::cell::{Cell, OnceCell, RefCell};
use std::ptr::null;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub enum ViewParentingType {
    Parented { parent_view: Weak<NSView> },
    Windowed { owned_window: Weak<NSWindow> },
    Uninitialized,
}

impl ViewParentingType {
    fn setup(&self, child: &View<BaseviewView>) {
        match self {
            ViewParentingType::Parented { parent_view } => {
                if let Some(parent_view) = parent_view.load() {
                    parent_view.addSubview(child);
                }
            }
            ViewParentingType::Windowed { owned_window, .. } => {
                if let Some(owned_window) = owned_window.load() {
                    owned_window.setContentView(Some(child));
                    set_delegate(&owned_window, child);
                }
            }
            _ => {}
        }
    }

    fn teardown(self) {
        if let ViewParentingType::Windowed { owned_window: parent_window } = self {
            if let Some(parent_window) = parent_window.load() {
                parent_window.close();
            }
        }
    }
}

pub(crate) struct BaseviewView {
    pub(crate) state: Rc<WindowSharedState>,
    pub(crate) mtm: MainThreadMarker,
    window_handler: WindowHandlerContainer,

    notification_center_observer: Cell<Option<NotificationCenterObserver>>,

    keyboard_state: KeyboardState,

    parenting: RefCell<ViewParentingType>,
    pub(crate) lifetime_tied_to_app: Cell<Option<Weak<NSApplication>>>,
    display_link: OnceCell<Retained<CADisplayLink>>,
    display_link_started: Cell<bool>,
    /// Whether the view's window is at least partly visible, as macOS last said. While it isn't,
    /// the display link stays paused and no frames are drawn, after one last frame
    /// (ConjurePluginKit request 66).
    window_visible: Cell<bool>,
    /// Whether the one last frame after the window became hidden has been drawn. While the window
    /// is hidden and this is true, no more frames are drawn. The last frame lets the handler finish
    /// what a frame finishes (the kit's window closes a scroll-wheel change open with the host
    /// there) before frames stop (ConjurePluginKit request 66).
    hidden_frame_drawn: Cell<bool>,
    /// That last frame is on its way through the main queue.
    hidden_frame_queued: Cell<bool>,
    /// For the frame counts (ConjurePluginKit requests 66 and 69): frames drawn, display-link
    /// firings skipped because the window was hidden, and time spent drawing, since `since`; and
    /// when the window last became visible, until its first frame after that.
    frame_log: Cell<(Option<Instant>, u32, u32, Duration)>,
    shown_at: Cell<Option<Instant>>,
    /// Display-link firings since the last frame-count line.
    link_firings: Cell<u32>,

    host: Host,
    pub(crate) cursor_manager: CursorManager,

    timers: TimerManager,

    #[cfg(feature = "opengl")]
    pub(crate) gl_context: OnceCell<super::gl::PlatformGlContext>,
}

impl BaseviewView {
    pub fn new(
        init: WindowInitializer, parenting: ViewParentingType, final_size: LogicalSize<f64>,
        mtm: MainThreadMarker,
    ) -> Result<(Retained<View<Self>>, Rc<WindowSharedState>)> {
        let view_rect =
            NSRect::new(NSPoint::ZERO, NSSize::new(final_size.width, final_size.height));

        let state = Rc::new(WindowSharedState::new(
            final_size,
            1.0,
            SizingStrategy::from_settings(&init.settings),
        ));

        let inner = BaseviewView {
            mtm,
            state: Rc::clone(&state),

            keyboard_state: KeyboardState::new(),
            display_link: OnceCell::new(),
            display_link_started: false.into(),
            window_visible: true.into(),
            hidden_frame_drawn: false.into(),
            hidden_frame_queued: false.into(),
            frame_log: Cell::new((None, 0, 0, Duration::ZERO)),
            shown_at: None.into(),
            link_firings: 0.into(),
            window_handler: WindowHandlerContainer::new(),
            notification_center_observer: None.into(),
            parenting: ViewParentingType::Uninitialized.into(),
            host: init.host,
            lifetime_tied_to_app: None.into(),
            cursor_manager: CursorManager::new(),

            timers: TimerManager::new(),

            #[cfg(feature = "opengl")]
            gl_context: OnceCell::new(),
        };

        let view = View::new(view_rect, inner, |view| {
            // Set up parenting before handler setup
            parenting.setup(view.view);
            view.parenting.replace(parenting);

            view.state.scale_factor.set(view.view.backing_scale_factor());
            view.state.size.set(view.view.size());

            Self::apply_size_constraints(view);

            #[cfg(feature = "opengl")]
            if let Some(gl_config) = init.settings.gl_config {
                let gl_context =
                    super::gl::PlatformGlContext::create(view.view, gl_config, view.mtm)?;
                let Ok(()) = view.gl_context.set(gl_context) else { unreachable!() };
            }

            let context = WindowContext::new(view);
            let handler = init.builder.build(crate::WindowContext::new(context))?;

            // Initialize handler
            view.window_handler.set(handler);

            // Set up anything that might trigger events to the handler

            // SAFETY: This static is a read-only constant
            let ns_filenames_pboard_type = unsafe { NSFilenamesPboardType };
            view.view.registerForDraggedTypes(&NSArray::from_slice(&[ns_filenames_pboard_type]));

            let display_link = view.view.setup_display_link();
            display_link.setPaused(true);
            let Ok(()) = view.display_link.set(display_link) else { unreachable!() };

            let notifier_view = Weak::new(view.view);
            let observer = NotificationCenterObserver::register_window_key_change(move |n| {
                if let Some(view) = notifier_view.load() {
                    if let Some(view) = view.inner_ref() {
                        BaseviewView::handle_notification(view, n);
                    }
                }
            });
            view.notification_center_observer.set(Some(observer));

            Ok(())
        })?;

        Ok((view, state))
    }

    pub fn show(this: ViewRef<Self>) {
        let Ok(parent) = this.parenting.try_borrow() else { return };

        if let ViewParentingType::Windowed { owned_window } = &*parent {
            if let Some(window) = owned_window.load() {
                window.makeKeyAndOrderFront(None)
            }
        }
    }

    pub fn hide(this: ViewRef<Self>) {
        let Ok(parent) = this.parenting.try_borrow() else { return };

        if let ViewParentingType::Windowed { owned_window } = &*parent {
            if let Some(window) = owned_window.load() {
                window.orderOut(None)
            }
        }
    }

    pub fn poll(this: ViewRef<Self>) {
        this.window_handler.use_handler(|h| h.poll());
    }

    /// Starts or pauses the display link. While the window isn't visible it stays paused: a frame
    /// asked for then is drawn when the window becomes visible again (`window_visibility_changed`;
    /// ConjurePluginKit request 66). The last frame after it hides comes through the main queue.
    pub fn set_next_frame_needed(&self, needed: bool) {
        let needed = needed && self.window_visible.get();
        if self.display_link_started.get() == needed {
            return;
        };

        let Some(display_link) = self.display_link.get() else { return };

        display_link.setPaused(!needed);
        self.display_link_started.set(needed);
    }

    /// Tells the handler where `event` happened, as a pointer move. A press or release says only
    /// which button, so without this the handler places it where the pointer last moved: a press
    /// with no move before it (a pen, an automated click, a window opening under a still
    /// pointer, or after the pointer left and came back without moving inside) landed in the
    /// wrong place, or nowhere (ConjurePluginKit; the 0.3 fork's "a position for every click").
    pub(crate) fn place_pointer(this: ViewRef<Self>, event: &NSEvent) {
        let point = this.view.convertPoint_fromView(event.locationInWindow(), None);

        let position = LogicalPosition { x: point.x, y: point.y };

        Self::trigger_event(
            this,
            Event::Mouse(MouseEvent::CursorMoved {
                position: position.to_physical(this.state.scale_factor.get()),
                modifiers: make_modifiers(event.modifierFlags()),
            }),
        );
    }

    pub fn close(this: ViewRef<Self>, from_host: bool) {
        this.state.closed.set(true);
        this.view.removeFromSuperview();
        this.notification_center_observer.take();
        this.window_handler.destroy();
        detach_sublayers(this.view);

        if let Some(link) = this.display_link.get() {
            link.setPaused(true);
            link.invalidate();
        }

        let parenting = this.parenting.replace(ViewParentingType::Uninitialized);
        parenting.teardown();

        if let Some(app) = this.lifetime_tied_to_app.take() {
            if let Some(app) = app.load() {
                app.stop(Some(&app));
            }
        }

        if !from_host {
            this.host.notify_destroyed();
        }
    }

    pub fn set_parent(this: ViewRef<Self>, new_parent: Retained<NSView>) {
        let previous_parenting = this.parenting.replace(ViewParentingType::Uninitialized);
        previous_parenting.teardown();

        let parenting = ViewParentingType::Parented { parent_view: Weak::from(new_parent) };
        parenting.setup(this.view);

        this.parenting.replace(parenting);
    }

    pub fn resize(
        this: ViewRef<Self>, size: LogicalSize<f64>, notify_host: bool, from_window: bool,
    ) {
        if size == this.inner.state.size.get() {
            return;
        }

        // NOTE: macOS gives you a personal rave if you pass in fractional pixels here. Even
        // though the size is in fractional pixels.
        let size = NSSize::new(size.width.round(), size.height.round());

        this.inner.state.resizing.set(true);
        this.view.setFrameSize(size);
        this.inner.state.resizing.set(false);
        this.view.setNeedsDisplay(true);

        // When using OpenGL the `NSOpenGLView` needs to be resized separately? Why? Because
        // macOS.
        #[cfg(feature = "opengl")]
        if let Some(gl_context) = this.gl_context.get() {
            gl_context.resize(size);
        }

        if !from_window {
            // If this is a standalone window then we'll also need to resize the window itself
            if let ViewParentingType::Windowed { owned_window } = &*this.parenting.borrow() {
                if let Some(owned_window) = owned_window.load() {
                    owned_window.setContentSize(size);
                }
            }
        }

        Self::view_did_change_backing_properties(this, notify_host);
    }

    /// Trigger the event immediately and return the event status.
    fn trigger_event(this: ViewRef<Self>, event: Event) -> EventStatus {
        // Input that can finish something: a release, a key, or anything while a button is held.
        // Pointer moves and the wheel over a covered window don't (ConjurePluginKit request 66).
        let finishes = matches!(event, Event::Mouse(ButtonReleased { .. }) | Event::Keyboard(_))
            || NSEvent::pressedMouseButtons() != 0;
        let status =
            this.window_handler.use_handler(|h| h.on_event(event)).unwrap_or(EventStatus::Ignored);
        if finishes && !this.window_visible.get() {
            // It reached a hidden window (a release after a drag that began before the window was
            // covered): one more frame, so the handler sees it (ConjurePluginKit request 66).
            this.hidden_frame_drawn.set(false);
            Self::draw_last_hidden_frame_soon(this);
        }
        status
    }

    /// Reads whether `view`'s window is at least partly visible, and keeps it for
    /// `set_next_frame_needed`. A view in no window counts as visible, as before.
    ///
    /// ConjurePluginKit: the display link goes on firing at the screen's rate while a window is
    /// covered, minimized or on another desktop, and every frame was laid out, turned into
    /// triangles and uploaded for nothing (request 66: 120 frames a second, about 27 % of a core,
    /// for a plug-in window nobody could see).
    pub(crate) fn refresh_window_visible(&self, view: &NSView) -> bool {
        let visible = view
            .window()
            .is_none_or(|w| w.occlusionState().contains(NSWindowOcclusionState::Visible));
        self.window_visible.set(visible);
        if visible {
            self.hidden_frame_drawn.set(false);
        }
        visible
    }

    /// The window became visible or stopped being visible. Either way a frame comes next: on
    /// becoming visible, so the window shows the current state, and frames go on from there as
    /// requests come; on becoming hidden, the last one before frames stop.
    fn window_visibility_changed(this: ViewRef<Self>) {
        let visible = this.refresh_window_visible(this.view);
        if event_log::on() {
            Self::log_window(this, if visible { "visibility notice: visible" } else { "visibility notice: hidden" });
            if visible {
                this.shown_at.set(Some(Instant::now()));
            }
        }
        this.state.redraw_requested.set(true);
        if visible {
            this.set_next_frame_needed(true);
        } else {
            this.set_next_frame_needed(false);
            Self::draw_last_hidden_frame_soon(this);
        }
    }

    /// The window is hidden and owes its last frame: draw it through the main queue, after what
    /// the main thread is doing now, rather than waiting for the display link, which may not fire
    /// while the window is minimized or on another desktop (ConjurePluginKit request 66).
    fn draw_last_hidden_frame_soon(this: ViewRef<Self>) {
        if this.hidden_frame_drawn.get() || this.hidden_frame_queued.replace(true) {
            return;
        }
        let view: MainThreadBoundWeak<View<Self>> =
            MainThreadBoundWeak::new(Weak::from_retained(&this.view.retain()));
        view.use_on_main_thread_later(|view| {
            let Some(view) = view.load() else { return };
            let Some(view) = view.inner_ref() else { return };
            view.hidden_frame_queued.set(false);
            if !view.hidden_frame_drawn.get() {
                Self::trigger_frame(view);
            }
        });
    }

    /// A window-log line with the window's class and what macOS says of its visibility.
    fn log_window(this: ViewRef<Self>, what: &str) {
        let window = this.view.window();
        let class = window.as_deref().map_or("none", |w| w.class().name().to_str().unwrap_or("?"));
        let occlusion = window.as_deref().map_or(0, |w| w.occlusionState().0);
        event_log::log(this.view, &format!("{what}; window class {class}, occlusionState {occlusion:#x}"), None);
    }

    /// Counts a frame drawn, or a firing skipped while hidden, and once a second writes the counts
    /// to the off-by-default window log (ConjurePluginKit request 66).
    fn log_frame(this: ViewRef<Self>, drawn: Option<Duration>) {
        let (since, mut frames, mut skipped, mut time) = this.frame_log.get();
        let since = since.unwrap_or_else(Instant::now);
        match drawn {
            Some(took) => {
                frames += 1;
                time += took;
                if let Some(shown) = this.shown_at.take() {
                    let ms = shown.elapsed().as_secs_f64() * 1000.0;
                    event_log::log(this.view, &format!("first frame {ms:.0} ms after becoming visible"), None);
                }
            }
            None => skipped += 1,
        }
        let secs = since.elapsed().as_secs_f64();
        if secs >= 1.0 {
            let each = if frames > 0 { time.as_secs_f64() * 1000.0 / frames as f64 } else { 0.0 };
            // The counts themselves and their interval, so a reader can add up any stretch of time;
            // a window that draws nothing writes nothing, which reads as no frames (request 69).
            let line = format!(
                "frames {frames} in {secs:.2} s, {} display-link firings, {skipped} skipped while hidden, {each:.2} ms each; visible {}",
                this.link_firings.replace(0), this.window_visible.get()
            );
            event_log::log(this.view, &line, None);
            if event_log::frames_wanted() {
                event_log::frames(&line);
            }
            this.frame_log.set((Some(Instant::now()), 0, 0, Duration::ZERO));
        } else {
            this.frame_log.set((Some(since), frames, skipped, time));
        }
    }

    fn trigger_frame(this: ViewRef<Self>) {
        if !this.refresh_window_visible(this.view) {
            if this.hidden_frame_drawn.get() {
                // Counted always: once a second the count goes to the window log if it is on, and
                // to the unified log's debug level if anything listens (request 69).
                Self::log_frame(this, None);
                // Hidden, and its last frame drawn: keep the request for when it's visible again.
                this.state.redraw_requested.set(true);
                this.set_next_frame_needed(false);
                return;
            }
            // The last frame while hidden: drawn as any other (nothing reaches the screen).
            this.hidden_frame_drawn.set(true);
        }
        let started = Instant::now();
        let result = this.window_handler.use_handler(|h| {
            this.state.redraw_requested.set(false);
            h.draw()?;

            Ok::<(), HandlerError>(())
        });
        if result.is_some() {
            Self::log_frame(this, Some(started.elapsed()));
        }

        let Some(result) = result else { return };

        if let Err(e) = result {
            warn!("Error while drawing: {}", e);
            Self::close(this, false);
        } else {
            this.set_next_frame_needed(this.state.redraw_requested.take())
        }
    }

    pub(crate) fn trigger_timer(this: ViewRef<Self>, handle: &TimerHandle) {
        this.window_handler.use_handler(|h| {
            h.on_timer(handle.into());
        });
    }

    pub(crate) fn create_timer(
        &self, this: &Retained<View<Self>>, duration: Duration,
    ) -> TimerHandle {
        let view = Weak::from_retained(this);
        self.timers.create_timer(view, duration)
    }

    fn apply_size_constraints(this: ViewRef<Self>) {
        let ViewParentingType::Windowed { owned_window } = &*this.parenting.borrow() else {
            return;
        };
        let Some(window) = owned_window.load() else { return };
        let scale_factor = window.backingScaleFactor();

        if let Some(min_size) = this.state.sizing_strategy.min_size() {
            let min_size = min_size.to_logical(scale_factor);
            window.setContentMinSize(NSSize::new(min_size.width, min_size.height));
        }

        if let Some(max_size) = this.state.sizing_strategy.max_size() {
            let max_size = max_size.to_logical(scale_factor);
            window.setContentMaxSize(NSSize::new(max_size.width, max_size.height));
        }
    }
}

impl Drop for BaseviewView {
    fn drop(&mut self) {
        self.state.closed.set(true);
    }
}

impl ViewImpl for BaseviewView {
    fn become_first_responder(this: ViewRef<Self>) -> bool {
        let Some(window) = this.view.window() else {
            return true;
        };

        if window.isKeyWindow() {
            Self::trigger_event(this, Event::Window(WindowEvent::Focused));
        }

        true
    }

    fn resign_first_responder(this: ViewRef<Self>) -> bool {
        Self::trigger_event(this, Event::Window(WindowEvent::Unfocused));
        true
    }

    fn window_should_close(this: ViewRef<Self>) -> bool {
        Self::trigger_event(this, Event::Window(WindowEvent::WillClose));
        Self::close(this, false);

        true
    }

    fn window_did_resize(this: ViewRef<Self>) {
        let Some(window) = this.view.window() else { return };

        let size = window.contentRectForFrameRect(window.frame()).size;
        let size = LogicalSize::new(size.width, size.height);

        BaseviewView::resize(this, size, true, true);
    }

    fn view_did_change_backing_properties(this: ViewRef<Self>, notify_host: bool) {
        let current_size = this.view.size();
        let current_scale_factor = this.view.backing_scale_factor();

        if this.state.scale_factor.get() != current_scale_factor {
            Self::apply_size_constraints(this);
        }

        // Only send the event when the window's size has actually changed to be in line with the
        // other platform implementations
        if this.state.scale_factor.get() != current_scale_factor
            || this.state.size.get() != current_size
        {
            let previous = this.state.size.replace(current_size);
            this.state.scale_factor.set(current_scale_factor);
            let new_size = WindowSize::from_logical(current_size, current_scale_factor);

            let result = this.window_handler.use_handler(|h| h.resized(new_size));

            if let Some(Err(e)) = result {
                warn!("Window Handler failed to resize: {}", e);
                this.state.size.set(previous);

                Self::resize(this, previous, false, false);
                return;
            }

            if notify_host {
                if let Err(e) = this.host.request_resize(new_size) {
                    warn!("Host failed to resize parent view: {}", e);

                    Self::resize(this, previous, false, false);
                }
            }
        }
    }

    fn draw_rect(this: ViewRef<Self>, _rect: NSRect) {
        this.refresh_window_visible(this.view);
        this.window_handler.use_handler(|h| {
            let mut rects_ptr = null();
            let mut rects_count = 0;
            unsafe { this.view.getRectsBeingDrawn_count(&mut rects_ptr, &mut rects_count) };

            let rects_count: usize = rects_count.try_into().unwrap_or(0);
            let rects;

            let damage_area = if rects_count == 0 {
                DamageArea::FullWindow
            } else {
                let scale_factor = this.view.backing_scale_factor();

                let rects_raw = unsafe { core::slice::from_raw_parts(rects_ptr, rects_count) };

                rects = rects_raw
                    .iter()
                    .map(|r| DamageRect::from_rect(*r, scale_factor).into())
                    .collect::<Vec<_>>();

                DamageArea::Rects(&rects)
            };

            h.damage(damage_area);
        });

        this.set_next_frame_needed(true);
    }

    fn display_link_fired(this: ViewRef<Self>, _sender: &CADisplayLink) {
        this.link_firings.set(this.link_firings.get() + 1);
        Self::trigger_frame(this);
    }

    /// `hitTest:` override that collapses hits on baseview's internal
    /// OpenGL render subview to this NSView.
    ///
    /// `src/gl/gl` attaches an `NSOpenGLView` as a subview of this
    /// view so the GL context is isolated from event handling. The side
    /// effect is that `[NSView hitTest:]` returns the GL subview for
    /// every click inside our frame — `NSOpenGLView` inherits the
    /// default `acceptsFirstMouse:` which returns `NO`, so AppKit treats
    /// the first click in a non-key window as an activation click and
    /// never dispatches `mouseDown:`. That's the "first click dead zone"
    /// symptom reported in baseview#129 / #202 / #169.
    ///
    /// Fix: if the hit lands on our own GL render subview (pointer
    /// equality against the `NSOpenGLView` stored in `GlContext`),
    /// collapse the result to `self`. AppKit then asks US about
    /// `acceptsFirstMouse:` (we return `YES`), and `mouseDown:` is
    /// dispatched on the first click. Hits on any other subview pass
    /// through unchanged — we only redirect our own render child, not
    /// anything the consumer may add.
    ///
    /// No-op without the `opengl` feature: there's no GL subview to
    /// collapse, so the override pass-through is equivalent to the
    /// default implementation.
    fn hit_test(this: ViewRef<'_, Self>, point: NSPoint) -> Option<&NSView> {
        let superclass = NSView::class();

        // SAFETY: Our superclass is NSView
        let super_result: Option<&NSView> =
            unsafe { msg_send![super(this.view, superclass), hitTest: point] };
        let super_result = super_result?;

        #[cfg(feature = "opengl")]
        {
            if let Some(gl_context) = this.gl_context.get() {
                if *super_result == **gl_context.view {
                    return Some(this.view);
                }
            }
        }

        Some(super_result)
    }

    fn view_will_move_to_window(this: ViewRef<Self>, new_window: Option<&NSWindow>) {
        // After the move the window's visibility is the new window's, which sends no notice when
        // it doesn't change: read it now, and draw a frame there (ConjurePluginKit request 66). A
        // view leaving its window draws nothing.
        let visible = new_window
            .is_none_or(|w| w.occlusionState().contains(NSWindowOcclusionState::Visible));
        if event_log::on() {
            let class = new_window.map_or("none", |w| w.class().name().to_str().unwrap_or("?"));
            let occlusion = new_window.map_or(0, |w| w.occlusionState().0);
            event_log::log(this.view, &format!("moves to window class {class}, occlusionState {occlusion:#x}"), None);
        }
        this.window_visible.set(visible);
        if visible {
            this.hidden_frame_drawn.set(false);
        }
        if new_window.is_some() {
            this.state.redraw_requested.set(true);
            if visible {
                this.set_next_frame_needed(true);
            } else {
                Self::draw_last_hidden_frame_soon(this);
            }
        }

        // The view's own tracking area follows its visible rectangle by itself
        // (`InVisibleRect`), so leaving a window needs nothing. Only the view's own area is ever
        // touched: hosts add tracking areas to the view too, and taking out the first one in the
        // list took out Pro Tools' (ConjurePluginKit, the 0.3 fork's ca7eefa).
        if let Some(new_window) = new_window {
            ensure_own_tracking_area(this.view);
            new_window.setAcceptsMouseMovedEvents(true);
            new_window.makeFirstResponder(Some(this.view));
        }

        unsafe {
            let () = msg_send![super(this.view, NSView::class()), viewWillMoveToWindow: new_window];
        }
    }

    fn update_tracking_areas(this: ViewRef<Self>) {
        // `InVisibleRect` keeps the view's own area right; others' areas are theirs.
        ensure_own_tracking_area(this.view);
    }

    fn mouse_moved(this: ViewRef<Self>, event: &NSEvent) {
        Self::place_pointer(this, event);

        // SAFETY: Our superclass is NSView
        let _: () = unsafe { msg_send![super(this.view, NSView::class()), mouseMoved: event] };
    }

    fn scroll_wheel(this: ViewRef<Self>, event: &NSEvent) {
        let x = event.scrollingDeltaX() as f32;
        let y = event.scrollingDeltaY() as f32;

        let delta = if event.hasPreciseScrollingDeltas() {
            ScrollDelta::Pixels { x, y }
        } else {
            ScrollDelta::Lines { x, y }
        };

        Self::trigger_event(
            this,
            Event::Mouse(MouseEvent::WheelScrolled {
                delta,
                modifiers: make_modifiers(event.modifierFlags()),
            }),
        );
    }

    /// A trackpad pinch, as a precise scroll with Control held, which GUI toolkits read as zoom
    /// (egui: Control-scroll zooms by exp(delta / 200), so 200 x the magnification gives the native
    /// 1 + magnification per event, compounded over the gesture). baseview has no pinch event of its
    /// own, so this needs no new event type. The event's real modifiers ride along.
    fn frame_size_changed(this: ViewRef<Self>) {
        // Baseview's own resize carries on from here, and tells the host itself.
        if this.inner.state.resizing.get() {
            return;
        }
        // Someone else resized the view, the host most likely: the contents follow, and the host,
        // which did it, isn't asked.
        Self::view_did_change_backing_properties(this, false);
    }

    fn magnify(this: ViewRef<Self>, event: &NSEvent) {
        let magnification = event.magnification() as f32;
        let mut modifiers = make_modifiers(event.modifierFlags());
        modifiers.insert(keyboard_types::Modifiers::CONTROL);
        Self::trigger_event(
            this,
            Event::Mouse(MouseEvent::WheelScrolled {
                delta: ScrollDelta::Pixels { x: 0.0, y: 200.0 * magnification },
                modifiers,
            }),
        );
    }

    fn dragging_entered(
        this: ViewRef<Self>, sender: Option<&ProtocolObject<dyn NSDraggingInfo>>,
    ) -> NSDragOperation {
        let modifiers = this.keyboard_state.last_mods();
        let drop_data = get_drop_data(sender);

        let event = MouseEvent::DragEntered {
            position: get_drag_position(sender).to_physical(this.view.backing_scale_factor()),
            modifiers: make_modifiers(modifiers),
            data: drop_data,
        };

        on_event(this, event)
    }

    fn dragging_updated(
        this: ViewRef<Self>, sender: Option<&ProtocolObject<dyn NSDraggingInfo>>,
    ) -> NSDragOperation {
        let modifiers = this.keyboard_state.last_mods();
        let drop_data = get_drop_data(sender);

        let event = MouseEvent::DragMoved {
            position: get_drag_position(sender).to_physical(this.view.backing_scale_factor()),
            modifiers: make_modifiers(modifiers),
            data: drop_data,
        };

        on_event(this, event)
    }

    fn prepare_for_drag_operation(
        _this: ViewRef<Self>, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>,
    ) -> bool {
        // Always accept drag operation if we get this far
        // This function won't be called unless dragging_entered/updated
        // has returned an acceptable operation
        true
    }

    fn perform_drag_operation(
        this: ViewRef<Self>, sender: Option<&ProtocolObject<dyn NSDraggingInfo>>,
    ) -> bool {
        let modifiers = this.keyboard_state.last_mods();
        let drop_data = get_drop_data(sender);

        let event = MouseEvent::DragDropped {
            position: get_drag_position(sender).to_physical(this.view.backing_scale_factor()),
            modifiers: make_modifiers(modifiers),
            data: drop_data,
        };

        let event_status = Self::trigger_event(this, Event::Mouse(event));

        matches!(event_status, EventStatus::AcceptDrop(_))
    }

    fn dragging_exited(this: ViewRef<Self>, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
        on_event(this, MouseEvent::DragLeft);
    }

    fn handle_notification(this: ViewRef<Self>, notification: &NSNotification) {
        let Some(window) = this.view.window() else { return };
        // The subject of the notification, in this case an NSWindow object.
        let Some(notification_object) = notification.object().and_then(|o| o.downcast().ok())
        else {
            return;
        };

        // Only trigger focus events if the NSWindow that's being notified about is our window,
        // and if the window's first responder is our NSView.
        if window != notification_object {
            return;
        }

        if *notification.name() == *unsafe { NSWindowDidChangeOcclusionStateNotification } {
            Self::window_visibility_changed(this);
            return;
        }

        let Some(first_responder) = window.firstResponder() else { return };

        // If the first responder isn't our NSView, the focus events will instead be triggered
        // by the becomeFirstResponder and resignFirstResponder methods on the NSView itself.
        if !this.view.isEqual(Some(&first_responder)) {
            return;
        }

        Self::trigger_event(
            this,
            Event::Window(if window.isKeyWindow() {
                WindowEvent::Focused
            } else {
                WindowEvent::Unfocused
            }),
        );
    }

    fn mouse_down(this: ViewRef<Self>, event: &NSEvent) {
        Self::place_pointer(this, event);
        Self::trigger_event(
            this,
            Event::Mouse(ButtonPressed {
                button: MouseButton::Left,
                modifiers: make_modifiers(event.modifierFlags()),
            }),
        );
    }

    fn mouse_up(this: ViewRef<Self>, event: &NSEvent) {
        Self::place_pointer(this, event);
        Self::trigger_event(
            this,
            Event::Mouse(ButtonReleased {
                button: MouseButton::Left,
                modifiers: make_modifiers(event.modifierFlags()),
            }),
        );
    }

    fn right_mouse_down(this: ViewRef<Self>, event: &NSEvent) {
        Self::place_pointer(this, event);
        Self::trigger_event(
            this,
            Event::Mouse(ButtonPressed {
                button: MouseButton::Right,
                modifiers: make_modifiers(event.modifierFlags()),
            }),
        );
    }

    fn right_mouse_up(this: ViewRef<Self>, event: &NSEvent) {
        Self::place_pointer(this, event);
        Self::trigger_event(
            this,
            Event::Mouse(ButtonReleased {
                button: MouseButton::Right,
                modifiers: make_modifiers(event.modifierFlags()),
            }),
        );
    }

    fn other_mouse_down(this: ViewRef<Self>, event: &NSEvent) {
        Self::place_pointer(this, event);
        Self::trigger_event(
            this,
            Event::Mouse(ButtonPressed {
                button: MouseButton::Middle,
                modifiers: make_modifiers(event.modifierFlags()),
            }),
        );
    }

    fn other_mouse_up(this: ViewRef<Self>, event: &NSEvent) {
        Self::place_pointer(this, event);
        Self::trigger_event(
            this,
            Event::Mouse(ButtonReleased {
                button: MouseButton::Middle,
                modifiers: make_modifiers(event.modifierFlags()),
            }),
        );
    }

    fn mouse_entered(this: ViewRef<Self>) {
        this.cursor_manager.set_is_inside(true);
        Self::trigger_event(this, Event::Mouse(MouseEvent::CursorEntered));
    }

    fn mouse_exited(this: ViewRef<Self>) {
        this.cursor_manager.set_is_inside(false);
        Self::trigger_event(this, Event::Mouse(MouseEvent::CursorLeft));
    }

    fn cursor_update(this: ViewRef<Self>, event: Option<&NSEvent>) -> bool {
        let Some(event) = event else { return false };
        let point = this.view.convertPoint_fromView(event.locationInWindow(), None);
        if NSPointInRect(point, this.view.frame()) {
            this.cursor_manager.update_to_current_cursor();
            true
        } else {
            false
        }
    }

    fn key_down(this: ViewRef<Self>, event: &NSEvent) {
        if let Some(key_event) = this.keyboard_state.process_native_event(event) {
            let status = Self::trigger_event(this, Event::Keyboard(key_event));

            if let EventStatus::Ignored = status {
                unsafe {
                    let superclass = msg_send![this.view, superclass];

                    let () = msg_send![super(this.view, superclass), keyDown:event];
                }
            }
        }
    }

    fn key_up(this: ViewRef<Self>, event: &NSEvent) {
        if let Some(key_event) = this.keyboard_state.process_native_event(event) {
            let status = Self::trigger_event(this, Event::Keyboard(key_event));

            if let EventStatus::Ignored = status {
                unsafe {
                    let superclass = msg_send![this.view, superclass];

                    let () = msg_send![super(this.view, superclass), keyUp:event];
                }
            }
        }
    }

    fn flags_changed(this: ViewRef<Self>, event: &NSEvent) {
        if let Some(key_event) = this.keyboard_state.process_native_event(event) {
            let status = Self::trigger_event(this, Event::Keyboard(key_event));

            if let EventStatus::Ignored = status {
                unsafe {
                    let superclass = msg_send![this.view, superclass];

                    let () = msg_send![super(this.view, superclass), flagsChanged:event];
                }
            }
        }
    }
}

/// Takes every sublayer off the view's layer, once the handler, and with it whatever drew into
/// the view, is gone. wgpu draws into a layer (raw-window-metal's) that it adds as a sublayer, and
/// that layer observes the view's layer, stopping only when it is freed. Left attached, it is freed
/// only after the view's layer, and the two observers it registered were never taken off: one leak
/// each time the window opened (ConjurePluginKit's host-check, `leaks`).
fn detach_sublayers(view: &NSView) {
    // SAFETY: CALayer's own calls on the view's layer, on the main thread. `sublayers` returns a
    // copy, so taking them off while going through it is safe.
    unsafe {
        let layer: Option<Retained<AnyObject>> = msg_send![view, layer];
        let Some(layer) = layer else { return };
        let sublayers: Option<Retained<AnyObject>> = msg_send![&*layer, sublayers];
        let Some(sublayers) = sublayers else { return };
        let count: usize = msg_send![&*sublayers, count];
        for i in 0..count {
            let sublayer: Retained<AnyObject> = msg_send![&*sublayers, objectAtIndex: i];
            let () = msg_send![&*sublayer, removeFromSuperlayer];
        }
    }
}

/// Adds the view's own tracking area if it has none, and touches no other.
fn ensure_own_tracking_area(view: &NSView) {
    let own = view.trackingAreas().iter().any(|area| {
        let owner = area.owner();
        owner.is_some_and(|o| core::ptr::eq(Retained::as_ptr(&o).cast::<u8>(), (view as *const NSView).cast::<u8>()))
    });
    if !own {
        view.addTrackingArea(&new_tracking_area(view));
    }
}

/// Info:
/// https://developer.apple.com/documentation/appkit/nstrackingarea
/// https://developer.apple.com/documentation/appkit/nstrackingarea/options
/// https://developer.apple.com/documentation/appkit/nstrackingareaoptions.
fn new_tracking_area(this: &NSView) -> Retained<NSTrackingArea> {
    let options = NSTrackingAreaOptions::MouseEnteredAndExited
        | NSTrackingAreaOptions::MouseMoved
        | NSTrackingAreaOptions::CursorUpdate
        // Always, not only in the key window or the active app: an Audio Unit extension (AUv3)
        // runs in a process of its own, which is never the active app while the host shows its
        // view, so the pointer's moves never arrived there and nothing hovered. JUCE uses the
        // same option for the same reason.
        | NSTrackingAreaOptions::ActiveAlways
        | NSTrackingAreaOptions::InVisibleRect
        | NSTrackingAreaOptions::EnabledDuringMouseDrag;

    // SAFETY: `this` is of the correct type (NSView)
    unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
            options,
            Some(this),
            None,
        )
    }
}

fn get_drag_position(sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) -> LogicalPosition<f64> {
    let point = match sender {
        Some(sender) => sender.draggingLocation(),
        None => NSPoint::ZERO,
    };

    LogicalPosition::new(point.x, point.y)
}

fn get_drop_data(sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) -> DropData {
    let Some(sender) = sender else {
        return DropData::None;
    };

    let pasteboard = sender.draggingPasteboard();
    let Some(file_list) = pasteboard.propertyListForType(unsafe { NSFilenamesPboardType }) else {
        return DropData::None;
    };

    let Ok(file_list) = file_list.downcast::<NSArray>() else {
        return DropData::None;
    };

    let files = file_list
        .into_iter()
        .filter_map(|s| s.downcast::<NSString>().ok())
        .map(|s| s.to_string().into())
        .collect();

    DropData::Files(files)
}

fn on_event(this: ViewRef<BaseviewView>, event: MouseEvent) -> NSDragOperation {
    let event_status = BaseviewView::trigger_event(this, Event::Mouse(event));
    match event_status {
        EventStatus::AcceptDrop(DropEffect::Copy) => NSDragOperation::Copy,
        EventStatus::AcceptDrop(DropEffect::Move) => NSDragOperation::Move,
        EventStatus::AcceptDrop(DropEffect::Link) => NSDragOperation::Link,
        EventStatus::AcceptDrop(DropEffect::Scroll) => NSDragOperation::Generic,
        _ => NSDragOperation::None,
    }
}

pub struct WindowHandlerContainer {
    inner: RefCell<Option<Box<dyn WindowHandler>>>,
    must_be_destroyed: Cell<bool>,
}

impl WindowHandlerContainer {
    pub fn new() -> WindowHandlerContainer {
        Self { inner: RefCell::new(None), must_be_destroyed: false.into() }
    }

    pub fn use_handler<T>(&self, user: impl FnOnce(&dyn WindowHandler) -> T) -> Option<T> {
        let returned = {
            let inner = self.inner.try_borrow().ok()?;
            user(inner.as_ref()?.as_ref())
        };

        if self.must_be_destroyed.get() {
            if let Ok(mut inner) = self.inner.try_borrow_mut() {
                *inner = None;
            }
        }

        Some(returned)
    }

    pub fn set(&self, handler: Box<dyn WindowHandler>) {
        self.inner.replace(Some(handler));
        self.must_be_destroyed.set(false);
    }

    pub fn destroy(&self) {
        match self.inner.try_borrow_mut() {
            Ok(mut inner) => *inner = None,
            Err(_) => self.must_be_destroyed.set(true),
        }
    }
}
