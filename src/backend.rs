//! The port Studio talks to a live database through, and the plain data it gets back.
//!
//! Like [`crate::probe`], this module knows nothing about sockets or the wire client: the
//! controller asks for databases, objects, columns or a script run, and receives display-ready
//! values. [`crate::driver`] fulfils the port over the `nusadb` crate; tests can fulfil it with
//! anything.
//!
//! NusaDB runs one engine per database, so a session belongs to a `(server, database)` pair —
//! a [`Target`] — and the backend keeps one open per target it has been asked about.

use std::time::Duration;

use crate::model::ConnectionParams;

/// Where a request goes: a saved server, and one database on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub server: u32,
    pub params: ConnectionParams,
    pub database: String,
}

/// Answer to a request, delivered from a background thread.
pub type Reply<T> = Box<dyn FnOnce(Result<T, String>) + Send + 'static>;

/// A table or view found by introspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbObject {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Sequence,
    Function,
}

impl ObjectKind {
    /// Every kind, in the order the navigator lists them.
    pub const ALL: [Self; 5] = [
        Self::Table,
        Self::View,
        Self::MaterializedView,
        Self::Sequence,
        Self::Function,
    ];

    /// Whether the object holds rows that can be browsed.
    #[must_use]
    pub const fn has_rows(self) -> bool {
        matches!(self, Self::Table | Self::View | Self::MaterializedView)
    }

    /// Singular noun, capitalised: `Table`.
    #[must_use]
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Table => "Table",
            Self::View => "View",
            Self::MaterializedView => "Materialized view",
            Self::Sequence => "Sequence",
            Self::Function => "Function",
        }
    }

    /// Group heading in the navigator: `Tables`.
    #[must_use]
    pub const fn plural(self) -> &'static str {
        match self {
            Self::Table => "Tables",
            Self::View => "Views",
            Self::MaterializedView => "Materialized views",
            Self::Sequence => "Sequences",
            Self::Function => "Functions",
        }
    }
}

/// What the server says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    /// `version()` — e.g. `NusaDB 0.1.0`.
    pub version: String,
    pub databases: Vec<String>,
}

/// A table constraint, with its columns in key order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
    pub name: String,
    /// `PRIMARY KEY` | `UNIQUE` | `FOREIGN KEY` | `CHECK` …
    pub kind: String,
    pub columns: Vec<String>,
    /// For a foreign key: the referenced table and its columns.
    pub references: Option<(String, Vec<String>)>,
    pub on_update: Option<String>,
    pub on_delete: Option<String>,
    /// For a CHECK constraint: its condition.
    pub check: Option<String>,
}

/// An index on a table. Indexes that back a constraint carry its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
}

/// A trigger, as `nusadb_triggers` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerDef {
    pub name: String,
    /// `before` | `after` | `instead of`
    pub timing: String,
    /// `insert`, `update or delete` …
    pub events: String,
    /// `row` | `statement`
    pub for_each: String,
    pub condition: String,
    pub action: String,
    pub enabled: bool,
}

/// A row-level security policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDef {
    pub name: String,
    /// `ALL` | `SELECT` | `INSERT` | `UPDATE` | `DELETE`
    pub command: String,
    pub roles: String,
    pub using: String,
    pub check: String,
    pub permissive: bool,
}

/// Everything the inspector shows about one object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectInfo {
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<Constraint>,
    /// Rows in a table or view, when they could be counted.
    pub row_count: Option<u64>,
    /// Kind-specific facts: a sequence's start and increment, a function's return type.
    pub details: Vec<(String, String)>,
    pub indexes: Vec<IndexDef>,
    pub triggers: Vec<TriggerDef>,
    pub policies: Vec<PolicyDef>,
    /// `COMMENT ON` the object itself.
    pub comment: Option<String>,
    /// The body of a function, or the query of a view or materialized view.
    pub definition: Option<String>,
    /// Some catalogs are readable only by a superuser; when one could not be read, this says
    /// what is missing so the inspector does not present a partial picture as complete.
    pub notes: Vec<String>,
}

/// What a database holds: its schemas, and the tables and views inside them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub schemas: Vec<String>,
    pub objects: Vec<DbObject>,
}

/// One column of a table, as the inspector describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    /// `information_schema.columns.data_type`: `numeric`, `character varying`.
    pub data_type: String,
    /// The full declared type, with length or precision: `numeric(12,2)`, `varchar(20)`.
    pub type_spec: String,
    pub nullable: bool,
    pub default: Option<String>,
    /// `COMMENT ON COLUMN`.
    pub comment: Option<String>,
}

/// A grid of rows, already rendered to text. `None` is SQL `NULL`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub types: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// How one statement of a script went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementReport {
    pub sql: String,
    pub elapsed: Duration,
    pub outcome: StatementOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementOutcome {
    /// The server's command tag (`SELECT 3`, `INSERT 1`, …) and any rows it returned.
    Done { tag: String, result: ResultSet },
    /// The server refused the statement. `code` is the SQLSTATE when the server sent one.
    Failed {
        code: Option<String>,
        message: String,
    },
}

/// A script run: one report per statement that was attempted (a failure stops the run), and
/// whether the session is left inside a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub statements: Vec<StatementReport>,
    pub txn: TxnState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    Idle,
    Active,
    /// A statement failed inside the transaction; only `ROLLBACK` is accepted now.
    Failed,
}

impl TxnState {
    #[must_use]
    pub const fn as_tag(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Active => "active",
            Self::Failed => "failed",
        }
    }
}

impl RunReport {
    /// The first statement that failed, if any.
    #[must_use]
    pub fn failure(&self) -> Option<&StatementReport> {
        self.statements
            .iter()
            .find(|s| matches!(s.outcome, StatementOutcome::Failed { .. }))
    }

    /// The last statement that produced columns — what the grid should show.
    #[must_use]
    pub fn last_result(&self) -> Option<&ResultSet> {
        self.statements.iter().rev().find_map(|s| match &s.outcome {
            StatementOutcome::Done { result, .. } if !result.columns.is_empty() => Some(result),
            _ => None,
        })
    }

    /// Wall time of every statement together.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.statements.iter().map(|s| s.elapsed).sum()
    }
}

/// What the listener of one database reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenEvent {
    /// Subscribed to exactly these channels now.
    Listening(Vec<String>),
    Notification {
        channel: String,
        payload: String,
        /// Backend pid of the session that sent it.
        pid: u32,
    },
    /// The listener stopped; why.
    Failed(String),
}

/// Where listener events go; called on the driver's threads.
pub type ListenSink = std::sync::Arc<dyn Fn(ListenEvent) + Send + Sync>;

/// The live-database port.
pub trait Backend {
    /// The server's version and its databases (`target.database` is the one to log in to).
    fn server_info(&self, target: Target, reply: Reply<ServerInfo>);
    /// Schemas and objects of `target.database`.
    fn catalog(&self, target: Target, reply: Reply<Catalog>);
    /// Columns, constraints, row count and details of one object.
    fn describe(&self, target: Target, object: DbObject, reply: Reply<ObjectInfo>);
    /// Stream the full result of `sql` into a file at `path`, returning how many rows it holds.
    fn export(
        &self,
        target: Target,
        sql: String,
        path: std::path::PathBuf,
        format: crate::export::Format,
        reply: Reply<usize>,
    );
    /// Run `statements` in order on the target's session, stopping at the first failure.
    fn run(&self, target: Target, statements: Vec<String>, reply: Reply<RunReport>);
    /// Roles, memberships, table grants and row-level security of `target.database`.
    fn security(&self, target: Target, reply: Reply<crate::security::Security>);
    /// Fetch the body of a server's Prometheus endpoint (`http://host:port/path`).
    fn scrape(&self, url: String, reply: Reply<String>);
    /// Every table of `target.database` with its columns and foreign keys, for the diagram.
    fn schema_model(&self, target: Target, reply: Reply<crate::erd::Model>);
    /// Keep a listener on `target.database` subscribed to exactly `channels`, on a connection of
    /// its own (a listener only hears between its own statements). An empty list stops it.
    fn listen(&self, target: Target, channels: Vec<String>, sink: ListenSink);
    /// Run `COPY … FROM STDIN` (`sql`) with `data` as its stream; the rows loaded.
    fn copy_in(&self, target: Target, sql: String, data: Vec<u8>, reply: Reply<u64>);
    /// Ask the server to cancel whatever is running on the target's session.
    fn cancel(&self, target: &Target);
    /// Close every session held for `server`.
    fn disconnect(&self, server: u32);
}

impl<T: Backend + ?Sized> Backend for std::rc::Rc<T> {
    fn server_info(&self, target: Target, reply: Reply<ServerInfo>) {
        (**self).server_info(target, reply);
    }
    fn catalog(&self, target: Target, reply: Reply<Catalog>) {
        (**self).catalog(target, reply);
    }
    fn describe(&self, target: Target, object: DbObject, reply: Reply<ObjectInfo>) {
        (**self).describe(target, object, reply);
    }
    fn run(&self, target: Target, statements: Vec<String>, reply: Reply<RunReport>) {
        (**self).run(target, statements, reply);
    }
    fn security(&self, target: Target, reply: Reply<crate::security::Security>) {
        (**self).security(target, reply);
    }
    fn scrape(&self, url: String, reply: Reply<String>) {
        (**self).scrape(url, reply);
    }
    fn schema_model(&self, target: Target, reply: Reply<crate::erd::Model>) {
        (**self).schema_model(target, reply);
    }
    fn listen(&self, target: Target, channels: Vec<String>, sink: ListenSink) {
        (**self).listen(target, channels, sink);
    }
    fn copy_in(&self, target: Target, sql: String, data: Vec<u8>, reply: Reply<u64>) {
        (**self).copy_in(target, sql, data, reply);
    }
    fn export(
        &self,
        target: Target,
        sql: String,
        path: std::path::PathBuf,
        format: crate::export::Format,
        reply: Reply<usize>,
    ) {
        (**self).export(target, sql, path, format, reply);
    }
    fn cancel(&self, target: &Target) {
        (**self).cancel(target);
    }
    fn disconnect(&self, server: u32) {
        (**self).disconnect(server);
    }
}

/// A backend for when the driver could not start: every request fails with the reason.
#[derive(Debug, Clone)]
pub struct OfflineBackend {
    reason: String,
}

impl OfflineBackend {
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl Backend for OfflineBackend {
    fn server_info(&self, _: Target, reply: Reply<ServerInfo>) {
        reply(Err(self.reason.clone()));
    }
    fn catalog(&self, _: Target, reply: Reply<Catalog>) {
        reply(Err(self.reason.clone()));
    }
    fn describe(&self, _: Target, _: DbObject, reply: Reply<ObjectInfo>) {
        reply(Err(self.reason.clone()));
    }
    fn run(&self, _: Target, _: Vec<String>, reply: Reply<RunReport>) {
        reply(Err(self.reason.clone()));
    }
    fn security(&self, _: Target, reply: Reply<crate::security::Security>) {
        reply(Err(self.reason.clone()));
    }
    fn scrape(&self, _: String, reply: Reply<String>) {
        reply(Err(self.reason.clone()));
    }
    fn schema_model(&self, _: Target, reply: Reply<crate::erd::Model>) {
        reply(Err(self.reason.clone()));
    }
    fn listen(&self, _: Target, _: Vec<String>, sink: ListenSink) {
        sink(ListenEvent::Failed(self.reason.clone()));
    }
    fn copy_in(&self, _: Target, _: String, _: Vec<u8>, reply: Reply<u64>) {
        reply(Err(self.reason.clone()));
    }
    fn export(
        &self,
        _: Target,
        _: String,
        _: std::path::PathBuf,
        _: crate::export::Format,
        reply: Reply<usize>,
    ) {
        reply(Err(self.reason.clone()));
    }
    fn cancel(&self, _: &Target) {}
    fn disconnect(&self, _: u32) {}
}

/// Format a duration the way a status bar wants it: `0.8 ms`, `34 ms`, `2.41 s`.
#[must_use]
pub fn format_elapsed(elapsed: Duration) -> String {
    let micros = elapsed.as_micros();
    if micros < 1_000 {
        format!("{:.1} ms", micros as f64 / 1_000.0)
    } else if micros < 1_000_000 {
        format!("{} ms", micros / 1_000)
    } else {
        format!("{:.2} s", elapsed.as_secs_f64())
    }
}

/// `1234567` → `1,234,567`.
#[must_use]
pub fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `1 row` / `2 rows`.
#[must_use]
pub fn rows_label(n: usize) -> String {
    if n == 1 {
        "1 row".to_owned()
    } else {
        format!("{} rows", group_thousands(n))
    }
}

/// Quote an identifier for SQL, doubling any embedded quote.
#[must_use]
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(tag: &str, columns: &[&str]) -> StatementReport {
        StatementReport {
            sql: String::new(),
            elapsed: Duration::from_millis(2),
            outcome: StatementOutcome::Done {
                tag: tag.to_owned(),
                result: ResultSet {
                    columns: columns.iter().map(|c| (*c).to_owned()).collect(),
                    ..ResultSet::default()
                },
            },
        }
    }

    #[test]
    fn the_grid_shows_the_last_statement_that_returned_columns() {
        let report = RunReport {
            statements: vec![done("SELECT 1", &["a"]), done("INSERT 1", &[])],
            txn: TxnState::Idle,
        };
        assert_eq!(
            report.last_result().map(|r| r.columns.clone()),
            Some(vec!["a".to_owned()])
        );
        assert_eq!(report.elapsed(), Duration::from_millis(4));
        assert!(report.failure().is_none());
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_elapsed(Duration::from_micros(800)), "0.8 ms");
        assert_eq!(format_elapsed(Duration::from_millis(34)), "34 ms");
        assert_eq!(format_elapsed(Duration::from_millis(2410)), "2.41 s");
    }

    #[test]
    fn counts_are_grouped_and_pluralised() {
        assert_eq!(group_thousands(1_240_553), "1,240,553");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(rows_label(1), "1 row");
        assert_eq!(rows_label(1000), "1,000 rows");
    }

    #[test]
    fn identifiers_are_quoted_safely() {
        assert_eq!(quote_ident("orders"), "\"orders\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }
}
