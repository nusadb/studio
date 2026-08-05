//! The object tree, flattened into the rows the navigator draws.
//!
//! The hierarchy is produced as a flat list rather than as nested nodes: the view renders one
//! `for` loop, and selection, scrolling, and (later) keyboard traversal stay ordinary list
//! operations. Depth is carried on each row, which is all the view needs to indent it.
//!
//! Only the server level is real today. A server's children come from introspecting a live
//! connection, so an expanded server shows a hint row until that lands — inventing plausible
//! databases would be a lie the user cannot tell apart from the truth.

use std::collections::HashSet;

use crate::model::ServerProfile;

/// One visible line of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Unique within the tree, e.g. `srv:3` or `srv:3/hint`.
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

/// Which nodes the user has opened.
///
/// Expansion is remembered by node id, not by position, so it survives the list being
/// filtered, reordered, or rebuilt.
#[derive(Debug, Default)]
pub struct TreeState {
    expanded: HashSet<String>,
}

/// The tree id of a server node.
#[must_use]
pub fn server_node_id(server_id: u32) -> String {
    format!("srv:{server_id}")
}

impl TreeState {
    /// An all-collapsed tree.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open or close the node `id`.
    pub fn toggle(&mut self, id: &str) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_owned());
        }
    }

    /// Close every node.
    pub fn collapse_all(&mut self) {
        self.expanded.clear();
    }

    /// Flatten `servers` into the visible rows, in order.
    pub fn rows<'a>(&self, servers: impl Iterator<Item = &'a ServerProfile>) -> Vec<Row> {
        let mut rows = Vec::new();

        for server in servers {
            let id = server_node_id(server.id);
            let expanded = self.expanded.contains(&id);

            rows.push(Row {
                label: server.name.clone(),
                kind: "server",
                depth: 0,
                expandable: true,
                expanded,
                status: server.status.as_tag(),
                badge: String::new(),
                dimmed: false,
                id: id.clone(),
            });

            if expanded {
                rows.push(Row {
                    id: format!("{id}/hint"),
                    label: "connect to load databases".to_owned(),
                    kind: "hint",
                    depth: 1,
                    expandable: false,
                    expanded: false,
                    status: "",
                    badge: String::new(),
                    dimmed: true,
                });
            }
        }

        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn a_collapsed_tree_is_one_row_per_server() {
        let servers = [server(1, "a"), server(2, "b")];
        let rows = TreeState::new().rows(servers.iter());

        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.depth == 0 && !row.expanded));
        assert_eq!(rows[0].id, "srv:1");
        assert_eq!(rows[1].label, "b");
    }

    #[test]
    fn expanding_a_server_reveals_an_honest_placeholder_not_invented_databases() {
        let servers = [server(1, "a")];
        let mut state = TreeState::new();
        state.toggle("srv:1");

        let rows = state.rows(servers.iter());
        assert_eq!(rows.len(), 2);
        assert!(rows[0].expanded);
        assert_eq!(rows[1].kind, "hint");
        assert_eq!(rows[1].depth, 1);
        assert!(rows[1].dimmed);
        assert!(!rows[1].expandable);
    }

    #[test]
    fn toggling_twice_closes_the_node_again() {
        let servers = [server(1, "a")];
        let mut state = TreeState::new();
        state.toggle("srv:1");
        state.toggle("srv:1");

        assert_eq!(state.rows(servers.iter()).len(), 1);
    }

    #[test]
    fn expansion_survives_the_list_being_filtered_down_and_back() {
        let servers = [server(1, "a"), server(2, "b")];
        let mut state = TreeState::new();
        state.toggle("srv:2");

        // Only the second server passes the filter…
        assert_eq!(state.rows(servers.iter().skip(1)).len(), 2);

        // …and it is still open once the filter is cleared.
        let rows = state.rows(servers.iter());
        assert_eq!(rows.len(), 3);
        assert!(rows[1].expanded);
    }

    #[test]
    fn collapse_all_closes_every_node() {
        let servers = [server(1, "a"), server(2, "b")];
        let mut state = TreeState::new();
        state.toggle("srv:1");
        state.toggle("srv:2");
        assert_eq!(state.rows(servers.iter()).len(), 4);

        state.collapse_all();
        assert_eq!(state.rows(servers.iter()).len(), 2);
    }
}
