// ConjurePluginKit: os_log is a C macro, so a small C file calls it on macOS. It writes the window's
// frame counts at debug level, which cost nothing unless something is listening (request 69), and,
// with the `window-log-system` feature, the whole window log (request 66).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new().file("src/wrappers/appkit/os_log.c").compile("baseview_os_log");
        println!("cargo:rerun-if-changed=src/wrappers/appkit/os_log.c");
    }
}
