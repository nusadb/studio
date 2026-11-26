//! Preferences that are not servers (the Settings screen and the theme switch). Kept in
//! `settings.json` next to `servers.json`. A missing or unreadable file, or a missing field,
//! means the default.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// `true` once the user picked the dark theme.
    #[serde(default)]
    pub dark: bool,
    /// SQL editor font size, in pixels.
    #[serde(default = "default_font")]
    pub editor_font: u32,
    /// Rows the Data screen loads per page.
    #[serde(default = "default_page")]
    pub page_size: usize,
    /// Show the details panel at start-up.
    #[serde(default)]
    pub details_panel: bool,
    /// Rows a query result keeps in the grid.
    #[serde(default = "default_result_rows")]
    pub result_rows: usize,
    /// Scripts the History tab keeps.
    #[serde(default = "default_history")]
    pub history: usize,
    /// Share of the Query screen's height the editor takes; the results get the rest.
    #[serde(default = "default_editor_share")]
    pub editor_share: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dark: false,
            editor_font: default_font(),
            page_size: default_page(),
            details_panel: false,
            result_rows: default_result_rows(),
            history: default_history(),
            editor_share: default_editor_share(),
        }
    }
}

const fn default_result_rows() -> usize {
    1_000
}

const fn default_history() -> usize {
    100
}

const fn default_editor_share() -> f32 {
    0.42
}

const fn default_font() -> u32 {
    13
}

const fn default_page() -> usize {
    200
}

impl Settings {
    /// Clamp values a hand-edited file could set out of range.
    #[must_use]
    pub fn sane(mut self) -> Self {
        self.editor_font = self.editor_font.clamp(10, 24);
        self.page_size = self.page_size.clamp(10, 10_000);
        self.result_rows = self.result_rows.clamp(100, 100_000);
        self.history = self.history.clamp(10, 5_000);
        if !self.editor_share.is_finite() {
            self.editor_share = default_editor_share();
        }
        self.editor_share = self.editor_share.clamp(0.12, 0.85);
        self
    }
}

/// Where the settings live, beside the saved servers.
#[must_use]
pub fn path_beside(servers: &Path) -> PathBuf {
    servers.with_file_name("settings.json")
}

/// The saved settings, or the defaults.
#[must_use]
pub fn load(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .unwrap_or_default()
        .sane()
}

/// Write `settings`; a failure only costs the preference, so it is reported and ignored.
pub fn save(path: &Path, settings: &Settings) {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(settings).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    };
    if let Err(error) = write() {
        eprintln!("nusadb-studio: cannot save settings: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::{Settings, load, path_beside, save};

    #[test]
    fn the_theme_choice_survives_a_restart_and_defaults_to_light() {
        let dir =
            std::env::temp_dir().join(format!("nusadb-studio-settings-{}", std::process::id()));
        let path = path_beside(&dir.join("servers.json"));
        assert!(!load(&path).dark, "no file: light");
        save(
            &path,
            &Settings {
                dark: true,
                page_size: 500,
                ..Settings::default()
            },
        );
        assert!(load(&path).dark);
        assert_eq!(load(&path).page_size, 500);
        assert_eq!(load(&path).editor_font, 13, "missing field: default");
        std::fs::write(&path, "not json").unwrap();
        assert!(!load(&path).dark, "unreadable: light");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
