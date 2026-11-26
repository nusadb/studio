//! Remembered passwords, kept in the operating system's keychain — macOS Keychain, Windows
//! Credential Manager, or a Secret Service on Linux (GNOME Keyring, KWallet) — and never in
//! `servers.json`.

const SERVICE: &str = "nusadb-studio";

/// The keychain account a server's password is filed under: the login it belongs to, so two
/// saved servers for the same user and address share it.
#[must_use]
pub fn account(user: &str, host: &str, port: u16) -> String {
    format!("{user}@{host}:{port}")
}

fn entry(account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, account).map_err(|e| describe(&e))
}

fn describe(error: &keyring::Error) -> String {
    match error {
        keyring::Error::NoStorageAccess(_) | keyring::Error::PlatformFailure(_) => {
            "No system keychain is available. The password is kept for this session only."
                .to_owned()
        },
        other => format!("The system keychain refused: {other}"),
    }
}

/// The remembered password, or `None` when there is none.
///
/// # Errors
///
/// When the keychain cannot be reached.
pub fn get(account: &str) -> Result<Option<String>, String> {
    match entry(account)?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(describe(&e)),
    }
}

/// Remember `password`.
///
/// # Errors
///
/// When the keychain cannot be reached.
pub fn set(account: &str, password: &str) -> Result<(), String> {
    entry(account)?
        .set_password(password)
        .map_err(|e| describe(&e))
}

/// Forget the password; forgetting one that is not there is not an error.
///
/// # Errors
///
/// When the keychain cannot be reached.
pub fn forget(account: &str) -> Result<(), String> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(describe(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::{account, forget, get, set};

    #[test]
    fn the_account_names_the_login_not_the_saved_name() {
        assert_eq!(
            account("analyst", "db.internal", 5433),
            "analyst@db.internal:5433"
        );
    }

    /// Needs a running Secret Service: `cargo test -- --ignored` inside an unlocked session.
    #[test]
    #[ignore = "needs a system keychain"]
    fn a_password_round_trips_through_the_system_keychain() {
        let who = account("studio-test", "keychain.invalid", 1);
        forget(&who).unwrap();
        assert_eq!(get(&who).unwrap(), None);
        set(&who, "s3cret").unwrap();
        assert_eq!(get(&who).unwrap().as_deref(), Some("s3cret"));
        set(&who, "rotated").unwrap();
        assert_eq!(get(&who).unwrap().as_deref(), Some("rotated"));
        forget(&who).unwrap();
        forget(&who).unwrap();
        assert_eq!(get(&who).unwrap(), None);
    }
}
