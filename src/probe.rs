//! The seam between the Connection Manager and a real network connection.
//!
//! The UI only ever asks one question — "can this server be reached with these parameters?" —
//! and knows nothing about the wire protocol. [`crate::driver`] answers it over
//! `nusadb-libnusa`; [`UnavailableProbe`] stands in when that adapter cannot be built at all.
//!
//! The report is delivered through a callback rather than a return value because the adapter
//! answers from a network task, not inline — which is also why it is `Send`.

use crate::model::ConnectionParams;

/// The result of one connection attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The server answered and accepted us.
    Reachable {
        /// What to show the user, e.g. the negotiated protocol version.
        detail: String,
    },
    /// The server was contacted but the attempt failed (refused, rejected, timed out).
    Failed {
        /// Why it failed, in the user's terms.
        message: String,
    },
    /// No attempt could be made at all.
    Unavailable {
        /// Why no attempt was possible.
        reason: String,
    },
}

/// Delivers a [`ProbeOutcome`] exactly once.
///
/// It is `Send` because an implementation may answer from another thread; the controller is
/// responsible for marshalling the outcome back onto the UI thread.
pub type ProbeReport = Box<dyn FnOnce(ProbeOutcome) + Send + 'static>;

/// Port: something that can check whether a server is reachable.
pub trait ConnectionProbe {
    /// Start probing `params` and report the outcome through `report`.
    ///
    /// Implementations must call `report` exactly once, inline or later.
    fn probe(&self, params: &ConnectionParams, report: ProbeReport);
}

/// The stand-in used when no real probe could be started.
///
/// Studio connects through the driver adapter in [`crate::driver`]; this one exists for the
/// case where that adapter cannot be created at all — a runtime that fails to start, say. It
/// reports [`ProbeOutcome::Unavailable`] carrying the reason, because a fake success would
/// teach the user to trust a result the app never produced.
#[derive(Debug, Clone)]
pub struct UnavailableProbe {
    reason: String,
}

impl UnavailableProbe {
    /// A probe that declines every request, explaining why.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl ConnectionProbe for UnavailableProbe {
    fn probe(&self, params: &ConnectionParams, report: ProbeReport) {
        report(ProbeOutcome::Unavailable {
            reason: format!("{} was not contacted: {}", params.endpoint(), self.reason),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    /// A probe that answers with whatever the test asked for — the second implementation the
    /// port exists for, and the one the controller will be exercised against.
    struct ScriptedProbe(ProbeOutcome);

    impl ConnectionProbe for ScriptedProbe {
        fn probe(&self, _params: &ConnectionParams, report: ProbeReport) {
            report(self.0.clone());
        }
    }

    fn collect(probe: &dyn ConnectionProbe) -> ProbeOutcome {
        let (tx, rx) = mpsc::channel();
        probe.probe(
            &ConnectionParams::default(),
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        rx.try_recv().expect("probe must report exactly once")
    }

    #[test]
    fn the_unavailable_probe_never_claims_a_connection() {
        let outcome = collect(&UnavailableProbe::new("the driver runtime did not start"));
        match outcome {
            ProbeOutcome::Unavailable { reason } => {
                assert!(reason.contains("127.0.0.1:5678"), "reason names the target");
                assert!(
                    reason.contains("did not start"),
                    "reason carries the cause: {reason}"
                );
            },
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn the_port_carries_any_outcome_back_to_the_caller() {
        let reachable = ProbeOutcome::Reachable {
            detail: "protocol 1.1".to_owned(),
        };
        assert_eq!(collect(&ScriptedProbe(reachable.clone())), reachable);

        let failed = ProbeOutcome::Failed {
            message: "connection refused".to_owned(),
        };
        assert_eq!(collect(&ScriptedProbe(failed.clone())), failed);
    }
}
