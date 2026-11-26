//! The Diagram screen: the database the navigator points at, drawn as an ER diagram.

use std::collections::BTreeSet;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{AppWindow, Controller, on_ui, sync_model};
use crate::backend::ObjectKind;
use crate::erd::{self, Model, Point};
use crate::tree::Node;
use crate::ui::{ErdBox, ErdColumn};

/// The diagram on screen.
#[derive(Debug, Clone)]
pub(super) struct ErdState {
    server: u32,
    database: String,
    /// Every table of the database, as read.
    full: Model,
    /// The tables shown (one schema, or all) and where their boxes are.
    shown: Model,
    placed: Vec<Point>,
    /// "" for every schema.
    schema: String,
    selected: Option<usize>,
    search: String,
}

impl ErdState {
    fn show(&mut self) {
        let keep = |schema: &str| self.schema.is_empty() || self.schema == schema;
        self.shown = Model {
            tables: self
                .full
                .tables
                .iter()
                .filter(|t| keep(&t.schema))
                .cloned()
                .collect(),
            foreign_keys: self
                .full
                .foreign_keys
                .iter()
                .filter(|f| keep(&f.from.0) && keep(&f.to.0))
                .cloned()
                .collect(),
        };
        self.placed = erd::layout(&self.shown);
        self.selected = None;
    }
}

#[allow(clippy::cast_possible_truncation)]
fn f(value: f64) -> f32 {
    value as f32
}

impl Controller {
    /// The Diagram was opened, or the navigator moved while it is open.
    pub(super) fn erd_follow(&self, window: &AppWindow) {
        if window.get_screen() != "diagram" {
            return;
        }
        let Some((server, database)) = self.target.borrow().clone() else {
            if self.erd.borrow().is_none() {
                window.set_erd_state(SharedString::new());
            }
            return;
        };
        let same = self
            .erd
            .borrow()
            .as_ref()
            .is_some_and(|s| s.server == server && s.database == database);
        let loading = self
            .erd_loading
            .borrow()
            .as_ref()
            .is_some_and(|(s, d)| *s == server && *d == database);
        if !same && !loading {
            self.erd_load(window, server, &database);
        }
    }

    pub(super) fn erd_load(&self, window: &AppWindow, server: u32, database: &str) {
        let Some(target) = self.target_for(server, database) else {
            return;
        };
        let name = self
            .store
            .borrow()
            .get(server)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        window.set_erd_target(format!("{name} / {database}").into());
        window.set_erd_state("loading".into());
        self.erd_loading
            .replace(Some((server, database.to_owned())));
        let weak = window.as_weak();
        let database = database.to_owned();
        self.backend.schema_model(
            target,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    c.erd_loaded(w, server, &database, result)
                });
            }),
        );
    }

    fn erd_loaded(
        &self,
        window: &AppWindow,
        server: u32,
        database: &str,
        result: Result<Model, String>,
    ) {
        self.erd_loading.replace(None);
        let full = match result {
            Ok(model) => model,
            Err(message) => {
                window.set_erd_error(message.into());
                window.set_erd_state("error".into());
                return;
            },
        };
        // Start on `public` when there are several schemas and it has tables; otherwise all.
        let schemas: BTreeSet<&str> = full.tables.iter().map(|t| t.schema.as_str()).collect();
        let schema = if schemas.len() > 1 && schemas.contains("public") {
            "public".to_owned()
        } else {
            String::new()
        };
        let mut list: Vec<SharedString> = vec![SharedString::new()];
        list.extend(schemas.iter().map(|s| (*s).into()));
        window.set_erd_schemas(ModelRc::new(VecModel::from(list)));
        let mut state = ErdState {
            server,
            database: database.to_owned(),
            full,
            shown: Model::default(),
            placed: Vec::new(),
            schema,
            selected: None,
            search: String::new(),
        };
        state.show();
        self.erd.replace(Some(state));
        window.set_erd_search(SharedString::new());
        window.set_erd_zoom(1.0);
        window.set_erd_state("ready".into());
        self.publish_erd(window);
    }

    pub(super) fn publish_erd(&self, window: &AppWindow) {
        let guard = self.erd.borrow();
        let Some(s) = guard.as_ref() else {
            return;
        };
        let model = &s.shown;
        let selected_key = s.selected.map(|i| model.tables[i].key());
        let mut related: BTreeSet<(String, String)> = BTreeSet::new();
        let (mut edges, mut hot) = (Vec::new(), Vec::new());
        for fk in &model.foreign_keys {
            let Some(d) = erd::connector(model, &s.placed, fk) else {
                continue;
            };
            let touches = selected_key
                .as_ref()
                .is_some_and(|k| *k == fk.from || *k == fk.to);
            if touches {
                related.insert(fk.from.clone());
                related.insert(fk.to.clone());
                hot.push(d);
            } else {
                edges.push(d);
            }
        }
        let needle = s.search.trim().to_lowercase();
        let boxes: Vec<ErdBox> = model
            .tables
            .iter()
            .zip(&s.placed)
            .enumerate()
            .map(|(i, (t, p))| {
                let columns: Vec<ErdColumn> = t
                    .columns
                    .iter()
                    .take(erd::MAX_ROWS)
                    .map(|c| ErdColumn {
                        name: c.name.as_str().into(),
                        r#type: c.data_type.as_str().into(),
                        key: match (c.primary, c.foreign) {
                            (true, true) => "PK FK",
                            (true, false) => "PK",
                            (false, true) => "FK",
                            (false, false) => "",
                        }
                        .into(),
                        required: !c.nullable,
                    })
                    .collect();
                let is_selected = s.selected == Some(i);
                let is_related = related.contains(&t.key());
                ErdBox {
                    x: f(p.x),
                    y: f(p.y),
                    h: f(t.height()),
                    title: if s.schema.is_empty() && t.schema != "public" {
                        format!("{}.{}", t.schema, t.name)
                    } else {
                        t.name.clone()
                    }
                    .into(),
                    columns: ModelRc::new(VecModel::from(columns)),
                    more: if t.columns.len() > erd::MAX_ROWS {
                        format!("+ {} more columns", t.columns.len() - erd::MAX_ROWS).into()
                    } else {
                        SharedString::new()
                    },
                    selected: is_selected,
                    related: is_related && !is_selected,
                    dim: s.selected.is_some() && !is_selected && !is_related,
                    hit: !needle.is_empty() && t.name.to_lowercase().contains(&needle),
                }
            })
            .collect();
        let (w, h) = erd::extent(model, &s.placed);
        window.set_erd_canvas_w(f(w));
        window.set_erd_canvas_h(f(h));
        window.set_erd_edges(edges.join(" ").into());
        window.set_erd_hot_edges(hot.join(" ").into());
        // Updated row by row (the model is bound once at start-up), so the box being dragged
        // keeps its pressed state instead of being rebuilt under the pointer.
        sync_model(&self.erd_boxes, boxes);
        window.set_erd_schema(s.schema.as_str().into());
        let summary = match s.selected {
            Some(i) => {
                let t = &model.tables[i];
                let out = model
                    .foreign_keys
                    .iter()
                    .filter(|k| k.from == t.key())
                    .count();
                let inc = model
                    .foreign_keys
                    .iter()
                    .filter(|k| k.to == t.key())
                    .count();
                format!(
                    "{}: references {out} table{}, referenced by {inc}",
                    t.name,
                    if out == 1 { "" } else { "s" }
                )
            },
            None => format!(
                "{} tables · {} foreign keys",
                model.tables.len(),
                model.foreign_keys.len()
            ),
        };
        window.set_erd_summary(summary.into());
    }

    pub(super) fn erd_change(&self, window: &AppWindow, change: impl FnOnce(&mut ErdState)) {
        if let Some(s) = self.erd.borrow_mut().as_mut() {
            change(s);
        }
        self.publish_erd(window);
    }

    pub(super) fn erd_select(&self, window: &AppWindow, row: i32) {
        self.erd_change(window, |s| {
            s.selected = usize::try_from(row)
                .ok()
                .filter(|&i| i < s.shown.tables.len());
        });
    }

    pub(super) fn erd_drag(&self, window: &AppWindow, row: i32, dx: f32, dy: f32) {
        self.erd_change(window, |s| {
            if let Some(p) = usize::try_from(row).ok().and_then(|i| s.placed.get_mut(i)) {
                p.x = (p.x + f64::from(dx)).max(0.0);
                p.y = (p.y + f64::from(dy)).max(0.0);
            }
        });
    }

    pub(super) fn erd_search(&self, window: &AppWindow, text: &str) {
        self.erd_change(window, |s| {
            s.search = text.to_owned();
            let needle = text.trim().to_lowercase();
            // The first match is selected, so its relations light up too.
            s.selected = (!needle.is_empty())
                .then(|| {
                    s.shown
                        .tables
                        .iter()
                        .position(|t| t.name.to_lowercase().contains(&needle))
                })
                .flatten();
        });
    }

    pub(super) fn erd_pick_schema(&self, window: &AppWindow, schema: &str) {
        self.erd_change(window, |s| {
            s.schema = schema.to_owned();
            s.show();
        });
    }

    pub(super) fn erd_relayout(&self, window: &AppWindow) {
        self.erd_change(window, |s| {
            s.placed = erd::layout(&s.shown);
        });
    }

    pub(super) fn erd_reload(&self, window: &AppWindow) {
        let key = self
            .erd
            .borrow()
            .as_ref()
            .map(|s| (s.server, s.database.clone()));
        if let Some((server, database)) = key {
            self.erd.replace(None);
            self.erd_load(window, server, &database);
        }
    }

    /// Double-click: the table's page, on its Columns tab.
    pub(super) fn erd_open(&self, window: &AppWindow, row: i32) {
        let node = self.erd.borrow().as_ref().and_then(|s| {
            let t = s.shown.tables.get(usize::try_from(row).ok()?)?;
            Some(Node::Object {
                server: s.server,
                database: s.database.clone(),
                schema: t.schema.clone(),
                name: t.name.clone(),
                kind: ObjectKind::Table,
            })
        });
        if let Some(node) = node {
            self.selection.replace(Some(node.clone()));
            self.open_object(window, &node, "columns");
            self.publish(window);
        }
    }

    pub(super) fn erd_export(&self, window: &AppWindow) {
        let (svg, name) = {
            let guard = self.erd.borrow();
            let Some(s) = guard.as_ref() else {
                return;
            };
            let title = if s.schema.is_empty() {
                s.database.clone()
            } else {
                format!("{}.{}", s.database, s.schema)
            };
            (
                erd::to_svg(&s.shown, &s.placed, &format!("ER diagram of {title}")),
                format!("{title}-erd.svg"),
            )
        };
        let Some(path) = Self::choose_svg_path(&name) else {
            return;
        };
        match std::fs::write(&path, svg) {
            Ok(()) => self.toast(window, &format!("Saved {}", path.display()), false),
            Err(e) => self.toast(window, &format!("Could not save the diagram: {e}"), true),
        }
    }

    fn choose_svg_path(default_name: &str) -> Option<std::path::PathBuf> {
        #[cfg(debug_assertions)]
        if let Some(dir) = std::env::var_os("NUSADB_STUDIO_EXPORT_DIR") {
            return Some(std::path::PathBuf::from(dir).join(default_name));
        }
        rfd::FileDialog::new()
            .set_title("Export diagram")
            .set_file_name(default_name)
            .add_filter("SVG image", &["svg"])
            .save_file()
    }
}
