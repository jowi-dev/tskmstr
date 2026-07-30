//! Keychain-backed Jira API token storage and resolution.
//!
//! Token resolution follows a fixed precedence: an explicitly supplied
//! environment variable value always wins over whatever is stored in the
//! keychain. This lets CI and scripting scenarios override the interactively
//! configured token without touching the keychain.

use thiserror::Error;

pub mod macos;

pub use macos::MacosKeychain;

/// A store capable of retrieving and persisting the Jira API token.
///
/// Implementations may back this with the OS keychain, a file, or (for
/// tests) an in-memory map.
pub trait KeychainStore {
    /// Fetch the currently stored token, if any.
    fn get_token(&self) -> Result<Option<String>, KeychainError>;

    /// Persist `token`, replacing any previously stored value.
    fn set_token(&self, token: &str) -> Result<(), KeychainError>;
}

/// Errors that can occur while reading or writing a keychain-backed token.
#[derive(Debug, Error)]
pub enum KeychainError {
    /// The underlying backend failed in a way not otherwise categorized.
    #[error("keychain operation failed: {0}")]
    Backend(String),
}

/// Errors that can occur while resolving the token to use for Jira requests.
#[derive(Debug, Error)]
pub enum AuthError {
    /// The token could not be read from the keychain.
    #[error("failed to read token from keychain: {0}")]
    Keychain(#[from] KeychainError),

    /// No token was available from either the environment or the keychain.
    #[error(
        "no Jira API token found; run `tm auth login` to store one, or set \
         the JIRA_API_TOKEN environment variable"
    )]
    NoTokenAvailable,
}

/// Resolve the Jira API token to use, preferring `env_token` over whatever is
/// stored in `store`.
///
/// `env_token` must be supplied by the caller (typically via
/// `std::env::var("JIRA_API_TOKEN").ok()`); this function never reads process
/// environment itself. An empty-string env value is treated as absent.
pub fn resolve_token(
    store: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<String, AuthError> {
    if let Some(token) = env_token.filter(|token| !token.is_empty()) {
        return Ok(token);
    }

    store.get_token()?.ok_or(AuthError::NoTokenAvailable)
}

/// An in-memory [`KeychainStore`] fake for use in tests.
///
/// This is a plain public struct (not `#[cfg(test)]`-gated) so that
/// integration tests and other crates' test code can depend on it directly.
pub struct InMemoryKeychain {
    token: std::sync::Mutex<Option<String>>,
}

impl InMemoryKeychain {
    /// Create an empty in-memory keychain with no stored token.
    pub fn empty() -> Self {
        Self {
            token: std::sync::Mutex::new(None),
        }
    }

    /// Create an in-memory keychain pre-populated with `token`.
    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            token: std::sync::Mutex::new(Some(token.into())),
        }
    }
}

impl KeychainStore for InMemoryKeychain {
    fn get_token(&self) -> Result<Option<String>, KeychainError> {
        Ok(self.token.lock().expect("lock poisoned").clone())
    }

    fn set_token(&self, token: &str) -> Result<(), KeychainError> {
        *self.token.lock().expect("lock poisoned") = Some(token.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_token_wins_over_keychain() {
        let store = InMemoryKeychain::with_token("keychain-token");
        let resolved = resolve_token(&store, Some("env-token".to_string())).unwrap();
        assert_eq!(resolved, "env-token");
    }

    #[test]
    fn falls_back_to_keychain_when_env_absent() {
        let store = InMemoryKeychain::with_token("keychain-token");
        let resolved = resolve_token(&store, None).unwrap();
        assert_eq!(resolved, "keychain-token");
    }

    #[test]
    fn empty_string_env_treated_as_absent() {
        let store = InMemoryKeychain::with_token("keychain-token");
        let resolved = resolve_token(&store, Some(String::new())).unwrap();
        assert_eq!(resolved, "keychain-token");
    }

    #[test]
    fn neither_env_nor_keychain_is_an_actionable_error() {
        let store = InMemoryKeychain::empty();
        let err = resolve_token(&store, None).expect_err("should fail");
        let message = err.to_string();
        assert!(
            message.contains("tm auth login"),
            "error should mention `tm auth login`: {message}"
        );
        assert!(
            message.contains("JIRA_API_TOKEN"),
            "error should mention JIRA_API_TOKEN: {message}"
        );
    }
}
