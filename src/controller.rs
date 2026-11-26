//! Adapter between the Slint window and the pure Rust core.
//!
//! This is the only module that imports both `crate::ui` (the generated bindings) and the
//! domain modules, which is what keeps `model` / `store` / `tree` / `backend` free of the UI
//! toolkit.
//!
//! The flow is one-directional: a view raises an intent as a callback, the controller applies
//! it to the state it owns, and then re-publishes what the window shows. Work that talks to a
//! server goes through the [`Backend`] port and answers on a worker thread; the answer is
//! hopped back onto the UI thread with [`on_ui`], applied, and published the same way.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use crate::backend::{
    Backend, DbObject, ObjectInfo, ObjectKind, ResultSet, RunReport, ServerInfo, StatementOutcome,
    Target, TxnState, format_elapsed, group_thousands, quote_ident, rows_label,
};
mod analysis_view;
mod chart_view;
mod dump_view;
mod erd_view;
mod file_view;
mod find_view;
mod grid_view;
mod guard_view;
mod import_view;
mod maint_view;
mod mon_view;
mod notify_view;
mod object_view;
mod page_view;
mod pivot_view;
mod sec_view;
mod vault_view;

use grid_view::{History, ResultEdit, Which};

use crate::complete;
use crate::designer;
use crate::edits::{self, EditSet};
use crate::export;
use crate::model::{ConnectionStatus, Environment, ServerDraft, ServerProfile};
use crate::probe::{ConnectionProbe, ProbeOutcome};
use crate::risk;
use crate::sql_highlight::highlight_line;
use crate::sql_split::split_statements;
use crate::store::ServerStore;
use crate::tree::{Fetch, Load, Node, TreeState, kind_tag, server_node_id};
use crate::ui::{
    AppWindow, CompletionRow, DesignColumn, DesignIndex, GridCell, GridColumn, GridRow, HistoryRow,
    InspectorSection, MessageRow, PlanRow, Property, QueryTabRow, ResultPart, ServerRow, SqlLayer,
    TreeRow,
};

/// Sentinel the UI uses for "nothing is selected".
const NO_SELECTION: i32 = -1;

/// The starter script, naming the run shortcut the way this platform writes it.
fn starter_sql() -> String {
    let mac = cfg!(target_os = "macos")
        || std::env::var("NUSADB_STUDIO_PLATFORM").is_ok_and(|p| p == "mac");
    STARTER_SQL.replace("@RUN@", if mac { "⌘↩" } else { "Ctrl+Enter" })
}

/// What the editor holds on a cold start: something that runs, and says how.
const STARTER_SQL: &str = "\
-- Pick a database in the navigator, then press Run or @RUN@.
-- Statements run in order and stop at the first error.
SELECT current_database(), now();
";

/// The server an error row in the navigator belongs to: clicking the error selects the
/// server, whose inspector shows the full message.
fn error_row_server(id: &str) -> Option<u32> {
    let mut parts = id.split('\u{1f}');
    (parts.next()? == "err").then_some(())?;
    parts.next()?.parse().ok()
}

/// Scripts kept in History.
const HISTORY_LIMIT: usize = 100;

/// One past run of the editor's script.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct HistoryEntry {
    sql: String,
    /// "server / database"
    target: String,
    /// Unix seconds.
    when: u64,
    ok: bool,
    summary: String,
}

/// Inspector action labels. Kept as constants so the view and the dispatcher cannot drift.
mod action {
    pub const CONNECT: &str = "Connect";
    pub const DISCONNECT: &str = "Disconnect";
    pub const EDIT: &str = "Edit server…";
    pub const NEW_QUERY: &str = "New query here";
    pub const BROWSE: &str = "Browse data";
    pub const DETAILS: &str = "Details";
    pub const IMPORT: &str = "Import data…";
    pub const QUERY_TABLE: &str = "Query in editor";
    pub const EDIT_STRUCTURE: &str = "Edit structure…";
    pub const SHOW_DDL: &str = "Show DDL";
    pub const REFRESH_MATVIEW: &str = "Refresh data";
    pub const OPEN_DEFINITION: &str = "Open definition";
    pub const NEW_TABLE: &str = "New table…";
    pub const SECURITY: &str = "Roles & security…";
    pub const DIAGRAM: &str = "ER diagram";
    pub const TABLE_SECURITY: &str = "Privileges & policies…";
    pub const COPY_NAME: &str = "Copy name";
    pub const REFRESH: &str = "Refresh";
    pub const TRUNCATE: &str = "Truncate…";
    pub const DROP: &str = "Drop…";
    pub const NEW_DATABASE: &str = "New database…";
    pub const NEW_SCHEMA: &str = "New schema…";
    pub const NEW_VIEW: &str = "New view…";
    pub const RENAME: &str = "Rename…";
    pub const DUPLICATE: &str = "Duplicate…";
    pub const EXPORT_SQL: &str = "Export as SQL…";
}

/// Which object the Data screen shows, and where in it.
#[derive(Debug, Clone)]
struct DataState {
    server: u32,
    database: String,
    schema: String,
    name: String,
    kind: ObjectKind,
    filter: String,
    /// Column name and descending?, applied on the server so paging stays correct.
    sort: Option<(String, bool)>,
    page: usize,
    total: Option<usize>,
    /// A random sample of this many percent (0 = every row), drawn with `seed` so paging
    /// through it stays consistent; tables only (`TABLESAMPLE`).
    sample: u32,
    seed: u32,
    /// Changes staged against the loaded page.
    edits: EditSet,
}

/// Why a script was sent, which decides what its answer replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunKind {
    /// The editor's script: fills the grid, JSON and messages.
    Script,
    /// EXPLAIN of the editor's script: fills the Plan tab.
    Explain,
    /// BEGIN / COMMIT / ROLLBACK from the toolbar: leaves the grid alone.
    Txn,
}

/// A run that finished while its tab was in the background, kept until the tab is shown.
#[derive(Debug, Clone)]
struct Deferred {
    kind: RunKind,
    server: u32,
    database: String,
    result: Result<RunReport, String>,
}

/// One query tab. The active tab's live state is held in the controller's own fields and the
/// window; this keeps everything for tabs in the background.
struct QueryTab {
    id: u32,
    title: String,
    sql: String,
    target: Option<(u32, String)>,
    running: bool,
    result: Option<ResultSet>,
    sort: Option<(usize, bool)>,
    result_sql: Option<String>,
    columns: ModelRc<GridColumn>,
    columns_width: f32,
    rows: ModelRc<GridRow>,
    messages: ModelRc<MessageRow>,
    json: SharedString,
    plan: ModelRc<PlanRow>,
    summary: SharedString,
    rows_note: SharedString,
    has_error: bool,
    result_tab: SharedString,
    status_rows: SharedString,
    status_timing: SharedString,
    /// The `.sql` file this tab was opened from or saved to.
    path: Option<PathBuf>,
    /// The file's text as last read or written; `None` for a tab without a file.
    saved: Option<String>,
    deferred: Option<Deferred>,
    /// Staged edits of an editable result.
    result_edit: Option<ResultEdit>,
    /// Every result set of the last run, and which one is showing.
    parts: Vec<Part>,
    part: usize,
}

/// One result set of a script run, with the statement that produced it.
#[derive(Clone)]
struct Part {
    sql: String,
    result: ResultSet,
    elapsed: std::time::Duration,
}

impl QueryTab {
    fn new(id: u32, title: String) -> Self {
        Self {
            id,
            title,
            sql: String::new(),
            target: None,
            running: false,
            result: None,
            sort: None,
            result_sql: None,
            columns: ModelRc::default(),
            columns_width: 0.0,
            rows: ModelRc::default(),
            messages: ModelRc::default(),
            json: SharedString::new(),
            plan: ModelRc::default(),
            summary: SharedString::new(),
            rows_note: SharedString::new(),
            has_error: false,
            result_tab: "results".into(),
            status_rows: SharedString::new(),
            status_timing: SharedString::new(),
            path: None,
            saved: None,
            result_edit: None,
            parts: Vec::new(),
            part: 0,
            deferred: None,
        }
    }
}

/// An action waiting for the user to confirm it.
#[derive(Debug, Clone)]
enum Pending {
    Run(RunKind, Vec<String>),
    SaveData,
    /// Staged edits of an editable Query result.
    SaveResult,
    ApplyDesign,
    ApplySecurity,
    /// `COMMIT PREPARED '…'` or `ROLLBACK PREPARED '…'`.
    FinishPrepared(String),
    Import,
    /// Close a tab whose file has unsaved changes.
    CloseTab(u32),
    /// TRUNCATE or DROP of a navigator object.
    Destroy(Node, String),
    /// Close Studio with unsaved work.
    Quit,
    /// Disconnect a server with an open transaction or unsaved edits on it.
    Disconnect(u32),
    /// `DROP INDEX` of an HNSW index from the Vectors tab.
    DropVectorIndex(String),
}

/// The table open in the designer.
#[derive(Debug, Clone)]
struct DesignState {
    server: u32,
    database: String,
    /// As loaded, for an existing table; `None` while creating one.
    original: Option<designer::TableSpec>,
    spec: designer::TableSpec,
}

/// Owns Studio's state and translates UI intents into changes to it.
pub struct Controller {
    store: RefCell<ServerStore>,
    store_path: Option<PathBuf>,
    tree: RefCell<TreeState>,
    tree_filter: RefCell<String>,
    probe: Box<dyn ConnectionProbe>,
    backend: Box<dyn Backend>,
    /// The server loaded in the connection form.
    form_server: Cell<Option<u32>>,
    /// What is selected in the navigator.
    selection: RefCell<Option<Node>>,
    /// Columns of tables the inspector has described, by node id.
    info: RefCell<HashMap<String, Load<ObjectInfo>>>,
    /// `version()` of each server, once connected.
    versions: RefCell<HashMap<u32, String>>,
    /// The `(server, database)` the editor runs against.
    target: RefCell<Option<(u32, String)>>,
    sql: RefCell<String>,
    running: Cell<bool>,
    txn: Cell<TxnState>,
    data: RefCell<Option<DataState>>,
    data_loading: Cell<bool>,
    /// The user confirmed quitting; the next close request goes through.
    quitting: Cell<bool>,
    /// Rows per page in the Data screen (Settings).
    page_size: Cell<usize>,
    /// Databases whose table list was asked for outside the navigator.
    catalog_requested: RefCell<std::collections::HashSet<(u32, String)>>,
    /// Scripts the History tab keeps (Settings).
    history_limit: Cell<usize>,
    /// What the name dialog is open for.
    prompt: RefCell<Option<object_view::Prompt>>,
    /// An "Export as SQL…" in progress.
    dump: RefCell<Option<dump_view::Dump>>,
    /// Column widths the user dragged, per grid (see `grid_view::width_scope`), by column name.
    col_widths: RefCell<HashMap<String, HashMap<String, f32>>>,
    /// The Query result, when it can be edited in place, with its staged edits.
    result_edit: RefCell<Option<ResultEdit>>,
    /// Columns the user hid, per grid.
    col_hidden: RefCell<grid_view::Hidden>,
    /// Undo history of staged edits, per grid.
    undo_history: RefCell<HashMap<Which, History>>,
    /// The last result shown in the Query grid, in the server's order, and the ordering the
    /// user picked on top of it.
    query_result: RefCell<Option<ResultSet>>,
    query_sort: Cell<Option<(usize, bool)>>,
    /// The statement whose result the Query grid shows, for exporting it in full.
    query_sql: RefCell<Option<String>>,
    /// Every result set of the last script run; `query_result` is `parts[part]`.
    parts: RefCell<Vec<Part>>,
    part: Cell<usize>,
    chart: RefCell<chart_view::ChartChoice>,
    pivot: RefCell<pivot_view::PivotChoice>,
    pivot_result: RefCell<Option<ResultSet>>,
    profile: RefCell<Option<analysis_view::ProfileRun>>,
    vectors: RefCell<Option<analysis_view::VectorState>>,
    /// Bumped per Profile / Vectors load, so a late reply for an older one is dropped.
    analysis_serial: Cell<u64>,
    /// Vectors behind the embedding map's points, for "search from this point".
    map_rows: RefCell<Vec<String>>,
    toast_serial: Cell<i32>,
    /// The open completion popup: the word it completes, the caret, and the choices.
    completion: RefCell<Option<(complete::Word, usize, Vec<complete::Suggestion>)>>,
    caret_serial: Cell<i32>,
    /// The editor's caret as last reported, in bytes.
    caret: Cell<usize>,
    /// Which find match is selected.
    find_index: Cell<Option<usize>>,
    /// The rows currently on the Data screen.
    data_result: RefCell<Option<ResultSet>>,
    /// Scripts run, newest first.
    history: RefCell<Vec<HistoryEntry>>,
    history_path: Option<PathBuf>,
    design: RefCell<Option<DesignState>>,
    security: RefCell<Option<sec_view::SecurityState>>,
    /// The object open on the Data screen's page (its rows and its detail tabs).
    page: RefCell<Option<Node>>,
    /// The database a security read is in flight for.
    security_loading: RefCell<Option<(u32, String)>>,
    security_models: sec_view::SecurityModels,
    monitor: RefCell<Option<mon_view::MonitorState>>,
    erd: RefCell<Option<erd_view::ErdState>>,
    /// The database a diagram read is in flight for.
    erd_loading: RefCell<Option<(u32, String)>>,
    erd_boxes: Rc<VecModel<crate::ui::ErdBox>>,
    notify: RefCell<notify_view::NotifyState>,
    import: RefCell<Option<import_view::ImportState>>,
    monitor_timer: slint::Timer,
    design_model: Rc<VecModel<DesignColumn>>,
    design_index_model: Rc<VecModel<DesignIndex>>,
    /// After applying, reopen this table in the designer once its new shape is described.
    design_reopen: RefCell<Option<Node>>,
    /// Query tabs, in order; the active one's state lives in the fields above.
    tabs: RefCell<Vec<QueryTab>>,
    active_tab: Cell<u32>,
    next_tab: Cell<u32>,
    /// What the confirmation dialog will do if accepted.
    pending: RefCell<Option<Pending>>,
    /// The navigator's model, kept for the window's lifetime and updated in place: replacing
    /// it on every click would recreate each row between the two clicks of a double-click.
    tree_model: Rc<VecModel<TreeRow>>,
}

thread_local! {
    /// The live controller and window, for answers arriving from worker threads.
    static ACTIVE: RefCell<Option<(Rc<Controller>, Weak<AppWindow>)>> = const { RefCell::new(None) };
}

/// Run `apply` on the UI thread with the controller and window, if both still exist.
fn on_ui<F>(weak: &Weak<AppWindow>, apply: F)
where
    F: FnOnce(&Controller, &AppWindow) + Send + 'static,
{
    let _ = weak.upgrade_in_event_loop(move |window| {
        let controller = ACTIVE.with(|active| active.borrow().as_ref().map(|(c, _)| Rc::clone(c)));
        if let Some(controller) = controller {
            apply(&controller, &window);
        }
    });
}

impl Controller {
    /// Build a controller over `store`, persisted at `store_path` when given.
    #[must_use]
    pub fn new(
        store: ServerStore,
        store_path: Option<PathBuf>,
        probe: Box<dyn ConnectionProbe>,
        backend: Box<dyn Backend>,
    ) -> Self {
        let history_path = store_path
            .as_ref()
            .map(|p| p.with_file_name("history.json"));
        let history = history_path
            .as_deref()
            .map(load_history)
            .unwrap_or_default();
        Self {
            store: RefCell::new(store),
            store_path,
            tree: RefCell::new(TreeState::new()),
            tree_filter: RefCell::new(String::new()),
            probe,
            backend,
            form_server: Cell::new(None),
            selection: RefCell::new(None),
            info: RefCell::new(HashMap::new()),
            versions: RefCell::new(HashMap::new()),
            target: RefCell::new(None),
            sql: RefCell::new(starter_sql()),
            running: Cell::new(false),
            txn: Cell::new(TxnState::Idle),
            data: RefCell::new(None),
            data_loading: Cell::new(false),
            quitting: Cell::new(false),
            page_size: Cell::new(200),
            catalog_requested: RefCell::new(std::collections::HashSet::new()),
            history_limit: Cell::new(HISTORY_LIMIT),
            prompt: RefCell::new(None),
            dump: RefCell::new(None),
            col_widths: RefCell::new(HashMap::new()),
            result_edit: RefCell::new(None),
            col_hidden: RefCell::new(HashMap::new()),
            undo_history: RefCell::new(HashMap::new()),
            tree_model: Rc::new(VecModel::default()),
            pending: RefCell::new(None),
            design: RefCell::new(None),
            security: RefCell::new(None),
            page: RefCell::new(None),
            security_loading: RefCell::new(None),
            security_models: sec_view::SecurityModels::default(),
            monitor: RefCell::new(None),
            erd: RefCell::new(None),
            erd_loading: RefCell::new(None),
            erd_boxes: Rc::new(VecModel::default()),
            notify: RefCell::new(notify_view::NotifyState::default()),
            import: RefCell::new(None),
            monitor_timer: slint::Timer::default(),
            design_model: Rc::new(VecModel::default()),
            design_index_model: Rc::new(VecModel::default()),
            design_reopen: RefCell::new(None),
            tabs: RefCell::new(vec![QueryTab::new(1, "Query 1".to_owned())]),
            active_tab: Cell::new(1),
            next_tab: Cell::new(2),
            query_result: RefCell::new(None),
            query_sort: Cell::new(None),
            query_sql: RefCell::new(None),
            parts: RefCell::new(Vec::new()),
            part: Cell::new(0),
            chart: RefCell::new(chart_view::ChartChoice::default()),
            pivot: RefCell::new(pivot_view::PivotChoice::default()),
            pivot_result: RefCell::new(None),
            profile: RefCell::new(None),
            vectors: RefCell::new(None),
            analysis_serial: Cell::new(0),
            map_rows: RefCell::new(Vec::new()),
            toast_serial: Cell::new(0),
            completion: RefCell::new(None),
            caret_serial: Cell::new(0),
            caret: Cell::new(0),
            find_index: Cell::new(None),
            data_result: RefCell::new(None),
            history: RefCell::new(history),
            history_path,
        }
    }

    // ── Publishing ──────────────────────────────────────────────────────────

    /// Push the navigator, servers, inspector and chrome into the window.
    fn publish(&self, window: &AppWindow) {
        let store = self.store.borrow();

        let rows: Vec<ServerRow> = store.all().map(server_row).collect();
        window.set_servers(ModelRc::new(VecModel::from(rows)));

        let tree_rows: Vec<TreeRow> = self
            .tree
            .borrow()
            .rows(store.all(), &self.tree_filter.borrow())
            .into_iter()
            .map(|row| TreeRow {
                id: row.id.as_str().into(),
                label: row.label.as_str().into(),
                kind: row.kind.into(),
                depth: row.depth,
                expandable: row.expandable,
                expanded: row.expanded,
                status: row.status.into(),
                badge: row.badge.as_str().into(),
                dimmed: row.dimmed,
            })
            .collect();
        sync_model(&self.tree_model, tree_rows);
        window.set_selected_node(
            self.selection
                .borrow()
                .as_ref()
                .map_or_else(SharedString::new, |n| n.id().into()),
        );

        let form = self.form_server.get();
        window.set_editing(form.is_some());
        window.set_selected_id(form.map_or(NO_SELECTION, id_for_ui));

        drop(store);
        self.publish_inspector(window);
        self.publish_target(window);
        self.security_follow(window);
        self.monitor_follow(window);
        self.erd_follow(window);
    }

    /// The top bar, status bar and the editor's target line.
    /// Load the table list of the editor's database if nothing has yet, so completion and the
    /// "Read-only" check work without first opening the database in the navigator.
    fn ensure_catalog(&self, window: &AppWindow) {
        let Some((server, database)) = self.target.borrow().clone() else {
            return;
        };
        if self.tree.borrow().catalog(server, &database).is_some() {
            return;
        }
        if self
            .catalog_requested
            .borrow_mut()
            .insert((server, database.clone()))
        {
            self.dispatch(window, Fetch::Catalog { server, database });
        }
    }

    fn publish_target(&self, window: &AppWindow) {
        self.ensure_catalog(window);
        let store = self.store.borrow();
        let target = self.target.borrow();
        let profile = target.as_ref().and_then(|(s, _)| store.get(*s));
        match (profile, target.as_ref()) {
            (Some(profile), Some((_, database))) => {
                let live = profile.status == ConnectionStatus::Connected;
                let label = format!("{} / {}", profile.name, database);
                window.set_has_target(live);
                window.set_target_label(label.as_str().into());
                window.set_connected(live);
                window.set_connection_label(label.as_str().into());
                window.set_connection_status(profile.status.as_tag().into());
                window.set_connection_env(profile.environment.as_tag().into());
                window.set_status_server(
                    format!("{} · {}", profile.name, profile.params.endpoint()).into(),
                );
                window.set_status_database(database.as_str().into());
            },
            _ => {
                window.set_has_target(false);
                window.set_target_label(SharedString::new());
                window.set_connected(false);
                window.set_connection_label("Not connected".into());
                window.set_connection_status(ConnectionStatus::Disconnected.as_tag().into());
                window.set_connection_env(SharedString::new());
                window.set_status_server("Not connected".into());
                window.set_status_database(SharedString::new());
            },
        }
        window.set_txn_state(self.txn.get().as_tag().into());
        window.set_running(self.running.get());
    }

    /// Describe the navigator selection in the right-hand pane.
    fn publish_inspector(&self, window: &AppWindow) {
        let store = self.store.borrow();
        let tree = self.tree.borrow();
        let selection = self.selection.borrow();

        let mut title = String::new();
        let mut kind = "";
        let mut subtitle = String::new();
        let mut properties = Vec::new();
        let mut sections: Vec<InspectorSection> = Vec::new();
        let mut definition = String::new();
        let mut comment = String::new();
        let mut notes: Vec<SharedString> = Vec::new();
        let mut actions: Vec<&str> = Vec::new();

        if let Some(node) = selection.as_ref()
            && let Some(profile) = store.get(node.server())
        {
            match node {
                Node::Server(_) => {
                    title.clone_from(&profile.name);
                    kind = "server";
                    subtitle = format!("Server · {}", env_label(profile.environment));
                    let live = profile.status == ConnectionStatus::Connected;
                    actions.push(if live {
                        action::DISCONNECT
                    } else {
                        action::CONNECT
                    });
                    actions.push(action::EDIT);
                    properties.push(property("Endpoint", &profile.params.endpoint(), true));
                    properties.push(property("User", &profile.params.user, true));
                    properties.push(property("Database", &profile.params.database, true));
                    properties.push(property(
                        "TLS",
                        if profile.params.tls { "on" } else { "off" },
                        false,
                    ));
                    properties.push(property("Status", status_label(profile.status), false));
                    if let Some(version) = self.versions.borrow().get(&profile.id) {
                        properties.push(property("Version", version, false));
                    }
                    if let Some(databases) = tree.databases(profile.id) {
                        properties.push(property("Databases", &databases.len().to_string(), false));
                    }
                    if let Some(error) = tree.databases_error(profile.id) {
                        subtitle = error.to_owned();
                    }
                },
                Node::Database { database, .. } => {
                    title.clone_from(database);
                    kind = "database";
                    subtitle = format!("Database on {}", profile.name);
                    actions.push(action::NEW_QUERY);
                    actions.push(action::NEW_TABLE);
                    actions.push(action::DIAGRAM);
                    actions.push(action::SECURITY);
                    if let Some(catalog) = tree.catalog(profile.id, database) {
                        let user_schemas = catalog
                            .schemas
                            .iter()
                            .filter(|s| *s != "information_schema")
                            .count();
                        properties.push(property("Schemas", &user_schemas.to_string(), false));
                        push_counts(&mut properties, &catalog.objects);
                    }
                },
                Node::Schema {
                    database, schema, ..
                } => {
                    title.clone_from(schema);
                    kind = "schema";
                    subtitle = format!("Schema in {database}");
                    actions.push(action::NEW_QUERY);
                    actions.push(action::NEW_TABLE);
                    if let Some(catalog) = tree.catalog(profile.id, database) {
                        let in_schema: Vec<_> = catalog
                            .objects
                            .iter()
                            .filter(|o| o.schema == *schema)
                            .cloned()
                            .collect();
                        push_counts(&mut properties, &in_schema);
                    }
                },
                Node::Group {
                    database,
                    schema,
                    kind: group,
                    ..
                } => {
                    title = group.plural().to_owned();
                    kind = "group";
                    subtitle = format!("{schema} · {database}");
                    actions.push(action::NEW_QUERY);
                    if *group == ObjectKind::Table {
                        actions.push(action::NEW_TABLE);
                    }
                },
                Node::Object {
                    database,
                    schema,
                    name,
                    kind: object,
                    ..
                } => {
                    title.clone_from(name);
                    kind = kind_tag(*object);
                    subtitle = format!("{} in {schema} · {database}", object.noun());
                    if object.has_rows() {
                        actions.push(action::BROWSE);
                    }
                    // The full columns, constraints, indexes, triggers, policies and DDL live
                    // on the object page; this panel only summarises.
                    actions.push(action::DETAILS);
                    actions.push(action::QUERY_TABLE);
                    if *object == ObjectKind::Table {
                        actions.push(action::IMPORT);
                        actions.push(action::EDIT_STRUCTURE);
                        actions.push(action::TABLE_SECURITY);
                    }
                    if *object == ObjectKind::MaterializedView {
                        actions.push(action::REFRESH_MATVIEW);
                    }
                    match self.info.borrow().get(&node.id()) {
                        Some(Load::Ready(info)) => {
                            let described = describe_object(*object, info);
                            properties.extend(described.properties);
                            sections = described.sections;
                            definition = described.definition;
                            comment = described.comment;
                            notes = described.notes;
                        },
                        Some(Load::Failed(message)) => {
                            properties.push(property("Details", message, false));
                        },
                        Some(Load::Loading) => {
                            properties.push(property("Details", "loading…", false))
                        },
                        None => {},
                    }
                },
            }
        }

        window.set_inspector_title(title.into());
        window.set_inspector_kind(kind.into());
        window.set_inspector_subtitle(subtitle.into());
        window.set_inspector_properties(ModelRc::new(VecModel::from(properties)));
        window.set_inspector_sections(ModelRc::new(VecModel::from(sections)));
        window.set_inspector_definition(definition.into());
        window.set_inspector_comment(comment.into());
        window.set_inspector_notes(ModelRc::new(VecModel::from(notes)));
        let maintenance = selection
            .as_ref()
            .map(maint_view::offered)
            .unwrap_or_default();
        // The right-click menu: the same actions, the maintenance, then copy / refresh and
        // the destructive ones last, each group after a separator.
        if let Some(node) = selection.as_ref() {
            actions.extend(object_view::creating(node));
        }
        let mut menu: Vec<SharedString> = actions.iter().map(|a| SharedString::from(*a)).collect();
        let extra = selection
            .as_ref()
            .map(object_view::offered)
            .unwrap_or_default();
        for group in [maintenance.clone(), extra] {
            if !group.is_empty() {
                if !menu.is_empty() {
                    menu.push("-".into());
                }
                menu.extend(group.iter().map(|m| SharedString::from(*m)));
            }
        }
        window.set_context_actions(ModelRc::new(VecModel::from(menu)));
        let maintenance: Vec<SharedString> =
            maintenance.into_iter().map(SharedString::from).collect();
        window.set_inspector_maintenance(ModelRc::new(VecModel::from(maintenance)));
        let actions: Vec<SharedString> = actions.into_iter().map(SharedString::from).collect();
        window.set_inspector_actions(ModelRc::new(VecModel::from(actions)));
    }

    // ── Servers ─────────────────────────────────────────────────────────────

    /// Write the server list to disk, reporting a failure in the form.
    fn persist(&self, window: &AppWindow) {
        let Some(path) = &self.store_path else {
            return;
        };
        if let Err(error) = self.store.borrow().save(path) {
            window.set_form_error(
                format!(
                    "Saved for this session, but the list could not be written to {}: {error}",
                    path.display()
                )
                .into(),
            );
        }
    }

    /// Load the server `id` into the connection form.
    fn edit_server(&self, window: &AppWindow, id: u32) {
        let draft = self.store.borrow().get(id).map(ServerDraft::from_profile);
        let Some(draft) = draft else {
            return;
        };
        self.form_server.set(Some(id));
        write_draft(window, &draft);
        clear_feedback(window);
        self.publish(window);
    }

    /// Return the form to an empty "new server" state.
    fn reset_form(&self, window: &AppWindow) {
        self.form_server.set(None);
        write_draft(window, &ServerDraft::default());
        clear_feedback(window);
        self.publish(window);
    }

    /// Validate the form and either update the server being edited or add a new one.
    fn save(&self, window: &AppWindow) {
        let valid = match read_draft(window).validate() {
            Ok(valid) => valid,
            Err(error) => {
                window.set_form_error(error.to_string().into());
                return;
            },
        };
        window.set_form_error(SharedString::new());

        let existing = self
            .form_server
            .get()
            .filter(|id| self.store.borrow().get(*id).is_some());
        let before = existing.and_then(|id| self.store.borrow().get(id).cloned());

        let id = match existing {
            Some(id) => {
                let changed = self
                    .store
                    .borrow()
                    .get(id)
                    .is_some_and(|p| p.params != valid.params);
                self.store.borrow_mut().update(id, valid);
                // Sessions were opened with the old address or credentials; they no longer
                // describe this server.
                if changed {
                    self.drop_sessions(id);
                }
                id
            },
            None => self.store.borrow_mut().insert(valid),
        };

        self.form_server.set(Some(id));
        // Saved: the form has done its job.
        window.set_server_form_open(false);
        self.vault_saved(window, before, id);
        self.persist(window);
        self.publish(window);
    }

    /// Remove the server `id` and everything that pointed at it.
    fn remove(&self, window: &AppWindow, id: u32) {
        self.drop_sessions(id);
        if !self.store.borrow_mut().remove(id) {
            return;
        }
        if self
            .selection
            .borrow()
            .as_ref()
            .is_some_and(|n| n.server() == id)
        {
            self.selection.replace(None);
        }
        self.persist(window);
        if self.form_server.get() == Some(id) {
            self.reset_form(window);
        } else {
            self.publish(window);
        }
        self.publish_data(window);
    }

    /// Close the sessions of `id` and forget what was loaded from it.
    fn drop_sessions(&self, id: u32) {
        self.backend.disconnect(id);
        self.tree.borrow_mut().forget_server(id);
        self.info
            .borrow_mut()
            .retain(|key, _| Node::parse(key).is_none_or(|n| n.server() != id));
        self.store
            .borrow_mut()
            .set_status(id, ConnectionStatus::Disconnected);
        if self.target.borrow().as_ref().is_some_and(|(s, _)| *s == id) {
            self.target.replace(None);
            self.txn.set(TxnState::Idle);
        }
        if self.data.borrow().as_ref().is_some_and(|d| d.server == id) {
            self.data.replace(None);
        }
    }

    /// Open the server: load its databases, which proves the session works.
    fn connect(&self, window: &AppWindow, id: u32) {
        let fetch = self.tree.borrow_mut().expand(&server_node_id(id));
        match fetch {
            Some(fetch) => self.dispatch(window, fetch),
            // Already loaded (or loading): reconnecting means reloading.
            None => {
                self.tree.borrow_mut().forget_server(id);
                if let Some(fetch) = self.tree.borrow_mut().expand(&server_node_id(id)) {
                    self.dispatch(window, fetch);
                }
            },
        }
        self.publish(window);
    }

    fn disconnect(&self, window: &AppWindow, id: u32) {
        self.drop_sessions(id);
        self.publish(window);
        self.publish_data(window);
    }

    /// A target on `server` for `database`.
    fn target_for(&self, server: u32, database: &str) -> Option<Target> {
        let store = self.store.borrow();
        let profile = store.get(server)?;
        Some(Target {
            server,
            params: profile.params.clone(),
            database: database.to_owned(),
        })
    }

    /// Start a fetch the tree asked for.
    fn dispatch(&self, window: &AppWindow, fetch: Fetch) {
        let weak = window.as_weak();
        match fetch {
            Fetch::Databases(server) => {
                let login = self
                    .store
                    .borrow()
                    .get(server)
                    .map(|p| p.params.database.clone());
                let Some(target) = login.and_then(|db| self.target_for(server, &db)) else {
                    return;
                };
                self.store
                    .borrow_mut()
                    .set_status(server, ConnectionStatus::Connecting);
                self.backend.server_info(
                    target,
                    Box::new(move |result| {
                        on_ui(&weak, move |c, w| c.databases_loaded(w, server, result));
                    }),
                );
            },
            Fetch::Catalog { server, database } => {
                let Some(target) = self.target_for(server, &database) else {
                    return;
                };
                self.backend.catalog(
                    target,
                    Box::new(move |result| {
                        on_ui(&weak, move |c, w| {
                            c.tree.borrow_mut().set_catalog(server, &database, result);
                            c.publish(w);
                        });
                    }),
                );
            },
        }
    }

    fn databases_loaded(
        &self,
        window: &AppWindow,
        server: u32,
        result: Result<ServerInfo, String>,
    ) {
        let result = result.map(|info| {
            self.versions.borrow_mut().insert(server, info.version);
            info.databases
        });
        if result.is_ok() {
            self.store.borrow_mut().touch(server, unix_now());
            self.persist(window);
        }
        let status = if result.is_ok() {
            ConnectionStatus::Connected
        } else {
            ConnectionStatus::Error
        };
        if !self.store.borrow_mut().set_status(server, status) {
            return; // removed while loading
        }
        // The first server to come up becomes the query target, on its login database.
        if let Ok(databases) = &result
            && self.target.borrow().is_none()
        {
            let login = self
                .store
                .borrow()
                .get(server)
                .map(|p| p.params.database.clone())
                .unwrap_or_default();
            let database = if databases.contains(&login) {
                login
            } else {
                databases.first().cloned().unwrap_or(login)
            };
            self.target.replace(Some((server, database)));
        }
        self.tree.borrow_mut().set_databases(server, result);
        self.publish(window);
    }

    // ── Navigator ───────────────────────────────────────────────────────────

    fn toggle_node(&self, window: &AppWindow, id: &str) {
        let fetch = self.tree.borrow_mut().toggle(id);
        if let Some(fetch) = fetch {
            self.dispatch(window, fetch);
        }
        self.publish(window);
    }

    /// Keyboard in the navigator: move, open/close, or open the selected row.
    fn tree_key(&self, window: &AppWindow, key: &str) {
        let rows: Vec<TreeRow> = (0..self.tree_model.row_count())
            .filter_map(|i| self.tree_model.row_data(i))
            .filter(|r| r.kind != "hint")
            .collect();
        if rows.is_empty() {
            return;
        }
        let current = window.get_selected_node();
        let at = rows.iter().position(|r| r.id == current);
        let target = match (key, at) {
            (_, None) => Some(0),
            ("up", Some(i)) => i.checked_sub(1),
            ("down", Some(i)) => (i + 1 < rows.len()).then_some(i + 1),
            ("right", Some(i)) if rows[i].expandable && !rows[i].expanded => {
                self.toggle_node(window, rows[i].id.as_str());
                None
            },
            ("right", Some(i)) => (i + 1 < rows.len() && rows[i].expanded).then_some(i + 1),
            ("left", Some(i)) if rows[i].expanded => {
                self.toggle_node(window, rows[i].id.as_str());
                None
            },
            // Left on a closed row goes to its parent: the nearest row above, one level up.
            ("left", Some(i)) => rows[..i].iter().rposition(|r| r.depth < rows[i].depth),
            ("enter", Some(i)) => {
                self.open_node(window, rows[i].id.as_str());
                None
            },
            _ => None,
        };
        if let Some(i) = target {
            self.select_node(window, rows[i].id.as_str());
        }
    }

    fn select_node(&self, window: &AppWindow, id: &str) {
        let Some(node) = Node::parse(id).or_else(|| error_row_server(id).map(Node::Server)) else {
            return;
        };
        match &node {
            Node::Server(server) => {
                let draft = self
                    .store
                    .borrow()
                    .get(*server)
                    .map(ServerDraft::from_profile);
                if let Some(draft) = draft {
                    self.form_server.set(Some(*server));
                    write_draft(window, &draft);
                    clear_feedback(window);
                }
            },
            Node::Object { .. } => self.load_info(window, &node),
            _ => {},
        }
        // Clicking anything inside a database points the editor at that database.
        if let Some(database) = node.database() {
            self.set_target(node.server(), database);
        }
        self.selection.replace(Some(node));
        self.publish(window);
    }

    /// Point the editor at `(server, database)`.
    fn set_target(&self, server: u32, database: &str) {
        let same = self
            .target
            .borrow()
            .as_ref()
            .is_some_and(|(s, d)| *s == server && d == database);
        if !same {
            self.target.replace(Some((server, database.to_owned())));
            // Each database has its own session, so the transaction state is that session's;
            // a fresh target starts idle until it runs something.
            self.txn.set(TxnState::Idle);
        }
    }

    /// Double-click: open a table's rows, or open/close anything else.
    fn open_node(&self, window: &AppWindow, id: &str) {
        match Node::parse(id) {
            Some(node @ Node::Object { .. }) => {
                self.selection.replace(Some(node.clone()));
                self.load_info(window, &node);
                let has_rows = matches!(&node, Node::Object { kind, .. } if kind.has_rows());
                self.open_object(window, &node, if has_rows { "data" } else { "overview" });
                self.publish(window);
            },
            Some(_) => self.toggle_node(window, id),
            None => {},
        }
    }

    fn load_info(&self, window: &AppWindow, node: &Node) {
        let Node::Object {
            server,
            database,
            schema,
            name,
            kind,
        } = node
        else {
            return;
        };
        let key = node.id();
        if matches!(
            self.info.borrow().get(&key),
            Some(Load::Ready(_) | Load::Loading)
        ) {
            return;
        }
        let Some(target) = self.target_for(*server, database) else {
            return;
        };
        self.info.borrow_mut().insert(key.clone(), Load::Loading);
        let weak = window.as_weak();
        let object = DbObject {
            schema: schema.clone(),
            name: name.clone(),
            kind: *kind,
        };
        self.backend.describe(
            target,
            object,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    let load = match result {
                        Ok(info) => Load::Ready(info),
                        Err(message) => Load::Failed(message),
                    };
                    c.info.borrow_mut().insert(key.clone(), load);
                    c.publish_inspector(w);
                    c.publish_page(w);
                    // A Query result waiting for its table's key can now be judged editable.
                    if c.query_result.borrow().is_some() && !w.get_results_editable() {
                        c.result_shape_refresh(w);
                    }
                    let reopen = c
                        .design_reopen
                        .borrow()
                        .as_ref()
                        .is_some_and(|n| n.id() == key);
                    if reopen && let Some(node) = c.design_reopen.borrow_mut().take() {
                        c.design_edit(w, &node);
                    }
                    // The Data screen's editability depends on the key this just described.
                    if c.data.borrow().is_some() {
                        c.render_data(w);
                    }
                });
            }),
        );
    }

    fn refresh_tree(&self, window: &AppWindow) {
        self.catalog_requested.borrow_mut().clear();
        let fetches = self.tree.borrow_mut().refresh();
        self.info.borrow_mut().clear();
        for fetch in fetches {
            self.dispatch(window, fetch);
        }
        self.publish(window);
        self.catalog_changed(window);
    }

    fn inspector_action(&self, window: &AppWindow, label: &str) {
        let Some(node) = self.selection.borrow().clone() else {
            return;
        };
        if maint_view::offered(&node).contains(&label) {
            self.maintenance(window, &node, label);
            return;
        }
        if self.object_action(window, &node, label) {
            return;
        }
        match label {
            action::CONNECT => self.connect(window, node.server()),
            action::DISCONNECT => self.disconnect_checked(window, node.server()),
            action::EDIT => {
                self.edit_server(window, node.server());
                window.set_screen("connections".into());
            },
            action::NEW_QUERY => {
                if let Some(database) = node.database() {
                    self.set_target(node.server(), database);
                }
                window.set_screen("workspace".into());
                self.publish(window);
            },
            action::BROWSE => {
                self.open_data(window, &node);
            },
            action::EDIT_STRUCTURE => self.design_edit(window, &node),
            action::DETAILS => self.open_object(window, &node, "columns"),
            action::IMPORT => {
                self.open_object(window, &node, "data");
                self.import_start(window);
            },
            action::SHOW_DDL => self.open_object(window, &node, "ddl"),
            action::OPEN_DEFINITION => self.open_object(window, &node, "definition"),
            action::REFRESH_MATVIEW => {
                if let Node::Object {
                    server,
                    database,
                    schema,
                    name,
                    ..
                } = &node
                {
                    self.set_target(*server, database);
                    // REFRESH takes the bare name: NusaDB keeps materialized views in
                    // `public` and refuses a qualifier here.
                    let _ = schema;
                    let statement = format!("REFRESH MATERIALIZED VIEW {}", quote_ident(name));
                    let Some(target) = self.target_for(*server, database) else {
                        return;
                    };
                    let weak = window.as_weak();
                    let key = node.id();
                    let label = name.clone();
                    self.toast(window, &format!("Refreshing {label}…"), false);
                    self.backend.run(
                        target,
                        vec![statement],
                        Box::new(move |result| {
                            on_ui(&weak, move |c, w| {
                                let failure = match &result {
                                    Err(m) => Some(m.clone()),
                                    Ok(r) => r.failure().map(|f| match &f.outcome {
                                        StatementOutcome::Failed { message, .. } => message.clone(),
                                        StatementOutcome::Done { .. } => String::new(),
                                    }),
                                };
                                match failure {
                                    Some(m) => c.toast(w, &format!("Refresh failed: {m}"), true),
                                    None => {
                                        c.toast(w, &format!("{label} refreshed"), false);
                                        c.info.borrow_mut().remove(&key);
                                        if let Some(node) = Node::parse(&key) {
                                            c.load_info(w, &node);
                                        }
                                    },
                                }
                            });
                        }),
                    );
                }
            },
            action::DIAGRAM => {
                if let Some(database) = node.database() {
                    self.set_target(node.server(), database);
                    window.set_screen("diagram".into());
                    self.erd_follow(window);
                    self.publish(window);
                }
            },
            action::SECURITY => {
                if let Some(database) = node.database() {
                    self.security_open_at(window, node.server(), database, None);
                }
            },
            action::TABLE_SECURITY => {
                if let Node::Object {
                    server,
                    database,
                    schema,
                    name,
                    ..
                } = &node
                {
                    self.security_open_at(
                        window,
                        *server,
                        database,
                        Some((schema.clone(), name.clone())),
                    );
                }
            },
            action::NEW_TABLE => {
                if let Some(database) = node.database() {
                    self.set_target(node.server(), database);
                }
                let schema = match &node {
                    Node::Schema { schema, .. } | Node::Group { schema, .. } => schema.clone(),
                    _ => "public".to_owned(),
                };
                self.design_new(window, &schema);
            },
            action::QUERY_TABLE => {
                if let Node::Object {
                    server,
                    database,
                    schema,
                    name,
                    kind,
                } = &node
                {
                    self.set_target(*server, database);
                    let sql = starter_for(*kind, schema, name);
                    self.new_tab(window, Some(name), &sql);
                    window.set_screen("workspace".into());
                    self.publish(window);
                }
            },
            _ => {},
        }
    }

    // ── Query screen ────────────────────────────────────────────────────────

    fn replace_sql(&self, window: &AppWindow, sql: &str) {
        self.sql.replace(sql.to_owned());
        window.set_sql_text(sql.into());
        publish_editor(window, sql);
    }

    fn sql_edited(&self, window: &AppWindow, text: &str, caret: usize) {
        self.caret.set(caret);
        let was_dirty = self.tab_dirty(self.active_tab.get());
        self.sql.replace(text.to_owned());
        if self.tab_dirty(self.active_tab.get()) != was_dirty {
            self.publish_tabs(window);
        }
        publish_editor(window, text);
        self.prefetch_columns(window, text);
        // Suggest while a name is being typed: after two characters, or right after `table.`.
        let word = complete::word_at(text, caret);
        let typing_name = caret > 0
            && text[..caret]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
        if typing_name && (word.prefix.chars().count() >= 2 || word.qualifier.is_some()) {
            self.show_completions(window, text, caret);
        } else {
            self.close_completions(window);
        }
    }

    /// Everything the editor can complete in the current database.
    fn vocabulary(&self) -> complete::Vocabulary {
        let mut vocab = complete::Vocabulary::default();
        let Some((server, database)) = self.target.borrow().clone() else {
            return vocab;
        };
        if let Some(catalog) = self.tree.borrow().catalog(server, &database) {
            for o in &catalog.objects {
                if o.kind.has_rows() && o.schema != "information_schema" {
                    vocab.relations.push((
                        o.schema.clone(),
                        o.name.clone(),
                        o.kind == ObjectKind::View,
                    ));
                }
            }
        }
        for (key, load) in self.info.borrow().iter() {
            if let (
                Some(Node::Object {
                    server: s,
                    database: d,
                    name,
                    ..
                }),
                Load::Ready(info),
            ) = (Node::parse(key), load)
                && s == server
                && d == database
            {
                for c in &info.columns {
                    vocab.columns.push((
                        name.clone(),
                        c.name.clone(),
                        short_type(&c.data_type).to_owned(),
                    ));
                }
            }
        }
        vocab
    }

    /// Describe the tables the script names, so their columns can be completed.
    fn prefetch_columns(&self, window: &AppWindow, text: &str) {
        let Some((server, database)) = self.target.borrow().clone() else {
            return;
        };
        let wanted = complete::referenced_tables(text);
        let objects: Vec<_> = self
            .tree
            .borrow()
            .catalog(server, &database)
            .map(|c| {
                c.objects
                    .iter()
                    .filter(|o| {
                        o.kind.has_rows() && wanted.iter().any(|w| w.eq_ignore_ascii_case(&o.name))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for o in objects {
            let node = Node::Object {
                server,
                database: database.clone(),
                schema: o.schema,
                name: o.name,
                kind: o.kind,
            };
            self.load_info(window, &node);
        }
    }

    fn show_completions(&self, window: &AppWindow, text: &str, caret: usize) {
        let (word, list) = complete::suggest(text, caret, &self.vocabulary(), 30);
        if list.is_empty() {
            self.close_completions(window);
            return;
        }
        let (line, col) = complete::line_col(text, word.start);
        let rows: Vec<CompletionRow> = list
            .iter()
            .map(|s| CompletionRow {
                label: s.label.as_str().into(),
                kind: s.kind.into(),
                detail: s.detail.as_str().into(),
            })
            .collect();
        window.set_completion_line(i32::try_from(line).unwrap_or(0));
        window.set_completion_col(i32::try_from(col).unwrap_or(0));
        window.set_completion_index(0);
        window.set_completions(ModelRc::new(VecModel::from(rows)));
        self.completion.replace(Some((word, caret, list)));
    }

    fn close_completions(&self, window: &AppWindow) {
        if self.completion.borrow_mut().take().is_some() {
            window.set_completions(ModelRc::default());
        }
    }

    fn accept_completion(&self, window: &AppWindow, index: usize) {
        let Some((word, caret, list)) = self.completion.borrow_mut().take() else {
            return;
        };
        window.set_completions(ModelRc::default());
        let Some(choice) = list.get(index) else {
            return;
        };
        let text = self.sql.borrow().clone();
        let (next, at) = complete::apply(&text, &word, caret, &choice.label);
        self.sql.replace(next.clone());
        window.set_sql_text(next.as_str().into());
        publish_editor(window, &next);
        self.move_caret(window, at, None);
    }

    /// Put the editor's caret at byte `at`, selecting up to `end` when given.
    fn move_caret(&self, window: &AppWindow, at: usize, end: Option<usize>) {
        self.caret.set(at);
        window.set_select_end(end.map_or(-1, |e| i32::try_from(e).unwrap_or(-1)));
        window.set_caret_target(i32::try_from(at).unwrap_or(0));
        self.caret_serial
            .set(self.caret_serial.get().wrapping_add(1));
        window.set_caret_serial(self.caret_serial.get());
        // Bring it into view: line and column (in characters) of byte `at`.
        let text = self.sql.borrow();
        let before = &text[..at.min(text.len())];
        let line = before.matches('\n').count();
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count());
        window.set_reveal_line(i32::try_from(line).unwrap_or(0));
        window.set_reveal_col(i32::try_from(col).unwrap_or(0));
        window.set_reveal_serial(self.caret_serial.get());
    }

    /// Keep the caret at byte `at` in view as it moves (typing, arrows, End), scrolling the
    /// editor sideways or down only when it would leave the visible area.
    fn reveal_at(&self, window: &AppWindow, at: usize) {
        let text = self.sql.borrow();
        let at = at.min(text.len());
        if !text.is_char_boundary(at) {
            return;
        }
        let before = &text[..at];
        let line = before.matches('\n').count();
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count());
        window.set_reveal_line(i32::try_from(line).unwrap_or(0));
        window.set_reveal_col(i32::try_from(col).unwrap_or(0));
        window.set_reveal_serial(window.get_reveal_serial().wrapping_add(1));
    }

    fn current_target(&self) -> Option<Target> {
        let (server, database) = self.target.borrow().clone()?;
        let live = self
            .store
            .borrow()
            .get(server)
            .is_some_and(|p| p.status == ConnectionStatus::Connected);
        if !live {
            return None;
        }
        self.target_for(server, &database)
    }

    fn run(&self, window: &AppWindow, kind: RunKind, statements: Vec<String>) {
        if self.running.get() || statements.is_empty() {
            return;
        }
        // A new result would replace the one with staged edits.
        if kind == RunKind::Script && self.result_has_unsaved(window) {
            return;
        }
        let Some(target) = self.current_target() else {
            return;
        };
        self.running.set(true);
        self.publish_target(window);
        self.publish_tabs(window);
        let weak = window.as_weak();
        let database = target.database.clone();
        let server = target.server;
        let tab = self.active_tab.get();
        self.backend.run(
            target,
            statements,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    c.route_run(w, tab, kind, server, database, result)
                });
            }),
        );
    }

    /// Deliver a finished run to the tab it was started from: straight to the window if that
    /// tab is showing, otherwise kept until it is.
    fn route_run(
        &self,
        window: &AppWindow,
        tab: u32,
        kind: RunKind,
        server: u32,
        database: String,
        result: Result<RunReport, String>,
    ) {
        if tab == self.active_tab.get() {
            self.finish_run(window, kind, server, &database, result);
            self.publish_tabs(window);
            return;
        }
        if let Ok(report) = &result {
            // The session's transaction state is real now, whichever tab is showing.
            if self
                .target
                .borrow()
                .as_ref()
                .is_some_and(|(s, d)| *s == server && *d == database)
            {
                self.txn.set(report.txn);
                self.publish_target(window);
            }
        }
        if let Some(entry) = self.tabs.borrow_mut().iter_mut().find(|t| t.id == tab) {
            entry.running = false;
            entry.deferred = Some(Deferred {
                kind,
                server,
                database,
                result,
            });
        }
        self.publish_tabs(window);
    }

    // ── Tabs ────────────────────────────────────────────────────────────────

    /// Copy the active tab's live state into its entry.
    fn stash_active(&self, window: &AppWindow) {
        let id = self.active_tab.get();
        let mut tabs = self.tabs.borrow_mut();
        let Some(tab) = tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        tab.sql = self.sql.borrow().clone();
        tab.target = self.target.borrow().clone();
        tab.running = self.running.get();
        tab.result = self.query_result.borrow().clone();
        tab.sort = self.query_sort.get();
        tab.result_sql = self.query_sql.borrow().clone();
        tab.result_edit = self.result_edit.borrow().clone();
        tab.parts = self.parts.borrow().clone();
        tab.part = self.part.get();
        tab.columns = window.get_grid_columns();
        tab.columns_width = window.get_grid_columns_width();
        tab.rows = window.get_grid_rows();
        tab.messages = window.get_messages();
        tab.json = window.get_json_text();
        tab.plan = window.get_plan_rows();
        tab.summary = window.get_result_summary();
        tab.rows_note = window.get_rows_note();
        tab.has_error = window.get_has_error();
        tab.result_tab = window.get_result_tab();
        tab.status_rows = window.get_status_rows();
        tab.status_timing = window.get_status_timing();
    }

    /// Make tab `id` the one showing.
    fn activate_tab(&self, window: &AppWindow, id: u32) {
        if !self.tabs.borrow().iter().any(|t| t.id == id) {
            return;
        }
        if id != self.active_tab.get() {
            self.stash_active(window);
        }
        self.active_tab.set(id);
        let deferred = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| t.id == id) else {
                return;
            };
            self.sql.replace(tab.sql.clone());
            let target_changed = *self.target.borrow() != tab.target;
            self.target.replace(tab.target.clone());
            if target_changed {
                self.txn.set(TxnState::Idle);
            }
            self.running.set(tab.running);
            self.query_result.replace(tab.result.clone());
            self.query_sort.set(tab.sort);
            self.query_sql.replace(tab.result_sql.clone());
            self.result_edit.replace(tab.result_edit.clone());
            self.parts.replace(tab.parts.clone());
            self.part.set(tab.part);
            window.set_grid_columns(tab.columns.clone());
            window.set_grid_columns_width(tab.columns_width);
            window.set_grid_rows(tab.rows.clone());
            window.set_messages(tab.messages.clone());
            window.set_json_text(tab.json.clone());
            window.set_plan_rows(tab.plan.clone());
            window.set_result_summary(tab.summary.clone());
            window.set_rows_note(tab.rows_note.clone());
            window.set_has_error(tab.has_error);
            window.set_result_tab(tab.result_tab.clone());
            window.set_status_rows(tab.status_rows.clone());
            window.set_status_timing(tab.status_timing.clone());
            tab.deferred.take()
        };
        window.set_cell_open(false);
        window.set_grid_selected_row(-1);
        window.set_grid_selected_col(-1);
        let sql = self.sql.borrow().clone();
        window.set_sql_text(sql.as_str().into());
        publish_editor(window, &sql);
        window.set_results_error(SharedString::new());
        self.forget_history(Which::Query);
        self.result_shape_refresh(window);
        self.publish_parts(window);
        if let Some(done) = deferred {
            self.finish_run(window, done.kind, done.server, &done.database, done.result);
        }
        self.publish_tabs(window);
        self.publish(window);
    }

    /// Open a new tab, optionally with `sql` and a title, and show it.
    fn new_tab(&self, window: &AppWindow, title: Option<&str>, sql: &str) {
        let id = self.next_tab.get();
        self.next_tab.set(id + 1);
        // "Query n" takes the lowest number no open tab uses, so closing tabs frees numbers.
        let title = title.map_or_else(
            || {
                let used: Vec<String> =
                    self.tabs.borrow().iter().map(|t| t.title.clone()).collect();
                let n = (1..)
                    .find(|n| !used.iter().any(|t| *t == format!("Query {n}")))
                    .unwrap_or(1);
                format!("Query {n}")
            },
            str::to_owned,
        );
        let mut tab = QueryTab::new(id, title);
        tab.sql = sql.to_owned();
        // A new tab starts on the database the user is working in.
        tab.target = self.target.borrow().clone();
        self.stash_active(window);
        self.tabs.borrow_mut().push(tab);
        self.activate_tab(window, id);
        // A new tab is for typing: the editor takes the keyboard, caret at the end.
        let end = self.sql.borrow().len();
        self.move_caret(window, end, None);
    }

    fn close_tab(&self, window: &AppWindow, id: u32) {
        let (index, count) = {
            let tabs = self.tabs.borrow();
            (tabs.iter().position(|t| t.id == id), tabs.len())
        };
        let Some(index) = index else {
            return;
        };
        if count == 1 {
            // The last tab closes too: the Query screen then shows "No query open".
            self.tabs.borrow_mut().clear();
            self.active_tab.set(u32::MAX);
            self.sql.replace(String::new());
            window.set_sql_text(SharedString::new());
            publish_editor(window, "");
            self.query_result.replace(None);
            self.result_edit.replace(None);
            self.parts.replace(Vec::new());
            self.publish_parts(window);
            window.set_cell_open(false);
            window.set_grid_columns(ModelRc::new(VecModel::<GridColumn>::default()));
            window.set_grid_rows(ModelRc::new(VecModel::<GridRow>::default()));
            window.set_messages(ModelRc::default());
            window.set_json_text(SharedString::new());
            window.set_plan_rows(ModelRc::default());
            window.set_result_summary(SharedString::new());
            window.set_rows_note(SharedString::new());
            window.set_status_rows(SharedString::new());
            window.set_status_timing(SharedString::new());
            window.set_result_tab("results".into());
            self.chart_reset(window);
            self.publish_tabs(window);
            self.publish(window);
            return;
        }
        let was_active = id == self.active_tab.get();
        self.tabs.borrow_mut().remove(index);
        if was_active {
            let next = {
                let tabs = self.tabs.borrow();
                tabs[index.min(tabs.len() - 1)].id
            };
            // The closed tab's state is gone; make sure activation does not stash it back.
            self.active_tab.set(next);
            self.activate_tab_fresh(window, next);
        } else {
            self.publish_tabs(window);
        }
    }

    /// Activate without stashing the current (already removed) tab.
    fn activate_tab_fresh(&self, window: &AppWindow, id: u32) {
        self.active_tab.set(u32::MAX);
        self.activate_tab(window, id);
    }

    fn publish_tabs(&self, window: &AppWindow) {
        let active = self.active_tab.get();
        let store = self.store.borrow();
        let rows: Vec<QueryTabRow> = self
            .tabs
            .borrow()
            .iter()
            .map(|t| {
                let is_active = t.id == active;
                let target = if is_active {
                    self.target.borrow().clone()
                } else {
                    t.target.clone()
                };
                let running = if is_active {
                    self.running.get()
                } else {
                    t.running
                };
                QueryTabRow {
                    id: i32::try_from(t.id).unwrap_or(-1),
                    title: t.title.as_str().into(),
                    target: target
                        .and_then(|(s, d)| store.get(s).map(|p| format!("{} / {d}", p.name)))
                        .unwrap_or_default()
                        .into(),
                    active: is_active,
                    running,
                    unseen: t.deferred.is_some(),
                    dirty: self.tab_dirty(t.id),
                }
            })
            .collect();
        window.set_query_tabs(ModelRc::new(VecModel::from(rows)));
    }

    fn run_script(&self, window: &AppWindow) {
        let statements = split_statements(&self.sql.borrow());
        self.run_statements(window, statements);
    }

    /// The part of the script the caret points at: the selection when there is one, otherwise
    /// the statement under the caret. Offsets are bytes into the editor's text.
    fn current_part(&self, cursor: i32, anchor: i32) -> Vec<String> {
        let sql = self.sql.borrow();
        let (Ok(cursor), Ok(anchor)) = (usize::try_from(cursor), usize::try_from(anchor)) else {
            return Vec::new();
        };
        let (a, b) = (
            cursor.min(anchor).min(sql.len()),
            cursor.max(anchor).min(sql.len()),
        );
        if a != b && sql.is_char_boundary(a) && sql.is_char_boundary(b) {
            return split_statements(&sql[a..b]);
        }
        crate::sql_split::statement_at(&sql, cursor)
            .into_iter()
            .collect()
    }

    /// ⌘↩ / Ctrl+Enter: run the selection, or the statement at the caret.
    fn run_current(&self, window: &AppWindow, cursor: i32, anchor: i32) {
        let statements = self.current_part(cursor, anchor);
        self.run_statements(window, statements);
    }

    fn run_statements(&self, window: &AppWindow, statements: Vec<String>) {
        if statements.is_empty() {
            window.set_messages(ModelRc::new(VecModel::from(vec![MessageRow {
                ok: false,
                title: "Nothing to run. The editor holds only comments.".into(),
                detail: SharedString::new(),
                time: SharedString::new(),
            }])));
            window.set_result_tab("messages".into());
            return;
        }
        let production = self.target_is_production();
        if let Some(found) = risk::assess(&statements, production) {
            self.ask(
                window,
                Pending::Run(RunKind::Script, statements),
                if production {
                    "Run this on a production server?"
                } else {
                    "This script can destroy data"
                },
                &found.reasons,
                "Run anyway",
            );
            return;
        }
        self.run(window, RunKind::Script, statements);
    }

    /// Whether the editor's target server is tagged production.
    fn target_is_production(&self) -> bool {
        let target = self.target.borrow();
        let store = self.store.borrow();
        target
            .as_ref()
            .and_then(|(s, _)| store.get(*s))
            .is_some_and(|p| p.environment == Environment::Prod)
    }

    /// Open the confirmation dialog for `action`. A reason that is a SQL statement is shown
    /// in the code font; plain sentences are not.
    fn ask(
        &self,
        window: &AppWindow,
        action: Pending,
        title: &str,
        reasons: &[String],
        accept: &str,
    ) {
        let on = |server: u32, database: &str, tail: &str| {
            self.store
                .borrow()
                .get(server)
                .map(|p| format!("On {} / {database}. {tail}", p.name))
                .unwrap_or_default()
        };
        let subtitle = match &action {
            // Closing an edited script touches no database; say what does happen instead.
            Pending::CloseTab(id) if self.tab_dirty(*id) => {
                "The file on disk keeps its last saved version.".to_owned()
            },
            Pending::CloseTab(_) | Pending::Quit | Pending::Disconnect(_) => String::new(),
            Pending::Destroy(node, _) => on(
                node.server(),
                node.database().unwrap_or_default(),
                "This cannot be undone.",
            ),
            _ => self
                .target
                .borrow()
                .clone()
                .map(|(s, d)| on(s, &d, "This cannot be undone once committed."))
                .unwrap_or_default(),
        };
        self.pending.replace(Some(action));
        window.set_confirm_title(title.into());
        window.set_confirm_subtitle(subtitle.into());
        let codes: Vec<bool> = reasons.iter().map(|r| is_statement(r)).collect();
        let reasons: Vec<SharedString> = reasons.iter().map(|r| r.as_str().into()).collect();
        window.set_confirm_bullets(codes.iter().filter(|c| !**c).count() > 1);
        window.set_confirm_codes(ModelRc::new(VecModel::from(codes)));
        window.set_confirm_reasons(ModelRc::new(VecModel::from(reasons)));
        window.set_confirm_accept_label(accept.into());
        window.set_confirm_open(true);
    }

    fn confirm(&self, window: &AppWindow, accepted: bool) {
        window.set_confirm_open(false);
        let Some(action) = self.pending.take() else {
            return;
        };
        if !accepted {
            return;
        }
        match action {
            Pending::Run(kind, statements) => self.run(window, kind, statements),
            Pending::SaveData => self.data_save_confirmed(window),
            Pending::SaveResult => self.result_save_confirmed(window),
            Pending::ApplyDesign => self.design_apply_confirmed(window),
            Pending::ApplySecurity => self.security_apply_confirmed(window),
            Pending::FinishPrepared(sql) => self.monitor_finish(window, sql),
            Pending::Destroy(node, sql) => self.object_destroy_confirmed(window, &node, &sql),
            Pending::Quit => self.quit_confirmed(window),
            Pending::Disconnect(id) => self.disconnect(window, id),
            Pending::Import => self.import_confirmed(window),
            Pending::CloseTab(id) => self.close_tab(window, id),
            Pending::DropVectorIndex(sql) => self.vectors_drop_confirmed(window, &sql),
        }
    }

    /// Explain what Run would run: the selection or the statement at the caret.
    fn explain_current(&self, window: &AppWindow, cursor: i32, anchor: i32) {
        let statements: Vec<String> = self
            .current_part(cursor, anchor)
            .into_iter()
            .map(|s| format!("EXPLAIN {s}"))
            .collect();
        self.run(window, RunKind::Explain, statements);
    }

    /// `LISTEN ch` / `UNLISTEN ch` run from the editor also drive the Notifications panel,
    /// whose own listener is what receives the messages.
    fn follow_listen(&self, window: &AppWindow, report: &RunReport) {
        for statement in &report.statements {
            if !matches!(statement.outcome, StatementOutcome::Done { .. }) {
                continue;
            }
            let mut words = statement.sql.split_whitespace();
            let verb = words.next().unwrap_or_default().to_ascii_uppercase();
            let channel = words.next().unwrap_or_default().trim_end_matches(';');
            match verb.as_str() {
                "LISTEN" if !channel.is_empty() => self.notify_listen(window, channel),
                "UNLISTEN" if channel == "*" => {
                    let all = self.notify.borrow().channels.clone();
                    for name in all {
                        self.notify_unlisten(window, &name);
                    }
                },
                "UNLISTEN" if !channel.is_empty() => {
                    self.notify_unlisten(window, &channel.trim_matches('"').to_lowercase());
                },
                _ => {},
            }
        }
    }

    /// The status-bar numbers for a block selected in the Query (`data` false) or Data grid.
    fn selection_summary(
        &self,
        window: &AppWindow,
        data: bool,
        (r1, c1, r2, c2): (i32, i32, i32, i32),
    ) {
        let set = if data {
            self.data_result.borrow().clone()
        } else {
            self.query_result
                .borrow()
                .as_ref()
                .map(|r| sorted(r, self.query_sort.get()))
        };
        let text = match (
            set,
            usize::try_from(r1),
            usize::try_from(c1),
            usize::try_from(r2),
            usize::try_from(c2),
        ) {
            (Some(set), Ok(r1), Ok(c1), Ok(r2), Ok(c2)) => crate::stats::summary(
                set.rows
                    .iter()
                    .skip(r1)
                    .take(r2.saturating_sub(r1) + 1)
                    .flat_map(|row| row.iter().skip(c1).take(c2.saturating_sub(c1) + 1))
                    .map(Option::as_deref),
            ),
            _ => String::new(),
        };
        window
            .global::<crate::ui::SelectionStore>()
            .set_summary(text.into());
    }

    /// Show result set `index` of the last run in the grid, JSON tab and status bar.
    fn show_part(&self, window: &AppWindow, index: usize) {
        let Some(part) = self.parts.borrow().get(index).cloned() else {
            return;
        };
        self.part.set(index);
        window
            .global::<crate::ui::SelectionStore>()
            .set_summary(SharedString::new());
        let result = &part.result;
        self.query_result.replace(Some(result.clone()));
        self.query_sql.replace(Some(part.sql.clone()));
        self.query_sort.set(None);
        self.result_edit.replace(None);
        self.forget_history(Which::Query);
        window.set_cell_open(false);
        window.set_grid_selected_row(-1);
        window.set_grid_selected_col(-1);
        self.render_query_grid(window);
        self.result_shape_refresh(window);
        window.set_json_text(to_json(result).into());
        let n = result.rows.len();
        window.set_rows_note(
            if n >= crate::driver::row_cap() {
                format!(
                    "Showing the first {} rows. Add a LIMIT or a WHERE to see the rest.",
                    group_thousands(crate::driver::row_cap())
                )
            } else {
                String::new()
            }
            .into(),
        );
        window.set_status_rows(rows_label(n).into());
        window.set_result_summary(
            format!("{} · {}", rows_label(n), format_elapsed(part.elapsed)).into(),
        );
        self.chart_reset(window);
    }

    /// The result tabs under the editor; none when the run returned one result set or none.
    fn publish_parts(&self, window: &AppWindow) {
        let parts = self.parts.borrow();
        let rows: Vec<ResultPart> = if parts.len() < 2 {
            Vec::new()
        } else {
            parts
                .iter()
                .enumerate()
                .map(|(i, p)| ResultPart {
                    label: part_label(i, &p.sql, &parts).into(),
                    sql: first_line(&p.sql).into(),
                    rows: rows_label(p.result.rows.len()).into(),
                })
                .collect()
        };
        window.set_result_parts(ModelRc::new(VecModel::from(rows)));
        window.set_result_part(i32::try_from(self.part.get()).unwrap_or(0));
    }

    /// A result tab picked: refused while the showing one has unsaved edits.
    fn pick_part(&self, window: &AppWindow, index: usize) {
        if index == self.part.get() || self.result_has_unsaved(window) {
            window.set_result_part(i32::try_from(self.part.get()).unwrap_or(0));
            return;
        }
        self.show_part(window, index);
        self.publish_parts(window);
        window.set_result_tab("results".into());
    }

    fn finish_run(
        &self,
        window: &AppWindow,
        kind: RunKind,
        server: u32,
        database: &str,
        result: Result<RunReport, String>,
    ) {
        self.running.set(false);
        let report = match result {
            Ok(report) => report,
            Err(message) => {
                // The session could not be opened at all.
                window.set_messages(ModelRc::new(VecModel::from(vec![MessageRow {
                    ok: false,
                    title: message.into(),
                    detail: format!("while connecting to {database}").into(),
                    time: SharedString::new(),
                }])));
                window.set_has_error(true);
                window.set_result_tab("messages".into());
                self.publish_target(window);
                return;
            },
        };

        self.txn.set(report.txn);
        self.follow_listen(window, &report);
        let failure = report.failure().is_some();
        window.set_has_error(failure);
        window.set_messages(ModelRc::new(VecModel::from(messages(&report))));
        window.set_status_timing(format_elapsed(report.elapsed()).into());

        match kind {
            RunKind::Script => {
                let parts: Vec<Part> = report
                    .statements
                    .iter()
                    .filter_map(|s| match &s.outcome {
                        StatementOutcome::Done { result, .. } if !result.columns.is_empty() => {
                            Some(Part {
                                sql: s.sql.clone(),
                                result: result.clone(),
                                elapsed: s.elapsed,
                            })
                        },
                        _ => None,
                    })
                    .collect();
                let shown = !parts.is_empty();
                // The last result set shows first, as before; the others are a click away.
                let last = parts.len().saturating_sub(1);
                self.parts.replace(parts);
                self.part.set(last);
                if shown {
                    self.show_part(window, last);
                } else {
                    self.query_result.replace(None);
                    self.result_edit.replace(None);
                    self.result_shape_refresh(window);
                    window.set_cell_open(false);
                    window.set_grid_columns(ModelRc::new(VecModel::<GridColumn>::default()));
                    window.set_grid_rows(ModelRc::new(VecModel::<GridRow>::default()));
                    window.set_json_text(SharedString::new());
                    window.set_rows_note(SharedString::new());
                    window.set_status_rows(SharedString::new());
                    window.set_result_summary(
                        format!(
                            "{} statement{} · {}",
                            report.statements.len(),
                            if report.statements.len() == 1 {
                                ""
                            } else {
                                "s"
                            },
                            format_elapsed(report.elapsed())
                        )
                        .into(),
                    );
                }
                self.publish_parts(window);
                self.record_history(window, server, database, &report);
                window.set_result_tab(
                    if failure || !shown {
                        "messages"
                    } else {
                        "results"
                    }
                    .into(),
                );
                // A script that changed the schema makes the loaded catalog stale.
                if report.statements.iter().any(|s| changes_schema(&s.sql))
                    && self.tree.borrow().catalog(server, database).is_some()
                {
                    let id = Node::Database {
                        server,
                        database: database.to_owned(),
                    }
                    .id();
                    self.tree.borrow_mut().forget_catalog(server, database);
                    if self.tree.borrow().is_expanded(&id)
                        && let Some(fetch) = self.tree.borrow_mut().expand(&id)
                    {
                        self.dispatch(window, fetch);
                    }
                    self.publish(window);
                }
            },
            RunKind::Explain => {
                let mut steps = Vec::new();
                for statement in &report.statements {
                    if let StatementOutcome::Done { result, .. } = &statement.outcome {
                        let sql = statement
                            .sql
                            .strip_prefix("EXPLAIN ")
                            .unwrap_or(&statement.sql);
                        let lines: Vec<String> = result
                            .rows
                            .iter()
                            .map(|row| {
                                row.iter()
                                    .map(|v| v.as_deref().unwrap_or("NULL"))
                                    .collect::<Vec<_>>()
                                    .join("  ")
                            })
                            .collect();
                        steps.extend(plan_rows(first_line(sql), &lines));
                    }
                }
                window.set_plan_rows(ModelRc::new(VecModel::from(steps)));
                window.set_result_tab(if failure { "messages" } else { "plan" }.into());
            },
            RunKind::Txn => {
                if failure {
                    window.set_result_tab("messages".into());
                }
            },
        }
        self.publish_target(window);
    }

    fn stop(&self) {
        if let Some(target) = self.current_target() {
            self.backend.cancel(&target);
        }
    }

    // ── Data screen ─────────────────────────────────────────────────────────

    fn open_data(&self, window: &AppWindow, node: &Node) {
        let Node::Object {
            server,
            database,
            schema,
            name,
            kind,
        } = node
        else {
            return;
        };
        if !kind.has_rows() {
            return;
        }
        if self.data_has_unsaved(window) {
            window.set_screen("datagrid".into());
            return;
        }
        self.data.replace(Some(DataState {
            server: *server,
            database: database.clone(),
            schema: schema.clone(),
            name: name.clone(),
            kind: *kind,
            filter: String::new(),
            sort: None,
            page: 0,
            total: None,
            sample: 0,
            seed: 0,
            edits: EditSet::default(),
        }));
        window.set_data_sample(0);
        self.page.replace(Some(node.clone()));
        window.set_data_tab("data".into());
        self.load_info(window, node);
        self.publish_page(window);
        window.set_data_cell_open(false);
        window.set_data_selected_row(-1);
        window.set_data_selected_col(-1);
        window.set_data_filter(SharedString::new());
        window.set_data_rows(ModelRc::new(VecModel::<GridRow>::default()));
        window.set_data_columns(ModelRc::new(VecModel::<GridColumn>::default()));
        window.set_screen("datagrid".into());
        self.load_data(window);
    }

    fn load_data(&self, window: &AppWindow) {
        let Some(state) = self.data.borrow().clone() else {
            self.publish_data(window);
            return;
        };
        let Some(target) = self.target_for(state.server, &state.database) else {
            return;
        };
        let mut from = format!(
            "{}.{}",
            quote_ident(&state.schema),
            quote_ident(&state.name)
        );
        if state.sample > 0 && state.kind == ObjectKind::Table {
            from.push_str(&format!(
                " TABLESAMPLE BERNOULLI ({}) REPEATABLE ({})",
                state.sample, state.seed
            ));
        }
        let filter = state.filter.trim();
        let clause = if filter.is_empty() {
            String::new()
        } else {
            format!(" WHERE {filter}")
        };
        let order = state
            .sort
            .as_ref()
            .map_or_else(String::new, |(column, desc)| {
                format!(
                    " ORDER BY {}{}",
                    quote_ident(column),
                    if *desc { " DESC" } else { "" }
                )
            });
        let statements = vec![
            format!("SELECT count(*) FROM {from}{clause}"),
            format!(
                "SELECT * FROM {from}{clause}{order} LIMIT {} OFFSET {}",
                self.page_size.get(),
                state.page * self.page_size.get()
            ),
        ];
        self.data_loading.set(true);
        window
            .global::<crate::ui::SelectionStore>()
            .set_summary(SharedString::new());
        window.set_data_error(SharedString::new());
        self.publish_data(window);
        let weak = window.as_weak();
        self.backend.run(
            target,
            statements,
            Box::new(move |result| on_ui(&weak, move |c, w| c.data_loaded(w, result))),
        );
    }

    fn data_loaded(&self, window: &AppWindow, result: Result<RunReport, String>) {
        self.data_loading.set(false);
        // New rows: earlier edits no longer apply to them.
        self.forget_history(Which::Data);
        let report = match result {
            Ok(report) => report,
            Err(message) => {
                window.set_data_error(message.into());
                self.publish_data(window);
                return;
            },
        };
        if let Some(failed) = report.failure()
            && let StatementOutcome::Failed { message, .. } = &failed.outcome
        {
            window.set_data_error(message.as_str().into());
            self.publish_data(window);
            return;
        }
        let total = report.statements.first().and_then(|s| match &s.outcome {
            StatementOutcome::Done { result, .. } => result
                .rows
                .first()
                .and_then(|row| row.first().cloned().flatten())
                .and_then(|n| n.parse::<usize>().ok()),
            StatementOutcome::Failed { .. } => None,
        });
        if let Some(state) = self.data.borrow_mut().as_mut() {
            state.total = total;
        }
        if let Some(StatementOutcome::Done { result, .. }) =
            report.statements.get(1).map(|s| &s.outcome)
        {
            let sort = self.data.borrow().as_ref().and_then(|d| {
                d.sort.as_ref().and_then(|(name, desc)| {
                    result
                        .columns
                        .iter()
                        .position(|c| c == name)
                        .map(|i| (i, *desc))
                })
            });
            let _ = sort;
            self.data_result.replace(Some(result.clone()));
            window.set_data_cell_open(false);
            window.set_data_selected_row(-1);
            window.set_data_selected_col(-1);
            self.render_data(window);
        }
        window.set_status_timing(format_elapsed(report.elapsed()).into());
        self.publish_data(window);
    }

    fn publish_data(&self, window: &AppWindow) {
        let data = self.data.borrow();
        let Some(state) = data.as_ref() else {
            // An object without rows (a function, a sequence) still has its page.
            if self.page.borrow().is_none() {
                window.set_data_object_label(SharedString::new());
            }
            return;
        };
        let server = self
            .store
            .borrow()
            .get(state.server)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        window.set_data_object_label(format!("{}.{}", state.schema, state.name).into());
        window.set_data_object_kind(kind_tag(state.kind).into());
        window.set_data_target_label(format!("{server} / {}", state.database).into());
        let first = state.page * self.page_size.get();
        // Loaded rows only: staged new rows are not part of the table yet.
        let shown = self
            .data_result
            .borrow()
            .as_ref()
            .map_or(0, |r| r.rows.len());
        window.set_data_first_row(i32::try_from(first + 1).unwrap_or(i32::MAX));
        let range = match (shown, state.total) {
            (0, Some(0)) => "No rows".to_owned(),
            (0, _) => String::new(),
            (n, Some(total)) => format!(
                "{}–{} of {}",
                group_thousands(first + 1),
                group_thousands(first + n),
                group_thousands(total)
            ),
            (n, None) => format!(
                "{}–{}",
                group_thousands(first + 1),
                group_thousands(first + n)
            ),
        };
        window.set_data_range_label(range.into());
        window.set_data_can_prev(state.page > 0);
        window.set_data_can_next(
            state
                .total
                .is_some_and(|total| first + self.page_size.get() < total),
        );
        window.set_data_loading(self.data_loading.get());
    }

    fn data_filter(&self, window: &AppWindow, filter: &str) {
        if self.data_has_unsaved(window) {
            return;
        }
        if let Some(state) = self.data.borrow_mut().as_mut() {
            state.filter = filter.to_owned();
            state.page = 0;
        }
        self.load_data(window);
    }

    fn data_page(&self, window: &AppWindow, forward: bool) {
        if self.data_has_unsaved(window) {
            return;
        }
        if let Some(state) = self.data.borrow_mut().as_mut() {
            state.page = if forward {
                state.page + 1
            } else {
                state.page.saturating_sub(1)
            };
        }
        self.load_data(window);
    }

    /// Settings → Rows per page; takes effect at the next load.
    pub fn set_page_size(&self, rows: usize) {
        self.page_size.set(rows.max(1));
        // A page is fetched in one result, so the result cap never cuts one short.
        crate::driver::set_row_cap(crate::driver::row_cap().max(rows));
    }

    /// Settings → Rows kept in the results and scripts kept in History.
    pub fn set_limits(&self, result_rows: usize, history: usize) {
        crate::driver::set_row_cap(result_rows.max(self.page_size.get()));
        self.history_limit.set(history.max(1));
    }

    fn data_open_in_query(&self, window: &AppWindow) {
        let Some(state) = self.data.borrow().clone() else {
            return;
        };
        let filter = state.filter.trim();
        let sql = format!(
            "SELECT *\nFROM {}.{}{}{}\nLIMIT {};\n",
            quote_ident(&state.schema),
            quote_ident(&state.name),
            if filter.is_empty() {
                String::new()
            } else {
                format!("\nWHERE {filter}")
            },
            state
                .sort
                .as_ref()
                .map_or_else(String::new, |(column, desc)| format!(
                    "\nORDER BY {}{}",
                    quote_ident(column),
                    if *desc { " DESC" } else { "" }
                )),
            self.page_size.get()
        );
        self.set_target(state.server, &state.database);
        self.new_tab(window, Some(&state.name), &sql);
        window.set_screen("workspace".into());
        self.publish(window);
    }

    // ── Grid: ordering and cells ────────────────────────────────────────────

    /// Draw the Query grid from the kept result, in the user's chosen order.
    fn render_query_grid(&self, window: &AppWindow) {
        let Some(result) = self.query_result.borrow().clone() else {
            return;
        };
        let sort = self.query_sort.get();
        let shown = sorted(&result, sort);
        let (mut columns, _, rows) = grid(
            &shown,
            crate::driver::row_cap(),
            sort,
            &self.widths(Which::Query),
        );
        let width = self.hide_columns(window, Which::Query, &mut columns);
        window.set_grid_columns(ModelRc::new(VecModel::from(columns)));
        window.set_grid_columns_width(width);
        window.set_grid_rows(ModelRc::new(VecModel::from(rows)));
        window.set_cell_open(false);
        window.set_grid_selected_row(-1);
        window.set_grid_selected_col(-1);
        self.publish_result_edits(window);
    }

    /// Header click on the Query grid: ascending → descending → as returned. Sorting happens
    /// here on the rows already fetched, so it never re-runs a script that may have side effects.
    fn grid_sort(&self, window: &AppWindow, column: usize) {
        // Staged edits are tied to the rows as they are ordered now.
        if self.result_has_unsaved(window) {
            return;
        }
        self.query_sort
            .set(next_sort(self.query_sort.get(), column));
        self.render_query_grid(window);
    }

    fn grid_cell(&self, window: &AppWindow, row: usize, column: usize) {
        if let Some((name, kind, value, staged)) = self.query_value(row, column) {
            window.set_cell_column(name.into());
            window.set_cell_kind(kind.into());
            window.set_cell_null(value.is_none());
            let text: SharedString = value.unwrap_or_default().into();
            window.set_cell_value(text.clone());
            window.set_cell_draft(text);
            window.set_results_cell_staged(staged);
            window.set_cell_open(true);
        }
    }

    /// Header click on the Data grid: the order is applied by the server, from the first page.
    fn data_sort(&self, window: &AppWindow, column: usize) {
        if self.data_has_unsaved(window) {
            return;
        }
        let name = self
            .data_result
            .borrow()
            .as_ref()
            .and_then(|r| r.columns.get(column).cloned());
        let Some(name) = name else {
            return;
        };
        if let Some(state) = self.data.borrow_mut().as_mut() {
            state.sort = match &state.sort {
                Some((current, false)) if *current == name => Some((name, true)),
                Some((current, true)) if *current == name => None,
                _ => Some((name, false)),
            };
            state.page = 0;
        }
        self.load_data(window);
    }

    fn data_cell(&self, window: &AppWindow, row: usize, column: usize) {
        let Some((name, kind, value, staged, new_row)) = self.data_value(row, column) else {
            return;
        };
        window.set_data_cell_column(name.as_str().into());
        window.set_data_cell_kind(kind.into());
        window.set_data_cell_null(value.is_none());
        let text: SharedString = value.unwrap_or_default().into();
        window.set_data_cell_value(text.clone());
        window.set_data_cell_draft(text);
        window.set_data_cell_staged(staged);
        window.set_data_cell_new_row(new_row);
        window.set_data_cell_open(true);
    }

    /// A Data grid cell as shown: its column, type, value with staged edits laid over, whether
    /// it is staged, and whether it is in a new row.
    fn data_value(
        &self,
        row: usize,
        column: usize,
    ) -> Option<(String, String, Option<String>, bool, bool)> {
        let result = self.data_result.borrow();
        let result = result.as_ref()?;
        let data = self.data.borrow();
        let state = data.as_ref()?;
        let name = result.columns.get(column)?.clone();
        let kind = result.types.get(column).cloned().unwrap_or_default();
        let loaded = result.rows.len();
        let (value, staged, new_row) = if row < loaded {
            let staged = state.edits.updates.get(&(row, column));
            let value = match staged {
                Some(edits::Value::Text(t)) => Some(t.clone()),
                Some(_) => None,
                None => result.rows[row].get(column).cloned().flatten(),
            };
            (value, staged.is_some(), false)
        } else {
            let staged = state
                .edits
                .inserts
                .get(row - loaded)
                .and_then(|r| r.get(column));
            match staged {
                Some(edits::Value::Text(t)) => (Some(t.clone()), true, true),
                Some(edits::Value::Null) => (None, true, true),
                _ => (None, false, true),
            }
        };
        Some((name, kind, value, staged, new_row))
    }

    /// The columns (with cast types) and primary-key positions of the open table, or why it
    /// cannot be edited.
    fn data_edit_shape(&self) -> Result<(Vec<edits::Column>, Vec<usize>), String> {
        let data = self.data.borrow();
        let state = data.as_ref().ok_or_else(String::new)?;
        if state.kind != ObjectKind::Table {
            return Err("Read-only: views cannot be edited".to_owned());
        }
        let node = Node::Object {
            server: state.server,
            database: state.database.clone(),
            schema: state.schema.clone(),
            name: state.name.clone(),
            kind: state.kind,
        };
        let info = self.info.borrow();
        let Some(Load::Ready(info)) = info.get(&node.id()) else {
            return Err("Loading table details…".to_owned());
        };
        let result = self.data_result.borrow();
        let result = result.as_ref().ok_or_else(String::new)?;
        let columns: Vec<edits::Column> = result
            .columns
            .iter()
            .map(|name| edits::Column {
                name: name.clone(),
                data_type: info
                    .columns
                    .iter()
                    .find(|c| c.name == *name)
                    .map_or_else(|| "text".to_owned(), |c| c.data_type.clone()),
            })
            .collect();
        let key: Vec<usize> = info
            .constraints
            .iter()
            .find(|c| c.kind == "PRIMARY KEY")
            .map(|pk| {
                pk.columns
                    .iter()
                    .filter_map(|k| result.columns.iter().position(|c| c == k))
                    .collect()
            })
            .unwrap_or_default();
        if key.is_empty() {
            return Err("Read-only: this table has no primary key".to_owned());
        }
        Ok((columns, key))
    }

    /// Draw the Data grid: loaded rows with staged edits laid over them, then new rows.
    fn render_data(&self, window: &AppWindow) {
        let Some(result) = self.data_result.borrow().clone() else {
            return;
        };
        let data = self.data.borrow();
        let Some(state) = data.as_ref() else {
            return;
        };
        let edits = &state.edits;
        let mut shown = result.clone();
        let width = result.columns.len();
        let mut states: Vec<Vec<&'static str>> = vec![vec![""; width]; result.rows.len()];
        for (r, row) in shown.rows.iter_mut().enumerate() {
            if edits.deletes.contains(&r) {
                states[r] = vec!["deleted"; width];
                continue;
            }
            for (c, cell) in row.iter_mut().enumerate() {
                match edits.updates.get(&(r, c)) {
                    Some(edits::Value::Text(t)) => {
                        *cell = Some(t.clone());
                        states[r][c] = "edited";
                    },
                    Some(edits::Value::Null) => {
                        *cell = None;
                        states[r][c] = "edited";
                    },
                    _ => {},
                }
            }
        }
        for new in &edits.inserts {
            shown.rows.push(
                new.iter()
                    .map(|v| match v {
                        edits::Value::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect(),
            );
            states.push(
                new.iter()
                    .map(|v| {
                        if *v == edits::Value::Default {
                            "default"
                        } else {
                            "new"
                        }
                    })
                    .collect(),
            );
        }
        let sort = state.sort.as_ref().and_then(|(name, desc)| {
            result
                .columns
                .iter()
                .position(|c| c == name)
                .map(|i| (i, *desc))
        });
        let pending = edits.count();
        drop(data);
        let widths = self.widths(Which::Data);
        let (mut columns, _, mut rows) = grid(&shown, self.page_size.get() + 1_000, sort, &widths);
        let width_px = self.hide_columns(window, Which::Data, &mut columns);
        for (row, row_states) in rows.iter_mut().zip(&states) {
            for (c, st) in row_states.iter().enumerate() {
                if let Some(mut cell) = row.cells.row_data(c) {
                    cell.state = (*st).into();
                    row.cells.set_row_data(c, cell);
                }
            }
        }
        window.set_data_columns(ModelRc::new(VecModel::from(columns)));
        window.set_data_columns_width(width_px);
        window.set_data_rows(ModelRc::new(VecModel::from(rows)));
        window.set_data_pending(i32::try_from(pending).unwrap_or(i32::MAX));
        match self.data_edit_shape() {
            Ok(_) => {
                window.set_data_editable(true);
                window.set_data_read_only_reason(SharedString::new());
            },
            Err(reason) => {
                window.set_data_editable(false);
                window.set_data_read_only_reason(reason.into());
            },
        }
    }

    /// Run `change` on the staged edits, then redraw (keeping the cell panel in step).
    fn edit_data(
        &self,
        window: &AppWindow,
        change: impl FnOnce(&mut EditSet, &[Vec<Option<String>>], usize),
    ) {
        if self.data_edit_shape().is_err() {
            return;
        }
        let loaded = self
            .data_result
            .borrow()
            .as_ref()
            .map(|r| r.rows.clone())
            .unwrap_or_default();
        let width = self
            .data_result
            .borrow()
            .as_ref()
            .map_or(0, |r| r.columns.len());
        if let Some(state) = self.data.borrow_mut().as_mut() {
            self.remember(Which::Data, &state.edits);
            change(&mut state.edits, &loaded, width);
        }
        window.set_data_error(SharedString::new());
        self.render_data(window);
        self.publish_data(window);
        let (row, col) = (
            window.get_data_selected_row(),
            window.get_data_selected_col(),
        );
        if window.get_data_cell_open()
            && let (Ok(row), Ok(col)) = (usize::try_from(row), usize::try_from(col))
        {
            self.data_cell(window, row, col);
        }
    }

    fn selected_data_cell(window: &AppWindow) -> Option<(usize, usize)> {
        Some((
            usize::try_from(window.get_data_selected_row()).ok()?,
            usize::try_from(window.get_data_selected_col()).ok()?,
        ))
    }

    fn data_cell_set(&self, window: &AppWindow, value: edits::Value) {
        let Some((row, col)) = Self::selected_data_cell(window) else {
            return;
        };
        self.edit_data(window, |edits, loaded, _| {
            edits.set(loaded, row, col, value)
        });
    }

    /// An inline edit kept in the grid: stage it for that cell.
    fn data_cell_commit(&self, window: &AppWindow, row: usize, col: usize, text: &str) {
        let value = edits::Value::Text(text.to_owned());
        self.edit_data(window, |edits, loaded, _| {
            edits.set(loaded, row, col, value)
        });
    }

    fn data_cell_revert(&self, window: &AppWindow) {
        let Some((row, col)) = Self::selected_data_cell(window) else {
            return;
        };
        self.edit_data(window, |edits, loaded, _| {
            if row < loaded.len() {
                edits.updates.remove(&(row, col));
            } else {
                edits.set(loaded, row, col, edits::Value::Default);
            }
        });
    }

    fn data_add_row(&self, window: &AppWindow) {
        self.edit_data(window, |edits, _, width| edits.add_row(width));
        // Select the first cell of the new row, ready for typing.
        let loaded = self
            .data_result
            .borrow()
            .as_ref()
            .map_or(0, |r| r.rows.len());
        let inserts = self
            .data
            .borrow()
            .as_ref()
            .map_or(0, |d| d.edits.inserts.len());
        let row = loaded + inserts.saturating_sub(1);
        window.set_data_selected_row(i32::try_from(row).unwrap_or(-1));
        window.set_data_selected_col(0);
        // Typing fills the cell in place; the value panel stays closed unless it was open.
        if window.get_data_cell_open() {
            self.data_cell(window, row, 0);
        }
    }

    /// Mark rows `first..=last` for deletion (or unmark them); new rows in the range go away.
    fn data_delete_row(&self, window: &AppWindow, first: usize, last: usize) {
        self.edit_data(window, |edits, loaded, _| {
            // Highest first, so removing an unsaved new row does not shift the ones before it.
            for row in (first..=last).rev() {
                edits.toggle_delete(loaded.len(), row);
            }
        });
    }

    /// All rows, or a fresh random sample of `percent` (paging stays inside the same draw).
    fn data_pick_sample(&self, window: &AppWindow, percent: i32) {
        if self.data_has_unsaved(window) {
            window.set_data_sample(
                self.data
                    .borrow()
                    .as_ref()
                    .map_or(0, |d| i32::try_from(d.sample).unwrap_or(0)),
            );
            return;
        }
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.subsec_nanos() % 1_000_000);
        if let Some(state) = self.data.borrow_mut().as_mut() {
            state.sample = u32::try_from(percent).unwrap_or(0);
            state.seed = seed;
            state.page = 0;
        }
        window.set_data_sample(percent);
        self.load_data(window);
    }

    fn data_discard(&self, window: &AppWindow) {
        self.forget_history(Which::Data);
        if let Some(state) = self.data.borrow_mut().as_mut() {
            state.edits = EditSet::default();
        }
        window.set_data_cell_open(false);
        self.render_data(window);
        self.publish_data(window);
    }

    /// The unsaved-changes guard: navigation that would throw edits away is refused, with the
    /// reason shown where the user is looking.
    fn data_has_unsaved(&self, window: &AppWindow) -> bool {
        let pending = self.data.borrow().as_ref().map_or(0, |d| d.edits.count());
        if pending > 0 {
            window.set_data_error(
                format!(
                    "{} unsaved change{}. Save or discard {} first.",
                    pending,
                    if pending == 1 { "" } else { "s" },
                    if pending == 1 { "it" } else { "them" }
                )
                .into(),
            );
        }
        pending > 0
    }

    fn data_save(&self, window: &AppWindow) {
        // ⌘S / Ctrl+S with nothing staged, or while a save is under way, does nothing.
        if self
            .data
            .borrow()
            .as_ref()
            .is_none_or(|d| d.edits.count() == 0)
            || window.get_data_saving()
        {
            return;
        }
        let production = self.data.borrow().as_ref().is_some_and(|d| {
            self.store
                .borrow()
                .get(d.server)
                .is_some_and(|p| p.environment == Environment::Prod)
        });
        if production {
            let count = self.data.borrow().as_ref().map_or(0, |d| d.edits.count());
            let table = self
                .data
                .borrow()
                .as_ref()
                .map(|d| format!("{}.{}", d.schema, d.name))
                .unwrap_or_default();
            self.ask(
                window,
                Pending::SaveData,
                "Save these changes to production?",
                &[format!(
                    "{count} row{} of {table} on a production server",
                    if count == 1 { "" } else { "s" }
                )],
                "Save to production",
            );
            return;
        }
        self.data_save_confirmed(window);
    }

    fn data_save_confirmed(&self, window: &AppWindow) {
        let Ok((columns, key)) = self.data_edit_shape() else {
            return;
        };
        let Some(state) = self.data.borrow().clone() else {
            return;
        };
        let loaded = self
            .data_result
            .borrow()
            .as_ref()
            .map(|r| r.rows.clone())
            .unwrap_or_default();
        let table = format!(
            "{}.{}",
            quote_ident(&state.schema),
            quote_ident(&state.name)
        );
        let statements = match state.edits.statements(&table, &columns, &key, &loaded) {
            Ok(statements) if statements.is_empty() => return,
            Ok(statements) => statements,
            Err(message) => {
                window.set_data_error(message.into());
                return;
            },
        };
        let Some(target) = self.target_for(state.server, &state.database) else {
            return;
        };
        // Inside the user's own open transaction the changes join it; otherwise they get one
        // of their own, so a failure part-way leaves the table exactly as it was.
        let joins_open_txn = self.txn.get() != TxnState::Idle
            && self
                .target
                .borrow()
                .as_ref()
                .is_some_and(|(s, d)| *s == state.server && *d == state.database);
        let mut script = statements;
        if !joins_open_txn {
            script.insert(0, "BEGIN".to_owned());
            script.push("COMMIT".to_owned());
        }
        window.set_data_saving(true);
        let weak = window.as_weak();
        let rollback_target = target.clone();
        self.backend.run(
            target,
            script,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    c.data_saved(w, result, joins_open_txn, rollback_target);
                });
            }),
        );
    }

    fn data_saved(
        &self,
        window: &AppWindow,
        result: Result<RunReport, String>,
        joined_txn: bool,
        target: Target,
    ) {
        window.set_data_saving(false);
        let failure = match &result {
            Err(message) => Some(message.clone()),
            Ok(report) => report.failure().map(|f| match &f.outcome {
                StatementOutcome::Failed { code, message } => {
                    format!(
                        "{} (in: {})",
                        explain(code.as_deref(), message),
                        first_line(&f.sql)
                    )
                },
                StatementOutcome::Done { .. } => String::new(),
            }),
        };
        if let Some(message) = failure {
            if !joined_txn {
                // The transaction this save opened is now failed; close it so the session is
                // usable again. Nothing it did is kept.
                self.backend
                    .run(target, vec!["ROLLBACK".to_owned()], Box::new(|_| {}));
                window.set_data_error(format!("Nothing was saved: {message}").into());
            } else {
                window.set_data_error(
                    format!("Not saved, and your open transaction has failed: {message}").into(),
                );
            }
            if let Ok(report) = result {
                self.txn.set(if joined_txn {
                    report.txn
                } else {
                    TxnState::Idle
                });
            }
            self.publish_target(window);
            return;
        }
        if let Ok(report) = &result {
            self.txn.set(report.txn);
        }
        let saved = self.data.borrow().as_ref().map_or(0, |d| d.edits.count());
        self.toast(
            window,
            &format!("Saved {saved} row{}", if saved == 1 { "" } else { "s" }),
            false,
        );
        if let Some(state) = self.data.borrow_mut().as_mut() {
            state.edits = EditSet::default();
            // Row counts in the inspector are stale now.
            let node = Node::Object {
                server: state.server,
                database: state.database.clone(),
                schema: state.schema.clone(),
                name: state.name.clone(),
                kind: state.kind,
            };
            self.info.borrow_mut().remove(&node.id());
        }
        if let Some(node) = self.selection.borrow().clone() {
            self.load_info(window, &node);
        }
        self.publish_target(window);
        self.load_data(window);
    }

    // ── Designer ────────────────────────────────────────────────────────────

    fn design_new(&self, window: &AppWindow, schema: &str) {
        let Some((server, database)) = self.target.borrow().clone() else {
            window.set_screen("designer".into());
            return;
        };
        let spec = designer::TableSpec {
            schema: schema.to_owned(),
            name: String::new(),
            columns: vec![designer::ColumnSpec {
                name: "id".to_owned(),
                data_type: "serial".to_owned(),
                nullable: false,
                default: String::new(),
                primary: true,
                original: None,
            }],
            primary_key_name: None,
            indexes: Vec::new(),
        };
        self.design.replace(Some(DesignState {
            server,
            database,
            original: None,
            spec,
        }));
        window.set_design_selected(-1);
        window.set_screen("designer".into());
        self.publish_design(window, true);
    }

    /// Open an existing table, from its described shape.
    fn design_edit(&self, window: &AppWindow, node: &Node) {
        let Node::Object {
            server,
            database,
            schema,
            name,
            kind: ObjectKind::Table,
        } = node
        else {
            return;
        };
        let info = match self.info.borrow().get(&node.id()) {
            Some(Load::Ready(info)) => Some(info.clone()),
            _ => None,
        };
        let Some(info) = info else {
            // Not described yet: describe it, then come back here.
            self.design_reopen.replace(Some(node.clone()));
            self.load_info(window, node);
            window.set_screen("designer".into());
            return;
        };
        let pk = info.constraints.iter().find(|c| c.kind == "PRIMARY KEY");
        let spec = designer::TableSpec {
            schema: schema.clone(),
            name: name.clone(),
            columns: info
                .columns
                .iter()
                .map(|c| designer::ColumnSpec {
                    name: c.name.clone(),
                    data_type: c.type_spec.clone(),
                    nullable: c.nullable,
                    default: c.default.clone().unwrap_or_default(),
                    primary: pk.is_some_and(|k| k.columns.contains(&c.name)),
                    original: Some(c.name.clone()),
                })
                .collect(),
            primary_key_name: pk.map(|k| k.name.clone()),
            // Indexes a constraint creates (the key, UNIQUE, a foreign key's) change with
            // that constraint, so only the ones made on purpose are editable here.
            indexes: info
                .indexes
                .iter()
                .filter(|i| !index_backs_constraint(&i.name, &info))
                .map(|i| designer::IndexSpec {
                    name: i.name.clone(),
                    columns: i.columns.join(", "),
                    unique: i.unique,
                    predicate: String::new(),
                    original: Some(i.name.clone()),
                })
                .collect(),
        };
        self.set_target(*server, database);
        self.design.replace(Some(DesignState {
            server: *server,
            database: database.clone(),
            original: Some(spec.clone()),
            spec,
        }));
        window.set_design_selected(-1);
        window.set_screen("designer".into());
        self.publish_design(window, true);
    }

    /// The SQL the designer would run, or what is wrong with the form.
    fn design_sql(state: &DesignState) -> Result<Vec<String>, String> {
        match &state.original {
            None => designer::create_all(&state.spec),
            Some(before) => designer::alter_sql(before, &state.spec),
        }
    }

    /// Push the designer's state into the window. `rebuild` replaces the column rows (after
    /// adding, removing or retyping); otherwise rows are updated in place so a field being
    /// typed into keeps its focus and caret.
    fn publish_design(&self, window: &AppWindow, rebuild: bool) {
        let design = self.design.borrow();
        let Some(state) = design.as_ref() else {
            window.set_design_mode(SharedString::new());
            return;
        };
        let server = self
            .store
            .borrow()
            .get(state.server)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        window.set_design_mode(
            if state.original.is_some() {
                "edit"
            } else {
                "new"
            }
            .into(),
        );
        window.set_design_original(
            state
                .original
                .as_ref()
                .map(|o| format!("{}.{}", o.schema, o.name))
                .unwrap_or_default()
                .into(),
        );
        window.set_design_target(format!("{server} / {}", state.database).into());
        window.set_design_schema(state.spec.schema.as_str().into());
        window.set_design_name(state.spec.name.as_str().into());
        let rows: Vec<DesignColumn> = state
            .spec
            .columns
            .iter()
            .map(|c| {
                let before = state.original.as_ref().and_then(|o| {
                    c.original
                        .as_ref()
                        .and_then(|name| o.columns.iter().find(|x| x.name == *name))
                });
                let column_state = match before {
                    None => "new",
                    Some(b) if b != c => "changed",
                    Some(_) => "",
                };
                DesignColumn {
                    name: c.name.as_str().into(),
                    r#type: c.data_type.as_str().into(),
                    nullable: c.nullable,
                    default: c.default.as_str().into(),
                    primary: c.primary,
                    state: column_state.into(),
                }
            })
            .collect();
        let index_rows: Vec<DesignIndex> = state
            .spec
            .indexes
            .iter()
            .map(|i| {
                let before = state.original.as_ref().and_then(|o| {
                    i.original
                        .as_ref()
                        .and_then(|name| o.indexes.iter().find(|x| x.name == *name))
                });
                DesignIndex {
                    name: i.name.as_str().into(),
                    columns: i.columns.as_str().into(),
                    unique: i.unique,
                    predicate: i.predicate.as_str().into(),
                    state: match before {
                        None => "new",
                        Some(b) if b != i => "changed",
                        Some(_) => "",
                    }
                    .into(),
                }
            })
            .collect();
        if rebuild {
            self.design_model.set_vec(rows);
            self.design_index_model.set_vec(index_rows);
        } else {
            sync_model(&self.design_model, rows);
            sync_model(&self.design_index_model, index_rows);
        }
        match Self::design_sql(state) {
            Ok(statements) => {
                let ddl = statements
                    .iter()
                    .map(|s| format!("{s};"))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                window.set_design_has_changes(!statements.is_empty());
                window.set_design_ddl(ddl.into());
                window.set_design_problem(SharedString::new());
            },
            Err(problem) => {
                window.set_design_has_changes(true);
                window.set_design_problem(problem.into());
            },
        }
    }

    fn design_change(
        &self,
        window: &AppWindow,
        rebuild: bool,
        change: impl FnOnce(&mut designer::TableSpec),
    ) {
        if let Some(state) = self.design.borrow_mut().as_mut() {
            change(&mut state.spec);
        }
        self.publish_design(window, rebuild);
    }

    fn design_field(&self, window: &AppWindow, row: usize, field: &str, value: &str) {
        self.design_change(window, false, |spec| {
            if let Some(c) = spec.columns.get_mut(row) {
                match field {
                    "name" => c.name = value.to_owned(),
                    "type" => c.data_type = value.to_owned(),
                    "default" => c.default = value.to_owned(),
                    _ => {},
                }
            }
        });
    }

    fn design_flag(&self, window: &AppWindow, row: usize, flag: &str, on: bool) {
        self.design_change(window, false, |spec| {
            if let Some(c) = spec.columns.get_mut(row) {
                match flag {
                    "not-null" => c.nullable = !on,
                    "primary" => {
                        c.primary = on;
                        if on {
                            c.nullable = false;
                        }
                    },
                    _ => {},
                }
            }
        });
    }

    fn design_apply(&self, window: &AppWindow) {
        let (statements, production) = {
            let design = self.design.borrow();
            let Some(state) = design.as_ref() else {
                return;
            };
            let Ok(statements) = Self::design_sql(state) else {
                return;
            };
            let production = self
                .store
                .borrow()
                .get(state.server)
                .is_some_and(|p| p.environment == Environment::Prod);
            (statements, production)
        };
        if statements.is_empty() {
            return;
        }
        if let Some(found) = risk::assess(&statements, production) {
            self.ask(
                window,
                Pending::ApplyDesign,
                if production {
                    "Change this table on production?"
                } else {
                    "This change removes data"
                },
                &found.reasons,
                "Apply anyway",
            );
            return;
        }
        self.design_apply_confirmed(window);
    }

    fn design_apply_confirmed(&self, window: &AppWindow) {
        let Some(state) = self.design.borrow().clone() else {
            return;
        };
        let Ok(mut script) = Self::design_sql(&state) else {
            return;
        };
        let Some(target) = self.target_for(state.server, &state.database) else {
            return;
        };
        // Schema changes are transactional here: all of them, or none.
        script.insert(0, "BEGIN".to_owned());
        script.push("COMMIT".to_owned());
        window.set_design_applying(true);
        let weak = window.as_weak();
        let rollback = target.clone();
        self.backend.run(
            target,
            script,
            Box::new(move |result| on_ui(&weak, move |c, w| c.design_applied(w, result, rollback))),
        );
    }

    fn design_applied(
        &self,
        window: &AppWindow,
        result: Result<RunReport, String>,
        target: Target,
    ) {
        window.set_design_applying(false);
        let failure = match &result {
            Err(message) => Some(message.clone()),
            Ok(report) => report.failure().map(|f| match &f.outcome {
                StatementOutcome::Failed { code, message } => {
                    format!(
                        "{} (in: {})",
                        explain(code.as_deref(), message),
                        first_line(&f.sql)
                    )
                },
                StatementOutcome::Done { .. } => String::new(),
            }),
        };
        if let Some(message) = failure {
            self.backend
                .run(target, vec!["ROLLBACK".to_owned()], Box::new(|_| {}));
            window.set_design_problem(format!("Nothing was changed: {message}").into());
            self.toast(
                window,
                "The table was not changed. See the message above the SQL.",
                true,
            );
            return;
        }
        let Some(state) = self.design.borrow().clone() else {
            return;
        };
        let created = state.original.is_none();
        self.toast(
            window,
            &format!(
                "{} {}.{}",
                if created { "Created" } else { "Updated" },
                state.spec.schema,
                state.spec.name
            ),
            false,
        );
        // The navigator and every description of this table are stale now.
        let (server, database) = (state.server, state.database.clone());
        if let Some(old) = &state.original {
            let old_node = Node::Object {
                server,
                database: database.clone(),
                schema: old.schema.clone(),
                name: old.name.clone(),
                kind: ObjectKind::Table,
            };
            self.info.borrow_mut().remove(&old_node.id());
        }
        self.tree.borrow_mut().forget_catalog(server, &database);
        self.catalog_changed(window);
        let db_node = Node::Database {
            server,
            database: database.clone(),
        }
        .id();
        if self.tree.borrow().is_expanded(&db_node)
            && let Some(fetch) = self.tree.borrow_mut().expand(&db_node)
        {
            self.dispatch(window, fetch);
        }
        // Reopen the table as it now is, so further edits diff against the real thing.
        let node = Node::Object {
            server,
            database,
            schema: state.spec.schema.clone(),
            name: state.spec.name.trim().to_owned(),
            kind: ObjectKind::Table,
        };
        self.info.borrow_mut().remove(&node.id());
        self.design_reopen.replace(Some(node.clone()));
        self.load_info(window, &node);
        self.publish(window);
    }

    // ── Copy and export ─────────────────────────────────────────────────────

    /// Show a short notice in the corner.
    fn toast(&self, window: &AppWindow, text: &str, error: bool) {
        window.set_toast_text(text.into());
        window.set_toast_error(error);
        self.toast_serial
            .set(self.toast_serial.get().wrapping_add(1));
        window.set_toast_serial(self.toast_serial.get());
    }

    fn copy_rows(&self, window: &AppWindow, result: &ResultSet, rows: &[Vec<Option<String>>]) {
        window.invoke_copy_text(export::tsv(&result.columns, rows).into());
        self.toast(
            window,
            &format!(
                "Copied {} with the header, tab-separated and ready to paste into a spreadsheet",
                rows_label(rows.len())
            ),
            false,
        );
    }

    fn copy_query_row(&self, window: &AppWindow) {
        let Some(result) = self.query_result.borrow().clone() else {
            return;
        };
        let shown = sorted(&result, self.query_sort.get());
        if let Ok(row) = usize::try_from(window.get_grid_selected_row())
            && let Some(values) = shown.rows.get(row)
        {
            self.copy_rows(window, &shown, std::slice::from_ref(values));
        }
    }

    fn copy_query_all(&self, window: &AppWindow) {
        let Some(result) = self.query_result.borrow().clone() else {
            return;
        };
        let shown = sorted(&result, self.query_sort.get());
        self.copy_rows(window, &shown, &shown.rows);
    }

    fn data_copy_row(&self, window: &AppWindow) {
        let Some(result) = self.data_result.borrow().clone() else {
            return;
        };
        if let Ok(row) = usize::try_from(window.get_data_selected_row())
            && let Some(values) = result.rows.get(row)
        {
            self.copy_rows(window, &result, std::slice::from_ref(values));
        }
    }

    /// Ask where to save an export. `None` when the user cancels.
    fn choose_export_path(default_name: &str) -> Option<PathBuf> {
        // Debug builds can be driven without a desktop portal (automated UI checks).
        #[cfg(debug_assertions)]
        if let Some(dir) = std::env::var_os("NUSADB_STUDIO_EXPORT_DIR") {
            return Some(PathBuf::from(dir).join(default_name));
        }
        rfd::FileDialog::new()
            .set_title("Export rows")
            .set_file_name(default_name)
            .add_filter("CSV", &["csv"])
            .add_filter("Excel workbook", &["xlsx"])
            .add_filter("JSON", &["json"])
            .add_filter("SQL INSERT statements", &["sql"])
            .add_filter("JSON Lines (pandas, Spark)", &["jsonl"])
            .add_filter("TSV", &["tsv"])
            .add_filter("Markdown table", &["md"])
            .save_file()
    }

    /// Stream `sql` on `target` into a file the user picks.
    fn export_sql(&self, window: &AppWindow, target: Target, sql: String, default_name: &str) {
        let Some(path) = Self::choose_export_path(default_name) else {
            return;
        };
        let format = export::Format::from_path(&path);
        self.toast(window, &format!("Exporting to {}…", path.display()), false);
        let weak = window.as_weak();
        let shown = path.display().to_string();
        self.backend.export(
            target,
            sql,
            path,
            format,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| match result {
                    Ok(n) => c.toast(w, &format!("Exported {} to {shown}", rows_label(n)), false),
                    Err(message) => c.toast(w, &format!("Export failed: {message}"), true),
                });
            }),
        );
    }

    fn export_query(&self, window: &AppWindow) {
        let Some(result) = self.query_result.borrow().clone() else {
            return;
        };
        let sql = self.query_sql.borrow().clone();
        // A read-only statement is run again and streamed, so the file holds every row rather
        // than the 1,000 the grid keeps. Anything that writes is never re-run: its fetched rows
        // are saved as they are.
        if let Some(sql) = sql
            && risk::assess(std::slice::from_ref(&sql), true).is_none()
            && let Some(target) = self.current_target()
        {
            self.export_sql(window, target, sql, "result.csv");
            return;
        }
        let Some(path) = Self::choose_export_path("result.csv") else {
            return;
        };
        let shown = sorted(&result, self.query_sort.get());
        let bytes = export::to_bytes(
            export::Format::from_path(&path),
            &shown.columns,
            &shown.types,
            &shown.rows,
            export::table_from_path(&path),
        );
        match std::fs::write(&path, bytes) {
            Ok(()) => self.toast(
                window,
                &format!(
                    "Exported {} to {}",
                    rows_label(shown.rows.len()),
                    path.display()
                ),
                false,
            ),
            Err(e) => self.toast(window, &format!("Export failed: {e}"), true),
        }
    }

    fn data_export(&self, window: &AppWindow) {
        let Some(state) = self.data.borrow().clone() else {
            return;
        };
        let Some(target) = self.target_for(state.server, &state.database) else {
            return;
        };
        let filter = state.filter.trim();
        let sql = format!(
            "SELECT * FROM {}.{}{}{}",
            quote_ident(&state.schema),
            quote_ident(&state.name),
            if filter.is_empty() {
                String::new()
            } else {
                format!(" WHERE {filter}")
            },
            state
                .sort
                .as_ref()
                .map_or_else(String::new, |(column, desc)| format!(
                    " ORDER BY {}{}",
                    quote_ident(column),
                    if *desc { " DESC" } else { "" }
                ))
        );
        self.export_sql(window, target, sql, &format!("{}.csv", state.name));
    }

    // ── History ─────────────────────────────────────────────────────────────

    fn record_history(&self, window: &AppWindow, server: u32, database: &str, report: &RunReport) {
        let server_name = self
            .store
            .borrow()
            .get(server)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let summary = match (report.failure(), report.last_result()) {
            (Some(failed), _) => match &failed.outcome {
                StatementOutcome::Failed { message, .. } => format!("failed: {message}"),
                StatementOutcome::Done { .. } => String::new(),
            },
            (None, Some(result)) => rows_label(result.rows.len()),
            (None, None) => format!(
                "{} statement{}",
                report.statements.len(),
                if report.statements.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ),
        };
        let entry = HistoryEntry {
            sql: self.sql.borrow().trim().to_owned(),
            target: format!("{server_name} / {database}"),
            when: unix_now(),
            ok: report.failure().is_none(),
            summary,
        };
        {
            let mut history = self.history.borrow_mut();
            // Running the same script again moves it to the top instead of repeating it.
            history.retain(|h| !(h.sql == entry.sql && h.target == entry.target));
            history.insert(0, entry);
            history.truncate(self.history_limit.get());
        }
        if let Some(path) = &self.history_path {
            let _ = save_history(path, &self.history.borrow());
        }
        self.publish_history(window);
    }

    fn publish_history(&self, window: &AppWindow) {
        let now = unix_now();
        let rows: Vec<HistoryRow> = self
            .history
            .borrow()
            .iter()
            .map(|h| HistoryRow {
                title: first_line(&h.sql).into(),
                meta: format!("{} · {} · {}", h.target, ago(now, h.when), h.summary).into(),
                ok: h.ok,
            })
            .collect();
        window.set_history(ModelRc::new(VecModel::from(rows)));
    }

    fn pick_history(&self, window: &AppWindow, index: usize) {
        let sql = self.history.borrow().get(index).map(|h| h.sql.clone());
        if let Some(sql) = sql {
            self.replace_sql(window, &format!("{sql}\n"));
        }
    }

    // ── Connection test ─────────────────────────────────────────────────────

    /// Validate the form and ask the probe whether the server is reachable.
    fn test(&self, window: &AppWindow) {
        let valid = match read_draft(window).validate() {
            Ok(valid) => valid,
            Err(error) => {
                window.set_form_error(error.to_string().into());
                clear_result(window);
                return;
            },
        };

        window.set_form_error(SharedString::new());
        window.set_test_state("testing".into());
        window.set_test_message(format!("Contacting {}…", valid.params.endpoint()).into());

        // A test is about the form's values, which may not be saved yet, so it reports in the
        // form only and leaves the saved server's status alone.
        let weak = window.as_weak();
        self.probe.probe(
            &valid.params,
            Box::new(move |outcome| {
                let (state, message) = render_outcome(&outcome);
                let _ = weak.upgrade_in_event_loop(move |window| {
                    window.invoke_probe_finished(state.into(), message.as_str().into());
                });
            }),
        );
    }
}

/// Wire every window callback to `controller` and paint the initial state.
pub fn install(window: &AppWindow, controller: Rc<Controller>) {
    ACTIVE.with(|active| {
        active.replace(Some((Rc::clone(&controller), window.as_weak())));
    });

    window.set_version(env!("CARGO_PKG_VERSION").into());
    guard_view::install_close_guard(window, &controller);
    window.set_tree_rows(ModelRc::from(
        Rc::clone(&controller.tree_model) as Rc<dyn Model<Data = TreeRow>>
    ));
    let starter = controller.sql.borrow().clone();
    controller.replace_sql(window, &starter);
    window.set_result_summary(SharedString::new());
    controller.reset_form(window);
    controller.publish_data(window);
    controller.publish_history(window);
    controller.publish_tabs(window);
    controller.security_models.bind(window);
    controller.vault_load(window);
    controller.open_from_args(window);
    window.set_erd_boxes(ModelRc::from(
        Rc::clone(&controller.erd_boxes) as Rc<dyn Model<Data = crate::ui::ErdBox>>
    ));
    window.set_design_indexes(ModelRc::from(
        Rc::clone(&controller.design_index_model) as Rc<dyn Model<Data = DesignIndex>>
    ));
    window.set_design_columns(ModelRc::from(
        Rc::clone(&controller.design_model) as Rc<dyn Model<Data = DesignColumn>>
    ));

    // Every callback has the same shape: upgrade the window, then hand both to a method.
    macro_rules! bind {
        ($setter:ident, | $c:ident, $w:ident $(, $arg:ident)* | $body:expr) => {{
            let weak = window.as_weak();
            let handler = Rc::clone(&controller);
            window.$setter(move |$($arg),*| {
                let Some($w) = weak.upgrade() else {
                    return;
                };
                let $c = &handler;
                $body;
            });
        }};
    }

    // Profile and Vectors tabs talk through globals rather than window properties.
    macro_rules! bind_global {
        ($global:ty, $setter:ident, | $c:ident, $w:ident $(, $arg:ident)* | $body:expr) => {{
            let weak = window.as_weak();
            let handler = Rc::clone(&controller);
            window.global::<$global>().$setter(move |$($arg),*| {
                let Some($w) = weak.upgrade() else {
                    return;
                };
                let $c = &handler;
                $body;
            });
        }};
    }
    bind_global!(
        crate::ui::SelectionStore,
        on_query_block,
        |c, w, r1, c1, r2, c2| {
            c.selection_summary(&w, false, (r1, c1, r2, c2));
        }
    );
    bind_global!(
        crate::ui::SelectionStore,
        on_data_block,
        |c, w, r1, c1, r2, c2| {
            c.selection_summary(&w, true, (r1, c1, r2, c2));
        }
    );
    bind_global!(crate::ui::PivotStore, on_opened, |c, w| c.pivot_opened(&w));
    bind_global!(crate::ui::PivotStore, on_set, |c, w, what, number, text| {
        c.pivot_set(&w, &what, number, &text);
    });
    bind_global!(
        crate::ui::PivotStore,
        on_copy_cells,
        |c, w, r1, c1, r2, c2, h| {
            c.pivot_copy(&w, r1, c1, r2, c2, h);
        }
    );
    bind_global!(crate::ui::PivotStore, on_open_sql, |c, w| c
        .pivot_open_sql(&w));
    bind_global!(crate::ui::ProfileStore, on_run, |c, w, sample| c
        .profile_run(&w, sample));
    bind_global!(crate::ui::ProfileStore, on_open_sql, |c, w| c
        .profile_open_sql(&w));
    bind_global!(crate::ui::ProfileStore, on_copy_markdown, |c, w| c
        .profile_copy_markdown(&w));
    bind_global!(crate::ui::ProfileStore, on_export_csv, |c, w| c
        .profile_export_csv(&w));
    bind_global!(crate::ui::VectorStore, on_pick_column, |c, w, i| {
        w.global::<crate::ui::VectorStore>().set_column(i);
        w.global::<crate::ui::VectorStore>()
            .set_plan(SharedString::new());
        c.vectors_publish(&w);
    });
    bind_global!(crate::ui::VectorStore, on_pick_metric, |c, w, i| {
        let _ = c;
        w.global::<crate::ui::VectorStore>().set_metric(i);
    });
    bind_global!(crate::ui::VectorStore, on_pick_text_column, |c, w, i| {
        let _ = c;
        w.global::<crate::ui::VectorStore>().set_text_column(i);
    });
    bind_global!(crate::ui::VectorStore, on_from_row, |c, w| c
        .vectors_from_row(&w));
    bind_global!(crate::ui::VectorStore, on_similar, |c, w| c
        .vectors_similar(&w));
    bind_global!(crate::ui::VectorStore, on_check_recall, |c, w| c
        .vectors_recall(&w));
    bind_global!(crate::ui::VectorStore, on_drop_index, |c, w, col, m| {
        c.vectors_drop_index(&w, col, m);
    });
    bind_global!(crate::ui::VectorStore, on_load_map, |c, w| c
        .vectors_map(&w));
    bind_global!(crate::ui::VectorStore, on_pick_label, |c, w, i| {
        w.global::<crate::ui::VectorStore>().set_label_column(i);
        c.vectors_map(&w);
    });
    bind_global!(crate::ui::VectorStore, on_map_pick, |c, w, i| c
        .vectors_map_pick(&w, i));
    bind_global!(crate::ui::VectorStore, on_search, |c, w| c
        .vectors_search(&w));
    bind_global!(crate::ui::VectorStore, on_create_index, |c, w, col, m| {
        c.vectors_create_index(&w, col, m);
    });
    bind_global!(crate::ui::VectorStore, on_open_sql, |c, w| c
        .vectors_open_sql(&w));
    bind_global!(
        crate::ui::VectorStore,
        on_copy_cells,
        |c, w, r1, c1, r2, c2, h| {
            c.vectors_copy(&w, r1, c1, r2, c2, h);
        }
    );

    bind!(on_filter_changed, |c, w, text| {
        c.tree_filter.replace(text.to_string());
        c.publish(&w);
    });
    bind!(on_select_node, |c, w, id| c.select_node(&w, id.as_str()));
    bind!(on_tree_key, |c, w, key| c.tree_key(&w, key.as_str()));
    bind!(on_toggle_node, |c, w, id| c.toggle_node(&w, id.as_str()));
    bind!(on_open_node, |c, w, id| c.open_node(&w, id.as_str()));
    bind!(on_refresh_tree, |c, w| c.refresh_tree(&w));
    bind!(on_collapse_tree, |c, w| {
        c.tree.borrow_mut().collapse_all();
        c.publish(&w);
    });
    bind!(on_inspector_action, |c, w, label| c
        .inspector_action(&w, label.as_str()));

    bind!(on_chart_opened, |c, w| c.chart_opened(&w));
    bind!(on_chart_set, |c, w, what, number, text| {
        c.chart_set(&w, &what, number, &text);
    });
    bind!(on_chart_copy_data, |c, w| c.chart_copy_data(&w));
    bind!(on_chart_export, |c, w| c.chart_export(&w));
    bind!(on_chart_copy, |c, w| c.chart_copy(&w));
    bind!(on_pick_part, |c, w, index| {
        if let Ok(index) = usize::try_from(index) {
            c.pick_part(&w, index);
        }
    });
    bind!(on_sql_caret, |c, w, at| {
        if let Ok(at) = usize::try_from(at) {
            c.reveal_at(&w, at);
        }
    });
    bind!(on_sql_edited, |c, w, text, caret| {
        c.sql_edited(&w, text.as_str(), usize::try_from(caret).unwrap_or(0));
    });
    bind!(on_complete_request, |c, w, text, caret| {
        c.show_completions(&w, text.as_str(), usize::try_from(caret).unwrap_or(0));
    });
    bind!(on_complete_accept, |c, w, index| {
        if let Ok(index) = usize::try_from(index) {
            c.accept_completion(&w, index);
        }
    });
    bind!(on_complete_cancel, |c, w| c.close_completions(&w));
    bind!(on_run_script, |c, w| c.run_script(&w));
    bind!(on_explain_current, |c, w, cursor, anchor| c
        .explain_current(&w, cursor, anchor));
    bind!(on_prompt_accept, |c, w| c.prompt_accept(&w));
    bind!(on_prompt_cancel, |c, w| c.prompt_cancel(&w));
    bind!(on_run_current, |c, w, cursor, anchor| c
        .run_current(&w, cursor, anchor));
    bind!(on_stop_script, |c, w| {
        let _ = &w;
        c.stop();
    });
    bind!(on_txn, |c, w, statement| c.run(
        &w,
        RunKind::Txn,
        vec![statement.to_string()]
    ));
    bind!(on_grid_sort, |c, w, column| {
        if let Ok(column) = usize::try_from(column) {
            c.grid_sort(&w, column);
        }
    });
    bind!(on_grid_cell, |c, w, row, column| {
        if let (Ok(row), Ok(column)) = (usize::try_from(row), usize::try_from(column)) {
            c.grid_cell(&w, row, column);
        }
    });
    bind!(on_grid_close_cell, |c, w| {
        let _ = c;
        w.set_cell_open(false);
        w.set_grid_selected_row(-1);
        w.set_grid_selected_col(-1);
    });
    // ── Grids: widths, copy, and editing a Query result ──
    bind!(on_grid_column_resized, |c, w, column, width| {
        if let Ok(column) = usize::try_from(column) {
            c.column_resized(&w, Which::Query, column, width);
        }
    });
    bind!(on_grid_column_fit, |c, w, column| {
        if let Ok(column) = usize::try_from(column) {
            c.column_fit(&w, Which::Query, column);
        }
    });
    bind!(on_data_column_resized, |c, w, column, width| {
        if let Ok(column) = usize::try_from(column) {
            c.column_resized(&w, Which::Data, column, width);
        }
    });
    bind!(on_data_column_fit, |c, w, column| {
        if let Ok(column) = usize::try_from(column) {
            c.column_fit(&w, Which::Data, column);
        }
    });
    {
        // Hit-testing for drag selection: answered on the spot, from the column models.
        let weak = window.as_weak();
        window.on_grid_col_at(move |x| {
            weak.upgrade()
                .map_or(0, |w| Controller::col_at(&w, Which::Query, x))
        });
        let weak = window.as_weak();
        window.on_data_col_at(move |x| {
            weak.upgrade()
                .map_or(0, |w| Controller::col_at(&w, Which::Data, x))
        });
    }
    bind!(on_grid_action, |c, w, name, r1, c1, r2, c2| {
        c.grid_action(&w, Which::Query, name.as_str(), [r1, c1, r2, c2]);
    });
    bind!(on_data_grid_action, |c, w, name, r1, c1, r2, c2| {
        c.grid_action(&w, Which::Data, name.as_str(), [r1, c1, r2, c2]);
    });
    bind!(on_grid_paste, |c, w, row, column, text| {
        if let (Ok(row), Ok(column)) = (usize::try_from(row), usize::try_from(column)) {
            c.paste(&w, Which::Query, row, column, text.as_str());
        }
    });
    bind!(on_data_paste, |c, w, row, column, text| {
        if let (Ok(row), Ok(column)) = (usize::try_from(row), usize::try_from(column)) {
            c.paste(&w, Which::Data, row, column, text.as_str());
        }
    });
    bind!(on_grid_cell_commit, |c, w, row, column, text| {
        if let (Ok(row), Ok(column)) = (usize::try_from(row), usize::try_from(column)) {
            c.result_set(&w, row, column, edits::Value::Text(text.to_string()));
        }
    });
    bind!(on_grid_cell_apply, |c, w, text| {
        if let Some((row, column)) = Controller::result_selected(&w) {
            c.result_set(&w, row, column, edits::Value::Text(text.to_string()));
        }
    });
    bind!(on_grid_cell_set_null, |c, w| {
        if let Some((row, column)) = Controller::result_selected(&w) {
            c.result_set(&w, row, column, edits::Value::Null);
        }
    });
    bind!(on_grid_cell_revert, |c, w| c.result_revert(&w));
    bind!(on_grid_save, |c, w| c.result_save(&w));
    bind!(on_grid_discard, |c, w| c.result_discard(&w));
    bind!(on_pick_history, |c, w, index| {
        if let Ok(index) = usize::try_from(index) {
            c.pick_history(&w, index);
        }
    });
    bind!(on_data_sort, |c, w, column| {
        if let Ok(column) = usize::try_from(column) {
            c.data_sort(&w, column);
        }
    });
    bind!(on_data_cell, |c, w, row, column| {
        if let (Ok(row), Ok(column)) = (usize::try_from(row), usize::try_from(column)) {
            c.data_cell(&w, row, column);
        }
    });
    bind!(on_data_close_cell, |c, w| {
        let _ = c;
        w.set_data_cell_open(false);
        w.set_data_selected_row(-1);
        w.set_data_selected_col(-1);
    });

    bind!(on_data_apply_filter, |c, w, text| c
        .data_filter(&w, text.as_str()));
    bind!(on_data_prev, |c, w| c.data_page(&w, false));
    bind!(on_data_next, |c, w| c.data_page(&w, true));
    bind!(on_data_reload, |c, w| {
        if !c.data_has_unsaved(&w) {
            c.load_data(&w);
        }
    });
    bind!(on_data_add_row, |c, w| c.data_add_row(&w));
    bind!(on_data_delete_row, |c, w, first, last| {
        if let (Ok(first), Ok(last)) = (usize::try_from(first), usize::try_from(last)) {
            c.data_delete_row(&w, first.min(last), first.max(last));
        }
    });
    bind!(on_data_discard, |c, w| c.data_discard(&w));
    bind!(on_data_pick_sample, |c, w, p| c.data_pick_sample(&w, p));
    bind!(on_data_save, |c, w| c.data_save(&w));
    bind!(on_confirm_accept, |c, w| c.confirm(&w, true));
    bind!(on_tab_new, |c, w| c.new_tab(&w, None, ""));
    bind!(on_design_new, |c, w| c.design_new(&w, "public"));
    bind!(on_design_renamed, |c, w, schema, name| {
        c.design_change(&w, false, |spec| {
            spec.schema = schema.to_string();
            spec.name = name.to_string();
        });
    });
    bind!(on_design_field, |c, w, row, field, value| {
        if let Ok(row) = usize::try_from(row) {
            c.design_field(&w, row, field.as_str(), value.as_str());
        }
    });
    bind!(on_design_flag, |c, w, row, flag, on| {
        if let Ok(row) = usize::try_from(row) {
            c.design_flag(&w, row, flag.as_str(), on);
        }
    });
    bind!(on_design_add, |c, w| {
        c.design_change(&w, true, |spec| {
            spec.columns.push(designer::ColumnSpec {
                data_type: "text".to_owned(),
                nullable: true,
                ..designer::ColumnSpec::default()
            });
        });
        let last = c
            .design
            .borrow()
            .as_ref()
            .map_or(0, |d| d.spec.columns.len());
        w.set_design_selected(i32::try_from(last).unwrap_or(1) - 1);
    });
    bind!(on_design_remove, |c, w, row| {
        if let Ok(row) = usize::try_from(row) {
            c.design_change(&w, true, |spec| {
                if row < spec.columns.len() {
                    spec.columns.remove(row);
                }
            });
            w.set_design_selected(-1);
        }
    });
    bind!(on_design_set_type, |c, w, row, ty| {
        if let Ok(row) = usize::try_from(row) {
            c.design_change(&w, true, |spec| {
                if let Some(col) = spec.columns.get_mut(row) {
                    col.data_type = ty.to_string();
                }
            });
        }
    });
    bind!(on_design_apply, |c, w| c.design_apply(&w));
    bind!(on_data_pick_tab, |c, w, id| c.page_pick_tab(&w, &id));
    bind!(on_data_open_code, |c, w, code| c.page_open_code(&w, &code));
    bind!(on_screen_picked, |c, w, _screen| {
        c.security_follow(&w);
        c.monitor_follow(&w);
        c.erd_follow(&w);
    });
    bind!(on_erd_pick_schema, |c, w, schema| c
        .erd_pick_schema(&w, &schema));
    bind!(on_erd_select, |c, w, row| c.erd_select(&w, row));
    bind!(on_erd_open, |c, w, row| c.erd_open(&w, row));
    bind!(on_erd_drag, |c, w, row, dx, dy| c.erd_drag(&w, row, dx, dy));
    bind!(on_erd_relayout, |c, w| c.erd_relayout(&w));
    bind!(on_erd_export, |c, w| c.erd_export(&w));
    bind!(on_erd_searched, |c, w, text| c.erd_search(&w, &text));
    bind!(on_erd_reload, |c, w| c.erd_reload(&w));
    bind!(on_notify_listen, |c, w, ch| c.notify_listen(&w, &ch));
    bind!(on_notify_unlisten, |c, w, ch| c.notify_unlisten(&w, &ch));
    bind!(on_notify_send, |c, w, ch, payload| c
        .notify_send(&w, &ch, &payload));
    bind!(on_notify_clear, |c, w| c.notify_clear(&w));
    bind!(on_data_import, |c, w| c.import_start(&w));
    bind!(on_imp_set_delimiter, |c, w, d| c.import_delimiter(&w, &d));
    bind!(on_imp_set_header, |c, w, on| c.import_change(&w, |s| {
        s.header = on;
        true
    }));
    bind!(on_imp_set_empty_null, |c, w, on| c.import_change(&w, |s| {
        s.empty_null = on;
        true
    }));
    bind!(on_imp_toggle, |c, w, row, on| c.import_change(&w, |s| {
        if let Some(e) = usize::try_from(row).ok().and_then(|i| s.enabled.get_mut(i)) {
            *e = on;
        }
        false
    }));
    bind!(on_imp_cancel, |c, w| c.import_cancel(&w));
    bind!(on_imp_run, |c, w| c.import_run(&w));
    bind!(on_mon_toggle_pause, |c, w| {
        w.set_mon_paused(!w.get_mon_paused());
        c.monitor_tick(&w);
    });
    bind!(on_mon_edit_endpoint, |c, w| c.monitor_edit_endpoint(&w));
    bind!(on_mon_retry, |c, w| c.monitor_retry(&w));
    bind!(on_mon_refresh_xacts, |c, w| c.monitor_xacts(&w));
    bind!(on_mon_xact, |c, w, row, action| {
        if let Ok(row) = usize::try_from(row) {
            c.monitor_xact(&w, row, &action);
        }
    });
    {
        let weak = window.as_weak();
        let handler = Rc::clone(&controller);
        controller.monitor_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(2),
            move || {
                if let Some(w) = weak.upgrade() {
                    handler.monitor_tick(&w);
                }
            },
        );
    }
    bind!(on_sec_refresh, |c, w| c.security_reload(&w));
    bind!(on_sec_revert, |c, w| c.security_revert(&w));
    bind!(on_sec_apply, |c, w| c.security_apply(&w));
    bind!(on_sec_select_role, |c, w, row| {
        if let Ok(row) = usize::try_from(row) {
            c.security_select_role(&w, row);
        }
    });
    bind!(on_sec_new_role, |c, w, name| c.security_new_role(&w, &name));
    bind!(on_sec_drop_role, |c, w| c.security_toggle_drop(&w));
    bind!(on_sec_flag, |c, w, flag, on| {
        if let Ok(flag) = usize::try_from(flag) {
            c.security_flag(&w, flag, on);
        }
    });
    bind!(on_sec_join, |c, w, role| c.security_membership(
        &w,
        &role,
        Some(false)
    ));
    bind!(on_sec_leave, |c, w, role| c
        .security_membership(&w, &role, None));
    bind!(on_sec_admin, |c, w, role, on| c.security_membership(
        &w,
        &role,
        Some(on)
    ));
    bind!(on_sec_grant_cycle, |c, w, row, col| {
        if let (Ok(row), Ok(col)) = (usize::try_from(row), usize::try_from(col)) {
            c.security_grant(&w, Some(row), Some(col));
        }
    });
    bind!(on_sec_grant_row, |c, w, row| {
        if let Ok(row) = usize::try_from(row) {
            c.security_grant(&w, Some(row), None);
        }
    });
    bind!(on_sec_grant_column, |c, w, col| {
        if let Ok(col) = usize::try_from(col) {
            c.security_grant(&w, None, Some(col));
        }
    });
    bind!(on_sec_select_table, |c, w, row| {
        if let Ok(row) = usize::try_from(row) {
            c.security_select_table(&w, row);
        }
    });
    bind!(on_sec_rls, |c, w, row, on| {
        if let Ok(row) = usize::try_from(row) {
            c.security_rls(&w, row, on);
        }
    });
    bind!(on_sec_add_policy, |c, w| c.security_add_policy(&w));
    bind!(on_sec_remove_policy, |c, w, row| {
        if let Ok(row) = usize::try_from(row) {
            c.security_remove_policy(&w, row);
        }
    });
    bind!(on_sec_policy_field, |c, w, row, field, value| {
        if let Ok(row) = usize::try_from(row) {
            c.security_policy_field(&w, row, &field, &value);
        }
    });
    bind!(on_design_index_field, |c, w, row, field, value| {
        if let Ok(row) = usize::try_from(row) {
            c.design_change(&w, false, |spec| {
                if let Some(index) = spec.indexes.get_mut(row) {
                    match field.as_str() {
                        "name" => index.name = value.to_string(),
                        "columns" => index.columns = value.to_string(),
                        "predicate" => index.predicate = value.to_string(),
                        _ => {},
                    }
                }
            });
        }
    });
    bind!(on_design_index_unique, |c, w, row, on| {
        if let Ok(row) = usize::try_from(row) {
            c.design_change(&w, false, |spec| {
                if let Some(index) = spec.indexes.get_mut(row) {
                    index.unique = on;
                }
            });
        }
    });
    bind!(on_design_add_index, |c, w| {
        c.design_change(&w, true, |spec| {
            let table = designer::ident(&spec.name);
            let n = spec.indexes.len() + 1;
            spec.indexes.push(designer::IndexSpec {
                name: if table.is_empty() {
                    format!("ix_{n}")
                } else {
                    format!("ix_{table}_{n}")
                },
                ..designer::IndexSpec::default()
            });
        });
    });
    bind!(on_design_remove_index, |c, w, row| {
        if let Ok(row) = usize::try_from(row) {
            c.design_change(&w, true, |spec| {
                if row < spec.indexes.len() {
                    spec.indexes.remove(row);
                }
            });
        }
    });
    bind!(on_design_reset, |c, w| {
        let original = c.design.borrow().as_ref().map(|d| d.original.clone());
        match original {
            Some(Some(before)) => {
                if let Some(state) = c.design.borrow_mut().as_mut() {
                    state.spec = before;
                }
                c.publish_design(&w, true);
            },
            Some(None) => c.design_new(&w, "public"),
            None => {},
        }
    });
    bind!(on_tab_select, |c, w, id| {
        if let Ok(id) = u32::try_from(id) {
            c.activate_tab(&w, id);
        }
    });
    // A negative id is the tab in front (⌘W / Ctrl+W in the editor).
    bind!(on_tab_close, |c, w, id| {
        let id = u32::try_from(id).unwrap_or_else(|_| c.active_tab.get());
        c.close_tab_checked(&w, id);
    });
    bind!(on_file_open, |c, w| c.file_open(&w));
    bind!(on_find_changed, |c, w, text, case| c
        .find_changed(&w, &text, case));
    bind!(on_find_step, |c, w, forward| c.find_step(&w, forward));
    bind!(on_replace_one, |c, w| c.replace_one(&w));
    bind!(on_replace_all, |c, w| c.replace_all(&w));
    bind!(on_format_sql, |c, w| c.format_script(&w));
    bind!(on_file_save, |c, w| c.file_save(&w, false));
    bind!(on_file_save_as, |c, w| c.file_save(&w, true));
    bind!(on_copy_row, |c, w| c.copy_query_row(&w));
    bind!(on_copy_all, |c, w| c.copy_query_all(&w));
    bind!(on_export_result, |c, w| c.export_query(&w));
    bind!(on_data_copy_row, |c, w| c.data_copy_row(&w));
    bind!(on_data_export, |c, w| c.data_export(&w));
    bind!(on_confirm_cancel, |c, w| c.confirm(&w, false));
    bind!(on_data_cell_apply, |c, w, text| c
        .data_cell_set(&w, edits::Value::Text(text.to_string())));
    bind!(on_data_cell_commit, |c, w, row, col, text| {
        if let (Ok(row), Ok(col)) = (usize::try_from(row), usize::try_from(col)) {
            c.data_cell_commit(&w, row, col, text.as_str());
        }
    });
    bind!(on_data_cell_set_null, |c, w| c
        .data_cell_set(&w, edits::Value::Null));
    bind!(on_data_cell_default, |c, w| c
        .data_cell_set(&w, edits::Value::Default));
    bind!(on_data_cell_revert, |c, w| c.data_cell_revert(&w));
    bind!(on_data_open_in_query, |c, w| c.data_open_in_query(&w));

    bind!(on_edit_server, |c, w, id| {
        if let Ok(id) = u32::try_from(id) {
            c.edit_server(&w, id);
        }
    });
    bind!(on_remove_server, |c, w, id| {
        if let Ok(id) = u32::try_from(id) {
            c.remove(&w, id);
        }
    });
    bind!(on_connect_server, |c, w, id| {
        if let Ok(id) = u32::try_from(id) {
            c.connect(&w, id);
        }
    });
    bind!(on_disconnect_server, |c, w, id| {
        if let Ok(id) = u32::try_from(id) {
            c.disconnect_checked(&w, id);
        }
    });
    bind!(on_save_server, |c, w| c.save(&w));
    bind!(on_test_connection, |c, w| c.test(&w));
    bind!(on_reset_form, |c, w| c.reset_form(&w));
    bind!(on_probe_finished, |c, w, state, message| {
        let _ = c;
        w.set_test_state(state);
        w.set_test_message(message);
    });

    // Editing any field invalidates the last test result: it was measured against different
    // parameters, and leaving it on screen would vouch for something never tried.
    let weak = window.as_weak();
    window.on_draft_changed(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        clear_feedback(&window);
    });
}

// ── Pure helpers ────────────────────────────────────────────────────────────

fn load_history(path: &std::path::Path) -> Vec<HistoryEntry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_history(path: &std::path::Path, history: &[HistoryEntry]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(history).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `then` relative to `now`: "just now", "5 min ago", "3 h ago", "2 days ago", or a date.
#[must_use]
pub fn ago(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        86_400..604_800 => {
            let days = secs / 86_400;
            if days == 1 {
                "yesterday".to_owned()
            } else {
                format!("{days} days ago")
            }
        },
        _ => civil_date(then),
    }
}

/// `YYYY-MM-DD` (UTC) for Unix seconds — Howard Hinnant's days-to-civil algorithm.
fn civil_date(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0) + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The next ordering after a click on `column`: ascending → descending → none.
fn next_sort(current: Option<(usize, bool)>, column: usize) -> Option<(usize, bool)> {
    match current {
        Some((c, false)) if c == column => Some((column, true)),
        Some((c, true)) if c == column => None,
        _ => Some((column, false)),
    }
}

/// `result` with its rows ordered by `sort`; NULLs last, numbers compared as numbers.
fn sorted(result: &ResultSet, sort: Option<(usize, bool)>) -> ResultSet {
    let Some((column, descending)) = sort else {
        return result.clone();
    };
    let numeric = result.types.get(column).is_some_and(|k| is_numeric(k));
    let mut out = result.clone();
    out.rows.sort_by(|a, b| {
        let (x, y) = (
            a.get(column).cloned().flatten(),
            b.get(column).cloned().flatten(),
        );
        let order = match (&x, &y) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Greater,
            (Some(_), None) => return std::cmp::Ordering::Less,
            (Some(x), Some(y)) if numeric => match (x.parse::<f64>(), y.parse::<f64>()) {
                (Ok(x), Ok(y)) => x.total_cmp(&y),
                _ => x.cmp(y),
            },
            (Some(x), Some(y)) => x.cmp(y),
        };
        if descending { order.reverse() } else { order }
    });
    out
}

/// An EXPLAIN result as tree rows. The server indents each operator two spaces under its
/// parent and writes `Operator: detail` or `Operator (detail)`.
fn plan_rows(statement: &str, lines: &[String]) -> Vec<PlanRow> {
    let mut rows = vec![PlanRow {
        depth: 0,
        op: format!("-- {statement}").into(),
        detail: SharedString::new(),
        heading: true,
    }];
    for line in lines {
        let text = line.trim_start();
        if text.is_empty() {
            continue;
        }
        let depth = i32::try_from((line.len() - text.len()) / 2).unwrap_or(0);
        let (op, detail) = if let Some((op, detail)) = text.split_once(": ") {
            (op, detail)
        } else if let Some(at) = text.find(" (") {
            (&text[..at], &text[at + 1..])
        } else {
            (text, "")
        };
        rows.push(PlanRow {
            depth,
            op: op.into(),
            detail: detail.into(),
            heading: false,
        });
    }
    rows
}

/// Make `model` hold `rows`, touching only what changed, so unchanged rows keep their
/// on-screen elements (and with them hover state and half-finished double-clicks).
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() == rows.len() {
        for (i, row) in rows.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        }
    } else {
        model.set_vec(rows);
    }
}

/// Flatten a profile into the display-ready row the Servers screen renders.
fn server_row(profile: &ServerProfile) -> ServerRow {
    let last_used = profile.last_used.map_or_else(
        || "Never connected".to_owned(),
        |when| format!("Last connected {}", ago(unix_now(), when)),
    );
    ServerRow {
        last_used: last_used.into(),
        id: id_for_ui(profile.id),
        name: profile.name.as_str().into(),
        endpoint: profile.params.endpoint().into(),
        detail: profile.params.detail().into(),
        env: profile.environment.as_tag().into(),
        status: profile.status.as_tag().into(),
        tls: profile.params.tls,
    }
}

fn property(label: &str, value: &str, mono: bool) -> Property {
    noted(label, value, "", mono)
}

/// A property with a second, smaller line under it.
fn noted(label: &str, value: &str, note: &str, mono: bool) -> Property {
    Property {
        label: label.into(),
        value: value.into(),
        note: note.into(),
        mono,
    }
}

/// A first query for an object, ready to run or edit.
fn starter_for(kind: ObjectKind, schema: &str, name: &str) -> String {
    let qualified = format!("{}.{}", quote_ident(schema), quote_ident(name));
    match kind {
        ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
            format!("SELECT *\nFROM {qualified}\nLIMIT 100;\n")
        },
        // nextval is the only way to read a sequence here, and it consumes a value — say so.
        ObjectKind::Sequence => format!(
            "-- nextval() takes the next value: running this consumes one.\nSELECT nextval('{}');\n",
            name.replace('\'', "''")
        ),
        ObjectKind::Function => format!("SELECT {qualified}(/* arguments */);\n"),
    }
}

/// The server's message, with what to do about the errors a user can act on.
fn explain(code: Option<&str>, message: &str) -> String {
    match code {
        // NusaDB's concurrency control is optimistic: a conflicting writer fails instead of
        // waiting (docs/sql-reference.md, "Serialization conflicts").
        Some("40001") => format!(
            "{message}. Another session changed the same rows at the same moment. Reload and try again."
        ),
        Some("42501") => format!("{message}. Your role lacks that privilege."),
        Some("23505") => format!("{message}. A row with that value already exists."),
        Some("23503") => {
            format!("{message}. It is referenced by, or refers to, a row that does not match.")
        },
        Some("23502") => format!("{message}. That column needs a value."),
        _ => message.to_owned(),
    }
}

/// What the inspector shows for one described object.
struct Described {
    properties: Vec<Property>,
    sections: Vec<InspectorSection>,
    definition: String,
    comment: String,
    notes: Vec<SharedString>,
}

/// Whether an index exists because of a constraint (the key, a UNIQUE constraint, or the index
/// the engine builds for a foreign key) rather than being created on purpose.
fn index_backs_constraint(name: &str, info: &ObjectInfo) -> bool {
    info.constraints
        .iter()
        .any(|c| c.name == name || (c.kind == "FOREIGN KEY" && name.starts_with(&c.name)))
}

fn section(heading: &str, items: Vec<Property>, emphasise: bool, label_w: f32) -> InspectorSection {
    InspectorSection {
        heading: heading.into(),
        items: ModelRc::new(VecModel::from(items)),
        emphasise,
        label_w,
    }
}

/// Lay out a described object for the inspector.
///
/// Each column carries what a reader needs to use it — its type, and whether it is the key,
/// required, unique, a reference elsewhere, defaulted or commented — so the sections below it
/// are there to confirm, not to be cross-read.
fn describe_object(kind: ObjectKind, info: &ObjectInfo) -> Described {
    let mut properties = Vec::new();
    if let Some(rows) = info.row_count {
        properties.push(property(
            "Rows",
            &group_thousands(usize::try_from(rows).unwrap_or(usize::MAX)),
            false,
        ));
    }
    for (label, value) in &info.details {
        properties.push(property(label, value, false));
    }

    // A glance, not the reference: name and type of the first columns. Keys, defaults,
    // comments and everything else are on the object page (Details).
    const SHOWN: usize = 12;
    let in_pk = |name: &str| {
        info.constraints
            .iter()
            .any(|c| c.kind == "PRIMARY KEY" && c.columns.iter().any(|x| x == name))
    };
    let mut columns: Vec<Property> = info
        .columns
        .iter()
        .take(SHOWN)
        .map(|def| {
            let label = if in_pk(&def.name) {
                format!("{} \u{2022}", def.name)
            } else {
                def.name.clone()
            };
            property(&label, short_type(&def.type_spec), true)
        })
        .collect();
    if info.columns.len() > SHOWN {
        columns.push(noted(
            &format!("+ {} more", info.columns.len() - SHOWN),
            "",
            "open Details for all of them",
            false,
        ));
    }

    let counts: Vec<Property> = page_view::structure_counts(kind, info)
        .into_iter()
        .map(|(label, n)| property(label, &n.to_string(), false))
        .collect();

    let mut sections = Vec::new();
    if !columns.is_empty() {
        let heading = format!("Columns ({})", info.columns.len());
        sections.push(section(&heading, columns, true, 120.0));
    }
    if !counts.is_empty() {
        sections.push(section("Structure", counts, false, 120.0));
    }
    Described {
        properties,
        sections,
        // Definitions can run long; they are read on the object page.
        definition: String::new(),
        comment: info.comment.clone().unwrap_or_default(),
        notes: info.notes.iter().map(|n| n.as_str().into()).collect(),
    }
}

/// One "Tables: 3" line per kind of object present (tables and views always shown).
fn push_counts(properties: &mut Vec<Property>, objects: &[DbObject]) {
    for kind in ObjectKind::ALL {
        let n = objects.iter().filter(|o| o.kind == kind).count();
        if n > 0 || !matches!(kind, ObjectKind::Sequence | ObjectKind::Function) {
            properties.push(property(kind.plural(), &n.to_string(), false));
        }
    }
}

/// The compact spelling of a type name, as `psql`'s `\d` would print it.
fn short_type(name: &str) -> &str {
    match name {
        "integer" => "int",
        "bigint" => "int8",
        "smallint" => "int2",
        "boolean" => "bool",
        "double precision" => "float8",
        "real" => "float4",
        "character varying" => "varchar",
        "character" => "char",
        "timestamp with time zone" => "timestamptz",
        "timestamp without time zone" => "timestamp",
        "time with time zone" => "timetz",
        "time without time zone" => "time",
        other => other,
    }
}

const fn env_label(env: Environment) -> &'static str {
    match env {
        Environment::Prod => "production",
        Environment::Staging => "staging",
        Environment::Dev => "development",
    }
}

const fn status_label(status: ConnectionStatus) -> &'static str {
    match status {
        ConnectionStatus::Connected => "connected",
        ConnectionStatus::Connecting => "connecting…",
        ConnectionStatus::Error => "could not connect",
        ConnectionStatus::Disconnected => "not connected",
    }
}

/// The first line of a statement that is not a comment, shortened for one line of UI.
fn first_line(sql: &str) -> &str {
    let line = sql
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or("");
    match line.char_indices().nth(120) {
        Some((cut, _)) => &line[..cut],
        None => line,
    }
}

/// Whether a statement changes what the navigator shows.
fn changes_schema(sql: &str) -> bool {
    let body: String = sql
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join(" ");
    let word = body
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(word.as_str(), "CREATE" | "DROP" | "ALTER")
}

/// The Messages tab: one line per statement that ran.
fn messages(report: &RunReport) -> Vec<MessageRow> {
    report
        .statements
        .iter()
        .map(|s| match &s.outcome {
            StatementOutcome::Done { tag, result } => MessageRow {
                ok: true,
                title: if tag.is_empty() {
                    format!("{} returned", rows_label(result.rows.len()))
                } else {
                    tag.clone()
                }
                .into(),
                detail: first_line(&s.sql).into(),
                time: format_elapsed(s.elapsed).into(),
            },
            StatementOutcome::Failed { code, message } => MessageRow {
                ok: false,
                title: match code {
                    Some(code) => format!("{message}  [{code}]"),
                    None => message.clone(),
                }
                .into(),
                detail: first_line(&s.sql).into(),
                time: format_elapsed(s.elapsed).into(),
            },
        })
        .collect()
}

/// Types the grid right-aligns.
fn is_numeric(kind: &str) -> bool {
    matches!(kind, "int" | "float" | "numeric")
}

/// Column headers sized from their content, the total width, and the rows as cells.
fn grid(
    result: &ResultSet,
    limit: usize,
    sort: Option<(usize, bool)>,
    user_widths: &HashMap<String, f32>,
) -> (Vec<GridColumn>, f32, Vec<GridRow>) {
    let sample = result.rows.iter().take(200);
    let mut widths: Vec<usize> = result
        .columns
        .iter()
        .zip(result.types.iter().chain(std::iter::repeat(&String::new())))
        .map(|(name, kind)| name.chars().count() + kind.chars().count() + 2)
        .collect();
    for row in sample {
        for (i, value) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                let len = value.as_ref().map_or(4, |v| v.chars().count());
                *w = (*w).max(len);
            }
        }
    }
    let columns: Vec<GridColumn> = result
        .columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let kind = result.types.get(i).cloned().unwrap_or_default();
            let chars = widths.get(i).copied().unwrap_or(8);
            #[allow(clippy::cast_precision_loss)]
            let px = user_widths
                .get(name)
                .copied()
                .unwrap_or_else(|| (chars as f32).mul_add(7.4, 24.0).clamp(72.0, 380.0));
            GridColumn {
                name: name.as_str().into(),
                sort: match sort {
                    Some((c, false)) if c == i => "asc",
                    Some((c, true)) if c == i => "desc",
                    _ => "",
                }
                .into(),
                numeric: is_numeric(&kind),
                kind: kind.into(),
                width: px,
                offset: 0.0,
            }
        })
        .collect();
    let mut columns = columns;
    let mut running = 0.0;
    for column in &mut columns {
        column.offset = running;
        running += column.width;
    }
    let total = running;
    // Vectors are shown cut short ("[0.12, 0.5, …] 768d") and edited in the value panel.
    let vectors: Vec<bool> = result.types.iter().map(|t| t == "vector").collect();
    let rows = result
        .rows
        .iter()
        .take(limit)
        .map(|row| GridRow {
            cells: ModelRc::new(VecModel::from(
                row.iter()
                    .enumerate()
                    .map(|(i, value)| {
                        let vector = vectors.get(i).copied().unwrap_or(false);
                        GridCell {
                            // One line per cell; a newline inside a value would break the row.
                            text: match value.as_deref() {
                                Some(v) if vector => crate::vector::compact(v).into(),
                                Some(v) => v.replace(['\n', '\r'], " ").into(),
                                None => SharedString::new(),
                            },
                            null: value.is_none(),
                            state: SharedString::new(),
                            // A cut-short vector is edited in the value panel, whole.
                            long: value.as_deref().is_some_and(|v| {
                                v.contains(['\n', '\r'])
                                    || (vector && crate::vector::compact(v) != v)
                            }),
                        }
                    })
                    .collect::<Vec<_>>(),
            )),
        })
        .collect();
    (columns, total, rows)
}

/// The result as a JSON array of objects, numbers kept as numbers.
fn to_json(result: &ResultSet) -> String {
    let rows: Vec<serde_json::Value> = result
        .rows
        .iter()
        .take(crate::driver::row_cap())
        .map(|row| {
            let object: serde_json::Map<String, serde_json::Value> = result
                .columns
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let kind = result.types.get(i).map_or("", String::as_str);
                    (
                        name.clone(),
                        export::json_value(row.get(i).cloned().flatten(), kind),
                    )
                })
                .collect();
            serde_json::Value::Object(object)
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_default()
}

/// Push the colour layers and line numbers for `sql` into the editor.
fn publish_editor(window: &AppWindow, sql: &str) {
    let layers: Vec<SqlLayer> = sql_layers(sql)
        .into_iter()
        .map(|(kind, text)| SqlLayer {
            kind: kind.into(),
            text: text.into(),
        })
        .collect();
    window.set_sql_layers(ModelRc::new(VecModel::from(layers)));
    let count = sql.split('\n').count().max(1);
    let numbers: Vec<String> = (1..=count).map(|n| n.to_string()).collect();
    window.set_sql_line_numbers(numbers.join("\n").into());
}

/// Split editor text into one layer per token kind. In each layer, characters of other kinds
/// become spaces (tabs are kept as tabs), so with a monospaced face every layer lines up with
/// the source character for character.
fn sql_layers(sql: &str) -> Vec<(&'static str, String)> {
    // Each line as (character, kind) pairs.
    let lines: Vec<Vec<(char, &'static str)>> = sql
        .split('\n')
        .map(|line| {
            highlight_line(line.trim_end_matches('\r'))
                .into_iter()
                .flat_map(|token| {
                    let kind = token.kind.as_tag();
                    token
                        .text
                        .chars()
                        .map(move |c| (c, kind))
                        .collect::<Vec<_>>()
                })
                .collect()
        })
        .collect();

    let mut kinds: Vec<&'static str> = Vec::new();
    for &(_, kind) in lines.iter().flatten() {
        if kind != "space" && !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }

    kinds
        .into_iter()
        .map(|kind| {
            let text = lines
                .iter()
                .map(|line| {
                    let mut out: String = line
                        .iter()
                        .map(|&(c, k)| if k == kind || c == '\t' { c } else { ' ' })
                        .collect();
                    out.truncate(out.trim_end().len());
                    out
                })
                .collect::<Vec<_>>()
                .join("\n");
            (kind, text)
        })
        .collect()
}

/// Narrow a store identity to the `int` Slint uses.
fn id_for_ui(id: u32) -> i32 {
    i32::try_from(id).unwrap_or(NO_SELECTION)
}

fn read_draft(window: &AppWindow) -> ServerDraft {
    ServerDraft {
        name: window.get_draft_name().to_string(),
        host: window.get_draft_host().to_string(),
        port: window.get_draft_port().to_string(),
        user: window.get_draft_user().to_string(),
        password: window.get_draft_password().to_string(),
        database: window.get_draft_database().to_string(),
        tls: window.get_draft_tls(),
        ca_path: window.get_draft_ca_path().to_string(),
        environment: Environment::from_tag(window.get_draft_env().as_str()),
        metrics: window.get_draft_metrics().to_string(),
        remember: window.get_draft_remember(),
    }
}

fn write_draft(window: &AppWindow, draft: &ServerDraft) {
    window.set_draft_name(draft.name.as_str().into());
    window.set_draft_host(draft.host.as_str().into());
    window.set_draft_port(draft.port.as_str().into());
    window.set_draft_user(draft.user.as_str().into());
    window.set_draft_password(draft.password.as_str().into());
    window.set_draft_database(draft.database.as_str().into());
    window.set_draft_env(draft.environment.as_tag().into());
    window.set_draft_tls(draft.tls);
    window.set_draft_ca_path(draft.ca_path.as_str().into());
    window.set_draft_metrics(draft.metrics.as_str().into());
    window.set_draft_remember(draft.remember);
}

fn clear_feedback(window: &AppWindow) {
    window.set_form_error(SharedString::new());
    clear_result(window);
}

fn clear_result(window: &AppWindow) {
    window.set_test_state("idle".into());
    window.set_test_message(SharedString::new());
}

fn render_outcome(outcome: &ProbeOutcome) -> (&'static str, String) {
    match outcome {
        ProbeOutcome::Reachable { detail } => ("ok", detail.clone()),
        ProbeOutcome::Failed { message } => ("error", message.clone()),
        ProbeOutcome::Unavailable { reason } => ("info", reason.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_layer_lines_up_with_the_source_column_for_column() {
        let source = "SELECT 'é', 1 -- note\n\tFROM t;";
        let layers = sql_layers(source);
        let source_lines: Vec<Vec<char>> =
            source.split('\n').map(|l| l.chars().collect()).collect();
        for (_, text) in &layers {
            for (layer_line, src) in text.split('\n').zip(&source_lines) {
                for (i, c) in layer_line.chars().enumerate() {
                    assert!(
                        c == ' ' || c == src[i],
                        "layer char {c:?} at {i} is not the source's {:?}",
                        src[i]
                    );
                }
            }
            assert_eq!(text.split('\n').count(), 2);
        }
        let kinds: Vec<&str> = layers.iter().map(|(k, _)| *k).collect();
        assert!(
            kinds.contains(&"keyword") && kinds.contains(&"string") && kinds.contains(&"comment")
        );
    }

    #[test]
    fn overlaying_the_layers_rebuilds_the_source() {
        let source = "SELECT a, 'x y' FROM t WHERE n = 2;";
        let mut rebuilt: Vec<char> = vec![' '; source.chars().count()];
        for (_, text) in sql_layers(source) {
            for (i, c) in text.chars().enumerate() {
                if c != ' ' {
                    rebuilt[i] = c;
                }
            }
        }
        assert_eq!(rebuilt.into_iter().collect::<String>(), source);
    }

    #[test]
    fn the_starter_buffer_holds_one_runnable_statement() {
        assert_eq!(split_statements(&starter_sql()).len(), 1);
    }

    #[test]
    fn a_statement_is_labelled_by_its_first_line_of_code() {
        assert_eq!(first_line("-- why\n  SELECT 1\n  FROM t"), "SELECT 1");
    }

    #[test]
    fn schema_changes_are_recognised_past_leading_comments() {
        assert!(changes_schema("-- make it\ncreate table t (id int)"));
        assert!(changes_schema("DROP VIEW v"));
        assert!(!changes_schema("SELECT 'create'"));
    }

    #[test]
    fn grid_columns_are_sized_and_numbers_right_aligned() {
        let result = ResultSet {
            columns: vec!["id".into(), "name".into()],
            types: vec!["int".into(), "text".into()],
            rows: vec![vec![Some("1".into()), None]],
        };
        let (columns, total, rows) = grid(&result, 10, Some((0, true)), &HashMap::new());
        assert_eq!(columns[0].sort, "desc");
        assert!(columns[0].numeric && !columns[1].numeric);
        assert!(columns.iter().all(|c| c.width >= 72.0));
        assert!((total - columns.iter().map(|c| c.width).sum::<f32>()).abs() < f32::EPSILON);
        assert!(rows[0].cells.row_data(1).is_some_and(|c| c.null));
    }

    #[test]
    fn sorting_puts_nulls_last_and_compares_numbers_as_numbers() {
        let result = ResultSet {
            columns: vec!["n".into()],
            types: vec!["int".into()],
            rows: vec![vec![Some("10".into())], vec![None], vec![Some("9".into())]],
        };
        let up = sorted(&result, Some((0, false)));
        assert_eq!(
            up.rows,
            vec![vec![Some("9".into())], vec![Some("10".into())], vec![None]]
        );
        let down = sorted(&result, Some((0, true)));
        assert_eq!(down.rows[0], vec![Some("10".into())]);
        assert_eq!(next_sort(None, 2), Some((2, false)));
        assert_eq!(next_sort(Some((2, false)), 2), Some((2, true)));
        assert_eq!(next_sort(Some((2, true)), 2), None);
        assert_eq!(next_sort(Some((1, true)), 2), Some((2, false)));
    }

    #[test]
    fn plans_become_an_indented_tree() {
        let lines = vec![
            "Project (4 column(s))".to_owned(),
            "  Filter".to_owned(),
            "    IndexScan: customers using customers_pkey".to_owned(),
        ];
        let rows = plan_rows("SELECT 1", &lines);
        assert!(rows[0].heading);
        assert_eq!(
            (rows[1].depth, rows[1].op.as_str(), rows[1].detail.as_str()),
            (0, "Project", "(4 column(s))")
        );
        assert_eq!((rows[3].depth, rows[3].op.as_str()), (2, "IndexScan"));
        assert_eq!(rows[3].detail, "customers using customers_pkey");
    }

    #[test]
    fn times_read_relative_then_as_dates() {
        assert_eq!(ago(1_000, 990), "just now");
        assert_eq!(ago(10_000, 10_000 - 300), "5 min ago");
        assert_eq!(ago(100_000, 100_000 - 86_400), "yesterday");
        assert_eq!(civil_date(1_764_028_800), "2025-11-25");
        assert_eq!(
            ago(1_764_028_800 + 30 * 86_400, 1_764_028_800),
            "2025-11-25"
        );
    }

    #[test]
    fn json_keeps_numbers_booleans_and_nulls_typed() {
        let result = ResultSet {
            columns: vec!["n".into(), "b".into(), "t".into()],
            types: vec!["int".into(), "bool".into(), "text".into()],
            rows: vec![vec![Some("42".into()), Some("true".into()), None]],
        };
        let json: serde_json::Value = serde_json::from_str(&to_json(&result)).unwrap();
        assert_eq!(json[0]["n"], 42);
        assert_eq!(json[0]["b"], true);
        assert!(json[0]["t"].is_null());
    }
}

/// Whether a confirmation reason is a SQL statement (shown as code) rather than a sentence.
fn is_statement(reason: &str) -> bool {
    const VERBS: [&str; 12] = [
        "DROP", "TRUNCATE", "DELETE", "UPDATE", "ALTER", "CREATE", "GRANT", "REVOKE", "COMMIT",
        "ROLLBACK", "INSERT", "REFRESH",
    ];
    let first = reason.split_whitespace().next().unwrap_or_default();
    VERBS.contains(&first)
        && !reason.contains(": ")
        && !reason.ends_with('.')
        && !reason.contains(" on a production server")
}

#[cfg(test)]
mod confirm_tests {
    use super::is_statement;

    #[test]
    fn statements_and_sentences() {
        assert!(is_statement("DROP SCHEMA \"x\" CASCADE"));
        assert!(is_statement("COMMIT PREPARED 'g1'"));
        assert!(!is_statement("TRUNCATE zz: deletes every row"));
        assert!(!is_statement(
            "The changes since it was last saved will be lost."
        ));
        assert!(!is_statement("DROP TABLE t on a production server"));
    }
}

/// Tab label of result set `index`: the table it reads,
/// numbered only when the same name comes up more than once.
fn part_label(index: usize, sql: &str, parts: &[Part]) -> String {
    let name = |sql: &str| {
        crate::result_source::single_table(sql)
            .ok()
            .map(|s| s.table)
    };
    let Some(table) = name(sql) else {
        return format!("Result {}", index + 1);
    };
    let same: Vec<usize> = parts
        .iter()
        .enumerate()
        .filter(|(_, p)| name(&p.sql).as_deref() == Some(table.as_str()))
        .map(|(i, _)| i)
        .collect();
    if same.len() > 1 {
        let nth = same.iter().position(|i| *i == index).unwrap_or(0) + 1;
        format!("{table} ({nth})")
    } else {
        table
    }
}
