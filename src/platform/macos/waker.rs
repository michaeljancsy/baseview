use crate::platform::macos::view::BaseviewView;
use crate::wrappers::appkit::{MainThreadBoundWeak, View};
use objc2::rc::Weak;
use std::time::Duration;

#[derive(Clone)]
pub struct WindowWaker {
    view: MainThreadBoundWeak<View<BaseviewView>>,
}

impl WindowWaker {
    pub fn new(reference: Weak<View<BaseviewView>>) -> Self {
        Self { view: MainThreadBoundWeak::new(reference) }
    }

    pub fn request_redraw(&self) {
        self.request_redraw_after(Duration::ZERO)
    }

    pub fn request_redraw_after(&self, duration: Duration) {
        self.view.use_on_main_thread_after(duration, |view| {
            let Some(ns_view) = view.load() else { return };
            let Some(view) = ns_view.inner() else { return };
            // Ask for the frame the way the window context does, by setting the request as well
            // as restarting the display link. Restarting it alone is undone at the end of the frame
            // in progress, which pauses the link again when no request is set: a GUI that asks
            // for its next frame from inside a frame (egui does, through this waker) then got no
            // more frames until an input event set the request, so a window stayed blank or stale
            // until the pointer moved (ConjurePluginKit request 39, REAPER and others).
            view.state.redraw_requested.set(true);
            // Whether the window is visible, as now (ConjurePluginKit request 66).
            view.refresh_window_visible(&ns_view);
            view.set_next_frame_needed(true);
        })
    }

    pub fn request_poll(&self) {
        self.view.use_on_main_thread(|view| {
            let Some(view) = view.load() else { return };
            let Some(view) = view.inner_ref() else { return };

            BaseviewView::poll(view);
        })
    }
}
