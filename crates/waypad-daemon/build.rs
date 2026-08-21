//! Stamps the Windows executable with its icon and version metadata.
//!
//! The icon matters more than it looks: it is what the tray shows, what the
//! installer puts on the Start menu, and what Explorer draws next to the
//! executable. Without it all three fall back to the generic application icon.

fn main() {
    // Only Windows executables carry resources; on Linux this is a no-op and
    // the crate must still build without the toolchain that reads them.
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
