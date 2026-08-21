//! Stamps the Windows executable with its icon and version metadata.
//!
//! The icon matters more than it looks: it is what the tray shows, what the
//! installer puts on the Start menu, and what Explorer draws next to the
//! executable. Without it all three fall back to the generic application icon.

fn main() {
    // The *target*, not the host. `cfg!(windows)` in a build script reports the
    // machine doing the building, so cross-compiling from Windows to Linux
    // would otherwise try to embed a Windows resource into an ELF binary.
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/waypad.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/waypad.ico");
        resource.set("ProductName", "Waypad");
        resource.set("FileDescription", "Waypad remote control daemon");
        resource.set("CompanyName", "Waypad");
        resource.set("LegalCopyright", "Waypad contributors");
        if let Err(err) = resource.compile() {
            // A missing resource compiler must not stop the build: the daemon
            // works perfectly with a generic icon, and failing here would make
            // a working toolchain a hard requirement for no functional gain.
            println!("cargo:warning=could not embed the Windows icon: {err}");
        }
    }
}
