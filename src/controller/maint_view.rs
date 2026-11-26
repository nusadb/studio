//! Maintenance from the navigator: ANALYZE, VACUUM, REINDEX and CHECKPOINT, as NusaDB's
//! `sql-reference.md` (*Maintenance*) documents them. `CLUSTER` is not offered: the server accepts
//! it for compatibility only and does nothing.

use slint::ComponentHandle;

use super::{AppWindow, Controller, explain, on_ui};
use crate::backend::{ObjectKind, StatementOutcome, format_elapsed, quote_ident};
use crate::designer::valid_ident;
use crate::tree::Node;

pub(super) mod label {
    pub const ANALYZE: &str = "Analyze";
    pub const VACUUM: &str = "Vacuum";
    pub const REINDEX: &str = "Reindex";
    pub const ANALYZE_ALL: &str = "Analyze all tables";
    pub const VACUUM_ALL: &str = "Vacuum all tables";
    pub const REINDEX_DATABASE: &str = "Reindex database";
    pub const REINDEX_SCHEMA: &str = "Reindex schema";
    pub const CHECKPOINT: &str = "Checkpoint";
}

/// The maintenance a navigator node offers, in menu order.
pub(super) fn offered(node: &Node) -> Vec<&'static str> {
    match node {
        Node::Database { .. } => vec![
            label::ANALYZE_ALL,
            label::VACUUM_ALL,
            label::REINDEX_DATABASE,
            label::CHECKPOINT,
        ],
        Node::Schema { .. } => vec![label::REINDEX_SCHEMA],
        Node::Object {
            kind: ObjectKind::Table,
            ..
        } => vec![label::ANALYZE, label::VACUUM, label::REINDEX],
        _ => Vec::new(),
    }
}

/// A name `VACUUM` takes. NusaDB's VACUUM refuses quoted identifiers (`VACUUM "orders"` is
/// 0A000), so the name goes unquoted, which is only the same name when it is a plain lower-case
/// identifier.
fn bare(name: &str) -> Option<&str> {
    (valid_ident(name) && name == name.to_lowercase()).then_some(name)
}

/// The statement for `label` on `node`, or why there is none.
pub(super) fn statement(node: &Node, label: &str) -> Result<String, String> {
    let quoted =
        |schema: &str, name: &str| format!("{}.{}", quote_ident(schema), quote_ident(name));
    match (node, label) {
        (Node::Database { .. }, label::ANALYZE_ALL) => Ok("ANALYZE".to_owned()),
        (Node::Database { .. }, label::VACUUM_ALL) => Ok("VACUUM".to_owned()),
        (Node::Database { database, .. }, label::REINDEX_DATABASE) => {
            Ok(format!("REINDEX DATABASE {}", quote_ident(database)))
        },
        (Node::Database { .. }, label::CHECKPOINT) => Ok("CHECKPOINT".to_owned()),
        (Node::Schema { schema, .. }, label::REINDEX_SCHEMA) => {
            Ok(format!("REINDEX SCHEMA {}", quote_ident(schema)))
        },
        (Node::Object { schema, name, .. }, label::ANALYZE) => {
            Ok(format!("ANALYZE {}", quoted(schema, name)))
        },
        (Node::Object { schema, name, .. }, label::REINDEX) => {
            Ok(format!("REINDEX TABLE {}", quoted(schema, name)))
        },
        (Node::Object { schema, name, .. }, label::VACUUM) => match (bare(schema), bare(name)) {
            (Some(s), Some(n)) => Ok(format!("VACUUM {s}.{n}")),
            _ => Err(format!(
                "NusaDB's VACUUM does not accept a quoted name, and {schema}.{name} needs quoting. \
                 Run VACUUM (the whole database) instead."
            )),
        },
        _ => Err(format!("{label} does not apply here.")),
    }
}

/// What the statement does, for the toast once it finished.
fn done(label: &str) -> &'static str {
    match label {
        label::ANALYZE | label::ANALYZE_ALL => "statistics refreshed",
        label::VACUUM | label::VACUUM_ALL => "old row versions reclaimed",
        label::REINDEX | label::REINDEX_SCHEMA | label::REINDEX_DATABASE => {
            "indexes checked against their rows"
        },
        label::CHECKPOINT => "log folded into the image",
        _ => "done",
    }
}

impl Controller {
    /// Run a maintenance statement for the selected node, on its own database's session.
    pub(super) fn maintenance(&self, window: &AppWindow, node: &Node, label: &str) {
        let sql = match statement(node, label) {
            Ok(sql) => sql,
            Err(message) => {
                self.toast(window, &message, true);
                return;
            },
        };
        let Some(database) = node.database() else {
            return;
        };
        let Some(target) = self.target_for(node.server(), database) else {
            return;
        };
        self.toast(window, &format!("{sql}…"), false);
        let weak = window.as_weak();
        let label = label.to_owned();
        self.backend.run(
            target,
            vec![sql.clone()],
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    let message = match &result {
                        Err(m) => Err(m.clone()),
                        Ok(report) => {
                            match report.statements.first().map(|s| (&s.outcome, s.elapsed)) {
                                Some((StatementOutcome::Done { .. }, elapsed)) => Ok(format!(
                                    "{sql}: {} in {}",
                                    done(&label),
                                    format_elapsed(elapsed)
                                )),
                                Some((StatementOutcome::Failed { code, message }, _)) => {
                                    Err(explain(code.as_deref(), message))
                                },
                                None => Ok(sql.clone()),
                            }
                        },
                    };
                    match message {
                        Ok(text) => c.toast(w, &text, false),
                        Err(text) => c.toast(w, &format!("{sql} failed: {text}"), true),
                    }
                });
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(schema: &str, name: &str) -> Node {
        Node::Object {
            server: 1,
            database: "shop".into(),
            schema: schema.into(),
            name: name.into(),
            kind: ObjectKind::Table,
        }
    }

    #[test]
    fn each_node_offers_what_the_engine_supports() {
        let db = Node::Database {
            server: 1,
            database: "shop".into(),
        };
        assert_eq!(offered(&db).len(), 4);
        assert_eq!(
            offered(&table("public", "orders")),
            [label::ANALYZE, label::VACUUM, label::REINDEX]
        );
        assert_eq!(
            statement(&db, label::REINDEX_DATABASE).unwrap(),
            "REINDEX DATABASE \"shop\""
        );
        assert_eq!(statement(&db, label::CHECKPOINT).unwrap(), "CHECKPOINT");
    }

    #[test]
    fn vacuum_names_a_table_without_quotes_or_explains_why_it_cannot() {
        assert_eq!(
            statement(&table("public", "orders"), label::VACUUM).unwrap(),
            "VACUUM public.orders"
        );
        assert_eq!(
            statement(&table("public", "orders"), label::ANALYZE).unwrap(),
            "ANALYZE \"public\".\"orders\""
        );
        assert!(
            statement(&table("public", "Orders"), label::VACUUM)
                .unwrap_err()
                .contains("quoted name")
        );
    }
}
