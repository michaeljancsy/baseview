//! A diagnostic log of the AppKit calls baseview's view receives, for finding where a host loses
//! an event (ConjurePluginKit request 47). Off unless the file `~/Library/Logs/ConjureKit-window.log`
//! exists when the view first logs: create it (for example with `touch`) before starting the host,
//! then read it. Main thread only, as every AppKit call to a view is.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::runtime::Sel;
use objc2_app_kit::{NSEvent, NSEventType, NSView};

fn file() -> Option<&'static Mutex<File>> {
    static FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    FILE.get_or_init(|| {
        let home = std::env::var_os("HOME")?;
        let path = std::path::Path::new(&home).join("Library/Logs/ConjureKit-window.log");
        if !path.exists() {
            return None;
        }
        OpenOptions::new().append(true).open(path).ok().map(Mutex::new)
    })
    .as_ref()
}

/// Whether the log is on.
pub fn on() -> bool {
    cfg!(feature = "window-log-system") || file().is_some()
}

extern "C" {
    /// os_log.c: one line to macOS's unified log, subsystem com.michaeljancsy.conjurekit.window,
    /// marked public.
    #[cfg(feature = "window-log-system")]
    fn baseview_window_os_log(line: *const std::ffi::c_char);
    /// os_log.c: whether anything is listening for the "frames" debug lines.
    fn baseview_frames_log_wanted() -> bool;
    /// os_log.c: one "frames" debug line.
    fn baseview_frames_log(line: *const std::ffi::c_char);
}

/// ConjurePluginKit request 69: whether anything is listening for the window's frame counts.
pub fn frames_wanted() -> bool {
    // SAFETY: a plain query of the unified log.
    unsafe { baseview_frames_log_wanted() }
}

/// ConjurePluginKit request 69: one frame-count line, at debug level, which costs nothing unless
/// something is listening (`log stream --level debug`), in every build.
pub fn frames(line: &str) {
    if let Ok(line) = std::ffi::CString::new(line) {
        // SAFETY: a NUL-terminated string that outlives the call.
        unsafe { baseview_frames_log(line.as_ptr()) };
    }
}

/// Writes `line` where the log goes: the unified log in a `window-log-system` build, the file
/// otherwise.
fn write(line: &str) {
    #[cfg(feature = "window-log-system")]
    {
        if let Ok(line) = std::ffi::CString::new(line) {
            // SAFETY: a NUL-terminated string that outlives the call.
            unsafe { baseview_window_os_log(line.as_ptr()) };
        }
        return;
    }
    #[allow(unreachable_code)]
    if let Some(file) = file() {
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// A selector's name as the log writes it, without the trailing colon.
pub fn name(sel: Sel) -> &'static str {
    let name = sel.name().to_str().unwrap_or("?");
    name.strip_suffix(':').unwrap_or(name)
}

/// One line: the time, the view, its window, its frame, what happened, the buttons the system
/// says are down, and for an event its type and position in the window.
pub fn log(view: &NSView, what: &str, event: Option<&NSEvent>) {
    if !on() {
        return;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let window = view.window();
    let window_ptr = window.as_deref().map_or(std::ptr::null(), |w| w as *const _);
    let frame = view.frame();
    let buttons = NSEvent::pressedMouseButtons();
    let mut line = format!(
        "{now:.3} view {:p} window {:p} frame {:.0}x{:.0} {what} down-buttons {buttons:#x}",
        view as *const NSView, window_ptr, frame.size.width, frame.size.height
    );
    if let Some(event) = event {
        let at = event.locationInWindow();
        line.push_str(&format!(" type {} at {:.0},{:.0}", event.r#type().0, at.x, at.y));
    }
    write(&line);
}

/// A call from one of the selectors that share the view's move handler: only the drags.
/// Plain moves come many times a second and say nothing about a lost release.
pub fn log_drag(view: &NSView, sel: Sel, event: &NSEvent) {
    if on() && event.r#type() != NSEventType::MouseMoved {
        log(view, name(sel), Some(event));
    }
}
