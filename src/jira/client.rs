//! HTTP client for the Jira Cloud REST API (v3).
//!
//! [`JiraClient`] is the trait callers depend on; [`HttpJiraClient`] is the
//! `reqwest`-backed implementation used in production. Tests exercise
//! [`HttpJiraClient`] directly against an `httpmock` server.

use crate::jira::types::{
    CreateIssueRequest, Issue, JiraUser, Myself, RemoteLinkRequest, SearchResult, Transition,
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
    /// The requested issue does not exist (HTTP 404).
    #[error("Jira issue not found: {key}")]
    NotFound {
        /// The issue key that was not found.
        key: String,
    },

    /// [`JiraClient::assignable_users`] got a 404 for `project`: unlike an
    /// issue key 404, this doesn't mean "not found by key" (there is no
    /// key), it means the project itself doesn't exist or isn't visible to
    /// the authenticated user. Kept distinct from [`JiraError::NotFound`] so
    /// the error text names a project and an assignable-user search, not an
    /// issue.
    #[error("Jira project not found while searching assignable users: {project}")]
    ProjectNotFound {
        /// The project key that was not found.
        project: String,
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
    ///
    /// Note: the response shape for `/rest/api/3/search/jql` follows Atlassian's
    /// documented contract (`issues` plus an optional `nextPageToken`), but has
    /// not yet been verified against a live Jira instance (no API token was
    /// available at implementation time). Verify end-to-end before relying on
    /// this in production.
    fn search(&self, jql: &str) -> Result<SearchResult, JiraError>;

    /// Check that a project exists and is visible to the authenticated user.
    ///
    /// Used by `tm auth status` to verify the configured token has access to
    /// the expected project, beyond merely being a valid credential.
    fn get_project(&self, key: &str) -> Result<(), JiraError>;

    /// List users eligible to be assigned an issue in `project`
    /// (`GET /user/assignable/search?project={project}&maxResults=100`).
    ///
    /// Used by `tm ticket assign <KEY> <NAME>` to resolve `NAME` against the
    /// issue's project. Only the first 100 assignable users are fetched;
    /// projects with more than that are not paginated through.
    fn assignable_users(&self, project: &str) -> Result<Vec<JiraUser>, JiraError>;

    /// Set (or clear) an issue's assignee (`PUT /issue/{key}/assignee`).
    ///
    /// `account_id: None` clears the assignee (Jira's documented way to
    /// unassign an issue is `{"accountId": null}`, not omitting the field).
    fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), JiraError>;
}

/// Thin response body from `POST /rest/api/3/issue`, which returns only the
/// new issue's identifiers, not its fields.
#[derive(Debug, Clone, serde::Deserialize)]
struct CreatedIssue {
    key: String,
}

/// Envelope wrapping the response body of `GET /rest/api/3/issue/{key}/transitions`.
#[derive(Debug, Clone, serde::Deserialize)]
struct TransitionsEnvelope {
    transitions: Vec<Transition>,
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

    /// Turn a completed response into `Ok(())`, mapping non-2xx status codes
    /// to the appropriate [`JiraError`] variant.
    fn parse_empty(response: reqwest::blocking::Response, key: &str) -> Result<(), JiraError> {
        let status = response.status();
        if status.is_success() {
            return Ok(());
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

    fn get_issue(&self, key: &str) -> Result<Issue, JiraError> {
        let response = self
            .http
            .get(self.url(&format!("/issue/{key}")))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .send()?;
        Self::parse(response, key)
    }

    fn create_issue(&self, req: &CreateIssueRequest) -> Result<Issue, JiraError> {
        let response = self
            .http
            .post(self.url("/issue"))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&req.to_payload())
            .send()?;
        let created: CreatedIssue = Self::parse(response, "")?;
        self.get_issue(&created.key)
    }

    fn add_remote_link(&self, key: &str, link: &RemoteLinkRequest) -> Result<(), JiraError> {
        let response = self
            .http
            .post(self.url(&format!("/issue/{key}/remotelink")))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&link.to_payload())
            .send()?;
        Self::parse_empty(response, key)
    }

    fn transitions(&self, key: &str) -> Result<Vec<Transition>, JiraError> {
        let response = self
            .http
            .get(self.url(&format!("/issue/{key}/transitions")))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .send()?;
        let envelope: TransitionsEnvelope = Self::parse(response, key)?;
        Ok(envelope.transitions)
    }

    fn transition(&self, key: &str, transition_id: &str) -> Result<(), JiraError> {
        let response = self
            .http
            .post(self.url(&format!("/issue/{key}/transitions")))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "transition": { "id": transition_id } }))
            .send()?;
        Self::parse_empty(response, key)
    }

    fn search(&self, jql: &str) -> Result<SearchResult, JiraError> {
        let body = serde_json::json!({
            "jql": jql,
            "fields": ["summary", "status", "assignee", "description"],
            "maxResults": 50,
        });
        let response = self
            .http
            .post(self.url("/search/jql"))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()?;
        Self::parse(response, "")
    }

    fn get_project(&self, key: &str) -> Result<(), JiraError> {
        let response = self
            .http
            .get(self.url(&format!("/project/{key}")))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .send()?;
        Self::parse_empty(response, key)
    }

    fn assignable_users(&self, project: &str) -> Result<Vec<JiraUser>, JiraError> {
        let response = self
            .http
            .get(self.url("/user/assignable/search"))
            .query(&[("project", project), ("maxResults", "100")])
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .send()?;
        // `Self::parse`'s generic 404 handling assumes an issue key (it
        // builds `JiraError::NotFound { key }`), which is the wrong noun and
        // loses the project entirely for this endpoint. Handle 404 here
        // instead, before falling back to the shared path for every other
        // status.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(JiraError::ProjectNotFound {
                project: project.to_string(),
            });
        }
        Self::parse(response, "")
    }

    fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), JiraError> {
        let response = self
            .http
            .put(self.url(&format!("/issue/{key}/assignee")))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "accountId": account_id }))
            .send()?;
        Self::parse_empty(response, key)
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
    fn get_issue_returns_issue() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/issue/PROJ-1")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                );
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "key": "PROJ-1",
                    "fields": {
                        "summary": "Fix the thing",
                        "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                        "description": null,
                        "assignee": null
                    }
                }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let issue = client
            .get_issue("PROJ-1")
            .expect("get_issue should succeed");

        mock.assert();
        assert_eq!(issue.key, "PROJ-1");
        assert_eq!(issue.fields.summary, "Fix the thing");
    }

    #[test]
    fn get_issue_maps_404_to_not_found_with_key() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/issue/PROJ-404");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Issue does not exist"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client.get_issue("PROJ-404").expect_err("should fail");

        match err {
            JiraError::NotFound { key } => assert_eq!(key, "PROJ-404"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn create_issue_posts_payload_then_fetches_created_issue() {
        use crate::jira::types::CreateIssueRequest;
        use serde_json::json;

        let server = MockServer::start();
        let req = CreateIssueRequest {
            project_key: "PROJ".to_string(),
            summary: "Fix the thing".to_string(),
            description: json!({ "type": "doc", "version": 1, "content": [] }),
            issue_type_name: "Task".to_string(),
            assignee_account_id: None,
        };

        let create_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/issue")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                )
                .json_body(req.to_payload());
            then.status(201)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "10001",
                    "key": "PROJ-1",
                    "self": "https://example.atlassian.net/rest/api/3/issue/10001"
                }));
        });
        let get_mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/issue/PROJ-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "key": "PROJ-1",
                    "fields": {
                        "summary": "Fix the thing",
                        "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                        "description": { "type": "doc", "version": 1, "content": [] },
                        "assignee": null
                    }
                }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let issue = client
            .create_issue(&req)
            .expect("create_issue should succeed");

        create_mock.assert();
        get_mock.assert();
        assert_eq!(issue.key, "PROJ-1");
        assert_eq!(issue.fields.summary, "Fix the thing");
    }

    #[test]
    fn add_remote_link_posts_payload() {
        use crate::jira::types::RemoteLinkRequest;

        let server = MockServer::start();
        let link = RemoteLinkRequest {
            url: "https://github.com/example/repo/pull/1".to_string(),
            title: "example/repo#1".to_string(),
        };

        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/issue/PROJ-1/remotelink")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                )
                .json_body(link.to_payload());
            then.status(201)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "id": 1 }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .add_remote_link("PROJ-1", &link)
            .expect("add_remote_link should succeed");

        mock.assert();
    }

    #[test]
    fn add_remote_link_maps_404_to_not_found_with_key() {
        use crate::jira::types::RemoteLinkRequest;

        let server = MockServer::start();
        let link = RemoteLinkRequest {
            url: "https://github.com/example/repo/pull/1".to_string(),
            title: "example/repo#1".to_string(),
        };

        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/issue/PROJ-404/remotelink");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Issue does not exist"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client
            .add_remote_link("PROJ-404", &link)
            .expect_err("should fail");

        match err {
            JiraError::NotFound { key } => assert_eq!(key, "PROJ-404"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn transitions_returns_transition_list() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/issue/PROJ-1/transitions")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                );
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "transitions": [
                        {
                            "id": "11",
                            "name": "Start Progress",
                            "to": { "name": "In Progress", "statusCategory": { "key": "indeterminate" } }
                        }
                    ]
                }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let transitions = client
            .transitions("PROJ-1")
            .expect("transitions should succeed");

        mock.assert();
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].id, "11");
    }

    #[test]
    fn transitions_maps_404_to_not_found_with_key() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/issue/PROJ-404/transitions");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Issue does not exist"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client.transitions("PROJ-404").expect_err("should fail");

        match err {
            JiraError::NotFound { key } => assert_eq!(key, "PROJ-404"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn transition_posts_transition_id() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/issue/PROJ-1/transitions")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                )
                .json_body(serde_json::json!({ "transition": { "id": "31" } }));
            then.status(204);
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .transition("PROJ-1", "31")
            .expect("transition should succeed");

        mock.assert();
    }

    #[test]
    fn search_posts_jql_and_returns_results() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/search/jql")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                )
                .json_body(serde_json::json!({
                    "jql": "assignee = currentUser()",
                    "fields": ["summary", "status", "assignee", "description"],
                    "maxResults": 50
                }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "issues": [
                        {
                            "key": "PROJ-1",
                            "fields": {
                                "summary": "First",
                                "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                                "description": null,
                                "assignee": null
                            }
                        }
                    ]
                }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let result = client
            .search("assignee = currentUser()")
            .expect("search should succeed");

        mock.assert();
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.next_page_token, None);
    }

    #[test]
    fn search_maps_401_to_unauthorized() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/search/jql");
            then.status(401)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Unauthorized"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client
            .search("assignee = currentUser()")
            .expect_err("should fail");

        assert!(matches!(err, JiraError::Unauthorized));
    }

    #[test]
    fn get_project_ok_when_visible() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/project/PROJ")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                );
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "key": "PROJ" }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .get_project("PROJ")
            .expect("get_project should succeed");

        mock.assert();
    }

    #[test]
    fn get_project_maps_404_to_not_found_with_key() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/project/NOPE");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["No project could be found"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client.get_project("NOPE").expect_err("should fail");

        match err {
            JiraError::NotFound { key } => assert_eq!(key, "NOPE"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn assignable_users_returns_users() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/user/assignable/search")
                .query_param("project", "PROJ")
                .query_param("maxResults", "100")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                );
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!([
                    { "accountId": "acct-1", "displayName": "Ada Lovelace" },
                    { "accountId": "acct-2", "displayName": "Grace Hopper" }
                ]));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let users = client
            .assignable_users("PROJ")
            .expect("assignable_users should succeed");

        mock.assert();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].account_id, "acct-1");
        assert_eq!(users[1].display_name, "Grace Hopper");
    }

    #[test]
    fn assignable_users_maps_404_to_project_not_found_with_project_key() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/user/assignable/search")
                .query_param("project", "NOPE");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["No project could be found"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client.assignable_users("NOPE").expect_err("should fail");

        match err {
            JiraError::ProjectNotFound { project } => assert_eq!(project, "NOPE"),
            other => panic!("expected ProjectNotFound, got {other:?}"),
        }
    }

    #[test]
    fn assignable_users_maps_401_to_unauthorized() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/user/assignable/search");
            then.status(401)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Unauthorized"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client.assignable_users("PROJ").expect_err("should fail");

        assert!(matches!(err, JiraError::Unauthorized));
    }

    #[test]
    fn assign_puts_account_id() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/api/3/issue/PROJ-1/assignee")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                )
                .json_body(serde_json::json!({ "accountId": "acct-1" }));
            then.status(204);
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .assign("PROJ-1", Some("acct-1"))
            .expect("assign should succeed");

        mock.assert();
    }

    #[test]
    fn assign_none_puts_null_account_id() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/api/3/issue/PROJ-1/assignee")
                .json_body(serde_json::json!({ "accountId": null }));
            then.status(204);
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .assign("PROJ-1", None)
            .expect("assign should succeed");

        mock.assert();
    }

    #[test]
    fn assign_maps_404_to_not_found_with_key() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/api/3/issue/PROJ-404/assignee");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Issue does not exist"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client
            .assign("PROJ-404", Some("acct-1"))
            .expect_err("should fail");

        match err {
            JiraError::NotFound { key } => assert_eq!(key, "PROJ-404"),
            other => panic!("expected NotFound, got {other:?}"),
        }
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
