//! An in-memory [`JiraClient`] test double.
//!
//! [`FakeJiraClient`] is a plain public struct (not `#[cfg(test)]`-gated) so
//! other test code in the crate — notably the ticketing orchestration tests
//! in `crate::ticketing` — can depend on it directly, the same pattern used
//! by [`crate::github::gh_cli::FakeGhCli`].

use std::cell::RefCell;
use std::collections::HashMap;

use crate::jira::client::{JiraClient, JiraError, RankAnchor};
use crate::jira::types::{
    CreateIssueRequest, Issue, JiraUser, Myself, RemoteLinkRequest, SearchResult, Transition,
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

/// Canned outcome for [`FakeJiraClient::transitions`] on a given key.
enum TransitionsOutcome {
    Found(Vec<Transition>),
    Error { status: u16, message: String },
}

/// Canned outcome for [`FakeJiraClient::assignable_users`] on a given
/// project.
enum AssignableUsersOutcome {
    Found(Vec<JiraUser>),
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
    transitions_result: RefCell<HashMap<String, TransitionsOutcome>>,
    transition_result: RefCell<Result<(), (u16, String)>>,
    transition_calls: RefCell<Vec<(String, String)>>,
    assignable_users_result: RefCell<HashMap<String, AssignableUsersOutcome>>,
    assign_result: RefCell<Result<(), (u16, String)>>,
    assign_calls: RefCell<Vec<(String, Option<String>)>>,
    rank_result: RefCell<Result<(), (u16, String)>>,
    rank_calls: RefCell<Vec<(Vec<String>, RankAnchor)>>,
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
            transitions_result: RefCell::new(HashMap::new()),
            transition_result: RefCell::new(Ok(())),
            transition_calls: RefCell::new(Vec::new()),
            assignable_users_result: RefCell::new(HashMap::new()),
            assign_result: RefCell::new(Ok(())),
            assign_calls: RefCell::new(Vec::new()),
            rank_result: RefCell::new(Ok(())),
            rank_calls: RefCell::new(Vec::new()),
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

    /// Seed `transitions(key)` to return `transitions`.
    pub fn with_transitions(self, key: &str, transitions: Vec<Transition>) -> Self {
        self.transitions_result
            .borrow_mut()
            .insert(key.to_string(), TransitionsOutcome::Found(transitions));
        self
    }

    /// Seed `transitions(key)` to return [`JiraError::Api`] with the given
    /// status and message.
    pub fn with_transitions_error(self, key: &str, status: u16, message: &str) -> Self {
        self.transitions_result.borrow_mut().insert(
            key.to_string(),
            TransitionsOutcome::Error {
                status,
                message: message.to_string(),
            },
        );
        self
    }

    /// Seed `transition(..)` to return [`JiraError::Api`] with the given
    /// status and message.
    pub fn with_transition_error(self, status: u16, message: &str) -> Self {
        *self.transition_result.borrow_mut() = Err((status, message.to_string()));
        self
    }

    /// The `(key, transition_id)` pairs passed to `transition`, in call
    /// order.
    pub fn transition_calls(&self) -> Vec<(String, String)> {
        self.transition_calls.borrow().clone()
    }

    /// Seed `assignable_users(project)` to return `users`.
    pub fn with_assignable_users(self, project: &str, users: Vec<JiraUser>) -> Self {
        self.assignable_users_result
            .borrow_mut()
            .insert(project.to_string(), AssignableUsersOutcome::Found(users));
        self
    }

    /// Seed `assignable_users(project)` to return [`JiraError::Api`] with the
    /// given status and message.
    pub fn with_assignable_users_error(self, project: &str, status: u16, message: &str) -> Self {
        self.assignable_users_result.borrow_mut().insert(
            project.to_string(),
            AssignableUsersOutcome::Error {
                status,
                message: message.to_string(),
            },
        );
        self
    }

    /// Seed `assign(..)` to return [`JiraError::Api`] with the given status
    /// and message.
    pub fn with_assign_error(self, status: u16, message: &str) -> Self {
        *self.assign_result.borrow_mut() = Err((status, message.to_string()));
        self
    }

    /// The `(key, account_id)` pairs passed to `assign`, in call order.
    pub fn assign_calls(&self) -> Vec<(String, Option<String>)> {
        self.assign_calls.borrow().clone()
    }

    /// Seed `rank(..)` to return [`JiraError::Api`] with the given status
    /// and message.
    pub fn with_rank_error(self, status: u16, message: &str) -> Self {
        *self.rank_result.borrow_mut() = Err((status, message.to_string()));
        self
    }

    /// The `(keys, anchor)` pairs passed to `rank`, in call order.
    pub fn rank_calls(&self) -> Vec<(Vec<String>, RankAnchor)> {
        self.rank_calls.borrow().clone()
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

    fn transitions(&self, key: &str) -> Result<Vec<Transition>, JiraError> {
        match self.transitions_result.borrow().get(key) {
            Some(TransitionsOutcome::Found(transitions)) => Ok(transitions.clone()),
            Some(TransitionsOutcome::Error { status, message }) => Err(JiraError::Api {
                status: *status,
                message: message.clone(),
            }),
            None => Ok(Vec::new()),
        }
    }

    fn transition(&self, key: &str, transition_id: &str) -> Result<(), JiraError> {
        self.transition_calls
            .borrow_mut()
            .push((key.to_string(), transition_id.to_string()));
        match self.transition_result.borrow().as_ref() {
            Ok(()) => Ok(()),
            Err((status, message)) => Err(JiraError::Api {
                status: *status,
                message: message.clone(),
            }),
        }
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

    fn assignable_users(&self, project: &str) -> Result<Vec<JiraUser>, JiraError> {
        match self.assignable_users_result.borrow().get(project) {
            Some(AssignableUsersOutcome::Found(users)) => Ok(users.clone()),
            Some(AssignableUsersOutcome::Error { status, message }) => Err(JiraError::Api {
                status: *status,
                message: message.clone(),
            }),
            None => Ok(Vec::new()),
        }
    }

    fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), JiraError> {
        self.assign_calls
            .borrow_mut()
            .push((key.to_string(), account_id.map(str::to_string)));
        match self.assign_result.borrow().as_ref() {
            Ok(()) => Ok(()),
            Err((status, message)) => Err(JiraError::Api {
                status: *status,
                message: message.clone(),
            }),
        }
    }

    fn rank(&self, keys: &[String], anchor: RankAnchor) -> Result<(), JiraError> {
        self.rank_calls.borrow_mut().push((keys.to_vec(), anchor));
        match self.rank_result.borrow().as_ref() {
            Ok(()) => Ok(()),
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
    fn assignable_users_returns_seeded_users() {
        let fake = FakeJiraClient::new().with_assignable_users(
            "PROJ",
            vec![JiraUser {
                account_id: "acct-1".to_string(),
                display_name: "Ada Lovelace".to_string(),
            }],
        );

        let users = fake.assignable_users("PROJ").expect("should succeed");

        assert_eq!(users.len(), 1);
        assert_eq!(users[0].display_name, "Ada Lovelace");
    }

    #[test]
    fn assignable_users_unseeded_project_is_empty() {
        let fake = FakeJiraClient::new();
        let users = fake.assignable_users("PROJ").expect("should succeed");
        assert!(users.is_empty());
    }

    #[test]
    fn assignable_users_seeded_error_is_returned() {
        let fake = FakeJiraClient::new().with_assignable_users_error("PROJ", 500, "boom");
        let err = fake.assignable_users("PROJ").expect_err("should fail");
        match err {
            JiraError::Api { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn assign_records_calls_and_succeeds_by_default() {
        let fake = FakeJiraClient::new();

        fake.assign("PROJ-1", Some("acct-1"))
            .expect("should succeed");
        fake.assign("PROJ-2", None).expect("should succeed");

        assert_eq!(
            fake.assign_calls(),
            vec![
                ("PROJ-1".to_string(), Some("acct-1".to_string())),
                ("PROJ-2".to_string(), None),
            ]
        );
    }

    #[test]
    fn assign_seeded_error_is_returned() {
        let fake = FakeJiraClient::new().with_assign_error(500, "boom");
        let err = fake
            .assign("PROJ-1", Some("acct-1"))
            .expect_err("should fail");
        match err {
            JiraError::Api { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn rank_records_calls_and_succeeds_by_default() {
        let fake = FakeJiraClient::new();

        fake.rank(
            &["PROJ-1".to_string()],
            RankAnchor::Before("PROJ-2".to_string()),
        )
        .expect("should succeed");

        assert_eq!(
            fake.rank_calls(),
            vec![(
                vec!["PROJ-1".to_string()],
                RankAnchor::Before("PROJ-2".to_string())
            )]
        );
    }

    #[test]
    fn rank_seeded_error_is_returned() {
        let fake = FakeJiraClient::new().with_rank_error(500, "boom");
        let err = fake
            .rank(
                &["PROJ-1".to_string()],
                RankAnchor::After("PROJ-2".to_string()),
            )
            .expect_err("should fail");
        match err {
            JiraError::Api { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
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
