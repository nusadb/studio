//! Roles, grants and row-level security of one database, and the SQL that changes them.
//!
//! Everything here follows NusaDB's access-control model (`docs/sql-reference.md`, *Access
//! control*; `docs/deployment.md`, *Authentication*):
//!
//! - Roles live **inside each database**. Who may log in, and with which password, is the
//!   server's `--auth-user` list; `CREATE ROLE … PASSWORD` is refused, so there is no password here.
//! - Role attributes are `SUPERUSER`, `LOGIN`, `CREATEDB`, `CREATEROLE` and `INHERIT` (each with a
//!   `NO` form). There is no `RENAME`, `CONNECTION LIMIT` or `VALID UNTIL`.
//! - Table privileges are `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, `REFERENCES` and
//!   `TRIGGER`, each optionally `WITH GRANT OPTION`.
//! - A policy's command and kind cannot be altered (`ALTER POLICY` takes `TO`, `USING` and `WITH
//!   CHECK` only, and no `RENAME`), so changing those drops and recreates it. `CREATE POLICY`
//!   takes a bare table name, so only tables in `public` can carry one.
//!
//! Role and grant statements are transactional, so a whole set of changes runs in one
//! transaction and either all of it lands or none of it does.

use std::collections::{BTreeMap, BTreeSet};

use crate::backend::quote_ident;
use crate::designer::valid_ident;

/// The table privileges NusaDB knows, in the order the matrix shows them.
pub const PRIVILEGES: [&str; 7] = [
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
];

/// The commands a policy can cover.
pub const COMMANDS: [&str; 5] = ["ALL", "SELECT", "INSERT", "UPDATE", "DELETE"];

/// The pseudo-role every role belongs to. `information_schema` reports it in lower case.
pub const PUBLIC: &str = "public";

/// A role and its attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub name: String,
    pub superuser: bool,
    pub login: bool,
    pub createdb: bool,
    pub createrole: bool,
    pub inherit: bool,
    /// Whether the role exists in the database; `false` for one added here.
    pub exists: bool,
}

impl Role {
    /// A new role as `CREATE ROLE` makes it: `NOLOGIN`, `INHERIT`, nothing else.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            superuser: false,
            login: false,
            createdb: false,
            createrole: false,
            inherit: true,
            exists: false,
        }
    }

    /// The attribute keywords that differ from `CREATE ROLE`'s defaults, or from `before`.
    fn attributes(&self, before: Option<&Role>) -> Vec<&'static str> {
        let base = before.cloned().unwrap_or_else(|| Role::new(&self.name));
        let mut out = Vec::new();
        let mut flag = |now: bool, was: bool, on: &'static str, off: &'static str| {
            if now != was {
                out.push(if now { on } else { off });
            }
        };
        flag(self.login, base.login, "LOGIN", "NOLOGIN");
        flag(self.superuser, base.superuser, "SUPERUSER", "NOSUPERUSER");
        flag(self.createdb, base.createdb, "CREATEDB", "NOCREATEDB");
        flag(
            self.createrole,
            base.createrole,
            "CREATEROLE",
            "NOCREATEROLE",
        );
        flag(self.inherit, base.inherit, "INHERIT", "NOINHERIT");
        out
    }
}

/// `member` belongs to `role`; `admin` lets it grant `role` on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Membership {
    pub role: String,
    pub member: String,
    pub admin: bool,
}

/// One cell of the privilege matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grant {
    #[default]
    None,
    Granted,
    /// Granted, and the grantee may grant it on.
    WithGrantOption,
}

impl Grant {
    /// The next state when the cell is clicked: none → granted → with grant option → none.
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::None => Self::Granted,
            Self::Granted => Self::WithGrantOption,
            Self::WithGrantOption => Self::None,
        }
    }
}

/// A table or view privileges can be granted on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Relation {
    pub schema: String,
    pub name: String,
    pub view: bool,
}

/// `(grantee, schema, table, privilege index)`.
pub type GrantKey = (String, String, String, usize);

/// A row-level security policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub schema: String,
    pub table: String,
    pub name: String,
    /// One of [`COMMANDS`].
    pub command: String,
    /// Comma-separated role names; empty means `PUBLIC`.
    pub roles: String,
    pub using: String,
    pub check: String,
    pub permissive: bool,
    /// The policy's name when it was loaded; `None` for one added here.
    pub original: Option<String>,
}

impl Policy {
    /// The role list as `CREATE POLICY … TO` wants it.
    fn to_clause(&self) -> String {
        let names = split_roles(&self.roles);
        if names.is_empty() {
            "PUBLIC".to_owned()
        } else {
            names
                .iter()
                .map(|n| grantee(n))
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    fn create_sql(&self) -> String {
        let mut out = format!(
            "CREATE POLICY {} ON {}{} FOR {} TO {}",
            quote_ident(self.name.trim()),
            quote_ident(&self.table),
            if self.permissive {
                ""
            } else {
                " AS RESTRICTIVE"
            },
            self.command,
            self.to_clause()
        );
        if !self.using.trim().is_empty() {
            out.push_str(&format!(" USING ({})", self.using.trim()));
        }
        if !self.check.trim().is_empty() {
            out.push_str(&format!(" WITH CHECK ({})", self.check.trim()));
        }
        out
    }

    fn drop_sql(&self, name: &str) -> String {
        format!(
            "DROP POLICY {} ON {}",
            quote_ident(name),
            quote_ident(&self.table)
        )
    }

    /// The table the policy is on, as the matrix and the RLS list key it.
    #[must_use]
    pub fn table_key(&self) -> (String, String) {
        (self.schema.clone(), self.table.clone())
    }
}

/// Role names in a comma-separated list, trimmed, `PUBLIC` dropped (it is the empty list).
#[must_use]
pub fn split_roles(roles: &str) -> Vec<String> {
    roles
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty() && !r.eq_ignore_ascii_case(PUBLIC))
        .map(str::to_owned)
        .collect()
}

/// A role name as a grantee: the keyword for `PUBLIC`, quoted otherwise (`nusadb-root`).
fn grantee(name: &str) -> String {
    if name.eq_ignore_ascii_case(PUBLIC) {
        "PUBLIC".to_owned()
    } else {
        quote_ident(name)
    }
}

/// Everything about access to one database, as read or as edited.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Security {
    pub roles: Vec<Role>,
    pub members: BTreeSet<Membership>,
    pub relations: Vec<Relation>,
    /// Cells that are not [`Grant::None`].
    pub grants: BTreeMap<GrantKey, Grant>,
    /// Tables with row-level security on.
    pub rls: BTreeSet<(String, String)>,
    pub policies: Vec<Policy>,
    /// Whether the role attributes could be read (`nusadb_roles` is superuser-only).
    pub roles_readable: bool,
    /// Whether RLS state and policies could be read (superuser-only catalogs).
    pub rls_readable: bool,
    /// What could not be read, in words.
    pub notes: Vec<String>,
}

impl Security {
    #[must_use]
    pub fn grant(&self, grantee: &str, relation: &Relation, privilege: usize) -> Grant {
        self.grants
            .get(&(
                grantee.to_owned(),
                relation.schema.clone(),
                relation.name.clone(),
                privilege,
            ))
            .copied()
            .unwrap_or_default()
    }

    pub fn set_grant(&mut self, grantee: &str, relation: &Relation, privilege: usize, to: Grant) {
        let key = (
            grantee.to_owned(),
            relation.schema.clone(),
            relation.name.clone(),
            privilege,
        );
        if to == Grant::None {
            self.grants.remove(&key);
        } else {
            self.grants.insert(key, to);
        }
    }

    /// Roles `member` belongs to.
    #[must_use]
    pub fn member_of(&self, member: &str) -> Vec<&Membership> {
        self.members.iter().filter(|m| m.member == member).collect()
    }

    /// Roles that belong to `role`.
    #[must_use]
    pub fn members_of(&self, role: &str) -> Vec<&Membership> {
        self.members.iter().filter(|m| m.role == role).collect()
    }

    /// What makes this state impossible to apply, if anything.
    ///
    /// # Errors
    ///
    /// The first problem, in words.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for r in &self.roles {
            let name = r.name.trim();
            if name.is_empty() {
                return Err("A new role has no name.".to_owned());
            }
            if !r.exists && !valid_ident(name) {
                return Err(format!(
                    "Role name “{name}” may use letters, digits and _ only."
                ));
            }
            if !seen.insert(name.to_lowercase()) {
                return Err(format!("Two roles are called {name}."));
            }
        }
        let mut names = BTreeSet::new();
        for p in &self.policies {
            let name = p.name.trim();
            if name.is_empty() {
                return Err(format!("A policy on {} has no name.", p.table));
            }
            if !valid_ident(name) {
                return Err(format!(
                    "Policy name “{name}” may use letters, digits and _ only."
                ));
            }
            if !names.insert((p.table_key(), name.to_lowercase())) {
                return Err(format!("Two policies on {} are called {name}.", p.table));
            }
            if p.original.is_none() && p.schema != "public" {
                return Err(format!(
                    "{}.{} is outside public: NusaDB's CREATE POLICY takes a bare table name, so \
                     only tables in public can carry a policy.",
                    p.schema, p.table
                ));
            }
            if !COMMANDS.contains(&p.command.as_str()) {
                return Err(format!("Policy {name} has an unknown command."));
            }
            if p.using.trim().is_empty() && p.check.trim().is_empty() {
                return Err(format!(
                    "Policy {name} needs a USING or a WITH CHECK expression."
                ));
            }
        }
        Ok(())
    }
}

/// The statements that turn `before` into `after`, in an order the engine accepts; empty when
/// nothing changed.
///
/// Order: policies that go or are rebuilt are dropped first, then memberships and privileges
/// are revoked and roles dropped (a dropped role takes its own grants with it), then roles are
/// created and altered, memberships and privileges granted, RLS switched, and policies created
/// or altered last, once the roles they name exist.
///
/// # Errors
///
/// What makes `after` invalid.
pub fn changes(before: &Security, after: &Security) -> Result<Vec<String>, String> {
    after.validate()?;
    let mut out = Vec::new();
    let dropped: BTreeSet<&str> = before
        .roles
        .iter()
        .filter(|r| !after.roles.iter().any(|a| a.exists && a.name == r.name))
        .map(|r| r.name.as_str())
        .collect();

    // Policies: dropped, rebuilt (command, kind or name changed) or altered in place.
    let mut create = Vec::new();
    let mut alter = Vec::new();
    for old in &before.policies {
        let now = after.policies.iter().find(|p| {
            p.original.as_deref() == Some(old.name.as_str()) && p.table_key() == old.table_key()
        });
        match now {
            None => out.push(old.drop_sql(&old.name)),
            Some(now) if now == old => {},
            Some(now)
                if now.command != old.command
                    || now.permissive != old.permissive
                    || now.name.trim() != old.name
                    // ALTER POLICY cannot take an expression away, only replace it.
                    || (now.using.trim().is_empty() && !old.using.trim().is_empty())
                    || (now.check.trim().is_empty() && !old.check.trim().is_empty()) =>
            {
                out.push(old.drop_sql(&old.name));
                create.push(now.create_sql());
            },
            Some(now) => {
                let mut parts = Vec::new();
                if split_roles(&now.roles) != split_roles(&old.roles) {
                    parts.push(format!("TO {}", now.to_clause()));
                }
                if now.using.trim() != old.using.trim() {
                    parts.push(format!("USING ({})", now.using.trim()));
                }
                if now.check.trim() != old.check.trim() {
                    parts.push(format!("WITH CHECK ({})", now.check.trim()));
                }
                if !parts.is_empty() {
                    alter.push(format!(
                        "ALTER POLICY {} ON {} {}",
                        quote_ident(&old.name),
                        quote_ident(&old.table),
                        parts.join(" ")
                    ));
                }
            },
        }
    }
    create.extend(
        after
            .policies
            .iter()
            .filter(|p| p.original.is_none())
            .map(Policy::create_sql),
    );

    // Memberships that go, or lose their admin option.
    for m in &before.members {
        if dropped.contains(m.role.as_str()) || dropped.contains(m.member.as_str()) {
            continue;
        }
        let now = after
            .members
            .iter()
            .find(|n| n.role == m.role && n.member == m.member);
        match now {
            None => out.push(format!(
                "REVOKE {} FROM {}",
                quote_ident(&m.role),
                quote_ident(&m.member)
            )),
            Some(n) if m.admin && !n.admin => out.push(format!(
                "REVOKE ADMIN OPTION FOR {} FROM {}",
                quote_ident(&m.role),
                quote_ident(&m.member)
            )),
            _ => {},
        }
    }

    // Privileges that go, or lose their grant option, grouped per grantee and table.
    let mut revoke: BTreeMap<(String, String, String), Vec<&str>> = BTreeMap::new();
    let mut revoke_option: BTreeMap<(String, String, String), Vec<&str>> = BTreeMap::new();
    for ((who, schema, table, p), was) in &before.grants {
        if dropped.contains(who.as_str()) {
            continue;
        }
        let now = after
            .grants
            .get(&(who.clone(), schema.clone(), table.clone(), *p))
            .copied()
            .unwrap_or_default();
        let key = (who.clone(), schema.clone(), table.clone());
        match (was, now) {
            (_, Grant::None) => revoke.entry(key).or_default().push(PRIVILEGES[*p]),
            (Grant::WithGrantOption, Grant::Granted) => {
                revoke_option.entry(key).or_default().push(PRIVILEGES[*p]);
            },
            _ => {},
        }
    }
    let on = |schema: &str, table: &str| format!("{}.{}", quote_ident(schema), quote_ident(table));
    for ((who, schema, table), privs) in &revoke_option {
        out.push(format!(
            "REVOKE GRANT OPTION FOR {} ON {} FROM {}",
            privs.join(", "),
            on(schema, table),
            grantee(who)
        ));
    }
    for ((who, schema, table), privs) in &revoke {
        out.push(format!(
            "REVOKE {} ON {} FROM {}",
            privs.join(", "),
            on(schema, table),
            grantee(who)
        ));
    }

    for name in &dropped {
        out.push(format!("DROP ROLE {}", quote_ident(name)));
    }

    for r in &after.roles {
        if !r.exists {
            let attrs = r.attributes(None);
            out.push(format!(
                "CREATE ROLE {}{}{}",
                quote_ident(r.name.trim()),
                if attrs.is_empty() { "" } else { " " },
                attrs.join(" ")
            ));
        } else if let Some(old) = before.roles.iter().find(|o| o.name == r.name) {
            let attrs = r.attributes(Some(old));
            if !attrs.is_empty() {
                out.push(format!(
                    "ALTER ROLE {} {}",
                    quote_ident(&r.name),
                    attrs.join(" ")
                ));
            }
        }
    }

    for m in &after.members {
        let old = before
            .members
            .iter()
            .find(|o| o.role == m.role && o.member == m.member);
        if old.is_none() || (m.admin && !old.is_some_and(|o| o.admin)) {
            out.push(format!(
                "GRANT {} TO {}{}",
                quote_ident(m.role.trim()),
                quote_ident(m.member.trim()),
                if m.admin { " WITH ADMIN OPTION" } else { "" }
            ));
        }
    }

    let mut grant: BTreeMap<(String, String, String, bool), Vec<&str>> = BTreeMap::new();
    for ((who, schema, table, p), now) in &after.grants {
        let was = before
            .grants
            .get(&(who.clone(), schema.clone(), table.clone(), *p))
            .copied()
            .unwrap_or_default();
        let fresh = dropped.contains(who.as_str());
        let option = *now == Grant::WithGrantOption;
        if fresh || was == Grant::None || (option && was != Grant::WithGrantOption) {
            grant
                .entry((who.clone(), schema.clone(), table.clone(), option))
                .or_default()
                .push(PRIVILEGES[*p]);
        }
    }
    for ((who, schema, table, option), privs) in &grant {
        out.push(format!(
            "GRANT {} ON {} TO {}{}",
            privs.join(", "),
            on(schema, table),
            grantee(who),
            if *option { " WITH GRANT OPTION" } else { "" }
        ));
    }

    for (schema, table) in after.rls.difference(&before.rls) {
        out.push(format!(
            "ALTER TABLE {} ENABLE ROW LEVEL SECURITY",
            on(schema, table)
        ));
    }
    for (schema, table) in before.rls.difference(&after.rls) {
        out.push(format!(
            "ALTER TABLE {} DISABLE ROW LEVEL SECURITY",
            on(schema, table)
        ));
    }

    out.extend(create);
    out.extend(alter);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orders() -> Relation {
        Relation {
            schema: "public".into(),
            name: "orders".into(),
            view: false,
        }
    }

    fn base() -> Security {
        let mut analyst = Role::new("analyst");
        analyst.exists = true;
        analyst.login = true;
        let mut readers = Role::new("readers");
        readers.exists = true;
        let mut s = Security {
            roles: vec![analyst, readers],
            relations: vec![orders()],
            roles_readable: true,
            rls_readable: true,
            ..Security::default()
        };
        s.members.insert(Membership {
            role: "readers".into(),
            member: "analyst".into(),
            admin: false,
        });
        s.set_grant("readers", &orders(), 0, Grant::Granted);
        s.policies.push(Policy {
            schema: "public".into(),
            table: "orders".into(),
            name: "own".into(),
            command: "SELECT".into(),
            roles: "analyst".into(),
            using: "amount > 0".into(),
            check: String::new(),
            permissive: true,
            original: Some("own".into()),
        });
        s.rls.insert(("public".into(), "orders".into()));
        s
    }

    #[test]
    fn nothing_changed_means_nothing_to_run() {
        assert!(changes(&base(), &base()).unwrap().is_empty());
    }

    #[test]
    fn a_new_role_gets_only_the_attributes_that_differ_from_the_defaults() {
        let mut after = base();
        let mut app = Role::new("app");
        app.login = true;
        app.inherit = false;
        after.roles.push(app);
        after.set_grant("app", &orders(), 0, Grant::Granted);
        after.set_grant("app", &orders(), 1, Grant::Granted);
        after.members.insert(Membership {
            role: "readers".into(),
            member: "app".into(),
            admin: true,
        });
        assert_eq!(
            changes(&base(), &after).unwrap(),
            vec![
                "CREATE ROLE \"app\" LOGIN NOINHERIT",
                "GRANT \"readers\" TO \"app\" WITH ADMIN OPTION",
                "GRANT SELECT, INSERT ON \"public\".\"orders\" TO \"app\"",
            ]
        );
    }

    #[test]
    fn attribute_and_grant_edits_become_alter_grant_and_revoke() {
        let mut after = base();
        after.roles[0].login = false;
        after.roles[0].createdb = true;
        after.set_grant("readers", &orders(), 0, Grant::WithGrantOption);
        after.set_grant("public", &orders(), 0, Grant::Granted);
        after.members.clear();
        assert_eq!(
            changes(&base(), &after).unwrap(),
            vec![
                "REVOKE \"readers\" FROM \"analyst\"",
                "ALTER ROLE \"analyst\" NOLOGIN CREATEDB",
                "GRANT SELECT ON \"public\".\"orders\" TO PUBLIC",
                "GRANT SELECT ON \"public\".\"orders\" TO \"readers\" WITH GRANT OPTION",
            ]
        );
        let mut back = after.clone();
        back.set_grant("readers", &orders(), 0, Grant::Granted);
        back.set_grant("public", &orders(), 0, Grant::None);
        assert_eq!(
            changes(&after, &back).unwrap(),
            vec![
                "REVOKE GRANT OPTION FOR SELECT ON \"public\".\"orders\" FROM \"readers\"",
                "REVOKE SELECT ON \"public\".\"orders\" FROM PUBLIC",
            ]
        );
    }

    #[test]
    fn a_dropped_role_takes_its_grants_and_memberships_with_it() {
        let mut after = base();
        after.roles.retain(|r| r.name != "readers");
        after.grants.clear();
        after.members.clear();
        assert_eq!(
            changes(&base(), &after).unwrap(),
            vec!["DROP ROLE \"readers\""]
        );
    }

    #[test]
    fn a_policy_is_altered_in_place_when_it_can_be_and_rebuilt_when_not() {
        let mut after = base();
        after.policies[0].roles = "analyst, readers".into();
        after.policies[0].using = "amount > 10".into();
        assert_eq!(
            changes(&base(), &after).unwrap(),
            vec![
                "ALTER POLICY \"own\" ON \"orders\" TO \"analyst\", \"readers\" USING (amount > 10)"
            ]
        );
        after.policies[0].command = "ALL".into();
        after.policies[0].permissive = false;
        after.policies[0].check = "amount < 100".into();
        assert_eq!(
            changes(&base(), &after).unwrap(),
            vec![
                "DROP POLICY \"own\" ON \"orders\"",
                "CREATE POLICY \"own\" ON \"orders\" AS RESTRICTIVE FOR ALL TO \"analyst\", \"readers\" USING (amount > 10) WITH CHECK (amount < 100)",
            ]
        );
    }

    #[test]
    fn rls_switches_and_new_policies_come_after_the_roles_they_name() {
        let mut after = base();
        after.rls.clear();
        after.policies.push(Policy {
            schema: "public".into(),
            table: "orders".into(),
            name: "inserts".into(),
            command: "INSERT".into(),
            roles: String::new(),
            using: String::new(),
            check: "amount >= 0".into(),
            permissive: true,
            original: None,
        });
        assert_eq!(
            changes(&base(), &after).unwrap(),
            vec![
                "ALTER TABLE \"public\".\"orders\" DISABLE ROW LEVEL SECURITY",
                "CREATE POLICY \"inserts\" ON \"orders\" FOR INSERT TO PUBLIC WITH CHECK (amount >= 0)",
            ]
        );
    }

    #[test]
    fn what_nusadb_would_refuse_is_caught_before_it_runs() {
        let mut after = base();
        after.roles.push(Role::new("bad name"));
        assert!(
            changes(&base(), &after)
                .unwrap_err()
                .contains("letters, digits")
        );

        let mut after = base();
        let mut p = after.policies[0].clone();
        p.original = None;
        p.name = "other".into();
        p.schema = "sales".into();
        after.policies.push(p);
        assert!(
            changes(&base(), &after)
                .unwrap_err()
                .contains("outside public")
        );

        // NusaDB refuses a policy with neither clause (0A000); every other mix is accepted.
        let mut after = base();
        after.policies[0].using = String::new();
        assert!(
            changes(&base(), &after)
                .unwrap_err()
                .contains("USING or a WITH CHECK")
        );
    }

    #[test]
    fn a_grant_cell_cycles_through_its_three_states() {
        assert_eq!(Grant::None.cycle(), Grant::Granted);
        assert_eq!(Grant::Granted.cycle(), Grant::WithGrantOption);
        assert_eq!(Grant::WithGrantOption.cycle(), Grant::None);
    }
}
