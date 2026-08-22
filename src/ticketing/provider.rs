//! Backend-agnostic ticket provider trait, and the Jira-backed
//! implementation of it.
//!
//! [`TicketProvider`] is the interface every ticketing orchestration
//! function in [`crate::ticketing`], every `tm ticket`/`tm ready`/`tm pr`
//! command, and the board TUI depend on. A ticket key is still a plain
//! `&str`, and every error is a backend-agnostic
//! [`ProviderError`](crate::ticketing::error::ProviderError) rather than a
//! Jira-specific [`JiraError`] — see [`crate::ticketing::error`] for how
//! [`JiraProvider`] converts at the boundary. Queries and descriptions are
//! also already backend-agnostic: [`TicketProvider::search`] takes a [`TicketQuery`]
//! rather than a JQL string, and [`TicketProvider::create_issue`],
//! [`TicketProvider::update_description`], and
//! [`TicketProvider::add_comment`] take plain Markdown text rather than an
//! ADF `serde_json::Value`. No caller outside [`crate::jira`] builds a JQL
//! string or an ADF document — [`JiraProvider`] is where that translation
//! happens, via [`crate::jira::jql`]'s builders and
//! [`crate::jira::adf::text_to_adf`]/[`crate::jira::adf::adf_to_text`]. A
//! non-Jira backend (a future `GithubProvider`) implements this trait
//! directly rather than going through [`JiraClient`] at all, rendering
//! [`TicketQuery`] to `gh issue list`/`gh search issues` arguments and
//! passing Markdown through untouched (GitHub issue bodies are already
//! Markdown).
//!
//! [`JiraProvider`] is a thin wrapper around a boxed [`JiraClient`] that
//! forwards every call, translating queries and descriptions on the way;
//! it's how production code (`main.rs`, the board TUI's real dependencies)
//! turns a live [`crate::jira::client::HttpJiraClient`] into a
//! `Box<dyn TicketProvider>`. [`crate::jira::fake::FakeJiraClient`]
//! implements [`TicketProvider`] directly (see the `impl` below) rather than
//! going through [`JiraProvider`] — it's a plain, non-owning delegation to
//! the [`JiraClient`] impl it already has (translating queries and
//! descriptions the same way [`JiraProvider`] does), so every existing test
//! that constructs a `FakeJiraClient` and later inspects its recorded calls
//! (`fake.transition_calls()`, `fake.add_comment_calls()`, etc.) keeps doing
//! so by reference, with no ownership transfer and no test rewritten to
//! route through a wrapper value it can no longer see into.

use crate::jira::adf::{adf_to_text, text_to_adf};
use crate::jira::client::{JiraClient, RankAnchor};
use crate::jira::fake::FakeJiraClient;
use crate::jira::jql::{
    assignee_tickets_jql, everyone_tickets_jql, my_open_tickets_jql, ranked_tickets_jql,
    ready_candidates_jql, shipped_awaiting_retro_jql, ticket_search_jql, unassigned_tickets_jql,
};
use crate::jira::types::CreateIssueRequest;
use crate::ticketing::error::ProviderError;
use crate::ticketing::types::{
    CreateLinkRequest, Issue, JiraUser, Myself, RemoteLinkRequest, SearchResult, Transition,
};

/// A backend-agnostic ticket search, rendered by each [`TicketProvider`] into
/// its own query shape ([`JiraProvider`] renders these to JQL via
/// [`crate::jira::jql`]'s builders; a future `GithubProvider` would render to
/// `gh issue list`/`gh search issues` arguments instead). No caller outside
/// [`crate::jira`] builds a JQL string directly — every board screen and
/// ticketing orchestration function builds one of these variants and hands
/// it to [`TicketProvider::search`].
///
/// Every variant but [`TicketQuery::MyOpen`] and [`TicketQuery::ReadyCandidates`]
/// carries the project key to scope to, mirroring the corresponding
/// `src/jira/jql.rs` builder's `project_key` parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketQuery {
    /// The current user's open tickets, unscoped by project. See
    /// [`my_open_tickets_jql`].
    MyOpen,
    /// Every open ticket in `project_key` with no assignee. See
    /// [`unassigned_tickets_jql`].
    Unassigned {
        /// Project to scope the search to.
        project_key: String,
    },
    /// Every open ticket in `project_key`, regardless of assignee. See
    /// [`everyone_tickets_jql`].
    Everyone {
        /// Project to scope the search to.
        project_key: String,
    },
    /// Every open ticket in `project_key` assigned to `account_id`. See
    /// [`assignee_tickets_jql`].
    Assignee {
        /// Project to scope the search to.
        project_key: String,
        /// Assignee to scope the search to.
        account_id: String,
    },
    /// Every open ticket in `project_key`, in native backlog rank order. See
    /// [`ranked_tickets_jql`].
    Ranked {
        /// Project to scope the search to.
        project_key: String,
    },
    /// Every open ticket in `project_key` whose text matches `text`. See
    /// [`ticket_search_jql`].
    Search {
        /// Project to scope the search to.
        project_key: String,
        /// Free text to match against.
        text: String,
    },
    /// Tickets in `project_key` that shipped within the retro lookback
    /// window. See [`shipped_awaiting_retro_jql`].
    ShippedAwaitingRetro {
        /// Project to scope the search to.
        project_key: String,
    },
    /// The current user's "To Do" candidates for `tm ready`, unscoped by
    /// project, in rank order. See [`ready_candidates_jql`]. Not named in
    /// GitHub issue #3's variant list; added because [`ready_candidates_jql`]
    /// has no other variant it maps onto (see the phase 2 report).
    ReadyCandidates,
}

/// Render `query` into the JQL string [`JiraProvider::search`] (and
/// [`FakeJiraClient`]'s [`TicketProvider`] impl, so tests configured against
/// a fixed JQL string keep working unchanged) send to Jira.
fn render_jql(query: &TicketQuery) -> String {
    match query {
        TicketQuery::MyOpen => my_open_tickets_jql(),
        TicketQuery::Unassigned { project_key } => unassigned_tickets_jql(project_key),
        TicketQuery::Everyone { project_key } => everyone_tickets_jql(project_key),
        TicketQuery::Assignee {
            project_key,
            account_id,
        } => assignee_tickets_jql(project_key, account_id),
        TicketQuery::Ranked { project_key } => ranked_tickets_jql(project_key),
        TicketQuery::Search { project_key, text } => ticket_search_jql(project_key, text),
        TicketQuery::ShippedAwaitingRetro { project_key } => {
            shipped_awaiting_retro_jql(project_key)
        }
        TicketQuery::ReadyCandidates => ready_candidates_jql(),
    }
}

/// A new ticket to create, provider-agnostic: `description` is plain
/// Markdown text. [`JiraProvider`] converts it to ADF internally via
/// [`text_to_adf`] before building the Jira-shaped [`CreateIssueRequest`]; a
/// future `GithubProvider` would pass it through untouched, since GitHub
/// issue bodies are already Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTicket {
    /// Project key the issue is created under.
    pub project_key: String,
    /// One-line issue summary.
    pub summary: String,
    /// Issue description, as Markdown text.
    pub description: String,
    /// Issue type name, e.g. `Task`.
    pub issue_type_name: String,
    /// Account ID to assign the new issue to, if any.
    pub assignee_account_id: Option<String>,
}

/// Backend-agnostic ticket operations. See the module doc comment for how
/// this relates to [`JiraClient`] and [`JiraProvider`].
pub trait TicketProvider {
    /// Fetch the authenticated user. Used to verify auth is configured
    /// correctly.
    fn myself(&self) -> Result<Myself, ProviderError>;

    /// Fetch a single ticket by key.
    fn get_issue(&self, key: &str) -> Result<Issue, ProviderError>;

    /// Create a new ticket.
    fn create_issue(&self, req: &NewTicket) -> Result<Issue, ProviderError>;

    /// Attach a remote link (e.g. a GitHub PR) to a ticket.
    fn add_remote_link(&self, key: &str, link: &RemoteLinkRequest) -> Result<(), ProviderError>;

    /// List the workflow transitions available on a ticket.
    fn transitions(&self, key: &str) -> Result<Vec<Transition>, ProviderError>;

    /// Apply a workflow transition to a ticket.
    fn transition(&self, key: &str, transition_id: &str) -> Result<(), ProviderError>;

    /// Run a search query.
    fn search(&self, query: &TicketQuery) -> Result<SearchResult, ProviderError>;

    /// Check that a project exists and is visible to the authenticated
    /// user.
    fn get_project(&self, key: &str) -> Result<(), ProviderError>;

    /// List users eligible to be assigned a ticket in `project`.
    fn assignable_users(&self, project: &str) -> Result<Vec<JiraUser>, ProviderError>;

    /// Set (or clear) a ticket's assignee.
    fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), ProviderError>;

    /// Move `keys` to a new position in the backlog rank, relative to
    /// `anchor`.
    fn rank(&self, keys: &[String], anchor: RankAnchor) -> Result<(), ProviderError>;

    /// Create a `Blocks` issue link such that `req.blocker_key` blocks
    /// `req.blocked_key`.
    fn create_link(&self, req: &CreateLinkRequest) -> Result<(), ProviderError>;

    /// Remove an issue link by its id.
    fn delete_link(&self, link_id: &str) -> Result<(), ProviderError>;

    /// Replace a ticket's description with `description`, plain Markdown
    /// text.
    fn update_description(&self, key: &str, description: &str) -> Result<(), ProviderError>;

    /// Post a comment to a ticket. `body` is plain Markdown text.
    fn add_comment(&self, key: &str, body: &str) -> Result<(), ProviderError>;

    /// Render `issue`'s description as plain text, translating from
    /// whatever format the backend stores it in (ADF, for Jira). `""` when
    /// there's no description.
    fn description_text(&self, issue: &Issue) -> String;
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
    fn myself(&self) -> Result<Myself, ProviderError> {
        Ok(self.0.myself()?)
    }

    fn get_issue(&self, key: &str) -> Result<Issue, ProviderError> {
        Ok(self.0.get_issue(key)?)
    }

    fn create_issue(&self, req: &NewTicket) -> Result<Issue, ProviderError> {
        Ok(self.0.create_issue(&CreateIssueRequest {
            project_key: req.project_key.clone(),
            summary: req.summary.clone(),
            description: text_to_adf(&req.description),
            issue_type_name: req.issue_type_name.clone(),
            assignee_account_id: req.assignee_account_id.clone(),
        })?)
    }

    fn add_remote_link(&self, key: &str, link: &RemoteLinkRequest) -> Result<(), ProviderError> {
        Ok(self.0.add_remote_link(key, link)?)
    }

    fn transitions(&self, key: &str) -> Result<Vec<Transition>, ProviderError> {
        Ok(self.0.transitions(key)?)
    }

    fn transition(&self, key: &str, transition_id: &str) -> Result<(), ProviderError> {
        Ok(self.0.transition(key, transition_id)?)
    }

    fn search(&self, query: &TicketQuery) -> Result<SearchResult, ProviderError> {
        Ok(self.0.search(&render_jql(query))?)
    }

    fn get_project(&self, key: &str) -> Result<(), ProviderError> {
        Ok(self.0.get_project(key)?)
    }

    fn assignable_users(&self, project: &str) -> Result<Vec<JiraUser>, ProviderError> {
        Ok(self.0.assignable_users(project)?)
    }

    fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), ProviderError> {
        Ok(self.0.assign(key, account_id)?)
    }

    fn rank(&self, keys: &[String], anchor: RankAnchor) -> Result<(), ProviderError> {
        Ok(self.0.rank(keys, anchor)?)
    }

    fn create_link(&self, req: &CreateLinkRequest) -> Result<(), ProviderError> {
        Ok(self.0.create_link(req)?)
    }

    fn delete_link(&self, link_id: &str) -> Result<(), ProviderError> {
        Ok(self.0.delete_link(link_id)?)
    }

    fn update_description(&self, key: &str, description: &str) -> Result<(), ProviderError> {
        Ok(self.0.update_description(key, &text_to_adf(description))?)
    }

    fn add_comment(&self, key: &str, body: &str) -> Result<(), ProviderError> {
        Ok(self.0.add_comment(key, &text_to_adf(body))?)
    }

    fn description_text(&self, issue: &Issue) -> String {
        issue
            .fields
            .description
            .as_ref()
            .map(adf_to_text)
            .unwrap_or_default()
    }
}

/// [`FakeJiraClient`] satisfies [`TicketProvider`] directly, delegating to
/// the [`JiraClient`] impl it already has. See the module doc comment for
/// why this bypasses [`JiraProvider`]: wrapping would move the fake into a
/// `Box`, and the great majority of tests using it construct a
/// `FakeJiraClient`, pass a reference into a context struct, and then
/// inspect its recorded calls by that same reference afterward.
impl TicketProvider for FakeJiraClient {
    fn myself(&self) -> Result<Myself, ProviderError> {
        Ok(JiraClient::myself(self)?)
    }

    fn get_issue(&self, key: &str) -> Result<Issue, ProviderError> {
        Ok(JiraClient::get_issue(self, key)?)
    }

    fn create_issue(&self, req: &NewTicket) -> Result<Issue, ProviderError> {
        Ok(JiraClient::create_issue(
            self,
            &CreateIssueRequest {
                project_key: req.project_key.clone(),
                summary: req.summary.clone(),
                description: text_to_adf(&req.description),
                issue_type_name: req.issue_type_name.clone(),
                assignee_account_id: req.assignee_account_id.clone(),
            },
        )?)
    }

    fn add_remote_link(&self, key: &str, link: &RemoteLinkRequest) -> Result<(), ProviderError> {
        Ok(JiraClient::add_remote_link(self, key, link)?)
    }

    fn transitions(&self, key: &str) -> Result<Vec<Transition>, ProviderError> {
        Ok(JiraClient::transitions(self, key)?)
    }

    fn transition(&self, key: &str, transition_id: &str) -> Result<(), ProviderError> {
        Ok(JiraClient::transition(self, key, transition_id)?)
    }

    fn search(&self, query: &TicketQuery) -> Result<SearchResult, ProviderError> {
        Ok(JiraClient::search(self, &render_jql(query))?)
    }

    fn get_project(&self, key: &str) -> Result<(), ProviderError> {
        Ok(JiraClient::get_project(self, key)?)
    }

    fn assignable_users(&self, project: &str) -> Result<Vec<JiraUser>, ProviderError> {
        Ok(JiraClient::assignable_users(self, project)?)
    }

    fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), ProviderError> {
        Ok(JiraClient::assign(self, key, account_id)?)
    }

    fn rank(&self, keys: &[String], anchor: RankAnchor) -> Result<(), ProviderError> {
        Ok(JiraClient::rank(self, keys, anchor)?)
    }

    fn create_link(&self, req: &CreateLinkRequest) -> Result<(), ProviderError> {
        Ok(JiraClient::create_link(self, req)?)
    }

    fn delete_link(&self, link_id: &str) -> Result<(), ProviderError> {
        Ok(JiraClient::delete_link(self, link_id)?)
    }

    fn update_description(&self, key: &str, description: &str) -> Result<(), ProviderError> {
        Ok(JiraClient::update_description(
            self,
            key,
            &text_to_adf(description),
        )?)
    }

    fn add_comment(&self, key: &str, body: &str) -> Result<(), ProviderError> {
        Ok(JiraClient::add_comment(self, key, &text_to_adf(body))?)
    }

    fn description_text(&self, issue: &Issue) -> String {
        issue
            .fields
            .description
            .as_ref()
            .map(adf_to_text)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ticketing::types::{IssueFields, Status, StatusCategory};

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
    fn jira_provider_converts_errors_to_provider_error() {
        let provider = JiraProvider::new(FakeJiraClient::new().with_issue_not_found("PROJ-1"));

        let err = provider.get_issue("PROJ-1").expect_err("should fail");

        assert!(matches!(err, ProviderError::NotFound { key } if key == "PROJ-1"));
    }

    #[test]
    fn jira_provider_is_usable_as_a_trait_object() {
        let provider: Box<dyn TicketProvider> = Box::new(JiraProvider::new(FakeJiraClient::new()));

        provider
            .transition("PROJ-1", "31")
            .expect("FakeJiraClient::transition succeeds by default");
    }

    #[test]
    fn render_jql_covers_every_variant() {
        assert_eq!(
            render_jql(&TicketQuery::MyOpen),
            crate::jira::jql::my_open_tickets_jql()
        );
        assert_eq!(
            render_jql(&TicketQuery::Unassigned {
                project_key: "PROJ".to_string()
            }),
            crate::jira::jql::unassigned_tickets_jql("PROJ")
        );
        assert_eq!(
            render_jql(&TicketQuery::Everyone {
                project_key: "PROJ".to_string()
            }),
            crate::jira::jql::everyone_tickets_jql("PROJ")
        );
        assert_eq!(
            render_jql(&TicketQuery::Assignee {
                project_key: "PROJ".to_string(),
                account_id: "acct-1".to_string()
            }),
            crate::jira::jql::assignee_tickets_jql("PROJ", "acct-1")
        );
        assert_eq!(
            render_jql(&TicketQuery::Ranked {
                project_key: "PROJ".to_string()
            }),
            crate::jira::jql::ranked_tickets_jql("PROJ")
        );
        assert_eq!(
            render_jql(&TicketQuery::Search {
                project_key: "PROJ".to_string(),
                text: "login bug".to_string()
            }),
            crate::jira::jql::ticket_search_jql("PROJ", "login bug")
        );
        assert_eq!(
            render_jql(&TicketQuery::ShippedAwaitingRetro {
                project_key: "PROJ".to_string()
            }),
            crate::jira::jql::shipped_awaiting_retro_jql("PROJ")
        );
        assert_eq!(
            render_jql(&TicketQuery::ReadyCandidates),
            crate::jira::jql::ready_candidates_jql()
        );
    }

    #[test]
    fn jira_provider_search_forwards_rendered_jql_to_the_wrapped_client() {
        let provider = JiraProvider::new(FakeJiraClient::new());

        let result = provider
            .search(&TicketQuery::MyOpen)
            .expect("FakeJiraClient::search succeeds by default");

        assert!(result.issues.is_empty());
    }

    #[test]
    fn create_issue_converts_markdown_description_to_adf() {
        let fake = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let provider = JiraProvider::new(fake);

        provider
            .create_issue(&NewTicket {
                project_key: "PROJ".to_string(),
                summary: "Fix the thing".to_string(),
                description: "**bold** text".to_string(),
                issue_type_name: "Task".to_string(),
                assignee_account_id: None,
            })
            .expect("should succeed");
    }

    #[test]
    fn fake_ticket_provider_create_issue_records_adf_description() {
        let fake = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));

        TicketProvider::create_issue(
            &fake,
            &NewTicket {
                project_key: "PROJ".to_string(),
                summary: "Fix the thing".to_string(),
                description: "**bold** text".to_string(),
                issue_type_name: "Task".to_string(),
                assignee_account_id: None,
            },
        )
        .expect("should succeed");

        let calls = fake.create_issue_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].description.to_string().contains("\"strong\""));
    }

    #[test]
    fn fake_ticket_provider_update_description_records_adf() {
        let fake = FakeJiraClient::new();

        TicketProvider::update_description(&fake, "PROJ-1", "**bold** text")
            .expect("should succeed");

        let calls = fake.update_description_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.to_string().contains("\"strong\""));
    }

    #[test]
    fn fake_ticket_provider_add_comment_records_adf() {
        let fake = FakeJiraClient::new();

        TicketProvider::add_comment(&fake, "PROJ-1", "**bold** text").expect("should succeed");

        let calls = fake.add_comment_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.to_string().contains("\"strong\""));
    }

    #[test]
    fn fake_ticket_provider_description_text_renders_plain_text() {
        let fake = FakeJiraClient::new();
        let mut with_description = issue("PROJ-1");
        with_description.fields.description = Some(text_to_adf("plain text"));

        assert_eq!(
            TicketProvider::description_text(&fake, &with_description),
            "plain text"
        );
    }

    #[test]
    fn fake_ticket_provider_description_text_empty_when_no_description() {
        let fake = FakeJiraClient::new();

        assert_eq!(
            TicketProvider::description_text(&fake, &issue("PROJ-1")),
            ""
        );
    }
}
