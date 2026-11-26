//! Column profiling for the Data screen's Profile tab: per column the counts, nulls and
//! distinct values, the range, mean, spread and quartiles of numbers, the commonest values,
//! a histogram, and the correlation between numeric columns. Everything is computed by the
//! server with the aggregates the NusaDB SQL reference lists (`percentile_cont`, `stddev`,
//! `width_bucket`, `corr`, `vector_dims`, `vector_norm`), optionally on a `TABLESAMPLE`.

use crate::backend::{ResultSet, quote_ident};

/// Histogram bars.
pub const BUCKETS: u32 = 12;
/// Commonest values listed per column.
pub const TOP: usize = 5;
/// Numeric columns the correlation matrix takes at most.
pub const CORR_MAX: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Family {
    Number,
    Text,
    Time,
    Bool,
    Vector,
    /// JSON, bytes, arrays, geometry: counted only.
    #[default]
    Other,
}

impl Family {
    /// From `information_schema.columns.data_type`.
    #[must_use]
    pub fn of(data_type: &str) -> Self {
        let t = data_type.to_ascii_lowercase();
        if matches!(
            t.as_str(),
            "integer"
                | "int"
                | "bigint"
                | "smallint"
                | "numeric"
                | "decimal"
                | "real"
                | "double precision"
                | "float"
        ) || t.starts_with("numeric")
            || t.starts_with("decimal")
        {
            Self::Number
        } else if t.contains("char") || t == "text" || t == "uuid" || t == "inet" || t == "cidr" {
            Self::Text
        } else if t.starts_with("timestamp") || t == "date" || t.starts_with("time") {
            Self::Time
        } else if t == "boolean" || t == "bool" {
            Self::Bool
        } else if t == "vector" {
            Self::Vector
        } else {
            Self::Other
        }
    }

    const fn has_top(self) -> bool {
        matches!(self, Self::Number | Self::Text | Self::Time | Self::Bool)
    }
}

/// One column to profile.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub data_type: String,
}

/// What one statement of a profile run answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Stats(usize),
    Top(usize),
    Histogram(usize),
    /// Values beyond 1.5 × the interquartile range from the quartiles.
    Outliers(usize),
    Correlation(Vec<usize>),
}

/// `FROM` target, sampled when `sample` (percent) is given.
#[must_use]
pub fn source(schema: &str, table: &str, sample: Option<u32>) -> String {
    let base = format!("{}.{}", quote_ident(schema), quote_ident(table));
    match sample {
        Some(p) => format!("{base} TABLESAMPLE BERNOULLI ({p})"),
        None => base,
    }
}

/// First pass: statistics and commonest values of every column.
#[must_use]
pub fn first_pass(columns: &[Column], from: &str) -> Vec<(Step, String)> {
    let mut out = Vec::new();
    for (i, column) in columns.iter().enumerate() {
        let c = quote_ident(&column.name);
        let family = Family::of(&column.data_type);
        let stats = match family {
            Family::Number => format!(
                "SELECT count(*), count({c}), count(DISTINCT {c}), min({c})::text, max({c})::text, \
                 avg({c})::text, stddev({c})::text, \
                 percentile_cont(0.25) WITHIN GROUP (ORDER BY {c}), \
                 percentile_cont(0.5) WITHIN GROUP (ORDER BY {c}), \
                 percentile_cont(0.75) WITHIN GROUP (ORDER BY {c}), \
                 count(CASE WHEN {c} = 0 THEN 1 END), count(CASE WHEN {c} < 0 THEN 1 END) \
                 FROM {from}"
            ),
            Family::Text => format!(
                "SELECT count(*), count({c}), count(DISTINCT {c}), min({c})::text, max({c})::text, \
                 avg(length({c}::text))::text, min(length({c}::text)), max(length({c}::text)), \
                 count(CASE WHEN {c}::text = '' THEN 1 END) FROM {from}"
            ),
            Family::Time | Family::Bool => format!(
                "SELECT count(*), count({c}), count(DISTINCT {c}), min({c})::text, max({c})::text \
                 FROM {from}"
            ),
            Family::Vector => format!(
                "SELECT count(*), count({c}), min(vector_dims({c})), max(vector_dims({c})), \
                 avg(vector_norm({c}))::text FROM {from}"
            ),
            Family::Other => format!("SELECT count(*), count({c}) FROM {from}"),
        };
        out.push((Step::Stats(i), stats));
        if family.has_top() {
            out.push((
                Step::Top(i),
                format!(
                    "SELECT {c}::text, count(*) FROM {from} GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT {TOP}"
                ),
            ));
        }
    }
    out
}

/// Second pass, once the ranges are known: histograms of numeric columns with a spread, and
/// the correlation of every pair of numeric columns.
#[must_use]
pub fn second_pass(columns: &[Column], stats: &[ColumnStats], from: &str) -> Vec<(Step, String)> {
    let mut out = Vec::new();
    let mut numeric = Vec::new();
    for (i, column) in columns.iter().enumerate() {
        let Some(s) = stats.get(i) else { continue };
        if s.family != Family::Number {
            continue;
        }
        numeric.push(i);
        let (Some(lo), Some(hi)) = (s.min_value(), s.max_value()) else {
            continue;
        };
        if hi <= lo {
            continue;
        }
        let c = quote_ident(&column.name);
        if let Some([q1, _, q3]) = &s.quartiles
            && let (Ok(q1), Ok(q3)) = (q1.parse::<f64>(), q3.parse::<f64>())
        {
            let iqr = q3 - q1;
            out.push((
                Step::Outliers(i),
                format!(
                    "SELECT count(*) FROM {from} WHERE {c} < {} OR {c} > {}",
                    q1 - 1.5 * iqr,
                    q3 + 1.5 * iqr
                ),
            ));
        }
        out.push((
            Step::Histogram(i),
            format!(
                "SELECT width_bucket({c}, {lo}, {hi}, {BUCKETS}), count(*) FROM {from} \
                 WHERE {c} IS NOT NULL GROUP BY 1 ORDER BY 1"
            ),
        ));
    }
    numeric.truncate(CORR_MAX);
    if numeric.len() >= 2 {
        let mut parts = Vec::new();
        for (a_at, a) in numeric.iter().enumerate() {
            for b in &numeric[a_at + 1..] {
                parts.push(format!(
                    "corr({}, {})",
                    quote_ident(&columns[*a].name),
                    quote_ident(&columns[*b].name)
                ));
            }
        }
        out.push((
            Step::Correlation(numeric),
            format!("SELECT {} FROM {from}", parts.join(", ")),
        ));
    }
    out
}

/// What the profile found about one column.
#[derive(Debug, Clone, Default)]
pub struct ColumnStats {
    pub name: String,
    pub data_type: String,
    pub family: Family,
    pub rows: u64,
    pub non_null: u64,
    pub distinct: Option<u64>,
    pub min: Option<String>,
    pub max: Option<String>,
    pub mean: Option<String>,
    pub stddev: Option<String>,
    pub quartiles: Option<[String; 3]>,
    pub avg_length: Option<String>,
    pub length_range: Option<(u64, u64)>,
    pub zeros: Option<u64>,
    pub negatives: Option<u64>,
    pub empties: Option<u64>,
    pub outliers: Option<u64>,
    pub dims: Option<(u64, u64)>,
    pub avg_norm: Option<String>,
    /// Commonest values (`None` = NULL) with their counts.
    pub top: Vec<(Option<String>, u64)>,
    /// Histogram bars: lower bound, upper bound, count.
    pub histogram: Vec<(f64, f64, u64)>,
    pub error: Option<String>,
}

impl ColumnStats {
    #[must_use]
    pub fn new(column: &Column) -> Self {
        Self {
            name: column.name.clone(),
            data_type: column.data_type.clone(),
            family: Family::of(&column.data_type),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn min_value(&self) -> Option<f64> {
        self.min.as_deref().and_then(|v| v.parse().ok())
    }

    #[must_use]
    pub fn max_value(&self) -> Option<f64> {
        self.max.as_deref().and_then(|v| v.parse().ok())
    }

    #[must_use]
    pub fn nulls(&self) -> u64 {
        self.rows.saturating_sub(self.non_null)
    }

    /// Fold the result of a statistics statement in.
    pub fn read_stats(&mut self, result: &ResultSet) {
        let Some(row) = result.rows.first() else {
            return;
        };
        let text = |i: usize| row.get(i).cloned().flatten();
        let count = |i: usize| text(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        self.rows = count(0);
        self.non_null = count(1);
        match self.family {
            Family::Number => {
                self.distinct = Some(count(2));
                self.min = text(3);
                self.max = text(4);
                self.mean = text(5).map(|v| round(&v));
                self.stddev = text(6).map(|v| round(&v));
                if let (Some(a), Some(b), Some(c)) = (text(7), text(8), text(9)) {
                    self.quartiles = Some([round(&a), round(&b), round(&c)]);
                }
                self.zeros = Some(count(10));
                self.negatives = Some(count(11));
            },
            Family::Text => {
                self.distinct = Some(count(2));
                self.min = text(3);
                self.max = text(4);
                self.avg_length = text(5).map(|v| round(&v));
                if text(6).is_some() {
                    self.length_range = Some((count(6), count(7)));
                }
                self.empties = Some(count(8));
            },
            Family::Time | Family::Bool => {
                self.distinct = Some(count(2));
                self.min = text(3);
                self.max = text(4);
            },
            Family::Vector => {
                if let (Some(a), Some(b)) = (text(2), text(3)) {
                    self.dims = Some((a.parse().unwrap_or(0), b.parse().unwrap_or(0)));
                }
                self.avg_norm = text(4).map(|v| round(&v));
            },
            Family::Other => {},
        }
    }

    pub fn read_outliers(&mut self, result: &ResultSet) {
        self.outliers = result
            .rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .and_then(|v| v.parse().ok());
    }

    /// Every non-NULL value is different: the column could be a key.
    #[must_use]
    pub fn unique(&self) -> bool {
        self.non_null > 0 && self.distinct == Some(self.non_null) && self.nulls() == 0
    }

    pub fn read_top(&mut self, result: &ResultSet) {
        self.top = result
            .rows
            .iter()
            .map(|r| {
                (
                    r.first().cloned().flatten(),
                    r.get(1)
                        .cloned()
                        .flatten()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                )
            })
            .collect();
    }

    /// Fold a `width_bucket` count in. Bucket `BUCKETS + 1` holds the maximum itself and is
    /// merged into the last bar; empty buckets are kept so the shape is true.
    pub fn read_histogram(&mut self, result: &ResultSet) {
        let (Some(lo), Some(hi)) = (self.min_value(), self.max_value()) else {
            return;
        };
        let n = BUCKETS as usize;
        let width = (hi - lo) / f64::from(BUCKETS);
        let mut counts = vec![0_u64; n];
        for row in &result.rows {
            let bucket = row
                .first()
                .cloned()
                .flatten()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            let count = row
                .get(1)
                .cloned()
                .flatten()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            if bucket >= 1 {
                counts[(bucket - 1).min(n - 1)] += count;
            }
        }
        #[allow(clippy::cast_precision_loss)]
        {
            self.histogram = counts
                .into_iter()
                .enumerate()
                .map(|(i, c)| (lo + width * i as f64, lo + width * (i + 1) as f64, c))
                .collect();
        }
    }
}

/// Pairwise correlations of the numeric columns `which`, read from one row of `corr(a, b)`
/// values in the order `second_pass` asks for them.
#[must_use]
pub fn read_correlation(which: &[usize], result: &ResultSet) -> Vec<(usize, usize, Option<f64>)> {
    let Some(row) = result.rows.first() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut at = 0;
    for (a_at, a) in which.iter().enumerate() {
        for b in &which[a_at + 1..] {
            let value = row
                .get(at)
                .cloned()
                .flatten()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite());
            out.push((*a, *b, value));
            at += 1;
        }
    }
    out
}

/// A number from the server, shortened to what a reader needs: `30.0000000000` → `30`,
/// `15.81138830084` → `15.8114`. Anything else unchanged.
#[must_use]
pub fn round(text: &str) -> String {
    let Ok(v) = text.trim().parse::<f64>() else {
        return text.to_owned();
    };
    let s = if v.abs() >= 1000.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.4}")
    };
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s
    };
    if s == "-0" { "0".to_owned() } else { s }
}

/// The profile as a Markdown table, one row per column, for a report or an issue.
#[must_use]
pub fn markdown(stats: &[ColumnStats], title: &str) -> String {
    let mut out = format!("## Profile of {title}\n\n");
    out.push_str(
        "| Column | Type | Rows | NULL | Distinct | Min | Max | Mean | Std dev | Median |\n",
    );
    out.push_str("| --- | --- | ---: | ---: | ---: | --- | --- | ---: | ---: | ---: |\n");
    let cell = |v: &Option<String>| v.clone().unwrap_or_default().replace('|', "\\|");
    for s in stats {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            s.name,
            s.data_type,
            s.rows,
            s.nulls(),
            s.distinct.map(|d| d.to_string()).unwrap_or_default(),
            cell(&s.min),
            cell(&s.max),
            cell(&s.mean),
            cell(&s.stddev),
            s.quartiles
                .as_ref()
                .map(|q| q[1].clone())
                .unwrap_or_default()
        ));
    }
    out
}

/// The same table as CSV, for a spreadsheet.
#[must_use]
pub fn csv(stats: &[ColumnStats]) -> String {
    let q = |v: &str| {
        if v.contains([',', '"', '\n']) {
            format!("\"{}\"", v.replace('"', "\"\""))
        } else {
            v.to_owned()
        }
    };
    let mut out = String::from(
        "column,type,rows,nulls,distinct,min,max,mean,stddev,p25,median,p75,zeros,negatives,outliers,empty,min_length,max_length\n",
    );
    for s in stats {
        let o = |v: &Option<String>| q(v.as_deref().unwrap_or(""));
        let n = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_default();
        let (p25, p50, p75) = s
            .quartiles
            .as_ref()
            .map_or((String::new(), String::new(), String::new()), |q| {
                (q[0].clone(), q[1].clone(), q[2].clone())
            });
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{p25},{p50},{p75},{},{},{},{},{},{}\n",
            q(&s.name),
            q(&s.data_type),
            s.rows,
            s.nulls(),
            n(s.distinct),
            o(&s.min),
            o(&s.max),
            o(&s.mean),
            o(&s.stddev),
            n(s.zeros),
            n(s.negatives),
            n(s.outliers),
            n(s.empties),
            n(s.length_range.map(|r| r.0)),
            n(s.length_range.map(|r| r.1)),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(rows: &[&[Option<&str>]]) -> ResultSet {
        ResultSet {
            columns: Vec::new(),
            types: Vec::new(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|v| v.map(str::to_owned)).collect())
                .collect(),
        }
    }

    #[test]
    fn families() {
        assert_eq!(Family::of("integer"), Family::Number);
        assert_eq!(Family::of("numeric(12,2)"), Family::Number);
        assert_eq!(Family::of("character varying"), Family::Text);
        assert_eq!(Family::of("timestamp with time zone"), Family::Time);
        assert_eq!(Family::of("vector"), Family::Vector);
        assert_eq!(Family::of("jsonb"), Family::Other);
    }

    #[test]
    fn numbers_are_read_and_bucketed() {
        let column = Column {
            name: "score".to_owned(),
            data_type: "integer".to_owned(),
        };
        let mut s = ColumnStats::new(&column);
        s.read_stats(&set(&[&[
            Some("5"),
            Some("5"),
            Some("5"),
            Some("10"),
            Some("50"),
            Some("30.0000000000000000"),
            Some("15.8113883008418967"),
            Some("20"),
            Some("30"),
            Some("40"),
        ]]));
        assert_eq!(s.mean.as_deref(), Some("30"));
        assert_eq!(s.stddev.as_deref(), Some("15.8114"));
        assert_eq!(s.nulls(), 0);
        let second = second_pass(&[column], std::slice::from_ref(&s), "t");
        assert_eq!(second.len(), 2);
        assert!(second[0].1.contains("< -10 OR \"score\" > 70"));
        assert!(second[1].1.contains("width_bucket(\"score\", 10, 50, 12)"));
        s.read_histogram(&set(&[&[Some("1"), Some("1")], &[Some("13"), Some("1")]]));
        assert_eq!(s.histogram.len(), 12);
        assert_eq!(s.histogram[0].2, 1);
        assert_eq!(s.histogram[11].2, 1);
    }

    #[test]
    fn correlations_follow_the_pairs() {
        let which = vec![0, 2, 3];
        let r = read_correlation(&which, &set(&[&[Some("1"), Some("-0.5"), None]]));
        assert_eq!(r, vec![(0, 2, Some(1.0)), (0, 3, Some(-0.5)), (2, 3, None)]);
    }

    #[test]
    fn sampling() {
        assert_eq!(source("public", "t", None), "\"public\".\"t\"");
        assert_eq!(
            source("public", "t", Some(10)),
            "\"public\".\"t\" TABLESAMPLE BERNOULLI (10)"
        );
    }
}
