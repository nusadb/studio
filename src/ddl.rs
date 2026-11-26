//! Reconstructing the DDL of an object from its description.
//!
//! The server has no `SHOW CREATE TABLE`, so Studio writes one from what introspection returned:
//! the columns with their types, nullability and defaults, every constraint in its table form,
//! the indexes that no constraint already creates, comments, triggers and row-level security.
//! The result is meant to be read, copied and re-run — so it is ordered the way it has to run.

use crate::backend::{ObjectInfo, ObjectKind, quote_ident};

/// The DDL for `schema.name`, as one script.
#[must_use]
pub fn object_ddl(kind: ObjectKind, schema: &str, name: &str, info: &ObjectInfo) -> String {
    let qualified = format!("{}.{}", quote_ident(schema), quote_ident(name));
    match kind {
        ObjectKind::Table => table_ddl(&qualified, &quote_ident(name), info),
        ObjectKind::View => match &info.definition {
            Some(def) => format!("CREATE VIEW {qualified} AS\n{def};\n"),
            None => {
                "-- The view's query could not be read from information_schema.views.\n".to_owned()
            },
        },
        ObjectKind::MaterializedView => match &info.definition {
            Some(def) => format!("CREATE MATERIALIZED VIEW {qualified} AS\n{def};\n"),
            None => "-- The query behind this materialized view is visible to a superuser only.\n"
                .to_owned(),
        },
        ObjectKind::Sequence => {
            let get = |label: &str| {
                info.details
                    .iter()
                    .find(|(l, _)| l == label)
                    .map(|(_, v)| v.clone())
            };
            let mut out = format!("CREATE SEQUENCE {qualified}");
            if let Some(v) = get("Increment") {
                out.push_str(&format!("\n    INCREMENT BY {v}"));
            }
            if let Some(v) = get("Minimum") {
                out.push_str(&format!("\n    MINVALUE {v}"));
            }
            if let Some(v) = get("Maximum") {
                out.push_str(&format!("\n    MAXVALUE {v}"));
            }
            if let Some(v) = get("Start") {
                out.push_str(&format!("\n    START WITH {v}"));
            }
            if get("Cycles").as_deref() == Some("YES") {
                out.push_str("\n    CYCLE");
            }
            out.push_str(";\n");
            out
        },
        ObjectKind::Function => {
            let get = |label: &str| {
                info.details
                    .iter()
                    .find(|(l, _)| l == label)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            };
            match &info.definition {
                Some(body) => format!(
                    "CREATE OR REPLACE FUNCTION {qualified}({})\nRETURNS {}\nLANGUAGE {}\nAS $$\n{body}\n$$;\n",
                    get("Parameters"),
                    get("Returns"),
                    get("Language"),
                ),
                None => "-- The body of this function is visible to a superuser only.\n".to_owned(),
            }
        },
    }
}

fn table_ddl(qualified: &str, bare: &str, info: &ObjectInfo) -> String {
    let mut lines: Vec<String> = info
        .columns
        .iter()
        .map(|c| {
            let mut line = format!("    {} {}", quote_ident(&c.name), c.type_spec);
            if !c.nullable {
                line.push_str(" NOT NULL");
            }
            if let Some(default) = &c.default {
                line.push_str(" DEFAULT ");
                line.push_str(default);
            }
            line
        })
        .collect();
    for c in &info.constraints {
        let cols = c
            .columns
            .iter()
            .map(|x| quote_ident(x))
            .collect::<Vec<_>>()
            .join(", ");
        let body = match c.kind.as_str() {
            "PRIMARY KEY" => format!("PRIMARY KEY ({cols})"),
            "UNIQUE" => format!("UNIQUE ({cols})"),
            "CHECK" => format!("CHECK ({})", c.check.clone().unwrap_or_default()),
            "FOREIGN KEY" => {
                let mut fk = format!("FOREIGN KEY ({cols})");
                if let Some((table, target)) = &c.references {
                    let target = target
                        .iter()
                        .map(|x| quote_ident(x))
                        .collect::<Vec<_>>()
                        .join(", ");
                    fk.push_str(&format!(" REFERENCES {} ({target})", quote_ident(table)));
                }
                for (what, rule) in [("ON DELETE", &c.on_delete), ("ON UPDATE", &c.on_update)] {
                    if let Some(rule) = rule
                        && rule != "NO ACTION"
                    {
                        fk.push_str(&format!(" {what} {rule}"));
                    }
                }
                fk
            },
            _ => continue,
        };
        lines.push(format!("    CONSTRAINT {} {body}", quote_ident(&c.name)));
    }
    let mut out = format!("CREATE TABLE {qualified} (\n{}\n);\n", lines.join(",\n"));

    // Constraints create their own indexes (and the engine indexes foreign keys); only the
    // ones created on purpose need restating.
    let backed = |name: &str| {
        info.constraints.iter().any(|c| c.name == name)
            || info
                .constraints
                .iter()
                .any(|c| c.kind == "FOREIGN KEY" && name.starts_with(&c.name))
    };
    let own: Vec<_> = info.indexes.iter().filter(|i| !backed(&i.name)).collect();
    if !own.is_empty() {
        out.push_str(
            "\n-- Key columns as information_schema.statistics lists them; a partial WHERE,\n\
             -- DESC order or expression key is not in the catalog; add it back if the index had one.",
        );
    }
    for index in own {
        out.push_str(&format!(
            "\nCREATE {}INDEX {} ON {qualified} ({});",
            if index.unique { "UNIQUE " } else { "" },
            quote_ident(&index.name),
            index
                .columns
                .iter()
                .map(|x| quote_ident(x))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if let Some(comment) = &info.comment {
        out.push_str(&format!(
            "\n\nCOMMENT ON TABLE {qualified} IS {};",
            literal(comment)
        ));
    }
    for c in &info.columns {
        if let Some(comment) = &c.comment {
            out.push_str(&format!(
                "\nCOMMENT ON COLUMN {qualified}.{} IS {};",
                quote_ident(&c.name),
                literal(comment)
            ));
        }
    }

    // NusaDB's trigger and policy catalogs key a table by its bare name, and refuse a schema
    // qualifier in CREATE TRIGGER / CREATE POLICY (0A000) — so those name it unqualified.
    for t in &info.triggers {
        out.push_str(&format!(
            "\n\n-- needs the function it calls to exist first\nCREATE TRIGGER {} {} {} ON {bare}\n    FOR EACH {}{}\n    {};",
            quote_ident(&t.name),
            t.timing.to_uppercase(),
            t.events.to_uppercase().replace(',', " OR"),
            t.for_each.to_uppercase(),
            if t.condition.is_empty() { String::new() } else { format!("\n    WHEN ({})", t.condition) },
            t.action
        ));
        if !t.enabled {
            out.push_str(&format!(
                "\nALTER TABLE {bare} DISABLE TRIGGER {};",
                quote_ident(&t.name)
            ));
        }
    }

    if !info.policies.is_empty() {
        out.push_str(&format!(
            "\n\nALTER TABLE {qualified} ENABLE ROW LEVEL SECURITY;"
        ));
        for p in &info.policies {
            out.push_str(&format!(
                "\nCREATE POLICY {} ON {bare}{} FOR {} TO {}",
                quote_ident(&p.name),
                if p.permissive { "" } else { " AS RESTRICTIVE" },
                p.command,
                if p.roles.is_empty() {
                    "PUBLIC"
                } else {
                    &p.roles
                }
            ));
            if !p.using.is_empty() {
                out.push_str(&format!("\n    USING ({})", p.using));
            }
            if !p.check.is_empty() {
                out.push_str(&format!("\n    WITH CHECK ({})", p.check));
            }
            out.push(';');
        }
    }
    out.push('\n');
    out
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ColumnDef, Constraint, IndexDef, PolicyDef, TriggerDef};

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
                    name: "amount".into(),
                    data_type: "numeric".into(),
                    type_spec: "numeric(12,2)".into(),
                    nullable: true,
                    default: Some("0".into()),
                    comment: Some("in rupiah".into()),
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
                    name: "chk_amount".into(),
                    kind: "CHECK".into(),
                    columns: vec![],
                    references: None,
                    on_update: None,
                    on_delete: None,
                    check: Some("amount >= 0".into()),
                },
            ],
            indexes: vec![
                IndexDef {
                    name: "orders_pkey".into(),
                    unique: true,
                    columns: vec!["id".into()],
                },
                IndexDef {
                    name: "ix_amount".into(),
                    unique: false,
                    columns: vec!["amount".into()],
                },
            ],
            triggers: vec![TriggerDef {
                name: "trg".into(),
                timing: "before".into(),
                events: "update".into(),
                for_each: "row".into(),
                condition: String::new(),
                action: "EXECUTE FUNCTION audit()".into(),
                enabled: true,
            }],
            policies: vec![PolicyDef {
                name: "own".into(),
                command: "SELECT".into(),
                roles: "analyst".into(),
                using: "amount > 0".into(),
                check: String::new(),
                permissive: true,
            }],
            comment: Some("Orders".into()),
            ..ObjectInfo::default()
        }
    }

    #[test]
    fn a_table_is_rebuilt_in_runnable_order() {
        let ddl = object_ddl(ObjectKind::Table, "public", "orders", &orders());
        let expected = [
            "CREATE TABLE \"public\".\"orders\" (",
            "    \"id\" integer NOT NULL,",
            "    \"amount\" numeric(12,2) DEFAULT 0,",
            "    CONSTRAINT \"orders_pkey\" PRIMARY KEY (\"id\"),",
            "    CONSTRAINT \"chk_amount\" CHECK (amount >= 0)",
            ");",
            "CREATE INDEX \"ix_amount\" ON \"public\".\"orders\" (\"amount\");",
            "COMMENT ON TABLE \"public\".\"orders\" IS 'Orders';",
            "COMMENT ON COLUMN \"public\".\"orders\".\"amount\" IS 'in rupiah';",
            "CREATE TRIGGER \"trg\" BEFORE UPDATE ON \"orders\"",
            "ALTER TABLE \"public\".\"orders\" ENABLE ROW LEVEL SECURITY;",
            "CREATE POLICY \"own\" ON \"orders\" FOR SELECT TO analyst",
        ];
        let mut at = 0;
        for line in expected {
            let found = ddl[at..]
                .find(line)
                .unwrap_or_else(|| panic!("missing or out of order: {line}\n{ddl}"));
            at += found + line.len();
        }
        assert!(
            !ddl.contains("INDEX \"orders_pkey\""),
            "the key's own index is not restated"
        );
    }
}
