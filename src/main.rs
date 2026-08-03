//! NusaDB Studio — native desktop GUI (Rust + Slint). Task G1 scaffold: window shell
//! with the design palette (Plan 19). Driver wiring + navigator come in G2/G3.
//!
//! The `#![windows_subsystem = "windows"]` attribute keeps a console window from opening
//! alongside the GUI on Windows release builds; debug builds keep the console for logs.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Slint's `.slint` → Rust codegen uses `unwrap()` internally, so it is scoped into its own
// module that opts out of the crate's `unwrap_used = "deny"` lint; our own code stays under it.
mod ui {
    #![allow(clippy::unwrap_used)]
    slint::include_modules!();
}

// `run()`/`new()` come from this trait; it must be in scope because the generated
// `AppWindow` lives in the `ui` submodule rather than at the crate root.
use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    let app = ui::AppWindow::new()?;
    app.run()
}
