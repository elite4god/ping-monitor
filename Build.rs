// Embeds the application icon and version metadata into ping-monitor.exe.
// Does nothing when building for Linux/macOS.
fn main() {
    println!("cargo:rerun-if-changed=wix/ping-monitor.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("wix/ping-monitor.ico");
        res.set("ProductName", "Ping Monitor");
        res.set("FileDescription", "Ping Monitor");
        res.set("LegalCopyright", "Copyright (c) 2026 Jeremy Leonard");
        // FileVersion / ProductVersion are taken from Cargo.toml automatically.
        res.compile().expect("failed to embed Windows resources");
    }
}