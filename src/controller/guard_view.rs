//! The questions asked before work is lost for good, and only those: closing Studio, or
//! disconnecting a server, with an open transaction or unsaved edits, and closing a tab whose
//! result has unsaved edits. Everything that can be redone (Discard, Revert, Rollback, Run) does
//! not ask.

use std::rc::Rc;

use slint::{CloseRequestResponse, ComponentHandle};

use super::{AppWindow, Controller, Pending, TxnState};

impl Controller {
    /// What would be lost right now if Studio closed, one line each.
    fn unsaved_work(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.txn.get() != TxnState::Idle
            && let Some((_, database)) = self.target.borrow().as_ref()
        {
            out.push(format!(
                "A transaction is open on {database}. Closing rolls it back."
            ));
        }
        if let Some(d) = self.data.borrow().as_ref() {
            let n = d.edits.count();
            if n > 0 {
                out.push(format!("{} unsaved row{} in {}.", n, plural(n), d.name));
            }
        }
        let active = self.active_tab.get();
        for tab in self.tabs.borrow().iter() {
            let edits = if tab.id == active {
                self.result_pending()
            } else {
                tab.result_edit.as_ref().map_or(0, |e| e.edits.count())
            };
            if edits > 0 {
                out.push(format!(
                    "{} unsaved row{} in the results of {}.",
                    edits,
                    plural(edits),
                    tab.title
                ));
            }
        }
        let dirty: Vec<String> = self
            .tabs
            .borrow()
            .iter()
            .map(|t| t.id)
            .filter(|id| self.tab_dirty(*id))
            .filter_map(|id| {
                self.tabs
                    .borrow()
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.title.clone())
            })
            .collect();
        if !dirty.is_empty() {
            out.push(format!("Unsaved script: {}.", dirty.join(", ")));
        }
        out
    }

    /// The window's close button: asks when something would be lost.
    pub(super) fn close_requested(&self, window: &AppWindow) -> CloseRequestResponse {
        if self.quitting.get() {
            return CloseRequestResponse::HideWindow;
        }
        let lost = self.unsaved_work();
        if lost.is_empty() {
            return CloseRequestResponse::HideWindow;
        }
        self.ask(window, Pending::Quit, "Quit NusaDB Studio?", &lost, "Quit");
        CloseRequestResponse::KeepWindowShown
    }

    pub(super) fn quit_confirmed(&self, window: &AppWindow) {
        self.quitting.set(true);
        let _ = window.hide();
        let _ = slint::quit_event_loop();
    }

    /// Disconnect, asking first when it would roll back a transaction or drop unsaved edits.
    pub(super) fn disconnect_checked(&self, window: &AppWindow, id: u32) {
        let mut lost = Vec::new();
        if self.txn.get() != TxnState::Idle
            && let Some((server, database)) = self.target.borrow().as_ref()
            && *server == id
        {
            lost.push(format!(
                "The open transaction on {database} is rolled back."
            ));
        }
        if let Some(d) = self.data.borrow().as_ref()
            && d.server == id
        {
            let n = d.edits.count();
            if n > 0 {
                lost.push(format!(
                    "{} unsaved row{} in {} are lost.",
                    n,
                    plural(n),
                    d.name
                ));
            }
        }
        if lost.is_empty() {
            self.disconnect(window, id);
            return;
        }
        let name = self
            .store
            .borrow()
            .get(id)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        self.ask(
            window,
            Pending::Disconnect(id),
            &format!("Disconnect {name}?"),
            &lost,
            "Disconnect",
        );
    }

    /// Unsaved result edits in tab `id`, for the close-tab question.
    pub(super) fn tab_result_edits(&self, id: u32) -> usize {
        if id == self.active_tab.get() {
            return self.result_pending();
        }
        self.tabs
            .borrow()
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.result_edit.as_ref())
            .map_or(0, |e| e.edits.count())
    }
}

/// Route the window's close button through the controller.
pub(super) fn install_close_guard(window: &AppWindow, controller: &Rc<Controller>) {
    let weak = window.as_weak();
    let handler = Rc::clone(controller);
    window.window().on_close_requested(move || {
        weak.upgrade()
            .map_or(CloseRequestResponse::HideWindow, |w| {
                handler.close_requested(&w)
            })
    });
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
