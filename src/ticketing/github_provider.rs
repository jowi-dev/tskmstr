//! [`GithubProvider`]: the GitHub Issues [`TicketProvider`] implementation,
//! read path only (phase 5 of `docs/plans/github-issues-backend.md`; GitHub
//! issue #3).
//!
//! Status lives in the reserved `tm:status/*` label namespace rather than a
//! workflow field GitHub doesn't have: no label (or `tm:status/todo`) means
//! To Do, `tm:status/in-progress`/`tm:status/in-review`/`tm:status/blocked`
//! mean In Progress/In Review/Blocked, and a closed issue is always Done
//! regardless of what label it carries. [`synthesize_status_slug`] is the one
//! place that rule lives; [`transitions`](TicketProvider::transitions) and
//! [`transition`](TicketProvider::transition) both build on it rather than
//! fetching a transition list from anywhere, since GitHub has nothing to
//! fetch. Dependencies come from GitHub's native issue-dependencies GraphQL
//! feature (already wrapped by [`crate::github::gh_cli::GhCli::issue_dependencies`])
//! and are surfaced as [`LinkedIssue`]s under the link type name `"Blocks"`,
//! matching the hardcoded string [`crate::blocker_stacking`] and
//! [`super::open_blockers`] already key off of.
//!
//! A ticket key is `GH-<number>` ([`parse_issue_number`] strips the prefix);
//! this provider is driven entirely by the configured `repo` slug, never a
//! git checkout.
//!
//! Phase 6 (`docs/plans/github-issues-backend.md`) implements every
//! write-path method: [`TicketProvider::create_issue`] (`gh issue create`,
//! `tm:status/todo` applied at creation), [`TicketProvider::assign`]
//! (`gh issue edit --add-assignee/--remove-assignee`),
//! [`TicketProvider::add_comment`]/[`TicketProvider::update_description`]
//! (`gh issue comment`/`gh issue edit --body`, Markdown pass-through),
//! [`TicketProvider::create_link`]/[`TicketProvider::delete_link`] (GitHub's
//! native issue-dependencies GraphQL mutations, via
//! [`crate::github::gh_cli::GhCli::create_issue_dependency`]/
//! [`crate::github::gh_cli::GhCli::delete_issue_dependency`]), and
//! [`TicketProvider::rank`] (the local `ticket_rank` table in `runs.db`, via
//! an optional borrowed [`RunStore`] — see [`GithubProvider::with_rank_store`]).
//! [`TicketProvider::add_remote_link`] is the one method that stays a
//! deliberate no-op: see its doc comment for why GitHub needs no remote link
//! at all.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::github::gh_cli::{
    GhCli, IssueCreateRequest, IssueDependencies, IssueEditRequest, IssueInfo, IssueListFilter,
    IssueListState, IssueRef, IssueState, IssueStateChange,
};
use crate::jira::client::RankAnchor;
use crate::runs::RunStore;
use crate::ticketing::error::ProviderError;
use crate::ticketing::provider::{NewTicket, TicketProvider, TicketQuery};
use crate::ticketing::types::{
    CreateLinkRequest, Issue, IssueFields, IssueLink, IssueLinkType, JiraUser, LinkedIssue,
    LinkedIssueFields, Myself, RemoteLinkRequest, SearchResult, Status, StatusCategory, Transition,
    UserRef,
};
use serde_json::Value;

/// Prefix of every `tm`-managed status label. Exactly one of
/// `{prefix}todo`/`in-progress`/`in-review`/`blocked` should be set on an
/// open issue at a time; [`synthesize_status_slug`] documents the tie-break
/// used when more than one is present.
const STATUS_LABEL_PREFIX: &str = "tm:status/";

/// [`TicketProvider`] backed by GitHub Issues via a borrowed [`GhCli`].
///
/// Holds a `&dyn GhCli` rather than owning a boxed one (contrast
/// [`super::provider::JiraProvider`], which owns a boxed [`crate::jira::client::JiraClient`]):
/// tests construct a [`crate::github::gh_cli::FakeGhCli`], pass a reference
/// in, and inspect its recorded calls afterward, the same reason phase 1
/// gave for not moving `FakeJiraClient` into a box. Production wiring
/// (`main.rs`) leaks a `ShellGhCli` to get a `&'static dyn GhCli` it can
/// build a `Box<dyn TicketProvider>` from, since `tm` is a short-lived CLI
/// process and one leaked zero-sized value per invocation costs nothing.
pub struct GithubProvider<'a> {
    gh: &'a dyn GhCli,
    repo: String,
    key_prefix: String,
    /// Backing store for the local `ticket_rank` table (see the module doc
    /// comment). `None` in every existing test that doesn't exercise
    /// `rank`/`Ranked`/`ReadyCandidates` — attach one via
    /// [`GithubProvider::with_rank_store`]. Production wiring (`main.rs`)
    /// always attaches one; `JiraProvider` has no equivalent field at all,
    /// since Jira has its own native rank and never needs this table.
    rank_store: Option<&'a RunStore>,
}

impl<'a> GithubProvider<'a> {
    /// Build a provider against `repo` (an `"owner/name"` slug). Keys are
    /// `GH-<number>`; a configurable prefix (per GitHub issue #3's design)
    /// isn't wired up yet, so `key_prefix` is always `"GH"`. No rank store is
    /// attached by default — see [`GithubProvider::with_rank_store`].
    pub fn new(gh: &'a dyn GhCli, repo: String) -> Self {
        Self {
            gh,
            repo,
            key_prefix: "GH".to_string(),
            rank_store: None,
        }
    }

    /// Attach the local rank store backing [`TicketProvider::rank`] and the
    /// `Ranked`/`ReadyCandidates` [`TicketQuery`] variants. Without one,
    /// `rank` fails with [`ProviderError::Api`] and searches fall back to
    /// plain issue-number order (as if nothing were ever ranked) — the same
    /// degenerate behavior phase 5 shipped before this table existed.
    pub fn with_rank_store(mut self, store: &'a RunStore) -> Self {
        self.rank_store = Some(store);
        self
    }

    fn issue_key(&self, number: u64) -> String {
        format!("{}-{}", self.key_prefix, number)
    }

    fn current_login(&self) -> Result<String, ProviderError> {
        self.gh
            .current_user_login()
            .map_err(ProviderError::from)?
            .ok_or(ProviderError::Unauthorized)
    }

    fn to_issue(&self, key: &str, number: u64, info: IssueInfo, deps: IssueDependencies) -> Issue {
        let closed = matches!(info.state, IssueState::Closed);
        let status = status_for_slug(synthesize_status_slug(closed, &info.labels));
        let assignee = info.assignees.first().map(|login| UserRef {
            account_id: login.clone(),
            display_name: login.clone(),
        });
        let description = if info.body.trim().is_empty() {
            None
        } else {
            Some(Value::String(info.body))
        };
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: info.title,
                status,
                description,
                assignee,
                issue_links: self.to_issue_links(number, deps),
            },
        }
    }

    /// Build this issue's [`IssueLink`]s from its dependencies. `number` is
    /// *this* issue's own number, needed (alongside each dependency's
    /// number) so the synthesized link id encodes both ends of the
    /// relationship -- see [`link_id`]'s doc comment for why a one-sided id
    /// (just the neighbor's number, phase 5's shape) can't be parsed back
    /// into the pair [`TicketProvider::delete_link`] needs.
    fn to_issue_links(&self, number: u64, deps: IssueDependencies) -> Vec<IssueLink> {
        let link_type = || IssueLinkType {
            name: "Blocks".to_string(),
            inward: "is blocked by".to_string(),
            outward: "blocks".to_string(),
        };
        let mut links = Vec::with_capacity(deps.blocked_by.len() + deps.blocking.len());
        for dep in deps.blocked_by {
            // `dep` blocks this issue.
            let blocker_number = dep.number;
            links.push(IssueLink {
                id: link_id(blocker_number, number),
                link_type: link_type(),
                inward_issue: Some(self.to_linked_issue(dep)),
                outward_issue: None,
            });
        }
        for dep in deps.blocking {
            // This issue blocks `dep`.
            let blocked_number = dep.number;
            links.push(IssueLink {
                id: link_id(number, blocked_number),
                link_type: link_type(),
                inward_issue: None,
                outward_issue: Some(self.to_linked_issue(dep)),
            });
        }
        links
    }

    /// Build a [`LinkedIssue`] from a dependency reference. GitHub's
    /// dependencies GraphQL query returns only `number`/`title`/`state`/`url`
    /// for each linked issue (see
    /// [`crate::github::gh_cli::GhCli::issue_dependencies`]'s query), not its
    /// labels, so the linked issue's synthesized status can only ever be
    /// `Done` (closed) or `To Do` (open) — never `In Progress`/`In
    /// Review`/`Blocked`, which would need a follow-up `issue_view` per
    /// dependency. Deliberately not fetched here to avoid an
    /// N-dependencies-per-issue fan-out; phase 6 can revisit if a more
    /// precise status is worth that cost.
    fn to_linked_issue(&self, dep: IssueRef) -> LinkedIssue {
        let slug = if matches!(dep.state, IssueState::Closed) {
            "done"
        } else {
            "todo"
        };
        LinkedIssue {
            key: self.issue_key(dep.number),
            fields: LinkedIssueFields {
                summary: dep.title,
                status: status_for_slug(slug),
            },
        }
    }

    fn list_and_map(&self, filter: IssueListFilter) -> Result<Vec<Issue>, ProviderError> {
        let infos = self
            .gh
            .issue_list(&self.repo, &filter)
            .map_err(ProviderError::from)?;
        Ok(infos
            .into_iter()
            .map(|info| {
                let number = info.number;
                let key = self.issue_key(number);
                self.to_issue(&key, number, info, IssueDependencies::default())
            })
            .collect())
    }

    /// Sort `issues` for the `Ranked`/`ReadyCandidates` [`TicketQuery`]
    /// variants: issues with a recorded local rank come first, ascending by
    /// rank; every other issue follows, ascending by issue number. With no
    /// rank store attached (or nothing ranked yet), every issue falls into
    /// the second group, degenerating to the plain issue-number order phase
    /// 5 shipped before this table existed.
    fn apply_local_rank_order(&self, mut issues: Vec<Issue>) -> Vec<Issue> {
        let ranks: HashMap<String, f64> = self
            .rank_store
            .and_then(|store| store.all_ticket_ranks().ok())
            .unwrap_or_default()
            .into_iter()
            .collect();
        issues.sort_by(|a, b| {
            let ra = ranks.get(&a.key);
            let rb = ranks.get(&b.key);
            match (ra, rb) {
                (Some(x), Some(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => parse_issue_number(&a.key)
                    .unwrap_or(u64::MAX)
                    .cmp(&parse_issue_number(&b.key).unwrap_or(u64::MAX)),
            }
        });
        issues
    }
}

/// Build a link id encoding both ends of a `Blocks` dependency:
/// `blocker_number` blocks `blocked_number`. Deliberately symmetric
/// regardless of which side ([`GhCli::issue_dependencies`]'s `blocked_by` or
/// `blocking` connection) it was discovered from -- a link is one
/// relationship between two issues, not two, so both directions produce the
/// same id for the same pair. [`parse_link_id`] is the inverse.
///
/// Phase 5's id (`gh-dep-blocked-by-<neighbor>`/`gh-dep-blocking-<neighbor>`)
/// only encoded the *neighbor's* number, not the issue being viewed --
/// enough to render a link, but not enough for
/// [`TicketProvider::delete_link`] (which receives only the id, with no
/// other context) to know which two issues to call
/// [`GhCli::delete_issue_dependency`] on. Encoding both numbers up front
/// avoids threading a second parameter through `delete_link`'s signature.
fn link_id(blocker_number: u64, blocked_number: u64) -> String {
    format!("gh-dep-{blocker_number}-blocks-{blocked_number}")
}

/// Parse a [`link_id`]-shaped string back into `(blocker_number,
/// blocked_number)`. `None` for anything else, including phase 5's
/// now-stale one-sided id shape (a link fetched under phase 5 and deleted
/// only after upgrading to phase 6 simply won't parse -- an edge case
/// tolerated by returning [`ProviderError::LinkIdNotFound`], not a panic).
fn parse_link_id(link_id: &str) -> Option<(u64, u64)> {
    let rest = link_id.strip_prefix("gh-dep-")?;
    let (blocker, blocked) = rest.split_once("-blocks-")?;
    Some((blocker.parse().ok()?, blocked.parse().ok()?))
}

/// Wrap a [`crate::runs::RunStoreError`] as a [`ProviderError::Api`] with a
/// synthetic `status` of `0`, the same convention [`From<GhError>`] uses for
/// a backend with no real HTTP status to report.
fn store_err(err: crate::runs::RunStoreError) -> ProviderError {
    ProviderError::Api {
        status: 0,
        message: err.to_string(),
    }
}

/// Compute new rank values for `moving` (a batch of ticket keys, excluding
/// `anchor_key` itself), placing them contiguously immediately before
/// (`before: true`) or after (`before: false`) `anchor_key`, in the order
/// given, using fractional-index interpolation between `anchor_key`'s rank
/// and its nearest neighbor on that side in `existing` (sorted ascending by
/// rank, as [`crate::runs::RunStore::all_ticket_ranks`] returns it).
///
/// If `anchor_key` has no rank in `existing`, it is assigned one first (past
/// the current maximum, i.e. logically "at the end" -- matching the design
/// doc's "unranked issues fall to the end" for the anchor's own case), and
/// that assignment is included in the returned list alongside `moving`'s.
///
/// Returns `(ticket_key, rank)` pairs to persist via
/// [`crate::runs::RunStore::set_ticket_rank`]. Pure and independent of any
/// store so it's unit-testable without SQLite.
fn compute_new_ranks(
    existing: &[(String, f64)],
    moving: &[String],
    anchor_key: &str,
    before: bool,
) -> Vec<(String, f64)> {
    let anchor_index = existing.iter().position(|(key, _)| key == anchor_key);
    let (anchor_rank, mut assignments) = match anchor_index.map(|i| existing[i].1) {
        Some(rank) => (rank, Vec::new()),
        None => {
            let rank = existing.last().map_or(1000.0, |(_, r)| r + 1000.0);
            (rank, vec![(anchor_key.to_string(), rank)])
        }
    };

    let (low, high) = if before {
        let predecessor = anchor_index.filter(|&i| i > 0).map(|i| existing[i - 1].1);
        (predecessor.unwrap_or(anchor_rank - 1000.0), anchor_rank)
    } else {
        let successor = anchor_index
            .and_then(|i| existing.get(i + 1))
            .map(|(_, r)| *r);
        (anchor_rank, successor.unwrap_or(anchor_rank + 1000.0))
    };

    let slots = moving.len() as f64 + 1.0;
    for (i, key) in moving.iter().enumerate() {
        let frac = (i as f64 + 1.0) / slots;
        assignments.push((key.clone(), low + (high - low) * frac));
    }
    assignments
}

/// Extract the issue number out of a `GH-<number>`-shaped key (or any
/// `<prefix>-<number>` key -- the prefix itself isn't validated, since
/// `normalize_key`/`project_key_from_issue_key` upstream already establish
/// that whatever reaches a [`TicketProvider`] is well-formed for whichever
/// backend is configured).
fn parse_issue_number(key: &str) -> Result<u64, ProviderError> {
    key.rsplit('-')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| ProviderError::NotFound {
            key: key.to_string(),
        })
}

/// Classify an issue's current status into one of five slugs:
/// `"todo"`/`"in-progress"`/`"in-review"`/`"blocked"`/`"done"`.
///
/// A closed issue is always `"done"`, regardless of what status label it
/// carries -- closing an issue is the terminal action, and a stale status
/// label left on a closed issue shouldn't resurrect it into an open-looking
/// column. For an open issue with more than one `tm:status/*` label set (not
/// a state `tm` itself ever produces, but not one it can prevent a human or
/// another tool from creating), the tie-break is fixed priority order
/// `blocked` > `in-review` > `in-progress` > `todo`: the most
/// attention-worthy status wins. No label at all, or an unrecognized label,
/// both fall through to `"todo"`.
fn synthesize_status_slug(closed: bool, labels: &[String]) -> &'static str {
    if closed {
        return "done";
    }
    let has = |label: &str| labels.iter().any(|l| l == label);
    if has("tm:status/blocked") {
        "blocked"
    } else if has("tm:status/in-review") {
        "in-review"
    } else if has("tm:status/in-progress") {
        "in-progress"
    } else {
        "todo"
    }
}

fn status_for_slug(slug: &str) -> Status {
    let (name, category) = match slug {
        "todo" => ("To Do", "new"),
        "in-progress" => ("In Progress", "indeterminate"),
        "in-review" => ("In Review", "indeterminate"),
        "blocked" => ("Blocked", "indeterminate"),
        "done" => ("Done", "done"),
        other => unreachable!("status_for_slug called with unknown slug {other:?}"),
    };
    Status {
        name: name.to_string(),
        status_category: StatusCategory {
            key: category.to_string(),
        },
    }
}

fn transition_display_name(slug: &str) -> &'static str {
    match slug {
        "todo" => "To Do",
        "in-progress" => "In Progress",
        "in-review" => "In Review",
        "blocked" => "Blocked",
        "done" => "Done",
        "reopen" => "Reopen",
        other => unreachable!("transition_display_name called with unknown slug {other:?}"),
    }
}

/// The synthesized transitions available on an issue: for a closed issue,
/// only `Reopen` (leading back to `todo`, per the design doc); for an open
/// issue, every status slug but the current one, plus `Done`.
fn synthesize_transitions(closed: bool, labels: &[String]) -> Vec<Transition> {
    if closed {
        return vec![Transition {
            id: "reopen".to_string(),
            name: transition_display_name("reopen").to_string(),
            to: status_for_slug("todo"),
        }];
    }
    let current = synthesize_status_slug(false, labels);
    let mut transitions: Vec<Transition> = ["todo", "in-progress", "in-review", "blocked"]
        .into_iter()
        .filter(|slug| *slug != current)
        .map(|slug| Transition {
            id: slug.to_string(),
            name: transition_display_name(slug).to_string(),
            to: status_for_slug(slug),
        })
        .collect();
    transitions.push(Transition {
        id: "done".to_string(),
        name: transition_display_name("done").to_string(),
        to: status_for_slug("done"),
    });
    transitions
}

/// The label-edit + state-change request that applies `transition_id` to an
/// issue currently carrying `current_labels`. `None` for an unrecognized id.
fn transition_edit_request(
    transition_id: &str,
    current_labels: &[String],
) -> Option<IssueEditRequest> {
    let remove_labels: Vec<String> = current_labels
        .iter()
        .filter(|label| label.starts_with(STATUS_LABEL_PREFIX))
        .cloned()
        .collect();

    match transition_id {
        "reopen" => Some(IssueEditRequest {
            remove_labels,
            add_labels: vec!["tm:status/todo".to_string()],
            state: Some(IssueStateChange::Reopen),
            ..Default::default()
        }),
        "done" => Some(IssueEditRequest {
            remove_labels,
            state: Some(IssueStateChange::Close),
            ..Default::default()
        }),
        "todo" | "in-progress" | "in-review" | "blocked" => Some(IssueEditRequest {
            remove_labels,
            add_labels: vec![format!("{STATUS_LABEL_PREFIX}{transition_id}")],
            state: None,
            ..Default::default()
        }),
        _ => None,
    }
}

/// Classify a [`GhCli::issue_view`] failure for `key`: a [`GhError::Command`]
/// (the process ran and `gh` itself reported failure -- typically the issue
/// or repo doesn't exist or isn't visible) becomes
/// [`ProviderError::NotFound`], since that's the only way `issue_view` fails
/// in practice. Anything else ([`GhError::Spawn`]/[`GhError::Parse`]/
/// [`GhError::Timeout`] -- `gh` not installed, malformed JSON, a hung
/// process) is a real transient/environmental failure and is passed through
/// via [`ProviderError::from`] instead of being misreported as "not found".
///
/// Phase 5 mapped every `issue_view` error to `NotFound` unconditionally,
/// swallowing that distinction; this is phase 6's fix, applied to
/// [`TicketProvider::get_issue`], [`TicketProvider::transitions`], and
/// [`TicketProvider::transition`], the three callers that look an issue up
/// by key before doing anything else.
fn map_issue_view_error(key: &str, err: crate::github::gh_cli::GhError) -> ProviderError {
    use crate::github::gh_cli::GhError;
    match err {
        GhError::Command { .. } => ProviderError::NotFound {
            key: key.to_string(),
        },
        other => ProviderError::from(other),
    }
}

impl TicketProvider for GithubProvider<'_> {
    fn myself(&self) -> Result<Myself, ProviderError> {
        let login = self.current_login()?;
        Ok(Myself {
            account_id: login.clone(),
            display_name: login,
            email_address: None,
        })
    }

    fn get_issue(&self, key: &str) -> Result<Issue, ProviderError> {
        let number = parse_issue_number(key)?;
        let info = self
            .gh
            .issue_view(&self.repo, number)
            .map_err(|err| map_issue_view_error(key, err))?;
        let deps = self
            .gh
            .issue_dependencies(&self.repo, number)
            .unwrap_or_default();
        Ok(self.to_issue(key, number, info, deps))
    }

    fn create_issue(&self, req: &NewTicket) -> Result<Issue, ProviderError> {
        // `req.issue_type_name` is Jira-only (Jira's issue-type field has no
        // GitHub equivalent -- an issue is just an issue) and is deliberately
        // ignored here rather than encoded as a label or rejected: GitHub
        // callers pass whatever `NewTicket` requires today, and this is where
        // that Jira-specific field's journey ends, per the carry-forward
        // decision this phase settles (see the plan doc's phase 6 section).
        let info = self
            .gh
            .issue_create(
                &self.repo,
                &IssueCreateRequest {
                    title: req.summary.clone(),
                    body: req.description.clone(),
                    labels: vec!["tm:status/todo".to_string()],
                    assignees: req.assignee_account_id.clone().into_iter().collect(),
                },
            )
            .map_err(ProviderError::from)?;
        let number = info.number;
        let key = self.issue_key(number);
        Ok(self.to_issue(&key, number, info, IssueDependencies::default()))
    }

    /// A deliberate no-op: under the github backend, associating a ticket
    /// with a PR needs no remote link at all. `GH-123` in the PR title (see
    /// `crate::github::pr::with_issue_key_prefix`, already applied by
    /// [`crate::ticketing::associate`] before this is ever called) plus the
    /// `Closes #123` line `tm` puts in the PR body is the whole association
    /// -- GitHub renders the backlink on the issue page for free, with
    /// nothing for `tm` to post. Returning `Ok(())` unconditionally (rather
    /// than, say, appending a comment) is the cheapest honest behavior: a
    /// comment would be a second, redundant statement of a link GitHub
    /// already displays natively, and Jira's flow itself only calls this once
    /// per association, so there's no state this no-op could get out of sync
    /// with.
    fn add_remote_link(&self, _key: &str, _link: &RemoteLinkRequest) -> Result<(), ProviderError> {
        Ok(())
    }

    fn transitions(&self, key: &str) -> Result<Vec<Transition>, ProviderError> {
        let number = parse_issue_number(key)?;
        let info = self
            .gh
            .issue_view(&self.repo, number)
            .map_err(|err| map_issue_view_error(key, err))?;
        let closed = matches!(info.state, IssueState::Closed);
        Ok(synthesize_transitions(closed, &info.labels))
    }

    fn transition(&self, key: &str, transition_id: &str) -> Result<(), ProviderError> {
        let number = parse_issue_number(key)?;
        let info = self
            .gh
            .issue_view(&self.repo, number)
            .map_err(|err| map_issue_view_error(key, err))?;
        let req = transition_edit_request(transition_id, &info.labels).ok_or_else(|| {
            ProviderError::Api {
                status: 0,
                message: format!("unknown transition id {transition_id:?} for the github backend"),
            }
        })?;
        self.gh
            .issue_edit(&self.repo, number, &req)
            .map_err(ProviderError::from)
    }

    fn search(&self, query: &TicketQuery) -> Result<SearchResult, ProviderError> {
        let issues = match query {
            TicketQuery::MyOpen => {
                let login = self.current_login()?;
                self.list_and_map(IssueListFilter {
                    state: IssueListState::Open,
                    assignee: Some(login),
                    ..Default::default()
                })?
            }
            TicketQuery::Unassigned { .. } => self
                .list_and_map(IssueListFilter {
                    state: IssueListState::Open,
                    ..Default::default()
                })?
                .into_iter()
                .filter(|issue| issue.fields.assignee.is_none())
                .collect(),
            TicketQuery::Everyone { .. } => self.list_and_map(IssueListFilter {
                state: IssueListState::Open,
                ..Default::default()
            })?,
            TicketQuery::Assignee { account_id, .. } => self.list_and_map(IssueListFilter {
                state: IssueListState::Open,
                assignee: Some(account_id.clone()),
                ..Default::default()
            })?,
            TicketQuery::Ranked { .. } => {
                let issues = self.list_and_map(IssueListFilter {
                    state: IssueListState::Open,
                    ..Default::default()
                })?;
                self.apply_local_rank_order(issues)
            }
            TicketQuery::Search { text, .. } => {
                let needle = text.to_lowercase();
                self.list_and_map(IssueListFilter {
                    state: IssueListState::Open,
                    ..Default::default()
                })?
                .into_iter()
                .filter(|issue| issue.fields.summary.to_lowercase().contains(&needle))
                .collect()
            }
            TicketQuery::ShippedAwaitingRetro { .. } => {
                // No closed-at timestamp is exposed by `IssueInfo`, so this
                // can't scope to a lookback window the way the Jira JQL
                // builder does -- every closed issue is returned. Documented
                // as a known gap in the phase 5 report.
                self.list_and_map(IssueListFilter {
                    state: IssueListState::Closed,
                    ..Default::default()
                })?
            }
            TicketQuery::ReadyCandidates => {
                let login = self.current_login()?;
                let issues: Vec<Issue> = self
                    .list_and_map(IssueListFilter {
                        state: IssueListState::Open,
                        assignee: Some(login),
                        ..Default::default()
                    })?
                    .into_iter()
                    .filter(|issue| issue.fields.status.status_category.key == "new")
                    .collect();
                self.apply_local_rank_order(issues)
            }
        };
        Ok(SearchResult {
            issues,
            next_page_token: None,
        })
    }

    fn get_project(&self, key: &str) -> Result<(), ProviderError> {
        self.gh
            .issue_list(
                &self.repo,
                &IssueListFilter {
                    limit: 1,
                    ..Default::default()
                },
            )
            .map(|_| ())
            .map_err(|_| ProviderError::ProjectNotFound {
                project: key.to_string(),
            })
    }

    fn assignable_users(&self, _project: &str) -> Result<Vec<JiraUser>, ProviderError> {
        let logins = self
            .gh
            .repo_assignees(&self.repo)
            .map_err(ProviderError::from)?;
        Ok(logins
            .into_iter()
            .map(|login| JiraUser {
                account_id: login.clone(),
                display_name: login,
            })
            .collect())
    }

    /// Sets `key`'s sole assignee to `account_id` (`None` unassigns
    /// everyone). GitHub issues support multiple assignees, but `tm`'s
    /// `TicketProvider::assign` signature is exclusive-single-assignee
    /// (matching Jira), so this replaces the whole assignee list rather than
    /// adding to it: every existing assignee other than `account_id` is
    /// removed, and `account_id` is added only if not already present
    /// (avoiding a redundant add-and-remove-the-same-login round trip).
    fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), ProviderError> {
        let number = parse_issue_number(key)?;
        let info = self
            .gh
            .issue_view(&self.repo, number)
            .map_err(|err| map_issue_view_error(key, err))?;

        let already_assigned = account_id.is_some_and(|id| info.assignees.iter().any(|a| a == id));
        let remove_assignees: Vec<String> = info
            .assignees
            .into_iter()
            .filter(|a| Some(a.as_str()) != account_id)
            .collect();
        let add_assignees = if already_assigned {
            Vec::new()
        } else {
            account_id
                .map(|id| vec![id.to_string()])
                .unwrap_or_default()
        };

        if add_assignees.is_empty() && remove_assignees.is_empty() {
            return Ok(());
        }
        self.gh
            .issue_edit(
                &self.repo,
                number,
                &IssueEditRequest {
                    add_assignees,
                    remove_assignees,
                    ..Default::default()
                },
            )
            .map_err(ProviderError::from)
    }

    /// Moves `keys` to a new position in the local `ticket_rank` table
    /// relative to `anchor`, via [`compute_new_ranks`]. Requires a rank
    /// store to be attached (see [`GithubProvider::with_rank_store`]) --
    /// without one, this is [`ProviderError::Api`] naming the missing store
    /// rather than silently no-oping, since a rank request that appears to
    /// succeed but changes nothing would be a worse failure mode than a
    /// loud error.
    fn rank(&self, keys: &[String], anchor: RankAnchor) -> Result<(), ProviderError> {
        let store = self.rank_store.ok_or_else(|| ProviderError::Api {
            status: 500,
            message:
                "rank requires a local rank store, which this github provider instance has none of"
                    .to_string(),
        })?;
        let (anchor_key, before) = match &anchor {
            RankAnchor::Before(key) => (key.clone(), true),
            RankAnchor::After(key) => (key.clone(), false),
        };
        let existing = store.all_ticket_ranks().map_err(store_err)?;
        let moving: Vec<String> = keys
            .iter()
            .filter(|key| **key != anchor_key)
            .cloned()
            .collect();
        let assignments = compute_new_ranks(&existing, &moving, &anchor_key, before);
        for (key, rank) in &assignments {
            store.set_ticket_rank(key, *rank).map_err(store_err)?;
        }
        Ok(())
    }

    /// Records that `req.blocker_key` blocks `req.blocked_key` via GitHub's
    /// native issue dependencies GraphQL mutation
    /// ([`GhCli::create_issue_dependency`]).
    fn create_link(&self, req: &CreateLinkRequest) -> Result<(), ProviderError> {
        let blocker_number = parse_issue_number(&req.blocker_key)?;
        let blocked_number = parse_issue_number(&req.blocked_key)?;
        self.gh
            .create_issue_dependency(&self.repo, blocker_number, blocked_number)
            .map_err(ProviderError::from)
    }

    /// Removes a `Blocks` link by its [`link_id`]-shaped id. An id that
    /// doesn't parse (e.g. a stale phase-5-shaped id -- see [`parse_link_id`]'s
    /// doc comment) is [`ProviderError::LinkIdNotFound`], the same error a
    /// genuinely unknown id would produce.
    fn delete_link(&self, link_id: &str) -> Result<(), ProviderError> {
        let (blocker_number, blocked_number) =
            parse_link_id(link_id).ok_or_else(|| ProviderError::LinkIdNotFound {
                link_id: link_id.to_string(),
            })?;
        self.gh
            .delete_issue_dependency(&self.repo, blocker_number, blocked_number)
            .map_err(ProviderError::from)
    }

    /// Replaces `key`'s body via `gh issue edit --body` (plain Markdown,
    /// pass-through -- no ADF conversion on this path).
    fn update_description(&self, key: &str, description: &str) -> Result<(), ProviderError> {
        let number = parse_issue_number(key)?;
        self.gh
            .issue_edit(
                &self.repo,
                number,
                &IssueEditRequest {
                    body: Some(description.to_string()),
                    ..Default::default()
                },
            )
            .map_err(ProviderError::from)
    }

    /// Posts `body` as a comment on `key` (plain Markdown, pass-through).
    fn add_comment(&self, key: &str, body: &str) -> Result<(), ProviderError> {
        let number = parse_issue_number(key)?;
        self.gh
            .issue_comment(&self.repo, number, body)
            .map_err(ProviderError::from)
    }

    fn description_text(&self, issue: &Issue) -> String {
        issue
            .fields
            .description
            .as_ref()
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::{FakeGhCli, GhError};
    use crate::ticketing::types::IssueLink;

    // --- pure helpers ---

    #[test]
    fn parse_issue_number_strips_prefix() {
        assert_eq!(parse_issue_number("GH-123").unwrap(), 123);
    }

    #[test]
    fn parse_issue_number_rejects_non_numeric_suffix() {
        assert!(parse_issue_number("GH-abc").is_err());
    }

    #[test]
    fn parse_issue_number_rejects_no_dash() {
        assert!(parse_issue_number("GH123").is_err());
    }

    #[test]
    fn synthesize_status_slug_closed_is_always_done_even_with_a_status_label() {
        assert_eq!(
            synthesize_status_slug(true, &["tm:status/in-progress".to_string()]),
            "done"
        );
    }

    #[test]
    fn synthesize_status_slug_open_no_label_is_todo() {
        assert_eq!(synthesize_status_slug(false, &[]), "todo");
    }

    #[test]
    fn synthesize_status_slug_open_unknown_label_is_todo() {
        assert_eq!(
            synthesize_status_slug(false, &["enhancement".to_string()]),
            "todo"
        );
    }

    #[test]
    fn synthesize_status_slug_recognizes_each_known_label() {
        assert_eq!(
            synthesize_status_slug(false, &["tm:status/todo".to_string()]),
            "todo"
        );
        assert_eq!(
            synthesize_status_slug(false, &["tm:status/in-progress".to_string()]),
            "in-progress"
        );
        assert_eq!(
            synthesize_status_slug(false, &["tm:status/in-review".to_string()]),
            "in-review"
        );
        assert_eq!(
            synthesize_status_slug(false, &["tm:status/blocked".to_string()]),
            "blocked"
        );
    }

    #[test]
    fn synthesize_status_slug_multiple_labels_uses_priority_order() {
        assert_eq!(
            synthesize_status_slug(
                false,
                &[
                    "tm:status/todo".to_string(),
                    "tm:status/in-progress".to_string(),
                    "tm:status/blocked".to_string(),
                ]
            ),
            "blocked"
        );
        assert_eq!(
            synthesize_status_slug(
                false,
                &[
                    "tm:status/todo".to_string(),
                    "tm:status/in-review".to_string(),
                ]
            ),
            "in-review"
        );
    }

    #[test]
    fn synthesize_transitions_closed_issue_offers_only_reopen() {
        let transitions = synthesize_transitions(true, &[]);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].id, "reopen");
        assert_eq!(transitions[0].to.status_category.key, "new");
    }

    #[test]
    fn synthesize_transitions_open_issue_excludes_current_status_and_includes_done() {
        let transitions = synthesize_transitions(false, &["tm:status/in-progress".to_string()]);
        let ids: Vec<&str> = transitions.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["todo", "in-review", "blocked", "done"]);
    }

    #[test]
    fn synthesize_transitions_open_no_label_excludes_todo() {
        let transitions = synthesize_transitions(false, &[]);
        let ids: Vec<&str> = transitions.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["in-progress", "in-review", "blocked", "done"]);
    }

    #[test]
    fn transition_edit_request_todo_swaps_labels_only() {
        let req = transition_edit_request(
            "in-review",
            &["tm:status/in-progress".to_string(), "bug".to_string()],
        )
        .unwrap();
        assert_eq!(req.remove_labels, vec!["tm:status/in-progress".to_string()]);
        assert_eq!(req.add_labels, vec!["tm:status/in-review".to_string()]);
        assert_eq!(req.state, None);
    }

    #[test]
    fn transition_edit_request_done_closes_and_strips_status_labels() {
        let req = transition_edit_request("done", &["tm:status/blocked".to_string()]).unwrap();
        assert_eq!(req.remove_labels, vec!["tm:status/blocked".to_string()]);
        assert!(req.add_labels.is_empty());
        assert_eq!(req.state, Some(IssueStateChange::Close));
    }

    #[test]
    fn transition_edit_request_reopen_adds_todo_and_reopens() {
        let req = transition_edit_request("reopen", &[]).unwrap();
        assert_eq!(req.add_labels, vec!["tm:status/todo".to_string()]);
        assert_eq!(req.state, Some(IssueStateChange::Reopen));
    }

    #[test]
    fn transition_edit_request_unknown_id_is_none() {
        assert!(transition_edit_request("bogus", &[]).is_none());
    }

    // --- GithubProvider, via FakeGhCli ---

    fn issue_info(number: u64, title: &str, state: IssueState, labels: &[&str]) -> IssueInfo {
        IssueInfo {
            number,
            url: format!("https://github.com/jowi-dev/tskmstr/issues/{number}"),
            title: title.to_string(),
            body: String::new(),
            state,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            assignees: Vec::new(),
        }
    }

    #[test]
    fn get_issue_maps_open_no_label_issue_to_todo() {
        let fake = FakeGhCli::new()
            .with_issue_view(3, Ok(issue_info(3, "Fix the thing", IssueState::Open, &[])));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let issue = provider.get_issue("GH-3").unwrap();

        assert_eq!(issue.key, "GH-3");
        assert_eq!(issue.fields.summary, "Fix the thing");
        assert_eq!(issue.fields.status.name, "To Do");
        assert_eq!(issue.fields.status.status_category.key, "new");
    }

    #[test]
    fn get_issue_maps_closed_issue_to_done() {
        let fake = FakeGhCli::new().with_issue_view(
            3,
            Ok(issue_info(3, "Fix the thing", IssueState::Closed, &[])),
        );
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let issue = provider.get_issue("GH-3").unwrap();

        assert_eq!(issue.fields.status.name, "Done");
        assert_eq!(issue.fields.status.status_category.key, "done");
    }

    #[test]
    fn get_issue_unconfigured_number_is_not_found() {
        let fake = FakeGhCli::new();
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let err = provider.get_issue("GH-9").expect_err("should fail");

        assert!(matches!(err, ProviderError::NotFound { key } if key == "GH-9"));
    }

    #[test]
    fn get_issue_maps_dependencies_to_blocks_links() {
        let fake = FakeGhCli::new()
            .with_issue_view(
                3,
                Ok(issue_info(3, "Blocked ticket", IssueState::Open, &[])),
            )
            .with_issue_dependencies(
                3,
                Ok(IssueDependencies {
                    blocked_by: vec![IssueRef {
                        number: 2,
                        title: "Blocker".to_string(),
                        state: IssueState::Open,
                        url: String::new(),
                    }],
                    blocking: vec![IssueRef {
                        number: 4,
                        title: "Blocked".to_string(),
                        state: IssueState::Closed,
                        url: String::new(),
                    }],
                }),
            );
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let issue = provider.get_issue("GH-3").unwrap();

        assert_eq!(issue.fields.issue_links.len(), 2);
        let blocked_by: &IssueLink = &issue.fields.issue_links[0];
        assert_eq!(blocked_by.link_type.name, "Blocks");
        let blocker = blocked_by.inward_issue.as_ref().unwrap();
        assert_eq!(blocker.key, "GH-2");
        assert_eq!(blocker.fields.status.status_category.key, "new");

        let blocking: &IssueLink = &issue.fields.issue_links[1];
        let blocked = blocking.outward_issue.as_ref().unwrap();
        assert_eq!(blocked.key, "GH-4");
        assert_eq!(blocked.fields.status.status_category.key, "done");
    }

    #[test]
    fn get_issue_body_becomes_description_text() {
        let mut info = issue_info(3, "Fix the thing", IssueState::Open, &[]);
        info.body = "**bold** markdown".to_string();
        let fake = FakeGhCli::new().with_issue_view(3, Ok(info));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let issue = provider.get_issue("GH-3").unwrap();

        assert_eq!(provider.description_text(&issue), "**bold** markdown");
    }

    #[test]
    fn description_text_empty_when_body_is_blank() {
        let fake = FakeGhCli::new()
            .with_issue_view(3, Ok(issue_info(3, "Fix the thing", IssueState::Open, &[])));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let issue = provider.get_issue("GH-3").unwrap();

        assert_eq!(provider.description_text(&issue), "");
    }

    #[test]
    fn transitions_for_open_issue() {
        let fake = FakeGhCli::new().with_issue_view(
            3,
            Ok(issue_info(
                3,
                "T",
                IssueState::Open,
                &["tm:status/in-progress"],
            )),
        );
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let transitions = provider.transitions("GH-3").unwrap();

        let ids: Vec<&str> = transitions.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["todo", "in-review", "blocked", "done"]);
    }

    #[test]
    fn transitions_for_closed_issue() {
        let fake =
            FakeGhCli::new().with_issue_view(3, Ok(issue_info(3, "T", IssueState::Closed, &[])));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let transitions = provider.transitions("GH-3").unwrap();

        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].id, "reopen");
    }

    #[test]
    fn transition_applies_label_swap_via_issue_edit() {
        let fake = FakeGhCli::new().with_issue_view(
            3,
            Ok(issue_info(3, "T", IssueState::Open, &["tm:status/todo"])),
        );
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider.transition("GH-3", "blocked").unwrap();

        let calls = fake.issue_edit_calls();
        assert_eq!(calls.len(), 1);
        let (repo, number, req) = &calls[0];
        assert_eq!(repo, "jowi-dev/tskmstr");
        assert_eq!(*number, 3);
        assert_eq!(req.remove_labels, vec!["tm:status/todo".to_string()]);
        assert_eq!(req.add_labels, vec!["tm:status/blocked".to_string()]);
    }

    #[test]
    fn transition_done_closes_the_issue() {
        let fake = FakeGhCli::new().with_issue_view(
            3,
            Ok(issue_info(
                3,
                "T",
                IssueState::Open,
                &["tm:status/in-review"],
            )),
        );
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider.transition("GH-3", "done").unwrap();

        let calls = fake.issue_edit_calls();
        assert_eq!(calls[0].2.state, Some(IssueStateChange::Close));
    }

    #[test]
    fn transition_unknown_id_is_an_error() {
        let fake =
            FakeGhCli::new().with_issue_view(3, Ok(issue_info(3, "T", IssueState::Open, &[])));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let err = provider
            .transition("GH-3", "bogus")
            .expect_err("should fail");
        assert!(matches!(err, ProviderError::Api { .. }));
    }

    #[test]
    fn myself_returns_login_as_account_id_and_display_name() {
        let fake = FakeGhCli::new().with_current_user_login(Ok(Some("jowi-dev".to_string())));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let myself = provider.myself().unwrap();

        assert_eq!(myself.account_id, "jowi-dev");
        assert_eq!(myself.display_name, "jowi-dev");
    }

    #[test]
    fn myself_not_logged_in_is_unauthorized() {
        let fake = FakeGhCli::new().with_current_user_login(Ok(None));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let err = provider.myself().expect_err("should fail");

        assert!(matches!(err, ProviderError::Unauthorized));
    }

    #[test]
    fn assignable_users_maps_logins_to_ids_and_display_names() {
        let fake = FakeGhCli::new()
            .with_repo_assignees(Ok(vec!["jowi-dev".to_string(), "octocat".to_string()]));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let users = provider.assignable_users("ignored").unwrap();

        assert_eq!(users.len(), 2);
        assert_eq!(users[0].account_id, "jowi-dev");
        assert_eq!(users[0].display_name, "jowi-dev");
    }

    #[test]
    fn search_my_open_scopes_by_current_login() {
        let fake = FakeGhCli::new()
            .with_current_user_login(Ok(Some("jowi-dev".to_string())))
            .with_issue_list(Ok(vec![issue_info(1, "Mine", IssueState::Open, &[])]));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let result = provider.search(&TicketQuery::MyOpen).unwrap();

        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].key, "GH-1");
        let calls = fake.issue_list_calls();
        assert_eq!(calls[0].1.assignee, Some("jowi-dev".to_string()));
    }

    #[test]
    fn search_unassigned_filters_out_assigned_issues_client_side() {
        let mut assigned = issue_info(1, "Assigned", IssueState::Open, &[]);
        assigned.assignees = vec!["someone".to_string()];
        let unassigned = issue_info(2, "Unassigned", IssueState::Open, &[]);
        let fake = FakeGhCli::new().with_issue_list(Ok(vec![assigned, unassigned.clone()]));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let result = provider
            .search(&TicketQuery::Unassigned {
                project_key: "ignored".to_string(),
            })
            .unwrap();

        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].key, "GH-2");
    }

    #[test]
    fn search_ranked_sorts_ascending_by_issue_number() {
        let fake = FakeGhCli::new().with_issue_list(Ok(vec![
            issue_info(5, "Five", IssueState::Open, &[]),
            issue_info(1, "One", IssueState::Open, &[]),
            issue_info(3, "Three", IssueState::Open, &[]),
        ]));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let result = provider
            .search(&TicketQuery::Ranked {
                project_key: "ignored".to_string(),
            })
            .unwrap();

        let keys: Vec<&str> = result.issues.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["GH-1", "GH-3", "GH-5"]);
    }

    #[test]
    fn search_text_filters_by_summary_case_insensitively() {
        let fake = FakeGhCli::new().with_issue_list(Ok(vec![
            issue_info(1, "Fix login bug", IssueState::Open, &[]),
            issue_info(2, "Add a feature", IssueState::Open, &[]),
        ]));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let result = provider
            .search(&TicketQuery::Search {
                project_key: "ignored".to_string(),
                text: "LOGIN".to_string(),
            })
            .unwrap();

        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].key, "GH-1");
    }

    #[test]
    fn search_ready_candidates_filters_to_new_category_and_sorts() {
        let fake = FakeGhCli::new()
            .with_current_user_login(Ok(Some("jowi-dev".to_string())))
            .with_issue_list(Ok(vec![
                issue_info(5, "Todo five", IssueState::Open, &[]),
                issue_info(
                    1,
                    "In progress one",
                    IssueState::Open,
                    &["tm:status/in-progress"],
                ),
                issue_info(2, "Todo two", IssueState::Open, &["tm:status/todo"]),
            ]));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let result = provider.search(&TicketQuery::ReadyCandidates).unwrap();

        let keys: Vec<&str> = result.issues.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["GH-2", "GH-5"]);
    }

    #[test]
    fn create_issue_calls_issue_create_with_todo_label_and_ignores_issue_type_name() {
        let created = issue_info(9, "New", IssueState::Open, &[]);
        let fake = FakeGhCli::new().with_issue_create_result(Ok(created));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let issue = provider
            .create_issue(&NewTicket {
                project_key: "GH".to_string(),
                summary: "New".to_string(),
                description: "details".to_string(),
                issue_type_name: "Task".to_string(),
                assignee_account_id: Some("jowi-dev".to_string()),
            })
            .unwrap();

        assert_eq!(issue.key, "GH-9");
        let calls = fake.issue_create_calls();
        assert_eq!(calls.len(), 1);
        let (repo, req) = &calls[0];
        assert_eq!(repo, "jowi-dev/tskmstr");
        assert_eq!(req.title, "New");
        assert_eq!(req.body, "details");
        assert_eq!(req.labels, vec!["tm:status/todo".to_string()]);
        assert_eq!(req.assignees, vec!["jowi-dev".to_string()]);
    }

    #[test]
    fn create_issue_with_no_assignee_passes_empty_assignees() {
        let created = issue_info(9, "New", IssueState::Open, &[]);
        let fake = FakeGhCli::new().with_issue_create_result(Ok(created));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider
            .create_issue(&NewTicket {
                project_key: "GH".to_string(),
                summary: "New".to_string(),
                description: String::new(),
                issue_type_name: "Task".to_string(),
                assignee_account_id: None,
            })
            .unwrap();

        assert!(fake.issue_create_calls()[0].1.assignees.is_empty());
    }

    #[test]
    fn add_remote_link_is_a_no_op_that_succeeds() {
        let fake = FakeGhCli::new();
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider
            .add_remote_link(
                "GH-1",
                &RemoteLinkRequest {
                    url: "https://github.com/jowi-dev/tskmstr/pull/1".to_string(),
                    title: "PR #1".to_string(),
                },
            )
            .expect("should succeed as a no-op");
    }

    #[test]
    fn assign_sets_new_assignee_and_removes_prior_ones() {
        let mut info = issue_info(1, "T", IssueState::Open, &[]);
        info.assignees = vec!["someone-else".to_string()];
        let fake = FakeGhCli::new().with_issue_view(1, Ok(info));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider.assign("GH-1", Some("jowi-dev")).unwrap();

        let calls = fake.issue_edit_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2.add_assignees, vec!["jowi-dev".to_string()]);
        assert_eq!(
            calls[0].2.remove_assignees,
            vec!["someone-else".to_string()]
        );
    }

    #[test]
    fn assign_none_unassigns_everyone() {
        let mut info = issue_info(1, "T", IssueState::Open, &[]);
        info.assignees = vec!["jowi-dev".to_string()];
        let fake = FakeGhCli::new().with_issue_view(1, Ok(info));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider.assign("GH-1", None).unwrap();

        let calls = fake.issue_edit_calls();
        assert_eq!(calls[0].2.add_assignees, Vec::<String>::new());
        assert_eq!(calls[0].2.remove_assignees, vec!["jowi-dev".to_string()]);
    }

    #[test]
    fn assign_already_assigned_is_a_no_op_that_makes_no_call() {
        let mut info = issue_info(1, "T", IssueState::Open, &[]);
        info.assignees = vec!["jowi-dev".to_string()];
        let fake = FakeGhCli::new().with_issue_view(1, Ok(info));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider.assign("GH-1", Some("jowi-dev")).unwrap();

        assert!(fake.issue_edit_calls().is_empty());
    }

    #[test]
    fn update_description_edits_the_body() {
        let fake = FakeGhCli::new();
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider.update_description("GH-1", "**new** body").unwrap();

        let calls = fake.issue_edit_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2.body.as_deref(), Some("**new** body"));
    }

    #[test]
    fn add_comment_posts_markdown_as_is() {
        let fake = FakeGhCli::new();
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider.add_comment("GH-1", "**bold** comment").unwrap();

        assert_eq!(
            fake.issue_comment_calls(),
            vec![(
                "jowi-dev/tskmstr".to_string(),
                1,
                "**bold** comment".to_string()
            )]
        );
    }

    #[test]
    fn create_link_calls_create_issue_dependency_with_both_numbers() {
        let fake = FakeGhCli::new();
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider
            .create_link(&CreateLinkRequest {
                blocker_key: "GH-1".to_string(),
                blocked_key: "GH-2".to_string(),
            })
            .unwrap();

        assert_eq!(
            fake.create_issue_dependency_calls(),
            vec![("jowi-dev/tskmstr".to_string(), 1, 2)]
        );
    }

    #[test]
    fn delete_link_parses_the_id_and_calls_delete_issue_dependency() {
        let fake = FakeGhCli::new();
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider.delete_link(&link_id(1, 2)).unwrap();

        assert_eq!(
            fake.delete_issue_dependency_calls(),
            vec![("jowi-dev/tskmstr".to_string(), 1, 2)]
        );
    }

    #[test]
    fn delete_link_unparseable_id_is_link_id_not_found() {
        let fake = FakeGhCli::new();
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let err = provider
            .delete_link("not-a-link-id")
            .expect_err("should fail");

        assert!(
            matches!(err, ProviderError::LinkIdNotFound { link_id } if link_id == "not-a-link-id")
        );
    }

    #[test]
    fn link_id_round_trips_through_parse_link_id() {
        assert_eq!(parse_link_id(&link_id(1, 2)), Some((1, 2)));
    }

    #[test]
    fn get_issue_link_ids_round_trip_through_delete_link() {
        let fake = FakeGhCli::new()
            .with_issue_view(
                3,
                Ok(issue_info(3, "Blocked ticket", IssueState::Open, &[])),
            )
            .with_issue_dependencies(
                3,
                Ok(IssueDependencies {
                    blocked_by: vec![IssueRef {
                        number: 2,
                        title: "Blocker".to_string(),
                        state: IssueState::Open,
                        url: String::new(),
                    }],
                    blocking: Vec::new(),
                }),
            );
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());
        let issue = provider.get_issue("GH-3").unwrap();
        let link = &issue.fields.issue_links[0];

        provider.delete_link(&link.id).unwrap();

        // The blocker (GH-2) blocks the issue being viewed (GH-3).
        assert_eq!(
            fake.delete_issue_dependency_calls(),
            vec![("jowi-dev/tskmstr".to_string(), 2, 3)]
        );
    }

    #[test]
    fn rank_without_a_store_is_an_error() {
        let fake = FakeGhCli::new();
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let err = provider
            .rank(
                &["GH-1".to_string()],
                RankAnchor::Before("GH-2".to_string()),
            )
            .expect_err("should fail");

        assert!(matches!(err, ProviderError::Api { .. }));
    }

    #[test]
    fn rank_before_an_unranked_anchor_places_both_at_the_end() {
        let store = crate::runs::RunStore::open(std::path::Path::new(":memory:")).unwrap();
        let fake = FakeGhCli::new();
        let provider =
            GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string()).with_rank_store(&store);

        provider
            .rank(
                &["GH-1".to_string()],
                RankAnchor::Before("GH-2".to_string()),
            )
            .unwrap();

        let ranks = store.all_ticket_ranks().unwrap();
        let by_key: HashMap<String, f64> = ranks.into_iter().collect();
        assert!(by_key["GH-1"] < by_key["GH-2"]);
    }

    #[test]
    fn rank_after_places_moving_keys_between_anchor_and_successor() {
        let store = crate::runs::RunStore::open(std::path::Path::new(":memory:")).unwrap();
        store.set_ticket_rank("GH-1", 100.0).unwrap();
        store.set_ticket_rank("GH-5", 300.0).unwrap();
        let fake = FakeGhCli::new();
        let provider =
            GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string()).with_rank_store(&store);

        provider
            .rank(
                &["GH-2".to_string(), "GH-3".to_string()],
                RankAnchor::After("GH-1".to_string()),
            )
            .unwrap();

        let by_key: HashMap<String, f64> = store.all_ticket_ranks().unwrap().into_iter().collect();
        assert!(by_key["GH-1"] < by_key["GH-2"]);
        assert!(by_key["GH-2"] < by_key["GH-3"]);
        assert!(by_key["GH-3"] < by_key["GH-5"]);
    }

    #[test]
    fn compute_new_ranks_before_with_no_existing_entries() {
        let assignments = compute_new_ranks(&[], &["GH-1".to_string()], "GH-2", true);
        // GH-2 (the anchor) gets assigned first, then GH-1 goes before it.
        let by_key: HashMap<String, f64> = assignments.into_iter().collect();
        assert!(by_key["GH-1"] < by_key["GH-2"]);
    }

    #[test]
    fn compute_new_ranks_preserves_moving_key_order() {
        let existing = vec![("GH-1".to_string(), 0.0), ("GH-9".to_string(), 1000.0)];
        let assignments = compute_new_ranks(
            &existing,
            &["GH-2".to_string(), "GH-3".to_string(), "GH-4".to_string()],
            "GH-9",
            true,
        );
        let ranks: Vec<f64> = assignments.iter().map(|(_, r)| *r).collect();
        assert!(ranks.windows(2).all(|w| w[0] < w[1]));
        assert!(ranks.iter().all(|r| *r > 0.0 && *r < 1000.0));
    }

    #[test]
    fn search_ranked_uses_local_rank_when_available() {
        let store = crate::runs::RunStore::open(std::path::Path::new(":memory:")).unwrap();
        store.set_ticket_rank("GH-5", 100.0).unwrap();
        let fake = FakeGhCli::new().with_issue_list(Ok(vec![
            issue_info(1, "One", IssueState::Open, &[]),
            issue_info(5, "Five", IssueState::Open, &[]),
        ]));
        let provider =
            GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string()).with_rank_store(&store);

        let result = provider
            .search(&TicketQuery::Ranked {
                project_key: "ignored".to_string(),
            })
            .unwrap();

        // GH-5 has an explicit rank, so it sorts before GH-1 despite the
        // higher issue number.
        let keys: Vec<&str> = result.issues.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["GH-5", "GH-1"]);
    }

    #[test]
    fn get_project_ok_when_issue_list_succeeds() {
        let fake = FakeGhCli::new();
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        provider.get_project("jowi-dev/tskmstr").unwrap();
    }

    #[test]
    fn get_project_not_found_when_issue_list_fails() {
        let fake = FakeGhCli::new().with_issue_list(Err(GhError::Command {
            command: "gh issue list".to_string(),
            exit_code: Some(1),
            stderr: "repo not found".to_string(),
        }));
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let err = provider
            .get_project("jowi-dev/tskmstr")
            .expect_err("should fail");

        assert!(matches!(err, ProviderError::ProjectNotFound { .. }));
    }
}
