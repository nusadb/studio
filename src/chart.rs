//! The Chart tab of a query result: bar, line, scatter, histogram and pie, drawn from the rows
//! already fetched (nothing is re-run). Positions are fractions of the plot area (0 at the
//! left / bottom, 1 at the right / top) so the UI only scales them. An optional Series column
//! splits bars and lines into one colour per value of it.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::backend::ResultSet;

/// Bars (categories) drawn at most when no Top N is chosen; the rest go into "Other".
pub const MAX_BARS: usize = 40;
/// Series (colours) at most; further values are folded into "Other".
pub const MAX_SERIES: usize = 8;
pub const HISTOGRAM_BINS: usize = 20;

/// The categorical palette, light theme. Index `i % 8` per series; also used by the SVG export.
pub const PALETTE: [&str; MAX_SERIES] = [
    "#2E3A9E", "#D97706", "#0E7490", "#BE185D", "#4D7C0F", "#7C3AED", "#B45309", "#475569",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bar,
    Line,
    Scatter,
    Histogram,
    Pie,
}

impl Kind {
    #[must_use]
    pub fn from_id(id: &str) -> Self {
        match id {
            "line" => Self::Line,
            "scatter" => Self::Scatter,
            "histogram" => Self::Histogram,
            "pie" => Self::Pie,
            _ => Self::Bar,
        }
    }

    /// Whether the kind plots a second (Y) column.
    #[must_use]
    pub const fn uses_y(self) -> bool {
        !matches!(self, Self::Histogram)
    }

    /// Whether a Series column can split it.
    #[must_use]
    pub const fn uses_series(self) -> bool {
        matches!(self, Self::Bar | Self::Line | Self::Scatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// Scatter: every row is a point.
    None,
}

impl Agg {
    #[must_use]
    pub fn from_id(id: &str) -> Self {
        match id {
            "count" => Self::Count,
            "avg" => Self::Avg,
            "min" => Self::Min,
            "max" => Self::Max,
            "none" => Self::None,
            _ => Self::Sum,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Avg => "avg",
            Self::Min => "min",
            Self::Max => "max",
            Self::None => "",
        }
    }
}

/// Order of the categories along X (bar and pie; a line always runs in X order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// As the rows came.
    Result,
    /// Largest total first.
    Value,
    /// By label (numbers as numbers).
    Label,
}

impl Order {
    #[must_use]
    pub fn from_id(id: &str) -> Self {
        match id {
            "value" => Self::Value,
            "label" => Self::Label,
            _ => Self::Result,
        }
    }
}

/// What to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub kind: Kind,
    pub x: usize,
    pub y: Option<usize>,
    pub series: Option<usize>,
    pub agg: Agg,
    pub order: Order,
    /// Keep the largest N categories (the rest summed into "Other").
    pub top: Option<usize>,
}

impl Options {
    #[must_use]
    #[cfg(test)]
    pub const fn new(kind: Kind, x: usize, y: Option<usize>, agg: Agg) -> Self {
        Self {
            kind,
            x,
            y,
            series: None,
            agg,
            order: Order::Result,
            top: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    pub x0: f64,
    pub x1: f64,
    /// Bottom and height as fractions of the plot.
    pub y0: f64,
    pub h: f64,
    pub label: String,
    pub value: String,
    pub series: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
    pub label: String,
    pub series: usize,
}

/// A pie slice: an SVG path on a 200 × 200 box centred at (100, 100).
#[derive(Debug, Clone, PartialEq)]
pub struct Slice {
    pub path: String,
    pub label: String,
    pub value: String,
    pub share: f64,
    pub series: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tick {
    pub at: f64,
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Chart {
    pub bars: Vec<Bar>,
    pub points: Vec<Point>,
    pub slices: Vec<Slice>,
    /// Line charts join the points of each series in order.
    pub joined: bool,
    pub x_ticks: Vec<Tick>,
    pub y_ticks: Vec<Tick>,
    pub x_title: String,
    pub y_title: String,
    /// Legend: one name per series (empty for a single series).
    pub legend: Vec<String>,
    /// Why nothing is drawn, or what was left out.
    pub note: String,
    /// The numbers behind the chart, for "Copy data": header, then rows.
    pub table: Vec<Vec<String>>,
}

fn number(v: Option<&String>) -> Option<f64> {
    v.and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

/// Whether most non-null values of column `i` are numbers.
#[must_use]
pub fn is_numeric(result: &ResultSet, i: usize) -> bool {
    let mut seen = 0;
    let mut numbers = 0;
    for row in result.rows.iter().take(200) {
        if let Some(Some(v)) = row.get(i) {
            seen += 1;
            if v.trim().parse::<f64>().is_ok() {
                numbers += 1;
            }
        }
    }
    seen > 0 && numbers * 10 >= seen * 9
}

/// A short number for an axis or a bar: 1234567 → 1.23M, 0.000123 → 1.23e-4.
#[must_use]
pub fn fmt(v: f64) -> String {
    let a = v.abs();
    let s = if a >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.1}k", v / 1e3)
    } else if a == 0.0 || a >= 0.01 {
        format!("{v:.2}")
    } else {
        format!("{v:.2e}")
    };
    if s.contains('.') && !s.contains('e') {
        let (num, unit) = s.split_at(s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len()));
        format!("{}{unit}", num.trim_end_matches('0').trim_end_matches('.'))
    } else {
        s
    }
}

/// Round tick positions for the range `lo..hi` (about five of them).
fn nice_ticks(lo: f64, hi: f64) -> (f64, f64, Vec<f64>) {
    if hi.partial_cmp(&lo) != Some(std::cmp::Ordering::Greater) {
        let pad = if lo == 0.0 { 1.0 } else { lo.abs() * 0.1 };
        return nice_ticks(lo - pad, hi + pad);
    }
    let raw = (hi - lo) / 5.0;
    let mag = 10_f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .iter()
        .map(|m| m * mag)
        .find(|s| *s >= raw)
        .unwrap_or(10.0 * mag);
    let start = (lo / step).floor() * step;
    let end = (hi / step).ceil() * step;
    let mut ticks = Vec::new();
    let mut t = start;
    while t <= end + step * 0.5 {
        ticks.push(t);
        t += step;
    }
    (start, end, ticks)
}

fn y_axis(values: impl Iterator<Item = f64>, from_zero: bool) -> (f64, f64, Vec<Tick>) {
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for v in values {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if !lo.is_finite() {
        return (0.0, 1.0, Vec::new());
    }
    if from_zero {
        lo = lo.min(0.0);
        hi = hi.max(0.0);
    }
    let (start, end, ticks) = nice_ticks(lo, hi);
    let ticks = ticks
        .into_iter()
        .map(|t| Tick {
            at: (t - start) / (end - start),
            label: fmt(t),
        })
        .collect();
    (start, end, ticks)
}

/// Running fold of one group.
#[derive(Debug, Clone, Copy)]
struct Acc {
    sum: f64,
    n: u64,
    min: f64,
    max: f64,
}

impl Acc {
    const EMPTY: Self = Self {
        sum: 0.0,
        n: 0,
        min: f64::INFINITY,
        max: f64::NEG_INFINITY,
    };

    fn add(&mut self, v: f64) {
        self.sum += v;
        self.n += 1;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
    }

    #[allow(clippy::cast_precision_loss)]
    fn value(self, agg: Agg) -> Option<f64> {
        match agg {
            Agg::Count => Some(self.n as f64),
            Agg::Sum | Agg::None => Some(self.sum),
            _ if self.n == 0 => None,
            Agg::Avg => Some(self.sum / self.n as f64),
            Agg::Min => Some(self.min),
            Agg::Max => Some(self.max),
        }
    }
}

fn key(row: &[Option<String>], i: usize) -> String {
    row.get(i)
        .cloned()
        .flatten()
        .unwrap_or_else(|| "NULL".to_owned())
}

fn label_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x.total_cmp(&y),
        _ => a.cmp(b),
    }
}

/// Grouped values: categories (X) in their chosen order, series names, and the value of each
/// (category, series) pair.
struct Grouped {
    categories: Vec<String>,
    series: Vec<String>,
    values: HashMap<(usize, usize), f64>,
}

fn group(result: &ResultSet, o: &Options, agg: Agg) -> Grouped {
    let mut cat_order: Vec<String> = Vec::new();
    let mut cat_at: HashMap<String, usize> = HashMap::new();
    let mut ser_order: Vec<String> = Vec::new();
    let mut ser_at: HashMap<String, usize> = HashMap::new();
    let mut acc: HashMap<(usize, usize), Acc> = HashMap::new();
    for row in &result.rows {
        let c = key(row, o.x);
        let s = o.series.map_or_else(String::new, |i| key(row, i));
        let ci = *cat_at.entry(c.clone()).or_insert_with(|| {
            cat_order.push(c);
            cat_order.len() - 1
        });
        let si = *ser_at.entry(s.clone()).or_insert_with(|| {
            ser_order.push(s);
            ser_order.len() - 1
        });
        let entry = acc.entry((ci, si)).or_insert(Acc::EMPTY);
        match (agg, o.y) {
            (Agg::Count, _) | (_, None) => entry.add(1.0),
            (_, Some(y)) => {
                if let Some(v) = number(row.get(y).and_then(Option::as_ref)) {
                    entry.add(v);
                }
            },
        }
    }
    let mut values: HashMap<(usize, usize), f64> = acc
        .into_iter()
        .filter_map(|(k, a)| {
            a.value(if o.y.is_none() { Agg::Count } else { agg })
                .map(|v| (k, v))
        })
        .collect();

    // Series beyond MAX_SERIES are folded into "Other" (largest totals kept).
    let mut series_ix: Vec<usize> = (0..ser_order.len()).collect();
    if ser_order.len() > MAX_SERIES {
        let total = |s: usize| -> f64 {
            values
                .iter()
                .filter(|((_, si), _)| *si == s)
                .map(|(_, v)| *v)
                .sum()
        };
        series_ix.sort_by(|a, b| total(*b).total_cmp(&total(*a)));
        let keep: Vec<usize> = series_ix[..MAX_SERIES - 1].to_vec();
        let mut folded: HashMap<(usize, usize), f64> = HashMap::new();
        for ((ci, si), v) in values.drain() {
            let to = keep.iter().position(|k| *k == si).unwrap_or(MAX_SERIES - 1);
            *folded.entry((ci, to)).or_insert(0.0) += v;
        }
        values = folded;
        let mut names: Vec<String> = keep.iter().map(|k| ser_order[*k].clone()).collect();
        names.push("Other".to_owned());
        ser_order = names;
    }

    // Category order.
    let total = |ci: usize| -> f64 {
        values
            .iter()
            .filter(|((c, _), _)| *c == ci)
            .map(|(_, v)| *v)
            .sum()
    };
    let mut cats: Vec<usize> = (0..cat_order.len()).collect();
    let line = o.kind == Kind::Line;
    match (line, o.order) {
        (true, _) | (false, Order::Label) => {
            cats.sort_by(|a, b| label_cmp(&cat_order[*a], &cat_order[*b]));
        },
        (false, Order::Value) => cats.sort_by(|a, b| total(*b).total_cmp(&total(*a))),
        (false, Order::Result) => {},
    }
    // Top N (or the drawing limit): the rest summed into "Other".
    let limit = if line {
        None
    } else {
        Some(o.top.unwrap_or(MAX_BARS))
    };
    let mut categories: Vec<String> = Vec::new();
    let mut out: HashMap<(usize, usize), f64> = HashMap::new();
    let mut other: HashMap<usize, f64> = HashMap::new();
    let keep: Vec<usize> = match limit {
        Some(n) if cats.len() > n => {
            let mut by_value = cats.clone();
            by_value.sort_by(|a, b| total(*b).total_cmp(&total(*a)));
            let kept: Vec<usize> = by_value[..n].to_vec();
            cats.iter().copied().filter(|c| kept.contains(c)).collect()
        },
        _ => cats.clone(),
    };
    for (new_ci, ci) in keep.iter().enumerate() {
        categories.push(cat_order[*ci].clone());
        for si in 0..ser_order.len() {
            if let Some(v) = values.get(&(*ci, si)) {
                out.insert((new_ci, si), *v);
            }
        }
    }
    if keep.len() < cats.len() {
        for ci in cats.iter().filter(|c| !keep.contains(c)) {
            for si in 0..ser_order.len() {
                if let Some(v) = values.get(&(*ci, si)) {
                    *other.entry(si).or_insert(0.0) += v;
                }
            }
        }
        let at = categories.len();
        categories.push(format!("Other ({})", cats.len() - keep.len()));
        for (si, v) in other {
            out.insert((at, si), v);
        }
    }
    Grouped {
        categories,
        series: ser_order,
        values: out,
    }
}

/// Build the chart for `result` as `o` asks.
#[must_use]
pub fn build(result: &ResultSet, o: &Options) -> Chart {
    let name = |i: usize| result.columns.get(i).cloned().unwrap_or_default();
    let mut chart = Chart {
        x_title: name(o.x),
        ..Chart::default()
    };
    if result.rows.is_empty() || o.x >= result.columns.len() {
        chart.note = "No rows to draw.".to_owned();
        return chart;
    }
    match o.kind {
        Kind::Histogram => histogram(result, o.x, &mut chart),
        Kind::Scatter => scatter(result, o, &mut chart),
        Kind::Pie => pie(result, o, &mut chart),
        Kind::Bar | Kind::Line => categories(result, o, &mut chart),
    }
    chart
}

fn y_title(result: &ResultSet, o: &Options, agg: Agg) -> String {
    match o.y {
        Some(y) => format!(
            "{}({})",
            agg.label(),
            result.columns.get(y).cloned().unwrap_or_default()
        ),
        None => "count".to_owned(),
    }
}

fn categories(result: &ResultSet, o: &Options, chart: &mut Chart) {
    let agg = if o.y.is_none() { Agg::Count } else { o.agg };
    chart.y_title = y_title(result, o, agg);
    let g = group(result, o, agg);
    if g.values.is_empty() {
        chart.note = "The Y column has no numbers.".to_owned();
        return;
    }
    let multi = o.series.is_some();
    if multi {
        chart.legend.clone_from(&g.series);
    }
    let stacked_from_zero = o.kind == Kind::Bar;
    let (lo, hi, ticks) = y_axis(g.values.values().copied(), stacked_from_zero);
    chart.y_ticks = ticks;
    let scale = |v: f64| (v - lo) / (hi - lo);
    let base = scale(0.0_f64.clamp(lo, hi));
    #[allow(clippy::cast_precision_loss)]
    let n = g.categories.len() as f64;
    #[allow(clippy::cast_precision_loss)]
    let ns = g.series.len() as f64;
    let label_every = (g.categories.len() / 12).max(1);
    let mut header = vec![chart.x_title.clone()];
    if multi {
        header.extend(g.series.iter().cloned());
    } else {
        header.push(chart.y_title.clone());
    }
    chart.table.push(header);
    for (ci, label) in g.categories.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let ci_f = ci as f64;
        if ci % label_every == 0 {
            chart.x_ticks.push(Tick {
                at: (ci_f + 0.5) / n,
                label: label.clone(),
            });
        }
        let mut row = vec![label.clone()];
        for si in 0..g.series.len() {
            let value = g.values.get(&(ci, si)).copied();
            row.push(value.map(fmt).unwrap_or_default());
            let Some(value) = value else { continue };
            let series_name = if multi {
                format!("{} · ", g.series[si])
            } else {
                String::new()
            };
            #[allow(clippy::cast_precision_loss)]
            let si_f = si as f64;
            match o.kind {
                Kind::Bar => {
                    let slot = 0.76 / n / ns;
                    let left = (ci_f + 0.12) / n + slot * si_f;
                    let top = scale(value);
                    chart.bars.push(Bar {
                        x0: left,
                        x1: left + slot,
                        y0: top.min(base),
                        h: (top - base).abs(),
                        label: format!("{series_name}{label}"),
                        value: fmt(value),
                        series: si,
                    });
                },
                _ => chart.points.push(Point {
                    x: (ci_f + 0.5) / n,
                    y: scale(value),
                    label: format!("{series_name}{label}: {}", fmt(value)),
                    series: si,
                }),
            }
        }
        chart.table.push(row);
    }
    chart.joined = o.kind == Kind::Line;
    if chart.joined {
        // Each series' points together, left to right, so consecutive points form its line.
        chart
            .points
            .sort_by(|a, b| a.series.cmp(&b.series).then(a.x.total_cmp(&b.x)));
    }
    if g.categories.iter().any(|c| c.starts_with("Other (")) {
        chart.note = "Smaller groups are summed into Other.".to_owned();
    }
}

/// An SVG arc path for a slice from `a0` to `a1` (radians, 0 at the top, clockwise), as a donut
/// with outer radius 96 and inner radius 56 on a 200 × 200 box.
fn arc(a0: f64, a1: f64) -> String {
    let (r1, r0) = (96.0, 56.0);
    let p = |r: f64, a: f64| (100.0 + r * a.sin(), 100.0 - r * a.cos());
    // A full circle cannot be one arc: split it in two.
    if a1 - a0 >= std::f64::consts::TAU - 1e-9 {
        let mid = a0 + std::f64::consts::PI;
        return format!("{} {}", arc(a0, mid), arc(mid, a1));
    }
    let large = i32::from(a1 - a0 > std::f64::consts::PI);
    let (x0, y0) = p(r1, a0);
    let (x1, y1) = p(r1, a1);
    let (x2, y2) = p(r0, a1);
    let (x3, y3) = p(r0, a0);
    format!(
        "M {x0:.2} {y0:.2} A {r1} {r1} 0 {large} 1 {x1:.2} {y1:.2} L {x2:.2} {y2:.2} A {r0} {r0} 0 {large} 0 {x3:.2} {y3:.2} Z"
    )
}

fn pie(result: &ResultSet, o: &Options, chart: &mut Chart) {
    let agg = if o.y.is_none() { Agg::Count } else { o.agg };
    chart.y_title = y_title(result, o, agg);
    let opts = Options {
        series: None,
        top: Some(o.top.unwrap_or(MAX_SERIES - 1)),
        order: if o.order == Order::Result {
            Order::Value
        } else {
            o.order
        },
        ..*o
    };
    let g = group(result, &opts, agg);
    let parts: Vec<(String, f64)> = g
        .categories
        .iter()
        .enumerate()
        .filter_map(|(ci, c)| g.values.get(&(ci, 0)).map(|v| (c.clone(), *v)))
        .filter(|(_, v)| *v > 0.0)
        .collect();
    let total: f64 = parts.iter().map(|p| p.1).sum();
    if parts.is_empty() || total <= 0.0 {
        chart.note = "A pie needs positive values.".to_owned();
        return;
    }
    chart.table.push(vec![
        chart.x_title.clone(),
        chart.y_title.clone(),
        "share".to_owned(),
    ]);
    let mut at = 0.0;
    for (i, (label, v)) in parts.iter().enumerate() {
        let share = v / total;
        let a0 = at * std::f64::consts::TAU;
        at += share;
        let a1 = at * std::f64::consts::TAU;
        chart.slices.push(Slice {
            path: arc(a0, a1),
            label: label.clone(),
            value: fmt(*v),
            share,
            series: i % MAX_SERIES,
        });
        chart
            .legend
            .push(format!("{label}  {:.1} %", share * 100.0));
        chart.table.push(vec![
            label.clone(),
            fmt(*v),
            format!("{:.1} %", share * 100.0),
        ]);
    }
    if !result.rows.is_empty() && parts.len() < g.categories.len() {
        chart.note = "Zero and negative groups are left out.".to_owned();
    }
}

fn scatter(result: &ResultSet, o: &Options, chart: &mut Chart) {
    let Some(y) = o.y else {
        chart.note = "Pick a Y column.".to_owned();
        return;
    };
    let x = o.x;
    chart.y_title = result.columns.get(y).cloned().unwrap_or_default();
    let mut ser_order: Vec<String> = Vec::new();
    let mut triples: Vec<(f64, f64, usize)> = Vec::new();
    for r in &result.rows {
        let (Some(a), Some(b)) = (
            number(r.get(x).and_then(Option::as_ref)),
            number(r.get(y).and_then(Option::as_ref)),
        ) else {
            continue;
        };
        let s = match o.series {
            Some(i) => {
                let k = key(r, i);
                match ser_order.iter().position(|n| *n == k) {
                    Some(p) => p.min(MAX_SERIES - 1),
                    None => {
                        ser_order.push(k);
                        (ser_order.len() - 1).min(MAX_SERIES - 1)
                    },
                }
            },
            None => 0,
        };
        triples.push((a, b, s));
    }
    if triples.is_empty() {
        chart.note = "Scatter needs numbers in both columns.".to_owned();
        return;
    }
    if o.series.is_some() {
        ser_order.truncate(MAX_SERIES);
        if ser_order.len() == MAX_SERIES {
            ser_order[MAX_SERIES - 1] = "Other".to_owned();
        }
        chart.legend = ser_order.clone();
    }
    let (xlo, xhi, xt) = y_axis(triples.iter().map(|p| p.0), false);
    let (ylo, yhi, yt) = y_axis(triples.iter().map(|p| p.1), false);
    chart.x_ticks = xt;
    chart.y_ticks = yt;
    chart
        .table
        .push(vec![chart.x_title.clone(), chart.y_title.clone()]);
    chart.points = triples
        .iter()
        .map(|(a, b, s)| {
            chart.table.push(vec![fmt(*a), fmt(*b)]);
            Point {
                x: (a - xlo) / (xhi - xlo),
                y: (b - ylo) / (yhi - ylo),
                label: match ser_order.get(*s) {
                    Some(n) if o.series.is_some() => format!("{n}: {}, {}", fmt(*a), fmt(*b)),
                    _ => format!("{}, {}", fmt(*a), fmt(*b)),
                },
                series: *s,
            }
        })
        .collect();
    let skipped = result.rows.len() - triples.len();
    if skipped > 0 {
        chart.note = format!("{skipped} rows without two numbers are left out.");
    }
}

fn histogram(result: &ResultSet, x: usize, chart: &mut Chart) {
    chart.y_title = "rows".to_owned();
    let values: Vec<f64> = result
        .rows
        .iter()
        .filter_map(|r| number(r.get(x).and_then(Option::as_ref)))
        .collect();
    if values.is_empty() {
        chart.note = "A histogram needs a numeric column.".to_owned();
        return;
    }
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let bins = if hi > lo { HISTOGRAM_BINS } else { 1 };
    #[allow(clippy::cast_precision_loss)]
    let width = if hi > lo {
        (hi - lo) / bins as f64
    } else {
        1.0
    };
    let mut counts = vec![0_u64; bins];
    for v in &values {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let at = (((v - lo) / width) as usize).min(bins - 1);
        counts[at] += 1;
    }
    #[allow(clippy::cast_precision_loss)]
    let (_, top, ticks) = y_axis(counts.iter().map(|c| *c as f64), true);
    chart.y_ticks = ticks;
    chart
        .table
        .push(vec!["from".to_owned(), "to".to_owned(), "rows".to_owned()]);
    #[allow(clippy::cast_precision_loss)]
    for (i, c) in counts.iter().enumerate() {
        let from = lo + width * i as f64;
        chart.bars.push(Bar {
            x0: i as f64 / bins as f64,
            x1: (i + 1) as f64 / bins as f64,
            y0: 0.0,
            h: *c as f64 / top,
            label: format!("{} – {}", fmt(from), fmt(from + width)),
            value: c.to_string(),
            series: 0,
        });
        chart
            .table
            .push(vec![fmt(from), fmt(from + width), c.to_string()]);
        if i % 4 == 0 || i + 1 == bins {
            chart.x_ticks.push(Tick {
                at: i as f64 / bins as f64,
                label: fmt(from),
            });
        }
    }
    let skipped = result.rows.len() - values.len();
    if skipped > 0 {
        chart.note = format!("{skipped} rows without a number are left out.");
    }
}

/// The numbers behind the chart as tab-separated text, ready for a spreadsheet.
#[must_use]
pub fn table_tsv(chart: &Chart) -> String {
    chart
        .table
        .iter()
        .map(|r| r.join("\t"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The chart as a standalone SVG, for a report or a slide.
#[must_use]
pub fn svg(chart: &Chart, title: &str) -> String {
    let (w, h) = (900.0, 520.0);
    let legend_w = if chart.legend.is_empty() { 0.0 } else { 170.0 };
    let (left, right, top, bottom) = (70.0, 20.0 + legend_w, 40.0, 70.0);
    let pw = w - left - right;
    let ph = h - top - bottom;
    let px = |f: f64| left + f * pw;
    let py = |f: f64| top + (1.0 - f) * ph;
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let colour = |s: usize| PALETTE[s % MAX_SERIES];
    let mut out = String::new();
    let _ = writeln!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="Inter, Arial, sans-serif" font-size="11">"#
    );
    let _ = writeln!(out, r##"<rect width="{w}" height="{h}" fill="#ffffff"/>"##);
    let _ = writeln!(
        out,
        r##"<text x="{left}" y="24" font-size="14" font-weight="600" fill="#1f2328">{}</text>"##,
        esc(title)
    );
    if chart.slices.is_empty() {
        for t in &chart.y_ticks {
            let y = py(t.at);
            let _ = writeln!(
                out,
                r##"<line x1="{left}" x2="{}" y1="{y:.1}" y2="{y:.1}" stroke="#e5e7eb"/><text x="{}" y="{:.1}" text-anchor="end" fill="#6b7280">{}</text>"##,
                left + pw,
                left - 6.0,
                y + 4.0,
                esc(&t.label)
            );
        }
        for t in &chart.x_ticks {
            let _ = writeln!(
                out,
                r##"<text x="{:.1}" y="{:.1}" text-anchor="middle" fill="#6b7280">{}</text>"##,
                px(t.at),
                top + ph + 16.0,
                esc(&t.label)
            );
        }
    }
    for b in &chart.bars {
        let _ = writeln!(
            out,
            r##"<rect x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" fill="{}"><title>{}: {}</title></rect>"##,
            px(b.x0),
            py(b.y0 + b.h),
            (px(b.x1) - px(b.x0)).max(1.0),
            b.h * ph,
            colour(b.series),
            esc(&b.label),
            esc(&b.value)
        );
    }
    if chart.joined {
        let mut series: Vec<usize> = chart.points.iter().map(|p| p.series).collect();
        series.dedup();
        series.sort_unstable();
        series.dedup();
        for s in series {
            let path: Vec<String> = chart
                .points
                .iter()
                .filter(|p| p.series == s)
                .map(|p| format!("{:.1},{:.1}", px(p.x), py(p.y)))
                .collect();
            if path.len() > 1 {
                let _ = writeln!(
                    out,
                    r#"<polyline points="{}" fill="none" stroke="{}" stroke-width="2"/>"#,
                    path.join(" "),
                    colour(s)
                );
            }
        }
    }
    for p in &chart.points {
        let _ = writeln!(
            out,
            r#"<circle cx="{:.1}" cy="{:.1}" r="3" fill="{}"><title>{}</title></circle>"#,
            px(p.x),
            py(p.y),
            colour(p.series),
            esc(&p.label)
        );
    }
    if !chart.slices.is_empty() {
        let size = ph.min(pw);
        let scale = size / 200.0;
        let _ = writeln!(
            out,
            r#"<g transform="translate({:.1} {:.1}) scale({scale:.3})">"#,
            left + (pw - size) / 2.0,
            top
        );
        for s in &chart.slices {
            let _ = writeln!(
                out,
                r#"<path d="{}" fill="{}"><title>{}: {}</title></path>"#,
                s.path,
                colour(s.series),
                esc(&s.label),
                esc(&s.value)
            );
        }
        out.push_str("</g>\n");
    } else {
        let _ = writeln!(
            out,
            r##"<text x="{:.1}" y="{:.1}" text-anchor="middle" fill="#374151">{}</text>"##,
            left + pw / 2.0,
            h - 16.0,
            esc(&chart.x_title)
        );
        let _ = writeln!(
            out,
            r##"<text transform="translate(16 {:.1}) rotate(-90)" text-anchor="middle" fill="#374151">{}</text>"##,
            top + ph / 2.0,
            esc(&chart.y_title)
        );
    }
    for (i, name) in chart.legend.iter().enumerate() {
        let y = top + 10.0 + 20.0 * i as f64;
        let x = w - legend_w;
        let _ = writeln!(
            out,
            r##"<rect x="{x:.1}" y="{:.1}" width="10" height="10" rx="2" fill="{}"/><text x="{:.1}" y="{y:.1}" fill="#374151">{}</text>"##,
            y - 9.0,
            colour(i),
            x + 16.0,
            esc(name)
        );
    }
    out.push_str("</svg>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> ResultSet {
        ResultSet {
            columns: vec!["city".into(), "score".into(), "n".into(), "kind".into()],
            types: vec!["text".into(), "int".into(), "int".into(), "text".into()],
            rows: vec![
                vec![
                    Some("A".into()),
                    Some("10".into()),
                    Some("1".into()),
                    Some("x".into()),
                ],
                vec![
                    Some("B".into()),
                    Some("20".into()),
                    Some("2".into()),
                    Some("y".into()),
                ],
                vec![
                    Some("A".into()),
                    Some("30".into()),
                    Some("3".into()),
                    Some("y".into()),
                ],
                vec![None, Some("x".into()), Some("4".into()), Some("x".into())],
            ],
        }
    }

    fn opts(kind: Kind, x: usize, y: Option<usize>, agg: Agg) -> Options {
        Options::new(kind, x, y, agg)
    }

    #[test]
    fn bars_group_and_sum() {
        let c = build(&data(), &opts(Kind::Bar, 0, Some(1), Agg::Sum));
        let labels: Vec<_> = c
            .bars
            .iter()
            .map(|b| (b.label.as_str(), b.value.as_str()))
            .collect();
        assert_eq!(labels, vec![("A", "40"), ("B", "20"), ("NULL", "0")]);
        assert_eq!(c.y_title, "sum(score)");
        let counted = build(&data(), &opts(Kind::Bar, 0, None, Agg::Sum));
        assert_eq!(counted.bars[0].value, "2");
        assert_eq!(table_tsv(&c).lines().next(), Some("city\tsum(score)"));
    }

    #[test]
    fn order_top_and_series() {
        let mut o = opts(Kind::Bar, 0, Some(1), Agg::Sum);
        o.order = Order::Value;
        o.top = Some(1);
        let c = build(&data(), &o);
        let labels: Vec<_> = c.bars.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, vec!["A", "Other (2)"]);
        let mut s = opts(Kind::Bar, 0, Some(1), Agg::Sum);
        s.series = Some(3);
        let c = build(&data(), &s);
        assert_eq!(c.legend, vec!["x".to_owned(), "y".to_owned()]);
        assert_eq!(c.bars.len(), 4);
    }

    #[test]
    fn pie_shares() {
        let c = build(&data(), &opts(Kind::Pie, 0, Some(1), Agg::Sum));
        assert_eq!(c.slices.len(), 2);
        let total: f64 = c.slices.iter().map(|s| s.share).sum();
        assert!((total - 1.0).abs() < 1e-9);
        assert!(c.slices[0].path.starts_with('M'));
    }

    #[test]
    fn scatter_and_histogram() {
        let s = build(&data(), &opts(Kind::Scatter, 2, Some(1), Agg::None));
        assert_eq!(s.points.len(), 3);
        assert!(s.note.contains("1 rows"));
        let h = build(&data(), &opts(Kind::Histogram, 1, None, Agg::None));
        assert_eq!(h.bars.len(), HISTOGRAM_BINS);
        let total: u64 = h
            .bars
            .iter()
            .map(|b| b.value.parse::<u64>().unwrap_or(0))
            .sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn numbers_are_short() {
        assert_eq!(fmt(1_234_567.0), "1.23M");
        assert_eq!(fmt(12_500.0), "12.5k");
        assert_eq!(fmt(40.0), "40");
        assert_eq!(fmt(0.5), "0.5");
        assert!(
            svg(
                &build(&data(), &opts(Kind::Line, 2, Some(1), Agg::Sum)),
                "t"
            )
            .starts_with("<svg")
        );
    }
}
