//! Embed the Windows icon and version resource in `illness.exe`.
//!
//! The resource compiler is only invoked for Windows targets; other targets build
//! exactly as before.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/illness.ico");

    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            embed_windows_resources();
        }
    }
}

#[cfg(windows)]
fn embed_windows_resources() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is set by Cargo for build scripts");
    let icon = std::path::Path::new(&manifest_dir)
        .join("../../assets/illness.ico")
        .to_string_lossy()
        .into_owned();

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(&icon);
    resource.set("ProductName", "Illness");
    resource.set("FileDescription", "Illness");
    resource.set("ProductVersion", env!("CARGO_PKG_VERSION"));
    resource.set("FileVersion", env!("CARGO_PKG_VERSION"));
    resource.set("CompanyName", "Simulp");
    resource.set("LegalCopyright", "Copyright (c) 2026 Simulp");
    resource.set("OriginalFilename", "illness.exe");

    resource
        .compile()
        .expect("failed to embed the Windows icon and version resource");
}
