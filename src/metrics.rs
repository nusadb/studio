//! NusaDB's Prometheus endpoint (`--metrics-listen`): its address, its text format, and the
//! figures the Monitor screen derives from two scrapes.
//!
//! The metric set is the one `docs/deployment.md` (*Metrics*) documents: connection and query
//! counters, `nusadb_database_stopped{database}`, and the scheduled-backup gauges. The server
//! has no latency histograms or per-database counters, so nothing here pretends to have them.

use std::collections::VecDeque;

/// Turn what the user typed into the URL to scrape: `host:port` or `http://host:port[/path]`.
/// Empty stays empty (no endpoint). The endpoint is plain HTTP and unauthenticated by design,
/// so `https://` is refused rather than silently downgraded.
///
/// # Errors
///
/// When the text is not an address of that shape.
pub fn normalize_url(input: &str) -> Result<String, ()> {
    let text = input.trim();
    if text.is_empty() {
        return Ok(String::new());
    }
    let rest = match text.strip_prefix("http://") {
        Some(rest) => rest,
        None if text.contains("://") => return Err(()),
        None => text,
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = authority.rsplit_once(':').ok_or(())?;
    let port: u16 = port.parse().map_err(|_| ())?;
    if host.is_empty() || port == 0 || host.contains(char::is_whitespace) {
        return Err(());
    }
    let path = if path.is_empty() || path == "/" {
        "/metrics"
    } else {
        path
    };
    Ok(format!("http://{host}:{port}{path}"))
}

/// `(host, port, path)` of a URL [`normalize_url`] produced.
#[must_use]
pub fn split_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = rest
        .find('/')
        .map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let (host, port) = authority.rsplit_once(':')?;
    Some((host.to_owned(), port.parse().ok()?, path.to_owned()))
}

/// One line of the exposition format.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

/// Parse the Prometheus text format: `name{label="v",…} value [timestamp]`, `#` lines skipped.
#[must_use]
pub fn parse(text: &str) -> Vec<Sample> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (head, tail) = match line.find('{') {
            Some(open) => {
                let Some(close) = line[open..].find('}').map(|c| open + c) else {
                    continue;
                };
                (&line[..close + 1], line[close + 1..].trim())
            },
            None => match line.split_once(char::is_whitespace) {
                Some((name, tail)) => (name, tail.trim()),
                None => continue,
            },
        };
        let Some(value) = tail.split_whitespace().next().and_then(parse_value) else {
            continue;
        };
        let (name, labels) = match head.find('{') {
            Some(open) => (&head[..open], parse_labels(&head[open + 1..head.len() - 1])),
            None => (head, Vec::new()),
        };
        out.push(Sample {
            name: name.to_owned(),
            labels,
            value,
        });
    }
    out
}

fn parse_value(text: &str) -> Option<f64> {
    match text {
        "+Inf" => Some(f64::INFINITY),
        "-Inf" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ => text.parse().ok(),
    }
}

/// `a="x",b="y\"z"` → pairs, honouring the format's `\"`, `\\` and `\n` escapes.
fn parse_labels(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        let name: String = chars
            .by_ref()
            .skip_while(|c| *c == ',' || c.is_whitespace())
            .take_while(|c| *c != '=')
            .collect();
        if name.is_empty() || chars.next() != Some('"') {
            break;
        }
        let mut value = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some('n') => value.push('\n'),
                    Some(other) => value.push(other),
                    None => break,
                },
                '"' => break,
                other => value.push(other),
            }
        }
        out.push((name.trim().to_owned(), value));
    }
    out
}

/// The documented NusaDB metrics, read from one scrape.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub connections_total: f64,
    pub connections_active: f64,
    pub queries_total: f64,
    pub query_errors_total: f64,
    /// `(database, stopped)` for each database opened since the server started.
    pub databases: Vec<(String, bool)>,
    /// Unix seconds; 0 before the first scheduled backup.
    pub backup_last_success: f64,
    pub backup_last_duration: f64,
    pub backup_failures_total: f64,
    pub archive_prune_failures_total: f64,
    /// Whether the scrape held NusaDB's metrics at all (a wrong port answers something else).
    pub recognised: bool,
}

impl Snapshot {
    #[must_use]
    pub fn from_samples(samples: &[Sample]) -> Self {
        let mut s = Self::default();
        for sample in samples {
            let v = sample.value;
            match sample.name.as_str() {
                "nusadb_connections_total" => s.connections_total = v,
                "nusadb_connections_active" => s.connections_active = v,
                "nusadb_queries_total" => s.queries_total = v,
                "nusadb_query_errors_total" => s.query_errors_total = v,
                "nusadb_backup_last_success_timestamp_seconds" => s.backup_last_success = v,
                "nusadb_backup_last_duration_seconds" => s.backup_last_duration = v,
                "nusadb_backup_failures_total" => s.backup_failures_total = v,
                "nusadb_archive_prune_failures_total" => s.archive_prune_failures_total = v,
                "nusadb_database_stopped" => {
                    let db = sample
                        .labels
                        .iter()
                        .find(|(k, _)| k == "database")
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    s.databases.push((db, v >= 1.0));
                },
                _ => continue,
            }
            s.recognised = true;
        }
        s.databases.sort();
        s
    }
}

/// Per-second rates between two scrapes `seconds` apart. `None` when a counter went backwards,
/// which means the server restarted in between and the difference means nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub connections: f64,
    pub queries: f64,
    pub errors: f64,
}

#[must_use]
pub fn rates(before: &Snapshot, after: &Snapshot, seconds: f64) -> Option<Rates> {
    if seconds <= 0.0
        || after.connections_total < before.connections_total
        || after.queries_total < before.queries_total
        || after.query_errors_total < before.query_errors_total
    {
        return None;
    }
    Some(Rates {
        connections: (after.connections_total - before.connections_total) / seconds,
        queries: (after.queries_total - before.queries_total) / seconds,
        errors: (after.query_errors_total - before.query_errors_total) / seconds,
    })
}

/// The last `cap` values of one figure, oldest first, for a sparkline.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    cap: usize,
    values: VecDeque<f64>,
}

impl Series {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            values: VecDeque::with_capacity(cap),
        }
    }

    pub fn push(&mut self, value: f64) {
        if self.values.len() == self.cap {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    #[must_use]
    pub fn values(&self) -> Vec<f64> {
        self.values.iter().copied().collect()
    }

    #[must_use]
    pub fn max(&self) -> f64 {
        self.values.iter().copied().fold(0.0, f64::max)
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }
}

/// An SVG path drawing `values` as a line in a `width` × `height` box, scaled to `top` (at least
/// 1, so a flat zero sits on the baseline rather than in the middle).
#[must_use]
pub fn sparkline(values: &[f64], width: f64, height: f64, top: f64) -> String {
    if values.len() < 2 {
        return String::new();
    }
    let top = top.max(1.0);
    let step = width / (values.len() - 1) as f64;
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = i as f64 * step;
            let y = height - (v.max(0.0) / top).min(1.0) * height;
            format!("{} {x:.1} {y:.1}", if i == 0 { "M" } else { "L" })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A rate for a tile: `0`, `0.4`, `12`, `1.3k`.
#[must_use]
pub fn per_second(value: f64) -> String {
    if value >= 1000.0 {
        format!("{:.1}k", value / 1000.0)
    } else if value >= 10.0 {
        format!("{value:.0}")
    } else if value > 0.0 {
        format!("{value:.1}")
    } else {
        "0".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRAPE: &str = "# HELP nusadb_connections_total Total client connections accepted.
# TYPE nusadb_connections_total counter
nusadb_connections_total 12
nusadb_connections_active 3
nusadb_queries_total 400
nusadb_query_errors_total 7
nusadb_database_stopped{database=\"nusadb\"} 0
nusadb_database_stopped{database=\"billing\"} 1
nusadb_backup_last_success_timestamp_seconds 0
nusadb_backup_last_duration_seconds 0.000
nusadb_backup_failures_total 0
nusadb_archive_prune_failures_total 0
";

    #[test]
    fn an_address_becomes_the_url_to_scrape() {
        assert_eq!(normalize_url("").unwrap(), "");
        assert_eq!(
            normalize_url(" 127.0.0.1:9100 ").unwrap(),
            "http://127.0.0.1:9100/metrics"
        );
        assert_eq!(
            normalize_url("http://db1.internal:9100/").unwrap(),
            "http://db1.internal:9100/metrics"
        );
        assert_eq!(
            normalize_url("http://db1:9100/custom").unwrap(),
            "http://db1:9100/custom"
        );
        assert!(normalize_url("https://db1:9100").is_err());
        assert!(normalize_url("db1").is_err());
        assert!(normalize_url("db1:http").is_err());
        assert_eq!(
            split_url("http://db1:9100/metrics"),
            Some(("db1".into(), 9100, "/metrics".into()))
        );
    }

    #[test]
    fn the_documented_metrics_are_read_from_a_scrape() {
        let s = Snapshot::from_samples(&parse(SCRAPE));
        assert!(s.recognised);
        assert_eq!(s.connections_active, 3.0);
        assert_eq!(s.queries_total, 400.0);
        assert_eq!(
            s.databases,
            vec![("billing".into(), true), ("nusadb".into(), false)]
        );
        assert!(!Snapshot::from_samples(&parse("up 1\n")).recognised);
    }

    #[test]
    fn labels_keep_escaped_quotes() {
        let samples = parse("m{a=\"x\\\"y\",b=\"z\"} 2 1700000000\n");
        assert_eq!(
            samples[0].labels,
            vec![("a".into(), "x\"y".into()), ("b".into(), "z".into())]
        );
        assert_eq!(samples[0].value, 2.0);
    }

    #[test]
    fn rates_come_from_two_scrapes_and_a_restart_gives_none() {
        let a = Snapshot::from_samples(&parse(SCRAPE));
        let mut b = a.clone();
        b.queries_total += 50.0;
        b.query_errors_total += 2.0;
        let r = rates(&a, &b, 2.0).unwrap();
        assert_eq!((r.queries, r.errors, r.connections), (25.0, 1.0, 0.0));
        b.queries_total = 3.0;
        assert!(rates(&a, &b, 2.0).is_none());
    }

    #[test]
    fn a_series_keeps_its_last_values_and_draws_them() {
        let mut s = Series::new(3);
        for v in [1.0, 2.0, 3.0, 4.0] {
            s.push(v);
        }
        assert_eq!(s.values(), vec![2.0, 3.0, 4.0]);
        assert_eq!(
            sparkline(&s.values(), 100.0, 20.0, 4.0),
            "M 0.0 10.0 L 50.0 5.0 L 100.0 0.0"
        );
        assert_eq!(per_second(0.0), "0");
        assert_eq!(per_second(0.44), "0.4");
        assert_eq!(per_second(1234.0), "1.2k");
    }
}
