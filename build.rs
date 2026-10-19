fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/oxide.ico");
    // Embed the app icon in the Windows executable.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("packaging/windows/oxide.ico");
        resource.compile().expect("embed the Windows icon");
    }
}
