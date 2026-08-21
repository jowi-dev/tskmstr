//! Backend-agnostic ticket provider trait, and the Jira-backed
//! implementation of it.
//!
//! [`TicketProvider`] is the interface every ticketing orchestration
//! function in [`crate::ticketing`], every `tm ticket`/`tm ready`/`tm pr`
//! command, and the board TUI depend on. It carries the same fourteen
//! operations [`JiraClient`] exposes, unchanged in shape — a ticket key is
//! still a plain `&str`, a description is still an ADF `serde_json::Value`,
//! and every error is still a [`JiraError`]. A non-Jira backend (a future
//! `GithubProvider`) implements this trait directly rather than going
//! through [`JiraClient`] at all; the Jira-specific vocabulary baked into
//! this phase's method signatures (JQL strings, ADF values) is exactly what
//! a later phase abstracts further, not something this trait tries to hide
//! today.
//!
//! [`JiraProvider`] is a thin wrapper around a boxed [`JiraClient`] that
//! forwards every call unchanged; it's how production code (`main.rs`, the
//! board TUI's real dependencies) turns a live
//! [`crate::jira::client::HttpJiraClient`] into a `Box<dyn TicketProvider>`.
//! [`crate::jira::fake::FakeJiraClient`] implements [`TicketProvider`]
//! directly (see the `impl` below) rather than going through
//! [`JiraProvider`] — it's a plain, non-owning delegation to the
//! [`JiraClient`] impl it already has, so every existing test that
//! constructs a `FakeJiraClient` and later inspects its recorded calls
//! (`fake.transition_calls()`, `fake.add_comment_calls()`, etc.) keeps doing
//! so by reference, with no ownership transfer and no test rewritten to
//! route through a wrapper value it can no longer see into.

use crate::jira::client::{JiraClient, JiraError, RankAnchor};
use crate::jira::fake::FakeJiraClient;
use crate::jira::types::{
    CreateIssueRequest, CreateLinkRequest, Issue, JiraUser, Myself, RemoteLinkRequest,
    SearchResult, Transition,
};

/// Backend-agnostic ticket operations. See the module doc comment for how
/// this relates to [`JiraClient`] and [`JiraProvider`].
pub trait TicketProvider {
    /// Fetch the authenticated user. Used to verify auth is configured
    /// correctly.
    fn myself(&self) -> Result<Myself, JiraError>;

    /// Fetch a single ticket by key.
    fn get_issue(&self, key: &str) -> Result<Issue, JiraError>;

    /// Create a new ticket.
    fn create_issue(&self, req: &CreateIssueRequest) -> Result<Issue, JiraError>;

    /// Attach a remote link (e.g. a GitHub PR) to a ticket.
    fn add_remote_link(&self, key: &str, link: &RemoteLinkRequest) -> Result<(), JiraError>;

    /// List the workflow transitions available on a ticket.
    fn transitions(&self, key: &str) -> Result<Vec<Transition>, JiraError>;

    /// Apply a workflow transition to a ticket.
    fn transition(&self, key: &str, transition_id: &str) -> Result<(), JiraError>;

    /// Run a search query (a JQL string against Jira today).
    fn search(&self, jql: &str) -> Result<SearchResult, JiraError>;

    /// Check that a project exists and is visible to the authenticated
    /// user.
    fn get_project(&self, key: &str) -> Result<(), JiraError>;

    /// List users eligible to be assigned a ticket in `project`.
    fn assignable_users(&self, project: &str) -> Result<Vec<JiraUser>, JiraError>;

    /// Set (or clear) a ticket's assignee.
    fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), JiraError>;

    /// Move `keys` to a new position in the backlog rank, relative to
    /// `anchor`.
    fn rank(&self, keys: &[String], anchor: RankAnchor) -> Result<(), JiraError>;

    /// Create a `Blocks` issue link such that `req.blocker_key` blocks
    /// `req.blocked_key`.
    fn create_link(&self, req: &CreateLinkRequest) -> Result<(), JiraError>;

    /// Remove an issue link by its id.
    fn delete_link(&self, link_id: &str) -> Result<(), JiraError>;

    /// Replace a ticket's description (an ADF document value).
    fn update_description(
        &self,
        key: &str,
        description: &serde_json::Value,
    ) -> Result<(), JiraError>;

    /// Post a comment to a ticket (an ADF document value).
    fn add_comment(&self, key: &str, body: &serde_json::Value) -> Result<(), JiraError>;
}

/// [`TicketProvider`] backed by a boxed [`JiraClient`].
///
/// Every method forwards unchanged to the wrapped client — this phase is a
/// pure retyping of every ticketing call site from `&dyn JiraClient` to
/// `&dyn TicketProvider`, with no behavior change.
pub struct JiraProvider(Box<dyn JiraClient>);

impl JiraProvider {
    /// Wrap any Jira client — live or fake — as a [`TicketProvider`].
    pub fn new(client: impl JiraClient + 'static) -> Self {
        Self(Box::new(client))
    }
}

impl TicketProvider for JiraProvider {
    fn myself(&self) -> Result<Myself, JiraError> {
        self.0.myself()
    }

    fn get_issue(&self, key: &str) -> Result<Issue, JiraError> {
        self.0.get_issue(key)
    }

    fn create_issue(&self, req: &CreateIssueRequest) -> Result<Issue, JiraError> {
        self.0.create_issue(req)
    }

    fn add_remote_link(&self, key: &str, link: &RemoteLinkRequest) -> Result<(), JiraError> {
        self.0.add_remote_link(key, link)
    }

    fn transitions(&self, key: &str) -> Result<Vec<Transition>, JiraError> {
        self.0.transitions(key)
    }

    fn transition(&self, key: &str, transition_id: &str) -> Result<(), JiraError> {
        self.0.transition(key, transition_id)
    }

    fn search(&self, jql: &str) -> Result<SearchResult, JiraError> {
        self.0.search(jql)
    }

    fn get_project(&self, key: &str) -> Result<(), JiraError> {
        self.0.get_project(key)
    }

    fn assignable_users(&self, project: &str) -> Result<Vec<JiraUser>, JiraError> {
        self.0.assignable_users(project)
    }

    fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), JiraError> {
        self.0.assign(key, account_id)
    }

    fn rank(&self, keys: &[String], anchor: RankAnchor) -> Result<(), JiraError> {
        self.0.rank(keys, anchor)
    }

    fn create_link(&self, req: &CreateLinkRequest) -> Result<(), JiraError> {
        self.0.create_link(req)
    }

    fn delete_link(&self, link_id: &str) -> Result<(), JiraError> {
        self.0.delete_link(link_id)
    }

    fn update_description(
        &self,
        key: &str,
        description: &serde_json::Value,
    ) -> Result<(), JiraError> {
        self.0.update_description(key, description)
    }

    fn add_comment(&self, key: &str, body: &serde_json::Value) -> Result<(), JiraError> {
        self.0.add_comment(key, body)
    }
}

/// [`FakeJiraClient`] satisfies [`TicketProvider`] directly, delegating to
/// the [`JiraClient`] impl it already has. See the module doc comment for
/// why this bypasses [`JiraProvider`]: wrapping would move the fake into a
/// `Box`, and the great majority of tests using it construct a
/// `FakeJiraClient`, pass a reference into a context struct, and then
/// inspect its recorded calls by that same reference afterward.
impl TicketProvider for FakeJiraClient {
    fn myself(&self) -> Result<Myself, JiraError> {
        JiraClient::myself(self)
    }

    fn get_issue(&self, key: &str) -> Result<Issue, JiraError> {
        JiraClient::get_issue(self, key)
    }

    fn create_issue(&self, req: &CreateIssueRequest) -> Result<Issue, JiraError> {
        JiraClient::create_issue(self, req)
    }

    fn add_remote_link(&self, key: &str, link: &RemoteLinkRequest) -> Result<(), JiraError> {
        JiraClient::add_remote_link(self, key, link)
    }

    fn transitions(&self, key: &str) -> Result<Vec<Transition>, JiraError> {
        JiraClient::transitions(self, key)
    }

    fn transition(&self, key: &str, transition_id: &str) -> Result<(), JiraError> {
        JiraClient::transition(self, key, transition_id)
    }

    fn search(&self, jql: &str) -> Result<SearchResult, JiraError> {
        JiraClient::search(self, jql)
    }

    fn get_project(&self, key: &str) -> Result<(), JiraError> {
        JiraClient::get_project(self, key)
    }

    fn assignable_users(&self, project: &str) -> Result<Vec<JiraUser>, JiraError> {
        JiraClient::assignable_users(self, project)
    }

    fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), JiraError> {
        JiraClient::assign(self, key, account_id)
    }

    fn rank(&self, keys: &[String], anchor: RankAnchor) -> Result<(), JiraError> {
        JiraClient::rank(self, keys, anchor)
    }

    fn create_link(&self, req: &CreateLinkRequest) -> Result<(), JiraError> {
        JiraClient::create_link(self, req)
    }

    fn delete_link(&self, link_id: &str) -> Result<(), JiraError> {
        JiraClient::delete_link(self, link_id)
    }

    fn update_description(
        &self,
        key: &str,
        description: &serde_json::Value,
    ) -> Result<(), JiraError> {
        JiraClient::update_description(self, key, description)
    }

    fn add_comment(&self, key: &str, body: &serde_json::Value) -> Result<(), JiraError> {
        JiraClient::add_comment(self, key, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jira::types::{IssueFields, Status, StatusCategory};

    fn issue(key: &str) -> Issue {
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
                issue_links: vec![],
            },
        }
    }

    #[test]
    fn jira_provider_forwards_get_issue_to_the_wrapped_client() {
        let provider =
            JiraProvider::new(FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1")));

        let result = provider.get_issue("PROJ-1").expect("issue should be found");

        assert_eq!(result.key, "PROJ-1");
    }

    #[test]
    fn jira_provider_forwards_errors_unchanged() {
        let provider = JiraProvider::new(FakeJiraClient::new().with_issue_not_found("PROJ-1"));

        let err = provider.get_issue("PROJ-1").expect_err("should fail");

        assert!(matches!(err, JiraError::NotFound { key } if key == "PROJ-1"));
    }

    #[test]
    fn jira_provider_is_usable_as_a_trait_object() {
        let provider: Box<dyn TicketProvider> = Box::new(JiraProvider::new(FakeJiraClient::new()));

        provider
            .transition("PROJ-1", "31")
            .expect("FakeJiraClient::transition succeeds by default");
    }
}
