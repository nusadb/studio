//! Compile the Slint UI (`ui/app.slint`) into generated Rust consumed via
//! `slint::include_modules!()` in `main.rs`.

fn main() {
    slint_build::compile("ui/app.slint").expect("compile ui/app.slint");

    // The NusaDB driver's version, as locked, for Settings → About.
    println!("cargo:rerun-if-changed=Cargo.lock");
    let driver = std::fs::read_to_string("Cargo.lock")
        .ok()
        .and_then(|lock| {
            let at = lock.find("name = \"nusadb\"")?;
            let rest = &lock[at..];
            let v = rest.find("version = \"")? + "version = \"".len();
            rest[v..].split('"').next().map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=NUSADB_DRIVER_VERSION={driver}");

    // The Windows executable carries its own icon (Explorer, taskbar, Alt-Tab) and version info.
    // Decided by the target, not by the machine running the build.
    println!("cargo:rerun-if-changed=packaging/icons/nusadb-studio.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("packaging/icons/nusadb-studio.ico")
            .set("ProductName", "NusaDB Studio")
            .set("FileDescription", "NusaDB Studio");
        resource.compile().expect("embed the Windows icon");
    }
}
