fn main() {
    // Tauri needs Common Controls v6 on Windows. tauri-build would put the
    // manifest that asks for it into the app binary only, and test binaries
    // then fail to start (STATUS_ENTRYPOINT_NOT_FOUND). The linker puts the
    // same manifest (tauri-build's default) into every binary instead.
    let windows = tauri_build::WindowsAttributes::new_without_app_manifest()
        .window_icon_path("icons/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let dir = std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo");
        let manifest = std::path::Path::new(&dir).join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }
    tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows))
        .expect("tauri-build failed");
}
