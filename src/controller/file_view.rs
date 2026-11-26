//! Opening and saving `.sql` files in the Query editor's tabs.

use std::path::{Path, PathBuf};

use super::{AppWindow, Controller, Pending};

/// Largest script opened into the editor.
const MAX_SCRIPT_BYTES: u64 = 8 * 1024 * 1024;

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn pick_open() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("NUSADB_STUDIO_OPEN_FILE") {
        return Some(PathBuf::from(path));
    }
    rfd::FileDialog::new()
        .set_title("Open SQL script")
        .add_filter("SQL", &["sql"])
        .add_filter("All files", &["*"])
        .pick_file()
}

fn pick_save(default_name: &str) -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::var_os("NUSADB_STUDIO_EXPORT_DIR") {
        return Some(PathBuf::from(dir).join(default_name));
    }
    rfd::FileDialog::new()
        .set_title("Save SQL script")
        .set_file_name(default_name)
        .add_filter("SQL", &["sql"])
        .save_file()
}

impl Controller {
    /// Ctrl+O / "Open…": a script into a new tab (or the empty one in front).
    pub(super) fn file_open(&self, window: &AppWindow) {
        if let Some(path) = pick_open() {
            self.open_path(window, path);
        }
    }

    /// Scripts named on the command line — "Open with NusaDB Studio" from a file manager.
    pub(super) fn open_from_args(&self, window: &AppWindow) {
        let paths: Vec<PathBuf> = std::env::args_os()
            .skip(1)
            .map(PathBuf::from)
            .filter(|p| !p.to_string_lossy().starts_with('-'))
            .collect();
        for path in paths {
            self.open_path(window, path);
        }
    }

    fn open_path(&self, window: &AppWindow, path: PathBuf) {
        // Already open: just bring its tab forward.
        let open = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.path.as_deref() == Some(path.as_path()))
            .map(|t| t.id);
        if let Some(id) = open {
            self.activate_tab(window, id);
            return;
        }
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_SCRIPT_BYTES {
            self.toast(
                window,
                "That file is larger than 8 MiB, too large to edit here.",
                true,
            );
            return;
        }
        let text = match std::fs::read(&path).map(String::from_utf8) {
            Ok(Ok(text)) => text,
            Ok(Err(_)) => {
                self.toast(window, "The file is not UTF-8 text.", true);
                return;
            },
            Err(e) => {
                self.toast(
                    window,
                    &format!("Could not open {}: {e}", path.display()),
                    true,
                );
                return;
            },
        };
        self.new_tab(window, Some(&file_name(&path)), &text);
        let id = self.active_tab.get();
        if let Some(tab) = self.tabs.borrow_mut().iter_mut().find(|t| t.id == id) {
            tab.path = Some(path);
            tab.saved = Some(text);
        }
        self.publish_tabs(window);
    }

    /// Ctrl+S / "Save": write the tab's script to its file, asking where the first time.
    pub(super) fn file_save(&self, window: &AppWindow, save_as: bool) {
        let id = self.active_tab.get();
        let (path, title) = {
            let tabs = self.tabs.borrow();
            let Some(tab) = tabs.iter().find(|t| t.id == id) else {
                return;
            };
            (tab.path.clone(), tab.title.clone())
        };
        let path = match path.filter(|_| !save_as) {
            Some(path) => path,
            None => {
                let name = if title.ends_with(".sql") {
                    title
                } else {
                    format!("{title}.sql")
                };
                let Some(path) = pick_save(&name.replace(['/', '\\'], "-")) else {
                    return;
                };
                path
            },
        };
        let text = self.sql.borrow().clone();
        if let Err(e) = std::fs::write(&path, &text) {
            self.toast(
                window,
                &format!("Could not save {}: {e}", path.display()),
                true,
            );
            return;
        }
        if let Some(tab) = self.tabs.borrow_mut().iter_mut().find(|t| t.id == id) {
            tab.title = file_name(&path);
            tab.path = Some(path.clone());
            tab.saved = Some(text);
        }
        self.toast(window, &format!("Saved {}", path.display()), false);
        self.publish_tabs(window);
    }

    /// Whether tab `id` has a file whose text differs from the editor's.
    pub(super) fn tab_dirty(&self, id: u32) -> bool {
        let tabs = self.tabs.borrow();
        let Some(tab) = tabs.iter().find(|t| t.id == id) else {
            return false;
        };
        let Some(saved) = &tab.saved else {
            return false;
        };
        if id == self.active_tab.get() {
            *self.sql.borrow() != *saved
        } else {
            tab.sql != *saved
        }
    }

    /// Closing a tab with unsaved file changes asks first.
    pub(super) fn close_tab_checked(&self, window: &AppWindow, id: u32) {
        let edits = self.tab_result_edits(id);
        if edits > 0 {
            let name = self
                .tabs
                .borrow()
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.title.clone())
                .unwrap_or_default();
            let mut reasons = vec![format!(
                "{edits} edited row{} in its results {} not saved.",
                if edits == 1 { "" } else { "s" },
                if edits == 1 { "is" } else { "are" }
            )];
            if self.tab_dirty(id) {
                reasons.push("Its script has unsaved changes too.".to_owned());
            }
            self.ask(
                window,
                Pending::CloseTab(id),
                &format!("Close {name}?"),
                &reasons,
                "Close without saving",
            );
            return;
        }
        if self.tab_dirty(id) {
            let name = self
                .tabs
                .borrow()
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.title.clone())
                .unwrap_or_default();
            self.ask(
                window,
                Pending::CloseTab(id),
                &format!("Close {name} without saving?"),
                &["The changes since it was last saved will be lost.".to_owned()],
                "Close without saving",
            );
            return;
        }
        self.close_tab(window, id);
    }
}
