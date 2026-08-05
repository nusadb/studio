//! NusaDB Studio — native desktop database-management GUI (Rust + Slint).
//!
//! This file is the composition root and nothing else: it builds the window, the server
//! store, and the connection probe, hands them to the controller, and runs the event loop.
//! Swapping in a different connection probe is a one-line change here.
//!
//! Module map:
//! - [`model`] — domain types and form validation (no Slint, no driver)
//! - [`store`] — the saved-server list and the identities the UI addresses them by
//! - [`tree`] — the object hierarchy, flattened into navigator rows
//! - [`probe`] — the port a connection test goes through
//! - [`driver`] — the adapter that fulfils that port over `nusadb-libnusa`
//! - [`sql_highlight`] — token classification for the editor
//! - [`controller`] — the adapter binding the window to the modules above
//! - [`ui`] — bindings generated from `ui/app.slint`
//!
//! The `#![windows_subsystem = "windows"]` attribute keeps a console window from opening
//! alongside the GUI on Windows release builds; debug builds keep the console for logs.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod controller;
mod driver;
mod model;
mod probe;
mod sql_highlight;
mod store;
mod tree;
mod ui;

use std::rc::Rc;

// `run()` / `new()` / `as_weak()` come from this trait; it must be in scope because the
// generated `AppWindow` lives in the `ui` submodule rather than at the crate root.
use slint::ComponentHandle;

use crate::controller::Controller;
use crate::driver::DriverProbe;
use crate::probe::{ConnectionProbe, UnavailableProbe};
use crate::store::ServerStore;

fn main() -> Result<(), slint::PlatformError> {
    let window = ui::AppWindow::new()?;

    // If the driver's runtime cannot start there is no way to reach a server, so Studio still
    // opens — with a probe that says exactly that, rather than a Test Connection button that
    // fails in some unexplained way.
    let probe: Box<dyn ConnectionProbe> = match DriverProbe::new() {
        Ok(driver) => Box::new(driver),
        Err(error) => Box::new(UnavailableProbe::new(format!(
            "the connection driver could not start ({error})"
        ))),
    };

    let controller = Rc::new(Controller::new(ServerStore::with_local_default(), probe));
    controller::install(&window, controller);

    window.run()
}
