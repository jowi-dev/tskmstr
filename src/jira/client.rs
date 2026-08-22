//! HTTP client for the Jira Cloud REST API (v3).
//!
//! [`JiraClient`] is the trait callers depend on; [`HttpJiraClient`] is the
//! `reqwest`-backed implementation used in production. Tests exercise
//! [`HttpJiraClient`] directly against an `httpmock` server.

use crate::jira::types::CreateIssueRequest;
use crate::ticketing::types::{
    CreateLinkRequest, Issue, JiraUser, Myself, RemoteLinkRequest, SearchResult, Transition,
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

    /// [`JiraClient::rank`] got a 404: unlike a single-issue 404, a rank
    /// request names both the issue(s) being moved and an anchor issue, and
    /// the 404 doesn't say which one is missing. Kept distinct from
    /// [`JiraError::NotFound`] so the error text names every key involved
    /// instead of a single one.
    #[error("Jira issue not found while ranking {keys} relative to {anchor}")]
    RankNotFound {
        /// The issue keys that were to be ranked, comma-joined.
        keys: String,
        /// The anchor issue key (from `rankBeforeIssue`/`rankAfterIssue`).
        anchor: String,
    },

    /// [`JiraClient::rank`] got a 207 Multi-Status, meaning at least one
    /// issue in the request could not be re-ranked. Treated as a hard error
    /// since tskmstr only ever ranks one issue at a time and expects
    /// all-or-nothing success.
    #[error("Jira rank request partially failed: {message}")]
    RankPartialFailure {
        /// Detail extracted from the response body, if any.
        message: String,
    },

    /// [`JiraClient::create_link`] got a 404: like [`JiraError::RankNotFound`],
    /// a link request names two issues and Jira's 404 body doesn't reliably
    /// say which one is missing. Kept distinct from [`JiraError::NotFound`]
    /// so the error text names both keys involved instead of a single one.
    #[error("Jira issue not found while linking {blocker} as blocking {blocked}")]
    LinkNotFound {
        /// The issue key that was to block `blocked`.
        blocker: String,
        /// The issue key that was to be blocked by `blocker`.
        blocked: String,
    },

    /// [`JiraClient::delete_link`] got a 404 (HTTP `DELETE
    /// /rest/api/3/issueLink/{linkId}`). The generic [`JiraError::NotFound`]
    /// would report `link_id` using the `key` wording, misreporting a link
    /// id as an issue key — the same reasoning as
    /// [`JiraError::ProjectNotFound`] being kept distinct from `NotFound`.
    #[error("Jira issue link not found: {link_id}")]
    LinkIdNotFound {
        /// The issue-link id that was not found.
        link_id: String,
    },
}

/// Where to rank an issue relative to another, for [`JiraClient::rank`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RankAnchor {
    /// Rank before (above) the given issue key.
    Before(String),
    /// Rank after (below) the given issue key.
    After(String),
}

impl RankAnchor {
    /// The anchor issue key, regardless of direction.
    fn key(&self) -> &str {
        match self {
            RankAnchor::Before(key) | RankAnchor::After(key) => key,
        }
    }
}

/// Issues requested per page by [`JiraClient::search`]. 100 is the largest
/// page `POST /rest/api/3/search/jql` reliably honors; asking for more just
/// gets silently clamped.
pub const SEARCH_PAGE_SIZE: usize = 100;

/// How many pages [`JiraClient::search`] will follow before giving up.
///
/// A bound rather than an unlimited loop: a broad board filter on a large
/// project could otherwise fan out into dozens of round-trips on every
/// refresh. [`SEARCH_PAGE_SIZE`] * this is the effective ceiling on any one
/// search, and the unfollowed `nextPageToken` is returned so callers can tell
/// the results were truncated rather than complete.
pub const MAX_SEARCH_PAGES: usize = 5;

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
    /// Pages are followed automatically via `nextPageToken`, up to
    /// [`MAX_SEARCH_PAGES`] pages of [`SEARCH_PAGE_SIZE`] issues each. The
    /// returned [`SearchResult`] holds every issue collected across those
    /// pages; its `next_page_token` is `Some` only when the page budget ran
    /// out first, i.e. the results are truncated and more matches exist.
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

    /// Move `keys` to a new position in the backlog rank, relative to
    /// `anchor` (`PUT /rest/agile/1.0/issue/rank`).
    ///
    /// This is the Jira Software Agile API, not the platform REST API the
    /// rest of this trait uses — a different base path
    /// (`/rest/agile/1.0`) and host behavior (a 204 on success, and a 207
    /// Multi-Status on partial failure, mapped to
    /// [`JiraError::RankPartialFailure`]). `keys` accepts more than one
    /// issue because that's what the API supports, though tskmstr only ever
    /// ranks one issue at a time today.
    fn rank(&self, keys: &[String], anchor: RankAnchor) -> Result<(), JiraError>;

    /// Create a `Blocks` issue link (`POST /rest/api/3/issueLink`), 201 on
    /// success, such that `req.blocker_key` blocks `req.blocked_key`.
    ///
    /// A 404 means one of the two issues in `req` doesn't exist, but Jira's
    /// error body doesn't reliably say which — mapped to
    /// [`JiraError::LinkNotFound`] naming both keys rather than the generic
    /// [`JiraError::NotFound`], following the precedent of
    /// [`JiraError::RankNotFound`].
    fn create_link(&self, req: &CreateLinkRequest) -> Result<(), JiraError>;

    /// Remove an issue link by its id (`DELETE /rest/api/3/issueLink/{linkId}`).
    ///
    /// Jira documents a 200 on success; some deployments return 204, so both
    /// are accepted. A 404 means `link_id` itself doesn't exist, mapped to
    /// [`JiraError::LinkIdNotFound`] rather than the generic
    /// [`JiraError::NotFound`], which would misreport a link id as an issue
    /// key.
    fn delete_link(&self, link_id: &str) -> Result<(), JiraError>;

    /// Replace an issue's description (`PUT /issue/{key}` with `{"fields":
    /// {"description": description}}`), 204 on success.
    ///
    /// `description` is an ADF document value (see
    /// [`crate::jira::adf::text_to_adf`]), not plain text — Jira Cloud's v3
    /// API only accepts descriptions in ADF. This replaces the whole
    /// description; there is no partial-update form.
    fn update_description(
        &self,
        key: &str,
        description: &serde_json::Value,
    ) -> Result<(), JiraError>;

    /// Post a comment to an issue (`POST /issue/{key}/comment`), 201 on
    /// success.
    ///
    /// `body` is an ADF document value (see
    /// [`crate::jira::adf::text_to_adf`]), not plain text — Jira Cloud's v3
    /// comment endpoint accepts the same ADF shape
    /// [`JiraClient::update_description`] does. This only ever posts a new
    /// comment; there is no edit/replace form.
    fn add_comment(&self, key: &str, body: &serde_json::Value) -> Result<(), JiraError>;
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

    /// Build the full URL for a path under `/rest/agile/1.0/`, the Jira
    /// Software Agile API used by [`JiraClient::rank`]. Distinct from
    /// [`Self::url`] because it lives under a different base path than the
    /// platform REST API (`/rest/api/3`) the rest of this client talks to.
    fn agile_url(&self, path: &str) -> String {
        format!("{}/rest/agile/1.0{path}", self.ctx.base_url)
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
        let mut issues = Vec::new();
        let mut token: Option<String> = None;

        for _ in 0..MAX_SEARCH_PAGES {
            let mut body = serde_json::json!({
                "jql": jql,
                "fields": ["summary", "status", "assignee", "description", "issuelinks"],
                "maxResults": SEARCH_PAGE_SIZE,
            });
            if let Some(token) = &token {
                body["nextPageToken"] = serde_json::Value::String(token.clone());
            }
            let response = self
                .http
                .post(self.url("/search/jql"))
                .basic_auth(&self.ctx.email, Some(&self.ctx.token))
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .json(&body)
                .send()?;
            let page: SearchResult = Self::parse(response, "")?;
            issues.extend(page.issues);
            token = page.next_page_token;
            if token.is_none() {
                break;
            }
        }

        // `token` is `Some` only if we stopped on the page budget rather than
        // running out of pages -- see `MAX_SEARCH_PAGES`.
        Ok(SearchResult {
            issues,
            next_page_token: token,
        })
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

    fn rank(&self, keys: &[String], anchor: RankAnchor) -> Result<(), JiraError> {
        let mut body = serde_json::json!({ "issues": keys });
        match &anchor {
            RankAnchor::Before(key) => {
                body["rankBeforeIssue"] = serde_json::Value::from(key.clone())
            }
            RankAnchor::After(key) => body["rankAfterIssue"] = serde_json::Value::from(key.clone()),
        }

        let response = self
            .http
            .put(self.agile_url("/issue/rank"))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()?;

        let status = response.status();
        // 207 Multi-Status is in the 2xx range (`is_success()` would treat
        // it as Ok), but for this endpoint it means partial failure — some
        // issues in the request couldn't be re-ranked — so it must be
        // checked before the success fast path, not folded into the shared
        // `parse_empty` handling every other endpoint uses.
        if status.as_u16() == 207 {
            let body = response.text().unwrap_or_default();
            return Err(JiraError::RankPartialFailure {
                message: extract_error_message(&body),
            });
        }
        if status.is_success() {
            return Ok(());
        }
        // A 404 doesn't name a single issue key the way every other
        // endpoint's does, so it needs both the moved issue(s) and the
        // anchor to produce a useful error.
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(JiraError::RankNotFound {
                keys: keys.join(", "),
                anchor: anchor.key().to_string(),
            });
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(JiraError::Unauthorized);
        }

        let body = response.text().unwrap_or_default();
        Err(JiraError::Api {
            status: status.as_u16(),
            message: extract_error_message(&body),
        })
    }

    fn create_link(&self, req: &CreateLinkRequest) -> Result<(), JiraError> {
        let response = self
            .http
            .post(self.url("/issueLink"))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&req.to_payload())
            .send()?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        // Like `rank`'s 404 handling, a link request names two issues and
        // Jira's 404 body doesn't say which is missing, so this can't go
        // through `parse_empty`'s single-key `NotFound`.
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(JiraError::LinkNotFound {
                blocker: req.blocker_key.clone(),
                blocked: req.blocked_key.clone(),
            });
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(JiraError::Unauthorized);
        }

        let body = response.text().unwrap_or_default();
        Err(JiraError::Api {
            status: status.as_u16(),
            message: extract_error_message(&body),
        })
    }

    fn delete_link(&self, link_id: &str) -> Result<(), JiraError> {
        let response = self
            .http
            .delete(self.url(&format!("/issueLink/{link_id}")))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .send()?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        // Like `create_link`'s 404 handling, a link id is not an issue key,
        // so this can't go through `parse_empty`'s single-key `NotFound`.
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(JiraError::LinkIdNotFound {
                link_id: link_id.to_string(),
            });
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(JiraError::Unauthorized);
        }

        let body = response.text().unwrap_or_default();
        Err(JiraError::Api {
            status: status.as_u16(),
            message: extract_error_message(&body),
        })
    }

    fn update_description(
        &self,
        key: &str,
        description: &serde_json::Value,
    ) -> Result<(), JiraError> {
        let response = self
            .http
            .put(self.url(&format!("/issue/{key}")))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "fields": { "description": description } }))
            .send()?;
        Self::parse_empty(response, key)
    }

    fn add_comment(&self, key: &str, body: &serde_json::Value) -> Result<(), JiraError> {
        let response = self
            .http
            .post(self.url(&format!("/issue/{key}/comment")))
            .basic_auth(&self.ctx.email, Some(&self.ctx.token))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "body": body }))
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

    /// A minimal `issues[]` entry for search responses, keyed by `key`.
    fn issue_json(key: &str) -> serde_json::Value {
        serde_json::json!({
            "key": key,
            "fields": {
                "summary": key,
                "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                "description": null,
                "assignee": null
            }
        })
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
        use crate::ticketing::types::RemoteLinkRequest;

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
        use crate::ticketing::types::RemoteLinkRequest;

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
                    "fields": ["summary", "status", "assignee", "description", "issuelinks"],
                    "maxResults": SEARCH_PAGE_SIZE
                }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "issues": [issue_json("PROJ-1")] }));
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
    fn search_follows_next_page_token_until_exhausted() {
        let server = MockServer::start();
        let first = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/search/jql")
                .json_body(serde_json::json!({
                    "jql": "project = PROJ",
                    "fields": ["summary", "status", "assignee", "description", "issuelinks"],
                    "maxResults": SEARCH_PAGE_SIZE
                }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "issues": [issue_json("PROJ-1")],
                    "nextPageToken": "page-2"
                }));
        });
        let second = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/search/jql")
                .json_body(serde_json::json!({
                    "jql": "project = PROJ",
                    "fields": ["summary", "status", "assignee", "description", "issuelinks"],
                    "maxResults": SEARCH_PAGE_SIZE,
                    "nextPageToken": "page-2"
                }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "issues": [issue_json("PROJ-2")] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let result = client
            .search("project = PROJ")
            .expect("search should succeed");

        first.assert();
        second.assert();
        let keys: Vec<&str> = result.issues.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["PROJ-1", "PROJ-2"]);
        assert_eq!(result.next_page_token, None);
    }

    #[test]
    fn search_stops_at_max_pages_and_reports_the_unfollowed_token() {
        let server = MockServer::start();
        // Every page claims another page follows, so only MAX_SEARCH_PAGES
        // bounds the loop.
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/search/jql");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "issues": [issue_json("PROJ-1")],
                    "nextPageToken": "more"
                }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let result = client
            .search("project = PROJ")
            .expect("search should succeed");

        assert_eq!(mock.hits(), MAX_SEARCH_PAGES);
        assert_eq!(result.issues.len(), MAX_SEARCH_PAGES);
        assert_eq!(
            result.next_page_token,
            Some("more".to_string()),
            "the token we stopped on is retained so callers can tell results were truncated"
        );
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
    fn rank_puts_before_anchor_body() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/agile/1.0/issue/rank")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                )
                .json_body(serde_json::json!({
                    "issues": ["PROJ-1"],
                    "rankBeforeIssue": "PROJ-2"
                }));
            then.status(204);
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .rank(
                &["PROJ-1".to_string()],
                RankAnchor::Before("PROJ-2".to_string()),
            )
            .expect("rank should succeed");

        mock.assert();
    }

    #[test]
    fn rank_puts_after_anchor_body() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/agile/1.0/issue/rank")
                .json_body(serde_json::json!({
                    "issues": ["PROJ-1"],
                    "rankAfterIssue": "PROJ-2"
                }));
            then.status(204);
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .rank(
                &["PROJ-1".to_string()],
                RankAnchor::After("PROJ-2".to_string()),
            )
            .expect("rank should succeed");

        mock.assert();
    }

    #[test]
    fn rank_maps_401_to_unauthorized() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/agile/1.0/issue/rank");
            then.status(401)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Unauthorized"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client
            .rank(
                &["PROJ-1".to_string()],
                RankAnchor::Before("PROJ-2".to_string()),
            )
            .expect_err("should fail");

        assert!(matches!(err, JiraError::Unauthorized));
    }

    #[test]
    fn rank_maps_404_to_rank_not_found_with_keys_and_anchor() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/agile/1.0/issue/rank");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Issue does not exist"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client
            .rank(
                &["PROJ-1".to_string()],
                RankAnchor::Before("PROJ-404".to_string()),
            )
            .expect_err("should fail");

        match err {
            JiraError::RankNotFound { keys, anchor } => {
                assert_eq!(keys, "PROJ-1");
                assert_eq!(anchor, "PROJ-404");
            }
            other => panic!("expected RankNotFound, got {other:?}"),
        }
    }

    #[test]
    fn rank_maps_207_to_rank_partial_failure() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/agile/1.0/issue/rank");
            then.status(207)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["PROJ-1 could not be ranked"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client
            .rank(
                &["PROJ-1".to_string()],
                RankAnchor::Before("PROJ-2".to_string()),
            )
            .expect_err("should fail");

        match err {
            JiraError::RankPartialFailure { message } => {
                assert!(message.contains("could not be ranked"), "{message}");
            }
            other => panic!("expected RankPartialFailure, got {other:?}"),
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

    #[test]
    fn create_link_posts_blocks_payload() {
        use crate::ticketing::types::CreateLinkRequest;

        let server = MockServer::start();
        let req = CreateLinkRequest {
            blocker_key: "PROJ-1".to_string(),
            blocked_key: "PROJ-2".to_string(),
        };

        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/issueLink")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                )
                .json_body(req.to_payload());
            then.status(201);
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .create_link(&req)
            .expect("create_link should succeed");

        mock.assert();
    }

    #[test]
    fn create_link_maps_404_to_link_not_found_with_both_keys() {
        use crate::ticketing::types::CreateLinkRequest;

        let server = MockServer::start();
        let req = CreateLinkRequest {
            blocker_key: "PROJ-1".to_string(),
            blocked_key: "PROJ-404".to_string(),
        };

        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/issueLink");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Issue does not exist"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client.create_link(&req).expect_err("should fail");

        match err {
            JiraError::LinkNotFound { blocker, blocked } => {
                assert_eq!(blocker, "PROJ-1");
                assert_eq!(blocked, "PROJ-404");
            }
            other => panic!("expected LinkNotFound, got {other:?}"),
        }
    }

    #[test]
    fn create_link_maps_401_to_unauthorized() {
        use crate::ticketing::types::CreateLinkRequest;

        let server = MockServer::start();
        let req = CreateLinkRequest {
            blocker_key: "PROJ-1".to_string(),
            blocked_key: "PROJ-2".to_string(),
        };

        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/issueLink");
            then.status(401)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Unauthorized"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client.create_link(&req).expect_err("should fail");

        assert!(matches!(err, JiraError::Unauthorized));
    }

    #[test]
    fn delete_link_succeeds_on_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::DELETE)
                .path("/rest/api/3/issueLink/10001")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                );
            then.status(200);
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .delete_link("10001")
            .expect("delete_link should succeed");

        mock.assert();
    }

    #[test]
    fn delete_link_succeeds_on_204() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::DELETE)
                .path("/rest/api/3/issueLink/10001");
            then.status(204);
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .delete_link("10001")
            .expect("delete_link should succeed");

        mock.assert();
    }

    #[test]
    fn delete_link_maps_404_to_link_id_not_found() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::DELETE)
                .path("/rest/api/3/issueLink/nope");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Issue link does not exist"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client.delete_link("nope").expect_err("should fail");

        match err {
            JiraError::LinkIdNotFound { link_id } => assert_eq!(link_id, "nope"),
            other => panic!("expected LinkIdNotFound, got {other:?}"),
        }
    }

    #[test]
    fn delete_link_maps_401_to_unauthorized() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::DELETE)
                .path("/rest/api/3/issueLink/10001");
            then.status(401)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Unauthorized"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client.delete_link("10001").expect_err("should fail");

        assert!(matches!(err, JiraError::Unauthorized));
    }

    #[test]
    fn update_description_puts_description() {
        let server = MockServer::start();
        let description = serde_json::json!({ "type": "doc", "version": 1, "content": [] });
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/api/3/issue/PROJ-1")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                )
                .json_body(serde_json::json!({ "fields": { "description": description } }));
            then.status(204);
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .update_description("PROJ-1", &description)
            .expect("update_description should succeed");

        mock.assert();
    }

    #[test]
    fn update_description_maps_404_to_not_found_with_key() {
        let server = MockServer::start();
        let description = serde_json::json!({ "type": "doc", "version": 1, "content": [] });
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/api/3/issue/PROJ-404");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Issue does not exist"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client
            .update_description("PROJ-404", &description)
            .expect_err("should fail");

        match err {
            JiraError::NotFound { key } => assert_eq!(key, "PROJ-404"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn add_comment_posts_body() {
        let server = MockServer::start();
        let body = serde_json::json!({ "type": "doc", "version": 1, "content": [] });
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/issue/PROJ-1/comment")
                .header(
                    "Authorization",
                    "Basic YWRhQGV4YW1wbGUuY29tOnRlc3QtdG9rZW4=",
                )
                .json_body(serde_json::json!({ "body": body }));
            then.status(201);
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        client
            .add_comment("PROJ-1", &body)
            .expect("add_comment should succeed");

        mock.assert();
    }

    #[test]
    fn add_comment_maps_404_to_not_found_with_key() {
        let server = MockServer::start();
        let body = serde_json::json!({ "type": "doc", "version": 1, "content": [] });
        let _mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/issue/PROJ-404/comment");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "errorMessages": ["Issue does not exist"] }));
        });

        let client = HttpJiraClient::new(test_ctx(&server));
        let err = client
            .add_comment("PROJ-404", &body)
            .expect_err("should fail");

        match err {
            JiraError::NotFound { key } => assert_eq!(key, "PROJ-404"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
