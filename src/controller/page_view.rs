//! The object page on the Data screen: a table's rows under "Data", and the whole of what the
//! catalog knows about any object under its other tabs. The inspector only summarises; this is
//! where the full detail is read.

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{AppWindow, Controller, kind_tag};
use crate::backend::{ObjectInfo, ObjectKind};
use crate::detail;
use crate::tree::{Load, Node};
use crate::ui::{DetailRow, DetailSection, DetailTab};

fn section(id: &str, empty: &str) -> DetailSection {
    DetailSection {
        id: id.into(),
        headers: ModelRc::default(),
        widths: ModelRc::default(),
        rows: ModelRc::default(),
        empty: empty.into(),
        code: SharedString::new(),
        note: SharedString::new(),
        min_width: 0,
    }
}

fn to_section(page: &detail::Page) -> DetailSection {
    let headers: Vec<SharedString> = page.headers.iter().map(|h| (*h).into()).collect();
    let widths: Vec<i32> = page
        .widths
        .iter()
        .map(|w| i32::try_from(*w).unwrap_or(120))
        .collect();
    let rows: Vec<DetailRow> = page
        .rows
        .iter()
        .map(|r| {
            let cells: Vec<SharedString> = r.iter().map(|c| c.as_str().into()).collect();
            DetailRow {
                cells: ModelRc::new(VecModel::from(cells)),
            }
        })
        .collect();
    DetailSection {
        id: page.id.into(),
        headers: ModelRc::new(VecModel::from(headers)),
        widths: ModelRc::new(VecModel::from(widths)),
        rows: ModelRc::new(VecModel::from(rows)),
        empty: page.empty.as_str().into(),
        code: page.code.as_str().into(),
        note: page.note.as_str().into(),
        // Each column may shrink to 65 % of its width; below that the table scrolls sideways
        // rather than breaking words. Gaps (12 px) and side padding (24 px) included.
        min_width: i32::try_from(page.widths.iter().map(|w| w * 65 / 100 + 12).sum::<u32>() + 24)
            .unwrap_or(0),
    }
}

impl Controller {
    /// Open `node`'s page on `tab`: `data` for its rows, or a detail tab (`columns`, `ddl` …).
    pub(super) fn open_object(&self, window: &AppWindow, node: &Node, tab: &str) {
        let Node::Object { kind, .. } = node else {
            return;
        };
        let same = self
            .page
            .borrow()
            .as_ref()
            .is_some_and(|p| p.id() == node.id());
        if kind.has_rows() {
            if self.data_node().is_none_or(|open| open.id() != node.id()) {
                // Loads the rows; refuses while the open table has unsaved edits.
                self.open_data(window, node);
            }
            if self.data_node().is_none_or(|open| open.id() != node.id()) {
                return;
            }
        } else if !same {
            if self.data_has_unsaved(window) {
                window.set_screen("datagrid".into());
                return;
            }
            self.data.replace(None);
            self.data_result.replace(None);
        }
        self.page.replace(Some(node.clone()));
        window.set_data_tab(
            if kind.has_rows() || tab != "data" {
                tab
            } else {
                "overview"
            }
            .into(),
        );
        window.set_screen("datagrid".into());
        self.load_info(window, node);
        self.publish_page(window);
    }

    /// The object whose rows the Data screen holds.
    fn data_node(&self) -> Option<Node> {
        self.data.borrow().as_ref().map(|d| Node::Object {
            server: d.server,
            database: d.database.clone(),
            schema: d.schema.clone(),
            name: d.name.clone(),
            kind: d.kind,
        })
    }

    /// Push the open object's tabs and the active tab's content.
    pub(super) fn publish_page(&self, window: &AppWindow) {
        let Some(node) = self.page.borrow().clone() else {
            window.set_data_tabs(ModelRc::default());
            window.set_data_page_id(SharedString::new());
            return;
        };
        window.set_data_page_id(node.id().into());
        let Node::Object {
            server,
            database,
            schema,
            name,
            kind,
        } = &node
        else {
            return;
        };
        let info = self.info.borrow().get(&node.id()).cloned();
        let pages = match &info {
            Some(Load::Ready(info)) => detail::pages(*kind, schema, name, info),
            _ => Vec::new(),
        };
        let mut tabs = Vec::new();
        if kind.has_rows() {
            tabs.push(DetailTab {
                id: "data".into(),
                label: "Data".into(),
            });
        }
        window.global::<crate::ui::VectorStore>().set_available(
            matches!(&info, Some(Load::Ready(i)) if Controller::has_vectors(&i.columns)),
        );
        if let Some(Load::Ready(info)) = &info {
            tabs.extend(
                Controller::analysis_tabs(*kind, &info.columns)
                    .into_iter()
                    .map(|(id, label)| DetailTab {
                        id: id.into(),
                        label: label.into(),
                    }),
            );
        }
        tabs.extend(pages.iter().map(|p| DetailTab {
            id: p.id.into(),
            label: p.label().into(),
        }));
        let mut tab = window.get_data_tab().to_string();
        let analysis = tabs
            .iter()
            .any(|t| t.id == tab.as_str() && (tab == "profile" || tab == "vectors"));
        if tab != "data" && !analysis && !pages.is_empty() && !pages.iter().any(|p| p.id == tab) {
            // A tab this kind does not have (a view has no Indexes): fall back to the first.
            tab = pages[0].id.to_owned();
            window.set_data_tab(tab.as_str().into());
        }
        window.set_data_tabs(ModelRc::new(VecModel::from(tabs)));
        // The catalog was (re)read while the Vectors tab is showing: read its state again.
        if tab == "vectors"
            && matches!(&info, Some(Load::Ready(_)))
            && self.vectors.borrow().is_none()
        {
            self.vectors_load(window);
        }
        let content = match (&info, pages.iter().find(|p| p.id == tab)) {
            (_, Some(page)) => to_section(page),
            (Some(Load::Failed(message)), _) => section(&tab, message),
            _ => section(&tab, "Reading the catalog…"),
        };
        window.set_data_section(content);

        if !kind.has_rows() {
            // No rows to page through, so the Data screen's header comes from here.
            let server_name = self
                .store
                .borrow()
                .get(*server)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            window.set_data_object_label(format!("{schema}.{name}").into());
            window.set_data_object_kind(kind_tag(*kind).into());
            window.set_data_target_label(format!("{server_name} / {database}").into());
        }
    }

    pub(super) fn page_pick_tab(&self, window: &AppWindow, id: &str) {
        window.set_data_tab(id.into());
        window
            .global::<crate::ui::SelectionStore>()
            .set_summary(SharedString::new());
        match id {
            "profile" => {
                self.profile_publish(window);
                // Small tables are profiled at once; a large one waits for a click (or a sample).
                let small = self.page.borrow().as_ref().is_some_and(|n| {
                    matches!(self.info.borrow().get(&n.id()), Some(Load::Ready(i)) if i.row_count.is_some_and(|r| r <= 200_000))
                });
                if small
                    && window
                        .global::<crate::ui::ProfileStore>()
                        .get_state()
                        .is_empty()
                {
                    self.profile_run(window, 0);
                }
            },
            "vectors" => self.vectors_load(window),
            _ => {},
        }
        if id == "data"
            && self.data.borrow().is_none()
            && let Some(node) = self.page.borrow().clone()
        {
            self.open_data(window, &node);
        }
        self.publish_page(window);
    }

    /// Open a detail tab's SQL in a new editor tab, on the object's database.
    pub(super) fn page_open_code(&self, window: &AppWindow, code: &str) {
        let Some(Node::Object {
            server,
            database,
            name,
            ..
        }) = self.page.borrow().clone()
        else {
            return;
        };
        self.set_target(server, &database);
        self.new_tab(window, Some(&name), code);
        window.set_screen("workspace".into());
        self.publish(window);
    }
}

/// How many of each part an object has, for the inspector's summary.
pub(super) fn structure_counts(kind: ObjectKind, info: &ObjectInfo) -> Vec<(&'static str, usize)> {
    let mut out = Vec::new();
    if kind == ObjectKind::Table {
        out.push(("Constraints", info.constraints.len()));
        out.push(("Indexes", info.indexes.len()));
        out.push(("Triggers", info.triggers.len()));
        out.push(("Policies", info.policies.len()));
    }
    out
}
