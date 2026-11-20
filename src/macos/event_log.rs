#![allow(unexpected_cfgs)]

//! A diagnostic log of the AppKit events baseview's view receives, for finding where a host loses
//! one (ConjurePluginKit request 47). Off unless the file `~/Library/Logs/ConjureKit-window.log`
//! exists when the view first logs: create it (for example with `touch`) before starting the host,
//! then read it. Main thread only, as every AppKit event is.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use cocoa::base::{id, nil};
use cocoa::foundation::{NSPoint, NSRect};
use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};

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
    file().is_some()
}

/// One line: the time, the view, its window, its frame, what happened, and for an event its type,
/// position in the window, button number, click count, and the buttons the system says are down.
#[allow(unexpected_cfgs)]
pub fn log(view: &Object, what: &str, event: id) {
    let Some(file) = file() else { return };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let (window, frame): (id, NSRect) = unsafe { (msg_send![view, window], msg_send![view, frame]) };
    let buttons: usize = unsafe { msg_send![class!(NSEvent), pressedMouseButtons] };
    let mut line = format!(
        "{now:.3} view {:p} window {:p} frame {:.0}x{:.0} {what} down-buttons {buttons:#x}",
        view as *const Object, window, frame.size.width, frame.size.height
    );
    if event != nil {
        let (kind, at): (u64, NSPoint) = unsafe { (msg_send![event, type], msg_send![event, locationInWindow]) };
        line += &format!(" type {kind} at {:.0},{:.0}", at.x, at.y);
    }
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "{line}");
    }
}
