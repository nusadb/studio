//! The Monitor screen: a server's Prometheus endpoint scraped every two seconds while the screen
//! is open, and the prepared transactions of the database the navigator points at.

use std::time::Instant;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{AppWindow, Controller, Pending, ago, explain, on_ui, unix_now};
use crate::backend::{RunReport, StatementOutcome};
use crate::metrics::{self, Series, Snapshot};
use crate::ui::{MonDb, MonTile, MonXact};

/// Two minutes of history at one scrape every two seconds.
const HISTORY: usize = 60;
/// The sparkline's box, as `MonTile.path` is drawn in.
const SPARK_W: f64 = 200.0;
const SPARK_H: f64 = 36.0;

/// The server being watched.
#[derive(Debug, Clone)]
pub(super) struct MonitorState {
    server: u32,
    url: String,
    last: Option<(Instant, Snapshot)>,
    active: Series,
    queries: Series,
    errors: Series,
    connections: Series,
    in_flight: bool,
    /// gids of the prepared transactions shown, row for row.
    xacts: Vec<String>,
    /// The database those were read from.
    xacts_db: Option<(u32, String)>,
}

impl MonitorState {
    fn new(server: u32, url: String) -> Self {
        Self {
            server,
            url,
            last: None,
            active: Series::new(HISTORY),
            queries: Series::new(HISTORY),
            errors: Series::new(HISTORY),
            connections: Series::new(HISTORY),
            in_flight: false,
            xacts: Vec::new(),
            xacts_db: None,
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole(value: f64) -> String {
    crate::backend::group_thousands(value.max(0.0).round() as usize)
}

fn tile(
    label: &str,
    value: String,
    unit: &str,
    sub: String,
    series: &Series,
    alert: bool,
) -> MonTile {
    let values = series.values();
    MonTile {
        label: label.into(),
        value: value.into(),
        unit: unit.into(),
        sub: sub.into(),
        path: metrics::sparkline(&values, SPARK_W, SPARK_H, series.max() * 1.15).into(),
        alert,
    }
}

impl Controller {
    /// The server the Monitor watches: the one the navigator points at.
    fn monitor_server(&self) -> Option<u32> {
        if let Some((server, _)) = self.target.borrow().as_ref() {
            return Some(*server);
        }
        self.selection
            .borrow()
            .as_ref()
            .map(crate::tree::Node::server)
    }

    /// The Monitor was opened, or the navigator or a server's endpoint changed while it is open.
    pub(super) fn monitor_follow(&self, window: &AppWindow) {
        if window.get_screen() != "monitor" {
            return;
        }
        let Some(server) = self.monitor_server() else {
            window.set_mon_state(SharedString::new());
            return;
        };
        let (name, url) = {
            let store = self.store.borrow();
            let Some(profile) = store.get(server) else {
                return;
            };
            (profile.name.clone(), profile.metrics.clone())
        };
        window.set_mon_server(name.into());
        window.set_mon_endpoint(url.as_str().into());
        let same = self
            .monitor
            .borrow()
            .as_ref()
            .is_some_and(|m| m.server == server && m.url == url);
        if !same {
            self.monitor
                .replace((!url.is_empty()).then(|| MonitorState::new(server, url.clone())));
            if url.is_empty() {
                window.set_mon_state("no-endpoint".into());
                return;
            }
            window.set_mon_state("loading".into());
            self.monitor_poll(window);
            self.monitor_xacts(window);
            return;
        }
        // Same server, but the navigator may have moved to another of its databases.
        let target = self.target.borrow().clone();
        let stale = self
            .monitor
            .borrow()
            .as_ref()
            .is_some_and(|m| m.xacts_db != target);
        if stale {
            self.monitor_xacts(window);
        }
    }

    /// The two-second timer fired.
    pub(super) fn monitor_tick(&self, window: &AppWindow) {
        if window.get_screen() == "monitor" && !window.get_mon_paused() {
            self.monitor_poll(window);
        }
    }

    pub(super) fn monitor_poll(&self, window: &AppWindow) {
        let url = {
            let mut guard = self.monitor.borrow_mut();
            let Some(m) = guard.as_mut() else {
                return;
            };
            if m.in_flight {
                return;
            }
            m.in_flight = true;
            m.url.clone()
        };
        let weak = window.as_weak();
        let started = Instant::now();
        let asked = url.clone();
        self.backend.scrape(
            url,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    c.monitor_scraped(w, &asked, started, result)
                });
            }),
        );
    }

    fn monitor_scraped(
        &self,
        window: &AppWindow,
        url: &str,
        started: Instant,
        result: Result<String, String>,
    ) {
        let mut guard = self.monitor.borrow_mut();
        let Some(m) = guard.as_mut().filter(|m| m.url == url) else {
            return;
        };
        m.in_flight = false;
        let latency = started.elapsed();
        let snapshot = match result {
            Ok(body) => {
                let snap = Snapshot::from_samples(&metrics::parse(&body));
                if snap.recognised {
                    Ok(snap)
                } else {
                    Err(format!(
                        "{url} answered, but not with NusaDB's metrics. Is it the \
                         --metrics-listen address?"
                    ))
                }
            },
            Err(message) => Err(message),
        };
        let snap = match snapshot {
            Ok(snap) => snap,
            Err(message) => {
                if m.last.is_none() {
                    window.set_mon_error(message.into());
                    window.set_mon_state("error".into());
                } else {
                    // Keep the last figures on screen and say they are stale.
                    window.set_mon_stale(true);
                    window.set_mon_updated(format!("last scrape failed: {message}").into());
                }
                return;
            },
        };
        let now = Instant::now();
        let rates = m.last.as_ref().and_then(|(at, before)| {
            metrics::rates(before, &snap, now.duration_since(*at).as_secs_f64())
        });
        if m.last.is_some() && rates.is_none() {
            // A counter went backwards: the server restarted, and old history means nothing.
            m.queries.clear();
            m.errors.clear();
            m.connections.clear();
            m.active.clear();
        }
        m.active.push(snap.connections_active);
        if let Some(r) = rates {
            m.queries.push(r.queries);
            m.errors.push(r.errors);
            m.connections.push(r.connections);
        }
        let error_share = rates
            .filter(|r| r.queries > 0.0)
            .map(|r| r.errors / r.queries * 100.0);
        let tiles = vec![
            tile(
                "ACTIVE CONNECTIONS",
                whole(snap.connections_active),
                "",
                format!(
                    "{} accepted since start · Studio's own included",
                    whole(snap.connections_total)
                ),
                &m.active,
                false,
            ),
            tile(
                "STATEMENTS",
                rates.map_or("–".to_owned(), |r| metrics::per_second(r.queries)),
                "/s",
                format!("{} since start", whole(snap.queries_total)),
                &m.queries,
                false,
            ),
            tile(
                "ERRORS",
                rates.map_or("–".to_owned(), |r| metrics::per_second(r.errors)),
                "/s",
                error_share.map_or_else(
                    || format!("{} since start", whole(snap.query_errors_total)),
                    |p| format!("{p:.1}% of statements just now"),
                ),
                &m.errors,
                rates.is_some_and(|r| r.errors > 0.0),
            ),
            tile(
                "NEW CONNECTIONS",
                rates.map_or("–".to_owned(), |r| metrics::per_second(r.connections)),
                "/s",
                "connections opened".to_owned(),
                &m.connections,
                false,
            ),
        ];
        window.set_mon_tiles(ModelRc::new(VecModel::from(tiles)));
        let share = if snap.queries_total > 0.0 {
            format!(
                " ({:.2}%)",
                snap.query_errors_total / snap.queries_total * 100.0
            )
        } else {
            String::new()
        };
        window.set_mon_totals(
            format!(
                "Since the server started: {} connections · {} statements · {} errors{share}",
                whole(snap.connections_total),
                whole(snap.queries_total),
                whole(snap.query_errors_total)
            )
            .into(),
        );
        let dbs: Vec<MonDb> = snap
            .databases
            .iter()
            .map(|(name, stopped)| MonDb {
                name: name.as_str().into(),
                stopped: *stopped,
            })
            .collect();
        window.set_mon_databases(ModelRc::new(VecModel::from(dbs)));

        let mut detail = Vec::new();
        let (last, mut alert) = if snap.backup_last_success > 0.0 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let when = snap.backup_last_success as u64;
            detail.push(format!("took {:.1} s", snap.backup_last_duration));
            (
                format!("Last backup completed {}.", ago(unix_now(), when)),
                false,
            )
        } else {
            detail.push("Runs when the server has --backup-dir.".to_owned());
            (
                "No scheduled backup has completed since the server started.".to_owned(),
                false,
            )
        };
        if snap.backup_failures_total > 0.0 {
            alert = true;
            detail.insert(
                0,
                format!(
                    "{} scheduled backups failed. See the server log.",
                    whole(snap.backup_failures_total)
                ),
            );
        }
        if snap.archive_prune_failures_total > 0.0 {
            alert = true;
            detail.push(format!(
                "{} archive prunes failed.",
                whole(snap.archive_prune_failures_total)
            ));
        }
        window.set_mon_backup_last(last.into());
        window.set_mon_backup_detail(detail.join(" ").into());
        window.set_mon_backup_alert(alert);
        window.set_mon_updated(format!("answered in {} ms", latency.as_millis()).into());
        m.last = Some((now, snap));
        window.set_mon_error(SharedString::new());
        window.set_mon_stale(false);
        window.set_mon_state("ready".into());
    }

    /// Read the prepared transactions of the database the navigator points at.
    pub(super) fn monitor_xacts(&self, window: &AppWindow) {
        let Some((server, database)) = self.target.borrow().clone() else {
            window.set_mon_xacts_database("none".into());
            window.set_mon_xacts_note(
                "Pick a database in the navigator to list its prepared transactions.".into(),
            );
            return;
        };
        let Some(target) = self.target_for(server, &database) else {
            return;
        };
        window.set_mon_xacts_database(database.as_str().into());
        if let Some(m) = self.monitor.borrow_mut().as_mut() {
            m.xacts_db = Some((server, database.clone()));
        }
        let weak = window.as_weak();
        self.backend.run(
            target,
            vec![
                "SELECT gid, prepared, owner, database FROM nusadb_prepared_xacts ORDER BY prepared"
                    .to_owned(),
            ],
            Box::new(move |result| on_ui(&weak, move |c, w| c.monitor_xacts_read(w, result))),
        );
    }

    fn monitor_xacts_read(&self, window: &AppWindow, result: Result<RunReport, String>) {
        let rows = match result
            .as_ref()
            .map(|r| r.statements.first().map(|s| &s.outcome))
        {
            Ok(Some(StatementOutcome::Done { result, .. })) => result.rows.clone(),
            Ok(Some(StatementOutcome::Failed { code, message })) => {
                window.set_mon_xacts(ModelRc::new(VecModel::from(Vec::<MonXact>::new())));
                window.set_mon_xacts_note(explain(code.as_deref(), message).into());
                return;
            },
            Ok(None) => Vec::new(),
            Err(message) => {
                window.set_mon_xacts_note(message.as_str().into());
                return;
            },
        };
        let cell =
            |r: &Vec<Option<String>>, i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let xacts: Vec<MonXact> = rows
            .iter()
            .map(|r| MonXact {
                gid: cell(r, 0).into(),
                prepared: cell(r, 1).into(),
                owner: cell(r, 2).into(),
                database: cell(r, 3).into(),
            })
            .collect();
        if let Some(m) = self.monitor.borrow_mut().as_mut() {
            m.xacts = rows.iter().map(|r| cell(r, 0)).collect();
        }
        window.set_mon_xacts_note("None waiting.".into());
        window.set_mon_xacts(ModelRc::new(VecModel::from(xacts)));
    }

    /// Ask before finishing a prepared transaction by hand.
    pub(super) fn monitor_xact(&self, window: &AppWindow, row: usize, action: &str) {
        let Some(gid) = self
            .monitor
            .borrow()
            .as_ref()
            .and_then(|m| m.xacts.get(row).cloned())
        else {
            return;
        };
        let verb = if action == "COMMIT" {
            "COMMIT"
        } else {
            "ROLLBACK"
        };
        let sql = format!("{verb} PREPARED '{}'", gid.replace('\'', "''"));
        self.ask(
            window,
            Pending::FinishPrepared(sql.clone()),
            if verb == "COMMIT" {
                "Commit this prepared transaction?"
            } else {
                "Roll back this prepared transaction?"
            },
            &[
                sql,
                "Do this only when its coordinator is gone: finishing it by hand can leave the \
                 other participants of the distributed transaction disagreeing with this one."
                    .to_owned(),
            ],
            if verb == "COMMIT" {
                "Commit"
            } else {
                "Roll back"
            },
        );
    }

    pub(super) fn monitor_finish(&self, window: &AppWindow, sql: String) {
        let Some((server, database)) = self.target.borrow().clone() else {
            return;
        };
        let Some(target) = self.target_for(server, &database) else {
            return;
        };
        let weak = window.as_weak();
        self.backend.run(
            target,
            vec![sql],
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    let failure = match &result {
                        Err(m) => Some(m.clone()),
                        Ok(r) => r.failure().map(|f| match &f.outcome {
                            StatementOutcome::Failed { code, message } => {
                                explain(code.as_deref(), message)
                            },
                            StatementOutcome::Done { .. } => String::new(),
                        }),
                    };
                    match failure {
                        Some(message) => c.toast(w, &message, true),
                        None => c.toast(w, "Prepared transaction finished", false),
                    }
                    c.monitor_xacts(w);
                });
            }),
        );
    }

    pub(super) fn monitor_edit_endpoint(&self, window: &AppWindow) {
        if let Some(server) = self.monitor_server() {
            self.edit_server(window, server);
            window.set_server_form_open(true);
            window.set_screen("connections".into());
        }
    }

    pub(super) fn monitor_retry(&self, window: &AppWindow) {
        self.monitor.replace(None);
        self.monitor_follow(window);
    }
}
