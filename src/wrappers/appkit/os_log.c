// ConjurePluginKit: the window's lines in macOS's unified log, subsystem
// com.michaeljancsy.conjurekit.window, every value public so it isn't redacted.
//
// Category "frames", at debug level, always built (request 69): once a second, how many frames the
// window drew. Debug lines are neither stored nor even formatted unless something is listening, as
// `log stream --level debug --predicate 'subsystem == "com.michaeljancsy.conjurekit.window"'` does.
//
// Category "window", at the default level (request 66): the whole window log, written only in
// builds with the `window-log-system` feature.
#include <os/log.h>
#include <stdbool.h>

static os_log_t category(const char *name) {
    return os_log_create("com.michaeljancsy.conjurekit.window", name);
}

void baseview_window_os_log(const char *line) {
    static os_log_t window_log;
    if (!window_log) {
        window_log = category("window");
    }
    os_log(window_log, "%{public}s", line);
}

static os_log_t frames_log(void) {
    static os_log_t log_handle;
    if (!log_handle) {
        log_handle = category("frames");
    }
    return log_handle;
}

bool baseview_frames_log_wanted(void) {
    return os_log_type_enabled(frames_log(), OS_LOG_TYPE_DEBUG);
}

void baseview_frames_log(const char *line) {
    os_log_debug(frames_log(), "%{public}s", line);
}
