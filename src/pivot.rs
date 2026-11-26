//! The Pivot tab of a query result: one column's values as rows, another's as columns, and a
//! third folded into each cell (sum, avg, min, max or count), with totals. Computed from the
//! rows already fetched; the result is an ordinary `ResultSet`, so it shows in the usual grid
//! and copies like one.

use std::collections::HashMap;

use crate::backend::ResultSet;
use crate::chart::Agg;

/// Distinct column values made into columns at most; the rest are left out with a note.
pub const MAX_COLUMNS: usize = 60;

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

    fn merge(&mut self, o: Self) {
        self.sum += o.sum;
        self.n += o.n;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
    }

    #[allow(clippy::cast_precision_loss)]
    fn value(self, agg: Agg) -> Option<f64> {
        if self.n == 0 {
            return None;
        }
        Some(match agg {
            Agg::Count => self.n as f64,
            Agg::Avg => self.sum / self.n as f64,
            Agg::Min => self.min,
            Agg::Max => self.max,
            Agg::Sum | Agg::None => self.sum,
        })
    }
}

fn text(row: &[Option<String>], i: usize) -> String {
    row.get(i)
        .cloned()
        .flatten()
        .unwrap_or_else(|| "NULL".to_owned())
}

fn cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x.total_cmp(&y),
        _ => a.cmp(b),
    }
}

fn show(v: Option<f64>) -> Option<String> {
    v.map(|v| {
        if v.fract() == 0.0 && v.abs() < 1e15 {
            format!("{v:.0}")
        } else {
            let s = format!("{v:.4}");
            s.trim_end_matches('0').trim_end_matches('.').to_owned()
        }
    })
}

/// The pivot of `result`: `rows` values down, `columns` values across, `value` folded by
/// `agg` (`value` `None` counts rows). Returns the table and a note (e.g. columns left out).
#[must_use]
pub fn pivot(
    result: &ResultSet,
    rows: usize,
    columns: usize,
    value: Option<usize>,
    agg: Agg,
) -> (ResultSet, String) {
    let agg = if value.is_none() { Agg::Count } else { agg };
    let mut cells: HashMap<(String, String), Acc> = HashMap::new();
    let mut row_keys: Vec<String> = Vec::new();
    let mut col_keys: Vec<String> = Vec::new();
    for r in &result.rows {
        let rk = text(r, rows);
        let ck = text(r, columns);
        if !row_keys.contains(&rk) {
            row_keys.push(rk.clone());
        }
        if !col_keys.contains(&ck) {
            col_keys.push(ck.clone());
        }
        let v = match value {
            None => Some(1.0),
            Some(i) => r
                .get(i)
                .cloned()
                .flatten()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite()),
        };
        let entry = cells.entry((rk, ck)).or_insert(Acc::EMPTY);
        match (agg, v) {
            (Agg::Count, _) => entry.add(1.0),
            (_, Some(v)) => entry.add(v),
            _ => {},
        }
    }
    row_keys.sort_by(|a, b| cmp(a, b));
    col_keys.sort_by(|a, b| cmp(a, b));
    let mut note = String::new();
    if col_keys.len() > MAX_COLUMNS {
        note = format!(
            "{} of {} column values shown; pivot on a column with fewer distinct values.",
            MAX_COLUMNS,
            col_keys.len()
        );
        col_keys.truncate(MAX_COLUMNS);
    }
    let name = |i: usize| result.columns.get(i).cloned().unwrap_or_default();
    let mut header = vec![format!("{} \\ {}", name(rows), name(columns))];
    header.extend(col_keys.iter().cloned());
    header.push("Total".to_owned());
    let mut out_rows = Vec::new();
    let mut col_totals: Vec<Acc> = vec![Acc::EMPTY; col_keys.len()];
    let mut grand = Acc::EMPTY;
    for rk in &row_keys {
        let mut row = vec![Some(rk.clone())];
        let mut total = Acc::EMPTY;
        for (ci, ck) in col_keys.iter().enumerate() {
            let acc = cells.get(&(rk.clone(), ck.clone())).copied();
            row.push(acc.and_then(|a| show(a.value(agg))));
            if let Some(a) = acc {
                total.merge(a);
                col_totals[ci].merge(a);
                grand.merge(a);
            }
        }
        row.push(show(total.value(agg)));
        out_rows.push(row);
    }
    let mut totals = vec![Some("Total".to_owned())];
    totals.extend(col_totals.iter().map(|a| show(a.value(agg))));
    totals.push(show(grand.value(agg)));
    out_rows.push(totals);
    let mut types = vec!["text".to_owned()];
    types.extend(std::iter::repeat_n("float".to_owned(), col_keys.len() + 1));
    (
        ResultSet {
            columns: header,
            types,
            rows: out_rows,
        },
        note,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_with_totals() {
        let data = ResultSet {
            columns: vec!["city".into(), "year".into(), "sales".into()],
            types: vec![],
            rows: vec![
                vec![Some("A".into()), Some("2024".into()), Some("10".into())],
                vec![Some("A".into()), Some("2025".into()), Some("5".into())],
                vec![Some("B".into()), Some("2024".into()), Some("7".into())],
                vec![Some("A".into()), Some("2024".into()), Some("1".into())],
            ],
        };
        let (p, note) = pivot(&data, 0, 1, Some(2), Agg::Sum);
        assert!(note.is_empty());
        assert_eq!(p.columns, vec!["city \\ year", "2024", "2025", "Total"]);
        assert_eq!(
            p.rows[0],
            vec![
                Some("A".into()),
                Some("11".into()),
                Some("5".into()),
                Some("16".into())
            ]
        );
        assert_eq!(
            p.rows[1],
            vec![Some("B".into()), Some("7".into()), None, Some("7".into())]
        );
        assert_eq!(
            p.rows[2],
            vec![
                Some("Total".into()),
                Some("18".into()),
                Some("5".into()),
                Some("23".into())
            ]
        );
        let (c, _) = pivot(&data, 0, 1, None, Agg::Sum);
        assert_eq!(c.rows[0][1].as_deref(), Some("2"));
    }
}
