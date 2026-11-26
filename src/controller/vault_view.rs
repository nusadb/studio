//! Remembered passwords: written to the system keychain when a server is saved with "Remember
//! password", read back at start-up. Keychain calls can block (an unlock prompt), so they run
//! on a thread of their own and report back to the UI.

use slint::ComponentHandle;

use super::{AppWindow, Controller, on_ui};
use crate::keychain;
use crate::model::ServerProfile;

impl Controller {
    /// After a save: remember, update or forget the server's password.
    pub(super) fn vault_saved(&self, window: &AppWindow, before: Option<ServerProfile>, id: u32) {
        let Some(after) = self.store.borrow().get(id).cloned() else {
            return;
        };
        let account =
            |p: &ServerProfile| keychain::account(&p.params.user, &p.params.host, p.params.port);
        let new_account = account(&after);
        // A login that was remembered under another account (or no longer should be) goes.
        let stale = before
            .filter(|b| b.remember && (!after.remember || account(b) != new_account))
            .map(|b| account(&b));
        let store = after
            .remember
            .then(|| after.params.password.clone())
            .filter(|p| !p.is_empty());
        if stale.is_none() && store.is_none() {
            return;
        }
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let mut outcome = Ok(());
            if let Some(old) = stale {
                outcome = keychain::forget(&old);
            }
            if let Some(password) = store {
                outcome = outcome.and(keychain::set(&new_account, &password));
            }
            if let Err(message) = outcome {
                on_ui(&weak, move |c, w| c.toast(w, &message, true));
            }
        });
    }

    /// At start-up: fill in the passwords of servers saved with "Remember password".
    pub(super) fn vault_load(&self, window: &AppWindow) {
        let wanted: Vec<(u32, String)> = self
            .store
            .borrow()
            .all()
            .filter(|p| p.remember && p.params.password.is_empty())
            .map(|p| {
                (
                    p.id,
                    keychain::account(&p.params.user, &p.params.host, p.params.port),
                )
            })
            .collect();
        if wanted.is_empty() {
            return;
        }
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let mut found = Vec::new();
            let mut failure = None;
            for (id, account) in wanted {
                match keychain::get(&account) {
                    Ok(Some(password)) => found.push((id, password)),
                    Ok(None) => {},
                    Err(message) => {
                        failure = Some(message);
                        break;
                    },
                }
            }
            on_ui(&weak, move |c, w| {
                for (id, password) in found {
                    c.store.borrow_mut().set_password(id, password);
                }
                if let Some(message) = failure {
                    c.toast(w, &message, true);
                }
            });
        });
    }
}
