//! The Security screen: roles, memberships, table privileges and row-level security of the
//! database the navigator points at, edited as one set of changes and applied in a transaction.

use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use super::{AppWindow, Controller, Pending, explain, first_line, on_ui, sync_model};
use crate::backend::{RunReport, StatementOutcome, Target};
use crate::model::Environment;
use crate::risk;
use crate::security::{self, Grant, Membership, PRIVILEGES, PUBLIC, Policy, Role, Security};
use crate::ui::{SecGrantRow, SecMember, SecPolicy, SecRole, SecTable};

/// The database open on the Security screen: as read, and as edited.
#[derive(Debug, Clone)]
pub(super) struct SecurityState {
    server: u32,
    database: String,
    loaded: Security,
    edit: Security,
    /// Selected role by name; [`PUBLIC`] for the pseudo-role.
    role: Option<String>,
    /// Selected table for row-level security.
    table: Option<(String, String)>,
    /// What each row of the role list stands for, as last published.
    role_rows: Vec<String>,
    /// What each row of the table list stands for.
    table_rows: Vec<(String, String)>,
    /// Index into `edit.policies` of each policy row shown.
    policy_rows: Vec<usize>,
}

/// The models the screen's lists are drawn from, kept so typing into a policy keeps its focus.
#[derive(Default)]
pub(super) struct SecurityModels {
    grants: Rc<VecModel<SecGrantRow>>,
    policies: Rc<VecModel<SecPolicy>>,
}

impl SecurityModels {
    pub(super) fn bind(&self, window: &AppWindow) {
        window.set_sec_grants(ModelRc::from(
            Rc::clone(&self.grants) as Rc<dyn Model<Data = SecGrantRow>>
        ));
        window.set_sec_policies(ModelRc::from(
            Rc::clone(&self.policies) as Rc<dyn Model<Data = SecPolicy>>
        ));
        let privileges: Vec<SharedString> = PRIVILEGES.iter().map(|p| (*p).into()).collect();
        window.set_sec_privileges(ModelRc::new(VecModel::from(privileges)));
    }
}

fn label(schema: &str, name: &str) -> String {
    if schema == "public" {
        name.to_owned()
    } else {
        format!("{schema}.{name}")
    }
}

fn role_detail(role: &Role) -> String {
    let mut parts = Vec::new();
    if role.superuser {
        parts.push("superuser");
    }
    parts.push(if role.login {
        "can log in"
    } else {
        "group, no login"
    });
    if role.createdb {
        parts.push("createdb");
    }
    if role.createrole {
        parts.push("createrole");
    }
    if !role.inherit {
        parts.push("noinherit");
    }
    parts.join(" · ")
}

fn flags(role: &Role) -> [bool; 5] {
    [
        role.login,
        role.superuser,
        role.createdb,
        role.createrole,
        role.inherit,
    ]
}

impl Controller {
    /// The rail opened the Security screen, or the navigator moved while it is open.
    pub(super) fn security_follow(&self, window: &AppWindow) {
        if window.get_screen() != "security" || window.get_sec_applying() {
            return;
        }
        let Some((server, database)) = self.target.borrow().clone() else {
            if self.security.borrow().is_none() {
                window.set_sec_state(SharedString::new());
            }
            return;
        };
        let (same, dirty) = self.security.borrow().as_ref().map_or((false, false), |s| {
            (
                s.server == server && s.database == database,
                s.loaded != s.edit,
            )
        });
        let loading = self
            .security_loading
            .borrow()
            .as_ref()
            .is_some_and(|(s, d)| *s == server && *d == database);
        // Unapplied changes stay on screen (the header names their database) until applied
        // or reverted, rather than vanishing because the navigator moved.
        if same || dirty || loading {
            return;
        }
        self.security_load(window, server, &database);
    }

    pub(super) fn security_load(&self, window: &AppWindow, server: u32, database: &str) {
        let Some(target) = self.target_for(server, database) else {
            return;
        };
        let name = self
            .store
            .borrow()
            .get(server)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        window.set_sec_target(format!("{name} / {database}").into());
        window.set_sec_state("loading".into());
        self.security_loading
            .replace(Some((server, database.to_owned())));
        let weak = window.as_weak();
        let database = database.to_owned();
        self.backend.security(
            target,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| {
                    c.security_loaded(w, server, &database, result)
                });
            }),
        );
    }

    fn security_loaded(
        &self,
        window: &AppWindow,
        server: u32,
        database: &str,
        result: Result<Security, String>,
    ) {
        self.security_loading.replace(None);
        let loaded = match result {
            Ok(s) => s,
            Err(message) => {
                window.set_sec_error(message.into());
                window.set_sec_state("error".into());
                return;
            },
        };
        let (role, table) = self
            .security
            .borrow()
            .as_ref()
            .filter(|s| s.server == server && s.database == database)
            .map(|s| (s.role.clone(), s.table.clone()))
            .unwrap_or_default();
        let role = role
            .filter(|r| r == PUBLIC || loaded.roles.iter().any(|x| x.name == *r))
            .or_else(|| loaded.roles.first().map(|r| r.name.clone()))
            .or_else(|| Some(PUBLIC.to_owned()));
        let table = table.filter(|t| {
            loaded
                .relations
                .iter()
                .any(|r| (&r.schema, &r.name) == (&t.0, &t.1))
        });
        self.security.replace(Some(SecurityState {
            server,
            database: database.to_owned(),
            edit: loaded.clone(),
            loaded,
            role,
            table,
            role_rows: Vec::new(),
            table_rows: Vec::new(),
            policy_rows: Vec::new(),
        }));
        window.set_sec_state("ready".into());
        self.publish_security(window, true);
    }

    /// Push the screen's state into the window. `rebuild` replaces the policy rows; otherwise
    /// they are updated in place so the field being typed into keeps its caret.
    pub(super) fn publish_security(&self, window: &AppWindow, rebuild: bool) {
        let mut guard = self.security.borrow_mut();
        let Some(state) = guard.as_mut() else {
            return;
        };
        let (loaded, edit) = (&state.loaded, &state.edit);
        window.set_sec_roles_editable(edit.roles_readable);
        window.set_sec_rls_editable(edit.rls_readable);
        window.set_sec_notes(edit.notes.join(" ").into());

        // Role list: PUBLIC, every role as loaded (dropped ones marked), then new ones.
        let mut rows = vec![SecRole {
            name: PUBLIC.into(),
            detail: "every role".into(),
            state: SharedString::new(),
            pseudo: true,
        }];
        state.role_rows = vec![PUBLIC.to_owned()];
        // Whether anything held by `name` (grants, memberships either way) was edited.
        let touched = |name: &str| {
            let grants = |s: &Security| {
                s.grants
                    .iter()
                    .filter(|(k, _)| k.0 == name)
                    .map(|(k, v)| (k.clone(), *v))
                    .collect::<Vec<_>>()
            };
            let members = |s: &Security| {
                s.members
                    .iter()
                    .filter(|m| m.role == name || m.member == name)
                    .cloned()
                    .collect::<Vec<_>>()
            };
            grants(loaded) != grants(edit) || members(loaded) != members(edit)
        };
        if touched(PUBLIC) {
            rows[0].state = "changed".into();
        }
        // Without nusadb_roles the attributes are unknown, not "off": say so.
        let detail = |r: &Role| {
            if edit.roles_readable {
                role_detail(r)
            } else {
                "attributes: superuser only".to_owned()
            }
        };
        for r in &loaded.roles {
            let now = edit.roles.iter().find(|e| e.exists && e.name == r.name);
            rows.push(SecRole {
                name: r.name.as_str().into(),
                detail: detail(now.unwrap_or(r)).into(),
                state: match now {
                    None => "dropped",
                    Some(n) if n != r || touched(&r.name) => "changed",
                    Some(_) => "",
                }
                .into(),
                pseudo: false,
            });
            state.role_rows.push(r.name.clone());
        }
        for r in edit.roles.iter().filter(|r| !r.exists) {
            rows.push(SecRole {
                name: r.name.as_str().into(),
                detail: role_detail(r).into(),
                state: "new".into(),
                pseudo: false,
            });
            state.role_rows.push(r.name.clone());
        }
        window.set_sec_roles(ModelRc::new(VecModel::from(rows)));

        let selected = state.role.clone();
        let index = selected
            .as_ref()
            .and_then(|s| state.role_rows.iter().position(|r| r == s));
        window.set_sec_role_selected(index.map_or(-1, |i| i32::try_from(i).unwrap_or(-1)));
        let sel = selected.unwrap_or_default();
        let pseudo = sel == PUBLIC;
        let current = edit.roles.iter().find(|r| r.name == sel);
        let before = loaded.roles.iter().find(|r| r.name == sel);
        window.set_sec_role_name(sel.as_str().into());
        window.set_sec_role_pseudo(pseudo);
        window.set_sec_role_new(current.is_some_and(|r| !r.exists));
        window.set_sec_role_dropped(!pseudo && current.is_none() && before.is_some());
        if pseudo && window.get_sec_role_tab() != "privileges" {
            window.set_sec_role_tab("privileges".into());
        }
        let now_flags = current.or(before).map(flags).unwrap_or_default();
        let was_flags = before.map(flags);
        let changed: Vec<bool> = (0..5)
            .map(|i| current.is_some() && was_flags.is_some_and(|w| w[i] != now_flags[i]))
            .collect();
        window.set_sec_flags(ModelRc::new(VecModel::from(now_flags.to_vec())));
        window.set_sec_flags_changed(ModelRc::new(VecModel::from(changed)));

        let member_of: Vec<SecMember> = edit
            .member_of(&sel)
            .into_iter()
            .map(|m| {
                let was = loaded
                    .members
                    .iter()
                    .find(|o| o.role == m.role && o.member == m.member);
                SecMember {
                    role: m.role.as_str().into(),
                    admin: m.admin,
                    state: match was {
                        None => "new",
                        Some(w) if w.admin != m.admin => "changed",
                        Some(_) => "",
                    }
                    .into(),
                }
            })
            .collect();
        window.set_sec_member_of(ModelRc::new(VecModel::from(member_of)));
        let members: Vec<SharedString> = edit
            .members_of(&sel)
            .iter()
            .map(|m| m.member.as_str().into())
            .collect();
        let can_join: Vec<SharedString> = edit
            .roles
            .iter()
            .filter(|r| {
                r.name != sel
                    && !edit
                        .members
                        .iter()
                        .any(|m| m.member == sel && m.role == r.name)
                    && !edit
                        .members
                        .iter()
                        .any(|m| m.role == sel && m.member == r.name)
            })
            .map(|r| r.name.as_str().into())
            .collect();
        window.set_sec_members(ModelRc::new(VecModel::from(members)));
        window.set_sec_can_join(ModelRc::new(VecModel::from(can_join)));

        let grants: Vec<SecGrantRow> = edit
            .relations
            .iter()
            .map(|rel| {
                let cells: Vec<i32> = (0..PRIVILEGES.len())
                    .map(|p| match edit.grant(&sel, rel, p) {
                        Grant::None => 0,
                        Grant::Granted => 1,
                        Grant::WithGrantOption => 2,
                    })
                    .collect();
                let changed: Vec<bool> = (0..PRIVILEGES.len())
                    .map(|p| edit.grant(&sel, rel, p) != loaded.grant(&sel, rel, p))
                    .collect();
                SecGrantRow {
                    relation: label(&rel.schema, &rel.name).into(),
                    view: rel.view,
                    cells: ModelRc::new(VecModel::from(cells)),
                    changed: ModelRc::new(VecModel::from(changed)),
                }
            })
            .collect();
        self.security_models.grants.set_vec(grants);

        // Row-level security: tables only (policies and RLS apply to base tables).
        state.table_rows = edit
            .relations
            .iter()
            .filter(|r| !r.view)
            .map(|r| (r.schema.clone(), r.name.clone()))
            .collect();
        let tables: Vec<SecTable> = state
            .table_rows
            .iter()
            .map(|key| SecTable {
                label: label(&key.0, &key.1).into(),
                rls: edit.rls.contains(key),
                changed: edit.rls.contains(key) != loaded.rls.contains(key),
                policies: i32::try_from(
                    edit.policies
                        .iter()
                        .filter(|p| p.table_key() == *key)
                        .count(),
                )
                .unwrap_or(0),
            })
            .collect();
        window.set_sec_tables(ModelRc::new(VecModel::from(tables)));
        let table_index = state
            .table
            .as_ref()
            .and_then(|t| state.table_rows.iter().position(|r| r == t));
        window.set_sec_table_selected(table_index.map_or(-1, |i| i32::try_from(i).unwrap_or(-1)));
        let (policy_allowed, blocked, table_label) = match &state.table {
            Some((schema, name)) if !edit.rls_readable => (
                false,
                "Policies are kept in superuser-only catalogs: connect as a superuser to change them."
                    .to_owned(),
                label(schema, name),
            ),
            Some((schema, name)) if schema != "public" => (
                false,
                format!(
                    "{schema}.{name} is outside public. NusaDB's CREATE POLICY takes a bare table \
                     name and resolves it in public only, so a policy cannot be added here yet."
                ),
                label(schema, name),
            ),
            Some((schema, name)) => (true, String::new(), label(schema, name)),
            None => (false, String::new(), String::new()),
        };
        window.set_sec_policy_allowed(policy_allowed);
        window.set_sec_policy_blocked(blocked.into());
        window.set_sec_table_label(table_label.into());
        state.policy_rows = edit
            .policies
            .iter()
            .enumerate()
            .filter(|(_, p)| state.table.as_ref() == Some(&p.table_key()))
            .map(|(i, _)| i)
            .collect();
        let policies: Vec<SecPolicy> = state
            .policy_rows
            .iter()
            .map(|&i| {
                let p = &edit.policies[i];
                let was = p.original.as_ref().and_then(|o| {
                    loaded
                        .policies
                        .iter()
                        .find(|l| l.name == *o && l.table_key() == p.table_key())
                });
                SecPolicy {
                    name: p.name.as_str().into(),
                    command: p.command.as_str().into(),
                    roles: p.roles.as_str().into(),
                    using: p.using.as_str().into(),
                    check: p.check.as_str().into(),
                    permissive: p.permissive,
                    state: match was {
                        None => "new",
                        Some(w) if w != p => "changed",
                        Some(_) => "",
                    }
                    .into(),
                }
            })
            .collect();
        if rebuild {
            self.security_models.policies.set_vec(policies);
        } else {
            sync_model(&self.security_models.policies, policies);
        }

        match security::changes(loaded, edit) {
            Ok(statements) => {
                window.set_sec_problem(SharedString::new());
                window.set_sec_has_changes(!statements.is_empty());
                let text: Vec<String> = statements.iter().map(|s| format!("{s};")).collect();
                window.set_sec_ddl(text.join("\n").into());
            },
            Err(problem) => {
                window.set_sec_problem(problem.into());
                window.set_sec_has_changes(true);
                window.set_sec_ddl(SharedString::new());
            },
        }
    }

    /// Apply `change` to the edited state and publish.
    pub(super) fn security_change(
        &self,
        window: &AppWindow,
        rebuild: bool,
        change: impl FnOnce(&mut SecurityState),
    ) {
        if let Some(state) = self.security.borrow_mut().as_mut() {
            change(state);
        }
        self.publish_security(window, rebuild);
    }

    pub(super) fn security_select_role(&self, window: &AppWindow, row: usize) {
        self.security_change(window, true, |s| {
            if let Some(name) = s.role_rows.get(row) {
                s.role = Some(name.clone());
            }
        });
    }

    pub(super) fn security_new_role(&self, window: &AppWindow, name: &str) {
        // NusaDB folds an unquoted name to lower case; the screen does the same.
        let name = name.trim().to_lowercase();
        if name.is_empty() {
            return;
        }
        self.security_change(window, true, |s| {
            if !s.edit.roles.iter().any(|r| r.name == name) {
                match s.loaded.roles.iter().find(|r| r.name == name) {
                    // A role dropped a moment ago comes back as it was.
                    Some(old) => s.edit.roles.push(old.clone()),
                    None => s.edit.roles.push(Role::new(&name)),
                }
            }
            s.role = Some(name);
        });
        window.set_sec_role_tab("attributes".into());
    }

    /// Drop the selected role, or keep it if it is already marked for dropping.
    pub(super) fn security_toggle_drop(&self, window: &AppWindow) {
        self.security_change(window, true, |s| {
            let Some(name) = s.role.clone() else {
                return;
            };
            if name == PUBLIC {
                return;
            }
            if let Some(i) = s.edit.roles.iter().position(|r| r.name == name) {
                // A dropped role takes its grants and memberships with it.
                s.edit.roles.remove(i);
                s.edit.grants.retain(|k, _| k.0 != name);
                s.edit
                    .members
                    .retain(|m| m.role != name && m.member != name);
                if !s.loaded.roles.iter().any(|r| r.name == name) {
                    s.role = Some(PUBLIC.to_owned());
                }
            } else if let Some(old) = s.loaded.roles.iter().find(|r| r.name == name) {
                s.edit.roles.push(old.clone());
                for (k, v) in &s.loaded.grants {
                    if k.0 == name {
                        s.edit.grants.insert(k.clone(), *v);
                    }
                }
                for m in &s.loaded.members {
                    if m.role == name || m.member == name {
                        s.edit.members.insert(m.clone());
                    }
                }
            }
        });
    }

    pub(super) fn security_flag(&self, window: &AppWindow, flag: usize, on: bool) {
        self.security_change(window, false, |s| {
            let Some(name) = s.role.clone() else {
                return;
            };
            if let Some(r) = s.edit.roles.iter_mut().find(|r| r.name == name) {
                match flag {
                    0 => r.login = on,
                    1 => r.superuser = on,
                    2 => r.createdb = on,
                    3 => r.createrole = on,
                    _ => r.inherit = on,
                }
            }
        });
    }

    pub(super) fn security_membership(&self, window: &AppWindow, role: &str, how: Option<bool>) {
        self.security_change(window, true, |s| {
            let Some(member) = s.role.clone() else {
                return;
            };
            s.edit
                .members
                .retain(|m| !(m.role == role && m.member == member));
            if let Some(admin) = how {
                s.edit.members.insert(Membership {
                    role: role.to_owned(),
                    member,
                    admin,
                });
            }
        });
    }

    /// Change grant cells of the selected role: one cell, a whole row or a whole column.
    pub(super) fn security_grant(
        &self,
        window: &AppWindow,
        row: Option<usize>,
        col: Option<usize>,
    ) {
        self.security_change(window, false, |s| {
            let Some(who) = s.role.clone() else {
                return;
            };
            if !s.edit.roles.iter().any(|r| r.name == who) && who != PUBLIC {
                return;
            }
            let rows: Vec<usize> =
                row.map_or_else(|| (0..s.edit.relations.len()).collect(), |r| vec![r]);
            let cols: Vec<usize> = col.map_or_else(|| (0..PRIVILEGES.len()).collect(), |c| vec![c]);
            let cells: Vec<(usize, usize)> = rows
                .iter()
                .flat_map(|&r| cols.iter().map(move |&c| (r, c)))
                .filter(|&(r, _)| r < s.edit.relations.len())
                .collect();
            if let ([(r, c)], Some(_), Some(_)) = (cells.as_slice(), row, col) {
                let rel = s.edit.relations[*r].clone();
                let next = s.edit.grant(&who, &rel, *c).cycle();
                s.edit.set_grant(&who, &rel, *c, next);
                return;
            }
            // A row or column: grant what is missing, or clear it all if nothing is missing.
            let full = cells
                .iter()
                .all(|&(r, c)| s.edit.grant(&who, &s.edit.relations[r], c) != Grant::None);
            for (r, c) in cells {
                let rel = s.edit.relations[r].clone();
                if full {
                    s.edit.set_grant(&who, &rel, c, Grant::None);
                } else if s.edit.grant(&who, &rel, c) == Grant::None {
                    s.edit.set_grant(&who, &rel, c, Grant::Granted);
                }
            }
        });
    }

    pub(super) fn security_select_table(&self, window: &AppWindow, row: usize) {
        self.security_change(window, true, |s| {
            if let Some(t) = s.table_rows.get(row) {
                s.table = Some(t.clone());
            }
        });
    }

    pub(super) fn security_rls(&self, window: &AppWindow, row: usize, on: bool) {
        self.security_change(window, false, |s| {
            if let Some(t) = s.table_rows.get(row).cloned() {
                s.table = Some(t.clone());
                if on {
                    s.edit.rls.insert(t);
                } else {
                    s.edit.rls.remove(&t);
                }
            }
        });
    }

    pub(super) fn security_add_policy(&self, window: &AppWindow) {
        self.security_change(window, true, |s| {
            let Some((schema, table)) = s.table.clone() else {
                return;
            };
            let n = s
                .edit
                .policies
                .iter()
                .filter(|p| p.schema == schema && p.table == table)
                .count()
                + 1;
            s.edit.policies.push(Policy {
                schema,
                name: format!("{table}_policy_{n}"),
                table,
                command: "ALL".to_owned(),
                roles: String::new(),
                using: String::new(),
                check: String::new(),
                permissive: true,
                original: None,
            });
        });
    }

    pub(super) fn security_remove_policy(&self, window: &AppWindow, row: usize) {
        self.security_change(window, true, |s| {
            if let Some(&i) = s.policy_rows.get(row) {
                s.edit.policies.remove(i);
            }
        });
    }

    pub(super) fn security_policy_field(
        &self,
        window: &AppWindow,
        row: usize,
        field: &str,
        value: &str,
    ) {
        self.security_change(window, false, |s| {
            let Some(&i) = s.policy_rows.get(row) else {
                return;
            };
            let p = &mut s.edit.policies[i];
            match field {
                "name" => p.name = value.trim().to_lowercase(),
                "command" => p.command = value.to_owned(),
                "kind" => p.permissive = value == "permissive",
                "roles" => p.roles = value.to_owned(),
                "using" => p.using = value.to_owned(),
                "check" => p.check = value.to_owned(),
                _ => {},
            }
        });
    }

    /// Tables were created, renamed or dropped: the Security and Diagram screens read the
    /// catalog once, so they reload (Security only when it holds no unapplied edits).
    pub(super) fn catalog_changed(&self, window: &AppWindow) {
        let clean = self
            .security
            .borrow()
            .as_ref()
            .is_some_and(|s| s.loaded == s.edit);
        if clean {
            self.security.replace(None);
            self.security_follow(window);
        }
        if self.erd.borrow().is_some() {
            self.erd.replace(None);
            self.erd_follow(window);
        }
    }

    pub(super) fn security_revert(&self, window: &AppWindow) {
        self.security_change(window, true, |s| s.edit = s.loaded.clone());
    }

    pub(super) fn security_reload(&self, window: &AppWindow) {
        let key = self
            .security
            .borrow()
            .as_ref()
            .map(|s| (s.server, s.database.clone()))
            .or_else(|| self.target.borrow().clone());
        if let Some((server, database)) = key {
            // A reload is a fresh read: forget the edits first.
            if let Some(s) = self.security.borrow_mut().as_mut() {
                s.edit = s.loaded.clone();
            }
            self.security_load(window, server, &database);
        }
    }

    fn security_statements(&self) -> Option<(Vec<String>, bool)> {
        let guard = self.security.borrow();
        let state = guard.as_ref()?;
        let statements = security::changes(&state.loaded, &state.edit).ok()?;
        let production = self
            .store
            .borrow()
            .get(state.server)
            .is_some_and(|p| p.environment == Environment::Prod);
        Some((statements, production))
    }

    pub(super) fn security_apply(&self, window: &AppWindow) {
        let Some((statements, production)) = self.security_statements() else {
            return;
        };
        if statements.is_empty() {
            return;
        }
        if let Some(found) = risk::assess(&statements, production) {
            self.ask(
                window,
                Pending::ApplySecurity,
                if production {
                    "Change access on production?"
                } else {
                    "This change removes roles or access"
                },
                &found.reasons,
                "Apply anyway",
            );
            return;
        }
        self.security_apply_confirmed(window);
    }

    pub(super) fn security_apply_confirmed(&self, window: &AppWindow) {
        let Some((mut script, _)) = self.security_statements() else {
            return;
        };
        let Some(target) = self
            .security
            .borrow()
            .as_ref()
            .and_then(|s| self.target_for(s.server, &s.database))
        else {
            return;
        };
        // Role, grant and policy statements are transactional: all of them, or none.
        script.insert(0, "BEGIN".to_owned());
        script.push("COMMIT".to_owned());
        window.set_sec_applying(true);
        let weak = window.as_weak();
        let rollback = target.clone();
        self.backend.run(
            target,
            script,
            Box::new(move |result| {
                on_ui(&weak, move |c, w| c.security_applied(w, result, rollback));
            }),
        );
    }

    fn security_applied(
        &self,
        window: &AppWindow,
        result: Result<RunReport, String>,
        target: Target,
    ) {
        window.set_sec_applying(false);
        let failure = match &result {
            Err(message) => Some(message.clone()),
            Ok(report) => report.failure().map(|f| match &f.outcome {
                StatementOutcome::Failed { code, message } => format!(
                    "{} (in: {})",
                    explain(code.as_deref(), message),
                    first_line(&f.sql)
                ),
                StatementOutcome::Done { .. } => String::new(),
            }),
        };
        if let Some(message) = failure {
            self.backend
                .run(target, vec!["ROLLBACK".to_owned()], Box::new(|_| {}));
            window.set_sec_problem(format!("Nothing was changed: {message}").into());
            self.toast(
                window,
                "Access was not changed. See the message above the SQL.",
                true,
            );
            return;
        }
        self.toast(window, "Roles, grants and policies updated", false);
        // Descriptions cached by the inspector may list policies that changed.
        self.info.borrow_mut().clear();
        let key = self
            .security
            .borrow()
            .as_ref()
            .map(|s| (s.server, s.database.clone()));
        if let Some((server, database)) = key {
            if let Some(s) = self.security.borrow_mut().as_mut() {
                s.loaded = s.edit.clone();
            }
            self.security_load(window, server, &database);
        }
    }

    /// Open the Security screen on `(server, database)`, optionally on one table's policies.
    pub(super) fn security_open_at(
        &self,
        window: &AppWindow,
        server: u32,
        database: &str,
        table: Option<(String, String)>,
    ) {
        self.set_target(server, database);
        window.set_screen("security".into());
        if let Some(t) = table {
            window.set_sec_tab("rls".into());
            let same = self
                .security
                .borrow()
                .as_ref()
                .is_some_and(|s| s.server == server && s.database == database);
            if same {
                self.security_change(window, true, |s| s.table = Some(t));
                return;
            }
            self.security.replace(Some(SecurityState {
                server,
                database: database.to_owned(),
                loaded: Security::default(),
                edit: Security::default(),
                role: None,
                table: Some(t),
                role_rows: Vec::new(),
                table_rows: Vec::new(),
                policy_rows: Vec::new(),
            }));
            self.security_load(window, server, database);
            return;
        }
        self.security_follow(window);
    }
}
