//! NusaDB Studio — native desktop database-management GUI (Rust + Slint).
//!
//! This file is the composition root and nothing else: it builds the window, the server
//! store, and the connection probe, hands them to the controller, and runs the event loop.
//! Swapping in a different connection probe is a one-line change here.
//!
//! Module map:
//! - [`model`] — domain types and form validation (no Slint, no driver)
//! - [`store`] — the saved-server list, its identities, and its file on disk
//! - [`tree`] — the object hierarchy, flattened into navigator rows
//! - [`probe`] — the port a connection test goes through
//! - [`backend`] — the port live sessions go through, and the data they return
//! - [`driver`] — the adapter that fulfils both ports over the `nusadb` crate
//! - [`sql_highlight`] — token classification for the editor
//! - [`sql_split`] — cutting a script into statements
//! - [`complete`] — editor completions
//! - [`designer`] — the table designer and the DDL it produces
//! - [`ddl`] — reconstructing an object's DDL from its description
//! - [`edits`] — changes staged in the Data grid, and the SQL that applies them
//! - [`risk`] — which scripts must be confirmed before they run
//! - [`export`] — CSV / JSON files and clipboard text
//! - [`controller`] — the adapter binding the window to the modules above
//! - [`ui`] — bindings generated from `ui/app.slint`
//!
//! The `#![windows_subsystem = "windows"]` attribute keeps a console window from opening
//! alongside the GUI on Windows release builds; debug builds keep the console for logs.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backend;
mod chart;
mod complete;
mod controller;
mod ddl;
mod designer;
mod detail;
mod driver;
mod edits;
mod erd;
mod export;
mod import;
mod keychain;
mod metrics;
mod model;
mod pca;
mod pivot;
mod probe;
mod profile;
mod result_source;
mod risk;
mod security;
mod settings;
mod sql_format;
mod sql_highlight;
mod sql_split;
mod stats;
mod store;
mod tree;
mod ui;
mod vector;

use std::rc::Rc;

// `run()` / `new()` / `as_weak()` come from this trait; it must be in scope because the
// generated `AppWindow` lives in the `ui` submodule rather than at the crate root.
use slint::ComponentHandle;

use crate::backend::{Backend, OfflineBackend};
use crate::controller::Controller;
use crate::driver::DriverProbe;
use crate::probe::{ConnectionProbe, UnavailableProbe};
use crate::store::ServerStore;

fn main() -> Result<(), slint::PlatformError> {
    let window = ui::AppWindow::new()?;
    {
        // Shortcuts and their labels follow the platform: ⌘ on a Mac, Ctrl elsewhere.
        // NUSADB_STUDIO_PLATFORM=mac shows the Mac set on another system (for testing).
        let mac = cfg!(target_os = "macos")
            || std::env::var("NUSADB_STUDIO_PLATFORM").is_ok_and(|p| p == "mac");
        window.global::<ui::Keys>().set_mac(mac);
    }

    // If the driver's runtime cannot start there is no way to reach a server, so Studio still
    // opens — with ports that say exactly that, rather than buttons that fail in some
    // unexplained way.
    let (probe, backend): (Box<dyn ConnectionProbe>, Box<dyn Backend>) = match DriverProbe::new() {
        Ok(driver) => {
            let driver = Rc::new(driver);
            (Box::new(Rc::clone(&driver)), Box::new(driver))
        },
        Err(error) => {
            let reason = format!("the connection driver could not start ({error})");
            (
                Box::new(UnavailableProbe::new(reason.clone())),
                Box::new(OfflineBackend::new(reason)),
            )
        },
    };

    // The saved list, or the local engine on a first start. A list that exists but cannot be
    // read is not overwritten: Studio runs without saving rather than lose the user's servers.
    let mut path = store::default_path();
    let store = match path.as_deref().map(ServerStore::load) {
        Some(Ok(Some(store))) => store,
        Some(Ok(None)) | None => ServerStore::with_local_default(),
        Some(Err(error)) => {
            eprintln!("nusadb-studio: cannot read the saved server list: {error}");
            path = None;
            ServerStore::with_local_default()
        },
    };

    // Preferences: light theme and the other defaults unless the user changed them.
    let settings_path = path.as_deref().map(settings::path_beside);
    let chosen = settings_path
        .as_deref()
        .map(settings::load)
        .unwrap_or_default();
    window.global::<ui::Theme>().set_dark(chosen.dark);
    #[allow(clippy::cast_precision_loss)]
    window
        .global::<ui::Metrics>()
        .set_text_code(chosen.editor_font as f32);
    window.set_setting_page_size(i32::try_from(chosen.page_size).unwrap_or(200));
    window.set_setting_details(chosen.details_panel);
    window.set_inspector_visible(chosen.details_panel);
    window.set_setting_result_rows(i32::try_from(chosen.result_rows).unwrap_or(1_000));
    window.set_setting_history(i32::try_from(chosen.history).unwrap_or(100));
    window.set_editor_share(chosen.editor_share);
    window.set_about_version(env!("CARGO_PKG_VERSION").into());
    window.set_about_driver(env!("NUSADB_DRIVER_VERSION").into());
    window.set_about_folder(
        path.as_deref()
            .and_then(std::path::Path::parent)
            .map(|d| d.display().to_string())
            .unwrap_or_default()
            .into(),
    );

    let controller = Rc::new(Controller::new(store, path, probe, backend));
    controller.set_page_size(chosen.page_size);
    controller.set_limits(chosen.result_rows, chosen.history);
    {
        // Every change is saved at once, from what the window shows.
        let weak = window.as_weak();
        let handler = Rc::clone(&controller);
        let save = Rc::new(move || {
            let Some(w) = weak.upgrade() else { return };
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let current = settings::Settings {
                dark: w.global::<ui::Theme>().get_dark(),
                editor_font: w.global::<ui::Metrics>().get_text_code().round() as u32,
                page_size: usize::try_from(w.get_setting_page_size()).unwrap_or(200),
                details_panel: w.get_setting_details(),
                result_rows: usize::try_from(w.get_setting_result_rows()).unwrap_or(1_000),
                history: usize::try_from(w.get_setting_history()).unwrap_or(100),
                editor_share: w.get_editor_share(),
            }
            .sane();
            w.set_inspector_visible(current.details_panel);
            handler.set_page_size(current.page_size);
            handler.set_limits(current.result_rows, current.history);
            if let Some(p) = settings_path.as_deref() {
                settings::save(p, &current);
            }
        });
        let on_theme = Rc::clone(&save);
        window.on_theme_changed(move |_| on_theme());
        let on_split = Rc::clone(&save);
        window.on_split_moved(move |_| on_split());
        window.on_settings_changed(move || save());
    }
    controller::install(&window, controller);

    window.run()
}
