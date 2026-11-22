// ConjurePluginKit request 66, `window-log-system` builds only: one window-log line to macOS's
// unified log, marked public so it isn't redacted. Read it with
// log show --predicate 'subsystem == "com.michaeljancsy.conjurekit.window"'
#include <os/log.h>

void baseview_window_os_log(const char *line) {
    static os_log_t log;
    if (!log) {
        log = os_log_create("com.michaeljancsy.conjurekit.window", "window");
    }
    os_log(log, "%{public}s", line);
}
