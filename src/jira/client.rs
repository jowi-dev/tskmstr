//! HTTP client for the Jira Cloud REST API (v3).
//!
//! [`JiraClient`] is the trait callers depend on; [`HttpJiraClient`] is the
//! `reqwest`-backed implementation used in production. Tests exercise
//! [`HttpJiraClient`] directly against an `httpmock` server.

use crate::jira::types::{
    CreateIssueRequest, Issue, Myself, RemoteLinkRequest, SearchResult, Transition,
};
use reqwest::blocking::Client;
use thiserror::Error;

/// Connection details for a [`HttpJiraClient`].
pub struct JiraClientContext {
    /// Base URL of the Jira instance, e.g. `https://example.atlassian.net`.
    pub base_url: String,
    /// Email address of the authenticated user, used for HTTP Basic auth.
    pub email: String,
    /// Jira API token, used as the HTTP Basic auth password.
    pub token: String,
}

/// Errors that can occur while calling the Jira REST API.
///
/// Display output never includes the API token.
#[derive(Debug, Error)]
pub enum JiraError {
    /// The requested issue or resource does not exist (HTTP 404).
    #[error("Jira issue not found: {key}")]
    NotFound {
        /// The issue key that was not found.
        key: String,
    },

    /// The request was rejected as unauthenticated or unauthorized (HTTP 401/403).
    #[error("Jira request unauthorized; check the configured email and API token")]
    Unauthorized,

    /// The request could not be sent, or the response could not be read.
    #[error("Jira request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// Jira returned an error response not otherwise categorized.
    #[error("Jira API error ({status}): {message}")]
    Api {
        /// HTTP status code returned by Jira.
        status: u16,
        /// Error message extracted from the response body, if any.
        message: String,
    },
}

/// Behavior tskmstr needs from the Jira Cloud REST API.
pub trait JiraClient {
    /// Fetch the authenticated user (`GET /myself`). Used to verify auth is
    /// configured correctly.
    fn myself(&self) -> Result<Myself, JiraError>;

    /// Fetch a single issue by key (`GET /issue/{key}`).
    fn get_issue(&self, key: &str) -> Result<Issue, JiraError>;

    /// Create a new issue (`POST /issue`).
    ///
    /// The create endpoint's response only contains `{id, key, self}`, not
    /// the full issue fields, so this performs the create request and then
    /// fetches the created issue by key. This costs an extra request but
    /// gives callers a complete [`Issue`] without duplicating field-mapping
    /// logic.
    fn create_issue(&self, req: &CreateIssueRequest) -> Result<Issue, JiraError>;

    /// Attach a remote link (e.g. a GitHub PR) to an issue
    /// (`POST /issue/{key}/remotelink`).
    fn add_remote_link(&self, key: &str, link: &RemoteLinkRequest) -> Result<(), JiraError>;

    /// List the workflow transitions available on an issue
    /// (`GET /issue/{key}/transitions`).
    fn transitions(&self, key: &str) -> Result<Vec<Transition>, JiraError>;

    /// Apply a workflow transition to an issue
    /// (`POST /issue/{key}/transitions`).
    fn transition(&self, key: &str, transition_id: &str) -> Result<(), JiraError>;

    /// Run a JQL search (`POST /search/jql`).
    ///
    /// Only the first page of results is fetched; `nextPageToken` is
    /// deserialized but not automatically followed. This is sufficient for
    /// tskmstr's current use (a user's own open tickets rarely exceed one
    /// page) but callers listing large result sets will need to page
    /// manually in a future revision.
    fn search(&self, jql: &str) -> Result<SearchResult, JiraError>;

    /// Check that a project exists and is visible to the authenticated user.
    ///
    /// Used by `tm auth status` to verify the configured token has access to
    /// the expected project, beyond merely being a valid credential.
    fn get_project(&self, key: &str) -> Result<(), JiraError>;
}

/// [`JiraClient`] implementation backed by `reqwest::blocking`.
pub struct HttpJiraClient {
    ctx: JiraClientContext,
    http: Client,
}

impl HttpJiraClient {
    /// Build a client for the given connection context.
    pub fn new(ctx: JiraClientContext) -> Self {
        Self {
            ctx,
            http: Client::new(),
        }
    }

    /// Build the full URL for a path under `/rest/api/3/`.
    fn url(&self, path: &str) -> String {
        format!("{}/rest/api/3{path}", self.ctx.base_url)
    }

    /// Turn a completed response into a typed value, mapping non-2xx status
    /// codes to the appropriate [`JiraError`] variant.
    ///
    /// `key` is used only to populate [`JiraError::NotFound`] when the
    /// response is a 404; pass an empty string for endpoints with no
    /// associated issue key.
    fn parse<T: for<'de> serde::Deserialize<'de>>(
        response: reqwest::blocking::Response,
        key: &str,
    ) -> Result<T, JiraError> {
        let status = response.status();
        if status.is_success() {
            return Ok(response.json::<T>()?);
        }
        Err(Self::error_for_status(response, status, key))
    }

    /// Build the [`JiraError`] for a non-2xx response.
    fn error_for_status(
        response: reqwest::blocking::Response,
        status: reqwest::StatusCode,
        key: &str,
    ) -> JiraError {
        if status == reqwest::StatusCode::NOT_FOUND {
            return JiraError::NotFound {
                key: key.to_string(),
            };
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return JiraError::Unauthorized;
        }

        let body = response.text().unwrap_or_default();
        let message = extract_error_message(&body);
        JiraError::Api {
            status: status.as_u16(),
            message,
        }
    }
}

/// Extract a human-readable error message from a Jira error response body.
///
/// Jira error bodies vary by endpoint: some use a top-level `errorMessages`
/// array of strings, others an `errors` object of field-to-message pairs.
/// Falls back to the raw body when neither shape is recognized.
fn extract_error_message(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };

    if let Some(messages) = value.get("errorMessages").and_then(|v| v.as_array()) {
        let joined: Vec<&str> = messages.iter().filter_map(|m| m.as_str()).collect();
        if !joined.is_empty() {
            return joined.join("; ");
        }
    }

    if let Some(errors) = value.get("errors").and_then(|v| v.as_object()) {
        let joined: Vec<String> = errors
            .iter()
            .filter_map(|(field, msg)| msg.as_str().map(|m| format!("{field}: {m}")))
            .collect();
        if !joined.is_empty() {
            return joined.join("; ");
        }
    }

    body.to_string()
}

impl JiraClient for HttpJiraClient {
    fn myself(&self) -> Result<Myself, JiraError> {
        let response = self
            .http
            .get(self.url("/myself"))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .send()?;
        Self::parse(response, "")
    }

    fn get_issue(&self, _key: &str) -> Result<Issue, JiraError> {
        todo!()
    }

    fn create_issue(&self, _req: &CreateIssueRequest) -> Result<Issue, JiraError> {
        todo!()
    }

    fn add_remote_link(&self, _key: &str, _link: &RemoteLinkRequest) -> Result<(), JiraError> {
        todo!()
    }

    fn transitions(&self, _key: &str) -> Result<Vec<Transition>, JiraError> {
        todo!()
    }

    fn transition(&self, _key: &str, _transition_id: &str) -> Result<(), JiraError> {
        todo!()
    }

    fn search(&self, _jql: &str) -> Result<SearchResult, JiraError> {
        todo!()
    }

    fn get_project(&self, _key: &str) -> Result<(), JiraError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;

    fn test_ctx(server: &MockServer) -> JiraClientContext {
        JiraClientContext {
            base_url: server.url(""),
            email: "ada@example.com".to_string(),
            token: "test-token".to_string(),
        }
    }

    #[test]
    fn myself_returns_authenticated_user() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/myself")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                );
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "accountId": "abc123",
                    "displayName": "Ada Lovelace",
                    "emailAddress": "ada@example.com"
                }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let myself = client.myself().expect("myself should succeed");

        mock.assert();
        assert_eq!(myself.account_id, "abc123");
        assert_eq!(myself.display_name, "Ada Lovelace");
    }

    #[test]
    fn myself_maps_401_to_unauthorized() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/myself");
            then.status(401)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Unauthorized"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client.myself().expect_err("should fail");

        assert!(matches!(err, JiraError::Unauthorized));
    }
}
