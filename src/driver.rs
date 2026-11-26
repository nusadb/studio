//! The driver-backed [`ConnectionProbe`] and [`Backend`], over the published `nusadb` crate.
//!
//! This is the adapter that fulfils the ports in [`crate::probe`] and [`crate::backend`]. It is the only module that
//! knows the wire client exists; the rest of Studio asks "is this server reachable?" and gets
//! an answer, with no idea that a socket was involved.
//!
//! The driver is async and Studio's event loop is not, so the probe owns a small Tokio runtime
//! on worker threads and answers from there. The port's report callback is `Send` for exactly
//! this reason — the controller marshals the outcome back onto the UI thread.

use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use nusadb::{
    AsyncConnection, CancelHandle, Config, Error, TlsConfig, TransactionStatus, TypeTag, Value,
};
use tokio::runtime::Runtime;

use crate::backend::{
    Backend, Catalog, ColumnDef, Constraint, DbObject, IndexDef, ObjectInfo, ObjectKind, PolicyDef,
    Reply, ResultSet, RunReport, ServerInfo, StatementOutcome, StatementReport, Target, TriggerDef,
    TxnState, quote_ident,
};
use crate::model::ConnectionParams;
use crate::probe::{ConnectionProbe, ProbeOutcome, ProbeReport};
use crate::security::{Grant, Membership, Policy, Relation, Role, Security};

/// Rows the grid is handed at most. A result larger than this is cut, and the UI says so —
/// rendering a million rows into a desktop list would freeze the window long before the user
/// could read any of them.
pub const MAX_GRID_ROWS: usize = 1_000;

/// The cap in force (Settings → Rows kept in the results); `MAX_GRID_ROWS` until changed.
static ROW_CAP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(MAX_GRID_ROWS);

/// Set how many rows a result keeps.
pub fn set_row_cap(rows: usize) {
    ROW_CAP.store(rows.max(1), std::sync::atomic::Ordering::Relaxed);
}

/// How many rows a result keeps now.
#[must_use]
pub fn row_cap() -> usize {
    ROW_CAP.load(std::sync::atomic::Ordering::Relaxed)
}

/// The session held for one `(server, database)` pair.
type Session = Arc<tokio::sync::Mutex<Option<(ConnectionParams, AsyncConnection)>>>;
type SessionKey = (u32, String);

/// A probe that really connects, and the sessions Studio keeps open.
pub struct DriverProbe {
    runtime: Runtime,
    sessions: Arc<Mutex<HashMap<SessionKey, Session>>>,
    cancels: Arc<Mutex<HashMap<SessionKey, CancelHandle>>>,
    /// One listener task per database, told which channels to hold.
    listeners: Arc<Mutex<HashMap<SessionKey, tokio::sync::watch::Sender<Vec<String>>>>>,
}

impl std::fmt::Debug for DriverProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverProbe").finish_non_exhaustive()
    }
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
            .worker_threads(2)
            .enable_io()
            .enable_time()
            .thread_name("nusadb-studio-driver")
            .build()?;
        Ok(Self {
            runtime,
            sessions: Arc::default(),
            cancels: Arc::default(),
            listeners: Arc::default(),
        })
    }

    /// The session slot for `target`, created empty on first use.
    fn session(&self, target: &Target) -> Session {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            sessions
                .entry((target.server, target.database.clone()))
                .or_default(),
        )
    }

    /// Run `work` against the target's open connection, connecting first when needed.
    ///
    /// A connection is reopened when the saved parameters changed since it was made, or when
    /// the previous one broke — the user should never have to "reconnect" by hand after a
    /// network blip.
    fn with_connection<T, F>(&self, target: Target, reply: Reply<T>, work: F)
    where
        T: Send + 'static,
        F: for<'c> FnOnce(
                &'c mut AsyncConnection,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<T, String>> + Send + 'c>,
            > + Send
            + 'static,
    {
        let session = self.session(&target);
        let cancels = Arc::clone(&self.cancels);
        self.runtime.spawn(async move {
            let mut slot = session.lock().await;
            let stale = match slot.as_ref() {
                Some((params, conn)) => *params != target.params || conn.is_broken(),
                None => true,
            };
            if stale {
                if let Some((_, old)) = slot.take() {
                    old.close().await;
                }
                let mut params = target.params.clone();
                params.database.clone_from(&target.database);
                let config = match build_config(&params) {
                    Ok(config) => config,
                    Err(message) => {
                        reply(Err(message));
                        return;
                    },
                };
                match AsyncConnection::connect_config(&config).await {
                    Ok(conn) => {
                        cancels
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(
                                (target.server, target.database.clone()),
                                conn.cancel_handle(),
                            );
                        *slot = Some((target.params.clone(), conn));
                    },
                    Err(error) => {
                        reply(Err(describe(&error)));
                        return;
                    },
                }
            }
            let Some((_, conn)) = slot.as_mut() else {
                reply(Err("The session closed unexpectedly.".to_owned()));
                return;
            };
            reply(work(conn).await);
        });
    }
}

impl Backend for DriverProbe {
    fn server_info(&self, target: Target, reply: Reply<ServerInfo>) {
        self.with_connection(target, reply, |conn| {
            Box::pin(async move {
                let version = rows(conn, "SELECT version()", &[]).await?;
                let databases =
                    rows(conn, "SELECT name FROM nusadb_databases ORDER BY name", &[]).await?;
                Ok(ServerInfo {
                    version: cell(version.first(), 0),
                    databases: databases.iter().map(|r| cell(Some(r), 0)).collect(),
                })
            })
        });
    }

    fn catalog(&self, target: Target, reply: Reply<Catalog>) {
        self.with_connection(target, reply, |conn| {
            Box::pin(async move {
                let schemas = rows(
                    conn,
                    "SELECT schema_name FROM information_schema.schemata ORDER BY schema_name",
                    &[],
                )
                .await?;
                let mut catalog = Catalog {
                    schemas: schemas.iter().map(|r| cell(Some(r), 0)).collect(),
                    objects: Vec::new(),
                };
                let sources = [
                    (
                        "SELECT table_schema, table_name FROM information_schema.tables",
                        ObjectKind::Table,
                    ),
                    (
                        "SELECT table_schema, table_name FROM information_schema.views",
                        ObjectKind::View,
                    ),
                    (
                        "SELECT sequence_schema, sequence_name FROM information_schema.sequences",
                        ObjectKind::Sequence,
                    ),
                    (
                        "SELECT routine_schema, routine_name FROM information_schema.routines",
                        ObjectKind::Function,
                    ),
                ];
                for (sql, kind) in sources {
                    // A server that lacks one of these catalogs still lists the others.
                    let Ok(found) = rows(conn, sql, &[]).await else {
                        continue;
                    };
                    for row in found {
                        let (schema, name) = (cell(Some(&row), 0), cell(Some(&row), 1));
                        // information_schema.tables lists views too on some servers; keep
                        // each relation once, under its most specific kind.
                        if kind == ObjectKind::View {
                            catalog.objects.retain(|o| {
                                !(o.kind == ObjectKind::Table
                                    && o.schema == schema
                                    && o.name == name)
                            });
                        }
                        catalog.objects.push(DbObject { schema, name, kind });
                    }
                }
                // Materialized views are listed as tables by information_schema; the engine's own
                // catalog (readable by a superuser) says which are which.
                if let Ok(matviews) = rows(conn, "SELECT name FROM nusadb_matviews", &[]).await {
                    for row in matviews {
                        let name = cell(Some(&row), 0);
                        for o in &mut catalog.objects {
                            if o.kind == ObjectKind::Table && o.name == name {
                                o.kind = ObjectKind::MaterializedView;
                            }
                        }
                    }
                }
                catalog
                    .objects
                    .sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
                catalog.objects.dedup();
                Ok(catalog)
            })
        });
    }

    fn describe(&self, target: Target, object: DbObject, reply: Reply<ObjectInfo>) {
        self.with_connection(target, reply, move |conn| {
            Box::pin(async move {
                let key = [object.schema.clone(), object.name.clone()];
                let mut info = ObjectInfo::default();
                if object.kind.has_rows() {
                    info.columns = rows(
                        conn,
                        "SELECT column_name, data_type, is_nullable, column_default, \
                         character_maximum_length, numeric_precision, numeric_scale \
                         FROM information_schema.columns \
                         WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
                        &key,
                    )
                    .await?
                    .iter()
                    .map(|r| ColumnDef {
                        name: cell(Some(r), 0),
                        type_spec: type_spec(
                            &cell(Some(r), 1),
                            &cell(Some(r), 4),
                            &cell(Some(r), 5),
                            &cell(Some(r), 6),
                        ),
                        data_type: cell(Some(r), 1),
                        nullable: cell(Some(r), 2) != "NO",
                        default: r.get(3).cloned().flatten(),
                        comment: None,
                    })
                    .collect();
                    describe_comments(conn, &object, &mut info).await;
                    if info.columns.is_empty() && object.kind != ObjectKind::Table {
                        result_columns(conn, &object, &mut info).await;
                    }
                }
                match object.kind {
                    ObjectKind::Table => {
                        // Each of these is a nicety: a catalog that cannot be read leaves its
                        // section empty and says so, instead of failing the whole description.
                        info.constraints = constraints(conn, &key).await.unwrap_or_default();
                        describe_indexes(conn, &key, &mut info).await;
                        describe_triggers(conn, &key, &mut info).await;
                        describe_policies(conn, &key, &mut info).await;
                        info.row_count = count_rows(conn, &object).await;
                    },
                    ObjectKind::View => {
                        info.row_count = count_rows(conn, &object).await;
                        info.definition = rows(
                            conn,
                            "SELECT view_definition FROM information_schema.views \
                             WHERE table_schema = $1 AND table_name = $2",
                            &key,
                        )
                        .await
                        .ok()
                        .and_then(|found| found.first().map(|r| cell(Some(r), 0)))
                        .filter(|d| !d.is_empty());
                    },
                    ObjectKind::MaterializedView => {
                        info.row_count = count_rows(conn, &object).await;
                        match rows(
                            conn,
                            "SELECT def FROM nusadb_matviews WHERE name = $1",
                            &key[1..],
                        )
                        .await
                        {
                            Ok(found) => info.definition = found.first().map(|r| cell(Some(r), 0)),
                            Err(_) => info.notes.push(
                                "The query behind it is visible to a superuser only.".to_owned(),
                            ),
                        }
                    },
                    ObjectKind::Sequence => {
                        let found = rows(
                            conn,
                            "SELECT data_type, start_value, minimum_value, maximum_value, \
                             increment, cycle_option FROM information_schema.sequences \
                             WHERE sequence_schema = $1 AND sequence_name = $2",
                            &key,
                        )
                        .await?;
                        let row = found.first();
                        for (i, label) in
                            ["Type", "Start", "Minimum", "Maximum", "Increment", "Cycles"]
                                .iter()
                                .enumerate()
                        {
                            let value = cell(row, i);
                            if !value.is_empty() {
                                info.details.push(((*label).to_owned(), value));
                            }
                        }
                    },
                    ObjectKind::Function => {
                        let found = rows(
                            conn,
                            "SELECT routine_type, data_type, external_language \
                             FROM information_schema.routines \
                             WHERE routine_schema = $1 AND routine_name = $2",
                            &key,
                        )
                        .await?;
                        for row in &found {
                            info.details
                                .push(("Kind".to_owned(), cell(Some(row), 0).to_lowercase()));
                            info.details
                                .push(("Returns".to_owned(), cell(Some(row), 1)));
                            info.details
                                .push(("Language".to_owned(), cell(Some(row), 2).to_lowercase()));
                        }
                        if found.len() > 1 {
                            info.details
                                .push(("Overloads".to_owned(), found.len().to_string()));
                        }
                        match rows(
                            conn,
                            "SELECT param_names, body FROM nusadb_functions WHERE name = $1",
                            &key[1..],
                        )
                        .await
                        {
                            Ok(found) => {
                                if let Some(row) = found.first() {
                                    let params = cell(Some(row), 0);
                                    if !params.is_empty() {
                                        info.details.push(("Parameters".to_owned(), params));
                                    }
                                    info.definition = Some(cell(Some(row), 1).trim().to_owned());
                                }
                            },
                            Err(_) => info
                                .notes
                                .push("The body is visible to a superuser only.".to_owned()),
                        }
                    },
                }
                Ok(info)
            })
        });
    }

    fn security(&self, target: Target, reply: Reply<Security>) {
        self.with_connection(target, reply, move |conn| {
            Box::pin(async move { load_security(conn).await })
        });
    }

    fn listen(&self, target: Target, channels: Vec<String>, sink: crate::backend::ListenSink) {
        let key = (target.server, target.database.clone());
        let mut listeners = self
            .listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(tx) = listeners.get(&key)
            && tx.send(channels.clone()).is_ok()
        {
            return;
        }
        if channels.is_empty() {
            listeners.remove(&key);
            sink(crate::backend::ListenEvent::Listening(Vec::new()));
            return;
        }
        let (tx, rx) = tokio::sync::watch::channel(channels);
        listeners.insert(key.clone(), tx);
        let registry = Arc::clone(&self.listeners);
        self.runtime.spawn(async move {
            let outcome = listener(target, rx, &sink).await;
            registry
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&key);
            if let Err(message) = outcome {
                sink(crate::backend::ListenEvent::Failed(message));
            }
        });
    }

    fn copy_in(&self, target: Target, sql: String, data: Vec<u8>, reply: Reply<u64>) {
        self.with_connection(target, reply, move |conn| {
            Box::pin(async move {
                let mut source: &[u8] = &data;
                conn.copy_in(&sql, &mut source)
                    .await
                    .map_err(|e| describe_statement_error(&e))
            })
        });
    }

    fn schema_model(&self, target: Target, reply: Reply<crate::erd::Model>) {
        self.with_connection(target, reply, move |conn| {
            Box::pin(async move { load_model(conn).await })
        });
    }

    fn scrape(&self, url: String, reply: Reply<String>) {
        self.runtime.spawn(async move {
            reply(scrape(&url).await);
        });
    }

    fn run(&self, target: Target, statements: Vec<String>, reply: Reply<RunReport>) {
        self.with_connection(target, reply, move |conn| {
            Box::pin(async move {
                let mut reports = Vec::with_capacity(statements.len());
                for sql in statements {
                    let started = Instant::now();
                    let outcome = match conn.query(&sql).await {
                        Ok(result) => StatementOutcome::Done {
                            tag: result.tag.clone(),
                            result: ResultSet {
                                types: result.column_types.iter().map(|t| type_name(*t)).collect(),
                                columns: result.columns,
                                rows: result
                                    .rows
                                    .iter()
                                    .take(row_cap())
                                    .map(|row| row.values().iter().map(render).collect())
                                    .collect(),
                            },
                        },
                        Err(error) => StatementOutcome::Failed {
                            code: error.code().map(str::to_owned),
                            message: describe_statement_error(&error),
                        },
                    };
                    let failed = matches!(outcome, StatementOutcome::Failed { .. });
                    reports.push(StatementReport {
                        sql,
                        elapsed: started.elapsed(),
                        outcome,
                    });
                    if failed {
                        break;
                    }
                }
                let txn = match conn.transaction_status() {
                    TransactionStatus::InTransaction => TxnState::Active,
                    TransactionStatus::Failed => TxnState::Failed,
                    _ => TxnState::Idle,
                };
                Ok(RunReport {
                    statements: reports,
                    txn,
                })
            })
        });
    }

    fn export(
        &self,
        target: Target,
        sql: String,
        path: std::path::PathBuf,
        format: crate::export::Format,
        reply: Reply<usize>,
    ) {
        self.with_connection(target, reply, move |conn| {
            Box::pin(async move {
                let file = std::fs::File::create(&path)
                    .map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
                let table = crate::export::table_from_path(&path);
                let mut stream = conn
                    .query_stream(&sql, &[])
                    .await
                    .map_err(|e| describe(&e))?;
                let columns = stream.columns().to_vec();
                let types = stream
                    .column_types()
                    .iter()
                    .map(|t| type_name(*t))
                    .collect();
                let mut writer = crate::export::Writer::new(
                    std::io::BufWriter::new(file),
                    format,
                    columns,
                    types,
                )
                .map_err(|e| e.to_string())?
                .with_table(table);
                while let Some(row) = stream.next().await {
                    let row = row.map_err(|e| describe(&e))?;
                    let values: Vec<Option<String>> = row.values().iter().map(render).collect();
                    writer.row(&values).map_err(|e| e.to_string())?;
                }
                writer.finish().map_err(|e| e.to_string())
            })
        });
    }

    fn cancel(&self, target: &Target) {
        let handle = self
            .cancels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(target.server, target.database.clone()))
            .cloned();
        if let Some(handle) = handle {
            // The cancel request is its own short-lived socket and blocks while it is sent.
            self.runtime.spawn_blocking(move || {
                drop(handle.cancel());
            });
        }
    }

    fn disconnect(&self, server: u32) {
        let doomed: Vec<Session> = {
            let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            let keys: Vec<SessionKey> =
                sessions.keys().filter(|k| k.0 == server).cloned().collect();
            keys.iter().filter_map(|k| sessions.remove(k)).collect()
        };
        self.cancels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|k, _| k.0 != server);
        self.runtime.spawn(async move {
            for session in doomed {
                if let Some((_, conn)) = session.lock().await.take() {
                    conn.close().await;
                }
            }
        });
    }
}

/// `count(*)` of a relation, or `None` when it cannot be counted.
async fn count_rows(conn: &mut AsyncConnection, object: &DbObject) -> Option<u64> {
    let sql = format!(
        "SELECT count(*) FROM {}.{}",
        quote_ident(&object.schema),
        quote_ident(&object.name)
    );
    rows(conn, &sql, &[])
        .await
        .ok()
        .and_then(|r| cell(r.first(), 0).parse().ok())
}

/// Table and column comments.
async fn describe_comments(conn: &mut AsyncConnection, object: &DbObject, info: &mut ObjectInfo) {
    let Ok(found) = rows(
        conn,
        "SELECT object_type, object_name, description FROM information_schema.nusadb_comments \
         WHERE object_schema = $1 AND table_name = $2",
        &[object.schema.clone(), object.name.clone()],
    )
    .await
    else {
        return;
    };
    for row in found {
        let (kind, name, text) = (
            cell(Some(&row), 0),
            cell(Some(&row), 1),
            cell(Some(&row), 2),
        );
        if kind == "COLUMN" {
            if let Some(column) = info.columns.iter_mut().find(|c| c.name == name) {
                column.comment = Some(text);
            }
        } else {
            info.comment = Some(text);
        }
    }
}

/// Indexes, grouped from `information_schema.statistics` (one row per indexed column).
async fn describe_indexes(conn: &mut AsyncConnection, key: &[String; 2], info: &mut ObjectInfo) {
    let Ok(found) = rows(
        conn,
        "SELECT index_name, non_unique, seq_in_index, column_name FROM information_schema.statistics \
         WHERE table_schema = $1 AND table_name = $2",
        key,
    )
    .await
    else {
        info.notes.push("Indexes could not be read.".to_owned());
        return;
    };
    let mut parts: Vec<(String, bool, i64, String)> = found
        .iter()
        .map(|r| {
            (
                cell(Some(r), 0),
                cell(Some(r), 1) == "0",
                cell(Some(r), 2).parse().unwrap_or(0),
                cell(Some(r), 3),
            )
        })
        .collect();
    parts.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
    for (name, unique, _, column) in parts {
        match info.indexes.iter_mut().find(|i| i.name == name) {
            Some(index) => index.columns.push(column),
            None => info.indexes.push(IndexDef {
                name,
                unique,
                columns: vec![column],
            }),
        }
    }
}

async fn describe_triggers(conn: &mut AsyncConnection, key: &[String; 2], info: &mut ObjectInfo) {
    match rows(
        conn,
        "SELECT name, timing, events, for_each, \"when\", action, enabled FROM nusadb_triggers \
         WHERE schema = $1 AND \"table\" = $2",
        key,
    )
    .await
    {
        Ok(found) => {
            info.triggers = found
                .iter()
                .map(|r| TriggerDef {
                    name: cell(Some(r), 0),
                    timing: cell(Some(r), 1),
                    events: cell(Some(r), 2),
                    for_each: cell(Some(r), 3),
                    condition: cell(Some(r), 4),
                    action: cell(Some(r), 5),
                    enabled: cell(Some(r), 6) != "f",
                })
                .collect();
        },
        // 42P01: no trigger was ever created, so the catalog does not exist yet — that is
        // an empty list, not a missing one.
        Err(e) if e.contains("not found") || e.contains("42P01") => {},
        Err(_) => info
            .notes
            .push("Triggers are visible to a superuser only.".to_owned()),
    }
}

async fn describe_policies(conn: &mut AsyncConnection, key: &[String; 2], info: &mut ObjectInfo) {
    match rows(
        conn,
        "SELECT name, command, roles, \"using\", \"check\", permissive FROM nusadb_policies \
         WHERE schema = $1 AND \"table\" = $2",
        key,
    )
    .await
    {
        Ok(found) => {
            info.policies = found
                .iter()
                .map(|r| PolicyDef {
                    name: cell(Some(r), 0),
                    command: cell(Some(r), 1),
                    roles: cell(Some(r), 2),
                    using: cell(Some(r), 3),
                    check: cell(Some(r), 4),
                    permissive: cell(Some(r), 5) != "f",
                })
                .collect();
        },
        Err(e) if e.contains("not found") || e.contains("42P01") => {},
        Err(_) => info
            .notes
            .push("Row-level security policies are visible to a superuser only.".to_owned()),
    }
}

/// A view's columns from the shape of its result. NusaDB's `information_schema.columns` does
/// not list a view's columns, so `SELECT * … LIMIT 0` is asked instead: it returns no rows,
/// only the column names and types.
async fn result_columns(conn: &mut AsyncConnection, object: &DbObject, info: &mut ObjectInfo) {
    let sql = format!(
        "SELECT * FROM {}.{} LIMIT 0",
        quote_ident(&object.schema),
        quote_ident(&object.name)
    );
    if let Ok(result) = conn.query(&sql).await {
        info.columns = result
            .columns
            .iter()
            .zip(result.column_types.iter())
            .map(|(name, tag)| ColumnDef {
                name: name.clone(),
                data_type: type_name(*tag),
                type_spec: type_name(*tag),
                nullable: true,
                default: None,
                comment: None,
            })
            .collect();
        info.notes.push(
            "Column types come from the view's result; NusaDB's information_schema.columns \
             does not list a view's columns."
                .to_owned(),
        );
    }
}

/// Tables, columns and foreign keys of the whole database, read in five catalog queries
/// rather than one description per table.
async fn load_model(conn: &mut AsyncConnection) -> Result<crate::erd::Model, String> {
    use crate::erd::{Column, ForeignKey, Model, Table};
    let tables = rows(
        conn,
        "SELECT table_schema, table_name FROM information_schema.tables \
         WHERE table_type = 'BASE TABLE' AND table_schema <> 'information_schema' \
         ORDER BY table_schema, table_name",
        &[],
    )
    .await?;
    // information_schema.tables reports a materialized view as a base table; it has no keys.
    let matviews: Vec<String> = rows(conn, "SELECT name FROM nusadb_matviews", &[])
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| cell(Some(r), 0))
        .collect();
    let columns = rows(
        conn,
        "SELECT table_schema, table_name, column_name, data_type, is_nullable, \
         character_maximum_length, numeric_precision, numeric_scale \
         FROM information_schema.columns WHERE table_schema <> 'information_schema' \
         ORDER BY table_schema, table_name, ordinal_position",
        &[],
    )
    .await?;
    let declared = rows(
        conn,
        "SELECT table_schema, table_name, constraint_name, constraint_type \
         FROM information_schema.table_constraints",
        &[],
    )
    .await?;
    let usage = rows(
        conn,
        "SELECT table_schema, constraint_name, column_name, ordinal_position \
         FROM information_schema.key_column_usage",
        &[],
    )
    .await?;
    let referential = rows(
        conn,
        "SELECT constraint_schema, constraint_name, unique_constraint_name \
         FROM information_schema.referential_constraints",
        &[],
    )
    .await
    .unwrap_or_default();

    let columns_of = |schema: &str, name: &str| -> Vec<String> {
        let mut cols: Vec<(i64, String)> = usage
            .iter()
            .filter(|r| cell(Some(r), 0) == schema && cell(Some(r), 1) == name)
            .map(|r| (cell(Some(r), 3).parse().unwrap_or(0), cell(Some(r), 2)))
            .collect();
        cols.sort();
        cols.into_iter().map(|(_, c)| c).collect()
    };
    let table_of = |schema: &str, constraint: &str| {
        declared
            .iter()
            .find(|d| cell(Some(d), 0) == schema && cell(Some(d), 2) == constraint)
            .map(|d| (cell(Some(d), 0), cell(Some(d), 1)))
    };

    let mut foreign_keys = Vec::new();
    for r in &referential {
        let (schema, name, target) = (cell(Some(r), 0), cell(Some(r), 1), cell(Some(r), 2));
        let (Some(from), Some(to)) = (table_of(&schema, &name), table_of(&schema, &target)) else {
            continue;
        };
        foreign_keys.push(ForeignKey {
            from_columns: columns_of(&schema, &name),
            to_columns: columns_of(&schema, &target),
            name,
            from,
            to,
        });
    }
    let keyed = |schema: &str, table: &str, column: &str, kind: &str| {
        declared.iter().any(|d| {
            cell(Some(d), 0) == schema
                && cell(Some(d), 1) == table
                && cell(Some(d), 3) == kind
                && columns_of(schema, &cell(Some(d), 2))
                    .iter()
                    .any(|c| c == column)
        })
    };

    let mut model = Model {
        tables: Vec::new(),
        foreign_keys,
    };
    for t in &tables {
        let (schema, name) = (cell(Some(t), 0), cell(Some(t), 1));
        if schema == "public" && matviews.contains(&name) {
            continue;
        }
        let cols = columns
            .iter()
            .filter(|c| cell(Some(c), 0) == schema && cell(Some(c), 1) == name)
            .map(|c| {
                let column = cell(Some(c), 2);
                let spec = type_spec(
                    &cell(Some(c), 3),
                    &cell(Some(c), 5),
                    &cell(Some(c), 6),
                    &cell(Some(c), 7),
                );
                Column {
                    primary: keyed(&schema, &name, &column, "PRIMARY KEY"),
                    foreign: keyed(&schema, &name, &column, "FOREIGN KEY"),
                    nullable: cell(Some(c), 4) != "NO",
                    data_type: short_type_name(&spec),
                    name: column,
                }
            })
            .collect();
        model.tables.push(Table {
            schema,
            name,
            columns: cols,
        });
    }
    Ok(model)
}

/// A compact type name for a diagram box: `int`, `timestamptz`, `varchar(80)`.
fn short_type_name(spec: &str) -> String {
    match spec {
        "integer" => "int",
        "bigint" => "int8",
        "smallint" => "int2",
        "boolean" => "bool",
        "double precision" => "float8",
        "real" => "float4",
        "timestamp with time zone" => "timestamptz",
        "timestamp without time zone" => "timestamp",
        "time with time zone" => "timetz",
        "time without time zone" => "time",
        other => other,
    }
    .to_owned()
}

/// The listener loop: its own connection, `LISTEN`/`UNLISTEN` as the wanted set changes, and
/// every notification handed to `sink`. Ends (closing the connection) when the set is empty.
async fn listener(
    target: Target,
    mut wanted: tokio::sync::watch::Receiver<Vec<String>>,
    sink: &crate::backend::ListenSink,
) -> Result<(), String> {
    use crate::backend::ListenEvent;
    let mut params = target.params.clone();
    params.database.clone_from(&target.database);
    let config = build_config(&params)?;
    let mut conn = tokio::time::timeout(CONNECT_TIMEOUT, AsyncConnection::connect_config(&config))
        .await
        .map_err(|_| "The listener could not connect in time".to_owned())?
        .map_err(|e| describe(&e))?;
    let mut held: Vec<String> = Vec::new();
    loop {
        let want = wanted.borrow_and_update().clone();
        if want.is_empty() {
            let _ = conn.query("UNLISTEN *").await;
            conn.close().await;
            sink(ListenEvent::Listening(Vec::new()));
            return Ok(());
        }
        if want != held {
            for gone in held.iter().filter(|c| !want.contains(c)) {
                conn.query(&format!("UNLISTEN {}", quote_ident(gone)))
                    .await
                    .map_err(|e| describe_statement_error(&e))?;
            }
            for new in want.iter().filter(|c| !held.contains(c)) {
                conn.query(&format!("LISTEN {}", quote_ident(new)))
                    .await
                    .map_err(|e| describe_statement_error(&e))?;
            }
            held = want;
            sink(ListenEvent::Listening(held.clone()));
        }
        // The driver's own timeout never cuts a frame in half; cancelling the poll from outside
        // could, so a change of channels is picked up between polls instead (within 500 ms).
        match conn
            .poll_notification(Some(Duration::from_millis(500)))
            .await
        {
            Ok(Some(n)) => sink(ListenEvent::Notification {
                channel: n.channel,
                payload: n.payload,
                pid: n.pid,
            }),
            Ok(None) => {},
            Err(e) => {
                return Err(format!(
                    "The listener's connection failed: {}",
                    describe(&e)
                ));
            },
        }
        if wanted.has_changed().is_err() {
            // Nobody can change the set any more: stop listening.
            conn.close().await;
            return Ok(());
        }
    }
}

/// The largest metrics body read; NusaDB's is about 1.5 KiB.
const MAX_SCRAPE_BYTES: u64 = 1 << 20;

/// One plain HTTP/1.1 GET of the metrics endpoint. The endpoint is unauthenticated plain HTTP
/// (`docs/deployment.md`, *Metrics*), answers with `Content-Length` and closes, so a minimal
/// client is all it needs; chunked bodies are decoded too in case a proxy sits in between.
async fn scrape(url: &str) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (host, port, path) = crate::metrics::split_url(url)
        .ok_or_else(|| format!("{url} is not an http://host:port address"))?;
    let connect = tokio::net::TcpStream::connect((host.as_str(), port));
    let mut stream = tokio::time::timeout(Duration::from_secs(3), connect)
        .await
        .map_err(|_| format!("No answer from {host}:{port} within 3s"))?
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::ConnectionRefused => format!(
                "Nothing is listening on {host}:{port} . Was the server started with --metrics-listen?"
            ),
            _ => format!("Could not reach {host}:{port}: {e}"),
        })?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: text/plain\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("Could not send the request: {e}"))?;
    let mut raw = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        (&mut stream).take(MAX_SCRAPE_BYTES).read_to_end(&mut raw),
    )
    .await
    .map_err(|_| "The metrics endpoint did not finish answering within 5s".to_owned())?
    .map_err(|e| format!("Could not read the answer: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or("The metrics endpoint did not answer in HTTP")?;
    let status = head.lines().next().unwrap_or_default();
    if !status
        .split_whitespace()
        .nth(1)
        .is_some_and(|code| code == "200")
    {
        return Err(format!("The metrics endpoint answered “{status}”"));
    }
    let chunked = head
        .lines()
        .any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:") && l.contains("chunked"));
    Ok(if chunked {
        dechunk(body)
    } else {
        body.to_owned()
    })
}

/// Decode a chunked HTTP body.
fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16) else {
            break;
        };
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].trim_start_matches("\r\n");
    }
    out
}

/// Whether an error is "this catalog has never been written to": NusaDB answers `42P01` for a
/// `nusadb_*` catalog until its first row, which means "empty", not "missing".
fn never_written(error: &str) -> bool {
    error.contains("42P01") || error.contains("not found")
}

/// Everything the Security view shows for one database. Roles, RLS state and policies come from
/// superuser-only catalogs; memberships and grants from `information_schema`, which every role
/// may read. What cannot be read is left empty and said so in `notes`.
async fn load_security(conn: &mut AsyncConnection) -> Result<Security, String> {
    let mut out = Security {
        roles_readable: true,
        rls_readable: true,
        ..Security::default()
    };
    match rows(
        conn,
        "SELECT name, superuser, login, createdb, createrole, inherit FROM nusadb_roles \
         ORDER BY name",
        &[],
    )
    .await
    {
        Ok(found) => {
            let on = |r: &Vec<Option<String>>, i| cell(Some(r), i) == "t";
            out.roles = found
                .iter()
                .map(|r| Role {
                    name: cell(Some(r), 0),
                    superuser: on(r, 1),
                    login: on(r, 2),
                    createdb: on(r, 3),
                    createrole: on(r, 4),
                    inherit: on(r, 5),
                    exists: true,
                })
                .collect();
        },
        Err(e) if never_written(&e) => {},
        Err(_) => {
            out.roles_readable = false;
            out.notes.push(
                "Role attributes are in nusadb_roles, which only a superuser may read: roles \
                 are listed by name, read-only."
                    .to_owned(),
            );
        },
    }

    for r in rows(
        conn,
        "SELECT role_name, grantee, is_grantable FROM information_schema.applicable_roles",
        &[],
    )
    .await
    .unwrap_or_default()
    {
        out.members.insert(Membership {
            role: cell(Some(&r), 0),
            member: cell(Some(&r), 1),
            admin: cell(Some(&r), 2) == "YES",
        });
    }

    out.relations = rows(
        conn,
        "SELECT table_schema, table_name, table_type FROM information_schema.tables \
         WHERE table_schema <> 'information_schema' ORDER BY table_schema, table_name",
        &[],
    )
    .await?
    .iter()
    .map(|r| Relation {
        schema: cell(Some(r), 0),
        name: cell(Some(r), 1),
        view: cell(Some(r), 2).contains("VIEW"),
    })
    .collect();

    // information_schema.tables reports a materialized view as a table; it takes grants but
    // not row-level security, so it is marked to keep it out of the RLS list.
    if let Ok(found) = rows(conn, "SELECT name FROM nusadb_matviews", &[]).await {
        for r in found {
            let name = cell(Some(&r), 0);
            for rel in out.relations.iter_mut().filter(|x| x.name == name) {
                rel.view = true;
            }
        }
    }

    for r in rows(
        conn,
        "SELECT grantee, table_schema, table_name, privilege_type, is_grantable \
         FROM information_schema.table_privileges",
        &[],
    )
    .await?
    {
        let privilege = cell(Some(&r), 3);
        let Some(p) = crate::security::PRIVILEGES
            .iter()
            .position(|x| x.eq_ignore_ascii_case(&privilege))
        else {
            continue;
        };
        let grant = if cell(Some(&r), 4) == "YES" {
            Grant::WithGrantOption
        } else {
            Grant::Granted
        };
        out.grants.insert(
            (cell(Some(&r), 0), cell(Some(&r), 1), cell(Some(&r), 2), p),
            grant,
        );
    }

    // Names that hold a grant or a membership but are not in the role list (a non-superuser
    // view, or a grant to PUBLIC) still need a row in the matrix.
    let mut names: std::collections::BTreeSet<String> =
        out.grants.keys().map(|k| k.0.clone()).collect();
    for m in &out.members {
        names.insert(m.role.clone());
        names.insert(m.member.clone());
    }
    for name in names {
        if name != crate::security::PUBLIC && !out.roles.iter().any(|r| r.name == name) {
            let mut role = Role::new(&name);
            role.exists = true;
            out.roles.push(role);
        }
    }

    match rows(conn, "SELECT schema, \"table\" FROM nusadb_rls", &[]).await {
        Ok(found) => {
            out.rls = found
                .iter()
                .map(|r| (cell(Some(r), 0), cell(Some(r), 1)))
                .collect();
        },
        Err(e) if never_written(&e) => {},
        Err(_) => out.rls_readable = false,
    }
    match rows(
        conn,
        "SELECT schema, \"table\", name, command, roles, \"using\", \"check\", permissive \
         FROM nusadb_policies ORDER BY schema, \"table\", name",
        &[],
    )
    .await
    {
        Ok(found) => {
            out.policies = found
                .iter()
                .map(|r| Policy {
                    schema: cell(Some(r), 0),
                    table: cell(Some(r), 1),
                    name: cell(Some(r), 2),
                    command: cell(Some(r), 3).to_uppercase(),
                    roles: cell(Some(r), 4).replace(',', ", "),
                    using: cell(Some(r), 5),
                    check: cell(Some(r), 6),
                    permissive: cell(Some(r), 7) != "f",
                    original: Some(cell(Some(r), 2)),
                })
                .collect();
        },
        Err(e) if never_written(&e) => {},
        Err(_) => out.rls_readable = false,
    }
    if !out.rls_readable {
        out.policies.clear();
        out.notes
            .push("Row-level security is visible to superusers only.".to_owned());
    }
    Ok(out)
}

/// Run `sql` with text parameters and return its rows rendered as text.
async fn rows(
    conn: &mut AsyncConnection,
    sql: &str,
    params: &[String],
) -> Result<Vec<Vec<Option<String>>>, String> {
    let result = if params.is_empty() {
        conn.query(sql).await
    } else {
        let bound: Vec<&str> = params.iter().map(String::as_str).collect();
        let refs: Vec<&(dyn nusadb::ToSql + Sync)> = bound
            .iter()
            .map(|p| p as &(dyn nusadb::ToSql + Sync))
            .collect();
        let plain: Vec<&dyn nusadb::ToSql> =
            refs.iter().map(|p| *p as &dyn nusadb::ToSql).collect();
        conn.query_params(sql, &plain).await
    }
    .map_err(|e| describe(&e))?;
    Ok(result
        .rows
        .iter()
        .map(|row| row.values().iter().map(render).collect())
        .collect())
}

/// Column `i` of `row` as text; empty for NULL or a missing row.
fn cell(row: Option<&Vec<Option<String>>>, i: usize) -> String {
    row.and_then(|r| r.get(i).cloned().flatten())
        .unwrap_or_default()
}

/// The constraints of one table, foreign keys resolved to the table and columns they point at.
async fn constraints(
    conn: &mut AsyncConnection,
    key: &[String; 2],
) -> Result<Vec<Constraint>, String> {
    let declared = rows(
        conn,
        "SELECT constraint_name, constraint_type FROM information_schema.table_constraints \
         WHERE table_schema = $1 AND table_name = $2",
        key,
    )
    .await?;
    let usage = rows(
        conn,
        "SELECT constraint_name, column_name, ordinal_position, table_name \
         FROM information_schema.key_column_usage WHERE table_schema = $1",
        &key[..1],
    )
    .await?;
    let all_declared = rows(
        conn,
        "SELECT constraint_name, table_name FROM information_schema.table_constraints \
         WHERE table_schema = $1",
        &key[..1],
    )
    .await?;
    let checks = rows(
        conn,
        "SELECT constraint_name, check_clause FROM information_schema.check_constraints \
         WHERE constraint_schema = $1",
        &key[..1],
    )
    .await
    .unwrap_or_default();
    let referential = rows(
        conn,
        "SELECT constraint_name, unique_constraint_name, update_rule, delete_rule \
         FROM information_schema.referential_constraints WHERE constraint_schema = $1",
        &key[..1],
    )
    .await
    .unwrap_or_default();

    let columns_of = |name: &str| -> Vec<String> {
        let mut cols: Vec<(i64, String)> = usage
            .iter()
            .filter(|r| cell(Some(r), 0) == name)
            .map(|r| (cell(Some(r), 2).parse().unwrap_or(0), cell(Some(r), 1)))
            .collect();
        cols.sort();
        cols.into_iter().map(|(_, c)| c).collect()
    };

    let mut out: Vec<Constraint> = declared
        .iter()
        .map(|row| {
            let name = cell(Some(row), 0);
            let kind = cell(Some(row), 1);
            let fk = referential.iter().find(|r| cell(Some(r), 0) == name);
            let references = fk.map(|r| {
                let target = cell(Some(r), 1);
                let table = all_declared
                    .iter()
                    .find(|d| cell(Some(d), 0) == target)
                    .map(|d| cell(Some(d), 1))
                    .unwrap_or_default();
                (table, columns_of(&target))
            });
            Constraint {
                columns: columns_of(&name),
                references,
                on_update: fk.map(|r| cell(Some(r), 2)).filter(|v| !v.is_empty()),
                on_delete: fk.map(|r| cell(Some(r), 3)).filter(|v| !v.is_empty()),
                check: checks
                    .iter()
                    .find(|r| cell(Some(r), 0) == name)
                    .map(|r| cell(Some(r), 1)),
                name,
                kind,
            }
        })
        .collect();
    // Primary key first, then unique, then foreign keys — the order a reader looks for them.
    let rank = |k: &str| match k {
        "PRIMARY KEY" => 0,
        "UNIQUE" => 1,
        "FOREIGN KEY" => 2,
        _ => 3,
    };
    out.sort_by(|a, b| rank(&a.kind).cmp(&rank(&b.kind)).then(a.name.cmp(&b.name)));
    Ok(out)
}

/// A column's declared type with its length or precision put back.
fn type_spec(data_type: &str, length: &str, precision: &str, scale: &str) -> String {
    match data_type {
        "character varying" | "character" if !length.is_empty() => {
            let short = if data_type == "character" {
                "char"
            } else {
                "varchar"
            };
            format!("{short}({length})")
        },
        "numeric" if !precision.is_empty() => {
            if scale.is_empty() || scale == "0" {
                format!("numeric({precision})")
            } else {
                format!("numeric({precision},{scale})")
            }
        },
        other => other.to_owned(),
    }
}

/// Render one value for the grid; `None` is SQL `NULL`.
fn render(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Text(text) | Value::Numeric(text) | Value::Typed { text, .. } => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// The SQL name of a column type, as the grid header shows it.
fn type_name(tag: TypeTag) -> String {
    match tag {
        TypeTag::Unknown => "?".to_owned(),
        TypeTag::Int => "int".to_owned(),
        TypeTag::TimestampTz => "timestamptz".to_owned(),
        TypeTag::TimeTz => "timetz".to_owned(),
        TypeTag::ArrayOf(_) | TypeTag::Array => "array".to_owned(),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// A statement error in the user's terms: the server's own message, which already names the
/// object it is about, without the driver's "server error [..]" framing.
fn describe_statement_error(error: &Error) -> String {
    match error {
        Error::Server { message, .. } => message.clone(),
        other => describe(other),
    }
}

impl ConnectionProbe for DriverProbe {
    fn probe(&self, params: &ConnectionParams, report: ProbeReport) {
        let config = match build_config(params) {
            Ok(config) => config,
            // A bad CA file is a local problem, not a server one; answer immediately rather
            // than opening a socket that was never going to be trusted.
            Err(message) => {
                report(ProbeOutcome::Failed { message });
                return;
            },
        };

        let endpoint = params.endpoint();
        self.runtime.spawn(async move {
            report(attempt(&config, &endpoint).await);
        });
    }
}

/// How long a connection test may wait before it is reported as unreachable.
///
/// Without a bound, a host that silently drops packets leaves the banner on "Testing…" until
/// the operating system gives up, which can be minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// Translate connection parameters into the driver's configuration.
fn build_config(params: &ConnectionParams) -> Result<Config, String> {
    let mut config = Config {
        host: params.host.clone(),
        port: params.port,
        user: params.user.clone(),
        database: params.database.clone(),
        // An empty password means trust-on-startup, which is a different handshake from an
        // empty SCRAM secret — so it is sent as no password rather than as "". The driver's
        // default carries a password, so this must be set explicitly either way.
        password: (!params.password.is_empty()).then(|| params.password.clone()),
        connect_timeout: Some(CONNECT_TIMEOUT),
        ..Config::default()
    };

    if params.tls {
        let pem = fs::read(&params.ca_path)
            .map_err(|error| format!("Cannot read the CA bundle at {}: {error}", params.ca_path))?;
        if !String::from_utf8_lossy(&pem).contains("-----BEGIN CERTIFICATE-----") {
            return Err(format!(
                "The file at {} is not a PEM certificate bundle.",
                params.ca_path
            ));
        }
        config.tls = Some(TlsConfig::with_ca_pem(pem));
    }

    Ok(config)
}

/// Connect, prove the session works, and disconnect again.
///
/// A handshake alone is not proof of a usable database — the role may authenticate and still
/// be unable to run anything — so one trivial round trip is made before calling it reachable.
async fn attempt(config: &Config, endpoint: &str) -> ProbeOutcome {
    let mut conn = match AsyncConnection::connect_config(config).await {
        Ok(conn) => conn,
        Err(error) => {
            return ProbeOutcome::Failed {
                message: describe(&error),
            };
        },
    };

    let outcome = match conn.query("SELECT 1").await {
        Ok(_) => ProbeOutcome::Reachable {
            detail: format!("{endpoint} answered: authenticated and serving queries."),
        },
        Err(error) => ProbeOutcome::Failed {
            message: describe(&error),
        },
    };

    // The test holds no session, so the socket is closed either way.
    conn.close().await;
    outcome
}

/// Put a driver error in the user's terms.
fn describe(error: &Error) -> String {
    match error {
        Error::Io(inner) if inner.kind() == std::io::ErrorKind::TimedOut => format!(
            "No answer within {}s. Check the host, the port, and any firewall in between.",
            CONNECT_TIMEOUT.as_secs()
        ),
        Error::Io(inner) if inner.kind() == std::io::ErrorKind::ConnectionRefused => {
            "Connection refused. Nothing is listening on that port.".to_owned()
        },
        Error::Io(inner) => format!("Could not reach the server: {inner}"),
        Error::Auth(inner) => format!("Authentication failed: {inner}"),
        Error::Server { code, message } => {
            format!("Server rejected the connection [{code}]: {message}")
        },
        Error::Protocol(inner) => format!("The server spoke an unexpected protocol: {inner}"),
        Error::Config(inner) => format!("The connection settings are not usable: {inner}"),
        other => other.to_string(),
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
            Err(message) => {
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
