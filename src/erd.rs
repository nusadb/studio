//! Entity-relationship diagram: the tables of a database, their columns and the foreign keys
//! between them, laid out automatically and drawn as boxes and connectors.
//!
//! Layout is layered: a table that others reference sits left of the tables referencing it, so
//! foreign keys read left to right, from the referenced table to the referencing one. Within
//! a layer, tables are ordered by where their neighbours sit (barycentre) so lines cross less.
//! Tables with no foreign key either way are gathered in a grid after the connected ones.

use std::collections::{BTreeMap, BTreeSet};

/// Box width, header height and row height, in diagram units (logical pixels at 100 %).
pub const BOX_W: f64 = 220.0;
pub const HEADER_H: f64 = 30.0;
pub const ROW_H: f64 = 20.0;
/// Columns listed per box; the rest are summarised in one last row.
pub const MAX_ROWS: usize = 14;
const LAYER_GAP: f64 = 110.0;
const BOX_GAP: f64 = 36.0;
const MARGIN: f64 = 40.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary: bool,
    pub foreign: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub columns: Vec<Column>,
}

impl Table {
    #[must_use]
    pub fn key(&self) -> (String, String) {
        (self.schema.clone(), self.name.clone())
    }

    /// Rows the box shows: the columns up to [`MAX_ROWS`], plus a "+ n more" row.
    #[must_use]
    pub fn rows(&self) -> usize {
        if self.columns.len() > MAX_ROWS {
            MAX_ROWS + 1
        } else {
            self.columns.len().max(1)
        }
    }

    #[must_use]
    pub fn height(&self) -> f64 {
        HEADER_H + self.rows() as f64 * ROW_H + 6.0
    }

    /// Where a column's row sits, for a connector to attach to. A column past the shown rows
    /// attaches to the "+ n more" row; an unknown one to the header.
    #[must_use]
    pub fn row_of(&self, column: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name == column)
            .map(|i| i.min(MAX_ROWS))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub name: String,
    /// The referencing table and its columns.
    pub from: (String, String),
    pub from_columns: Vec<String>,
    /// The referenced table and its columns.
    pub to: (String, String),
    pub to_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Model {
    pub tables: Vec<Table>,
    pub foreign_keys: Vec<ForeignKey>,
}

/// A box's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Top-left corner of each table's box, in `model.tables` order.
#[must_use]
pub fn layout(model: &Model) -> Vec<Point> {
    let n = model.tables.len();
    let index: BTreeMap<(String, String), usize> = model
        .tables
        .iter()
        .enumerate()
        .map(|(i, t)| (t.key(), i))
        .collect();
    // parents[i]: tables i references; children[i]: tables referencing i. Self-references and
    // keys to tables outside the model do not affect placement.
    let mut parents = vec![BTreeSet::new(); n];
    let mut children = vec![BTreeSet::new(); n];
    for fk in &model.foreign_keys {
        if let (Some(&c), Some(&p)) = (index.get(&fk.from), index.get(&fk.to))
            && c != p
        {
            parents[c].insert(p);
            children[p].insert(c);
        }
    }

    // Layer = longest chain of references above it. Bounded passes keep a cycle finite.
    let mut layer = vec![0usize; n];
    for _ in 0..n.max(1) {
        let mut changed = false;
        for i in 0..n {
            let want = parents[i]
                .iter()
                .map(|&p| layer[p] + 1)
                .max()
                .unwrap_or(0)
                .min(n);
            if want != layer[i] {
                layer[i] = want;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let connected: Vec<usize> = (0..n)
        .filter(|&i| !parents[i].is_empty() || !children[i].is_empty())
        .collect();
    let lonely: Vec<usize> = (0..n)
        .filter(|&i| parents[i].is_empty() && children[i].is_empty())
        .collect();

    let layers = connected
        .iter()
        .map(|&i| layer[i])
        .max()
        .map_or(0, |m| m + 1);
    let mut columns: Vec<Vec<usize>> = vec![Vec::new(); layers];
    for &i in &connected {
        columns[layer[i]].push(i);
    }
    // Barycentre ordering, sweeping right then left a few times.
    let mut rank = vec![0.0f64; n];
    for col in &columns {
        for (k, &i) in col.iter().enumerate() {
            rank[i] = k as f64;
        }
    }
    let mean = |set: &BTreeSet<usize>, rank: &[f64], fallback: f64| {
        if set.is_empty() {
            fallback
        } else {
            set.iter().map(|&j| rank[j]).sum::<f64>() / set.len() as f64
        }
    };
    for _ in 0..4 {
        for col in columns.iter_mut().skip(1) {
            col.sort_by(|&a, &b| {
                mean(&parents[a], &rank, rank[a]).total_cmp(&mean(&parents[b], &rank, rank[b]))
            });
            for (k, &i) in col.iter().enumerate() {
                rank[i] = k as f64;
            }
        }
        for c in (0..columns.len().saturating_sub(1)).rev() {
            columns[c].sort_by(|&a, &b| {
                mean(&children[a], &rank, rank[a]).total_cmp(&mean(&children[b], &rank, rank[b]))
            });
            for (k, &i) in columns[c].iter().enumerate() {
                rank[i] = k as f64;
            }
        }
    }

    let mut out = vec![Point { x: 0.0, y: 0.0 }; n];
    for (c, col) in columns.iter().enumerate() {
        let mut y = MARGIN;
        for &i in col {
            out[i] = Point {
                x: MARGIN + c as f64 * (BOX_W + LAYER_GAP),
                y,
            };
            y += model.tables[i].height() + BOX_GAP;
        }
    }

    // Unrelated tables: a grid after the connected layers, about as tall as they are.
    let start_x = MARGIN + layers as f64 * (BOX_W + LAYER_GAP);
    let tallest = columns
        .iter()
        .map(|col| {
            col.iter()
                .map(|&i| model.tables[i].height() + BOX_GAP)
                .sum::<f64>()
        })
        .fold(0.0, f64::max)
        .max(600.0);
    let (mut x, mut y) = (start_x, MARGIN);
    for &i in &lonely {
        let h = model.tables[i].height();
        if y > MARGIN && y + h > MARGIN + tallest {
            x += BOX_W + BOX_GAP;
            y = MARGIN;
        }
        out[i] = Point { x, y };
        y += h + BOX_GAP;
    }
    out
}

/// The diagram's extent, for the canvas and the "fit" zoom.
#[must_use]
pub fn extent(model: &Model, placed: &[Point]) -> (f64, f64) {
    let mut w: f64 = 0.0;
    let mut h: f64 = 0.0;
    for (t, p) in model.tables.iter().zip(placed) {
        w = w.max(p.x + BOX_W);
        h = h.max(p.y + t.height());
    }
    (w + MARGIN, h + MARGIN)
}

fn row_y(p: Point, row: Option<usize>) -> f64 {
    match row {
        Some(r) => p.y + HEADER_H + r as f64 * ROW_H + ROW_H / 2.0 + 3.0,
        None => p.y + HEADER_H / 2.0,
    }
}

/// SVG path data for one foreign key: a curve from the referencing column to the referenced
/// one, a crow's foot ("many") at the referencing end and a bar ("one") at the referenced end.
#[must_use]
pub fn connector(model: &Model, placed: &[Point], fk: &ForeignKey) -> Option<String> {
    let find = |key: &(String, String)| model.tables.iter().position(|t| t.key() == *key);
    let (c, p) = (find(&fk.from)?, find(&fk.to)?);
    let (child, parent) = (&model.tables[c], &model.tables[p]);
    let (cp, pp) = (placed[c], placed[p]);
    let y1 = row_y(
        cp,
        fk.from_columns.first().and_then(|col| child.row_of(col)),
    );
    let y2 = row_y(pp, fk.to_columns.first().and_then(|col| parent.row_of(col)));

    // Which sides face each other. dir1/dir2: +1 leaves rightwards, -1 leftwards.
    let (x1, dir1, x2, dir2) = if c == p {
        (cp.x + BOX_W, 1.0, pp.x + BOX_W, 1.0)
    } else if cp.x >= pp.x + BOX_W {
        (cp.x, -1.0, pp.x + BOX_W, 1.0)
    } else if cp.x + BOX_W <= pp.x {
        (cp.x + BOX_W, 1.0, pp.x, -1.0)
    } else {
        (cp.x + BOX_W, 1.0, pp.x + BOX_W, 1.0)
    };
    let bend = if c == p {
        40.0
    } else {
        ((x2 - x1).abs() / 2.0).clamp(40.0, 120.0)
    };
    let mut d = format!(
        "M {x1:.1} {y1:.1} C {:.1} {y1:.1} {:.1} {y2:.1} {x2:.1} {y2:.1}",
        x1 + dir1 * bend,
        x2 + dir2 * bend
    );
    // Crow's foot at the referencing end.
    let foot = x1 + dir1 * 12.0;
    d.push_str(&format!(
        " M {foot:.1} {y1:.1} L {x1:.1} {:.1} M {foot:.1} {y1:.1} L {x1:.1} {:.1}",
        y1 - 6.0,
        y1 + 6.0
    ));
    // Bar at the referenced end.
    let bar = x2 + dir2 * 9.0;
    d.push_str(&format!(
        " M {bar:.1} {:.1} L {bar:.1} {:.1}",
        y2 - 6.0,
        y2 + 6.0
    ));
    Some(d)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The whole diagram as a standalone SVG document, for export.
#[must_use]
pub fn to_svg(model: &Model, placed: &[Point], title: &str) -> String {
    let (w, h) = extent(model, placed);
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w:.0}\" height=\"{h:.0}\" \
         viewBox=\"0 0 {w:.0} {h:.0}\" font-family=\"Inter, Helvetica, Arial, sans-serif\">\n\
         <title>{}</title>\n<rect width=\"100%\" height=\"100%\" fill=\"#ffffff\"/>\n",
        escape(title)
    );
    for fk in &model.foreign_keys {
        if let Some(d) = connector(model, placed, fk) {
            out.push_str(&format!(
                "<path d=\"{d}\" fill=\"none\" stroke=\"#12806F\" stroke-width=\"1.4\"><title>{}</title></path>\n",
                escape(&fk.name)
            ));
        }
    }
    for (t, p) in model.tables.iter().zip(placed) {
        let th = t.height();
        out.push_str(&format!(
            "<g transform=\"translate({:.1} {:.1})\">\n\
             <rect width=\"{BOX_W}\" height=\"{th:.1}\" rx=\"5\" fill=\"#ffffff\" stroke=\"#C9CDD3\"/>\n\
             <rect width=\"{BOX_W}\" height=\"{HEADER_H}\" rx=\"5\" fill=\"#ECEEF0\"/>\n\
             <text x=\"10\" y=\"19\" font-size=\"12\" font-weight=\"600\" fill=\"#1E2125\">{}</text>\n",
            p.x,
            p.y,
            escape(&if t.schema == "public" { t.name.clone() } else { format!("{}.{}", t.schema, t.name) })
        ));
        for (r, c) in t.columns.iter().take(MAX_ROWS).enumerate() {
            let y = HEADER_H + r as f64 * ROW_H + 15.0;
            let mark = if c.primary {
                "PK"
            } else if c.foreign {
                "FK"
            } else {
                ""
            };
            out.push_str(&format!(
                "<text x=\"10\" y=\"{y:.1}\" font-size=\"9\" font-weight=\"600\" fill=\"#9A6508\">{mark}</text>\
                 <text x=\"30\" y=\"{y:.1}\" font-size=\"11\" fill=\"#1E2125\"{}>{}</text>\
                 <text x=\"{:.0}\" y=\"{y:.1}\" font-size=\"10\" fill=\"#5D636B\" text-anchor=\"end\">{}</text>\n",
                if c.nullable { "" } else { " font-weight=\"600\"" },
                escape(&c.name),
                BOX_W - 10.0,
                escape(&c.data_type)
            ));
        }
        if t.columns.len() > MAX_ROWS {
            let y = HEADER_H + MAX_ROWS as f64 * ROW_H + 15.0;
            out.push_str(&format!(
                "<text x=\"30\" y=\"{y:.1}\" font-size=\"10\" fill=\"#5D636B\">+ {} more</text>\n",
                t.columns.len() - MAX_ROWS
            ));
        }
        out.push_str("</g>\n");
    }
    out.push_str("</svg>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(name: &str, cols: &[&str]) -> Table {
        Table {
            schema: "public".into(),
            name: name.into(),
            columns: cols
                .iter()
                .enumerate()
                .map(|(i, c)| Column {
                    name: (*c).into(),
                    data_type: "int".into(),
                    nullable: i > 0,
                    primary: i == 0,
                    foreign: c.ends_with("_id"),
                })
                .collect(),
        }
    }

    fn fk(from: &str, col: &str, to: &str) -> ForeignKey {
        ForeignKey {
            name: format!("{from}_{col}_fkey"),
            from: ("public".into(), from.into()),
            from_columns: vec![col.into()],
            to: ("public".into(), to.into()),
            to_columns: vec!["id".into()],
        }
    }

    fn shop() -> Model {
        Model {
            tables: vec![
                table("order_lines", &["id", "order_id", "product_id"]),
                table("orders", &["id", "customer_id"]),
                table("customers", &["id", "name"]),
                table("products", &["id"]),
                table("audit", &["id"]),
            ],
            foreign_keys: vec![
                fk("order_lines", "order_id", "orders"),
                fk("order_lines", "product_id", "products"),
                fk("orders", "customer_id", "customers"),
            ],
        }
    }

    #[test]
    fn referenced_tables_sit_left_of_the_tables_referencing_them() {
        let m = shop();
        let p = layout(&m);
        let x = |name: &str| p[m.tables.iter().position(|t| t.name == name).unwrap()].x;
        assert!(x("customers") < x("orders"));
        assert!(x("orders") < x("order_lines"));
        assert!(x("products") < x("order_lines"));
        // The unrelated table goes after every connected layer.
        assert!(x("audit") > x("order_lines"));
    }

    #[test]
    fn boxes_in_one_layer_do_not_overlap() {
        let m = shop();
        let p = layout(&m);
        for i in 0..m.tables.len() {
            for j in i + 1..m.tables.len() {
                let (a, b) = (p[i], p[j]);
                let overlap_x = a.x < b.x + BOX_W && b.x < a.x + BOX_W;
                let overlap_y =
                    a.y < b.y + m.tables[j].height() && b.y < a.y + m.tables[i].height();
                assert!(
                    !(overlap_x && overlap_y),
                    "{} and {}",
                    m.tables[i].name,
                    m.tables[j].name
                );
            }
        }
    }

    #[test]
    fn a_cycle_and_a_self_reference_still_lay_out_and_draw() {
        let mut m = Model {
            tables: vec![table("a", &["id", "b_id"]), table("b", &["id", "a_id"])],
            foreign_keys: vec![
                fk("a", "b_id", "b"),
                fk("b", "a_id", "a"),
                fk("a", "id", "a"),
            ],
        };
        let p = layout(&m);
        assert_eq!(p.len(), 2);
        for k in &m.foreign_keys {
            assert!(connector(&m, &p, k).unwrap().starts_with("M "));
        }
        let names: Vec<String> = (0..20).map(|i| format!("c{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        m.tables[0] = table("a", &refs);
        assert_eq!(m.tables[0].rows(), MAX_ROWS + 1);
        // A column past the shown rows attaches to the "+ n more" row.
        assert_eq!(m.tables[0].row_of("c19"), Some(MAX_ROWS));
    }

    #[test]
    fn a_connector_leaves_the_referencing_column_and_reaches_the_key() {
        let m = shop();
        let p = layout(&m);
        let d = connector(&m, &p, &m.foreign_keys[2]).unwrap();
        let orders = p[1];
        let customers = p[2];
        // Starts on the left edge of orders at its customer_id row (row 1) …
        assert!(d.starts_with(&format!(
            "M {:.1} {:.1}",
            orders.x,
            orders.y + HEADER_H + ROW_H + ROW_H / 2.0 + 3.0
        )));
        // … and ends on the right edge of customers.
        assert!(d.contains(&format!(
            "{:.1} {:.1}",
            customers.x + BOX_W,
            customers.y + HEADER_H + ROW_H / 2.0 + 3.0
        )));
    }

    #[test]
    fn the_svg_export_is_well_formed_and_escaped() {
        let mut m = shop();
        m.tables[4].name = "a<b".into();
        let svg = to_svg(&m, &layout(&m), "shop & co");
        assert!(svg.starts_with("<svg"));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains("a&lt;b"));
        assert!(svg.contains("shop &amp; co"));
        assert_eq!(svg.matches("<path").count(), 3);
    }
}
