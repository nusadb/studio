//! Changes staged in the Data grid, and the SQL that applies them.
//!
//! Edits are held locally until the user saves; saving turns them into one UPDATE per changed
//! row, one INSERT per new row and one DELETE per removed row, which the controller runs inside
//! a single transaction. Rows are addressed by their primary key as it was *loaded*, so editing
//! the key itself still updates the right row.
//!
//! Every value is sent as `CAST('text' AS <column type>)`: the server does not coerce a text
//! literal on assignment, and the cast makes a bad value fail with the server's own message
//! ("invalid Bool value") rather than a type mismatch the user cannot act on.

use std::collections::{BTreeMap, BTreeSet};

use crate::backend::quote_ident;

/// What a staged cell holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// For a new row: leave the column out, so the server applies its default.
    Default,
    Null,
    Text(String),
}

/// One column of the table being edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// The type as `information_schema.columns.data_type` spells it — a valid cast target.
    pub data_type: String,
}

/// Staged changes against the rows currently loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditSet {
    /// (row, column) → new value, for loaded rows.
    pub updates: BTreeMap<(usize, usize), Value>,
    /// New rows, one value per column.
    pub inserts: Vec<Vec<Value>>,
    /// Loaded rows to delete.
    pub deletes: BTreeSet<usize>,
}

impl EditSet {
    /// Number of rows touched, the figure the UI shows as "3 changes".
    #[must_use]
    pub fn count(&self) -> usize {
        let updated: BTreeSet<usize> = self
            .updates
            .keys()
            .map(|(r, _)| *r)
            .filter(|r| !self.deletes.contains(r))
            .collect();
        updated.len() + self.inserts.len() + self.deletes.len()
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// Stage `value` for cell (`row`, `col`), where rows past `loaded` are new rows. Setting a
    /// loaded cell back to what it was unstages it.
    pub fn set(&mut self, loaded: &[Vec<Option<String>>], row: usize, col: usize, value: Value) {
        if row >= loaded.len() {
            if let Some(cells) = self.inserts.get_mut(row - loaded.len())
                && let Some(cell) = cells.get_mut(col)
            {
                *cell = value;
            }
            return;
        }
        let original = loaded[row].get(col).cloned().flatten();
        let unchanged = match (&value, &original) {
            (Value::Null, None) => true,
            (Value::Text(new), Some(old)) => new == old,
            _ => false,
        };
        if unchanged || value == Value::Default {
            self.updates.remove(&(row, col));
        } else {
            self.updates.insert((row, col), value);
        }
    }

    /// Add an empty new row of `columns` cells, every one left to its default.
    pub fn add_row(&mut self, columns: usize) {
        self.inserts.push(vec![Value::Default; columns]);
    }

    /// Delete row `row` — or, if it is already marked, take the mark back. A new row is simply
    /// dropped.
    pub fn toggle_delete(&mut self, loaded: usize, row: usize) {
        if row >= loaded {
            let index = row - loaded;
            if index < self.inserts.len() {
                self.inserts.remove(index);
            }
        } else if !self.deletes.remove(&row) {
            self.deletes.insert(row);
        }
    }

    /// Statements that apply the edits, in the order DELETE, UPDATE, INSERT (so a key freed by
    /// a deletion can be reused by an insert in the same save).
    ///
    /// # Errors
    ///
    /// Fails when there is no primary key to address loaded rows by, or a key value of a row
    /// being changed is NULL.
    pub fn statements(
        &self,
        table: &str,
        columns: &[Column],
        key: &[usize],
        loaded: &[Vec<Option<String>>],
    ) -> Result<Vec<String>, String> {
        let touches_loaded = !self.deletes.is_empty() || !self.updates.is_empty();
        if touches_loaded && key.is_empty() {
            return Err(
                "This table has no primary key, so existing rows cannot be addressed safely."
                    .to_owned(),
            );
        }
        let mut out = Vec::new();

        for row in &self.deletes {
            out.push(format!(
                "DELETE FROM {table} WHERE {}",
                key_clause(columns, key, loaded, *row)?
            ));
        }

        let mut by_row: BTreeMap<usize, Vec<(usize, &Value)>> = BTreeMap::new();
        for ((row, col), value) in &self.updates {
            if !self.deletes.contains(row) {
                by_row.entry(*row).or_default().push((*col, value));
            }
        }
        for (row, cells) in by_row {
            let sets: Vec<String> = cells
                .iter()
                .filter_map(|(col, value)| {
                    let column = columns.get(*col)?;
                    Some(format!(
                        "{} = {}",
                        quote_ident(&column.name),
                        literal(value, &column.data_type)
                    ))
                })
                .collect();
            if sets.is_empty() {
                continue;
            }
            out.push(format!(
                "UPDATE {table} SET {} WHERE {}",
                sets.join(", "),
                key_clause(columns, key, loaded, row)?
            ));
        }

        for values in &self.inserts {
            let given: Vec<(&Column, &Value)> = columns
                .iter()
                .zip(values)
                .filter(|(_, v)| **v != Value::Default)
                .collect();
            if given.is_empty() {
                out.push(format!("INSERT INTO {table} DEFAULT VALUES"));
            } else {
                let names: Vec<String> = given.iter().map(|(c, _)| quote_ident(&c.name)).collect();
                let vals: Vec<String> = given
                    .iter()
                    .map(|(c, v)| literal(v, &c.data_type))
                    .collect();
                out.push(format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    names.join(", "),
                    vals.join(", ")
                ));
            }
        }
        Ok(out)
    }
}

/// `"id" = CAST('7' AS integer) AND …` for the loaded row `row`.
fn key_clause(
    columns: &[Column],
    key: &[usize],
    loaded: &[Vec<Option<String>>],
    row: usize,
) -> Result<String, String> {
    let mut parts = Vec::new();
    for &k in key {
        let column = columns
            .get(k)
            .ok_or("The key column is missing from the result.")?;
        let value = loaded
            .get(row)
            .and_then(|r| r.get(k))
            .cloned()
            .flatten()
            .ok_or_else(|| {
                format!(
                    "Row {} has no value in key column {}.",
                    row + 1,
                    column.name
                )
            })?;
        parts.push(format!(
            "{} = {}",
            quote_ident(&column.name),
            literal(&Value::Text(value), &column.data_type)
        ));
    }
    Ok(parts.join(" AND "))
}

/// A typed SQL literal.
fn literal(value: &Value, data_type: &str) -> String {
    match value {
        Value::Default => "DEFAULT".to_owned(),
        Value::Null => "NULL".to_owned(),
        Value::Text(text) => format!("CAST('{}' AS {data_type})", text.replace('\'', "''")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns() -> Vec<Column> {
        vec![
            Column {
                name: "id".into(),
                data_type: "integer".into(),
            },
            Column {
                name: "name".into(),
                data_type: "text".into(),
            },
            Column {
                name: "active".into(),
                data_type: "boolean".into(),
            },
        ]
    }

    fn loaded() -> Vec<Vec<Option<String>>> {
        vec![
            vec![Some("1".into()), Some("Ayu".into()), Some("true".into())],
            vec![Some("2".into()), None, Some("false".into())],
        ]
    }

    #[test]
    fn an_edit_becomes_a_typed_update_addressed_by_the_loaded_key() {
        let mut edits = EditSet::default();
        edits.set(&loaded(), 0, 1, Value::Text("A'yu".into()));
        edits.set(&loaded(), 0, 2, Value::Text("false".into()));
        let sql = edits
            .statements("\"t\"", &columns(), &[0], &loaded())
            .unwrap();
        assert_eq!(
            sql,
            vec![
                "UPDATE \"t\" SET \"name\" = CAST('A''yu' AS text), \"active\" = CAST('false' AS boolean) \
                 WHERE \"id\" = CAST('1' AS integer)"
            ]
        );
        assert_eq!(edits.count(), 1);
    }

    #[test]
    fn changing_the_key_still_targets_the_original_row() {
        let mut edits = EditSet::default();
        edits.set(&loaded(), 1, 0, Value::Text("20".into()));
        let sql = edits.statements("t", &columns(), &[0], &loaded()).unwrap();
        assert!(
            sql[0].ends_with("WHERE \"id\" = CAST('2' AS integer)"),
            "{}",
            sql[0]
        );
        assert!(sql[0].contains("\"id\" = CAST('20' AS integer)"));
    }

    #[test]
    fn setting_a_cell_back_unstages_it() {
        let mut edits = EditSet::default();
        edits.set(&loaded(), 1, 1, Value::Text("x".into()));
        edits.set(&loaded(), 1, 1, Value::Null);
        assert!(edits.is_empty());
    }

    #[test]
    fn deletes_come_first_and_override_edits_to_the_same_row() {
        let mut edits = EditSet::default();
        edits.set(&loaded(), 0, 1, Value::Text("gone".into()));
        edits.toggle_delete(2, 0);
        let sql = edits.statements("t", &columns(), &[0], &loaded()).unwrap();
        assert_eq!(
            sql,
            vec!["DELETE FROM t WHERE \"id\" = CAST('1' AS integer)"]
        );
        edits.toggle_delete(2, 0);
        assert_eq!(edits.count(), 1, "undeleting brings the edit back");
    }

    #[test]
    fn new_rows_send_only_what_was_typed() {
        let mut edits = EditSet::default();
        edits.add_row(3);
        edits.set(&loaded(), 2, 1, Value::Text("Citra".into()));
        edits.set(&loaded(), 2, 2, Value::Null);
        edits.add_row(3);
        let sql = edits.statements("t", &columns(), &[], &loaded()).unwrap();
        assert_eq!(
            sql,
            vec![
                "INSERT INTO t (\"name\", \"active\") VALUES (CAST('Citra' AS text), NULL)",
                "INSERT INTO t DEFAULT VALUES",
            ]
        );
        edits.toggle_delete(2, 3);
        assert_eq!(edits.inserts.len(), 1, "deleting a new row just drops it");
    }

    #[test]
    fn existing_rows_need_a_primary_key() {
        let mut edits = EditSet::default();
        edits.toggle_delete(2, 1);
        assert!(edits.statements("t", &columns(), &[], &loaded()).is_err());
    }
}
