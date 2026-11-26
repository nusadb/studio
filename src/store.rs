//! The in-memory list of saved servers.
//!
//! The store owns both the servers and the identifiers the UI addresses them by. Identifiers
//! are handed out from a counter rather than being list positions, so filtering the list or
//! removing an entry can never make a pending callback act on the wrong server.
//!
//! The list is saved as JSON in the user's config directory. Passwords are never written: they
//! live in memory for the session only, until the OS keychain integration lands.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{ConnectionParams, ConnectionStatus, Environment, ServerProfile, ValidServer};

/// The on-disk form of one saved server — everything except the password.
#[derive(Debug, Serialize, Deserialize)]
struct SavedServer {
    name: String,
    environment: String,
    host: String,
    port: u16,
    user: String,
    database: String,
    #[serde(default)]
    tls: bool,
    #[serde(default)]
    ca_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_used: Option<u64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    metrics: String,
    /// The password is in the system keychain (never here).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    remember_password: bool,
}

/// Where the server list lives: `<config dir>/nusadb-studio/servers.json`.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    Some(base.join("nusadb-studio").join("servers.json"))
}

/// Saved servers.
#[derive(Debug, Default)]
pub struct ServerStore {
    servers: Vec<ServerProfile>,
    next_id: u32,
}

impl ServerStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A store seeded with the default local engine target.
    ///
    /// This is a convenience, not fabricated data: it is the address `nusadb-server` listens
    /// on by default, and the user can remove it like any other entry.
    #[must_use]
    pub fn with_local_default() -> Self {
        let mut store = Self::new();
        store.insert(ValidServer {
            name: "local".to_owned(),
            environment: crate::model::Environment::Dev,
            params: crate::model::ConnectionParams::default(),
            metrics: String::new(),
            remember: false,
        });
        store
    }

    /// Read a saved list. A missing file is not an error: it is a first start.
    ///
    /// # Errors
    ///
    /// Returns the I/O or parse error for a file that exists but cannot be used, so the caller
    /// can say so instead of silently starting empty and overwriting it.
    pub fn load(path: &Path) -> io::Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let saved: Vec<SavedServer> = serde_json::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut store = Self::new();
        for server in saved {
            let last_used = server.last_used;
            let id = store.insert(ValidServer {
                name: server.name,
                environment: Environment::from_tag(&server.environment),
                params: ConnectionParams {
                    host: server.host,
                    port: server.port,
                    user: server.user,
                    database: server.database,
                    password: String::new(),
                    tls: server.tls,
                    ca_path: server.ca_path,
                },
                metrics: server.metrics,
                remember: server.remember_password,
            });
            if let Some(profile) = store.servers.iter_mut().find(|p| p.id == id) {
                profile.last_used = last_used;
            }
        }
        Ok(Some(store))
    }

    /// Write the list to `path`, creating its directory. Passwords are left out.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the directory or file cannot be written.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let saved: Vec<SavedServer> = self
            .servers
            .iter()
            .map(|p| SavedServer {
                name: p.name.clone(),
                environment: p.environment.as_tag().to_owned(),
                host: p.params.host.clone(),
                port: p.params.port,
                user: p.params.user.clone(),
                database: p.params.database.clone(),
                tls: p.params.tls,
                ca_path: p.params.ca_path.clone(),
                last_used: p.last_used,
                metrics: p.metrics.clone(),
                remember_password: p.remember,
            })
            .collect();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(&saved)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        // Write-then-rename, so a crash mid-write never leaves a half file behind.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Every server, ignoring the filter.
    pub fn all(&self) -> impl Iterator<Item = &ServerProfile> {
        self.servers.iter()
    }

    /// Look up a server by identity.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<&ServerProfile> {
        self.servers.iter().find(|profile| profile.id == id)
    }

    /// Append a validated server and return the identity it was given.
    pub fn insert(&mut self, valid: ValidServer) -> u32 {
        self.next_id = self.next_id.saturating_add(1);
        let id = self.next_id;
        self.servers.push(ServerProfile::from_valid(id, valid));
        id
    }

    /// Overwrite the server `id` with `valid`, keeping its identity and connection status.
    ///
    /// Returns `false` when no server has that identity.
    pub fn update(&mut self, id: u32, valid: ValidServer) -> bool {
        let Some(profile) = self.servers.iter_mut().find(|profile| profile.id == id) else {
            return false;
        };
        profile.name = valid.name;
        profile.environment = valid.environment;
        profile.params = valid.params;
        profile.metrics = valid.metrics;
        profile.remember = valid.remember;
        true
    }

    /// Fill in a password read from the keychain. Returns `false` for an unknown `id`.
    pub fn set_password(&mut self, id: u32, password: String) -> bool {
        let Some(profile) = self.servers.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        profile.params.password = password;
        true
    }

    /// Drop the server `id`. Returns `false` when no server has that identity.
    pub fn remove(&mut self, id: u32) -> bool {
        let before = self.servers.len();
        self.servers.retain(|profile| profile.id != id);
        self.servers.len() != before
    }

    /// Note that a session to `id` opened at `when` (Unix seconds).
    pub fn touch(&mut self, id: u32, when: u64) {
        if let Some(profile) = self.servers.iter_mut().find(|profile| profile.id == id) {
            profile.last_used = Some(when);
        }
    }

    /// Record a new connection state for the server `id`.
    ///
    /// Returns `false` when no server has that identity — which is the normal outcome for a
    /// probe that finishes after the user removed the server.
    pub fn set_status(&mut self, id: u32, status: ConnectionStatus) -> bool {
        let Some(profile) = self.servers.iter_mut().find(|profile| profile.id == id) else {
            return false;
        };
        profile.status = status;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConnectionParams, Environment, ServerDraft};

    fn valid(name: &str, host: &str) -> ValidServer {
        ServerDraft {
            name: name.to_owned(),
            host: host.to_owned(),
            ..ServerDraft::default()
        }
        .validate()
        .unwrap()
    }

    #[test]
    fn a_fresh_store_is_empty_and_the_seeded_one_holds_the_local_target() {
        assert_eq!(ServerStore::new().all().count(), 0);

        let seeded = ServerStore::with_local_default();
        let only = seeded.all().next().unwrap();
        assert_eq!(seeded.all().count(), 1);
        assert_eq!(only.params, ConnectionParams::default());
    }

    #[test]
    fn identities_are_unique_and_never_reused_after_removal() {
        let mut store = ServerStore::new();
        let first = store.insert(valid("a", "10.0.0.1"));
        let second = store.insert(valid("b", "10.0.0.2"));
        assert_ne!(first, second);

        assert!(store.remove(first));
        let third = store.insert(valid("c", "10.0.0.3"));
        assert_ne!(third, first, "a removed identity must not come back");
        assert_ne!(third, second);
    }

    #[test]
    fn removing_an_unknown_identity_is_a_no_op() {
        let mut store = ServerStore::new();
        let id = store.insert(valid("a", "10.0.0.1"));
        assert!(!store.remove(id + 999));
        assert_eq!(store.all().count(), 1);
    }

    #[test]
    fn update_keeps_identity_and_status() {
        let mut store = ServerStore::new();
        let id = store.insert(valid("a", "10.0.0.1"));
        assert!(store.set_status(id, ConnectionStatus::Connected));

        let mut replacement = valid("renamed", "10.0.0.9");
        replacement.environment = Environment::Prod;
        assert!(store.update(id, replacement));

        let profile = store.get(id).unwrap();
        assert_eq!(profile.id, id);
        assert_eq!(profile.name, "renamed");
        assert_eq!(profile.params.host, "10.0.0.9");
        assert_eq!(profile.environment, Environment::Prod);
        assert_eq!(
            profile.status,
            ConnectionStatus::Connected,
            "editing the address must not silently claim the session dropped"
        );
    }

    #[test]
    fn updating_an_unknown_identity_reports_failure() {
        let mut store = ServerStore::new();
        assert!(!store.update(42, valid("ghost", "10.0.0.1")));
        assert!(!store.set_status(42, ConnectionStatus::Error));
    }

    #[test]
    fn the_list_round_trips_through_disk_without_passwords() {
        let dir = std::env::temp_dir().join(format!("nusadb-studio-test-{}", std::process::id()));
        let path = dir.join("servers.json");
        let mut store = ServerStore::new();
        let mut secret = valid("prod", "10.0.0.1");
        secret.params.password = "hunter2".to_owned();
        secret.environment = Environment::Prod;
        let id = store.insert(secret);
        store.touch(id, 1_700_000_000);
        store.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("hunter2"),
            "a password must never reach the disk"
        );

        let loaded = ServerStore::load(&path).unwrap().unwrap();
        let only = loaded.all().next().unwrap();
        assert_eq!(only.name, "prod");
        assert_eq!(only.environment, Environment::Prod);
        assert!(only.params.password.is_empty());
        assert_eq!(only.last_used, Some(1_700_000_000));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remembering_a_password_stores_only_the_flag() {
        let dir =
            std::env::temp_dir().join(format!("nusadb-studio-remember-{}", std::process::id()));
        let path = dir.join("servers.json");
        let mut store = ServerStore::new();
        let mut kept = valid("kept", "10.0.0.2");
        kept.params.password = "correct horse".to_owned();
        kept.remember = true;
        let kept = store.insert(kept);
        let plain = store.insert(valid("plain", "10.0.0.3"));
        store.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("correct horse"));
        assert_eq!(
            text.matches("remember_password").count(),
            1,
            "only set flags are written"
        );

        let mut loaded = ServerStore::load(&path).unwrap().unwrap();
        assert!(loaded.get(kept).unwrap().remember);
        assert!(!loaded.get(plain).unwrap().remember);
        assert!(loaded.get(kept).unwrap().params.password.is_empty());
        assert!(loaded.set_password(kept, "from keychain".to_owned()));
        assert_eq!(loaded.get(kept).unwrap().params.password, "from keychain");
        assert!(!loaded.set_password(999, String::new()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_a_first_start_not_an_error() {
        let path = std::env::temp_dir().join("nusadb-studio-definitely-missing.json");
        assert!(ServerStore::load(&path).unwrap().is_none());
    }
}
