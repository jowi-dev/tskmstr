//! macOS keychain-backed [`KeychainStore`] implementation, shelling out to
//! the `security` command-line tool.

use std::process::Command;

use super::{KeychainError, KeychainStore};

/// Account name used for all tskmstr keychain entries.
const ACCOUNT: &str = "jira";

/// A [`KeychainStore`] backed by the macOS `security` CLI.
///
/// Tokens are stored as a generic password item under the configured
/// `service` name and the fixed account `jira`. The service name defaults to
/// `tskmstr` but can be overridden (for example, tests use a throwaway
/// service so they never touch a real user's stored token).
pub struct MacosKeychain {
    service: String,
}

impl Default for MacosKeychain {
    fn default() -> Self {
        Self::new()
    }
}

impl MacosKeychain {
    /// Create a keychain store using the default `tskmstr` service name.
    pub fn new() -> Self {
        Self {
            service: "tskmstr".to_string(),
        }
    }

    /// Create a keychain store using a custom service name.
    ///
    /// Useful for tests, which should never read or write the real
    /// `tskmstr` keychain entry.
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

impl KeychainStore for MacosKeychain {
    fn get_token(&self) -> Result<Option<String>, KeychainError> {
        let output = Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                &self.service,
                "-a",
                ACCOUNT,
                "-w",
            ])
            .output()
            .map_err(|err| KeychainError::Backend(format!("failed to run `security`: {err}")))?;

        interpret_get_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    fn set_token(&self, token: &str) -> Result<(), KeychainError> {
        let output = Command::new("security")
            .args([
                "add-generic-password",
                "-s",
                &self.service,
                "-a",
                ACCOUNT,
                "-w",
                token,
                "-U",
            ])
            .output()
            .map_err(|err| KeychainError::Backend(format!("failed to run `security`: {err}")))?;

        interpret_set_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        )
    }
}

/// Interpret the result of a `security find-generic-password -w` invocation.
///
/// This is a pure function over the command's exit code and captured
/// stdout/stderr so the parsing logic can be unit tested without shelling
/// out to the real `security` binary.
///
/// A missing item is reported by `security` via a non-zero exit status
/// (typically 44, `errSecItemNotFound`) with a "could not be found" message
/// on stderr; that case is mapped to `Ok(None)` rather than an error. Any
/// other non-zero exit is treated as a genuine backend failure. Neither
/// branch ever echoes `stdout` (the token) into an error message.
fn interpret_get_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<Option<String>, KeychainError> {
    match exit_code {
        Some(0) => Ok(Some(stdout.trim_end_matches(['\n', '\r']).to_string())),
        Some(_) if item_not_found(stderr) => Ok(None),
        Some(code) => Err(KeychainError::Backend(format!(
            "security find-generic-password failed (exit {code}): {}",
            stderr.trim()
        ))),
        None => Err(KeychainError::Backend(
            "security find-generic-password was terminated by a signal".to_string(),
        )),
    }
}

/// Interpret the result of a `security add-generic-password` invocation.
///
/// Pure over the exit code and captured stderr for the same testability
/// reasons as [`interpret_get_output`]. The token value itself is never
/// passed to this function, so it cannot leak into an error message.
fn interpret_set_output(exit_code: Option<i32>, stderr: &str) -> Result<(), KeychainError> {
    match exit_code {
        Some(0) => Ok(()),
        Some(code) => Err(KeychainError::Backend(format!(
            "security add-generic-password failed (exit {code}): {}",
            stderr.trim()
        ))),
        None => Err(KeychainError::Backend(
            "security add-generic-password was terminated by a signal".to_string(),
        )),
    }
}

/// Whether `security`'s stderr indicates the requested item does not exist.
fn item_not_found(stderr: &str) -> bool {
    stderr.contains("could not be found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_output_success_returns_trimmed_token() {
        let result = interpret_get_output(Some(0), "secret-token\n", "").unwrap();
        assert_eq!(result, Some("secret-token".to_string()));
    }

    #[test]
    fn get_output_item_not_found_returns_none() {
        let stderr = "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.\n";
        let result = interpret_get_output(Some(44), "", stderr).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn get_output_other_failure_is_an_error() {
        let err = interpret_get_output(Some(1), "", "security: some other failure").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("some other failure"));
    }

    #[test]
    fn get_output_signal_termination_is_an_error() {
        let err = interpret_get_output(None, "", "").unwrap_err();
        assert!(err.to_string().contains("signal"));
    }

    #[test]
    fn set_output_success_is_ok() {
        interpret_set_output(Some(0), "").unwrap();
    }

    #[test]
    fn set_output_failure_is_an_error() {
        let err = interpret_set_output(Some(1), "security: duplicate item").unwrap_err();
        assert!(err.to_string().contains("duplicate item"));
    }

    #[test]
    fn set_output_error_never_contains_the_token() {
        // Regression guard: the token is never passed into the interpreter,
        // so it cannot appear in a constructed error message even if a
        // caller passes attacker-controlled stderr.
        let err = interpret_set_output(Some(1), "unrelated failure").unwrap_err();
        assert!(!err.to_string().contains("super-secret-token"));
    }

    /// Round-trips a token through the real macOS keychain using a
    /// throwaway service name, then cleans up the entry it created.
    ///
    /// Ignored by default: it shells out to the real `security` CLI and
    /// mutates (a scoped corner of) the user's actual keychain.
    #[test]
    #[ignore = "touches the real macOS keychain; run explicitly with --ignored"]
    fn round_trips_through_real_keychain() {
        let service = "tskmstr-test";
        let store = MacosKeychain::with_service(service);

        let cleanup = || {
            let _ = Command::new("security")
                .args(["delete-generic-password", "-s", service, "-a", ACCOUNT])
                .output();
        };
        cleanup();

        store.set_token("integration-test-token").unwrap();
        let fetched = store.get_token().unwrap();

        cleanup();

        assert_eq!(fetched, Some("integration-test-token".to_string()));
    }
}
