//! Vector columns (`VECTOR(n)`): how a value is shown in a grid, the four distance metrics
//! NusaDB has, the nearest-neighbour query Studio builds, and what `EXPLAIN` says about it.
//!
//! From the NusaDB SQL reference: operators `<->` L2, `<=>` cosine, `<#>` negative inner
//! product, `<+>` L1; an HNSW index (`USING hnsw (col <opclass>)`) answers only the metric its
//! operator class declares; `SET hnsw_ef_search = n` trades latency for recall.

use crate::backend::quote_ident;

/// Values shown before the grid cuts a vector short.
const SHOWN: usize = 3;

/// A vector as a grid shows it: the first few values and the dimension count, so a 768-wide
/// embedding fits a cell. Short vectors are shown whole.
#[must_use]
pub fn compact(text: &str) -> String {
    let Some(values) = parse(text) else {
        return text.to_owned();
    };
    if values.len() <= SHOWN + 1 {
        return text.to_owned();
    }
    let head: Vec<String> = values.iter().take(SHOWN).map(|v| short(*v)).collect();
    format!("[{}, …] {}d", head.join(", "), values.len())
}

fn short(v: f32) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

/// `[1, 2.5, -3]` into numbers; `None` when it is not a vector literal.
#[must_use]
pub fn parse(text: &str) -> Option<Vec<f32>> {
    let inner = text.trim().strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|p| p.trim().parse::<f32>().ok().filter(|v| v.is_finite()))
        .collect()
}

/// A query vector typed or pasted by the user, in the canonical `[..]` form. Accepts the
/// brackets missing, and spaces or newlines between values, as copied from Python or a CSV.
///
/// # Errors
///
/// When a part is not a number, or there is nothing.
pub fn normalise(text: &str) -> Result<Vec<f32>, String> {
    let inner = text.trim().trim_start_matches('[').trim_end_matches(']');
    let parts: Vec<&str> = inner
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return Err("Paste a vector, e.g. [0.1, 0.2, 0.3].".to_owned());
    }
    parts
        .iter()
        .map(|p| {
            p.parse::<f32>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| format!("\"{p}\" is not a number."))
        })
        .collect()
}

/// The canonical text of `values`, as NusaDB writes a vector.
#[must_use]
pub fn literal(values: &[f32]) -> String {
    let parts: Vec<String> = values.iter().map(ToString::to_string).collect();
    format!("[{}]", parts.join(","))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Cosine,
    L2,
    InnerProduct,
    L1,
}

impl Metric {
    pub const ALL: [Self; 4] = [Self::Cosine, Self::L2, Self::InnerProduct, Self::L1];

    #[must_use]
    pub const fn operator(self) -> &'static str {
        match self {
            Self::L2 => "<->",
            Self::Cosine => "<=>",
            Self::InnerProduct => "<#>",
            Self::L1 => "<+>",
        }
    }

    #[must_use]
    pub const fn opclass(self) -> &'static str {
        match self {
            Self::L2 => "vector_l2_ops",
            Self::Cosine => "vector_cosine_ops",
            Self::InnerProduct => "vector_ip_ops",
            Self::L1 => "vector_l1_ops",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::L2 => "L2",
            Self::Cosine => "cosine",
            Self::InnerProduct => "inner product",
            Self::L1 => "L1",
        }
    }
}

/// Narrowing of a nearest-neighbour search.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Limits {
    /// A WHERE condition on other columns (metadata filtering), as typed.
    pub filter: String,
    /// Only rows at most this far away.
    pub max_distance: Option<f64>,
    /// Compare every row instead of using an HNSW index (`(col <op> q) + 0` defeats the index;
    /// used to measure the index's recall).
    pub exact: bool,
}

/// The `SELECT` of a nearest-neighbour search within `limits`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn knn_select(
    schema: &str,
    table: &str,
    column: &str,
    shown: &[String],
    metric: Metric,
    query: &[f32],
    k: usize,
    limits: &Limits,
) -> String {
    let distance = format!(
        "{} {} '{}'",
        quote_ident(column),
        metric.operator(),
        literal(query)
    );
    let mut select: Vec<String> = shown.iter().map(|c| quote_ident(c)).collect();
    select.push(format!("{distance} AS distance"));
    let mut conditions = Vec::new();
    if !limits.filter.trim().is_empty() {
        conditions.push(format!("({})", limits.filter.trim()));
    }
    if let Some(max) = limits.max_distance {
        conditions.push(format!("({distance}) <= {max}"));
    }
    let clause = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    let order = if limits.exact {
        format!("({distance}) + 0")
    } else {
        "distance".to_owned()
    };
    format!(
        "SELECT {} FROM {}.{}{clause} ORDER BY {order} LIMIT {k}",
        select.join(", "),
        quote_ident(schema),
        quote_ident(table)
    )
}

/// Hybrid search: the nearest vectors and the best full-text matches of `text_column`, each
/// ranked, fused with `rrf_score` (reciprocal rank fusion) and joined back to the rows by
/// the single-column key `key`. Both rankings take the top `pool` candidates.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn hybrid_sql(
    schema: &str,
    table: &str,
    key: &str,
    column: &str,
    shown: &[String],
    metric: Metric,
    query: &[f32],
    text_column: &str,
    text: &str,
    k: usize,
) -> String {
    let pool = (k * 5).max(50);
    let t = format!("{}.{}", quote_ident(schema), quote_ident(table));
    let key = quote_ident(key);
    let distance = format!(
        "{} {} '{}'",
        quote_ident(column),
        metric.operator(),
        literal(query)
    );
    let words = text.replace('\'', "''");
    let doc = format!("to_tsvector({}::text)", quote_ident(text_column));
    let select: Vec<String> = shown
        .iter()
        .map(|c| format!("d.{}", quote_ident(c)))
        .collect();
    format!(
        "WITH v AS (SELECT {key}, row_number() OVER (ORDER BY {distance}) AS r FROM {t} \
         ORDER BY {distance} LIMIT {pool}), \
         w AS (SELECT {key}, row_number() OVER (ORDER BY ts_rank({doc}, plainto_tsquery('{words}')) DESC) AS r \
         FROM {t} WHERE {doc} @@ plainto_tsquery('{words}') LIMIT {pool}), \
         f AS (SELECT {key}, sum(s) AS score FROM (SELECT {key}, rrf_score(r) AS s FROM v \
         UNION ALL SELECT {key}, rrf_score(r) AS s FROM w) u GROUP BY {key}) \
         SELECT {}, f.score FROM f JOIN {t} d ON d.{key} = f.{key} ORDER BY f.score DESC LIMIT {k}",
        select.join(", ")
    )
}

/// What `EXPLAIN` says about a nearest-neighbour query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Answered by this HNSW index.
    Index { name: String, opclass: String },
    /// Every row compared: no HNSW index for that metric.
    Exact,
    /// No `VectorKnn` step (the query is not a nearest-neighbour search).
    Other,
}

/// Read a `VectorKnn` line of `EXPLAIN` output, e.g.
/// `VectorKnn: items using HNSW index items_emb (k=3, metric=vector_cosine_ops)` or
/// `VectorKnn: items exact scan, no HNSW index for vector_l2_ops (k=3)`.
#[must_use]
pub fn read_plan(lines: &[String]) -> Plan {
    for line in lines {
        let Some(at) = line.find("VectorKnn:") else {
            continue;
        };
        let rest = &line[at..];
        if let Some(after) = rest.split("using HNSW index ").nth(1) {
            let name = after
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned();
            let opclass = after
                .split("metric=")
                .nth(1)
                .map(|m| m.trim_end_matches(')').trim().to_owned())
                .unwrap_or_default();
            return Plan::Index { name, opclass };
        }
        return Plan::Exact;
    }
    Plan::Other
}

/// The `CREATE INDEX` for an HNSW index on `column` for `metric`.
#[must_use]
pub fn index_sql(schema: &str, table: &str, column: &str, metric: Metric) -> String {
    let short = match metric {
        Metric::Cosine => "cosine",
        Metric::L2 => "l2",
        Metric::InnerProduct => "ip",
        Metric::L1 => "l1",
    };
    let name = format!("{table}_{column}_{short}");
    format!(
        "CREATE INDEX {} ON {}.{} USING hnsw ({} {})",
        quote_ident(&name),
        quote_ident(schema),
        quote_ident(table),
        quote_ident(column),
        metric.opclass()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_vectors_are_shortened() {
        assert_eq!(compact("[1,0,0]"), "[1,0,0]");
        assert_eq!(compact("[0.1234,2,3,4,5,6]"), "[0.123, 2, 3, …] 6d");
        assert_eq!(compact("not a vector"), "not a vector");
    }

    #[test]
    fn pasted_vectors_are_read() {
        assert_eq!(normalise("[1, 2.5, -3]").ok(), Some(vec![1.0, 2.5, -3.0]));
        assert_eq!(normalise("1 2\n3").ok(), Some(vec![1.0, 2.0, 3.0]));
        assert!(normalise("[1, x]").is_err());
        assert!(normalise("  ").is_err());
        assert_eq!(literal(&[1.0, 0.5]), "[1,0.5]");
    }

    #[test]
    fn limits_narrow_the_search() {
        let limits = Limits {
            filter: "lang = 'id'".to_owned(),
            max_distance: Some(0.5),
            exact: true,
        };
        let sql = knn_select(
            "public",
            "docs",
            "emb",
            &["id".to_owned()],
            Metric::L2,
            &[1.0],
            3,
            &limits,
        );
        assert_eq!(
            sql,
            "SELECT \"id\", \"emb\" <-> '[1]' AS distance FROM \"public\".\"docs\" WHERE (lang = 'id') AND (\"emb\" <-> '[1]') <= 0.5 ORDER BY (\"emb\" <-> '[1]') + 0 LIMIT 3"
        );
    }

    #[test]
    fn hybrid_query_is_built() {
        let sql = hybrid_sql(
            "public",
            "docs",
            "id",
            "emb",
            &["id".to_owned(), "title".to_owned()],
            Metric::Cosine,
            &[1.0],
            "body",
            "kopi's",
            5,
        );
        assert!(sql.contains("rrf_score(r)"));
        assert!(sql.contains("plainto_tsquery('kopi''s')"));
        assert!(sql.ends_with("ORDER BY f.score DESC LIMIT 5"));
    }

    #[test]
    fn explain_is_read() {
        let used = vec![
            "Limit 3".to_owned(),
            "    VectorKnn: zz_docs using HNSW index zz_docs_emb (k=3, metric=vector_cosine_ops)"
                .to_owned(),
        ];
        assert_eq!(
            read_plan(&used),
            Plan::Index {
                name: "zz_docs_emb".to_owned(),
                opclass: "vector_cosine_ops".to_owned()
            }
        );
        let exact =
            vec!["VectorKnn: zz_docs exact scan, no HNSW index for vector_l2_ops (k=3)".to_owned()];
        assert_eq!(read_plan(&exact), Plan::Exact);
        assert_eq!(read_plan(&["Seq Scan".to_owned()]), Plan::Other);
    }
}
