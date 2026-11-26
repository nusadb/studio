//! Grid behaviour shared by the Query and Data screens: column widths the user dragged, copying
//! a cell, and editing a query result that was read from one table.
//!
//! A result is editable when its statement is a plain read of one table (see
//! [`crate::result_source`]) and every column it returns is a column of that table, primary key
//! included. Edits are staged like the Data grid's and saved the same way: in one transaction
//! of their own, or inside the user's open one. The statement is then read again, so what the
//! grid shows is what the table now holds.

use std::collections::{HashMap, HashSet};

use slint::{ComponentHandle, Model, ModelRc, SharedString};

use super::{
    AppWindow, Controller, Pending, TxnState, explain, first_line, is_numeric, on_ui, sorted,
};
use crate::backend::{ObjectKind, ResultSet, RunReport, StatementOutcome, quote_ident};
use crate::edits::{self, EditSet};
use crate::model::Environment;
use crate::result_source;
use crate::tree::{Load, Node};
use crate::ui::{GridColumn, GridRow};

/// Narrowest a dragged column gets, and the widest a fitted one does.
const MIN_COLUMN: f32 = 48.0;
const MAX_FIT: f32 = 640.0;

/// A query result being edited in place.
#[derive(Debug, Clone)]
pub(super) struct ResultEdit {
    pub server: u32,
    pub database: String,
    pub schema: String,
    pub table: String,
    pub columns: Vec<edits::Column>,
    pub key: Vec<usize>,
    pub edits: EditSet,
}

/// Which grid a request is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Which {
    Query,
    Data,
}

impl Controller {
    // ── Column widths ───────────────────────────────────────────────────────

    /// Where the widths of a grid are remembered: per query tab, and per table for Data.
    pub(super) fn width_scope(&self, which: Which) -> String {
        match which {
            Which::Query => format!("query:{}", self.active_tab.get()),
            Which::Data => self.data.borrow().as_ref().map_or_else(String::new, |d| {
                format!("data:{}:{}:{}.{}", d.server, d.database, d.schema, d.name)
            }),
        }
    }

    /// The widths the user set for the grid, by column name.
    pub(super) fn widths(&self, which: Which) -> HashMap<String, f32> {
        self.col_widths
            .borrow()
            .get(&self.width_scope(which))
            .cloned()
            .unwrap_or_default()
    }

    fn grid_models(window: &AppWindow, which: Which) -> (ModelRc<GridColumn>, ModelRc<GridRow>) {
        match which {
            Which::Query => (window.get_grid_columns(), window.get_grid_rows()),
            Which::Data => (window.get_data_columns(), window.get_data_rows()),
        }
    }

    /// Set column `c` to `width` and remember it for this grid.
    pub(super) fn column_resized(&self, window: &AppWindow, which: Which, c: usize, width: f32) {
        let (columns, _) = Self::grid_models(window, which);
        let Some(mut column) = columns.row_data(c) else {
            return;
        };
        let width = width.max(MIN_COLUMN).round();
        if (column.width - width).abs() < 0.5 {
            return;
        }
        column.width = width;
        let name = column.name.to_string();
        columns.set_row_data(c, column);
        let mut offset = 0.0;
        for i in 0..columns.row_count() {
            if let Some(mut col) = columns.row_data(i) {
                if (col.offset - offset).abs() > 0.1 {
                    col.offset = offset;
                    offset += col.width;
                    columns.set_row_data(i, col);
                } else {
                    offset += col.width;
                }
            }
        }
        match which {
            Which::Query => window.set_grid_columns_width(offset),
            Which::Data => window.set_data_columns_width(offset),
        }
        let scope = self.width_scope(which);
        self.col_widths
            .borrow_mut()
            .entry(scope)
            .or_default()
            .insert(name, width);
    }

    /// Fit column `c` to its header and the values in view.
    pub(super) fn column_fit(&self, window: &AppWindow, which: Which, c: usize) {
        let (columns, rows) = Self::grid_models(window, which);
        let Some(column) = columns.row_data(c) else {
            return;
        };
        let mut chars = column.name.chars().count() + column.kind.chars().count() + 2;
        for r in 0..rows.row_count().min(2_000) {
            if let Some(cell) = rows.row_data(r).and_then(|row| row.cells.row_data(c)) {
                let len = if cell.null {
                    4
                } else {
                    cell.text.chars().count()
                };
                chars = chars.max(len);
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let width = (chars as f32).mul_add(7.4, 24.0).clamp(72.0, MAX_FIT);
        self.column_resized(window, which, c, width);
    }

    /// A Query grid cell as shown: column, type, value with staged edits, and whether staged.
    pub(super) fn query_value(
        &self,
        row: usize,
        column: usize,
    ) -> Option<(String, String, Option<String>, bool)> {
        let result = self.query_result.borrow();
        let shown = sorted(result.as_ref()?, self.query_sort.get());
        let name = shown.columns.get(column)?.clone();
        let kind = shown.types.get(column).cloned().unwrap_or_default();
        let staged = self
            .result_edit
            .borrow()
            .as_ref()
            .and_then(|e| e.edits.updates.get(&(row, column)).cloned());
        let value = match &staged {
            Some(edits::Value::Text(t)) => Some(t.clone()),
            Some(_) => None,
            None => shown.rows.get(row)?.get(column)?.clone(),
        };
        Some((name, kind, value, staged.is_some()))
    }

    // ── Editing a query result ──────────────────────────────────────────────

    pub(super) fn result_pending(&self) -> usize {
        self.result_edit
            .borrow()
            .as_ref()
            .map_or(0, |e| e.edits.count())
    }

    /// Refused with a message when staged result edits would be thrown away.
    pub(super) fn result_has_unsaved(&self, window: &AppWindow) -> bool {
        let pending = self.result_pending();
        if pending > 0 {
            self.toast(
                window,
                &format!(
                    "The results have {pending} unsaved row{}. Save or discard {} first.",
                    if pending == 1 { "" } else { "s" },
                    if pending == 1 { "it" } else { "them" }
                ),
                true,
            );
        }
        pending > 0
    }

    /// Work out whether the result in the Query grid can be edited, and publish that. Staged
    /// edits survive as long as the result still comes from the same table.
    pub(super) fn result_shape_refresh(&self, window: &AppWindow) {
        let shape = self.result_shape(window);
        match shape {
            Ok(mut edit) => {
                if let Some(old) = self.result_edit.borrow().as_ref()
                    && (old.server, &old.database, &old.schema, &old.table)
                        == (edit.server, &edit.database, &edit.schema, &edit.table)
                {
                    edit.edits = old.edits.clone();
                }
                self.result_edit.replace(Some(edit));
                window.set_results_editable(true);
                window.set_results_read_only(SharedString::new());
            },
            Err(reason) => {
                self.result_edit.replace(None);
                window.set_results_editable(false);
                window.set_results_read_only(reason.into());
            },
        }
        self.publish_result_edits(window);
    }

    fn result_shape(&self, window: &AppWindow) -> Result<ResultEdit, String> {
        let result = self.query_result.borrow().clone().ok_or_else(String::new)?;
        let sql = self.query_sql.borrow().clone().ok_or_else(String::new)?;
        let source = result_source::single_table(&sql).map_err(|why| sentence(&why))?;
        let schema = match source.schema {
            Some(schema) => schema,
            None if self
                .sql
                .borrow()
                .to_ascii_lowercase()
                .contains("search_path") =>
            {
                return Err(
                    "The script changes search_path, so name the table with its schema to edit it."
                        .to_owned(),
                );
            },
            None => "public".to_owned(),
        };
        let (server, database) = self.target.borrow().clone().ok_or_else(String::new)?;
        let node = Node::Object {
            server,
            database: database.clone(),
            schema: schema.clone(),
            name: source.table.clone(),
            kind: ObjectKind::Table,
        };
        let info = match self.info.borrow().get(&node.id()) {
            Some(Load::Ready(info)) => Some(info.clone()),
            Some(Load::Failed(_)) => {
                return Err(format!(
                    "{schema}.{} was not found as a table.",
                    source.table
                ));
            },
            _ => None,
        };
        let Some(info) = info else {
            // Read the table's columns and key; this runs again when they arrive.
            self.load_info(window, &node);
            return Err(String::new());
        };
        if info.columns.is_empty() {
            return Err(format!(
                "{schema}.{} was not found as a table.",
                source.table
            ));
        }
        let mut columns = Vec::new();
        for (i, name) in result.columns.iter().enumerate() {
            if result.columns[..i].contains(name) {
                return Err(format!("The column {name} appears twice."));
            }
            let Some(col) = info.columns.iter().find(|c| c.name == *name) else {
                return Err(format!("{name} is not a column of {}.", source.table));
            };
            columns.push(edits::Column {
                name: name.clone(),
                data_type: col.data_type.clone(),
            });
        }
        let pk = info
            .constraints
            .iter()
            .find(|c| c.kind == "PRIMARY KEY")
            .map(|pk| pk.columns.clone())
            .unwrap_or_default();
        if pk.is_empty() {
            return Err(format!(
                "{} has no primary key, so a row cannot be told apart.",
                source.table
            ));
        }
        let key: Vec<usize> = pk
            .iter()
            .filter_map(|k| result.columns.iter().position(|c| c == k))
            .collect();
        if key.len() != pk.len() {
            return Err(format!(
                "Add the primary key ({}) to the select list to edit.",
                pk.join(", ")
            ));
        }
        Ok(ResultEdit {
            server,
            database,
            schema,
            table: source.table,
            columns,
            key,
            edits: EditSet::default(),
        })
    }

    /// Cell states and the pending count for the Query grid.
    pub(super) fn publish_result_edits(&self, window: &AppWindow) {
        let edit = self.result_edit.borrow();
        let pending = edit.as_ref().map_or(0, |e| e.edits.count());
        window.set_results_pending(i32::try_from(pending).unwrap_or(i32::MAX));
        let rows = window.get_grid_rows();
        let result = self.query_result.borrow();
        let Some(result) = result.as_ref() else {
            return;
        };
        let shown = sorted(result, self.query_sort.get());
        for r in 0..rows.row_count() {
            let Some(row) = rows.row_data(r) else {
                continue;
            };
            for c in 0..row.cells.row_count() {
                let Some(mut cell) = row.cells.row_data(c) else {
                    continue;
                };
                let staged = edit.as_ref().and_then(|e| e.edits.updates.get(&(r, c)));
                let (raw, null, state): (String, bool, &str) = match staged {
                    Some(edits::Value::Text(t)) => (t.clone(), false, "edited"),
                    Some(_) => (String::new(), true, "edited"),
                    None => {
                        let v = shown
                            .rows
                            .get(r)
                            .and_then(|row| row.get(c))
                            .cloned()
                            .flatten();
                        (v.clone().unwrap_or_default(), v.is_none(), "")
                    },
                };
                let long = raw.contains('\n');
                let text = raw.replace(['\n', '\r'], " ");
                if cell.text.as_str() != text
                    || cell.null != null
                    || cell.state.as_str() != state
                    || cell.long != long
                {
                    cell.text = text.into();
                    cell.null = null;
                    cell.state = state.into();
                    cell.long = long;
                    row.cells.set_row_data(c, cell);
                }
            }
        }
    }

    /// Stage `value` for a cell of the result.
    pub(super) fn result_set(
        &self,
        window: &AppWindow,
        row: usize,
        col: usize,
        value: edits::Value,
    ) {
        {
            let result = self.query_result.borrow();
            let Some(result) = result.as_ref() else {
                return;
            };
            let shown = sorted(result, self.query_sort.get());
            let mut edit = self.result_edit.borrow_mut();
            let Some(edit) = edit.as_mut() else {
                return;
            };
            self.remember(Which::Query, &edit.edits);
            edit.edits.set(&shown.rows, row, col, value);
        }
        self.publish_result_edits(window);
        if window.get_cell_open() {
            self.grid_cell(window, row, col);
        }
    }

    pub(super) fn result_selected(window: &AppWindow) -> Option<(usize, usize)> {
        Some((
            usize::try_from(window.get_grid_selected_row()).ok()?,
            usize::try_from(window.get_grid_selected_col()).ok()?,
        ))
    }

    pub(super) fn result_revert(&self, window: &AppWindow) {
        let Some((row, col)) = Self::result_selected(window) else {
            return;
        };
        if let Some(edit) = self.result_edit.borrow_mut().as_mut() {
            self.remember(Which::Query, &edit.edits);
            edit.edits.updates.remove(&(row, col));
        }
        self.publish_result_edits(window);
        if window.get_cell_open() {
            self.grid_cell(window, row, col);
        }
    }

    pub(super) fn result_discard(&self, window: &AppWindow) {
        self.forget_history(Which::Query);
        if let Some(edit) = self.result_edit.borrow_mut().as_mut() {
            edit.edits = EditSet::default();
        }
        window.set_results_error(SharedString::new());
        self.publish_result_edits(window);
        if let Some((row, col)) = Self::result_selected(window)
            && window.get_cell_open()
        {
            self.grid_cell(window, row, col);
        }
    }

    /// ⌘S / Ctrl+S or Save changes under the results.
    pub(super) fn result_save(&self, window: &AppWindow) {
        let Some(edit) = self.result_edit.borrow().clone() else {
            return;
        };
        let count = edit.edits.count();
        if count == 0 || window.get_results_saving() {
            return;
        }
        let production = self
            .store
            .borrow()
            .get(edit.server)
            .is_some_and(|p| p.environment == Environment::Prod);
        if production {
            self.ask(
                window,
                Pending::SaveResult,
                "Save these changes to production?",
                &[format!(
                    "{count} row{} of {}.{} on a production server",
                    if count == 1 { "" } else { "s" },
                    edit.schema,
                    edit.table
                )],
                "Save to production",
            );
            return;
        }
        self.result_save_confirmed(window);
    }

    pub(super) fn result_save_confirmed(&self, window: &AppWindow) {
        let Some(edit) = self.result_edit.borrow().clone() else {
            return;
        };
        let loaded = {
            let result = self.query_result.borrow();
            let Some(result) = result.as_ref() else {
                return;
            };
            sorted(result, self.query_sort.get()).rows
        };
        let table = format!("{}.{}", quote_ident(&edit.schema), quote_ident(&edit.table));
        let statements = match edit
            .edits
            .statements(&table, &edit.columns, &edit.key, &loaded)
        {
            Ok(s) if s.is_empty() => return,
            Ok(s) => s,
            Err(message) => {
                window.set_results_error(message.into());
                return;
            },
        };
        let Some(target) = self.target_for(edit.server, &edit.database) else {
            return;
        };
        let joins_open_txn = self.txn.get() != TxnState::Idle;
        let mut script = statements;
        if !joins_open_txn {
            script.insert(0, "BEGIN".to_owned());
            script.push("COMMIT".to_owned());
        }
        window.set_results_saving(true);
        window.set_results_error(SharedString::new());
        let weak = window.as_weak();
        let rollback = target.clone();
        self.backend.run(
            target,
            script,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    c.result_saved(w, result, joins_open_txn, rollback)
                });
            }),
        );
    }

    fn result_saved(
        &self,
        window: &AppWindow,
        result: Result<RunReport, String>,
        joined_txn: bool,
        target: crate::backend::Target,
    ) {
        window.set_results_saving(false);
        let failure = match &result {
            Err(message) => Some(message.clone()),
            Ok(report) => report.failure().map(|f| match &f.outcome {
                StatementOutcome::Failed { code, message } => format!(
                    "{} (in: {})",
                    explain(code.as_deref(), message),
                    first_line(&f.sql)
                ),
                StatementOutcome::Done { .. } => String::new(),
            }),
        };
        if let Some(message) = failure {
            if joined_txn {
                window.set_results_error(
                    format!("Not saved, and your open transaction has failed: {message}").into(),
                );
            } else {
                self.backend
                    .run(target, vec!["ROLLBACK".to_owned()], Box::new(|_| {}));
                window.set_results_error(format!("Nothing was saved: {message}").into());
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
        let saved = self.result_pending();
        self.forget_history(Which::Query);
        if let Some(edit) = self.result_edit.borrow_mut().as_mut() {
            edit.edits = EditSet::default();
        }
        self.publish_target(window);
        self.toast(
            window,
            &format!("Saved {saved} row{}", if saved == 1 { "" } else { "s" }),
            false,
        );
        self.result_reread(window);
    }

    /// Run the result's statement again (only that one, not the rest of the script) so the
    /// grid shows what the table holds now.
    fn result_reread(&self, window: &AppWindow) {
        let Some(sql) = self.query_sql.borrow().clone() else {
            return;
        };
        let Some(target) = self.current_target() else {
            return;
        };
        let weak = window.as_weak();
        let tab = self.active_tab.get();
        self.backend.run(
            target,
            vec![sql],
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    if c.active_tab.get() != tab {
                        return;
                    }
                    if let Ok(report) = result
                        && let Some(fresh) = report.last_result()
                    {
                        let fresh: ResultSet = fresh.clone();
                        c.query_result.replace(Some(fresh));
                        c.render_query_grid(w);
                    }
                });
            }),
        );
    }
}

/// A reason as a sentence: capital first letter, full stop.
fn sentence(why: &str) -> String {
    let mut chars = why.chars();
    chars.next().map_or_else(String::new, |first| {
        format!("{}{}.", first.to_uppercase(), chars.as_str())
    })
}

/// Undo history of one grid's staged edits: earlier states, and states undone.
#[derive(Debug, Default)]
pub(super) struct History {
    back: Vec<EditSet>,
    forward: Vec<EditSet>,
}

/// Most steps an undo history keeps.
const HISTORY_DEPTH: usize = 200;

impl Controller {
    // ── Hidden columns ──────────────────────────────────────────────────────

    /// Give hidden columns no width and lay the rest out again; returns the total width.
    pub(super) fn hide_columns(
        &self,
        window: &AppWindow,
        which: Which,
        columns: &mut [GridColumn],
    ) -> f32 {
        let hidden = self
            .col_hidden
            .borrow()
            .get(&self.width_scope(which))
            .cloned()
            .unwrap_or_default();
        let mut offset = 0.0;
        let mut any = false;
        for column in columns.iter_mut() {
            if hidden.contains(column.name.as_str()) {
                column.width = 0.0;
                any = true;
            }
            column.offset = offset;
            offset += column.width;
        }
        match which {
            Which::Query => window.set_grid_has_hidden(any),
            Which::Data => window.set_data_has_hidden(any),
        }
        offset
    }

    /// The column under `x` (content coordinates, gutter excluded), skipping hidden ones.
    pub(super) fn col_at(window: &AppWindow, which: Which, x: f32) -> i32 {
        let (columns, _) = Self::grid_models(window, which);
        let mut last = 0;
        for i in 0..columns.row_count() {
            let Some(c) = columns.row_data(i) else {
                continue;
            };
            if c.width <= 0.0 {
                continue;
            }
            last = i;
            if x < c.offset + c.width {
                return i32::try_from(i).unwrap_or(0);
            }
        }
        i32::try_from(last).unwrap_or(0)
    }

    // ── Block actions ───────────────────────────────────────────────────────

    /// The visible columns from `c1` to `c2`.
    fn block_columns(window: &AppWindow, which: Which, c1: usize, c2: usize) -> Vec<usize> {
        let (columns, _) = Self::grid_models(window, which);
        (c1..=c2)
            .filter(|c| columns.row_data(*c).is_some_and(|col| col.width > 0.0))
            .collect()
    }

    /// Names, types and values (staged edits included) of a block.
    #[allow(clippy::type_complexity)]
    fn block(
        &self,
        window: &AppWindow,
        which: Which,
        (r1, c1, r2, c2): (usize, usize, usize, usize),
    ) -> (Vec<String>, Vec<String>, Vec<Vec<Option<String>>>) {
        let cols = Self::block_columns(window, which, c1, c2);
        let mut names = Vec::new();
        let mut kinds = Vec::new();
        let mut rows = Vec::new();
        for r in r1..=r2 {
            let mut row = Vec::new();
            for &c in &cols {
                let cell = match which {
                    Which::Query => self.query_value(r, c).map(|(n, k, v, _)| (n, k, v)),
                    Which::Data => self.data_value(r, c).map(|(n, k, v, _, _)| (n, k, v)),
                };
                let Some((name, kind, value)) = cell else {
                    continue;
                };
                if r == r1 {
                    names.push(name);
                    kinds.push(kind);
                }
                row.push(value);
            }
            if !row.is_empty() {
                rows.push(row);
            }
        }
        (names, kinds, rows)
    }

    /// A shortcut or menu action of a grid on the block r1,c1 – r2,c2.
    pub(super) fn grid_action(
        &self,
        window: &AppWindow,
        which: Which,
        name: &str,
        block: [i32; 4],
    ) {
        match name {
            "undo" | "redo" => return self.undo(window, which, name == "redo"),
            "show-all" => {
                self.col_hidden
                    .borrow_mut()
                    .remove(&self.width_scope(which));
                return self.rerender(window, which);
            },
            _ => {},
        }
        let [r1, c1, r2, c2] = block.map(|v| usize::try_from(v).ok());
        if let Some(c) = c1
            && matches!(name, "sort-asc" | "sort-desc" | "copy-name")
        {
            return self.header_action(window, which, name, c);
        }
        if name == "hide" {
            let (columns, _) = Self::grid_models(window, which);
            let visible = (0..columns.row_count())
                .filter(|i| columns.row_data(*i).is_some_and(|c| c.width > 0.0))
                .count();
            if let Some(col) = c1.and_then(|c| columns.row_data(c))
                && visible > 1
            {
                self.col_hidden
                    .borrow_mut()
                    .entry(self.width_scope(which))
                    .or_default()
                    .insert(col.name.to_string());
                self.rerender(window, which);
            }
            return;
        }
        let (Some(r1), Some(c1), Some(r2), Some(c2)) = (r1, c1, r2, c2) else {
            return;
        };
        match name {
            "copy" | "copy-headers" => {
                let (names, _, rows) = self.block(window, which, (r1, c1, r2, c2));
                let mut text = crate::export::tsv(&names, &rows);
                if name == "copy" {
                    // Without the header line.
                    text = text
                        .split_once('\n')
                        .map_or_else(String::new, |(_, rest)| rest.to_owned());
                    if rows.len() == 1 && names.len() == 1 {
                        // One cell: its whole value, line breaks and all.
                        text = rows[0][0].clone().unwrap_or_default();
                    }
                }
                window.invoke_copy_text(text.into());
                let cells = rows.len() * names.len();
                self.toast(
                    window,
                    &if cells == 1 {
                        format!("Copied {}", names.first().cloned().unwrap_or_default())
                    } else {
                        format!("Copied {} rows × {} columns", rows.len(), names.len())
                    },
                    false,
                );
            },
            "copy-markdown" => {
                let (names, _, rows) = self.block(window, which, (r1, c1, r2, c2));
                let mut out = Vec::new();
                if let Ok(mut w) = crate::export::Writer::new(
                    &mut out,
                    crate::export::Format::Markdown,
                    names,
                    Vec::new(),
                ) {
                    for row in &rows {
                        let _ = w.row(row);
                    }
                    let _ = w.finish();
                }
                window.invoke_copy_text(String::from_utf8_lossy(&out).into_owned().into());
                self.toast(window, "Copied as a Markdown table", false);
            },
            "copy-insert" => {
                // Whole rows: an INSERT of one column is rarely wanted.
                let last = Self::grid_models(window, which)
                    .0
                    .row_count()
                    .saturating_sub(1);
                let (names, kinds, rows) = self.block(window, which, (r1, 0, r2, last));
                let table = self.grid_table(which);
                let cols = names
                    .iter()
                    .map(|n| quote_ident(n))
                    .collect::<Vec<_>>()
                    .join(", ");
                let lines: Vec<String> = rows
                    .iter()
                    .map(|row| {
                        let values = row
                            .iter()
                            .zip(&kinds)
                            .map(|(v, k)| insert_literal(v.as_deref(), k))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("INSERT INTO {table} ({cols}) VALUES ({values});")
                    })
                    .collect();
                window.invoke_copy_text(lines.join("\n").into());
                self.toast(
                    window,
                    &format!(
                        "Copied {} INSERT statement{}",
                        lines.len(),
                        if lines.len() == 1 { "" } else { "s" }
                    ),
                    false,
                );
            },
            "set-null" | "revert" => {
                let cols = Self::block_columns(window, which, c1, c2);
                let null = name == "set-null";
                self.stage(window, which, |edits, loaded| {
                    for r in r1..=r2 {
                        for &c in &cols {
                            if null {
                                edits.set(loaded, r, c, edits::Value::Null);
                            } else {
                                edits.updates.remove(&(r, c));
                            }
                        }
                    }
                });
            },
            "filter" if which == Which::Data => {
                let Some((column, _, value, _, _)) = self.data_value(r1, c1) else {
                    return;
                };
                let filter = match value {
                    None => format!("{} IS NULL", quote_ident(&column)),
                    Some(v) => format!("{} = '{}'", quote_ident(&column), v.replace('\'', "''")),
                };
                window.set_data_filter(filter.as_str().into());
                self.data_filter(window, &filter);
            },
            _ => {},
        }
    }

    /// The table a grid's rows belong to, for INSERT statements.
    fn grid_table(&self, which: Which) -> String {
        match which {
            Which::Data => self.data.borrow().as_ref().map_or_else(
                || "result".to_owned(),
                |d| format!("{}.{}", quote_ident(&d.schema), quote_ident(&d.name)),
            ),
            Which::Query => self.result_edit.borrow().as_ref().map_or_else(
                || "result".to_owned(),
                |e| format!("{}.{}", quote_ident(&e.schema), quote_ident(&e.table)),
            ),
        }
    }

    /// ⌘V / Ctrl+V: tab-separated text pasted from cell (r, c), into the visible columns to
    /// its right and the rows below. `NULL` pastes as NULL.
    pub(super) fn paste(&self, window: &AppWindow, which: Which, r: usize, c: usize, text: &str) {
        let editable = match which {
            Which::Data => self.data_edit_shape().is_ok(),
            Which::Query => self.result_edit.borrow().is_some(),
        };
        if !editable || text.is_empty() {
            return;
        }
        let (columns, rows) = Self::grid_models(window, which);
        let row_count = rows.row_count();
        let cols: Vec<usize> = (c..columns.row_count())
            .filter(|i| columns.row_data(*i).is_some_and(|col| col.width > 0.0))
            .collect();
        let lines: Vec<&str> = text.trim_end_matches(['\n', '\r']).split('\n').collect();
        let mut cut = 0usize;
        let mut cells = Vec::new();
        for (dr, line) in lines.iter().enumerate() {
            let row = r + dr;
            if row >= row_count {
                cut += 1;
                continue;
            }
            for (dc, value) in line.trim_end_matches('\r').split('\t').enumerate() {
                let Some(&col) = cols.get(dc) else { break };
                let value = if value == "NULL" {
                    edits::Value::Null
                } else {
                    edits::Value::Text(value.to_owned())
                };
                cells.push((row, col, value));
            }
        }
        let pasted = cells.len();
        self.stage(window, which, move |edits, loaded| {
            for (row, col, value) in cells {
                edits.set(loaded, row, col, value);
            }
        });
        if cut > 0 {
            self.toast(
                window,
                &format!(
                    "Pasted {pasted} cells. {cut} row{} past the last row were left out.",
                    if cut == 1 { "" } else { "s" }
                ),
                true,
            );
        }
    }

    /// Stage a change to a grid's edits as one undo step.
    fn stage(
        &self,
        window: &AppWindow,
        which: Which,
        change: impl FnOnce(&mut EditSet, &[Vec<Option<String>>]),
    ) {
        match which {
            Which::Data => self.edit_data(window, |edits, loaded, _| change(edits, loaded)),
            Which::Query => {
                let loaded = {
                    let result = self.query_result.borrow();
                    let Some(result) = result.as_ref() else {
                        return;
                    };
                    sorted(result, self.query_sort.get()).rows
                };
                {
                    let mut edit = self.result_edit.borrow_mut();
                    let Some(edit) = edit.as_mut() else { return };
                    self.remember(Which::Query, &edit.edits);
                    change(&mut edit.edits, &loaded);
                }
                self.publish_result_edits(window);
            },
        }
    }

    // ── Undo ────────────────────────────────────────────────────────────────

    /// Keep `before` as the state an undo returns to; a new change forgets what was undone.
    pub(super) fn remember(&self, which: Which, before: &EditSet) {
        let mut all = self.undo_history.borrow_mut();
        let h = all.entry(which).or_default();
        h.back.push(before.clone());
        if h.back.len() > HISTORY_DEPTH {
            h.back.remove(0);
        }
        h.forward.clear();
    }

    pub(super) fn forget_history(&self, which: Which) {
        self.undo_history.borrow_mut().remove(&which);
    }

    fn undo(&self, window: &AppWindow, which: Which, redo: bool) {
        let current = match which {
            Which::Data => self.data.borrow().as_ref().map(|d| d.edits.clone()),
            Which::Query => self.result_edit.borrow().as_ref().map(|e| e.edits.clone()),
        };
        let Some(current) = current else { return };
        let target = {
            let mut all = self.undo_history.borrow_mut();
            let h = all.entry(which).or_default();
            let (from, to) = if redo {
                (&mut h.forward, &mut h.back)
            } else {
                (&mut h.back, &mut h.forward)
            };
            let Some(target) = from.pop() else {
                drop(all);
                self.toast(
                    window,
                    if redo {
                        "Nothing to redo"
                    } else {
                        "Nothing to undo"
                    },
                    false,
                );
                return;
            };
            to.push(current);
            target
        };
        match which {
            Which::Data => {
                if let Some(d) = self.data.borrow_mut().as_mut() {
                    d.edits = target;
                }
                self.render_data(window);
                self.publish_data(window);
            },
            Which::Query => {
                if let Some(e) = self.result_edit.borrow_mut().as_mut() {
                    e.edits = target;
                }
                self.publish_result_edits(window);
            },
        }
    }

    /// Draw a grid again after its columns changed (hidden or shown).
    fn rerender(&self, window: &AppWindow, which: Which) {
        match which {
            Which::Query => {
                let (row, col) = (
                    window.get_grid_selected_row(),
                    window.get_grid_selected_col(),
                );
                self.render_query_grid(window);
                window.set_grid_selected_row(row);
                window.set_grid_selected_col(col);
            },
            Which::Data => self.render_data(window),
        }
    }
}

/// A value as an SQL literal for an INSERT: numbers bare, everything else quoted.
fn insert_literal(value: Option<&str>, kind: &str) -> String {
    match value {
        None => "NULL".to_owned(),
        Some(v) if is_numeric(kind) && !v.is_empty() => v.to_owned(),
        Some(v) => format!("'{}'", v.replace('\'', "''")),
    }
}

/// Columns the user hid, per grid scope.
pub(super) type Hidden = HashMap<String, HashSet<String>>;

impl Controller {
    /// Header menu: order by a column, or copy its name.
    fn header_action(&self, window: &AppWindow, which: Which, name: &str, c: usize) {
        let (columns, _) = Self::grid_models(window, which);
        let Some(column) = columns.row_data(c) else {
            return;
        };
        if name == "copy-name" {
            window.invoke_copy_text(column.name.clone());
            self.toast(window, &format!("Copied {}", column.name), false);
            return;
        }
        let desc = name == "sort-desc";
        match which {
            Which::Query => {
                if self.result_has_unsaved(window) {
                    return;
                }
                self.query_sort.set(Some((c, desc)));
                self.render_query_grid(window);
            },
            Which::Data => {
                if self.data_has_unsaved(window) {
                    return;
                }
                if let Some(state) = self.data.borrow_mut().as_mut() {
                    state.sort = Some((column.name.to_string(), desc));
                    state.page = 0;
                }
                self.load_data(window);
            },
        }
    }
}
