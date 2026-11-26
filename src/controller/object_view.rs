//! Object actions from the navigator's right-click menu that are not screens of their own:
//! copying a name, refreshing, and the destructive TRUNCATE and DROP, which always ask first.

use slint::{ComponentHandle, SharedString};

use super::{AppWindow, Controller, Pending, action, on_ui};
use crate::backend::{ObjectKind, StatementOutcome, quote_ident};
use crate::tree::Node;

/// Creating and reshaping entries, shown with the object's own actions at the top of the menu.
pub(super) fn creating(node: &Node) -> Vec<&'static str> {
    match node {
        Node::Server(_) => vec![action::NEW_DATABASE],
        Node::Database { .. } => vec![action::NEW_SCHEMA],
        Node::Schema { .. } => vec![action::NEW_VIEW],
        // A folder offers to make one more of what it holds.
        Node::Group { kind, .. } if *kind == ObjectKind::View => vec![action::NEW_VIEW],
        // NusaDB renames tables only (there is no ALTER VIEW … RENAME).
        Node::Object { kind, .. } if *kind == ObjectKind::Table => {
            vec![action::RENAME, action::DUPLICATE]
        },
        _ => Vec::new(),
    }
}

/// The entries at the bottom of the menu: copy the name, refresh, then the destructive ones.
pub(super) fn offered(node: &Node) -> Vec<&'static str> {
    match node {
        Node::Object { kind, .. } => {
            let mut out = vec![action::COPY_NAME];
            if *kind == ObjectKind::Table {
                out.push(action::TRUNCATE);
            }
            if drop_noun(*kind).is_some() {
                out.push(action::DROP);
            }
            out
        },
        Node::Server(_) => vec![action::COPY_NAME, action::REFRESH],
        Node::Database { .. } | Node::Schema { .. } => vec![
            action::EXPORT_SQL,
            action::COPY_NAME,
            action::REFRESH,
            action::DROP,
        ],
        Node::Group { .. } => vec![action::REFRESH],
    }
}

/// What the name dialog is asking for.
#[derive(Debug, Clone)]
pub(super) enum Prompt {
    NewDatabase(u32),
    NewSchema(u32, String),
    RenameTable(Node),
    Duplicate(Node),
}

/// What DROP calls the object, for the kinds Studio offers to drop.
fn drop_noun(kind: ObjectKind) -> Option<&'static str> {
    match kind {
        ObjectKind::Table => Some("TABLE"),
        ObjectKind::View => Some("VIEW"),
        ObjectKind::MaterializedView => Some("MATERIALIZED VIEW"),
        ObjectKind::Sequence => Some("SEQUENCE"),
        _ => None,
    }
}

/// The name as it would be written in SQL: quoted where it must be, with its schema.
/// Materialized views go bare: NusaDB refuses a qualifier on them.
fn sql_name(kind: ObjectKind, schema: &str, name: &str) -> String {
    if kind == ObjectKind::MaterializedView {
        quote_ident(name)
    } else {
        format!("{}.{}", quote_ident(schema), quote_ident(name))
    }
}

impl Controller {
    /// Run an action of this module; `false` when `label` is not one.
    pub(super) fn object_action(&self, window: &AppWindow, node: &Node, label: &str) -> bool {
        match label {
            action::COPY_NAME => {
                let text = match node {
                    Node::Object {
                        schema, name, kind, ..
                    } => sql_name(*kind, schema, name),
                    Node::Schema { schema, .. } => quote_ident(schema),
                    Node::Database { database, .. } => quote_ident(database),
                    Node::Server { .. } => self
                        .store
                        .borrow()
                        .get(node.server())
                        .map(|p| p.name.clone())
                        .unwrap_or_default(),
                    Node::Group { .. } => return true,
                };
                window.invoke_copy_text(text.as_str().into());
                self.toast(window, &format!("Copied {text}"), false);
            },
            action::REFRESH => self.refresh_tree(window),
            action::EXPORT_SQL => self.dump_start(window, node),
            action::TRUNCATE | action::DROP => self.object_destroy(window, node, label),
            action::NEW_DATABASE => self.prompt_open(
                window,
                Prompt::NewDatabase(node.server()),
                "New database",
                "A separate database on this server; connect to it by name.",
                "",
                "",
                "Create",
            ),
            action::NEW_SCHEMA => {
                let Some(db) = node.database() else {
                    return true;
                };
                self.prompt_open(
                    window,
                    Prompt::NewSchema(node.server(), db.to_owned()),
                    "New schema",
                    &format!("In {db}."),
                    "",
                    "",
                    "Create",
                );
            },
            action::NEW_VIEW => {
                if let Node::Schema {
                    server,
                    database,
                    schema,
                }
                | Node::Group {
                    server,
                    database,
                    schema,
                    ..
                } = node
                {
                    self.set_target(*server, database);
                    let q = quote_ident(schema);
                    let sql = format!(
                        "CREATE VIEW {q}.new_view AS\nSELECT *\nFROM {q}.table_name\nWHERE true;\n"
                    );
                    self.new_tab(window, Some("new view"), &sql);
                    window.set_screen("workspace".into());
                    self.publish(window);
                }
            },
            action::RENAME => {
                if let Node::Object { name, .. } = node {
                    self.prompt_open(
                        window,
                        Prompt::RenameTable(node.clone()),
                        &format!("Rename {name}"),
                        "Indexes, constraints, triggers and grants follow the table.",
                        name,
                        "",
                        "Rename",
                    );
                }
            },
            action::DUPLICATE => {
                if let Node::Object { name, .. } = node {
                    self.prompt_open(
                        window,
                        Prompt::Duplicate(node.clone()),
                        &format!("Duplicate {name}"),
                        "Same columns, defaults, constraints and indexes.",
                        &format!("{name}_copy"),
                        "Copy the rows too",
                        "Duplicate",
                    );
                }
            },
            _ => return false,
        }
        true
    }

    fn object_destroy(&self, window: &AppWindow, node: &Node, label: &str) {
        match node {
            Node::Database { database, .. } => {
                let statement = format!("DROP DATABASE {}", quote_ident(database));
                self.ask(
                    window,
                    Pending::Destroy(node.clone(), statement.clone()),
                    &format!("Drop the database {database}?"),
                    &[
                        "Removes the database with every table and row in it.".to_owned(),
                        statement,
                    ],
                    "Drop database",
                );
                return;
            },
            Node::Schema { schema, .. } => {
                let statement = format!("DROP SCHEMA {} CASCADE", quote_ident(schema));
                self.ask(
                    window,
                    Pending::Destroy(node.clone(), statement.clone()),
                    &format!("Drop the schema {schema}?"),
                    &[
                        "Removes the schema and every table, view and sequence in it.".to_owned(),
                        statement,
                    ],
                    "Drop schema",
                );
                return;
            },
            _ => {},
        }
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
        let target = sql_name(*kind, schema, name);
        let (statement, title, reason, accept) = if label == action::TRUNCATE {
            (
                format!("TRUNCATE {target}"),
                format!("Empty {name}?"),
                "Deletes every row. The table and its structure stay.".to_owned(),
                "Truncate",
            )
        } else {
            let Some(noun) = drop_noun(*kind) else {
                return;
            };
            (
                format!("DROP {noun} {target}"),
                format!("Drop {name}?"),
                if *kind == ObjectKind::Table {
                    "Removes the table and every row in it.".to_owned()
                } else {
                    format!("Removes the {}.", kind.noun())
                },
                "Drop",
            )
        };
        let production = self
            .store
            .borrow()
            .get(*server)
            .is_some_and(|p| p.environment == crate::model::Environment::Prod);
        let mut reasons = vec![reason, statement.clone()];
        if production {
            reasons.insert(0, format!("{database} is on a production server."));
        }
        self.ask(
            window,
            Pending::Destroy(node.clone(), statement),
            &title,
            &reasons,
            accept,
        );
    }

    /// The confirmed TRUNCATE or DROP.
    pub(super) fn object_destroy_confirmed(
        &self,
        window: &AppWindow,
        node: &Node,
        statement: &str,
    ) {
        // A database is dropped from another one: a session cannot drop the database it is in.
        let via = match node {
            Node::Database { server, database } => self.other_database(*server, database),
            _ => node.database().map(str::to_owned),
        };
        let Some(target) = via.and_then(|db| self.target_for(node.server(), &db)) else {
            self.toast(
                window,
                "No other database to connect through for this.",
                true,
            );
            return;
        };
        let weak = window.as_weak();
        let node = node.clone();
        let dropping = statement.starts_with("DROP");
        self.backend.run(
            target,
            vec![statement.to_owned()],
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    let failure = match &result {
                        Err(m) => Some(m.clone()),
                        Ok(r) => r.failure().map(|f| match &f.outcome {
                            StatementOutcome::Failed { code, message } => {
                                super::explain(code.as_deref(), message)
                            },
                            StatementOutcome::Done { .. } => String::new(),
                        }),
                    };
                    let name = match &node {
                        Node::Object { name, .. } => name.clone(),
                        Node::Schema { schema, .. } => schema.clone(),
                        Node::Database { database, .. } => database.clone(),
                        _ => String::new(),
                    };
                    if let Some(message) = failure {
                        c.toast(w, &message, true);
                        return;
                    }
                    c.toast(
                        w,
                        &if dropping {
                            format!("Dropped {name}")
                        } else {
                            format!("Emptied {name}")
                        },
                        false,
                    );
                    c.info.borrow_mut().remove(&node.id());
                    if let Node::Database { server, database } = &node
                        && c.target
                            .borrow()
                            .as_ref()
                            .is_some_and(|(s, d)| s == server && d == database)
                    {
                        c.target.replace(None);
                        c.publish_target(w);
                    }
                    if dropping {
                        // Its page would show an object that no longer exists.
                        if w.get_data_page_id().as_str() == node.id() {
                            w.set_screen("workspace".into());
                        }
                        c.refresh_tree(w);
                    } else if w.get_data_page_id().as_str() == node.id() {
                        c.load_data(w);
                    }
                });
            }),
        );
    }
}

impl Controller {
    /// A database on `server` other than `not`: the server's own default if it differs,
    /// otherwise the first other one the navigator knows.
    fn other_database(&self, server: u32, not: &str) -> Option<String> {
        let default = self
            .store
            .borrow()
            .get(server)
            .map(|p| p.params.database.clone());
        if let Some(d) = default.filter(|d| d != not) {
            return Some(d);
        }
        self.tree
            .borrow()
            .databases(server)
            .and_then(|all| all.iter().find(|d| *d != not).cloned())
    }

    #[allow(clippy::too_many_arguments)]
    fn prompt_open(
        &self,
        window: &AppWindow,
        prompt: Prompt,
        title: &str,
        subtitle: &str,
        value: &str,
        check: &str,
        accept: &str,
    ) {
        self.prompt.replace(Some(prompt));
        window.set_prompt_title(title.into());
        window.set_prompt_subtitle(subtitle.into());
        window.set_prompt_value(value.into());
        window.set_prompt_check_label(check.into());
        window.set_prompt_checked(false);
        window.set_prompt_accept_label(accept.into());
        window.set_prompt_error(SharedString::new());
        window.set_prompt_open(true);
    }

    pub(super) fn prompt_cancel(&self, window: &AppWindow) {
        self.prompt.replace(None);
        window.set_prompt_open(false);
    }

    /// Run what the name dialog asked for; on failure the dialog stays open with the reason.
    pub(super) fn prompt_accept(&self, window: &AppWindow) {
        let Some(prompt) = self.prompt.borrow().clone() else {
            return;
        };
        let value = window.get_prompt_value().trim().to_owned();
        if value.is_empty() {
            window.set_prompt_error("Give it a name.".into());
            return;
        }
        let with_rows = window.get_prompt_checked();
        let name = quote_ident(&value);
        let (server, database, statements, done) = match &prompt {
            Prompt::NewDatabase(server) => {
                let Some(via) = self
                    .store
                    .borrow()
                    .get(*server)
                    .map(|p| p.params.database.clone())
                else {
                    return;
                };
                (
                    *server,
                    via,
                    vec![format!("CREATE DATABASE {name}")],
                    format!("Created the database {value}"),
                )
            },
            Prompt::NewSchema(server, database) => (
                *server,
                database.clone(),
                vec![format!("CREATE SCHEMA {name}")],
                format!("Created the schema {value}"),
            ),
            Prompt::RenameTable(Node::Object {
                server,
                database,
                schema,
                name: old,
                ..
            }) => {
                if value == *old {
                    return self.prompt_cancel(window);
                }
                (
                    *server,
                    database.clone(),
                    vec![format!(
                        "ALTER TABLE {}.{} RENAME TO {name}",
                        quote_ident(schema),
                        quote_ident(old)
                    )],
                    format!("Renamed {old} to {value}"),
                )
            },
            Prompt::Duplicate(Node::Object {
                server,
                database,
                schema,
                name: old,
                ..
            }) => {
                let from = format!("{}.{}", quote_ident(schema), quote_ident(old));
                let to = format!("{}.{name}", quote_ident(schema));
                let mut sql = vec![
                    "BEGIN".to_owned(),
                    format!("CREATE TABLE {to} (LIKE {from} INCLUDING ALL)"),
                ];
                if with_rows {
                    sql.push(format!("INSERT INTO {to} SELECT * FROM {from}"));
                }
                sql.push("COMMIT".to_owned());
                (
                    *server,
                    database.clone(),
                    sql,
                    format!("Created {value} from {old}"),
                )
            },
            _ => return,
        };
        let Some(target) = self.target_for(server, &database) else {
            return;
        };
        // Unquoted names fold to lower case on the server.
        let new_name = value.to_lowercase();
        let weak = window.as_weak();
        let rollback = target.clone();
        let opened_txn = statements.first().is_some_and(|s| s == "BEGIN");
        self.backend.run(
            target,
            statements,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    let failure = match &result {
                        Err(m) => Some(m.clone()),
                        Ok(r) => r.failure().map(|f| match &f.outcome {
                            StatementOutcome::Failed { code, message } => {
                                super::explain(code.as_deref(), message)
                            },
                            StatementOutcome::Done { .. } => String::new(),
                        }),
                    };
                    if let Some(message) = failure {
                        if opened_txn {
                            c.backend
                                .run(rollback, vec!["ROLLBACK".to_owned()], Box::new(|_| {}));
                        }
                        // NusaDB copies constraint names verbatim on LIKE (engine finding 20).
                        let message = match clashing_index(&message) {
                            Some(index) if done.starts_with("Created ") => {
                                if index.starts_with(&format!("{new_name}_")) {
                                    format!(
                                        "Another table already has an index named {index}. \
                                         Pick a different name."
                                    )
                                } else {
                                    format!(
                                        "NusaDB copies the index name {index}, which already \
                                         exists. Open the DDL in the editor, rename it there and \
                                         run it."
                                    )
                                }
                            },
                            _ => message,
                        };
                        w.set_prompt_error(message.into());
                        return;
                    }
                    c.prompt_cancel(w);
                    c.toast(w, &done, false);
                    c.refresh_tree(w);
                });
            }),
        );
    }
}

/// The index named in an "index … already exists" error, if that is the error.
fn clashing_index(message: &str) -> Option<String> {
    let rest = message.split("index ").nth(1)?;
    let name = rest
        .strip_suffix(" already exists")
        .or_else(|| rest.find(" already exists").map(|at| &rest[..at]))?;
    Some(name.trim_matches('"').to_owned())
}

#[cfg(test)]
mod tests {
    use super::clashing_index;

    #[test]
    fn index_clash_is_recognised() {
        assert_eq!(
            clashing_index("constraint violation: index zz_b_pkey already exists").as_deref(),
            Some("zz_b_pkey")
        );
        assert_eq!(clashing_index("table not found: x"), None);
    }
}
