//! The in-memory list of saved servers.
//!
//! The store owns both the servers and the identifiers the UI addresses them by. Identifiers
//! are handed out from a counter rather than being list positions, so filtering the list or
//! removing an entry can never make a pending callback act on the wrong server.
//!
//! Persisting the list to disk (and the password to the OS keychain) is not implemented yet;
//! it will sit behind this same API.

use crate::model::{ConnectionStatus, ServerProfile, ValidServer};

/// Saved servers plus the navigator's current search filter.
#[derive(Debug, Default)]
pub struct ServerStore {
    servers: Vec<ServerProfile>,
    next_id: u32,
    filter: String,
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
        });
        store
    }

    /// Set the search filter; matching is case-insensitive.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.trim().to_lowercase();
    }

    /// The servers passing the current filter, in insertion order.
    pub fn visible(&self) -> impl Iterator<Item = &ServerProfile> {
        self.servers
            .iter()
            .filter(|profile| profile.matches(&self.filter))
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
        true
    }

    /// Drop the server `id`. Returns `false` when no server has that identity.
    pub fn remove(&mut self, id: u32) -> bool {
        let before = self.servers.len();
        self.servers.retain(|profile| profile.id != id);
        self.servers.len() != before
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
        assert_eq!(ServerStore::new().visible().count(), 0);

        let seeded = ServerStore::with_local_default();
        let only = seeded.visible().next().unwrap();
        assert_eq!(seeded.visible().count(), 1);
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
        assert_eq!(store.visible().count(), 1);
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
    fn the_filter_spans_name_host_and_database_and_ignores_case() {
        let mut store = ServerStore::new();
        store.insert(valid("prod-jakarta", "10.0.2.15"));
        store.insert(valid("local-dev", "127.0.0.1"));

        store.set_filter("  JAKARTA ");
        let hits: Vec<_> = store
            .visible()
            .map(|profile| profile.name.clone())
            .collect();
        assert_eq!(hits, ["prod-jakarta"]);

        store.set_filter("127.0");
        assert_eq!(store.visible().count(), 1);

        store.set_filter("");
        assert_eq!(store.visible().count(), 2);
    }

    #[test]
    fn filtering_does_not_disturb_identities() {
        let mut store = ServerStore::new();
        let jakarta = store.insert(valid("prod-jakarta", "10.0.2.15"));
        store.insert(valid("local-dev", "127.0.0.1"));

        store.set_filter("local");
        assert_eq!(store.visible().count(), 1);
        assert_eq!(store.get(jakarta).unwrap().name, "prod-jakarta");
        assert!(
            store.remove(jakarta),
            "a filtered-out server is still addressable"
        );
    }
}
