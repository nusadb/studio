//! "Export as SQL…" on a database or a schema: one `.sql` file that recreates it, structure and
//! rows.
//!
//! Order matters for a file meant to be run: schema, sequences, then tables with every table
//! after the tables its foreign keys point to, then their rows in that same order, then views
//! and materialized views (which read the tables). The whole file is one transaction. Rows are
//! streamed per table into a temporary file by the ordinary export, so a big table is never
//! held in memory, and the parts are joined at the end.
//!
//! Not in the dump: functions (NusaDB's catalog does not give their parameter types, see the
//! report), roles and grants (per server, and passwords live outside SQL).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::PathBuf;

use slint::ComponentHandle;

use super::{AppWindow, Controller, on_ui};
use crate::backend::{DbObject, ObjectInfo, ObjectKind, quote_ident};
use crate::ddl;
use crate::tree::Node;

/// A dump in progress.
pub(super) struct Dump {
    server: u32,
    database: String,
    path: PathBuf,
    scope: String,
    objects: Vec<DbObject>,
    infos: HashMap<usize, ObjectInfo>,
    waiting: usize,
    failed: Vec<String>,
    /// Tables in dependency order, and the next one whose rows are being written.
    tables: Vec<usize>,
    next: usize,
    dir: PathBuf,
}

/// The order tables can be created and filled in: every table after those it references.
/// A cycle (or a self-reference) cannot be ordered; those tables keep their catalog order.
fn dependency_order(objects: &[DbObject], infos: &HashMap<usize, ObjectInfo>) -> Vec<usize> {
    let tables: Vec<usize> = (0..objects.len())
        .filter(|i| objects[*i].kind == ObjectKind::Table)
        .collect();
    let find = |reference: &str, from: &DbObject| -> Option<usize> {
        let (schema, name) = reference
            .split_once('.')
            .map_or((from.schema.as_str(), reference), |(s, n)| (s, n));
        tables
            .iter()
            .copied()
            .find(|t| objects[*t].name == name && objects[*t].schema == schema)
    };
    let mut done: Vec<usize> = Vec::new();
    let mut placed: HashSet<usize> = HashSet::new();
    let mut left = tables.clone();
    while !left.is_empty() {
        let before = left.len();
        left.retain(|t| {
            let needs: Vec<usize> = infos
                .get(t)
                .map(|info| {
                    info.constraints
                        .iter()
                        .filter_map(|c| c.references.as_ref())
                        .filter_map(|(table, _)| find(table, &objects[*t]))
                        .filter(|r| r != t)
                        .collect()
                })
                .unwrap_or_default();
            if needs.iter().all(|r| placed.contains(r)) {
                done.push(*t);
                placed.insert(*t);
                false
            } else {
                true
            }
        });
        if left.len() == before {
            // A cycle: take the rest as they are.
            done.append(&mut left);
        }
    }
    done
}

impl Controller {
    /// Start "Export as SQL…" for a database or schema node.
    pub(super) fn dump_start(&self, window: &AppWindow, node: &Node) {
        if self.dump.borrow().is_some() {
            self.toast(window, "An export is already running.", true);
            return;
        }
        let (server, database, schema) = match node {
            Node::Database { server, database } => (*server, database.clone(), None),
            Node::Schema {
                server,
                database,
                schema,
            } => (*server, database.clone(), Some(schema.clone())),
            _ => return,
        };
        let objects: Vec<DbObject> = self
            .tree
            .borrow()
            .catalog(server, &database)
            .map(|c| {
                c.objects
                    .iter()
                    .filter(|o| schema.as_ref().is_none_or(|s| *s == o.schema))
                    .filter(|o| o.schema != "information_schema")
                    .filter(|o| {
                        matches!(
                            o.kind,
                            ObjectKind::Table
                                | ObjectKind::View
                                | ObjectKind::MaterializedView
                                | ObjectKind::Sequence
                        )
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if objects.is_empty() {
            self.toast(
                window,
                "Nothing to export: open the database in the navigator first.",
                true,
            );
            return;
        }
        let scope = schema.clone().unwrap_or_else(|| database.clone());
        let Some(path) = Self::choose_dump_path(&format!("{scope}.sql")) else {
            return;
        };
        let Some(target) = self.target_for(server, &database) else {
            return;
        };
        let dir = std::env::temp_dir().join(format!(
            "nusadb-studio-dump-{}-{}",
            std::process::id(),
            self.toast_serial.get()
        ));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.toast(window, &format!("Export failed: {e}"), true);
            return;
        }
        self.toast(window, &format!("Exporting {scope}…"), false);
        let count = objects.len();
        self.dump.replace(Some(Dump {
            server,
            database,
            path,
            scope,
            objects: objects.clone(),
            infos: HashMap::new(),
            waiting: count,
            failed: Vec::new(),
            tables: Vec::new(),
            next: 0,
            dir,
        }));
        for (i, object) in objects.into_iter().enumerate() {
            let weak = window.as_weak();
            self.backend.describe(
                target.clone(),
                object,
                Box::new(move |result| {
                    on_ui(&weak, move |c, w| c.dump_described(w, i, result));
                }),
            );
        }
    }

    fn choose_dump_path(default_name: &str) -> Option<PathBuf> {
        #[cfg(debug_assertions)]
        if let Some(dir) = std::env::var_os("NUSADB_STUDIO_EXPORT_DIR") {
            return Some(PathBuf::from(dir).join(default_name));
        }
        rfd::FileDialog::new()
            .set_title("Export as SQL")
            .set_file_name(default_name)
            .add_filter("SQL", &["sql"])
            .save_file()
    }

    fn dump_described(&self, window: &AppWindow, i: usize, result: Result<ObjectInfo, String>) {
        let ready = {
            let mut dump = self.dump.borrow_mut();
            let Some(d) = dump.as_mut() else { return };
            match result {
                Ok(info) => {
                    d.infos.insert(i, info);
                },
                Err(e) => d.failed.push(format!("{}: {e}", d.objects[i].name)),
            }
            d.waiting -= 1;
            if d.waiting == 0 {
                d.tables = dependency_order(&d.objects, &d.infos);
                true
            } else {
                false
            }
        };
        if ready {
            self.dump_next_rows(window);
        }
    }

    /// Stream the rows of the next table to its temporary file, or finish.
    fn dump_next_rows(&self, window: &AppWindow) {
        let job = {
            let dump = self.dump.borrow();
            let Some(d) = dump.as_ref() else { return };
            d.tables.get(d.next).map(|&t| {
                let o = &d.objects[t];
                (
                    d.server,
                    d.database.clone(),
                    format!(
                        "SELECT * FROM {}.{}",
                        quote_ident(&o.schema),
                        quote_ident(&o.name)
                    ),
                    d.dir.join(format!("{}.sql", d.next)),
                )
            })
        };
        let Some((server, database, sql, part)) = job else {
            return self.dump_finish(window);
        };
        let Some(target) = self.target_for(server, &database) else {
            return;
        };
        let weak = window.as_weak();
        self.backend.export(
            target,
            sql,
            part,
            crate::export::Format::Sql,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    if let Some(d) = c.dump.borrow_mut().as_mut() {
                        if let Err(e) = result {
                            let t = d.tables[d.next];
                            d.failed.push(format!("rows of {}: {e}", d.objects[t].name));
                        }
                        d.next += 1;
                    }
                    c.dump_next_rows(w);
                });
            }),
        );
    }

    fn dump_finish(&self, window: &AppWindow) {
        let Some(d) = self.dump.borrow_mut().take() else {
            return;
        };
        let written = write_dump(&d);
        let _ = std::fs::remove_dir_all(&d.dir);
        match written {
            Ok(rows) if d.failed.is_empty() => self.toast(
                window,
                &format!("Exported {} ({rows} rows) to {}", d.scope, d.path.display()),
                false,
            ),
            Ok(_) => self.toast(
                window,
                &format!(
                    "Exported {} with gaps ({}). See the comments at the top of the file.",
                    d.scope,
                    d.failed.len()
                ),
                true,
            ),
            Err(e) => self.toast(window, &format!("Export failed: {e}"), true),
        }
    }
}

/// Put the parts together in order; returns the number of rows written.
fn write_dump(d: &Dump) -> std::io::Result<usize> {
    let mut out = std::io::BufWriter::new(std::fs::File::create(&d.path)?);
    writeln!(out, "-- {} exported by NusaDB Studio", d.scope)?;
    writeln!(
        out,
        "-- Functions, roles and grants are not included. Run in one transaction: on any error \
         nothing is created."
    )?;
    for f in &d.failed {
        writeln!(out, "-- Not exported: {f}")?;
    }
    writeln!(out, "\nBEGIN;\n")?;
    let mut schemas: Vec<&str> = d
        .objects
        .iter()
        .map(|o| o.schema.as_str())
        .filter(|s| *s != "public")
        .collect();
    schemas.sort_unstable();
    schemas.dedup();
    for s in schemas {
        writeln!(out, "CREATE SCHEMA IF NOT EXISTS {};", quote_ident(s))?;
    }
    let ddl_of = |i: usize| -> Option<String> {
        let o = &d.objects[i];
        d.infos
            .get(&i)
            .map(|info| ddl::object_ddl(o.kind, &o.schema, &o.name, info))
    };
    let of_kind = |k: ObjectKind| (0..d.objects.len()).filter(move |i| d.objects[*i].kind == k);
    for i in of_kind(ObjectKind::Sequence) {
        if let Some(text) = ddl_of(i) {
            writeln!(out, "\n{}", text.trim_end())?;
        }
    }
    for &t in &d.tables {
        if let Some(text) = ddl_of(t) {
            writeln!(out, "\n{}", text.trim_end())?;
        }
    }
    let mut rows = 0;
    for (n, &t) in d.tables.iter().enumerate() {
        let part = d.dir.join(format!("{n}.sql"));
        let Ok(file) = std::fs::File::open(&part) else {
            continue;
        };
        let o = &d.objects[t];
        let qualified = format!("{}.{}", quote_ident(&o.schema), quote_ident(&o.name));
        // Each part inserts into "<n>" (its file name); point it at the real table.
        let placeholder = format!("INSERT INTO {} (", quote_ident(&n.to_string()));
        let mut first = true;
        for line in std::io::BufReader::new(file).lines() {
            let line = line?;
            let Some(rest) = line.strip_prefix(&placeholder) else {
                continue;
            };
            if first {
                writeln!(out, "\n-- Rows of {qualified}")?;
                first = false;
            }
            writeln!(out, "INSERT INTO {qualified} ({rest}")?;
            rows += 1;
        }
    }
    for kind in [ObjectKind::View, ObjectKind::MaterializedView] {
        for i in of_kind(kind) {
            if let Some(text) = ddl_of(i) {
                writeln!(out, "\n{}", text.trim_end())?;
            }
        }
    }
    writeln!(out, "\nCOMMIT;")?;
    out.flush()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Constraint;

    fn table(schema: &str, name: &str) -> DbObject {
        DbObject {
            schema: schema.into(),
            name: name.into(),
            kind: ObjectKind::Table,
        }
    }

    fn refers(to: &str) -> ObjectInfo {
        ObjectInfo {
            constraints: vec![Constraint {
                name: format!("fk_{to}"),
                kind: "FOREIGN KEY".into(),
                columns: vec!["x".into()],
                references: Some((to.into(), vec!["id".into()])),
                on_update: None,
                on_delete: None,
                check: None,
            }],
            ..ObjectInfo::default()
        }
    }

    #[test]
    fn tables_come_after_the_tables_they_reference() {
        let objects = vec![
            table("public", "order_items"),
            table("public", "orders"),
            table("public", "customers"),
            table("public", "employees"),
        ];
        let mut infos = HashMap::new();
        infos.insert(0, refers("orders"));
        infos.insert(1, refers("public.customers"));
        infos.insert(2, ObjectInfo::default());
        infos.insert(3, refers("employees")); // refers to itself
        let order: Vec<&str> = dependency_order(&objects, &infos)
            .into_iter()
            .map(|i| objects[i].name.as_str())
            .collect();
        let pos = |n: &str| order.iter().position(|x| *x == n).unwrap();
        assert!(pos("customers") < pos("orders"));
        assert!(pos("orders") < pos("order_items"));
        assert_eq!(order.len(), 4);
    }
}
