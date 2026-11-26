//! Pure domain types for Studio.
//!
//! Nothing in this module knows about Slint or about the wire driver: a [`ServerProfile`] is
//! what the app means by "a server the user saved", and a [`ServerDraft`] is the unvalidated
//! text the connection form produces. Keeping both free of the UI toolkit and of the network
//! client is what makes the rules below testable without a window or a running server.

use std::fmt;

/// Default connect target for a local server.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Default port of the Nusa Wire Protocol listener.
pub const DEFAULT_PORT: u16 = 5678;
/// Default bootstrap user.
pub const DEFAULT_USER: &str = "nusadb-root";
/// Default database opened on connect.
pub const DEFAULT_DATABASE: &str = "nusadb";

/// Where a saved server sits in the user's environment ladder; drives its colour tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Environment {
    /// Production — the tag the UI paints most loudly.
    Prod,
    /// Pre-production.
    Staging,
    /// Local or scratch server.
    #[default]
    Dev,
}

impl Environment {
    /// The tag the view layer maps onto a colour.
    #[must_use]
    pub const fn as_tag(self) -> &'static str {
        match self {
            Self::Prod => "prod",
            Self::Staging => "staging",
            Self::Dev => "dev",
        }
    }

    /// Parse a tag produced by [`Environment::as_tag`].
    ///
    /// An unrecognised tag falls back to [`Environment::Dev`] — the interpretation that makes
    /// the UI under-state rather than over-state how sensitive a server is.
    #[must_use]
    pub fn from_tag(tag: &str) -> Self {
        match tag {
            "prod" => Self::Prod,
            "staging" => Self::Staging,
            _ => Self::Dev,
        }
    }
}

/// Live state of a saved server's connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    /// No session open.
    #[default]
    Disconnected,
    /// A connection attempt is in flight.
    Connecting,
    /// Authenticated and ready.
    Connected,
    /// The last attempt failed.
    Error,
}

impl ConnectionStatus {
    /// The tag `StatusDot` in the UI layer maps onto a colour.
    #[must_use]
    pub const fn as_tag(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Error => "error",
        }
    }
}

/// Everything needed to reach one server.
///
/// Shared by a saved profile and by an unsaved draft being tested, so the connection probe
/// port has exactly one input type regardless of where the parameters came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionParams {
    /// Hostname or address of the listener.
    pub host: String,
    /// TCP port of the listener.
    pub port: u16,
    /// Role to authenticate as.
    pub user: String,
    /// Database to open on connect (the engine is one engine per database).
    pub database: String,
    /// SCRAM password; empty means trust-on-startup.
    pub password: String,
    /// Whether to negotiate TLS.
    pub tls: bool,
    /// Path to the PEM bundle holding the CA that signed the server's certificate.
    ///
    /// Required when [`tls`](Self::tls) is set: the driver verifies the server against a root
    /// store built from this file. There is deliberately no "trust anything" fallback — a TLS
    /// switch that quietly accepts any certificate is worse than no switch at all.
    pub ca_path: String,
}

impl ConnectionParams {
    /// `host:port`, as shown on server rows and cards.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// `user · database`, the secondary line on a server card.
    #[must_use]
    pub fn detail(&self) -> String {
        format!("{} · {}", self.user, self.database)
    }
}

impl Default for ConnectionParams {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
            user: DEFAULT_USER.to_owned(),
            database: DEFAULT_DATABASE.to_owned(),
            password: String::new(),
            tls: false,
            ca_path: String::new(),
        }
    }
}

/// A draft that passed validation: a named, well-formed server that has not been given an
/// identity yet.
///
/// Only [`ServerDraft::validate`] can produce one, so an invalid server can never reach the
/// store — the type, not a runtime check at the call site, is what enforces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidServer {
    /// Display name.
    pub name: String,
    /// Environment tag.
    pub environment: Environment,
    /// Connection parameters.
    pub params: ConnectionParams,
    /// The server's Prometheus endpoint (`--metrics-listen`), or empty.
    pub metrics: String,
    /// Keep the password in the system keychain.
    pub remember: bool,
}

/// A server the user saved, as held by the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerProfile {
    /// Stable identity assigned by the store.
    pub id: u32,
    /// Display name.
    pub name: String,
    /// Environment tag.
    pub environment: Environment,
    /// Last known connection state.
    pub status: ConnectionStatus,
    /// Connection parameters.
    pub params: ConnectionParams,
    /// When a session to it last opened, in Unix seconds.
    pub last_used: Option<u64>,
    /// The server's Prometheus endpoint (`--metrics-listen`), or empty.
    pub metrics: String,
    /// The password is remembered in the system keychain.
    pub remember: bool,
}

impl ServerProfile {
    /// Give a validated draft the identity `id`.
    #[must_use]
    pub fn from_valid(id: u32, valid: ValidServer) -> Self {
        Self {
            id,
            name: valid.name,
            environment: valid.environment,
            status: ConnectionStatus::default(),
            params: valid.params,
            last_used: None,
            metrics: valid.metrics,
            remember: valid.remember,
        }
    }
}

/// Why a draft could not become a [`ValidServer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {
    /// The display name was blank.
    NameEmpty,
    /// The host was blank.
    HostEmpty,
    /// The port was not a number in `1..=65535`.
    PortInvalid,
    /// The user was blank.
    UserEmpty,
    /// The database was blank.
    DatabaseEmpty,
    /// TLS was requested without naming a CA bundle to verify the server against.
    TlsWithoutCa,
    /// The metrics address was not `host:port` or an `http://` URL.
    MetricsInvalid,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NameEmpty => "Name cannot be empty.",
            Self::HostEmpty => "Host cannot be empty.",
            Self::PortInvalid => "Port must be a number between 1 and 65535.",
            Self::UserEmpty => "User cannot be empty.",
            Self::DatabaseEmpty => "Database cannot be empty.",
            Self::TlsWithoutCa => "TLS needs a CA certificate file to verify the server.",
            Self::MetricsInvalid => {
                "Metrics address must be host:port or an http:// URL (the --metrics-listen address)."
            },
        };
        f.write_str(message)
    }
}

impl std::error::Error for ValidationError {}

/// The raw contents of the connection form, before validation.
///
/// `port` is a `String` because that is what a text field holds; turning it into a `u16` is
/// exactly the job of [`ServerDraft::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDraft {
    /// Display name.
    pub name: String,
    /// Hostname or address.
    pub host: String,
    /// Port, as typed.
    pub port: String,
    /// Role to authenticate as.
    pub user: String,
    /// SCRAM password.
    pub password: String,
    /// Database to open.
    pub database: String,
    /// Whether to negotiate TLS.
    pub tls: bool,
    /// Path to the CA bundle, as typed.
    pub ca_path: String,
    /// Environment tag.
    pub environment: Environment,
    /// Metrics address, as typed.
    pub metrics: String,
    /// Remember the password in the system keychain.
    pub remember: bool,
}

impl Default for ServerDraft {
    /// A blank name over the documented local defaults, so adding a server is one keystroke
    /// plus a name.
    fn default() -> Self {
        Self {
            name: String::new(),
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT.to_string(),
            user: DEFAULT_USER.to_owned(),
            password: String::new(),
            database: DEFAULT_DATABASE.to_owned(),
            tls: false,
            ca_path: String::new(),
            environment: Environment::default(),
            metrics: String::new(),
            remember: false,
        }
    }
}

impl ServerDraft {
    /// Fill the form from an existing profile, for editing.
    #[must_use]
    pub fn from_profile(profile: &ServerProfile) -> Self {
        Self {
            name: profile.name.clone(),
            host: profile.params.host.clone(),
            port: profile.params.port.to_string(),
            user: profile.params.user.clone(),
            password: profile.params.password.clone(),
            database: profile.params.database.clone(),
            tls: profile.params.tls,
            ca_path: profile.params.ca_path.clone(),
            environment: profile.environment,
            metrics: profile.metrics.clone(),
            remember: profile.remember,
        }
    }

    /// Check every field and produce a [`ValidServer`].
    ///
    /// Surrounding whitespace is trimmed: a trailing space pasted into the host box is a typo,
    /// not an address.
    ///
    /// # Errors
    ///
    /// Returns the first [`ValidationError`] found, in field order, so the message the user
    /// sees always points at the topmost problem in the form.
    pub fn validate(&self) -> Result<ValidServer, ValidationError> {
        let name = self.name.trim();
        let host = self.host.trim();
        let user = self.user.trim();
        let database = self.database.trim();

        if name.is_empty() {
            return Err(ValidationError::NameEmpty);
        }
        if host.is_empty() {
            return Err(ValidationError::HostEmpty);
        }
        let port: u16 = self
            .port
            .trim()
            .parse()
            .ok()
            .filter(|port| *port > 0)
            .ok_or(ValidationError::PortInvalid)?;
        if user.is_empty() {
            return Err(ValidationError::UserEmpty);
        }
        if database.is_empty() {
            return Err(ValidationError::DatabaseEmpty);
        }
        let ca_path = self.ca_path.trim();
        if self.tls && ca_path.is_empty() {
            return Err(ValidationError::TlsWithoutCa);
        }

        let metrics = crate::metrics::normalize_url(&self.metrics)
            .map_err(|()| ValidationError::MetricsInvalid)?;

        Ok(ValidServer {
            name: name.to_owned(),
            environment: self.environment,
            metrics,
            remember: self.remember,
            params: ConnectionParams {
                host: host.to_owned(),
                port,
                user: user.to_owned(),
                database: database.to_owned(),
                password: self.password.clone(),
                tls: self.tls,
                ca_path: ca_path.to_owned(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> ServerDraft {
        ServerDraft {
            name: name.to_owned(),
            ..ServerDraft::default()
        }
    }

    #[test]
    fn default_draft_targets_the_documented_local_engine() {
        let valid = named("local").validate().unwrap();
        assert_eq!(valid.params.host, DEFAULT_HOST);
        assert_eq!(valid.params.port, DEFAULT_PORT);
        assert_eq!(valid.params.user, DEFAULT_USER);
        assert_eq!(valid.params.database, DEFAULT_DATABASE);
    }

    #[test]
    fn blank_fields_are_reported_in_form_order() {
        assert_eq!(named("").validate(), Err(ValidationError::NameEmpty));

        let mut draft = named("local");
        draft.host = "  ".to_owned();
        assert_eq!(draft.validate(), Err(ValidationError::HostEmpty));

        let mut draft = named("local");
        draft.user = String::new();
        assert_eq!(draft.validate(), Err(ValidationError::UserEmpty));

        let mut draft = named("local");
        draft.database = String::new();
        assert_eq!(draft.validate(), Err(ValidationError::DatabaseEmpty));
    }

    #[test]
    fn port_must_be_a_non_zero_u16() {
        for bad in ["", "0", "-1", "65536", "5678x", "  "] {
            let mut draft = named("local");
            draft.port = bad.to_owned();
            assert_eq!(
                draft.validate(),
                Err(ValidationError::PortInvalid),
                "port {bad:?} should be rejected"
            );
        }

        let mut draft = named("local");
        draft.port = " 65535 ".to_owned();
        assert_eq!(draft.validate().unwrap().params.port, 65535);
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let mut draft = named("  prod-jakarta  ");
        draft.host = "  10.0.2.15 ".to_owned();
        let valid = draft.validate().unwrap();
        assert_eq!(valid.name, "prod-jakarta");
        assert_eq!(valid.params.host, "10.0.2.15");
    }

    #[test]
    fn tls_requires_a_ca_bundle() {
        let mut draft = named("secure");
        draft.tls = true;
        assert_eq!(draft.validate(), Err(ValidationError::TlsWithoutCa));

        draft.ca_path = "  /etc/ssl/ca.pem  ".to_owned();
        assert_eq!(draft.validate().unwrap().params.ca_path, "/etc/ssl/ca.pem");
    }

    #[test]
    fn a_ca_bundle_is_not_demanded_without_tls() {
        let draft = named("plain");
        assert!(!draft.tls);
        assert_eq!(draft.validate().unwrap().params.ca_path, "");
    }

    #[test]
    fn a_draft_round_trips_through_a_profile() {
        let mut draft = named("staging");
        draft.tls = true;
        draft.ca_path = "/etc/ssl/ca.pem".to_owned();
        draft.environment = Environment::Staging;
        draft.password = "secret".to_owned();

        let profile = ServerProfile::from_valid(7, draft.validate().unwrap());
        assert_eq!(ServerDraft::from_profile(&profile), draft);
    }

    #[test]
    fn an_unknown_environment_tag_degrades_to_dev() {
        assert_eq!(Environment::from_tag("prod"), Environment::Prod);
        assert_eq!(Environment::from_tag("nonsense"), Environment::Dev);
    }
}
