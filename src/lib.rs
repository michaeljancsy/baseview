mod clipboard;
mod context;
mod damage;
pub mod dpi;
mod error;
mod event;
mod handler;
pub mod host;
mod keyboard;
mod mouse_cursor;
mod settings;
mod timer;
mod tracing;
mod waker;
mod window;

pub(crate) mod platform;

#[cfg(feature = "opengl")]
pub mod gl;

pub use clipboard::*;
pub use context::{PlatformHandle, WindowContext};
pub use damage::{DamageArea, DamageRect};
pub use error::*;
pub use event::*;
pub use handler::WindowHandler;
pub use mouse_cursor::MouseCursor;
pub use settings::*;
pub use timer::TimerHandle;
pub use waker::WindowWaker;
pub use window::*;

#[allow(unused, reason = "Some platforms may not use all exports from this mod")]
pub(crate) use tracing::*;

mod utils;
pub(crate) mod wrappers;

/// ConjurePluginKit (requests 68 and 69): whether anything is listening for the window's "frames"
/// lines, at debug level in macOS's unified log (subsystem `com.michaeljancsy.conjurekit.window`).
/// Always false elsewhere.
pub fn frames_log_wanted() -> bool {
    #[cfg(target_os = "macos")]
    return wrappers::appkit::event_log::frames_wanted();
    #[cfg(not(target_os = "macos"))]
    return false;
}

/// ConjurePluginKit (requests 68 and 69): one more "frames" line from the code that draws in the
/// window, such as why it drew, beside the window's own frame counts. Costs nothing unless
/// something is listening; ask [`frames_log_wanted`] first to skip building the line. Does nothing
/// elsewhere.
pub fn frames_log(line: &str) {
    #[cfg(target_os = "macos")]
    wrappers::appkit::event_log::frames(line);
    #[cfg(not(target_os = "macos"))]
    let _ = line;
}

/// Assumes the current baseview library is the only one running in this process.
///
/// This allows baseview to change some platform-specific settings for greater compatibility.
/// Which settings are actually changed is documented below for information, but is not to be
/// considered stable. They are considered an implementation detail.
///
/// # Safety
///
/// This function must *not* be called in the following cases:
///
/// * The current binary is a plugin that can be loaded into an external host;
/// * Multiple `baseview` versions are present in the final binary;
/// * `baseview` is being used in conjunction with other platform windowing libraries (e.g. `winit`,
///   SDL, etc.);
/// * The current process may host other plugins that need to interact with the platform's GUI capabilities.
///
/// # Platform-specific considerations
///
/// This function is currently a no-op on macOS and X11.
/// On Windows, this sets the process' DPI awareness setting to the latest option supported by the
/// currently running system (up to `PerMonitorAwareV2`).
#[inline]
pub unsafe fn assume_standalone_in_process() {
    platform::assume_standalone_in_process()
}
