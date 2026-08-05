//! Adapter between the Slint window and the pure Rust core.
//!
//! This is the only module that imports both `crate::ui` (the generated bindings) and the
//! domain modules, which is what keeps `model` / `store` / `probe` / `tree` free of the UI
//! toolkit.
//!
//! The flow is one-directional: a view raises an intent as a callback, the controller applies
//! it to the state it owns, and then re-publishes everything the window shows. The window
//! therefore never holds state the controller does not also hold, so there is no second copy
//! to fall out of sync.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::model::{ConnectionParams, ConnectionStatus, Environment, ServerDraft, ServerProfile};
use crate::probe::{ConnectionProbe, ProbeOutcome};
use crate::sql_highlight::highlight_line;
use crate::store::ServerStore;
use crate::tree::{TreeState, server_node_id};
use crate::ui::{AppWindow, GridColumn, Property, ServerRow, SqlLine, SqlToken, TreeRow};

/// Sentinel the UI uses for "nothing is selected".
const NO_SELECTION: i32 = -1;

/// What the editor holds on a cold start.
///
/// A scratch buffer, not a demo: it says what the tool is and what it cannot do yet, instead
/// of showing a query against tables no one has created.
const SCRATCH_SQL: &str = "\
-- NusaDB Studio scratch buffer.
-- Connect a server in the Connections screen, then press Run.

SELECT 1;";

/// Owns Studio's state and translates UI intents into changes to it.
pub struct Controller {
    store: RefCell<ServerStore>,
    tree: RefCell<TreeState>,
    probe: Box<dyn ConnectionProbe>,
    /// The server currently loaded in the connection form and described by the inspector.
    /// Selecting a server in the navigator and pressing Edit on its card are the same act, so
    /// one field covers both.
    selected: Cell<Option<u32>>,
}

impl Controller {
    /// Build a controller over `store`, using `probe` to answer connection tests.
    #[must_use]
    pub fn new(store: ServerStore, probe: Box<dyn ConnectionProbe>) -> Self {
        Self {
            store: RefCell::new(store),
            tree: RefCell::new(TreeState::new()),
            probe,
            selected: Cell::new(None),
        }
    }

    /// Push every derived view of the state into the window.
    ///
    /// Republishing wholesale rather than patching individual rows keeps the update path
    /// single and obvious; the lists are small enough that the cost is irrelevant.
    fn publish(&self, window: &AppWindow) {
        let store = self.store.borrow();

        let rows: Vec<ServerRow> = store.visible().map(server_row).collect();
        window.set_servers(ModelRc::new(VecModel::from(rows)));

        let tree_rows: Vec<TreeRow> = self
            .tree
            .borrow()
            .rows(store.visible())
            .into_iter()
            .map(|row| TreeRow {
                id: row.id.as_str().into(),
                label: row.label.as_str().into(),
                kind: row.kind.into(),
                depth: row.depth,
                expandable: row.expandable,
                expanded: row.expanded,
                status: row.status.into(),
                badge: row.badge.as_str().into(),
                dimmed: row.dimmed,
            })
            .collect();
        window.set_tree_rows(ModelRc::new(VecModel::from(tree_rows)));

        let selected = self.selected.get();
        window.set_editing(selected.is_some());
        window.set_selected_id(selected.map_or(NO_SELECTION, id_for_ui));
        window.set_selected_node(
            selected.map_or_else(SharedString::new, |id| server_node_id(id).as_str().into()),
        );

        match selected.and_then(|id| store.get(id)) {
            Some(profile) => self.publish_selection(window, profile),
            None => publish_no_selection(window),
        }
    }

    /// Fill the inspector, the breadcrumb, and the status bar from the selected server.
    fn publish_selection(&self, window: &AppWindow, profile: &ServerProfile) {
        window.set_inspector_title(profile.name.as_str().into());
        window.set_inspector_kind("server".into());
        window.set_inspector_properties(ModelRc::new(VecModel::from(vec![
            property("Endpoint", &profile.params.endpoint(), true),
            property("User", &profile.params.user, true),
            property("Database", &profile.params.database, true),
            property("Environment", profile.environment.as_tag(), false),
            property(
                "TLS",
                if profile.params.tls {
                    "enabled"
                } else {
                    "disabled"
                },
                false,
            ),
            property("Status", profile.status.as_tag(), false),
        ])));

        window.set_server_crumb(profile.name.as_str().into());
        window.set_database_crumb(profile.params.database.as_str().into());

        let connected = profile.status == ConnectionStatus::Connected;
        window.set_connected(connected);
        window.set_connection_status(profile.status.as_tag().into());
        window
            .set_connection_label(format!("{} · {}", profile.name, profile.params.database).into());

        window
            .set_status_server(format!("{} · {}", profile.name, profile.params.endpoint()).into());
        window.set_status_database(profile.params.database.as_str().into());
    }

    /// Load the server `id` into the form and the inspector.
    ///
    /// Unknown identities are ignored: a stale callback from a row that has since been removed
    /// should do nothing, not clear the form.
    fn select(&self, window: &AppWindow, id: u32) {
        let draft = self.store.borrow().get(id).map(ServerDraft::from_profile);
        let Some(draft) = draft else {
            return;
        };

        self.selected.set(Some(id));
        write_draft(window, &draft);
        clear_feedback(window);
        self.publish(window);
    }

    /// Handle a click on a tree row.
    ///
    /// Only server rows carry a selection today; a hint row is a message, not an object.
    fn select_node(&self, window: &AppWindow, node_id: &str) {
        let Some(id) = node_id
            .strip_prefix("srv:")
            .and_then(|raw| raw.parse::<u32>().ok())
        else {
            return;
        };
        self.select(window, id);
    }

    /// Drop the selection and return the form to the documented defaults.
    fn reset_form(&self, window: &AppWindow) {
        self.selected.set(None);
        write_draft(window, &ServerDraft::default());
        clear_feedback(window);
        self.publish(window);
    }

    /// Validate the form and either update the selected server or add a new one.
    fn save(&self, window: &AppWindow) {
        let valid = match read_draft(window).validate() {
            Ok(valid) => valid,
            Err(error) => {
                window.set_form_error(error.to_string().into());
                return;
            },
        };
        window.set_form_error(SharedString::new());

        // Resolve the target before taking a mutable borrow, so the store is never borrowed
        // twice at once.
        let existing = self
            .selected
            .get()
            .filter(|id| self.store.borrow().get(*id).is_some());

        let id = match existing {
            Some(id) => {
                self.store.borrow_mut().update(id, valid);
                id
            },
            None => self.store.borrow_mut().insert(valid),
        };

        self.selected.set(Some(id));
        self.publish(window);
    }

    /// Remove the server `id`, clearing the form when it was the one being edited.
    fn remove(&self, window: &AppWindow, id: u32) {
        if !self.store.borrow_mut().remove(id) {
            return;
        }
        if self.selected.get() == Some(id) {
            self.reset_form(window);
        } else {
            self.publish(window);
        }
    }

    /// Validate the form and ask the probe whether the server is reachable.
    fn test(&self, window: &AppWindow) {
        let valid = match read_draft(window).validate() {
            Ok(valid) => valid,
            Err(error) => {
                window.set_form_error(error.to_string().into());
                clear_result(window);
                return;
            },
        };

        window.set_form_error(SharedString::new());
        window.set_test_state("testing".into());
        window.set_test_message(format!("Contacting {}…", valid.params.endpoint()).into());

        // A test against the very server that is selected also tells us its reachability, so
        // mark it as being checked. Editing the form first would make that a different server,
        // hence the comparison against what is actually stored.
        if let Some(id) = self.probed_server(&valid.params) {
            self.store
                .borrow_mut()
                .set_status(id, ConnectionStatus::Connecting);
            self.publish(window);
        }

        // The probe answers from a network task, so the outcome is marshalled back onto the UI
        // thread through a window callback; if the window is gone by then it is simply dropped.
        let weak = window.as_weak();
        self.probe.probe(
            &valid.params,
            Box::new(move |outcome| {
                let (state, message) = render_outcome(&outcome);
                let _ = weak.upgrade_in_event_loop(move |window| {
                    window.invoke_probe_finished(state.into(), message.as_str().into());
                });
            }),
        );
    }

    /// The saved server a test of `params` is actually a test of, if any.
    ///
    /// Only an exact match counts: once the form has been edited the attempt says nothing
    /// about the stored server, and stamping its dot would be a claim we cannot back.
    fn probed_server(&self, params: &ConnectionParams) -> Option<u32> {
        let id = self.selected.get()?;
        let store = self.store.borrow();
        let profile = store.get(id)?;
        (profile.params == *params).then_some(id)
    }

    /// Apply a finished attempt: show it, and record what it proved about the server.
    fn finish_probe(&self, window: &AppWindow, state: &str, message: &str) {
        window.set_test_state(state.into());
        window.set_test_message(message.into());

        // The dot means "reachable when last checked", not "a session is open" — Studio holds
        // no session until the navigator opens one in G3.
        let status = match state {
            "ok" => ConnectionStatus::Connected,
            "error" => ConnectionStatus::Error,
            _ => ConnectionStatus::Disconnected,
        };
        if let Some(id) = self.selected.get() {
            self.store.borrow_mut().set_status(id, status);
        }
        self.publish(window);
    }
}

/// Wire every window callback to `controller` and paint the initial state.
pub fn install(window: &AppWindow, controller: Rc<Controller>) {
    window.set_version(env!("CARGO_PKG_VERSION").into());
    window.set_sql_lines(ModelRc::new(VecModel::from(sql_lines(SCRATCH_SQL))));
    window.set_grid_columns(ModelRc::new(VecModel::<GridColumn>::default()));
    window.set_result_summary("no query run".into());
    controller.reset_form(window);

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_filter_changed(move |text| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        handler.store.borrow_mut().set_filter(text.as_str());
        handler.publish(&window);
    });

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_select_node(move |id| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        handler.select_node(&window, id.as_str());
    });

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_toggle_node(move |id| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        handler.tree.borrow_mut().toggle(id.as_str());
        handler.publish(&window);
    });

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_refresh_tree(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        handler.publish(&window);
    });

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_collapse_tree(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        handler.tree.borrow_mut().collapse_all();
        handler.publish(&window);
    });

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_select_server(move |id| {
        let (Some(window), Ok(id)) = (weak.upgrade(), u32::try_from(id)) else {
            return;
        };
        handler.select(&window, id);
    });

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_edit_server(move |id| {
        let (Some(window), Ok(id)) = (weak.upgrade(), u32::try_from(id)) else {
            return;
        };
        handler.select(&window, id);
    });

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_remove_server(move |id| {
        let (Some(window), Ok(id)) = (weak.upgrade(), u32::try_from(id)) else {
            return;
        };
        handler.remove(&window, id);
    });

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_save_server(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        handler.save(&window);
    });

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_test_connection(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        handler.test(&window);
    });

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_reset_form(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        handler.reset_form(&window);
    });

    let weak = window.as_weak();
    let handler = Rc::clone(&controller);
    window.on_probe_finished(move |state, message| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        handler.finish_probe(&window, state.as_str(), message.as_str());
    });

    // Editing any field invalidates the last test result: it was measured against different
    // parameters, and leaving it on screen would vouch for something never tried.
    let weak = window.as_weak();
    window.on_draft_changed(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        clear_feedback(&window);
    });
}

/// Reset the panes that describe a selection when there is none.
fn publish_no_selection(window: &AppWindow) {
    window.set_inspector_title(SharedString::new());
    window.set_inspector_kind(SharedString::new());
    window.set_inspector_properties(ModelRc::new(VecModel::<Property>::default()));

    window.set_server_crumb("—".into());
    window.set_database_crumb("—".into());

    window.set_connected(false);
    window.set_connection_status(ConnectionStatus::Disconnected.as_tag().into());
    window.set_connection_label("No connection".into());

    window.set_status_server("no server".into());
    window.set_status_database("—".into());
}

/// Flatten a profile into the display-ready row the Connection Manager renders.
fn server_row(profile: &ServerProfile) -> ServerRow {
    ServerRow {
        id: id_for_ui(profile.id),
        name: profile.name.as_str().into(),
        endpoint: profile.params.endpoint().into(),
        detail: profile.params.detail().into(),
        env: profile.environment.as_tag().into(),
        status: profile.status.as_tag().into(),
        tls: profile.params.tls,
    }
}

/// Build one inspector row.
fn property(label: &str, value: &str, mono: bool) -> Property {
    Property {
        label: label.into(),
        value: value.into(),
        mono,
    }
}

/// Split editor text into the per-line, per-token model the editor renders.
///
/// Colouring is per token, as in the visual reference. The runs of a line include the
/// whitespace between them, so laying them side by side reproduces the source line exactly.
fn sql_lines(sql: &str) -> Vec<SqlLine> {
    sql.lines()
        .map(|line| {
            let tokens: Vec<SqlToken> = highlight_line(line)
                .into_iter()
                .map(|token| SqlToken {
                    text: token.text.as_str().into(),
                    kind: token.kind.as_tag().into(),
                })
                .collect();
            SqlLine {
                tokens: ModelRc::new(VecModel::from(tokens)),
            }
        })
        .collect()
}

/// Narrow a store identity to the `int` Slint uses.
///
/// Identities come from a counter that starts at 1, so the saturating branch is unreachable in
/// practice; it exists so the conversion cannot panic.
fn id_for_ui(id: u32) -> i32 {
    i32::try_from(id).unwrap_or(NO_SELECTION)
}

/// Read the connection form into a draft.
fn read_draft(window: &AppWindow) -> ServerDraft {
    ServerDraft {
        name: window.get_draft_name().to_string(),
        host: window.get_draft_host().to_string(),
        port: window.get_draft_port().to_string(),
        user: window.get_draft_user().to_string(),
        password: window.get_draft_password().to_string(),
        database: window.get_draft_database().to_string(),
        tls: window.get_draft_tls(),
        ca_path: window.get_draft_ca_path().to_string(),
        environment: Environment::from_tag(window.get_draft_env().as_str()),
    }
}

/// Write a draft into the connection form.
fn write_draft(window: &AppWindow, draft: &ServerDraft) {
    window.set_draft_name(draft.name.as_str().into());
    window.set_draft_host(draft.host.as_str().into());
    window.set_draft_port(draft.port.as_str().into());
    window.set_draft_user(draft.user.as_str().into());
    window.set_draft_password(draft.password.as_str().into());
    window.set_draft_database(draft.database.as_str().into());
    window.set_draft_env(draft.environment.as_tag().into());
    window.set_draft_tls(draft.tls);
    window.set_draft_ca_path(draft.ca_path.as_str().into());
}

/// Clear the validation message and the last test result.
fn clear_feedback(window: &AppWindow) {
    window.set_form_error(SharedString::new());
    clear_result(window);
}

/// Clear the last test result, leaving any validation message in place.
fn clear_result(window: &AppWindow) {
    window.set_test_state("idle".into());
    window.set_test_message(SharedString::new());
}

/// Map a probe outcome onto the banner's (state, message) pair.
///
/// Kept free of the window so it can run on the thread that produced the outcome; only the
/// two strings cross back to the UI thread.
fn render_outcome(outcome: &ProbeOutcome) -> (&'static str, String) {
    match outcome {
        ProbeOutcome::Reachable { detail } => ("ok", detail.clone()),
        ProbeOutcome::Failed { message } => ("error", message.clone()),
        ProbeOutcome::Unavailable { reason } => ("info", reason.clone()),
    }
}

#[cfg(test)]
mod tests {
    // `Model` supplies the row accessors used to read a line's runs back out.
    use slint::Model;

    use super::*;

    /// The rendered text of one line, reassembled from its runs.
    fn rendered(line: &SqlLine) -> String {
        let mut text = String::new();
        for token in line.tokens.iter() {
            text.push_str(&token.text);
        }
        text
    }

    #[test]
    fn every_editor_line_reassembles_from_its_runs() {
        let source = "SELECT 1;

   -- trailing note";
        let lines = sql_lines(source);
        assert_eq!(lines.len(), 3);
        for (line, expected) in lines.iter().zip(source.lines()) {
            assert_eq!(rendered(line), expected);
        }
    }

    #[test]
    fn the_scratch_buffer_opens_with_a_comment_and_holds_a_statement() {
        let lines = sql_lines(SCRATCH_SQL);
        assert_eq!(
            lines[0]
                .tokens
                .row_data(0)
                .map(|token| token.kind.to_string()),
            Some("comment".to_owned())
        );
        assert!(
            lines
                .iter()
                .any(|line| line.tokens.iter().any(|token| token.kind == "keyword")),
            "the buffer also contains a runnable statement"
        );
    }
}
