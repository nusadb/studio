//! LISTEN / NOTIFY in the Query screen's Notifications tab. The listener runs on a connection of
//! its own in the driver; sending goes through the editor's session, so a NOTIFY inside the
//! editor's open transaction is delivered at its COMMIT, as the server documents.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{AppWindow, Controller, explain, on_ui, unix_now};
use crate::backend::{ListenEvent, StatementOutcome, TxnState, quote_ident};
use crate::designer::valid_ident;
use crate::ui::NotifyRow;

/// Notifications kept on screen; older ones scroll away.
const KEEP: usize = 500;

/// The listener the tab drives.
#[derive(Debug, Default)]
pub(super) struct NotifyState {
    /// The database the listener is on.
    on: Option<(u32, String)>,
    pub(super) channels: Vec<String>,
    rows: Vec<NotifyRow>,
}

/// A channel name as the server folds an unquoted identifier, or why it cannot be one.
fn channel(text: &str) -> Result<String, String> {
    let name = text.trim().to_lowercase();
    if valid_ident(&name) {
        Ok(name)
    } else {
        Err(format!(
            "“{}” is not a channel name: letters, digits and _ only.",
            text.trim()
        ))
    }
}

/// `NOTIFY channel[, 'payload']`, the payload as a string literal.
fn notify_sql(channel: &str, payload: &str) -> String {
    if payload.is_empty() {
        format!("NOTIFY {}", quote_ident(channel))
    } else {
        format!(
            "NOTIFY {}, '{}'",
            quote_ident(channel),
            payload.replace('\'', "''")
        )
    }
}

/// `HH:MM:SS` of a Unix time, in UTC.
fn clock(secs: u64) -> String {
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

impl Controller {
    fn publish_notify(&self, window: &AppWindow) {
        let state = self.notify.borrow();
        window.set_notify_rows(ModelRc::new(VecModel::from(state.rows.clone())));
        let channels: Vec<SharedString> =
            state.channels.iter().map(|c| c.as_str().into()).collect();
        window.set_notify_channels(ModelRc::new(VecModel::from(channels)));
    }

    /// Ask the driver to hold `channels` on `on`, reporting back here.
    fn notify_apply(&self, window: &AppWindow, on: (u32, String), channels: Vec<String>) {
        let Some(target) = self.target_for(on.0, &on.1) else {
            return;
        };
        let weak = window.as_weak();
        let sink: crate::backend::ListenSink = Arc::new(move |event: ListenEvent| {
            on_ui(&weak, move |c, w| c.notify_event(w, event));
        });
        self.backend.listen(target, channels, sink);
    }

    pub(super) fn notify_listen(&self, window: &AppWindow, text: &str) {
        let name = match channel(text) {
            Ok(name) => name,
            Err(message) => {
                window.set_notify_failed(true);
                window.set_notify_status(message.into());
                return;
            },
        };
        let Some(target) = self.target.borrow().clone() else {
            return;
        };
        let (previous, mut channels) = {
            let state = self.notify.borrow();
            (state.on.clone(), state.channels.clone())
        };
        // The listener follows the editor's database: moving to another one starts over there.
        if previous.as_ref().is_some_and(|p| *p != target) {
            if let Some(old) = previous {
                self.notify_apply(window, old, Vec::new());
            }
            channels.clear();
        }
        if !channels.contains(&name) {
            channels.push(name);
        }
        self.notify.borrow_mut().on = Some(target.clone());
        window.set_notify_failed(false);
        window.set_notify_status("Connecting the listener…".into());
        self.notify_apply(window, target, channels);
    }

    pub(super) fn notify_unlisten(&self, window: &AppWindow, name: &str) {
        let (on, mut channels) = {
            let state = self.notify.borrow();
            (state.on.clone(), state.channels.clone())
        };
        let Some(on) = on else {
            return;
        };
        channels.retain(|c| c != name);
        self.notify_apply(window, on, channels);
    }

    fn notify_event(&self, window: &AppWindow, event: ListenEvent) {
        match event {
            ListenEvent::Listening(channels) => {
                let on = self.notify.borrow().on.clone();
                let label = on
                    .as_ref()
                    .and_then(|(s, d)| {
                        self.store
                            .borrow()
                            .get(*s)
                            .map(|p| format!("{} / {d}", p.name))
                    })
                    .unwrap_or_default();
                window.set_notify_failed(false);
                window.set_notify_status(
                    if channels.is_empty() {
                        "Not listening.".to_owned()
                    } else {
                        format!("Listening on {label}, on a connection of its own.")
                    }
                    .into(),
                );
                self.notify.borrow_mut().channels = channels;
            },
            ListenEvent::Notification {
                channel,
                payload,
                pid,
            } => {
                let mut state = self.notify.borrow_mut();
                state.rows.insert(
                    0,
                    NotifyRow {
                        time: clock(unix_now()).into(),
                        channel: channel.into(),
                        payload: payload.into(),
                        pid: pid.to_string().into(),
                    },
                );
                state.rows.truncate(KEEP);
            },
            ListenEvent::Failed(message) => {
                self.notify.borrow_mut().channels.clear();
                window.set_notify_failed(true);
                window.set_notify_status(message.into());
            },
        }
        self.publish_notify(window);
    }

    pub(super) fn notify_send(&self, window: &AppWindow, text: &str, payload: &str) {
        let name = match channel(text) {
            Ok(name) => name,
            Err(message) => {
                window.set_notify_failed(true);
                window.set_notify_status(message.into());
                return;
            },
        };
        let Some((server, database)) = self.target.borrow().clone() else {
            return;
        };
        let Some(target) = self.target_for(server, &database) else {
            return;
        };
        let queued = self.txn.get() != TxnState::Idle;
        let weak = window.as_weak();
        self.backend.run(
            target,
            vec![notify_sql(&name, payload)],
            Box::new(move |result| {
                on_ui(&weak, move |_c, w| {
                    let failure = match &result {
                        Err(m) => Some(m.clone()),
                        Ok(r) => r.failure().map(|f| match &f.outcome {
                            StatementOutcome::Failed { code, message } => explain(code.as_deref(), message),
                            StatementOutcome::Done { .. } => String::new(),
                        }),
                    };
                    w.set_notify_failed(failure.is_some());
                    w.set_notify_status(
                        match failure {
                            Some(m) => format!("NOTIFY failed: {m}"),
                            None if queued => format!(
                                "NOTIFY {name} queued: the editor has a transaction open, so it is delivered at COMMIT."
                            ),
                            None => format!("Sent to {name}."),
                        }
                        .into(),
                    );
                });
            }),
        );
    }

    pub(super) fn notify_clear(&self, window: &AppWindow) {
        self.notify.borrow_mut().rows.clear();
        self.publish_notify(window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_fold_like_unquoted_identifiers() {
        assert_eq!(channel(" Orders_Changed ").unwrap(), "orders_changed");
        assert!(channel("orders changed").is_err());
        assert_eq!(notify_sql("jobs", ""), "NOTIFY \"jobs\"");
        assert_eq!(
            notify_sql("jobs", "it's done"),
            "NOTIFY \"jobs\", 'it''s done'"
        );
        assert_eq!(clock(3_661), "01:01:01");
    }
}
