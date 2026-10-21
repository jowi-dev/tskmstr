//! Backend-agnostic ticket provider trait, and the Jira-backed
//! implementation of it.
//!
//! [`TicketProvider`] is the interface every ticketing orchestration
//! function in [`crate::ticketing`], every `tm ticket`/`tm ready`/`tm pr`
//! command, and the board TUI depend on. A ticket key is still a plain
//! `&str`, and every error is a backend-agnostic
//! [`crate::ticketing::error::ProviderError`] rather than a Jira-specific
//! [`crate::jira::client::JiraError`] — see [`crate::ticketing::error`] for how
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

use crate::config::DEFAULT_EPIC_ISSUE_TYPE;
use crate::jira::adf::{adf_to_text, text_to_adf};
use crate::jira::client::{JiraClient, RankAnchor};
use crate::jira::fake::FakeJiraClient;
use crate::jira::jql::{
    assignee_tickets_jql, children_jql, everyone_tickets_jql, keys_jql, my_open_tickets_jql,
    ranked_tickets_jql, ready_candidates_jql, shipped_awaiting_retro_jql, ticket_search_jql,
    unassigned_tickets_jql,
};
use crate::jira::types::CreateIssueRequest;
use crate::ticketing::error::ProviderError;
use crate::ticketing::types::{
    CreateLinkRequest, Issue, JiraUser, Myself, ParentRef, RemoteLinkRequest, SearchResult,
    Transition,
};

/// A backend-agnostic ticket search, rendered by each [`TicketProvider`] into
/// its own query shape ([`JiraProvider`] renders these to JQL via
/// [`crate::jira::jql`]'s builders; a future `GithubProvider` would render to
/// `gh issue list`/`gh search issues` arguments instead). No caller outside
/// [`crate::jira`] builds a JQL string directly — every board screen and
/// ticketing orchestration function builds one of these variants and hands
/// it to [`TicketProvider::search`].
///
/// Every variant but [`TicketQuery::MyOpen`], [`TicketQuery::ReadyCandidates`],
/// and [`TicketQuery::Keys`]
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
    /// Exactly these tickets, open or closed, in one search: the
    /// cross-project overview's per-scope tracker-status fetch (ADR-0009
    /// decision 3). See [`keys_jql`]. Unscoped by project, since the keys
    /// already name one.
    Keys {
        /// Ticket keys to fetch.
        keys: Vec<String>,
    },
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
        TicketQuery::Keys { keys } => keys_jql(keys),
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
    /// What kind of ticket to create. Each provider maps this onto its own
    /// model (a Jira issue type, a GitHub label).
    pub kind: TicketKind,
    /// Key of the epic to create the ticket under, if any. Never set
    /// alongside [`TicketKind::Epic`]: epics are not nested.
    pub parent_key: Option<String>,
    /// Account ID to assign the new issue to, if any.
    pub assignee_account_id: Option<String>,
}

/// The kind of ticket a [`NewTicket`] creates, independent of how any one
/// backend spells it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TicketKind {
    /// An ordinary unit of work. Every create path made one of these before
    /// epics existed, and still does unless asked otherwise.
    #[default]
    Task,
    /// A larger objective that groups other tickets under it as children
    /// (see [`TicketProvider::set_parent`]).
    Epic,
}

impl TicketKind {
    /// The Jira issue type name for this kind, given the type name the
    /// project uses for epics ([`crate::config::Config::epic_issue_type`]).
    fn jira_issue_type_name(self, epic_issue_type: &str) -> &str {
        match self {
            TicketKind::Task => "Task",
            TicketKind::Epic => epic_issue_type,
        }
    }
}

/// Refuse a [`NewTicket`] that asks for an epic or a parent, for a backend
/// that can't create either yet. Without this the backend would silently
/// create a plain, unparented Task, which looks like success but isn't.
pub(crate) fn refuse_epic_or_parent(req: &NewTicket) -> Result<(), ProviderError> {
    let operation = if req.kind == TicketKind::Epic {
        "creating an epic"
    } else if req.parent_key.is_some() {
        "creating a ticket under an epic"
    } else {
        return Ok(());
    };
    Err(ProviderError::NotSupported {
        operation: operation.to_string(),
    })
}

/// Document-label prefixes that look like ticket keys but never are:
/// `ADR-0006` in a PR body is an architecture-decision-record reference,
/// not a ticket, and scraping it as one associated PR #34 with the wrong
/// issue (GitHub issue #35).
const NON_TICKET_KEY_PREFIXES: &[&str] = &["ADR", "RFC"];

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

    /// Browsable URL for the ticket `key`, in this backend's own link shape
    /// (`{base_url}/browse/{key}` for Jira, the issue page for GitHub). No
    /// caller builds a ticket URL from config any more — under the github
    /// backend `jira_base_url` is empty, and `/browse/GH-13` is not a link
    /// (GitHub issue #13).
    fn issue_url(&self, key: &str) -> String;

    /// Translate a status target ([`crate::config::Config::status_on_pr`],
    /// [`crate::config::Config::status_on_create`], or an explicit
    /// `tm ticket transition <KEY> <STATUS>` argument) into this backend's
    /// own status vocabulary before transition matching. The default is the
    /// identity: a Jira workflow's status names are already Jira's
    /// vocabulary, and `tm` can't know one workflow's aliases for another's
    /// statuses. The github backend overrides this to map common Jira
    /// review-status names onto its fixed `tm:status/*` vocabulary, so a
    /// `status_on_pr = "Code Review"` inherited from a Jira-centric global
    /// config still lands the ticket in In Review instead of warning and
    /// leaving it in To Do (GitHub issue #13).
    fn normalize_status_target(&self, target: &str) -> String {
        target.to_string()
    }

    /// Ensure every issue in `issues` (fresh from [`Self::search`]) carries
    /// its direct `Blocks` blockers in `issue_links`, so
    /// [`crate::blocker_stacking::readiness`] sees real dependency data.
    /// Used by the board's readiness glyphs (GitHub issue #62) and by
    /// [`crate::ticketing::ready_tickets`] (GitHub issue #70).
    ///
    /// The default is a no-op: Jira's search already requests `issuelinks`.
    /// A backend whose search omits dependencies for cost reasons (the
    /// github backend) overrides this with a batched fetch. An `Err` means
    /// the issues' blockers are unknown, and the caller must not treat any
    /// of them as unblocked.
    fn hydrate_blockers(&self, _issues: &mut [Issue]) -> Result<(), ProviderError> {
        Ok(())
    }

    /// Ensure every issue in `issues` (fresh from [`Self::search`]) carries
    /// its epic in [`IssueFields::parent`](crate::ticketing::types::IssueFields::parent),
    /// so a board can show which epic each card belongs to.
    ///
    /// The default is a no-op, for a backend whose search already returns
    /// the parent or that has no parents. The github backend overrides this
    /// with one batched fetch for all of `issues`, the same bounded cost as
    /// [`Self::hydrate_blockers`]. An `Err` means the parents are unknown;
    /// the issues are left as they were.
    fn hydrate_parents(&self, _issues: &mut [Issue]) -> Result<(), ProviderError> {
        Ok(())
    }

    /// Strip leftover workflow-status markers from `key` if (and only if)
    /// the ticket is already closed, returning the markers removed (empty
    /// when there was nothing to strip or the ticket is still open).
    ///
    /// A ticket closed outside `tm` -- GitHub closing an issue because a
    /// merged PR's body said `Closes #N` -- keeps whatever status marker it
    /// last had, so a closed issue still shows `tm:status/in-review`
    /// everywhere labels are read directly (GitHub issue #75). Open tickets
    /// are never touched: a PR that only says `Refs #N` deliberately leaves
    /// its ticket open and in its current status.
    ///
    /// The default is a no-op: a Jira status is a workflow field that
    /// closing already moves, so there's no separate marker to go stale.
    /// The github backend overrides this to remove `tm:status/*` labels.
    fn clear_closed_status_labels(&self, _key: &str) -> Result<Vec<String>, ProviderError> {
        Ok(Vec::new())
    }

    /// Put `child_key` under the epic `parent_key`, or, with `None`, take
    /// it out from under whatever epic it sits under. Moving a ticket that
    /// already has a parent replaces that parent.
    ///
    /// Only the backend call: the "parent must be an epic" and "epics have
    /// no parent" rules live in [`crate::ticketing::set_ticket_parent`], so
    /// every backend enforces them the same way.
    ///
    /// The default is [`ProviderError::NotSupported`]. [`JiraProvider`]
    /// edits Jira's `parent` field; the github backend implements it with
    /// sub-issues.
    fn set_parent(&self, _child_key: &str, _parent_key: Option<&str>) -> Result<(), ProviderError> {
        Err(ProviderError::NotSupported {
            operation: "setting a ticket's parent epic".to_string(),
        })
    }

    /// The tickets directly under the epic `epic_key`, in whatever order
    /// the backend returns them. Empty when the epic has no children.
    ///
    /// The default is [`ProviderError::NotSupported`].
    fn children(&self, _epic_key: &str) -> Result<Vec<Issue>, ProviderError> {
        Err(ProviderError::NotSupported {
            operation: "listing an epic's child tickets".to_string(),
        })
    }

    /// Whether `issue` is an epic in this backend's model (a Jira issue of
    /// type Epic, a GitHub issue labeled `tm:epic`).
    ///
    /// The default is `false`: a backend with no epic support has no
    /// epics, so [`crate::ticketing::set_ticket_parent`] refuses every
    /// parent on it.
    fn is_epic(&self, _issue: &Issue) -> bool {
        false
    }

    /// Whether `token` is plausibly a ticket key in this backend's key
    /// scheme. Used by PR title/body/branch key scraping
    /// ([`crate::ticketing::resolve_existing_key`] and every
    /// [`crate::github::pr::find_pr_for_ticket`] caller) to skip tokens
    /// that merely look key-shaped — `ADR-0006` in a PR body must not
    /// resolve as a ticket (GitHub issue #35).
    ///
    /// The default accepts the Jira key shape (`[A-Z][A-Z0-9]+-\d+`) minus
    /// [`NON_TICKET_KEY_PREFIXES`]; the github backend overrides this to
    /// accept only its own `GH-<number>` keys.
    fn is_ticket_key(&self, token: &str) -> bool {
        let Some((prefix, number)) = token.split_once('-') else {
            return false;
        };
        prefix.len() >= 2
            && prefix
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
            && prefix
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit())
            && !NON_TICKET_KEY_PREFIXES.contains(&prefix)
    }
}

/// [`TicketProvider`] backed by a boxed [`JiraClient`].
///
/// Most methods forward unchanged to the wrapped client. `base_url` exists
/// only for [`TicketProvider::issue_url`]: the [`JiraClient`] trait has no
/// way to report the instance it talks to, so the provider carries it
/// alongside. `epic_issue_type` is the Jira issue type name epics are
/// created as and recognized by (see [`JiraProvider::with_epic_issue_type`]).
pub struct JiraProvider {
    client: Box<dyn JiraClient>,
    base_url: String,
    epic_issue_type: String,
}

impl JiraProvider {
    /// Wrap any Jira client — live or fake — as a [`TicketProvider`].
    /// `base_url` is the Jira instance's browsable root
    /// ([`crate::config::Config::jira_base_url`]), used only to render
    /// ticket URLs.
    pub fn new(client: impl JiraClient + 'static, base_url: impl Into<String>) -> Self {
        Self {
            client: Box::new(client),
            base_url: base_url.into(),
            epic_issue_type: DEFAULT_EPIC_ISSUE_TYPE.to_string(),
        }
    }

    /// Use `name` as the Jira issue type for epics instead of
    /// [`DEFAULT_EPIC_ISSUE_TYPE`], for a project whose epic type is named
    /// differently ([`crate::config::Config::epic_issue_type`]).
    pub fn with_epic_issue_type(mut self, name: impl Into<String>) -> Self {
        self.epic_issue_type = name.into();
        self
    }
}

impl TicketProvider for JiraProvider {
    fn myself(&self) -> Result<Myself, ProviderError> {
        Ok(self.client.myself()?)
    }

    fn get_issue(&self, key: &str) -> Result<Issue, ProviderError> {
        Ok(self.client.get_issue(key)?)
    }

    /// An epic is created as [`JiraProvider::with_epic_issue_type`]'s type
    /// (default `Epic`); if the project has no such type, Jira's create
    /// error is returned as is. A `parent_key` becomes Jira's `parent`
    /// field.
    fn create_issue(&self, req: &NewTicket) -> Result<Issue, ProviderError> {
        Ok(self.client.create_issue(&CreateIssueRequest {
            project_key: req.project_key.clone(),
            summary: req.summary.clone(),
            description: text_to_adf(&req.description),
            issue_type_name: req
                .kind
                .jira_issue_type_name(&self.epic_issue_type)
                .to_string(),
            parent_key: req.parent_key.clone(),
            assignee_account_id: req.assignee_account_id.clone(),
        })?)
    }

    fn add_remote_link(&self, key: &str, link: &RemoteLinkRequest) -> Result<(), ProviderError> {
        Ok(self.client.add_remote_link(key, link)?)
    }

    fn transitions(&self, key: &str) -> Result<Vec<Transition>, ProviderError> {
        Ok(self.client.transitions(key)?)
    }

    fn transition(&self, key: &str, transition_id: &str) -> Result<(), ProviderError> {
        Ok(self.client.transition(key, transition_id)?)
    }

    fn search(&self, query: &TicketQuery) -> Result<SearchResult, ProviderError> {
        Ok(self.client.search(&render_jql(query))?)
    }

    fn get_project(&self, key: &str) -> Result<(), ProviderError> {
        Ok(self.client.get_project(key)?)
    }

    fn assignable_users(&self, project: &str) -> Result<Vec<JiraUser>, ProviderError> {
        Ok(self.client.assignable_users(project)?)
    }

    fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), ProviderError> {
        Ok(self.client.assign(key, account_id)?)
    }

    fn rank(&self, keys: &[String], anchor: RankAnchor) -> Result<(), ProviderError> {
        Ok(self.client.rank(keys, anchor)?)
    }

    fn create_link(&self, req: &CreateLinkRequest) -> Result<(), ProviderError> {
        Ok(self.client.create_link(req)?)
    }

    fn delete_link(&self, link_id: &str) -> Result<(), ProviderError> {
        Ok(self.client.delete_link(link_id)?)
    }

    fn update_description(&self, key: &str, description: &str) -> Result<(), ProviderError> {
        Ok(self
            .client
            .update_description(key, &text_to_adf(description))?)
    }

    fn add_comment(&self, key: &str, body: &str) -> Result<(), ProviderError> {
        Ok(self.client.add_comment(key, &text_to_adf(body))?)
    }

    fn description_text(&self, issue: &Issue) -> String {
        issue
            .fields
            .description
            .as_ref()
            .map(adf_to_text)
            .unwrap_or_default()
    }

    fn issue_url(&self, key: &str) -> String {
        format!("{}/browse/{key}", self.base_url)
    }

    fn set_parent(&self, child_key: &str, parent_key: Option<&str>) -> Result<(), ProviderError> {
        Ok(self.client.set_parent(child_key, parent_key)?)
    }

    /// Searches `parent = <EPIC>` ([`children_jql`]), so a child of any
    /// status is included.
    fn children(&self, epic_key: &str) -> Result<Vec<Issue>, ProviderError> {
        Ok(self.client.search(&children_jql(epic_key))?.issues)
    }

    /// True when the issue's type name equals the epic type name, ignoring
    /// case. An issue fetched without `issuetype` is never an epic.
    fn is_epic(&self, issue: &Issue) -> bool {
        issue
            .fields
            .issue_type
            .as_ref()
            .is_some_and(|issue_type| issue_type.name.eq_ignore_ascii_case(&self.epic_issue_type))
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

    /// Fills in [`IssueFields::parent`](crate::ticketing::types::IssueFields::parent)
    /// from the fake's in-memory parent map, so a parent set through
    /// [`TicketProvider::set_parent`] reads back the way a real backend's
    /// would.
    fn get_issue(&self, key: &str) -> Result<Issue, ProviderError> {
        let mut issue = JiraClient::get_issue(self, key)?;
        issue.fields.parent = self.parent_of(key).map(|parent_key| ParentRef {
            summary: JiraClient::get_issue(self, &parent_key)
                .map(|parent| parent.fields.summary)
                .unwrap_or_default(),
            key: parent_key,
        });
        Ok(issue)
    }

    /// Records the request like any other create, then remembers a created
    /// epic as an epic and a created child under its parent, so the rest of
    /// the parent surface sees the new ticket.
    fn create_issue(&self, req: &NewTicket) -> Result<Issue, ProviderError> {
        let issue = JiraClient::create_issue(
            self,
            &CreateIssueRequest {
                project_key: req.project_key.clone(),
                summary: req.summary.clone(),
                description: text_to_adf(&req.description),
                issue_type_name: req
                    .kind
                    .jira_issue_type_name(DEFAULT_EPIC_ISSUE_TYPE)
                    .to_string(),
                parent_key: req.parent_key.clone(),
                assignee_account_id: req.assignee_account_id.clone(),
            },
        )?;
        if req.kind == TicketKind::Epic {
            self.mark_epic(&issue.key);
        }
        if let Some(parent_key) = &req.parent_key {
            self.store_parent(&issue.key, Some(parent_key));
        }
        Ok(issue)
    }

    fn set_parent(&self, child_key: &str, parent_key: Option<&str>) -> Result<(), ProviderError> {
        Ok(JiraClient::set_parent(self, child_key, parent_key)?)
    }

    fn children(&self, epic_key: &str) -> Result<Vec<Issue>, ProviderError> {
        self.child_keys(epic_key)
            .iter()
            .map(|key| TicketProvider::get_issue(self, key))
            .collect()
    }

    fn is_epic(&self, issue: &Issue) -> bool {
        self.is_epic_key(&issue.key)
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

    fn clear_closed_status_labels(&self, key: &str) -> Result<Vec<String>, ProviderError> {
        self.take_clear_closed_status_labels(key)
            .map_err(|(status, message)| ProviderError::Api { status, message })
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

    /// The fake's fixture instance is `https://example.atlassian.net`,
    /// matching the `jira_base_url` every test `Config` fixture uses, so
    /// URL assertions written against config-built URLs keep passing
    /// unchanged now that the URL comes from the provider.
    fn issue_url(&self, key: &str) -> String {
        format!("https://example.atlassian.net/browse/{key}")
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
                parent: None,
                labels: Vec::new(),
                issue_type: None,
            },
        }
    }

    #[test]
    fn jira_provider_forwards_get_issue_to_the_wrapped_client() {
        let provider = JiraProvider::new(
            FakeJiraClient::new().with_issue("PROJ-1", issue("PROJ-1")),
            "https://example.atlassian.net",
        );

        let result = provider.get_issue("PROJ-1").expect("issue should be found");

        assert_eq!(result.key, "PROJ-1");
    }

    #[test]
    fn jira_provider_converts_errors_to_provider_error() {
        let provider = JiraProvider::new(
            FakeJiraClient::new().with_issue_not_found("PROJ-1"),
            "https://example.atlassian.net",
        );

        let err = provider.get_issue("PROJ-1").expect_err("should fail");

        assert!(matches!(err, ProviderError::NotFound { key } if key == "PROJ-1"));
    }

    #[test]
    fn default_is_ticket_key_accepts_jira_shaped_keys() {
        let provider = JiraProvider::new(FakeJiraClient::new(), "https://example.atlassian.net");

        assert!(provider.is_ticket_key("PROJ-372"));
        assert!(provider.is_ticket_key("GH-30"));
    }

    #[test]
    fn default_is_ticket_key_rejects_known_doc_label_prefixes() {
        let provider = JiraProvider::new(FakeJiraClient::new(), "https://example.atlassian.net");

        assert!(!provider.is_ticket_key("ADR-0006"));
        assert!(!provider.is_ticket_key("RFC-2119"));
    }

    #[test]
    fn default_is_ticket_key_rejects_non_key_shapes() {
        let provider = JiraProvider::new(FakeJiraClient::new(), "https://example.atlassian.net");

        assert!(!provider.is_ticket_key("proj-372"));
        assert!(!provider.is_ticket_key("PROJ-"));
        assert!(!provider.is_ticket_key("372"));
        assert!(!provider.is_ticket_key(""));
    }

    #[test]
    fn jira_provider_is_usable_as_a_trait_object() {
        let provider: Box<dyn TicketProvider> = Box::new(JiraProvider::new(
            FakeJiraClient::new(),
            "https://example.atlassian.net",
        ));

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
        assert_eq!(
            render_jql(&TicketQuery::Keys {
                keys: vec!["PROJ-1".to_string()]
            }),
            crate::jira::jql::keys_jql(&["PROJ-1".to_string()])
        );
    }

    #[test]
    fn jira_provider_search_forwards_rendered_jql_to_the_wrapped_client() {
        let provider = JiraProvider::new(FakeJiraClient::new(), "https://example.atlassian.net");

        let result = provider
            .search(&TicketQuery::MyOpen)
            .expect("FakeJiraClient::search succeeds by default");

        assert!(result.issues.is_empty());
    }

    #[test]
    fn create_issue_converts_markdown_description_to_adf() {
        let fake = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let provider = JiraProvider::new(fake, "https://example.atlassian.net");

        provider
            .create_issue(&NewTicket {
                project_key: "PROJ".to_string(),
                summary: "Fix the thing".to_string(),
                description: "**bold** text".to_string(),
                kind: TicketKind::Task,
                parent_key: None,
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
                kind: TicketKind::Task,
                parent_key: None,
                assignee_account_id: None,
            },
        )
        .expect("should succeed");

        let calls = fake.create_issue_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].description.to_string().contains("\"strong\""));
    }

    fn new_ticket(kind: TicketKind, parent_key: Option<&str>) -> NewTicket {
        NewTicket {
            project_key: "PROJ".to_string(),
            summary: "Fix the thing".to_string(),
            description: String::new(),
            kind,
            parent_key: parent_key.map(str::to_string),
            assignee_account_id: None,
        }
    }

    #[test]
    fn fake_ticket_provider_maps_task_kind_to_the_jira_task_type() {
        let fake = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));

        TicketProvider::create_issue(&fake, &new_ticket(TicketKind::Task, None))
            .expect("should succeed");

        assert_eq!(fake.create_issue_calls()[0].issue_type_name, "Task");
    }

    /// A [`JiraProvider`] over a real [`HttpJiraClient`] pointed at
    /// `server`, so a test can assert the exact request a provider method
    /// sends. The fake can't show that: the provider boxes its client, so
    /// the fake's recorded calls are out of reach.
    fn http_provider(server: &httpmock::MockServer) -> JiraProvider {
        JiraProvider::new(
            crate::jira::client::HttpJiraClient::new(crate::jira::client::JiraClientContext {
                base_url: server.url(""),
                email: "ada@example.com".to_string(),
                token: "test-token".to_string(),
            }),
            server.url(""),
        )
    }

    /// Mock `POST /issue` expecting `fields` to contain `expected_fields`,
    /// plus the `GET /issue/PROJ-9` the client makes for the created issue.
    fn mock_create<'a>(
        server: &'a httpmock::MockServer,
        expected_fields: serde_json::Value,
    ) -> httpmock::Mock<'a> {
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/rest/api/3/issue/PROJ-9");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "key": "PROJ-9",
                    "fields": {
                        "summary": "Fix the thing",
                        "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                        "description": null,
                        "assignee": null
                    }
                }));
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/issue")
                .json_body_partial(serde_json::json!({ "fields": expected_fields }).to_string());
            then.status(201)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "id": "10009", "key": "PROJ-9" }));
        })
    }

    #[test]
    fn jira_provider_creates_an_epic_as_the_default_epic_type() {
        let server = httpmock::MockServer::start();
        let create = mock_create(
            &server,
            serde_json::json!({ "issuetype": { "name": "Epic" } }),
        );

        http_provider(&server)
            .create_issue(&new_ticket(TicketKind::Epic, None))
            .expect("epic create should succeed");

        create.assert();
    }

    #[test]
    fn jira_provider_creates_an_epic_as_the_configured_epic_type() {
        let server = httpmock::MockServer::start();
        let create = mock_create(
            &server,
            serde_json::json!({ "issuetype": { "name": "Initiative" } }),
        );

        http_provider(&server)
            .with_epic_issue_type("Initiative")
            .create_issue(&new_ticket(TicketKind::Epic, None))
            .expect("epic create should succeed");

        create.assert();
    }

    #[test]
    fn jira_provider_creates_a_task_under_a_parent_epic() {
        let server = httpmock::MockServer::start();
        let create = mock_create(
            &server,
            serde_json::json!({
                "issuetype": { "name": "Task" },
                "parent": { "key": "PROJ-1" }
            }),
        );

        http_provider(&server)
            .create_issue(&new_ticket(TicketKind::Task, Some("PROJ-1")))
            .expect("child create should succeed");

        create.assert();
    }

    #[test]
    fn jira_provider_set_parent_edits_the_parent_field() {
        let server = httpmock::MockServer::start();
        let set = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/api/3/issue/PROJ-2")
                .json_body(serde_json::json!({ "fields": { "parent": { "key": "PROJ-1" } } }));
            then.status(204);
        });
        let clear = server.mock(|when, then| {
            when.method(httpmock::Method::PUT)
                .path("/rest/api/3/issue/PROJ-3")
                .json_body(serde_json::json!({ "fields": { "parent": null } }));
            then.status(204);
        });
        let provider = http_provider(&server);

        provider
            .set_parent("PROJ-2", Some("PROJ-1"))
            .expect("set should succeed");
        provider
            .set_parent("PROJ-3", None)
            .expect("clear should succeed");

        set.assert();
        clear.assert();
    }

    #[test]
    fn jira_provider_children_searches_by_parent() {
        let server = httpmock::MockServer::start();
        let search = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/rest/api/3/search/jql")
                .json_body_partial(
                    serde_json::json!({ "jql": crate::jira::jql::children_jql("PROJ-1") })
                        .to_string(),
                );
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "issues": [{
                        "key": "PROJ-2",
                        "fields": {
                            "summary": "Child",
                            "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                            "description": null,
                            "assignee": null
                        }
                    }]
                }));
        });

        let children = http_provider(&server)
            .children("PROJ-1")
            .expect("children should succeed");

        search.assert();
        let keys: Vec<&str> = children.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["PROJ-2"]);
    }

    fn issue_of_type(type_name: Option<&str>) -> Issue {
        let mut issue = issue("PROJ-1");
        issue.fields.issue_type = type_name.map(|name| crate::ticketing::types::IssueType {
            name: name.to_string(),
        });
        issue
    }

    #[test]
    fn jira_provider_is_epic_matches_the_epic_type_case_insensitively() {
        let provider = JiraProvider::new(FakeJiraClient::new(), "https://example.atlassian.net");

        assert!(provider.is_epic(&issue_of_type(Some("Epic"))));
        assert!(provider.is_epic(&issue_of_type(Some("epic"))));
        assert!(!provider.is_epic(&issue_of_type(Some("Task"))));
        assert!(!provider.is_epic(&issue_of_type(None)));
    }

    #[test]
    fn jira_provider_is_epic_uses_the_configured_epic_type() {
        let provider = JiraProvider::new(FakeJiraClient::new(), "https://example.atlassian.net")
            .with_epic_issue_type("Initiative");

        assert!(provider.is_epic(&issue_of_type(Some("Initiative"))));
        assert!(!provider.is_epic(&issue_of_type(Some("Epic"))));
    }

    #[test]
    fn fake_ticket_provider_marks_a_created_epic_as_an_epic() {
        let fake = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));

        let created = TicketProvider::create_issue(&fake, &new_ticket(TicketKind::Epic, None))
            .expect("should succeed");

        assert!(fake.is_epic(&created));
        assert_eq!(fake.create_issue_calls()[0].issue_type_name, "Epic");
    }

    #[test]
    fn fake_ticket_provider_records_the_parent_of_a_ticket_created_under_an_epic() {
        let fake = FakeJiraClient::new()
            .with_epic("PROJ-1", issue("PROJ-1"))
            .with_issue("PROJ-9", issue("PROJ-9"))
            .with_create_issue_result(issue("PROJ-9"));

        TicketProvider::create_issue(&fake, &new_ticket(TicketKind::Task, Some("PROJ-1")))
            .expect("should succeed");

        let child = TicketProvider::get_issue(&fake, "PROJ-9").unwrap();
        assert_eq!(
            child.fields.parent.map(|p| p.key),
            Some("PROJ-1".to_string())
        );
    }

    #[test]
    fn fake_ticket_provider_set_parent_records_the_call_and_shows_on_get_issue() {
        let mut epic = issue("PROJ-1");
        epic.fields.summary = "The epic".to_string();
        let fake = FakeJiraClient::new()
            .with_epic("PROJ-1", epic)
            .with_issue("PROJ-2", issue("PROJ-2"));

        TicketProvider::set_parent(&fake, "PROJ-2", Some("PROJ-1")).unwrap();

        assert_eq!(
            fake.set_parent_calls(),
            vec![("PROJ-2".to_string(), Some("PROJ-1".to_string()))]
        );
        let child = TicketProvider::get_issue(&fake, "PROJ-2").unwrap();
        assert_eq!(
            child.fields.parent,
            Some(crate::ticketing::types::ParentRef {
                key: "PROJ-1".to_string(),
                summary: "The epic".to_string(),
            })
        );
    }

    #[test]
    fn fake_ticket_provider_set_parent_none_clears_the_parent() {
        let fake = FakeJiraClient::new()
            .with_epic("PROJ-1", issue("PROJ-1"))
            .with_issue("PROJ-2", issue("PROJ-2"))
            .with_parent("PROJ-2", "PROJ-1");

        TicketProvider::set_parent(&fake, "PROJ-2", None).unwrap();

        let child = TicketProvider::get_issue(&fake, "PROJ-2").unwrap();
        assert_eq!(child.fields.parent, None);
        assert!(fake.children("PROJ-1").unwrap().is_empty());
    }

    #[test]
    fn fake_ticket_provider_children_returns_only_that_epics_children_in_key_order() {
        let fake = FakeJiraClient::new()
            .with_epic("PROJ-1", issue("PROJ-1"))
            .with_epic("PROJ-5", issue("PROJ-5"))
            .with_issue("PROJ-3", issue("PROJ-3"))
            .with_issue("PROJ-2", issue("PROJ-2"))
            .with_issue("PROJ-4", issue("PROJ-4"))
            .with_parent("PROJ-3", "PROJ-1")
            .with_parent("PROJ-2", "PROJ-1")
            .with_parent("PROJ-4", "PROJ-5");

        let keys: Vec<String> = fake
            .children("PROJ-1")
            .unwrap()
            .into_iter()
            .map(|child| child.key)
            .collect();

        assert_eq!(keys, vec!["PROJ-2".to_string(), "PROJ-3".to_string()]);
    }

    #[test]
    fn fake_ticket_provider_only_seeded_or_created_epics_are_epics() {
        let fake = FakeJiraClient::new().with_epic("PROJ-1", issue("PROJ-1"));

        assert!(fake.is_epic(&issue("PROJ-1")));
        assert!(!fake.is_epic(&issue("PROJ-2")));
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
    fn jira_provider_issue_url_is_base_url_browse_key() {
        let provider = JiraProvider::new(FakeJiraClient::new(), "https://jira.example.com");

        assert_eq!(
            provider.issue_url("PROJ-7"),
            "https://jira.example.com/browse/PROJ-7"
        );
    }

    #[test]
    fn fake_ticket_provider_issue_url_uses_the_fixture_base_url() {
        let fake = FakeJiraClient::new();

        assert_eq!(
            TicketProvider::issue_url(&fake, "PROJ-7"),
            "https://example.atlassian.net/browse/PROJ-7"
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
