//! The driver-backed [`ConnectionProbe`], over `nusadb-libnusa`.
//!
//! This is the adapter that fulfils the port in [`crate::probe`]. It is the only module that
//! knows the wire client exists; the rest of Studio asks "is this server reachable?" and gets
//! an answer, with no idea that a socket was involved.
//!
//! The driver is async and Studio's event loop is not, so the probe owns a small Tokio runtime
//! on worker threads and answers from there. The port's report callback is `Send` for exactly
//! this reason — the controller marshals the outcome back onto the UI thread.

use std::fs;

use nusadb_libnusa::{Client, Config, Error, tls_client_config};
use tokio::runtime::Runtime;

use crate::model::ConnectionParams;
use crate::probe::{ConnectionProbe, ProbeOutcome, ProbeReport};

/// A probe that really connects.
#[derive(Debug)]
pub struct DriverProbe {
    runtime: Runtime,
}

impl DriverProbe {
    /// Start the runtime the connection attempts will run on.
    ///
    /// # Errors
    ///
    /// Returns the I/O error if the runtime's threads cannot be created — the caller is
    /// expected to fall back to a probe that says so rather than to carry on silently.
    pub fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            // One worker is plenty: a connection test is almost entirely waiting on a socket,
            // and Studio never has many in flight.
            .worker_threads(1)
            .enable_io()
            .enable_time()
            .thread_name("nusadb-studio-driver")
            .build()?;
        Ok(Self { runtime })
    }
}

impl ConnectionProbe for DriverProbe {
    fn probe(&self, params: &ConnectionParams, report: ProbeReport) {
        let config = match build_config(params) {
            Ok(config) => config,
            // A bad CA file is a local problem, not a server one; answer immediately rather
            // than opening a socket that was never going to be trusted.
            Err(outcome) => {
                report(outcome);
                return;
            },
        };

        let endpoint = params.endpoint();
        self.runtime.spawn(async move {
            report(attempt(&config, &endpoint).await);
        });
    }
}

/// Translate connection parameters into the driver's configuration.
fn build_config(params: &ConnectionParams) -> Result<Config, ProbeOutcome> {
    let mut config = Config::new(
        params.host.clone(),
        params.port,
        params.user.clone(),
        params.database.clone(),
    );

    // An empty password means trust-on-startup, which is a different handshake from an empty
    // SCRAM secret — so it is left unset rather than sent as "".
    if !params.password.is_empty() {
        config = config.password(params.password.clone());
    }

    if params.tls {
        let pem = fs::read(&params.ca_path).map_err(|error| ProbeOutcome::Failed {
            message: format!("Cannot read the CA bundle at {}: {error}", params.ca_path),
        })?;
        let tls = tls_client_config(&pem).map_err(|error| ProbeOutcome::Failed {
            message: format!("The CA bundle at {} is unusable: {error}", params.ca_path),
        })?;
        config = config.tls(tls);
    }

    Ok(config)
}

/// Connect, prove the session works, and disconnect again.
///
/// A handshake alone is not proof of a usable database — the role may authenticate and still
/// be unable to run anything — so one trivial round trip is made before calling it reachable.
async fn attempt(config: &Config, endpoint: &str) -> ProbeOutcome {
    let mut client = match Client::connect(config).await {
        Ok(client) => client,
        Err(error) => {
            return ProbeOutcome::Failed {
                message: describe(&error),
            };
        },
    };

    let outcome = match client.simple_query("SELECT 1").await {
        Ok(_) => ProbeOutcome::Reachable {
            detail: format!("{endpoint} answered — authenticated and serving queries."),
        },
        Err(error) => ProbeOutcome::Failed {
            message: describe(&error),
        },
    };

    // The test holds no session, so the socket is closed either way; a failure to close
    // cleanly tells the user nothing they can act on.
    drop(client.close().await);
    outcome
}

/// Put a driver error in the user's terms.
fn describe(error: &Error) -> String {
    match error {
        Error::Io(inner) => format!("Could not reach the server: {inner}"),
        Error::Auth(inner) => format!("Authentication failed: {inner}"),
        Error::Tls(inner) => format!("TLS failed: {inner}"),
        Error::Server { code, message } => {
            format!("Server rejected the connection [{code}]: {message}")
        },
        Error::Protocol(inner) => format!("The server spoke an unexpected protocol: {inner}"),
        Error::Decode(inner) => format!("Could not decode the server's reply: {inner}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parameters pointing at a port nothing is listening on.
    fn unreachable() -> ConnectionParams {
        ConnectionParams {
            // Port 1 on the loopback interface: reserved, and never a NusaDB listener.
            port: 1,
            ..ConnectionParams::default()
        }
    }

    #[test]
    fn tls_with_a_missing_ca_file_fails_before_any_socket_is_opened() {
        let params = ConnectionParams {
            tls: true,
            ca_path: "no/such/ca.pem".to_owned(),
            ..ConnectionParams::default()
        };

        match build_config(&params) {
            Err(ProbeOutcome::Failed { message }) => {
                assert!(
                    message.contains("no/such/ca.pem"),
                    "names the file: {message}"
                );
            },
            other => panic!("expected a failure naming the CA file, got {other:?}"),
        }
    }

    #[test]
    fn a_plaintext_configuration_is_accepted() {
        assert!(build_config(&ConnectionParams::default()).is_ok());
    }

    #[test]
    fn an_unreachable_server_is_reported_as_failed_not_as_unavailable() {
        let probe = DriverProbe::new().expect("runtime starts");
        let (tx, rx) = std::sync::mpsc::channel();
        probe.probe(
            &unreachable(),
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );

        let outcome = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the probe reports within the timeout");
        match outcome {
            ProbeOutcome::Failed { message } => {
                assert!(!message.is_empty(), "the failure explains itself");
            },
            other => panic!("expected Failed for a dead port, got {other:?}"),
        }
    }
}
