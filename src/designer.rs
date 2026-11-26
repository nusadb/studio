//! The table designer's model, and the DDL it produces.
//!
//! Names follow NusaDB's identifier rules (`docs/sql-reference.md`, "Identifiers fold to lower
//! case"): ASCII letters, digits and `_`, folded to lower case — what the engine itself does with
//! an unquoted name — so a table typed as `Invoices` is created as `invoices`, not as a
//! case-sensitive `"Invoices"` that every later query would have to quote.
//!
//! A new table becomes one `CREATE TABLE`. An existing table is diffed against how it was
//! loaded, and only what changed becomes `ALTER TABLE` statements — in an order that always
//! works: the old primary key goes before columns are dropped or retyped, new columns arrive
//! before the new key that may use them, and the table is renamed last so every earlier
//! statement can still name it.

use crate::backend::quote_ident;

/// One column as the designer edits it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColumnSpec {
    pub name: String,
    /// The declared type, as typed: `int`, `varchar(80)`, `numeric(12,2)`.
    pub data_type: String,
    pub nullable: bool,
    /// A SQL expression, or empty for no default: `0`, `'draft'`, `now()`.
    pub default: String,
    pub primary: bool,
    /// The column's name when the table was loaded; `None` for a column added here.
    pub original: Option<String>,
}

/// An index as the designer edits it. Indexes that back a constraint (the primary key, a
/// UNIQUE constraint, a foreign key) are not listed here: they change with the constraint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexSpec {
    pub name: String,
    /// Column list as typed: `customer, created_at DESC`.
    pub columns: String,
    pub unique: bool,
    /// Partial-index predicate, without `WHERE`; empty for a full index.
    pub predicate: String,
    /// The index's name when the table was loaded; `None` for one added here.
    pub original: Option<String>,
}

/// A table as the designer edits it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableSpec {
    pub schema: String,
    pub name: String,
    pub columns: Vec<ColumnSpec>,
    /// Name of the loaded primary-key constraint, so a key change can drop it.
    pub primary_key_name: Option<String>,
    pub indexes: Vec<IndexSpec>,
}

/// A name as NusaDB stores an unquoted identifier: trimmed and lower-cased.
#[must_use]
pub fn ident(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Whether `name` is a valid NusaDB identifier: ASCII letters, digits and `_`, not starting
/// with a digit.
pub fn valid_ident(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl TableSpec {
    fn qualified(&self) -> String {
        format!(
            "{}.{}",
            quote_ident(&ident(&self.schema)),
            quote_ident(&ident(&self.name))
        )
    }

    fn key(&self) -> Vec<String> {
        self.columns
            .iter()
            .filter(|c| c.primary)
            .map(|c| ident(&c.name))
            .collect()
    }

    /// Problems that would make the DDL fail, in the order a reader would fix them.
    ///
    /// # Errors
    ///
    /// The first problem found, phrased for the user.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Give the table a name.".to_owned());
        }
        if !valid_ident(&self.name) {
            return Err(
                "A table name uses letters, digits and _ only, and does not start with a digit."
                    .to_owned(),
            );
        }
        if !valid_ident(&self.schema) {
            return Err("The schema name uses letters, digits and _ only.".to_owned());
        }
        if self.columns.is_empty() {
            return Err("A table needs at least one column.".to_owned());
        }
        for (i, c) in self.columns.iter().enumerate() {
            if c.name.trim().is_empty() {
                return Err(format!("Column {} has no name.", i + 1));
            }
            if !valid_ident(&c.name) {
                return Err(format!(
                    "Column name “{}” may use letters, digits and _ only.",
                    c.name.trim()
                ));
            }
            if c.data_type.trim().is_empty() {
                return Err(format!("Column {} has no type.", c.name));
            }
            if self.columns[..i]
                .iter()
                .any(|o| o.name.eq_ignore_ascii_case(&c.name))
            {
                return Err(format!("Two columns are called {}.", c.name));
            }
        }
        for (i, x) in self.indexes.iter().enumerate() {
            if x.name.trim().is_empty() {
                return Err(format!("Index {} has no name. NusaDB needs one.", i + 1));
            }
            if !valid_ident(&x.name) {
                return Err(format!(
                    "Index name “{}” may use letters, digits and _ only.",
                    x.name.trim()
                ));
            }
            if x.columns.trim().is_empty() {
                return Err(format!("Index {} lists no columns.", x.name.trim()));
            }
            if self.indexes[..i]
                .iter()
                .any(|o| ident(&o.name) == ident(&x.name))
            {
                return Err(format!("Two indexes are called {}.", x.name.trim()));
            }
        }
        Ok(())
    }
}

/// `"name" type [NOT NULL] [DEFAULT expr]`
fn column_clause(c: &ColumnSpec) -> String {
    let mut out = format!("{} {}", quote_ident(&ident(&c.name)), c.data_type.trim());
    if !c.nullable && !c.primary {
        out.push_str(" NOT NULL");
    }
    if !c.default.trim().is_empty() {
        out.push_str(" DEFAULT ");
        out.push_str(c.default.trim());
    }
    out
}

/// The `CREATE TABLE` for a new table, laid out one column per line.
///
/// # Errors
///
/// What makes the spec invalid.
pub fn create_sql(spec: &TableSpec) -> Result<String, String> {
    spec.validate()?;
    let mut lines: Vec<String> = spec
        .columns
        .iter()
        .map(|c| format!("    {}", column_clause(c)))
        .collect();
    let key = spec.key();
    if !key.is_empty() {
        let cols: Vec<String> = key.iter().map(|k| quote_ident(k)).collect();
        lines.push(format!("    PRIMARY KEY ({})", cols.join(", ")));
    }
    Ok(format!(
        "CREATE TABLE {} (\n{}\n)",
        spec.qualified(),
        lines.join(",\n")
    ))
}

/// `CREATE [UNIQUE] INDEX name ON table (cols) [WHERE …]`
fn create_index(table: &str, index: &IndexSpec) -> String {
    // A vector operator class (`emb vector_cosine_ops`) makes it an HNSW index.
    let hnsw = [
        "vector_l2_ops",
        "vector_cosine_ops",
        "vector_ip_ops",
        "vector_l1_ops",
    ]
    .iter()
    .any(|op| index.columns.contains(op));
    let mut out = format!(
        "CREATE {}INDEX {} ON {table} {}({})",
        if index.unique { "UNIQUE " } else { "" },
        quote_ident(&ident(&index.name)),
        if hnsw { "USING hnsw " } else { "" },
        index.columns.trim()
    );
    if !index.predicate.trim().is_empty() {
        out.push_str(" WHERE ");
        out.push_str(index.predicate.trim());
    }
    out
}

/// Every statement that creates `spec`: the table, then its indexes.
///
/// # Errors
///
/// What makes the spec invalid.
pub fn create_all(spec: &TableSpec) -> Result<Vec<String>, String> {
    let mut out = vec![create_sql(spec)?];
    let table = spec.qualified();
    out.extend(spec.indexes.iter().map(|i| create_index(&table, i)));
    Ok(out)
}

/// The statements that turn `before` into `after`; empty when nothing changed.
///
/// # Errors
///
/// What makes `after` invalid.
pub fn alter_sql(before: &TableSpec, after: &TableSpec) -> Result<Vec<String>, String> {
    after.validate()?;
    // Every statement below runs before the rename, so all of them use the old name.
    let table = before.qualified();
    let mut out: Vec<String> = Vec::new();

    // NusaDB has no ALTER INDEX: an index whose name or definition changed is dropped and
    // created again. Drops run first, before a column they cover can go; creates run last,
    // once every column they name exists (and before the table rename, under the old name).
    let kept = |old: &IndexSpec| {
        after
            .indexes
            .iter()
            .find(|i| i.original.as_deref() == Some(old.name.as_str()))
    };
    let same = |a: &IndexSpec, b: &IndexSpec| {
        ident(&a.name) == ident(&b.name)
            && a.columns.trim() == b.columns.trim()
            && a.unique == b.unique
            && a.predicate.trim() == b.predicate.trim()
    };
    let mut recreate: Vec<&IndexSpec> = Vec::new();
    for old in &before.indexes {
        match kept(old) {
            Some(now) if same(old, now) => {},
            Some(now) => {
                out.push(format!("DROP INDEX {}", quote_ident(&old.name)));
                recreate.push(now);
            },
            None => out.push(format!("DROP INDEX {}", quote_ident(&old.name))),
        }
    }
    recreate.extend(after.indexes.iter().filter(|i| i.original.is_none()));

    let key_changed = before.key() != after.key()
        || after
            .columns
            .iter()
            .any(|c| c.primary && c.original.as_deref() != Some(c.name.as_str()));
    let old_key: Vec<String> = before
        .columns
        .iter()
        .filter(|c| c.primary)
        .map(|c| c.name.clone())
        .collect();
    if key_changed && !old_key.is_empty() {
        let name = before
            .primary_key_name
            .clone()
            .unwrap_or_else(|| format!("{}_pkey", before.name));
        out.push(format!(
            "ALTER TABLE {table} DROP CONSTRAINT {}",
            quote_ident(&name)
        ));
    }

    for old in &before.columns {
        if !after
            .columns
            .iter()
            .any(|c| c.original.as_deref() == Some(old.name.as_str()))
        {
            out.push(format!(
                "ALTER TABLE {table} DROP COLUMN {}",
                quote_ident(&old.name)
            ));
        }
    }

    for c in &after.columns {
        let Some(original) = &c.original else {
            continue;
        };
        let Some(old) = before.columns.iter().find(|o| o.name == *original) else {
            continue;
        };
        let name = quote_ident(&ident(&c.name));
        if ident(&c.name) != old.name {
            out.push(format!(
                "ALTER TABLE {table} RENAME COLUMN {} TO {name}",
                quote_ident(&old.name)
            ));
        }
        if c.data_type.trim() != old.data_type.trim() {
            out.push(format!(
                "ALTER TABLE {table} ALTER COLUMN {name} TYPE {}",
                c.data_type.trim()
            ));
        }
        // A key column is NOT NULL by being in the key; only non-key columns carry the flag.
        let was_required = !old.nullable || old.primary;
        let now_required = !c.nullable || c.primary;
        if was_required != now_required && !c.primary {
            out.push(format!(
                "ALTER TABLE {table} ALTER COLUMN {name} {} NOT NULL",
                if now_required { "SET" } else { "DROP" }
            ));
        }
        if c.default.trim() != old.default.trim() {
            if c.default.trim().is_empty() {
                out.push(format!(
                    "ALTER TABLE {table} ALTER COLUMN {name} DROP DEFAULT"
                ));
            } else {
                out.push(format!(
                    "ALTER TABLE {table} ALTER COLUMN {name} SET DEFAULT {}",
                    c.default.trim()
                ));
            }
        }
    }

    for c in after.columns.iter().filter(|c| c.original.is_none()) {
        out.push(format!(
            "ALTER TABLE {table} ADD COLUMN {}",
            column_clause(c)
        ));
    }

    let new_key = after.key();
    if key_changed && !new_key.is_empty() {
        let cols: Vec<String> = new_key.iter().map(|k| quote_ident(k.trim())).collect();
        out.push(format!(
            "ALTER TABLE {table} ADD PRIMARY KEY ({})",
            cols.join(", ")
        ));
    }

    out.extend(recreate.into_iter().map(|i| create_index(&table, i)));

    if after.name.trim() != before.name {
        out.push(format!(
            "ALTER TABLE {table} RENAME TO {}",
            quote_ident(after.name.trim())
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, nullable: bool, primary: bool) -> ColumnSpec {
        ColumnSpec {
            name: name.into(),
            data_type: ty.into(),
            nullable,
            default: String::new(),
            primary,
            original: Some(name.into()),
        }
    }

    fn orders() -> TableSpec {
        TableSpec {
            schema: "public".into(),
            name: "orders".into(),
            columns: vec![
                col("id", "integer", false, true),
                col("note", "text", true, false),
            ],
            primary_key_name: Some("orders_pkey".into()),
            indexes: vec![IndexSpec {
                name: "ix_note".into(),
                columns: "note".into(),
                unique: false,
                predicate: String::new(),
                original: Some("ix_note".into()),
            }],
        }
    }

    #[test]
    fn names_fold_to_lower_case_and_must_be_plain_identifiers() {
        let mut spec = orders();
        for c in &mut spec.columns {
            c.original = None;
        }
        spec.indexes.clear();
        spec.name = "Invoices".into();
        spec.columns[1].name = "Note".into();
        let sql = create_sql(&spec).unwrap();
        assert!(
            sql.starts_with("CREATE TABLE \"public\".\"invoices\""),
            "{sql}"
        );
        assert!(sql.contains("\"note\" text"));
        spec.columns[1].name = "due date".into();
        assert!(
            spec.validate()
                .unwrap_err()
                .contains("letters, digits and _")
        );
    }

    #[test]
    fn index_changes_drop_first_and_create_last() {
        let before = orders();
        let mut after = orders();
        after.indexes[0].unique = true;
        after.indexes.push(IndexSpec {
            name: "ix_recent".into(),
            columns: "id DESC".into(),
            unique: false,
            predicate: "id > 100".into(),
            original: None,
        });
        let sql = alter_sql(&before, &after).unwrap();
        assert_eq!(
            sql.first().map(String::as_str),
            Some("DROP INDEX \"ix_note\"")
        );
        assert!(sql.contains(
            &"CREATE UNIQUE INDEX \"ix_note\" ON \"public\".\"orders\" (note)".to_owned()
        ));
        assert!(
            sql.contains(
                &"CREATE INDEX \"ix_recent\" ON \"public\".\"orders\" (id DESC) WHERE id > 100"
                    .to_owned()
            )
        );
    }

    #[test]
    fn a_vector_opclass_makes_an_hnsw_index() {
        let index = IndexSpec {
            name: "docs_emb".into(),
            columns: "emb vector_cosine_ops".into(),
            unique: false,
            predicate: String::new(),
            original: None,
        };
        assert_eq!(
            create_index("\"public\".\"docs\"", &index),
            "CREATE INDEX \"docs_emb\" ON \"public\".\"docs\" USING hnsw (emb vector_cosine_ops)"
        );
    }

    #[test]
    fn a_removed_index_is_dropped_and_an_unnamed_one_is_refused() {
        let before = orders();
        let mut after = orders();
        after.indexes.clear();
        assert_eq!(
            alter_sql(&before, &after).unwrap(),
            vec!["DROP INDEX \"ix_note\""]
        );
        after.indexes.push(IndexSpec {
            columns: "note".into(),
            ..IndexSpec::default()
        });
        assert!(alter_sql(&before, &after).unwrap_err().contains("no name"));
    }

    #[test]
    fn a_new_table_is_one_readable_create() {
        let mut spec = orders();
        for c in &mut spec.columns {
            c.original = None;
        }
        spec.columns[1].default = "'draft'".into();
        spec.columns[1].nullable = false;
        assert_eq!(
            create_sql(&spec).unwrap(),
            "CREATE TABLE \"public\".\"orders\" (\n    \"id\" integer,\n    \"note\" text NOT NULL DEFAULT 'draft',\n    PRIMARY KEY (\"id\")\n)"
        );
    }

    #[test]
    fn nothing_changed_means_no_statements() {
        assert!(alter_sql(&orders(), &orders()).unwrap().is_empty());
    }

    #[test]
    fn each_change_becomes_its_own_alter() {
        let before = orders();
        let mut after = orders();
        after.columns[1].name = "memo".into();
        after.columns[1].data_type = "varchar(200)".into();
        after.columns[1].nullable = false;
        after.columns[1].default = "''".into();
        after.columns.push(ColumnSpec {
            name: "total".into(),
            data_type: "numeric(12,2)".into(),
            nullable: true,
            default: "0".into(),
            primary: false,
            original: None,
        });
        after.name = "purchase_orders".into();
        let sql = alter_sql(&before, &after).unwrap();
        assert_eq!(
            sql,
            vec![
                "ALTER TABLE \"public\".\"orders\" RENAME COLUMN \"note\" TO \"memo\"",
                "ALTER TABLE \"public\".\"orders\" ALTER COLUMN \"memo\" TYPE varchar(200)",
                "ALTER TABLE \"public\".\"orders\" ALTER COLUMN \"memo\" SET NOT NULL",
                "ALTER TABLE \"public\".\"orders\" ALTER COLUMN \"memo\" SET DEFAULT ''",
                "ALTER TABLE \"public\".\"orders\" ADD COLUMN \"total\" numeric(12,2) DEFAULT 0",
                "ALTER TABLE \"public\".\"orders\" RENAME TO \"purchase_orders\"",
            ]
        );
    }

    #[test]
    fn a_removed_column_is_dropped_and_a_key_change_rebuilds_the_key() {
        let before = orders();
        let mut after = orders();
        after.columns.remove(1);
        after.columns.push(ColumnSpec {
            name: "line".into(),
            data_type: "int".into(),
            nullable: false,
            default: String::new(),
            primary: true,
            original: None,
        });
        let sql = alter_sql(&before, &after).unwrap();
        assert_eq!(
            sql[0],
            "ALTER TABLE \"public\".\"orders\" DROP CONSTRAINT \"orders_pkey\""
        );
        assert_eq!(
            sql[1],
            "ALTER TABLE \"public\".\"orders\" DROP COLUMN \"note\""
        );
        assert!(sql[2].contains("ADD COLUMN \"line\" int"));
        assert_eq!(
            sql[3],
            "ALTER TABLE \"public\".\"orders\" ADD PRIMARY KEY (\"id\", \"line\")"
        );
    }

    #[test]
    fn invalid_specs_say_what_to_fix() {
        let mut spec = orders();
        spec.columns[1].name = "ID".into();
        assert_eq!(spec.validate().unwrap_err(), "Two columns are called ID.");
        spec.columns[1].name = String::new();
        assert_eq!(spec.validate().unwrap_err(), "Column 2 has no name.");
        spec.name = " ".into();
        assert_eq!(spec.validate().unwrap_err(), "Give the table a name.");
    }
}
