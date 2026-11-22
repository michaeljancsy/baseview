// ConjurePluginKit request 66: the `window-log-system` feature needs os_log, which is a C macro, so
// a small C file calls it. Without the feature, nothing is built.
fn main() {
    #[cfg(feature = "window-log-system")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new().file("src/wrappers/appkit/os_log.c").compile("baseview_os_log");
        println!("cargo:rerun-if-changed=src/wrappers/appkit/os_log.c");
    }
}
