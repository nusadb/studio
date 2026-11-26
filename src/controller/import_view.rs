//! Importing a CSV / TSV / JSON file into the table open on the Data screen.

use std::path::PathBuf;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{AppWindow, Controller, Pending, on_ui};
use crate::backend::{ObjectKind, format_elapsed, group_thousands};
use crate::import::{self, Delimiter, Parsed};
use crate::model::Environment;
use crate::tree::{Load, Node};
use crate::ui::{DetailRow, ImportColumn};

/// Largest file read: COPY is refused past the server's `--copy-max-bytes` anyway, and the
/// whole file is held in memory to be checked and previewed.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const PREVIEW_ROWS: usize = 12;

/// The import being prepared.
#[derive(Debug, Clone)]
pub(super) struct ImportState {
    node: Node,
    path: PathBuf,
    text: String,
    json: bool,
    delimiter: Delimiter,
    pub(super) header: bool,
    pub(super) empty_null: bool,
    parsed: Parsed,
    table_columns: Vec<String>,
    /// Per file column: its table column, if it has one and is ticked.
    mapping: Vec<Option<usize>>,
    /// Per file column: whether it is ticked.
    pub(super) enabled: Vec<bool>,
}

impl ImportState {
    fn reparse(&mut self) -> Result<(), String> {
        self.parsed = if self.json {
            import::parse_json(&self.text)?
        } else {
            import::parse_delimited(&self.text, self.delimiter, self.header, self.empty_null)
        };
        self.mapping = import::map_columns(&self.parsed, &self.table_columns);
        self.enabled = vec![true; self.mapping.len()];
        Ok(())
    }

    fn effective(&self) -> Vec<Option<usize>> {
        self.mapping
            .iter()
            .zip(&self.enabled)
            .map(|(m, on)| if *on { *m } else { None })
            .collect()
    }
}

fn choose_file() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("NUSADB_STUDIO_IMPORT_FILE") {
        return Some(PathBuf::from(path));
    }
    rfd::FileDialog::new()
        .set_title("Import rows from a file")
        .add_filter("Data files", &["csv", "tsv", "txt", "json"])
        .pick_file()
}

impl Controller {
    /// "Import…": pick a file and open the preview for the table on the Data screen.
    pub(super) fn import_start(&self, window: &AppWindow) {
        let Some(
            node @ Node::Object {
                kind: ObjectKind::Table,
                ..
            },
        ) = self.page.borrow().clone()
        else {
            return;
        };
        if self.data_has_unsaved(window) {
            return;
        }
        let columns: Vec<String> = match self.info.borrow().get(&node.id()) {
            Some(Load::Ready(info)) => info.columns.iter().map(|c| c.name.clone()).collect(),
            _ => {
                self.toast(
                    window,
                    "The table's columns are still being read. Try again in a moment.",
                    true,
                );
                self.load_info(window, &node);
                return;
            },
        };
        let Some(path) = choose_file() else {
            return;
        };
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size > MAX_FILE_BYTES {
            self.toast(
                window,
                &format!(
                    "{} is larger than 64 MiB; split it, or load it with nusadb-cli and COPY.",
                    path.display()
                ),
                true,
            );
            return;
        }
        let text = match std::fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    self.toast(window, "The file is not UTF-8 text.", true);
                    return;
                },
            },
            Err(e) => {
                self.toast(
                    window,
                    &format!("Could not read {}: {e}", path.display()),
                    true,
                );
                return;
            },
        };
        let json = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
            || text
                .trim_start_matches('\u{feff}')
                .trim_start()
                .starts_with('[');
        let delimiter = if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("tsv"))
        {
            Delimiter::Tab
        } else {
            Delimiter::sniff(&text)
        };
        let mut state = ImportState {
            node,
            path,
            text,
            json,
            delimiter,
            header: true,
            empty_null: true,
            parsed: Parsed::default(),
            table_columns: columns,
            mapping: Vec::new(),
            enabled: Vec::new(),
        };
        let problem = state.reparse().err();
        self.import.replace(Some(state));
        window.set_imp_busy(false);
        window.set_imp_open(true);
        self.publish_import(window, problem);
    }

    fn publish_import(&self, window: &AppWindow, problem: Option<String>) {
        let guard = self.import.borrow();
        let Some(s) = guard.as_ref() else {
            window.set_imp_open(false);
            return;
        };
        if let Node::Object { schema, name, .. } = &s.node {
            window.set_imp_table(format!("{schema}.{name}").into());
        }
        window.set_imp_file(
            s.path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
                .into(),
        );
        let rows = s.parsed.rows.len();
        window.set_imp_summary(
            format!(
                "{} · {} rows · {} columns",
                if s.json {
                    "JSON array"
                } else {
                    "delimited text"
                },
                group_thousands(rows),
                s.parsed.columns.len()
            )
            .into(),
        );
        window.set_imp_rows(i32::try_from(rows).unwrap_or(i32::MAX));
        window.set_imp_json(s.json);
        window.set_imp_delimiter(s.delimiter.label().into());
        window.set_imp_header(s.header);
        window.set_imp_empty_null(s.empty_null);
        let columns: Vec<ImportColumn> = s
            .parsed
            .columns
            .iter()
            .zip(s.mapping.iter().zip(&s.enabled))
            .map(|(file, (target, on))| ImportColumn {
                file: file.as_str().into(),
                target: target
                    .map(|t| s.table_columns[t].clone())
                    .unwrap_or_default()
                    .into(),
                on: *on,
            })
            .collect();
        window.set_imp_columns(ModelRc::new(VecModel::from(columns)));

        let effective = s.effective();
        let used: Vec<(usize, usize)> = effective
            .iter()
            .enumerate()
            .filter_map(|(f, t)| t.map(|t| (f, t)))
            .collect();
        let headers: Vec<SharedString> = used
            .iter()
            .map(|(_, t)| s.table_columns[*t].as_str().into())
            .collect();
        window.set_imp_preview_headers(ModelRc::new(VecModel::from(headers)));
        let preview: Vec<DetailRow> = s
            .parsed
            .rows
            .iter()
            .take(PREVIEW_ROWS)
            .map(|row| {
                let cells: Vec<SharedString> = used
                    .iter()
                    .map(|(f, _)| {
                        row.get(*f)
                            .cloned()
                            .flatten()
                            .map_or_else(|| "NULL".into(), |v| v.replace('\n', "↵").into())
                    })
                    .collect();
                DetailRow {
                    cells: ModelRc::new(VecModel::from(cells)),
                }
            })
            .collect();
        window.set_imp_preview(ModelRc::new(VecModel::from(preview)));
        let missing: Vec<&str> = s
            .table_columns
            .iter()
            .enumerate()
            .filter(|(i, _)| !used.iter().any(|(_, t)| t == i))
            .map(|(_, c)| c.as_str())
            .collect();
        window.set_imp_defaults(
            if missing.is_empty() {
                String::new()
            } else {
                format!(
                    "Not in the file, so they take their defaults: {}. A NOT NULL column without a \
                     default makes the whole import fail.",
                    missing.join(", ")
                )
            }
            .into(),
        );
        let problem = problem.or_else(|| {
            used.is_empty()
                .then(|| "No column of the file matches a column of the table.".to_owned())
        });
        window.set_imp_problem(problem.unwrap_or_default().into());
    }

    pub(super) fn import_change(
        &self,
        window: &AppWindow,
        change: impl FnOnce(&mut ImportState) -> bool,
    ) {
        let problem = {
            let mut guard = self.import.borrow_mut();
            let Some(s) = guard.as_mut() else {
                return;
            };
            if change(s) { s.reparse().err() } else { None }
        };
        self.publish_import(window, problem);
    }

    pub(super) fn import_delimiter(&self, window: &AppWindow, label: &str) {
        self.import_change(window, |s| {
            if let Some(d) = Delimiter::ALL.into_iter().find(|d| d.label() == label) {
                s.delimiter = d;
            }
            true
        });
    }

    pub(super) fn import_cancel(&self, window: &AppWindow) {
        self.import.replace(None);
        window.set_imp_open(false);
    }

    /// "Import N rows": confirm on production, then one COPY.
    pub(super) fn import_run(&self, window: &AppWindow) {
        let production = self
            .import
            .borrow()
            .as_ref()
            .and_then(|s| {
                self.store
                    .borrow()
                    .get(s.node.server())
                    .map(|p| p.environment == Environment::Prod)
            })
            .unwrap_or(false);
        if production {
            let rows = window.get_imp_rows();
            self.ask(
                window,
                Pending::Import,
                "Load rows into a production table?",
                &[format!(
                    "COPY {}: {rows} rows in one statement",
                    window.get_imp_table()
                )],
                "Import",
            );
            return;
        }
        self.import_confirmed(window);
    }

    pub(super) fn import_confirmed(&self, window: &AppWindow) {
        let Some(s) = self.import.borrow().clone() else {
            return;
        };
        let Node::Object {
            server,
            database,
            schema,
            name,
            ..
        } = &s.node
        else {
            return;
        };
        let (sql, data) =
            match import::copy_payload(schema, name, &s.table_columns, &s.parsed, &s.effective()) {
                Ok(payload) => payload,
                Err(message) => {
                    window.set_imp_problem(message.into());
                    return;
                },
            };
        let Some(target) = self.target_for(*server, database) else {
            return;
        };
        window.set_imp_busy(true);
        window.set_imp_problem(SharedString::new());
        let started = std::time::Instant::now();
        let weak = window.as_weak();
        let label = format!("{schema}.{name}");
        self.backend.copy_in(
            target,
            sql,
            data,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    w.set_imp_busy(false);
                    match result {
                        Ok(rows) => {
                            let node = c.import.borrow().as_ref().map(|s| s.node.clone());
                            c.import.replace(None);
                            w.set_imp_open(false);
                            c.toast(
                                w,
                                &format!(
                                    "Imported {} rows into {label} in {}",
                                    group_thousands(usize::try_from(rows).unwrap_or(usize::MAX)),
                                    format_elapsed(started.elapsed())
                                ),
                                false,
                            );
                            // Row count and rows on screen are stale now.
                            if let Some(node) = node {
                                c.info.borrow_mut().remove(&node.id());
                                c.load_info(w, &node);
                            }
                            c.load_data(w);
                        },
                        Err(message) => {
                            w.set_imp_problem(format!("Nothing was loaded: {message}").into());
                        },
                    }
                });
            }),
        );
    }
}
