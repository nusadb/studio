//! The object page: everything known about one table, view, sequence or function, laid out as
//! full-width tabs (Columns, Constraints, Indexes, Triggers, Policies, DDL…).
//!
//! The inspector beside the navigator is a summary; this is where the whole of it is read. Each
//! tab is a table whose columns are wide enough for what they hold (a policy's `USING`
//! expression, a trigger's action), which the narrow inspector could only cut off.

use crate::backend::{ObjectInfo, ObjectKind};
use crate::ddl;

/// One tab of the object page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Stable id the tab strip and the controller agree on: `columns`, `indexes`, `ddl` …
    pub id: &'static str,
    pub title: &'static str,
    /// How many rows it lists; `None` for a code tab.
    pub count: Option<usize>,
    pub headers: Vec<&'static str>,
    /// Column widths in logical pixels, one per header.
    pub widths: Vec<u32>,
    pub rows: Vec<Vec<String>>,
    /// What an empty table says instead of showing nothing.
    pub empty: String,
    /// Source text for a code tab (DDL, definition); empty for a table tab.
    pub code: String,
    /// One line under the table: what the catalog cannot say, and so on.
    pub note: String,
}

impl Page {
    fn table(
        id: &'static str,
        title: &'static str,
        headers: &[(&'static str, u32)],
        rows: Vec<Vec<String>>,
        empty: &str,
    ) -> Self {
        Self {
            id,
            title,
            count: Some(rows.len()),
            headers: headers.iter().map(|h| h.0).collect(),
            widths: headers.iter().map(|h| h.1).collect(),
            rows,
            empty: empty.to_owned(),
            code: String::new(),
            note: String::new(),
        }
    }

    fn code(id: &'static str, title: &'static str, code: String) -> Self {
        Self {
            id,
            title,
            count: None,
            headers: Vec::new(),
            widths: Vec::new(),
            rows: Vec::new(),
            empty: String::new(),
            code,
            note: String::new(),
        }
    }

    /// The tab's label: `Indexes 4`, or `DDL`.
    #[must_use]
    pub fn label(&self) -> String {
        match self.count {
            Some(n) => format!("{}  {n}", self.title),
            None => self.title.to_owned(),
        }
    }
}

fn yes(flag: bool) -> String {
    if flag {
        "yes".to_owned()
    } else {
        String::new()
    }
}

/// The tabs for `kind`, in reading order. `info` is what `describe` returned.
#[must_use]
pub fn pages(kind: ObjectKind, schema: &str, name: &str, info: &ObjectInfo) -> Vec<Page> {
    let mut out = Vec::new();
    if kind.has_rows() {
        let mut page = columns(kind, info);
        if let Some(comment) = info.comment.as_ref().filter(|c| !c.is_empty()) {
            page.note = if page.note.is_empty() {
                format!("Comment: {comment}")
            } else {
                format!("Comment: {comment}. {}", page.note)
            };
        }
        out.push(page);
    }
    if kind == ObjectKind::Table {
        out.push(constraints(info));
        out.push(indexes(info));
        out.push(triggers(info));
        out.push(policies(info));
    }
    if matches!(kind, ObjectKind::Sequence | ObjectKind::Function) {
        let rows: Vec<Vec<String>> = info
            .details
            .iter()
            .map(|(k, v)| vec![k.clone(), v.clone()])
            .collect();
        out.push(Page::table(
            "overview",
            "Overview",
            &[("Property", 200), ("Value", 420)],
            rows,
            "Nothing more is recorded about it.",
        ));
    }
    // A view's or materialized view's query is already the body of its DDL.
    if let Some(definition) = info
        .definition
        .as_ref()
        .filter(|d| kind == ObjectKind::Function && !d.trim().is_empty())
    {
        out.push(Page::code("definition", "Definition", definition.clone()));
    }
    if kind != ObjectKind::Function {
        out.push(Page::code(
            "ddl",
            "DDL",
            ddl::object_ddl(kind, schema, name, info),
        ));
    }
    if let Some(notes) = (!info.notes.is_empty()).then(|| info.notes.join(" ")) {
        for page in out.iter_mut().filter(|p| p.code.is_empty()) {
            page.note = if page.note.is_empty() {
                notes.clone()
            } else {
                format!("{} {notes}", page.note)
            };
        }
    }
    out
}

fn columns(kind: ObjectKind, info: &ObjectInfo) -> Page {
    let key_of = |column: &str| {
        let mut tags = Vec::new();
        for c in &info.constraints {
            if !c.columns.iter().any(|x| x == column) {
                continue;
            }
            match c.kind.as_str() {
                "PRIMARY KEY" => tags.push("PK".to_owned()),
                "UNIQUE" => tags.push("unique".to_owned()),
                "FOREIGN KEY" => tags.push(match &c.references {
                    Some((table, cols)) => format!("FK → {table}({})", cols.join(", ")),
                    None => "FK".to_owned(),
                }),
                _ => {},
            }
        }
        tags.join(", ")
    };
    let rows = info
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            vec![
                (i + 1).to_string(),
                c.name.clone(),
                c.type_spec.clone(),
                if c.nullable {
                    String::new()
                } else {
                    "NOT NULL".to_owned()
                },
                c.default.clone().unwrap_or_default(),
                key_of(&c.name),
                c.comment.clone().unwrap_or_default(),
            ]
        })
        .collect();
    let mut page = Page::table(
        "columns",
        "Columns",
        &[
            ("#", 36),
            ("Name", 170),
            ("Type", 150),
            ("Null", 100),
            ("Default", 170),
            ("Key", 200),
            ("Comment", 260),
        ],
        rows,
        "No columns.",
    );
    if kind != ObjectKind::Table {
        page.note = "A view's columns come from its query; their keys and defaults are those \
                     of the tables it reads."
            .to_owned();
    }
    page
}

fn constraints(info: &ObjectInfo) -> Page {
    let rows = info
        .constraints
        .iter()
        .map(|c| {
            let definition = match (&c.check, &c.references) {
                (Some(check), _) => check.clone(),
                (None, Some((table, cols))) => format!("→ {table} ({})", cols.join(", ")),
                (None, None) => String::new(),
            };
            let rule = |r: &Option<String>| {
                r.as_ref()
                    .filter(|r| r.as_str() != "NO ACTION")
                    .cloned()
                    .unwrap_or_default()
            };
            vec![
                c.kind.clone(),
                c.name.clone(),
                c.columns.join(", "),
                definition,
                rule(&c.on_delete),
                rule(&c.on_update),
            ]
        })
        .collect();
    Page::table(
        "constraints",
        "Constraints",
        &[
            ("Type", 110),
            ("Name", 190),
            ("Columns", 170),
            ("Check / references", 280),
            ("On delete", 100),
            ("On update", 100),
        ],
        rows,
        "No constraints.",
    )
}

fn indexes(info: &ObjectInfo) -> Page {
    let rows = info
        .indexes
        .iter()
        .map(|i| {
            let backs = info.constraints.iter().find(|c| {
                c.name == i.name || (c.kind == "FOREIGN KEY" && i.name.starts_with(&c.name))
            });
            vec![
                i.name.clone(),
                i.columns.join(", "),
                yes(i.unique),
                backs.map_or_else(
                    || "created".to_owned(),
                    |c| format!("{} {}", c.kind.to_lowercase(), c.name),
                ),
            ]
        })
        .collect();
    let mut page = Page::table(
        "indexes",
        "Indexes",
        &[
            ("Name", 220),
            ("Key columns", 260),
            ("Unique", 80),
            ("Origin", 280),
        ],
        rows,
        "No indexes.",
    );
    page.note = "NusaDB's information_schema.statistics lists key columns only: a partial WHERE, \
                 DESC order or expression key is not shown."
        .to_owned();
    page
}

fn triggers(info: &ObjectInfo) -> Page {
    let rows = info
        .triggers
        .iter()
        .map(|t| {
            vec![
                t.name.clone(),
                t.timing.to_uppercase(),
                t.events.to_uppercase(),
                t.for_each.to_uppercase(),
                t.condition.clone(),
                t.action.clone(),
                if t.enabled {
                    "enabled".to_owned()
                } else {
                    "disabled".to_owned()
                },
            ]
        })
        .collect();
    Page::table(
        "triggers",
        "Triggers",
        &[
            ("Name", 190),
            ("Timing", 90),
            ("Events", 150),
            ("For each", 90),
            ("When", 180),
            ("Action", 260),
            ("State", 80),
        ],
        rows,
        "No triggers.",
    )
}

fn policies(info: &ObjectInfo) -> Page {
    let rows = info
        .policies
        .iter()
        .map(|p| {
            vec![
                p.name.clone(),
                p.command.to_uppercase(),
                if p.permissive {
                    "permissive"
                } else {
                    "restrictive"
                }
                .to_owned(),
                if p.roles.is_empty() {
                    "PUBLIC".to_owned()
                } else {
                    p.roles.replace(',', ", ")
                },
                p.using.clone(),
                p.check.clone(),
            ]
        })
        .collect();
    Page::table(
        "policies",
        "Policies",
        &[
            ("Name", 170),
            ("Command", 90),
            ("Kind", 100),
            ("Roles", 170),
            ("USING", 240),
            ("WITH CHECK", 240),
        ],
        rows,
        "No row-level security policies.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ColumnDef, Constraint, IndexDef, PolicyDef};

    fn orders() -> ObjectInfo {
        ObjectInfo {
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: "integer".into(),
                    type_spec: "integer".into(),
                    nullable: false,
                    default: None,
                    comment: None,
                },
                ColumnDef {
                    name: "customer".into(),
                    data_type: "integer".into(),
                    type_spec: "integer".into(),
                    nullable: true,
                    default: Some("0".into()),
                    comment: Some("who ordered".into()),
                },
            ],
            constraints: vec![
                Constraint {
                    name: "orders_pkey".into(),
                    kind: "PRIMARY KEY".into(),
                    columns: vec!["id".into()],
                    references: None,
                    on_update: None,
                    on_delete: None,
                    check: None,
                },
                Constraint {
                    name: "orders_customer_fkey".into(),
                    kind: "FOREIGN KEY".into(),
                    columns: vec!["customer".into()],
                    references: Some(("customers".into(), vec!["id".into()])),
                    on_update: Some("NO ACTION".into()),
                    on_delete: Some("CASCADE".into()),
                    check: None,
                },
            ],
            indexes: vec![
                IndexDef {
                    name: "orders_pkey".into(),
                    unique: true,
                    columns: vec!["id".into()],
                },
                IndexDef {
                    name: "ix_customer".into(),
                    unique: false,
                    columns: vec!["customer".into()],
                },
            ],
            policies: vec![PolicyDef {
                name: "own".into(),
                command: "SELECT".into(),
                roles: "analyst,readers".into(),
                using: "customer = 1".into(),
                check: String::new(),
                permissive: false,
            }],
            ..ObjectInfo::default()
        }
    }

    #[test]
    fn a_table_gets_every_tab_with_full_values() {
        let pages = pages(ObjectKind::Table, "public", "orders", &orders());
        let ids: Vec<&str> = pages.iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            [
                "columns",
                "constraints",
                "indexes",
                "triggers",
                "policies",
                "ddl"
            ]
        );
        let cols = &pages[0];
        assert_eq!(cols.rows[0][5], "PK");
        assert_eq!(cols.rows[1][5], "FK → customers(id)");
        assert_eq!(cols.rows[1][6], "who ordered");
        assert_eq!(pages[1].rows[1][4], "CASCADE");
        assert_eq!(pages[1].rows[1][5], "");
        assert_eq!(pages[2].rows[0][3], "primary key orders_pkey");
        assert_eq!(pages[2].rows[1][3], "created");
        assert_eq!(pages[4].rows[0][3], "analyst, readers");
        assert_eq!(pages[4].rows[0][2], "restrictive");
        assert!(pages[5].code.starts_with("CREATE TABLE"));
        assert_eq!(pages[2].label(), "Indexes  2");
        assert_eq!(pages[5].label(), "DDL");
    }

    #[test]
    fn a_function_shows_its_overview_and_body() {
        let info = ObjectInfo {
            details: vec![("Returns".into(), "integer".into())],
            definition: Some("SELECT x + 1".into()),
            ..ObjectInfo::default()
        };
        let pages = pages(ObjectKind::Function, "public", "add_one", &info);
        let ids: Vec<&str> = pages.iter().map(|p| p.id).collect();
        assert_eq!(ids, ["overview", "definition"]);
        assert_eq!(pages[0].rows[0], vec!["Returns", "integer"]);
    }
}
