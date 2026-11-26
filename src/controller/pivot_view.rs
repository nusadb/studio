//! The Pivot tab under the editor: the showing result pivoted (see `crate::pivot`), shown in
//! the usual grid, copied like any grid, and offered as SQL for the server to compute.

use std::collections::HashMap;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{AppWindow, Controller, grid};
use crate::backend::{ResultSet, quote_ident};
use crate::chart::{self, Agg};
use crate::pivot;
use crate::ui::{GridColumn, GridRow, PivotStore};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PivotChoice {
    rows: Option<usize>,
    cols: Option<usize>,
    value: Option<usize>,
    agg: String,
}

fn index(i: Option<usize>) -> i32 {
    i.and_then(|v| i32::try_from(v).ok()).unwrap_or(-1)
}

impl Controller {
    pub(super) fn pivot_reset(&self, window: &AppWindow) {
        self.pivot.replace(PivotChoice {
            agg: "sum".to_owned(),
            ..PivotChoice::default()
        });
        self.pivot_result.replace(None);
        self.pivot_draw(window);
    }

    /// First visit: rows from the first text column, columns from the second, value from the
    /// first number.
    pub(super) fn pivot_opened(&self, window: &AppWindow) {
        let Some(result) = self.query_result.borrow().clone() else {
            return;
        };
        let mut c = self.pivot.borrow().clone();
        if c.rows.is_none() {
            let labels: Vec<usize> = (0..result.columns.len())
                .filter(|i| !chart::is_numeric(&result, *i))
                .collect();
            let numbers: Vec<usize> = (0..result.columns.len())
                .filter(|i| chart::is_numeric(&result, *i))
                .collect();
            c.rows = labels.first().copied().or(Some(0));
            c.cols = labels
                .get(1)
                .copied()
                .or_else(|| (0..result.columns.len()).find(|i| Some(*i) != c.rows));
            c.value = numbers
                .iter()
                .copied()
                .find(|i| Some(*i) != c.rows && Some(*i) != c.cols);
            if c.agg.is_empty() {
                c.agg = "sum".to_owned();
            }
            self.pivot.replace(c);
        }
        self.pivot_draw(window);
    }

    pub(super) fn pivot_set(&self, window: &AppWindow, what: &str, number: i32, text: &str) {
        {
            let mut c = self.pivot.borrow_mut();
            let pick = usize::try_from(number).ok();
            match what {
                "rows" => c.rows = pick,
                "cols" => c.cols = pick,
                "value" => c.value = pick,
                "agg" => c.agg = text.to_owned(),
                _ => {},
            }
        }
        self.pivot_draw(window);
    }

    pub(super) fn pivot_draw(&self, window: &AppWindow) {
        let store = window.global::<PivotStore>();
        let result = self.query_result.borrow().clone();
        let columns: Vec<SharedString> = result
            .as_ref()
            .map(|r| r.columns.iter().map(|c| c.as_str().into()).collect())
            .unwrap_or_default();
        store.set_columns(ModelRc::new(VecModel::from(columns)));
        let c = self.pivot.borrow().clone();
        store.set_rows(index(c.rows));
        store.set_cols(index(c.cols));
        store.set_value(index(c.value));
        store.set_agg(
            if c.agg.is_empty() {
                "sum"
            } else {
                c.agg.as_str()
            }
            .into(),
        );
        let built = match (&result, c.rows, c.cols) {
            (Some(r), Some(rows), Some(cols)) if rows != cols => {
                Some(pivot::pivot(r, rows, cols, c.value, Agg::from_id(&c.agg)))
            },
            _ => None,
        };
        match built {
            Some((table, note)) => {
                let (cols, width, rows) = grid(&table, table.rows.len(), None, &HashMap::new());
                store.set_grid_columns(ModelRc::new(VecModel::<GridColumn>::from(cols)));
                store.set_grid_width(width);
                store.set_grid_rows(ModelRc::new(VecModel::<GridRow>::from(rows)));
                store.set_note(note.into());
                self.pivot_result.replace(Some(table));
            },
            None => {
                store.set_grid_columns(ModelRc::default());
                store.set_grid_rows(ModelRc::default());
                store.set_note(
                    if result.is_some() {
                        "Pick two different columns for the rows and the columns."
                    } else {
                        "Run a query to pivot its rows."
                    }
                    .into(),
                );
                self.pivot_result.replace(None);
            },
        }
    }

    pub(super) fn pivot_copy(
        &self,
        window: &AppWindow,
        r1: i32,
        c1: i32,
        r2: i32,
        c2: i32,
        headers: bool,
    ) {
        let Some(set) = self.pivot_result.borrow().clone() else {
            return;
        };
        copy_block(window, &set, (r1, c1, r2, c2), headers);
    }

    /// The same pivot as SQL (`sum(CASE WHEN … END)` per column), to run on all rows on the
    /// server rather than on the rows fetched.
    pub(super) fn pivot_open_sql(&self, window: &AppWindow) {
        let (Some(result), Some(table)) = (
            self.query_result.borrow().clone(),
            self.pivot_result.borrow().clone(),
        ) else {
            return;
        };
        let c = self.pivot.borrow().clone();
        let (Some(rows), Some(cols)) = (c.rows, c.cols) else {
            return;
        };
        let Some(source) = self.query_sql.borrow().clone() else {
            return;
        };
        let agg = Agg::from_id(&c.agg);
        let value = c.value.map(|v| quote_ident(&result.columns[v]));
        let fold = |cond: &str| -> String {
            match (agg, &value) {
                (Agg::Count, _) | (_, None) => format!("count(CASE WHEN {cond} THEN 1 END)"),
                (a, Some(v)) => format!("{}(CASE WHEN {cond} THEN {v} END)", a.label()),
            }
        };
        let col = quote_ident(&result.columns[cols]);
        let mut parts = vec![quote_ident(&result.columns[rows])];
        for name in table
            .columns
            .iter()
            .skip(1)
            .take(table.columns.len().saturating_sub(2))
        {
            let cond = if name == "NULL" {
                format!("{col} IS NULL")
            } else {
                format!("{col}::text = '{}'", name.replace('\'', "''"))
            };
            parts.push(format!("{} AS {}", fold(&cond), quote_ident(name)));
        }
        parts.push(format!("{} AS \"Total\"", fold("true")));
        let sql = format!(
            "-- Pivot of the query below, computed by the server on every row.\nSELECT {}\nFROM (\n{}\n) AS q\nGROUP BY 1\nORDER BY 1;\n",
            parts.join(",\n       "),
            source.trim().trim_end_matches(';')
        );
        self.new_tab(window, Some("pivot"), &sql);
        self.publish(window);
    }
}

/// Copy rows `r1..=r2`, columns `c1..=c2` of `set`, tab-separated, optionally with headers.
pub(super) fn copy_block(
    window: &AppWindow,
    set: &ResultSet,
    (r1, c1, r2, c2): (i32, i32, i32, i32),
    headers: bool,
) {
    let (Ok(r1), Ok(r2), Ok(c1), Ok(c2)) = (
        usize::try_from(r1.min(r2)),
        usize::try_from(r1.max(r2)),
        usize::try_from(c1.min(c2)),
        usize::try_from(c1.max(c2)),
    ) else {
        return;
    };
    let last = set.columns.len().saturating_sub(1);
    let cols: Vec<String> = set
        .columns
        .get(c1..=c2.min(last))
        .map(<[String]>::to_vec)
        .unwrap_or_default();
    let rows: Vec<Vec<Option<String>>> = set
        .rows
        .iter()
        .skip(r1)
        .take(r2 - r1 + 1)
        .map(|r| r.iter().skip(c1).take(c2 - c1 + 1).cloned().collect())
        .collect();
    let text = if headers {
        crate::export::tsv(&cols, &rows)
    } else {
        rows.iter()
            .map(|r| {
                r.iter()
                    .map(|v| v.clone().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    window.invoke_copy_text(text.into());
}
