//! The Profile and Vectors tabs of a table's page on the Data screen.
//!
//! Profile: per-column statistics, common values, histograms and correlations, computed by the
//! server (`crate::profile`). Vectors: what the `VECTOR` columns hold, which metrics have an HNSW
//! index (read from `EXPLAIN`, the catalog does not record it), nearest-neighbour and hybrid
//! search, and creating an index (`crate::vector`).

use std::collections::HashMap;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{AppWindow, Controller, StatementOutcome, grid, on_ui};
use crate::backend::{ColumnDef, ObjectKind, ResultSet, RunReport};
use crate::profile::{self, Column, ColumnStats, Family, Step};
use crate::tree::{Load, Node};
use crate::ui::{
    CorrCell, CorrRow, GridColumn, GridRow, ProfileBar, ProfileCard, ProfileStore, Property,
    VectorColumn, VectorStore,
};
use crate::vector::{self, Metric, Plan};

/// The table a tab works on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Subject {
    id: String,
    server: u32,
    database: String,
    schema: String,
    table: String,
}

#[derive(Debug, Clone)]
pub(super) struct ProfileRun {
    subject: Subject,
    columns: Vec<Column>,
    stats: Vec<ColumnStats>,
    corr_columns: Vec<usize>,
    corr: Vec<(usize, usize, Option<f64>)>,
    sample: u32,
    sql: Vec<String>,
    serial: u64,
}

#[derive(Debug, Clone)]
pub(super) struct VectorCol {
    name: String,
    dims: Option<u64>,
    rows: u64,
    non_null: u64,
    norm: Option<String>,
    norm_range: Option<(String, String)>,
    unit: u64,
    zero: u64,
    /// Index name per metric, in `Metric::ALL` order.
    indexes: [Option<String>; 4],
}

#[derive(Debug, Clone)]
pub(super) struct VectorState {
    subject: Subject,
    columns: Vec<VectorCol>,
    /// Columns shown beside the distance: everything but the vectors.
    shown: Vec<String>,
    text_columns: Vec<String>,
    key: Option<String>,
    result: Option<ResultSet>,
    last_sql: String,
    serial: u64,
}

/// What `run_steps` hands its results to.
type StepsDone =
    Box<dyn FnOnce(&Controller, &AppWindow, Vec<(Step, Result<ResultSet, String>)>) + Send>;

const METRIC_ORDER: [Metric; 4] = Metric::ALL;

fn metric_at(i: usize) -> Metric {
    METRIC_ORDER.get(i).copied().unwrap_or(Metric::Cosine)
}

fn statement_results(report: &RunReport) -> Vec<Result<ResultSet, String>> {
    report
        .statements
        .iter()
        .map(|s| match &s.outcome {
            StatementOutcome::Done { result, .. } => Ok(result.clone()),
            StatementOutcome::Failed { message, .. } => Err(message.clone()),
        })
        .collect()
}

fn family_tag(f: Family) -> &'static str {
    match f {
        Family::Number => "number",
        Family::Text => "text",
        Family::Time => "time",
        Family::Bool => "bool",
        Family::Vector => "vector",
        Family::Other => "other",
    }
}

fn property(label: &str, value: impl Into<SharedString>) -> Property {
    Property {
        label: label.into(),
        value: value.into(),
        mono: true,
        note: SharedString::new(),
    }
}

fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn card(s: &ColumnStats) -> ProfileCard {
    #[allow(clippy::cast_precision_loss)]
    let null_share = if s.rows == 0 {
        0.0
    } else {
        s.nulls() as f64 / s.rows as f64
    };
    let mut stats = Vec::new();
    stats.push(property("Rows", group(s.rows)));
    if let Some(d) = s.distinct {
        stats.push(property("Distinct", group(d)));
    }
    if let Some(dims) = s.dims {
        stats.push(property(
            "Dimensions",
            if dims.0 == dims.1 {
                dims.0.to_string()
            } else {
                format!("{} – {}", dims.0, dims.1)
            },
        ));
    }
    if let Some(v) = &s.avg_norm {
        stats.push(property("Avg norm", v.as_str()));
    }
    if let Some(v) = &s.min {
        stats.push(property("Min", v.as_str()));
    }
    if let Some(v) = &s.max {
        stats.push(property("Max", v.as_str()));
    }
    if let Some(v) = &s.mean {
        stats.push(property("Mean", v.as_str()));
    }
    if let Some(v) = &s.stddev {
        stats.push(property("Std dev", v.as_str()));
    }
    if let Some([a, b, c]) = &s.quartiles {
        stats.push(property("Median", b.as_str()));
        stats.push(property("P25 / P75", format!("{a} / {c}")));
    }
    if let Some(v) = &s.avg_length {
        stats.push(property("Avg length", v.as_str()));
    }
    if let Some((a, b)) = s.length_range {
        stats.push(property("Length", format!("{a} – {b}")));
    }
    if let Some(n) = s.empties.filter(|n| *n > 0) {
        stats.push(property("Empty text", group(n)));
    }
    if let Some(n) = s.zeros.filter(|n| *n > 0) {
        stats.push(property("Zeros", group(n)));
    }
    if let Some(n) = s.negatives.filter(|n| *n > 0) {
        stats.push(property("Negatives", group(n)));
    }
    if let Some(n) = s.outliers {
        stats.push(property(
            "Outliers",
            if n == 0 {
                "none (1.5 × IQR)".to_owned()
            } else {
                format!("{} (1.5 × IQR)", group(n))
            },
        ));
    }
    if s.unique() {
        stats.push(property("Unique", "yes (key candidate)"));
    }
    if let Some(e) = &s.error {
        stats.push(property("Error", e.as_str()));
    }
    let top_max = s.top.iter().map(|t| t.1).max().unwrap_or(1).max(1);
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let top: Vec<ProfileBar> = s
        .top
        .iter()
        .map(|(v, n)| ProfileBar {
            label: v.as_deref().unwrap_or("NULL").into(),
            value: group(*n).into(),
            share: (*n as f64 / top_max as f64) as f32,
        })
        .collect();
    let hist_max = s.histogram.iter().map(|h| h.2).max().unwrap_or(1).max(1);
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let histogram: Vec<ProfileBar> = s
        .histogram
        .iter()
        .map(|(lo, hi, n)| ProfileBar {
            label: format!("{} – {}", crate::chart::fmt(*lo), crate::chart::fmt(*hi)).into(),
            value: group(*n).into(),
            share: (*n as f64 / hist_max as f64) as f32,
        })
        .collect();
    #[allow(clippy::cast_possible_truncation)]
    ProfileCard {
        name: s.name.as_str().into(),
        kind: s.data_type.as_str().into(),
        family: family_tag(s.family).into(),
        nulls: if s.rows == 0 {
            "no rows".into()
        } else {
            format!("{} NULL ({:.0} %)", group(s.nulls()), null_share * 100.0).into()
        },
        nulls_share: null_share as f32,
        stats: ModelRc::new(VecModel::from(stats)),
        top: ModelRc::new(VecModel::from(top)),
        histogram: ModelRc::new(VecModel::from(histogram)),
        hist_from: s
            .histogram
            .first()
            .map(|h| crate::chart::fmt(h.0))
            .unwrap_or_default()
            .into(),
        hist_to: s
            .histogram
            .last()
            .map(|h| crate::chart::fmt(h.1))
            .unwrap_or_default()
            .into(),
    }
}

impl Controller {
    /// The table open on the Data screen, and its catalog columns once read.
    fn analysis_subject(&self) -> Option<(Subject, Vec<ColumnDef>, Vec<String>)> {
        let node = self.page.borrow().clone()?;
        let Node::Object {
            server,
            database,
            schema,
            name,
            kind,
        } = &node
        else {
            return None;
        };
        if !kind.has_rows() {
            return None;
        }
        let info = match self.info.borrow().get(&node.id()) {
            Some(Load::Ready(info)) => info.clone(),
            _ => return None,
        };
        let key = info
            .constraints
            .iter()
            .find(|c| c.kind == "PRIMARY KEY")
            .map(|c| c.columns.clone())
            .unwrap_or_default();
        Some((
            Subject {
                id: node.id(),
                server: *server,
                database: database.clone(),
                schema: schema.clone(),
                table: name.clone(),
            },
            info.columns,
            key,
        ))
    }

    /// Whether the open object has vector columns (the Vectors tab is offered then).
    pub(super) fn has_vectors(columns: &[ColumnDef]) -> bool {
        columns
            .iter()
            .any(|c| Family::of(&c.data_type) == Family::Vector)
    }

    /// Run `steps` in the background and hand back each step's result. A failed statement
    /// stops a run on the server, so the steps after it are sent again; every step gets an
    /// answer.
    fn run_steps(
        &self,
        window: &AppWindow,
        server: u32,
        database: &str,
        steps: Vec<(Step, String)>,
        done: StepsDone,
    ) {
        if steps.is_empty() {
            done(self, window, Vec::new());
            return;
        }
        let Some(target) = self.target_for(server, database) else {
            return;
        };
        let weak = window.as_weak();
        let database = database.to_owned();
        let sql: Vec<String> = steps.iter().map(|s| s.1.clone()).collect();
        self.backend.run(
            target,
            sql,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    let results = match result {
                        Ok(report) => statement_results(&report),
                        Err(message) => vec![Err(message)],
                    };
                    let mut out: Vec<(Step, Result<ResultSet, String>)> = Vec::new();
                    let mut rest = Vec::new();
                    for (i, (step, sql)) in steps.into_iter().enumerate() {
                        match results.get(i) {
                            Some(r) => out.push((step, r.clone())),
                            None => rest.push((step, sql)),
                        }
                    }
                    if rest.is_empty() {
                        done(c, w, out);
                        return;
                    }
                    c.run_steps(
                        w,
                        server,
                        &database,
                        rest,
                        Box::new(move |c, w, more| {
                            out.extend(more);
                            done(c, w, out);
                        }),
                    );
                });
            }),
        );
    }

    // ── Profile ─────────────────────────────────────────────────────────────

    pub(super) fn profile_run(&self, window: &AppWindow, sample: i32) {
        let Some((subject, columns, _)) = self.analysis_subject() else {
            return;
        };
        let table = self.analysis_is_table();
        let sample = if table {
            u32::try_from(sample).unwrap_or(0)
        } else {
            0
        };
        let columns: Vec<Column> = columns
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                data_type: c.data_type.clone(),
            })
            .collect();
        let serial = self.analysis_serial.get().wrapping_add(1);
        self.analysis_serial.set(serial);
        let from = profile::source(
            &subject.schema,
            &subject.table,
            (sample > 0).then_some(sample),
        );
        let steps = profile::first_pass(&columns, &from);
        let run = ProfileRun {
            subject: subject.clone(),
            stats: columns.iter().map(ColumnStats::new).collect(),
            columns,
            corr_columns: Vec::new(),
            corr: Vec::new(),
            sample,
            sql: steps.iter().map(|s| s.1.clone()).collect(),
            serial,
        };
        self.profile.replace(Some(run));
        let store = window.global::<ProfileStore>();
        store.set_state("running".into());
        store.set_sample(i32::try_from(sample).unwrap_or(0));
        self.profile_steps(window, &subject, steps, serial, 1);
    }

    fn profile_steps(
        &self,
        window: &AppWindow,
        subject: &Subject,
        steps: Vec<(Step, String)>,
        serial: u64,
        pass: u8,
    ) {
        let Some(target) = self.target_for(subject.server, &subject.database) else {
            return;
        };
        let weak = window.as_weak();
        let subject = subject.clone();
        let sql: Vec<String> = steps.iter().map(|s| s.1.clone()).collect();
        self.backend.run(
            target,
            sql,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    c.profile_reply(w, &subject, steps, result, serial, pass);
                });
            }),
        );
    }

    fn profile_reply(
        &self,
        window: &AppWindow,
        subject: &Subject,
        steps: Vec<(Step, String)>,
        result: Result<RunReport, String>,
        serial: u64,
        pass: u8,
    ) {
        if self
            .profile
            .borrow()
            .as_ref()
            .is_none_or(|p| p.serial != serial)
        {
            return; // A newer profile started meanwhile.
        }
        let results = match result {
            Ok(report) => statement_results(&report),
            Err(message) => {
                let store = window.global::<ProfileStore>();
                store.set_state("error".into());
                store.set_message(message.into());
                return;
            },
        };
        let mut rest = Vec::new();
        {
            let mut guard = self.profile.borrow_mut();
            let Some(run) = guard.as_mut() else { return };
            for (i, (step, sql)) in steps.into_iter().enumerate() {
                let Some(r) = results.get(i) else {
                    rest.push((step, sql));
                    continue;
                };
                match (&step, r) {
                    (Step::Stats(c), Ok(set)) => run.stats[*c].read_stats(set),
                    (Step::Top(c), Ok(set)) => run.stats[*c].read_top(set),
                    (Step::Histogram(c), Ok(set)) => run.stats[*c].read_histogram(set),
                    (Step::Outliers(c), Ok(set)) => run.stats[*c].read_outliers(set),
                    (Step::Correlation(which), Ok(set)) => {
                        run.corr_columns.clone_from(which);
                        run.corr = profile::read_correlation(which, set);
                    },
                    (Step::Outliers(_), Err(_)) => {},
                    (Step::Stats(c) | Step::Top(c) | Step::Histogram(c), Err(e)) => {
                        run.stats[*c].error = Some(e.clone());
                    },
                    (Step::Correlation(_), Err(_)) => {},
                }
            }
        }
        // A failure stops a run part-way: the statements after it are sent again.
        if !rest.is_empty() {
            self.profile_steps(window, subject, rest, serial, pass);
            return;
        }
        if pass == 1 {
            let (second, from) = {
                let guard = self.profile.borrow();
                let Some(run) = guard.as_ref() else { return };
                let from = profile::source(
                    &run.subject.schema,
                    &run.subject.table,
                    (run.sample > 0).then_some(run.sample),
                );
                (profile::second_pass(&run.columns, &run.stats, &from), from)
            };
            let _ = from;
            if !second.is_empty() {
                if let Some(run) = self.profile.borrow_mut().as_mut() {
                    run.sql.extend(second.iter().map(|s| s.1.clone()));
                }
                self.profile_steps(window, subject, second, serial, 2);
                return;
            }
        }
        self.profile_publish(window);
    }

    fn analysis_is_table(&self) -> bool {
        matches!(
            self.page.borrow().as_ref(),
            Some(Node::Object {
                kind: ObjectKind::Table,
                ..
            })
        )
    }

    pub(super) fn profile_publish(&self, window: &AppWindow) {
        let store = window.global::<ProfileStore>();
        store.set_can_sample(self.analysis_is_table());
        let current = self
            .page
            .borrow()
            .as_ref()
            .map(Node::id)
            .unwrap_or_default();
        let guard = self.profile.borrow();
        let Some(run) = guard.as_ref().filter(|r| r.subject.id == current) else {
            store.set_state(SharedString::new());
            store.set_cards(ModelRc::default());
            store.set_corr_names(ModelRc::default());
            store.set_corr_rows(ModelRc::default());
            store.set_summary(SharedString::new());
            store.set_sample(0);
            return;
        };
        let cards: Vec<ProfileCard> = run.stats.iter().map(card).collect();
        store.set_cards(ModelRc::new(VecModel::from(cards)));
        let names: Vec<SharedString> = run
            .corr_columns
            .iter()
            .map(|i| run.columns[*i].name.as_str().into())
            .collect();
        let lookup: HashMap<(usize, usize), Option<f64>> = run
            .corr
            .iter()
            .flat_map(|(a, b, v)| [((*a, *b), *v), ((*b, *a), *v)])
            .collect();
        #[allow(clippy::cast_possible_truncation)]
        let rows: Vec<CorrRow> = run
            .corr_columns
            .iter()
            .map(|a| {
                let cells: Vec<CorrCell> = run
                    .corr_columns
                    .iter()
                    .map(|b| {
                        let v = if a == b {
                            Some(1.0)
                        } else {
                            lookup.get(&(*a, *b)).copied().flatten()
                        };
                        CorrCell {
                            text: v
                                .map_or_else(|| "–".to_owned(), |v| format!("{v:.2}"))
                                .into(),
                            value: v.unwrap_or(0.0) as f32,
                            valid: v.is_some(),
                        }
                    })
                    .collect();
                CorrRow {
                    name: run.columns[*a].name.as_str().into(),
                    cells: ModelRc::new(VecModel::from(cells)),
                }
            })
            .collect();
        store.set_corr_names(ModelRc::new(VecModel::from(names)));
        store.set_corr_rows(ModelRc::new(VecModel::from(rows)));
        let rows_read = run.stats.first().map_or(0, |s| s.rows);
        store.set_summary(
            format!(
                "{} rows read{}",
                group(rows_read),
                if run.sample > 0 {
                    format!(", {} % sample", run.sample)
                } else {
                    String::new()
                }
            )
            .into(),
        );
        store.set_sample(i32::try_from(run.sample).unwrap_or(0));
        store.set_state("ready".into());
    }

    pub(super) fn profile_copy_markdown(&self, window: &AppWindow) {
        let Some(run) = self.profile.borrow().clone() else {
            return;
        };
        let title = format!("{}.{}", run.subject.schema, run.subject.table);
        window.invoke_copy_text(profile::markdown(&run.stats, &title).into());
        self.toast(window, "Copied the profile as a Markdown table", false);
    }

    pub(super) fn profile_export_csv(&self, window: &AppWindow) {
        let Some(run) = self.profile.borrow().clone() else {
            return;
        };
        let name = format!("profile_{}.csv", run.subject.table);
        #[cfg(debug_assertions)]
        let debug = std::env::var_os("NUSADB_STUDIO_EXPORT_DIR")
            .map(|d| std::path::PathBuf::from(d).join(&name));
        #[cfg(not(debug_assertions))]
        let debug: Option<std::path::PathBuf> = None;
        let path = debug.or_else(|| {
            rfd::FileDialog::new()
                .set_title("Export profile")
                .set_file_name(&name)
                .add_filter("CSV", &["csv"])
                .save_file()
        });
        let Some(path) = path else {
            return;
        };
        match std::fs::write(&path, profile::csv(&run.stats)) {
            Ok(()) => self.toast(window, &format!("Saved {}", path.display()), false),
            Err(e) => self.toast(window, &format!("Could not save the profile: {e}"), true),
        }
    }

    pub(super) fn profile_open_sql(&self, window: &AppWindow) {
        let Some(run) = self.profile.borrow().clone() else {
            return;
        };
        let sql = run.sql.join(";\n\n") + ";\n";
        self.set_target(run.subject.server, &run.subject.database);
        self.new_tab(
            window,
            Some(&format!("profile {}", run.subject.table)),
            &sql,
        );
        window.set_screen("workspace".into());
        self.publish(window);
    }

    // ── Vectors ─────────────────────────────────────────────────────────────

    /// The Vectors tab was opened (or the table changed): read the columns and their indexes.
    pub(super) fn vectors_load(&self, window: &AppWindow) {
        let Some((subject, columns, key)) = self.analysis_subject() else {
            return;
        };
        if self
            .vectors
            .borrow()
            .as_ref()
            .is_some_and(|v| v.subject == subject)
        {
            self.vectors_publish(window);
            return;
        }
        let serial = self.analysis_serial.get().wrapping_add(1);
        self.analysis_serial.set(serial);
        let vector_cols: Vec<String> = columns
            .iter()
            .filter(|c| Family::of(&c.data_type) == Family::Vector)
            .map(|c| c.name.clone())
            .collect();
        let state = VectorState {
            subject: subject.clone(),
            columns: vector_cols
                .iter()
                .map(|n| VectorCol {
                    name: n.clone(),
                    dims: None,
                    rows: 0,
                    non_null: 0,
                    norm: None,
                    norm_range: None,
                    unit: 0,
                    zero: 0,
                    indexes: [None, None, None, None],
                })
                .collect(),
            shown: columns
                .iter()
                .filter(|c| Family::of(&c.data_type) != Family::Vector)
                .map(|c| c.name.clone())
                .collect(),
            text_columns: columns
                .iter()
                .filter(|c| Family::of(&c.data_type) == Family::Text)
                .map(|c| c.name.clone())
                .collect(),
            key: (key.len() == 1).then(|| key[0].clone()),
            result: None,
            last_sql: String::new(),
            serial,
        };
        self.vectors.replace(Some(state));
        let store = window.global::<VectorStore>();
        store.set_column(0);
        store.set_text_column(-1);
        store.set_plan(SharedString::new());
        store.set_summary(SharedString::new());
        store.set_error(SharedString::new());
        store.set_result_columns(ModelRc::default());
        store.set_result_rows(ModelRc::default());
        store.set_busy(true);
        self.vectors_publish(window);
        let t = profile::source(&subject.schema, &subject.table, None);
        let steps: Vec<(Step, String)> = vector_cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let q = crate::backend::quote_ident(c);
                (
                    Step::Stats(i),
                    format!(
                        "SELECT count(*), count({q}), max(vector_dims({q})), avg(vector_norm({q}))::text, \
                         min(vector_norm({q})), max(vector_norm({q})), \
                         count(CASE WHEN abs(vector_norm({q}) - 1) < 0.001 THEN 1 END), \
                         count(CASE WHEN vector_norm({q}) = 0 THEN 1 END) FROM {t}"
                    ),
                )
            })
            .collect();
        let probe_subject = subject.clone();
        self.run_steps(
            window,
            subject.server,
            &subject.database,
            steps,
            Box::new(move |c, w, results| {
                if c.vectors
                    .borrow()
                    .as_ref()
                    .is_none_or(|v| v.serial != serial)
                {
                    return;
                }
                let mut probes = Vec::new();
                if let Some(state) = c.vectors.borrow_mut().as_mut() {
                    for (step, r) in &results {
                        let (Step::Stats(i), Ok(set)) = (step, r) else {
                            continue;
                        };
                        let Some(row) = set.rows.first() else {
                            continue;
                        };
                        let n = |k: usize| {
                            row.get(k)
                                .cloned()
                                .flatten()
                                .and_then(|v| v.parse::<u64>().ok())
                        };
                        let col = &mut state.columns[*i];
                        col.rows = n(0).unwrap_or(0);
                        col.non_null = n(1).unwrap_or(0);
                        col.dims = n(2);
                        col.norm = row.get(3).cloned().flatten().map(|v| profile::round(&v));
                        if let (Some(a), Some(b)) =
                            (row.get(4).cloned().flatten(), row.get(5).cloned().flatten())
                        {
                            col.norm_range = Some((profile::round(&a), profile::round(&b)));
                        }
                        col.unit = n(6).unwrap_or(0);
                        col.zero = n(7).unwrap_or(0);
                        // Which metrics have an HNSW index: EXPLAIN names the index it would use.
                        let zero = vector::literal(&vec![
                            0.0;
                            usize::try_from(col.dims.unwrap_or(1))
                                .unwrap_or(1)
                                .max(1)
                        ]);
                        for (m_at, m) in METRIC_ORDER.iter().enumerate() {
                            probes.push((
                                Step::Histogram(i * 4 + m_at),
                                format!(
                                    "EXPLAIN SELECT 1 FROM {} ORDER BY {} {} '{zero}' LIMIT 1",
                                    profile::source(
                                        &probe_subject.schema,
                                        &probe_subject.table,
                                        None
                                    ),
                                    crate::backend::quote_ident(&col.name),
                                    m.operator()
                                ),
                            ));
                        }
                    }
                }
                c.run_steps(
                    w,
                    probe_subject.server,
                    &probe_subject.database,
                    probes,
                    Box::new(move |c, w, results| {
                        if let Some(state) = c.vectors.borrow_mut().as_mut()
                            && state.serial == serial
                        {
                            for (step, r) in &results {
                                let (Step::Histogram(at), Ok(set)) = (step, r) else {
                                    continue;
                                };
                                let lines: Vec<String> = set
                                    .rows
                                    .iter()
                                    .filter_map(|r| r.first().cloned().flatten())
                                    .collect();
                                if let Plan::Index { name, .. } = vector::read_plan(&lines)
                                    && let Some(col) = state.columns.get_mut(at / 4)
                                {
                                    col.indexes[at % 4] = Some(name);
                                }
                            }
                        }
                        w.global::<VectorStore>().set_busy(false);
                        c.vectors_publish(w);
                    }),
                );
            }),
        );
    }

    pub(super) fn vectors_publish(&self, window: &AppWindow) {
        let store = window.global::<VectorStore>();
        let guard = self.vectors.borrow();
        let Some(state) = guard.as_ref() else {
            store.set_columns(ModelRc::default());
            return;
        };
        let columns: Vec<VectorColumn> = state
            .columns
            .iter()
            .map(|c| {
                let indexes: Vec<SharedString> = c
                    .indexes
                    .iter()
                    .zip(METRIC_ORDER)
                    // One slot per metric (cosine, L2, inner product, L1); "" where none.
                    .map(|(ix, m)| {
                        ix.as_ref()
                            .map_or_else(SharedString::new, |n| format!("{n} · {}", m.label()).into())
                    })
                    .collect();
                let indexed: Vec<bool> = c.indexes.iter().map(Option::is_some).collect();
                VectorColumn {
                    name: c.name.as_str().into(),
                    dims: c
                        .dims
                        .map_or_else(|| "?".to_owned(), |d| d.to_string())
                        .into(),
                    rows: if c.rows == c.non_null {
                        format!("{} rows", group(c.rows))
                    } else {
                        format!(
                            "{} rows, {} NULL",
                            group(c.rows),
                            group(c.rows - c.non_null)
                        )
                    }
                    .into(),
                    norm: c.norm.clone().unwrap_or_else(|| "–".to_owned()).into(),
                    detail: {
                        let mut parts = Vec::new();
                        if let Some((a, b)) = &c.norm_range {
                            parts.push(format!("norm {a} – {b}"));
                        }
                        if c.non_null > 0 {
                            parts.push(if c.unit == c.non_null {
                                "all unit length".to_owned()
                            } else {
                                format!("{} of {} unit length", group(c.unit), group(c.non_null))
                            });
                        }
                        if c.zero > 0 {
                            parts.push(format!("{} zero vectors", group(c.zero)));
                        }
                        parts.join("  ·  ").into()
                    },
                    indexes: ModelRc::new(VecModel::from(indexes)),
                    indexed: ModelRc::new(VecModel::from(indexed)),
                }
            })
            .collect();
        store.set_columns(ModelRc::new(VecModel::from(columns)));
        let texts: Vec<SharedString> = std::iter::once("Vector only")
            .chain(state.text_columns.iter().map(String::as_str))
            .map(SharedString::from)
            .collect();
        store.set_text_columns(ModelRc::new(VecModel::from(texts)));
        store.set_hybrid_ok(state.key.is_some());
        let labels: Vec<SharedString> = state.shown.iter().map(|t| t.as_str().into()).collect();
        store.set_label_columns(ModelRc::new(VecModel::from(labels)));
        if store.get_label_column() < 0
            && let Some(first_text) = state
                .shown
                .iter()
                .position(|s| state.text_columns.contains(s))
        {
            store.set_label_column(i32::try_from(first_text).unwrap_or(-1));
        }
        store.set_has_selection(window.get_data_selected_row() >= 0);
        // Default to the metric that has an index, so the first search is fast.
        if store.get_plan().is_empty()
            && let Some(col) = state
                .columns
                .get(usize::try_from(store.get_column()).unwrap_or(0))
            && let Some(m) = col.indexes.iter().position(Option::is_some)
        {
            store.set_metric(i32::try_from(m).unwrap_or(0));
        }
    }

    pub(super) fn vectors_from_row(&self, window: &AppWindow) {
        let store = window.global::<VectorStore>();
        let Some(column) = self.vectors.borrow().as_ref().and_then(|s| {
            s.columns
                .get(usize::try_from(store.get_column()).unwrap_or(0))
                .map(|c| c.name.clone())
        }) else {
            return;
        };
        let Ok(row) = usize::try_from(window.get_data_selected_row()) else {
            store.set_error("Select a row on the Data tab first.".into());
            return;
        };
        let value = self.data_result.borrow().as_ref().and_then(|r| {
            let at = r.columns.iter().position(|c| *c == column)?;
            r.rows.get(row)?.get(at)?.clone()
        });
        match value {
            Some(v) => {
                store.set_query(v.into());
                store.set_error(SharedString::new());
            },
            None => store.set_error("That row has no vector in this column.".into()),
        }
    }

    /// "Similar rows" on the Data tab: take the selected row's vector and search.
    pub(super) fn vectors_similar(&self, window: &AppWindow) {
        window.set_data_tab("vectors".into());
        self.publish_page(window);
        self.vectors_load(window);
        self.vectors_from_row(window);
        if !window.global::<VectorStore>().get_query().is_empty() {
            self.vectors_search(window);
        }
    }

    pub(super) fn vectors_search(&self, window: &AppWindow) {
        let store = window.global::<VectorStore>();
        let Some(state) = self.vectors.borrow().clone() else {
            return;
        };
        let Some(col) = state
            .columns
            .get(usize::try_from(store.get_column()).unwrap_or(0))
        else {
            return;
        };
        let query = match vector::normalise(&store.get_query()) {
            Ok(q) => q,
            Err(e) => {
                store.set_error(e.into());
                return;
            },
        };
        if let Some(d) = col.dims
            && usize::try_from(d).ok() != Some(query.len())
        {
            store.set_error(
                format!(
                    "{} has {d} dimensions; this vector has {}.",
                    col.name,
                    query.len()
                )
                .into(),
            );
            return;
        }
        let Ok(k) = store.get_k().trim().parse::<usize>() else {
            store.set_error("Top k must be a whole number.".into());
            return;
        };
        let k = k.clamp(1, 10_000);
        let ef = store.get_ef().trim().to_owned();
        let ef = if ef.is_empty() {
            None
        } else if let Ok(v) = ef.parse::<usize>() {
            Some(v)
        } else {
            store.set_error("ef_search must be a whole number, or empty for the default.".into());
            return;
        };
        let metric = metric_at(usize::try_from(store.get_metric()).unwrap_or(0));
        let text_column = usize::try_from(store.get_text_column())
            .ok()
            .and_then(|i| state.text_columns.get(i).cloned());
        let text = store.get_text_query().trim().to_owned();
        let limits = match Self::vector_limits(&store) {
            Ok(l) => l,
            Err(e) => {
                store.set_error(e.into());
                return;
            },
        };
        let mut knn: Vec<String> = Vec::new();
        if let Some(ef) = ef {
            knn.push(format!("SET hnsw_ef_search = {ef}"));
        }
        knn.push(vector::knn_select(
            &state.subject.schema,
            &state.subject.table,
            &col.name,
            &state.shown,
            metric,
            &query,
            k,
            &limits,
        ));
        let select = knn.last().cloned().unwrap_or_default();
        store.set_recall(SharedString::new());
        let final_sql = match (&text_column, &state.key) {
            (Some(tc), Some(key)) if !text.is_empty() => vector::hybrid_sql(
                &state.subject.schema,
                &state.subject.table,
                key,
                &col.name,
                &state.shown,
                metric,
                &query,
                tc,
                &text,
                k,
            ),
            (Some(_), None) if !text.is_empty() => {
                store.set_error("Hybrid search needs a single-column primary key.".into());
                return;
            },
            _ => select.clone(),
        };
        let mut statements: Vec<String> = knn[..knn.len() - 1].to_vec();
        statements.push(format!("EXPLAIN {select}"));
        statements.push(final_sql.clone());
        let mut shown_sql: Vec<String> = knn[..knn.len() - 1].to_vec();
        shown_sql.push(final_sql);
        if let Some(s) = self.vectors.borrow_mut().as_mut() {
            s.last_sql = shown_sql.join(";\n") + ";\n";
        }
        store.set_error(SharedString::new());
        store.set_busy(true);
        let Some(target) = self.target_for(state.subject.server, &state.subject.database) else {
            return;
        };
        let weak = window.as_weak();
        let started = std::time::Instant::now();
        let label = metric.label();
        let hybrid = text_column.is_some() && !text.is_empty();
        self.backend.run(
            target,
            statements,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    let store = w.global::<VectorStore>();
                    store.set_busy(false);
                    let report = match result {
                        Ok(r) => r,
                        Err(e) => {
                            store.set_error(e.into());
                            return;
                        },
                    };
                    let results = statement_results(&report);
                    if let Some(Err(e)) = results.iter().find(|r| r.is_err()) {
                        store.set_error(e.clone().into());
                        return;
                    }
                    let n = results.len();
                    let plan_lines: Vec<String> = results
                        .get(n.saturating_sub(2))
                        .and_then(|r| r.as_ref().ok())
                        .map(|set| {
                            set.rows
                                .iter()
                                .filter_map(|r| r.first().cloned().flatten())
                                .collect()
                        })
                        .unwrap_or_default();
                    match vector::read_plan(&plan_lines) {
                        Plan::Index { name, .. } => {
                            store.set_plan("index".into());
                            store.set_plan_text(format!("HNSW index {name}").into());
                        },
                        Plan::Exact => {
                            store.set_plan("exact".into());
                            store.set_plan_text(
                                format!("Exact scan: no HNSW index for {label}").into(),
                            );
                        },
                        Plan::Other => store.set_plan(SharedString::new()),
                    }
                    let Some(Ok(set)) = results.last() else {
                        return;
                    };
                    // Distances and scores to six decimals: enough to rank, short enough to read.
                    let mut set = set.clone();
                    if let Some(last) = set.columns.len().checked_sub(1) {
                        for row in &mut set.rows {
                            if let Some(Some(v)) = row.get_mut(last)
                                && let Ok(x) = v.parse::<f64>()
                            {
                                *v = profile::round(&format!("{x:.6}"));
                            }
                        }
                    }
                    let set = &set;
                    let (columns, width, rows) = grid(set, set.rows.len(), None, &HashMap::new());
                    store.set_result_columns(ModelRc::new(VecModel::<GridColumn>::from(columns)));
                    store.set_result_width(width);
                    store.set_result_rows(ModelRc::new(VecModel::<GridRow>::from(rows)));
                    store.set_summary(
                        format!(
                            "{} {} · {} ms",
                            set.rows.len(),
                            if hybrid { "fused matches" } else { "nearest" },
                            started.elapsed().as_millis()
                        )
                        .into(),
                    );
                    if let Some(s) = c.vectors.borrow_mut().as_mut() {
                        s.result = Some(set.clone());
                    }
                });
            }),
        );
    }

    pub(super) fn vectors_create_index(&self, window: &AppWindow, column: i32, metric: i32) {
        let Some(state) = self.vectors.borrow().clone() else {
            return;
        };
        let Some(col) = state.columns.get(usize::try_from(column).unwrap_or(0)) else {
            return;
        };
        let metric = metric_at(usize::try_from(metric).unwrap_or(0));
        let sql = vector::index_sql(
            &state.subject.schema,
            &state.subject.table,
            &col.name,
            metric,
        );
        let Some(target) = self.target_for(state.subject.server, &state.subject.database) else {
            return;
        };
        window.global::<VectorStore>().set_busy(true);
        let weak = window.as_weak();
        let name = col.name.clone();
        self.backend.run(
            target,
            vec![sql],
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    w.global::<VectorStore>().set_busy(false);
                    let failure = match &result {
                        Err(e) => Some(e.clone()),
                        Ok(r) => r.failure().map(|f| match &f.outcome {
                            StatementOutcome::Failed { message, .. } => message.clone(),
                            StatementOutcome::Done { .. } => String::new(),
                        }),
                    };
                    if let Some(e) = failure {
                        c.toast(w, &format!("The index was not created: {e}"), true);
                        return;
                    }
                    c.toast(
                        w,
                        &format!("HNSW index for {} on {name} created", metric.label()),
                        false,
                    );
                    // Re-read: the Indexes tab and the probe both change.
                    c.vectors.replace(None);
                    if let Some(node) = c.page.borrow().clone() {
                        c.info.borrow_mut().remove(&node.id());
                        c.load_info(w, &node);
                    }
                    w.global::<VectorStore>().set_plan(SharedString::new());
                });
            }),
        );
    }

    pub(super) fn vectors_open_sql(&self, window: &AppWindow) {
        let Some(state) = self.vectors.borrow().clone() else {
            return;
        };
        if state.last_sql.is_empty() {
            return;
        }
        self.set_target(state.subject.server, &state.subject.database);
        self.new_tab(
            window,
            Some(&format!("search {}", state.subject.table)),
            &state.last_sql,
        );
        window.set_screen("workspace".into());
        self.publish(window);
    }

    pub(super) fn vectors_copy(
        &self,
        window: &AppWindow,
        r1: i32,
        c1: i32,
        r2: i32,
        c2: i32,
        headers: bool,
    ) {
        let Some(set) = self
            .vectors
            .borrow()
            .as_ref()
            .and_then(|s| s.result.clone())
        else {
            return;
        };
        let (Ok(r1), Ok(r2), Ok(c1), Ok(c2)) = (
            usize::try_from(r1.min(r2)),
            usize::try_from(r1.max(r2)),
            usize::try_from(c1.min(c2)),
            usize::try_from(c1.max(c2)),
        ) else {
            return;
        };
        let cols: Vec<String> = set
            .columns
            .get(c1..=c2.min(set.columns.len().saturating_sub(1)))
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

    /// Kind check used by the page: only tables and views get the analysis tabs.
    pub(super) fn analysis_tabs(
        kind: ObjectKind,
        columns: &[ColumnDef],
    ) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        if kind.has_rows() {
            out.push(("profile", "Profile"));
            if Self::has_vectors(columns) {
                out.push(("vectors", "Vectors"));
            }
        }
        out
    }

    fn vector_limits(store: &VectorStore<'_>) -> Result<vector::Limits, String> {
        let max = store.get_max_distance().trim().to_owned();
        let max_distance = if max.is_empty() {
            None
        } else {
            Some(
                max.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or("Max distance must be a number, or empty.")?,
            )
        };
        Ok(vector::Limits {
            filter: store.get_filter().trim().to_owned(),
            max_distance,
            exact: false,
        })
    }

    /// Recall of the HNSW index for this search: how many of the exact k nearest rows the
    /// index found. Both queries run with the same filter and ef_search.
    pub(super) fn vectors_recall(&self, window: &AppWindow) {
        let store = window.global::<VectorStore>();
        let Some(state) = self.vectors.borrow().clone() else {
            return;
        };
        let Some(col) = state
            .columns
            .get(usize::try_from(store.get_column()).unwrap_or(0))
            .cloned()
        else {
            return;
        };
        let (Ok(query), Ok(k), Ok(mut limits)) = (
            vector::normalise(&store.get_query()),
            store.get_k().trim().parse::<usize>(),
            Self::vector_limits(&store),
        ) else {
            store.set_error("Fill in the vector and Top k first.".into());
            return;
        };
        let k = k.clamp(1, 10_000);
        let metric = metric_at(usize::try_from(store.get_metric()).unwrap_or(0));
        // Compare by the primary key when there is one, else by every shown column.
        let shown: Vec<String> = state
            .key
            .clone()
            .map_or_else(|| state.shown.clone(), |k| vec![k]);
        let mut statements = Vec::new();
        if let Ok(ef) = store.get_ef().trim().parse::<usize>() {
            statements.push(format!("SET hnsw_ef_search = {ef}"));
        }
        let hnsw = vector::knn_select(
            &state.subject.schema,
            &state.subject.table,
            &col.name,
            &shown,
            metric,
            &query,
            k,
            &limits,
        );
        limits.exact = true;
        let exact = vector::knn_select(
            &state.subject.schema,
            &state.subject.table,
            &col.name,
            &shown,
            metric,
            &query,
            k,
            &limits,
        );
        statements.push(format!("EXPLAIN {hnsw}"));
        statements.push(hnsw);
        statements.push(exact);
        let Some(target) = self.target_for(state.subject.server, &state.subject.database) else {
            return;
        };
        store.set_busy(true);
        let weak = window.as_weak();
        self.backend.run(
            target,
            statements,
            Box::new(move |result| {
                on_ui(&weak, move |_c, w| {
                    let store = w.global::<VectorStore>();
                    store.set_busy(false);
                    let results = match result {
                        Ok(r) => statement_results(&r),
                        Err(e) => {
                            store.set_error(e.into());
                            return;
                        },
                    };
                    if let Some(Err(e)) = results.iter().find(|r| r.is_err()) {
                        store.set_error(e.clone().into());
                        return;
                    }
                    let n = results.len();
                    let rows = |i: usize| -> Vec<String> {
                        results
                            .get(i)
                            .and_then(|r| r.as_ref().ok())
                            .map(|set| {
                                set.rows
                                    .iter()
                                    .map(|r| {
                                        r.iter()
                                            .take(r.len().saturating_sub(1))
                                            .map(|v| v.clone().unwrap_or_default())
                                            .collect::<Vec<_>>()
                                            .join("\u{1f}")
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    let plan: Vec<String> = results
                        .get(n.saturating_sub(3))
                        .and_then(|r| r.as_ref().ok())
                        .map(|set| {
                            set.rows
                                .iter()
                                .filter_map(|r| r.first().cloned().flatten())
                                .collect()
                        })
                        .unwrap_or_default();
                    let found = rows(n.saturating_sub(2));
                    let truth = rows(n.saturating_sub(1));
                    let hit = truth.iter().filter(|t| found.contains(t)).count();
                    let text = if truth.is_empty() {
                        "Recall: no rows to compare.".to_owned()
                    } else if !matches!(vector::read_plan(&plan), Plan::Index { .. }) {
                        format!(
                            "Recall {} / {}: no HNSW index for this metric, the search is exact.",
                            truth.len(),
                            truth.len()
                        )
                    } else {
                        #[allow(clippy::cast_precision_loss)]
                        let pct = hit as f64 / truth.len() as f64 * 100.0;
                        format!(
                            "Recall@{} {hit} / {} ({pct:.0} %){}",
                            truth.len(),
                            truth.len(),
                            if hit < truth.len() {
                                ": raise ef_search for more"
                            } else {
                                ""
                            }
                        )
                    };
                    store.set_recall(text.into());
                });
            }),
        );
    }

    pub(super) fn vectors_drop_index(&self, window: &AppWindow, column: i32, metric: i32) {
        let Some(state) = self.vectors.borrow().clone() else {
            return;
        };
        let Some(name) = state
            .columns
            .get(usize::try_from(column).unwrap_or(usize::MAX))
            .and_then(|c| {
                c.indexes
                    .get(usize::try_from(metric).unwrap_or(usize::MAX))
                    .cloned()
                    .flatten()
            })
        else {
            return;
        };
        self.set_target(state.subject.server, &state.subject.database);
        let sql = format!("DROP INDEX {}", crate::backend::quote_ident(&name));
        self.ask(
            window,
            super::Pending::DropVectorIndex(sql.clone()),
            &format!("Drop the HNSW index {name}?"),
            &[
                "Searches with this metric then compare every row.".to_owned(),
                sql,
            ],
            "Drop index",
        );
    }

    pub(super) fn vectors_drop_confirmed(&self, window: &AppWindow, sql: &str) {
        let Some(state) = self.vectors.borrow().clone() else {
            return;
        };
        let Some(target) = self.target_for(state.subject.server, &state.subject.database) else {
            return;
        };
        let weak = window.as_weak();
        self.backend.run(
            target,
            vec![sql.to_owned()],
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    let failure = match &result {
                        Err(e) => Some(e.clone()),
                        Ok(r) => r.failure().map(|f| match &f.outcome {
                            StatementOutcome::Failed { message, .. } => message.clone(),
                            StatementOutcome::Done { .. } => String::new(),
                        }),
                    };
                    if let Some(e) = failure {
                        c.toast(w, &format!("The index was not dropped: {e}"), true);
                        return;
                    }
                    c.toast(w, "Index dropped", false);
                    c.vectors.replace(None);
                    if let Some(node) = c.page.borrow().clone() {
                        c.info.borrow_mut().remove(&node.id());
                        c.load_info(w, &node);
                    }
                });
            }),
        );
    }

    // ── Embedding map ───────────────────────────────────────────────────────

    /// Read a random sample of the vectors (with the label column) and project it to 2D.
    pub(super) fn vectors_map(&self, window: &AppWindow) {
        let store = window.global::<VectorStore>();
        let Some(state) = self.vectors.borrow().clone() else {
            return;
        };
        let Some(col) = state
            .columns
            .get(usize::try_from(store.get_column()).unwrap_or(0))
            .cloned()
        else {
            return;
        };
        let label = usize::try_from(store.get_label_column())
            .ok()
            .and_then(|i| state.shown.get(i).cloned());
        let key = state.key.clone().or_else(|| state.shown.first().cloned());
        let q = crate::backend::quote_ident(&col.name);
        let mut select = vec![format!("{q}::text")];
        select.push(key.as_ref().map_or_else(
            || "NULL".to_owned(),
            |k| format!("{}::text", crate::backend::quote_ident(k)),
        ));
        select.push(label.as_ref().map_or_else(
            || "NULL".to_owned(),
            |l| format!("{}::text", crate::backend::quote_ident(l)),
        ));
        let sql = format!(
            "SELECT {} FROM {} WHERE {q} IS NOT NULL ORDER BY random() LIMIT {MAP_SAMPLE}",
            select.join(", "),
            profile::source(&state.subject.schema, &state.subject.table, None)
        );
        let Some(target) = self.target_for(state.subject.server, &state.subject.database) else {
            return;
        };
        store.set_map_state("loading".into());
        let weak = window.as_weak();
        let label_name = label.clone().unwrap_or_default();
        let key_name = key.unwrap_or_default();
        self.backend.run(
            target,
            vec![sql],
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    let store = w.global::<VectorStore>();
                    let set = match result.map(|r| statement_results(&r)) {
                        Ok(mut v) if matches!(v.first(), Some(Ok(_))) => {
                            v.remove(0).unwrap_or_else(|_| ResultSet {
                                columns: Vec::new(),
                                types: Vec::new(),
                                rows: Vec::new(),
                            })
                        },
                        Ok(v) => {
                            store.set_map_state("error".into());
                            store.set_map_note(
                                v.into_iter()
                                    .find_map(Result::err)
                                    .unwrap_or_default()
                                    .into(),
                            );
                            return;
                        },
                        Err(e) => {
                            store.set_map_state("error".into());
                            store.set_map_note(e.into());
                            return;
                        },
                    };
                    c.vectors_map_draw(w, &set, &label_name, &key_name);
                });
            }),
        );
    }

    fn vectors_map_draw(&self, window: &AppWindow, set: &ResultSet, label: &str, key: &str) {
        let store = window.global::<VectorStore>();
        let mut vectors: Vec<Vec<f64>> = Vec::new();
        let mut rows: Vec<(String, String, String)> = Vec::new();
        for r in &set.rows {
            let text = r.first().cloned().flatten().unwrap_or_default();
            let Some(v) = vector::parse(&text) else {
                continue;
            };
            vectors.push(v.into_iter().map(f64::from).collect());
            rows.push((
                text,
                r.get(1).cloned().flatten().unwrap_or_default(),
                r.get(2).cloned().flatten().unwrap_or_default(),
            ));
        }
        if vectors.len() < 3 {
            store.set_map_state("error".into());
            store.set_map_note("At least 3 vectors are needed for a map.".into());
            return;
        }
        let (coords, share) = crate::pca::project(&vectors);
        // Colour by label when it has few values; otherwise one colour.
        let mut groups: Vec<String> = Vec::new();
        for (_, _, l) in &rows {
            if !groups.contains(l) {
                groups.push(l.clone());
            }
        }
        let coloured = !label.is_empty() && groups.len() <= 8;
        #[allow(clippy::cast_possible_truncation)]
        let points: Vec<crate::ui::ChartPoint> = coords
            .iter()
            .zip(&rows)
            .map(|((x, y), (_, k, l))| crate::ui::ChartPoint {
                x: *x as f32,
                y: *y as f32,
                label: if label.is_empty() {
                    format!("{key} {k}")
                } else {
                    format!("{key} {k}  ·  {label}: {l}")
                }
                .into(),
                series: if coloured {
                    i32::try_from(groups.iter().position(|g| g == l).unwrap_or(0)).unwrap_or(0)
                } else {
                    0
                },
            })
            .collect();
        store.set_map_points(ModelRc::new(VecModel::from(points)));
        let legend: Vec<SharedString> = if coloured {
            groups
                .iter()
                .map(|g| SharedString::from(if g.is_empty() { "NULL" } else { g.as_str() }))
                .collect()
        } else {
            Vec::new()
        };
        store.set_map_legend(ModelRc::new(VecModel::from(legend)));
        store.set_map_note(
            format!(
                "{} random rows · PCA keeps {:.0} % of the variance{}",
                vectors.len(),
                (share[0] + share[1]) * 100.0,
                if !label.is_empty() && !coloured {
                    " · over 8 labels, one colour"
                } else {
                    ""
                }
            )
            .into(),
        );
        self.map_rows
            .replace(rows.into_iter().map(|r| r.0).collect());
        store.set_map_state("ready".into());
    }

    /// A point on the map was clicked: search for its neighbours.
    pub(super) fn vectors_map_pick(&self, window: &AppWindow, index: i32) {
        let Some(text) = usize::try_from(index)
            .ok()
            .and_then(|i| self.map_rows.borrow().get(i).cloned())
        else {
            return;
        };
        let store = window.global::<VectorStore>();
        store.set_query(text.into());
        store.set_view("search".into());
        self.vectors_search(window);
    }
}

/// Rows the embedding map samples.
const MAP_SAMPLE: usize = 1500;
