use crate::platform::macos::view::BaseviewView;
use crate::wrappers::appkit::View;
use block2::RcBlock;
use objc2::__framework_prelude::Retained;
use objc2::rc::Weak;
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSTimer};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::time::Duration;

#[derive(Clone, Eq)]
pub struct TimerHandle {
    timer: NonNull<NSTimer>,
}

impl PartialEq for TimerHandle {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.timer == other.timer
    }
}

pub struct TimerManager {
    timers: RefCell<Vec<Retained<NSTimer>>>,
}

impl TimerManager {
    pub fn new() -> Self {
        TimerManager { timers: vec![].into() }
    }

    pub fn create_timer(&self, view: Weak<View<BaseviewView>>, duration: Duration) -> TimerHandle {
        let timer = create_timer(view, duration);

        let Some(ptr) = NonNull::new(Retained::as_ptr(&timer).cast_mut()) else { unreachable!() };

        let handle = TimerHandle { timer: ptr };

        self.add_timer(timer);

        handle
    }

    fn add_timer(&self, timer: Retained<NSTimer>) {
        self.timers.borrow_mut().push(timer);
    }
}

impl Drop for TimerManager {
    fn drop(&mut self) {
        for timer in self.timers.take() {
            timer.invalidate();
        }
    }
}

fn create_timer(view: Weak<View<BaseviewView>>, duration: Duration) -> Retained<NSTimer> {
    let interval = duration.as_secs_f64();
    let block = RcBlock::new(move |t| {
        let Some(view) = view.load() else { return };
        let Some(view) = view.inner_ref() else { return };

        let handle = TimerHandle { timer: t };

        BaseviewView::trigger_timer(view, &handle);
    });

    // ConjurePluginKit (request 68): added to the run loop's common modes, as the view's display
    // link is, rather than the default mode alone that `scheduledTimer…` uses, so a timer keeps
    // firing while the host runs a menu, a live resize or a modal dialog. egui-baseview's poll of a
    // product's "something changed" flag rides on such a timer.
    // SAFETY: block does not need to be sendable
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(interval, true, &block) };
    // SAFETY: the current run loop, on the main thread, which views and their timers live on.
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    timer
}
