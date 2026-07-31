//! An in-memory [`JiraClient`] test double.
//!
//! [`FakeJiraClient`] is a plain public struct (not `#[cfg(test)]`-gated) so
//! other test code in the crate — notably the ticketing orchestration tests
//! in `crate::ticketing` — can depend on it directly, the same pattern used
//! by [`crate::github::gh_cli::FakeGhCli`].

use std::cell::RefCell;
use std::collections::HashMap;

use crate::jira::client::{JiraClient, JiraError};
use crate::jira::types::{
    CreateIssueRequest, Issue, Myself, RemoteLinkRequest, SearchResult, Transition,
};

/// Canned outcome for [`FakeJiraClient::get_issue`] on a given key.
///
/// `JiraError` itself is not `Clone` (it wraps `reqwest::Error`), so outcomes
/// are stored in this smaller enum and turned into a fresh `JiraError` on
/// each call instead.
enum IssueOutcome {
    Found(Issue),
    NotFound,
    Error { status: u16, message: String },
}

/// Canned outcome for [`FakeJiraClient::myself`].
enum MyselfOutcome {
    Found(Myself),
    Unauthorized,
    Error { status: u16, message: String },
}

/// An in-memory [`JiraClient`] test double.
///
/// Issues are seeded by key via [`with_issue`](Self::with_issue),
/// [`with_issue_not_found`](Self::with_issue_not_found), and
/// [`with_issue_error`](Self::with_issue_error). `create_issue` and
/// `add_remote_link` calls are recorded for assertions; every other method
/// returns an inert default, since ticketing flows don't exercise them.
pub struct FakeJiraClient {
    issues: RefCell<HashMap<String, IssueOutcome>>,
    create_issue_result: RefCell<Option<Issue>>,
    create_issue_calls: RefCell<Vec<CreateIssueRequest>>,
    add_remote_link_calls: RefCell<Vec<(String, RemoteLinkRequest)>>,
    myself_result: RefCell<Option<MyselfOutcome>>,
    get_project_result: RefCell<Result<(), (u16, String)>>,
    search_result: RefCell<Option<Result<SearchResult, (u16, String)>>>,
}

impl Default for FakeJiraClient {
    fn default() -> Self {
        Self {
            issues: RefCell::new(HashMap::new()),
            create_issue_result: RefCell::new(None),
            create_issue_calls: RefCell::new(Vec::new()),
            add_remote_link_calls: RefCell::new(Vec::new()),
            myself_result: RefCell::new(None),
            get_project_result: RefCell::new(Ok(())),
            search_result: RefCell::new(None),
        }
    }
}

impl FakeJiraClient {
    /// Create a fake with no seeded issues.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed `get_issue(key)` to return `issue`.
    pub fn with_issue(self, key: &str, issue: Issue) -> Self {
        self.issues
            .borrow_mut()
            .insert(key.to_string(), IssueOutcome::Found(issue));
        self
    }

    /// Seed `get_issue(key)` to return [`JiraError::NotFound`].
    pub fn with_issue_not_found(self, key: &str) -> Self {
        self.issues
            .borrow_mut()
            .insert(key.to_string(), IssueOutcome::NotFound);
        self
    }

    /// Seed `get_issue(key)` to return [`JiraError::Api`] with the given
    /// status and message.
    pub fn with_issue_error(self, key: &str, status: u16, message: &str) -> Self {
        self.issues.borrow_mut().insert(
            key.to_string(),
            IssueOutcome::Error {
                status,
                message: message.to_string(),
            },
        );
        self
    }

    /// Set the [`Issue`] that `create_issue` returns on success.
    pub fn with_create_issue_result(self, issue: Issue) -> Self {
        *self.create_issue_result.borrow_mut() = Some(issue);
        self
    }

    /// The requests passed to `create_issue`, in call order.
    pub fn create_issue_calls(&self) -> Vec<CreateIssueRequest> {
        self.create_issue_calls.borrow().clone()
    }

    /// The `(key, link)` pairs passed to `add_remote_link`, in call order.
    pub fn add_remote_link_calls(&self) -> Vec<(String, RemoteLinkRequest)> {
        self.add_remote_link_calls.borrow().clone()
    }

    /// Seed `myself()` to return `myself`.
    pub fn with_myself(self, myself: Myself) -> Self {
        *self.myself_result.borrow_mut() = Some(MyselfOutcome::Found(myself));
        self
    }

    /// Seed `myself()` to return [`JiraError::Unauthorized`].
    pub fn with_myself_unauthorized(self) -> Self {
        *self.myself_result.borrow_mut() = Some(MyselfOutcome::Unauthorized);
        self
    }

    /// Seed `myself()` to return [`JiraError::Api`] with the given status and
    /// message.
    pub fn with_myself_error(self, status: u16, message: &str) -> Self {
        *self.myself_result.borrow_mut() = Some(MyselfOutcome::Error {
            status,
            message: message.to_string(),
        });
        self
    }

    /// Seed `get_project(key)` to return [`JiraError::NotFound`].
    pub fn with_get_project_not_found(self, key: &str) -> Self {
        *self.get_project_result.borrow_mut() = Err((404, key.to_string()));
        self
    }

    /// Seed `search(..)` to return `result`.
    pub fn with_search_result(self, result: SearchResult) -> Self {
        *self.search_result.borrow_mut() = Some(Ok(result));
        self
    }

    /// Seed `search(..)` to return [`JiraError::Api`] with the given status
    /// and message.
    pub fn with_search_error(self, status: u16, message: &str) -> Self {
        *self.search_result.borrow_mut() = Some(Err((status, message.to_string())));
        self
    }
}

impl JiraClient for FakeJiraClient {
    fn myself(&self) -> Result<Myself, JiraError> {
        match self.myself_result.borrow().as_ref() {
            Some(MyselfOutcome::Found(myself)) => Ok(myself.clone()),
            Some(MyselfOutcome::Unauthorized) => Err(JiraError::Unauthorized),
            Some(MyselfOutcome::Error { status, message }) => Err(JiraError::Api {
                status: *status,
                message: message.clone(),
            }),
            None => Err(JiraError::Api {
                status: 501,
                message: "FakeJiraClient::myself is not implemented".to_string(),
            }),
        }
    }

    fn get_issue(&self, key: &str) -> Result<Issue, JiraError> {
        match self.issues.borrow().get(key) {
            Some(IssueOutcome::Found(issue)) => Ok(issue.clone()),
            Some(IssueOutcome::Error { status, message }) => Err(JiraError::Api {
                status: *status,
                message: message.clone(),
            }),
            Some(IssueOutcome::NotFound) | None => Err(JiraError::NotFound {
                key: key.to_string(),
            }),
        }
    }

    fn create_issue(&self, req: &CreateIssueRequest) -> Result<Issue, JiraError> {
        self.create_issue_calls.borrow_mut().push(req.clone());
        self.create_issue_result
            .borrow()
            .clone()
            .ok_or_else(|| JiraError::Api {
                status: 500,
                message: "FakeJiraClient::create_issue result not configured".to_string(),
            })
    }

    fn add_remote_link(&self, key: &str, link: &RemoteLinkRequest) -> Result<(), JiraError> {
        self.add_remote_link_calls
            .borrow_mut()
            .push((key.to_string(), link.clone()));
        Ok(())
    }

    fn transitions(&self, _key: &str) -> Result<Vec<Transition>, JiraError> {
        Ok(Vec::new())
    }

    fn transition(&self, _key: &str, _transition_id: &str) -> Result<(), JiraError> {
        Ok(())
    }

    fn search(&self, _jql: &str) -> Result<SearchResult, JiraError> {
        match self.search_result.borrow().as_ref() {
            None => Ok(SearchResult {
                issues: Vec::new(),
                next_page_token: None,
            }),
            Some(Ok(result)) => Ok(result.clone()),
            Some(Err((status, message))) => Err(JiraError::Api {
                status: *status,
                message: message.clone(),
            }),
        }
    }

    fn get_project(&self, key: &str) -> Result<(), JiraError> {
        match self.get_project_result.borrow().as_ref() {
            Ok(()) => Ok(()),
            Err((404, _)) => Err(JiraError::NotFound {
                key: key.to_string(),
            }),
            Err((status, message)) => Err(JiraError::Api {
                status: *status,
                message: message.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(key: &str) -> Issue {
        use crate::jira::types::{IssueFields, Status, StatusCategory};

        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: "Fix the thing".to_string(),
                status: Status {
                    name: "To Do".to_string(),
                    status_category: StatusCategory {
                        key: "new".to_string(),
                    },
                },
                description: None,
                assignee: None,
            },
        }
    }

    #[test]
    fn get_issue_returns_seeded_issue() {
        let fake = FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1"));
        let result = fake.get_issue("PROJ-1").expect("issue should be found");
        assert_eq!(result.key, "PROJ-1");
    }

    #[test]
    fn get_issue_unseeded_key_is_not_found() {
        let fake = FakeJiraClient::new();
        let err = fake.get_issue("PROJ-1").expect_err("should fail");
        assert!(matches!(err, JiraError::NotFound { key } if key == "PROJ-1"));
    }

    #[test]
    fn get_issue_seeded_error_is_returned() {
        let fake = FakeJiraClient::new().with_issue_error("PROJ-1", 500, "boom");
        let err = fake.get_issue("PROJ-1").expect_err("should fail");
        match err {
            JiraError::Api { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn create_issue_records_calls_and_returns_configured_result() {
        use serde_json::json;

        let fake = FakeJiraClient::new().with_create_issue_result(issue("PROJ-2"));
        let req = CreateIssueRequest {
            project_key: "PROJ".to_string(),
            summary: "Fix the thing".to_string(),
            description: json!({ "type": "doc", "version": 1, "content": [] }),
            issue_type_name: "Task".to_string(),
            assignee_account_id: None,
        };

        let created = fake.create_issue(&req).expect("should succeed");

        assert_eq!(created.key, "PROJ-2");
        assert_eq!(fake.create_issue_calls(), vec![req]);
    }

    #[test]
    fn myself_returns_seeded_value() {
        let fake = FakeJiraClient::new().with_myself(Myself {
            account_id: "acct-1".to_string(),
            display_name: "Ada Lovelace".to_string(),
            email_address: Some("ada@example.com".to_string()),
        });
        let myself = fake.myself().expect("should succeed");
        assert_eq!(myself.account_id, "acct-1");
    }

    #[test]
    fn myself_unseeded_is_unauthorized_by_default_stub() {
        let fake = FakeJiraClient::new();
        let err = fake.myself().expect_err("should fail");
        match err {
            JiraError::Api { status, .. } => assert_eq!(status, 501),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn myself_seeded_unauthorized() {
        let fake = FakeJiraClient::new().with_myself_unauthorized();
        let err = fake.myself().expect_err("should fail");
        assert!(matches!(err, JiraError::Unauthorized));
    }

    #[test]
    fn get_project_ok_by_default() {
        let fake = FakeJiraClient::new();
        fake.get_project("PROJ").expect("should succeed");
    }

    #[test]
    fn get_project_seeded_not_found() {
        let fake = FakeJiraClient::new().with_get_project_not_found("PROJ");
        let err = fake.get_project("PROJ").expect_err("should fail");
        assert!(matches!(err, JiraError::NotFound { key } if key == "PROJ"));
    }

    #[test]
    fn add_remote_link_records_calls() {
        let fake = FakeJiraClient::new();
        let link = RemoteLinkRequest {
            url: "https://github.com/example/repo/pull/1".to_string(),
            title: "example/repo#1".to_string(),
        };

        fake.add_remote_link("PROJ-1", &link)
            .expect("should succeed");

        assert_eq!(
            fake.add_remote_link_calls(),
            vec![("PROJ-1".to_string(), link)]
        );
    }
}
