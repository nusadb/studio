//! The object tree, flattened into the rows the navigator draws.
//!
//! The hierarchy is produced as a flat list rather than as nested nodes: the view renders one
//! `for` loop, and selection, scrolling, and keyboard traversal stay ordinary list operations.
//! Depth is carried on each row, which is all the view needs to indent it.
//!
//! Levels: server → database → schema → Tables / Views / Sequences / Functions → object. Everything below a
//! server comes from introspecting a live session and is loaded lazily, the first time its
//! parent is opened; until it arrives the parent shows a "Loading…" row, and a failure shows
//! the server's message in place of children — never invented placeholders.

use std::collections::{HashMap, HashSet};

use crate::backend::{Catalog, ObjectKind};
use crate::model::ServerProfile;

/// Separates the parts of a node id. Not a character that appears in SQL names in practice,
/// and never one a user types into a search box.
const SEP: char = '\u{1f}';

/// One visible line of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub label: String,
    /// Picks the icon and its colour in the view.
    pub kind: &'static str,
    /// Indent level; servers are 0.
    pub depth: i32,
    pub expandable: bool,
    pub expanded: bool,
    /// Connection status tag, for server rows only.
    pub status: &'static str,
    pub badge: String,
    pub dimmed: bool,
}

/// What a tree id points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Server(u32),
    Database {
        server: u32,
        database: String,
    },
    Schema {
        server: u32,
        database: String,
        schema: String,
    },
    Group {
        server: u32,
        database: String,
        schema: String,
        kind: ObjectKind,
    },
    Object {
        server: u32,
        database: String,
        schema: String,
        name: String,
        kind: ObjectKind,
    },
}

impl Node {
    /// The stable id of this node.
    #[must_use]
    pub fn id(&self) -> String {
        let parts: Vec<String> = match self {
            Self::Server(s) => vec!["srv".into(), s.to_string()],
            Self::Database { server, database } => {
                vec!["db".into(), server.to_string(), database.clone()]
            },
            Self::Schema {
                server,
                database,
                schema,
            } => {
                vec![
                    "sc".into(),
                    server.to_string(),
                    database.clone(),
                    schema.clone(),
                ]
            },
            Self::Group {
                server,
                database,
                schema,
                kind,
            } => vec![
                "grp".into(),
                server.to_string(),
                database.clone(),
                schema.clone(),
                kind_tag(*kind).into(),
            ],
            Self::Object {
                server,
                database,
                schema,
                name,
                kind,
            } => vec![
                "obj".into(),
                server.to_string(),
                database.clone(),
                schema.clone(),
                kind_tag(*kind).into(),
                name.clone(),
            ],
        };
        parts.join(&SEP.to_string())
    }

    /// Parse an id produced by [`Node::id`].
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        let parts: Vec<&str> = id.split(SEP).collect();
        let server = parts.get(1)?.parse().ok()?;
        let s = |i: usize| parts.get(i).map(|p| (*p).to_owned());
        Some(match *parts.first()? {
            "srv" => Self::Server(server),
            "db" => Self::Database {
                server,
                database: s(2)?,
            },
            "sc" => Self::Schema {
                server,
                database: s(2)?,
                schema: s(3)?,
            },
            "grp" => Self::Group {
                server,
                database: s(2)?,
                schema: s(3)?,
                kind: kind_from_tag(parts.get(4)?)?,
            },
            "obj" => Self::Object {
                server,
                database: s(2)?,
                schema: s(3)?,
                kind: kind_from_tag(parts.get(4)?)?,
                name: s(5)?,
            },
            _ => return None,
        })
    }

    /// The server this node lives on.
    #[must_use]
    pub const fn server(&self) -> u32 {
        match self {
            Self::Server(server)
            | Self::Database { server, .. }
            | Self::Schema { server, .. }
            | Self::Group { server, .. }
            | Self::Object { server, .. } => *server,
        }
    }

    /// The database this node lives in, if it is below the server level.
    #[must_use]
    pub fn database(&self) -> Option<&str> {
        match self {
            Self::Server(_) => None,
            Self::Database { database, .. }
            | Self::Schema { database, .. }
            | Self::Group { database, .. }
            | Self::Object { database, .. } => Some(database),
        }
    }
}

/// The tag a kind is known by in ids and in the view (it also names the icon).
#[must_use]
pub const fn kind_tag(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "matview",
        ObjectKind::Sequence => "sequence",
        ObjectKind::Function => "function",
    }
}

fn kind_from_tag(tag: &str) -> Option<ObjectKind> {
    ObjectKind::ALL.into_iter().find(|k| kind_tag(*k) == tag)
}

/// State of something fetched from a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// Which nodes are open, and what has been loaded below them.
#[derive(Debug, Default)]
pub struct TreeState {
    expanded: HashSet<String>,
    databases: HashMap<u32, Load<Vec<String>>>,
    catalogs: HashMap<(u32, String), Load<Catalog>>,
}

/// What the controller must fetch after a node was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    Databases(u32),
    Catalog { server: u32, database: String },
}

impl TreeState {
    /// An all-collapsed, empty tree.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open or close `id`. Returns the fetch an opening needs, if its children are not loaded.
    pub fn toggle(&mut self, id: &str) -> Option<Fetch> {
        if self.expanded.remove(id) {
            return None;
        }
        self.expanded.insert(id.to_owned());
        self.fetch_for(id)
    }

    /// Open `id` (no-op if open). Returns the fetch it needs, like [`toggle`](Self::toggle).
    pub fn expand(&mut self, id: &str) -> Option<Fetch> {
        self.expanded.insert(id.to_owned());
        self.fetch_for(id)
    }

    #[must_use]
    pub fn is_expanded(&self, id: &str) -> bool {
        self.expanded.contains(id)
    }

    fn fetch_for(&mut self, id: &str) -> Option<Fetch> {
        match Node::parse(id)? {
            Node::Server(server) if !self.databases.contains_key(&server) => {
                self.databases.insert(server, Load::Loading);
                Some(Fetch::Databases(server))
            },
            Node::Database { server, database }
                if !self.catalogs.contains_key(&(server, database.clone())) =>
            {
                self.catalogs
                    .insert((server, database.clone()), Load::Loading);
                Some(Fetch::Catalog { server, database })
            },
            _ => None,
        }
    }

    /// Close every node.
    pub fn collapse_all(&mut self) {
        self.expanded.clear();
    }

    /// Record the database list of `server`.
    pub fn set_databases(&mut self, server: u32, result: Result<Vec<String>, String>) {
        self.databases.insert(server, into_load(result));
    }

    /// Record the catalog of one database.
    pub fn set_catalog(&mut self, server: u32, database: &str, result: Result<Catalog, String>) {
        self.catalogs
            .insert((server, database.to_owned()), into_load(result));
    }

    /// The loaded catalog of a database, if any.
    #[must_use]
    pub fn catalog(&self, server: u32, database: &str) -> Option<&Catalog> {
        match self.catalogs.get(&(server, database.to_owned())) {
            Some(Load::Ready(catalog)) => Some(catalog),
            _ => None,
        }
    }

    /// Why loading the databases of `server` failed, if it did.
    #[must_use]
    pub fn databases_error(&self, server: u32) -> Option<&str> {
        match self.databases.get(&server) {
            Some(Load::Failed(message)) => Some(message),
            _ => None,
        }
    }

    /// The loaded databases of a server, if any.
    #[must_use]
    pub fn databases(&self, server: u32) -> Option<&[String]> {
        match self.databases.get(&server) {
            Some(Load::Ready(list)) => Some(list),
            _ => None,
        }
    }

    /// Forget everything loaded for `server` (it disconnected or its address changed), and
    /// close its subtree.
    pub fn forget_server(&mut self, server: u32) {
        self.databases.remove(&server);
        self.catalogs.retain(|(s, _), _| *s != server);
        self.expanded
            .retain(|id| Node::parse(id).is_none_or(|n| n.server() != server));
    }

    /// Forget the catalog of one database, so the next opening reloads it.
    pub fn forget_catalog(&mut self, server: u32, database: &str) {
        self.catalogs.remove(&(server, database.to_owned()));
    }

    /// Drop what is loaded and return the fetches that re-load every open node.
    pub fn refresh(&mut self) -> Vec<Fetch> {
        self.databases.clear();
        self.catalogs.clear();
        let mut open: Vec<String> = self.expanded.iter().cloned().collect();
        open.sort();
        open.iter().filter_map(|id| self.fetch_for(id)).collect()
    }

    /// Flatten `servers` into the visible rows, in order. A non-empty `filter` keeps the rows
    /// whose label contains it (case-insensitive), plus the ancestors that lead to them, and
    /// opens those ancestors so the hits are visible.
    pub fn rows<'a>(
        &self,
        servers: impl Iterator<Item = &'a ServerProfile>,
        filter: &str,
    ) -> Vec<Row> {
        let needle = filter.trim().to_lowercase();
        let mut rows = Vec::new();
        for server in servers {
            let node = Node::Server(server.id);
            let mut row = self.row(&node, server.name.clone(), "server", 0, true);
            row.status = server.status.as_tag();
            let mut children = Vec::new();
            let child_hit = self.server_children(server.id, &needle, &mut children);
            emit(&mut rows, row, children, &needle, child_hit);
        }
        rows
    }

    fn row(
        &self,
        node: &Node,
        label: String,
        kind: &'static str,
        depth: i32,
        expandable: bool,
    ) -> Row {
        let id = node.id();
        Row {
            expanded: expandable && self.expanded.contains(&id),
            id,
            label,
            kind,
            depth,
            expandable,
            status: "",
            badge: String::new(),
            dimmed: false,
        }
    }

    /// Children of a server; returns whether any of them (or below) matched the filter.
    fn server_children(&self, server: u32, needle: &str, out: &mut Vec<Row>) -> bool {
        let open = self.expanded.contains(&Node::Server(server).id());
        let Some(load) = self.databases.get(&server) else {
            return false;
        };
        match load {
            Load::Loading if open => out.push(hint(server, "load", "Loading databases…", 1)),
            Load::Failed(message) if open => out.push(error_row(server, message, 1)),
            Load::Ready(databases) => {
                let mut hit = false;
                for database in databases {
                    let node = Node::Database {
                        server,
                        database: database.clone(),
                    };
                    let row = self.row(&node, database.clone(), "database", 1, true);
                    let mut children = Vec::new();
                    let child_hit = self.database_children(server, database, needle, &mut children);
                    hit |= emit(out, row, children, needle, child_hit);
                }
                if databases.is_empty() && open {
                    out.push(hint(server, "empty", "No databases", 1));
                }
                return hit;
            },
            _ => {},
        }
        false
    }

    fn database_children(
        &self,
        server: u32,
        database: &str,
        needle: &str,
        out: &mut Vec<Row>,
    ) -> bool {
        let open = self.expanded.contains(
            &Node::Database {
                server,
                database: database.to_owned(),
            }
            .id(),
        );
        let Some(load) = self.catalogs.get(&(server, database.to_owned())) else {
            return false;
        };
        let catalog = match load {
            Load::Ready(catalog) => catalog,
            Load::Loading => {
                if open {
                    out.push(hint(
                        server,
                        &format!("load{SEP}{database}"),
                        "Loading objects…",
                        2,
                    ));
                }
                return false;
            },
            Load::Failed(message) => {
                if open {
                    out.push(error_row(server, message, 2));
                }
                return false;
            },
        };

        let mut hit = false;
        for schema in &catalog.schemas {
            let node = Node::Schema {
                server,
                database: database.to_owned(),
                schema: schema.clone(),
            };
            let mut row = self.row(&node, schema.clone(), "schema", 2, true);
            // The standard's catalog is read-only and rarely what anyone is looking for.
            row.dimmed = schema == "information_schema";
            let mut children = Vec::new();
            let mut child_hit = false;
            for kind in ObjectKind::ALL {
                let objects: Vec<&str> = catalog
                    .objects
                    .iter()
                    .filter(|o| o.schema == *schema && o.kind == kind)
                    .map(|o| o.name.as_str())
                    .collect();
                if objects.is_empty() {
                    continue;
                }
                let group = Node::Group {
                    server,
                    database: database.to_owned(),
                    schema: schema.clone(),
                    kind,
                };
                let mut group_row = self.row(&group, kind.plural().to_owned(), "group", 3, true);
                group_row.badge = objects.len().to_string();
                let mut leaves = Vec::new();
                let mut leaf_hit = false;
                for name in objects {
                    let leaf = Node::Object {
                        server,
                        database: database.to_owned(),
                        schema: schema.clone(),
                        name: name.to_owned(),
                        kind,
                    };
                    let leaf_row = self.row(&leaf, name.to_owned(), kind_tag(kind), 4, false);
                    leaf_hit |= emit(&mut leaves, leaf_row, Vec::new(), needle, false);
                }
                child_hit |= emit(&mut children, group_row, leaves, needle, leaf_hit);
            }
            let has_objects = catalog.objects.iter().any(|o| o.schema == *schema);
            if !has_objects && self.expanded.contains(&node.id()) && needle.is_empty() {
                children.push(hint(
                    server,
                    &format!("none{SEP}{database}{SEP}{schema}"),
                    "Empty schema",
                    3,
                ));
            }
            hit |= emit(out, row, children, needle, child_hit);
        }
        hit
    }
}

/// Push `row` (and, when open or when the filter found something below it, its `children`)
/// into `out`. Returns whether this row or anything below it matched the filter.
fn emit(
    out: &mut Vec<Row>,
    mut row: Row,
    children: Vec<Row>,
    needle: &str,
    child_hit: bool,
) -> bool {
    if needle.is_empty() {
        let open = row.expanded;
        out.push(row);
        if open {
            out.extend(children);
        }
        return true;
    }
    let self_hit = row.label.to_lowercase().contains(needle);
    if !self_hit && !child_hit {
        return false;
    }
    if child_hit {
        // A hit inside a closed node would be invisible, so the path to it is opened.
        row.expanded = true;
    }
    let open = row.expanded;
    out.push(row);
    if open {
        out.extend(children);
    }
    true
}

fn into_load<T>(result: Result<T, String>) -> Load<T> {
    match result {
        Ok(value) => Load::Ready(value),
        Err(message) => Load::Failed(message),
    }
}

/// A non-interactive message row.
fn hint(server: u32, key: &str, label: &str, depth: i32) -> Row {
    Row {
        id: format!("hint{SEP}{server}{SEP}{key}"),
        label: label.to_owned(),
        kind: "hint",
        depth,
        expandable: false,
        expanded: false,
        status: "",
        badge: String::new(),
        dimmed: true,
    }
}

fn error_row(server: u32, message: &str, depth: i32) -> Row {
    Row {
        id: format!("err{SEP}{server}{SEP}{depth}"),
        label: message.to_owned(),
        kind: "error",
        depth,
        expandable: false,
        expanded: false,
        status: "",
        badge: String::new(),
        dimmed: false,
    }
}

/// The tree id of a server node.
#[must_use]
pub fn server_node_id(server_id: u32) -> String {
    Node::Server(server_id).id()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::DbObject;
    use crate::model::{ServerDraft, ServerProfile};

    fn server(id: u32, name: &str) -> ServerProfile {
        let valid = ServerDraft {
            name: name.to_owned(),
            ..ServerDraft::default()
        }
        .validate()
        .unwrap();
        ServerProfile::from_valid(id, valid)
    }

    fn catalog() -> Catalog {
        Catalog {
            schemas: vec!["information_schema".into(), "public".into()],
            objects: vec![
                DbObject {
                    schema: "public".into(),
                    name: "customers".into(),
                    kind: ObjectKind::Table,
                },
                DbObject {
                    schema: "public".into(),
                    name: "orders".into(),
                    kind: ObjectKind::Table,
                },
                DbObject {
                    schema: "public".into(),
                    name: "v_top".into(),
                    kind: ObjectKind::View,
                },
            ],
        }
    }

    fn labels(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.label.as_str()).collect()
    }

    #[test]
    fn ids_round_trip_even_with_awkward_names() {
        let node = Node::Object {
            server: 7,
            database: "my db".into(),
            schema: "sales/eu".into(),
            name: "order:lines".into(),
            kind: ObjectKind::View,
        };
        assert_eq!(Node::parse(&node.id()), Some(node));
        assert_eq!(Node::parse("garbage"), None);
    }

    #[test]
    fn a_collapsed_tree_is_one_row_per_server() {
        let servers = [server(1, "a"), server(2, "b")];
        let rows = TreeState::new().rows(servers.iter(), "");
        assert_eq!(labels(&rows), ["a", "b"]);
    }

    #[test]
    fn opening_a_server_asks_for_its_databases_once_and_shows_loading_meanwhile() {
        let servers = [server(1, "a")];
        let mut tree = TreeState::new();
        assert_eq!(tree.toggle(&server_node_id(1)), Some(Fetch::Databases(1)));
        assert_eq!(
            labels(&tree.rows(servers.iter(), "")),
            ["a", "Loading databases…"]
        );

        tree.toggle(&server_node_id(1));
        assert_eq!(
            tree.toggle(&server_node_id(1)),
            None,
            "already loading, no second fetch"
        );
    }

    #[test]
    fn a_failed_load_shows_the_message_in_place_of_children() {
        let servers = [server(1, "a")];
        let mut tree = TreeState::new();
        tree.toggle(&server_node_id(1));
        tree.set_databases(1, Err("Connection refused".into()));
        let rows = tree.rows(servers.iter(), "");
        assert_eq!(rows[1].kind, "error");
        assert_eq!(rows[1].label, "Connection refused");
    }

    #[test]
    fn the_full_hierarchy_unfolds_level_by_level() {
        let servers = [server(1, "a")];
        let mut tree = TreeState::new();
        tree.toggle(&server_node_id(1));
        tree.set_databases(1, Ok(vec!["nusadb".into()]));
        let db = Node::Database {
            server: 1,
            database: "nusadb".into(),
        }
        .id();
        assert_eq!(
            tree.toggle(&db),
            Some(Fetch::Catalog {
                server: 1,
                database: "nusadb".into()
            })
        );
        tree.set_catalog(1, "nusadb", Ok(catalog()));
        tree.toggle(
            &Node::Schema {
                server: 1,
                database: "nusadb".into(),
                schema: "public".into(),
            }
            .id(),
        );
        tree.toggle(
            &Node::Group {
                server: 1,
                database: "nusadb".into(),
                schema: "public".into(),
                kind: ObjectKind::Table,
            }
            .id(),
        );
        let rows = tree.rows(servers.iter(), "");
        assert_eq!(
            labels(&rows),
            [
                "a",
                "nusadb",
                "information_schema",
                "public",
                "Tables",
                "customers",
                "orders",
                "Views"
            ]
        );
        assert!(rows[2].dimmed, "the system schema is de-emphasised");
        assert_eq!(rows[4].badge, "2");
        assert_eq!(rows[5].depth, 4);
    }

    #[test]
    fn search_reaches_loaded_objects_and_opens_the_path_to_them() {
        let servers = [server(1, "a"), server(2, "b")];
        let mut tree = TreeState::new();
        tree.toggle(&server_node_id(1));
        tree.set_databases(1, Ok(vec!["nusadb".into()]));
        tree.toggle(
            &Node::Database {
                server: 1,
                database: "nusadb".into(),
            }
            .id(),
        );
        tree.set_catalog(1, "nusadb", Ok(catalog()));
        tree.collapse_all();

        let rows = tree.rows(servers.iter(), "ORD");
        assert_eq!(labels(&rows), ["a", "nusadb", "public", "Tables", "orders"]);
        assert!(rows.iter().take(4).all(|r| r.expanded));
    }

    #[test]
    fn forgetting_a_server_drops_its_subtree() {
        let servers = [server(1, "a")];
        let mut tree = TreeState::new();
        tree.toggle(&server_node_id(1));
        tree.set_databases(1, Ok(vec!["nusadb".into()]));
        tree.forget_server(1);
        assert_eq!(labels(&tree.rows(servers.iter(), "")), ["a"]);
        assert_eq!(tree.toggle(&server_node_id(1)), Some(Fetch::Databases(1)));
    }

    #[test]
    fn refresh_reloads_what_is_open() {
        let mut tree = TreeState::new();
        tree.toggle(&server_node_id(1));
        tree.set_databases(1, Ok(vec!["nusadb".into()]));
        assert_eq!(tree.refresh(), vec![Fetch::Databases(1)]);
    }
}
