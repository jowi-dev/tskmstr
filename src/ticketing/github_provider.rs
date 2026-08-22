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
//! Every write-path method ([`TicketProvider::create_issue`],
//! [`TicketProvider::add_remote_link`], [`TicketProvider::assign`],
//! [`TicketProvider::rank`], [`TicketProvider::create_link`],
//! [`TicketProvider::delete_link`], [`TicketProvider::update_description`],
//! [`TicketProvider::add_comment`]) is a stub returning
//! [`not_yet_implemented`]'s [`ProviderError::Api`] — phase 6's job. That's
//! eight stubs out of sixteen trait methods, past the "two or three" the
//! carry-forward decisions doc named as the threshold for splitting
//! [`TicketProvider`] into narrower capability traits; phase 6 should weigh
//! that split rather than adding a ninth.

use crate::github::gh_cli::{
    GhCli, IssueDependencies, IssueEditRequest, IssueInfo, IssueListFilter, IssueListState,
    IssueRef, IssueState, IssueStateChange,
};
use crate::jira::client::RankAnchor;
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
}

impl<'a> GithubProvider<'a> {
    /// Build a provider against `repo` (an `"owner/name"` slug). Keys are
    /// `GH-<number>`; a configurable prefix (per GitHub issue #3's design)
    /// isn't wired up yet, so `key_prefix` is always `"GH"`.
    pub fn new(gh: &'a dyn GhCli, repo: String) -> Self {
        Self {
            gh,
            repo,
            key_prefix: "GH".to_string(),
        }
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

    fn to_issue(&self, key: &str, info: IssueInfo, deps: IssueDependencies) -> Issue {
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
                issue_links: self.to_issue_links(deps),
            },
        }
    }

    fn to_issue_links(&self, deps: IssueDependencies) -> Vec<IssueLink> {
        let link_type = || IssueLinkType {
            name: "Blocks".to_string(),
            inward: "is blocked by".to_string(),
            outward: "blocks".to_string(),
        };
        let mut links = Vec::with_capacity(deps.blocked_by.len() + deps.blocking.len());
        for dep in deps.blocked_by {
            let number = dep.number;
            links.push(IssueLink {
                id: format!("gh-dep-blocked-by-{number}"),
                link_type: link_type(),
                inward_issue: Some(self.to_linked_issue(dep)),
                outward_issue: None,
            });
        }
        for dep in deps.blocking {
            let number = dep.number;
            links.push(IssueLink {
                id: format!("gh-dep-blocking-{number}"),
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
                let key = self.issue_key(info.number);
                self.to_issue(&key, info, IssueDependencies::default())
            })
            .collect())
    }
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

/// Build the [`ProviderError`] for a write-path method not implemented until
/// phase 6.
fn not_yet_implemented(method: &str) -> ProviderError {
    ProviderError::Api {
        status: 501,
        message: format!(
            "{method} is not yet implemented for the github backend \
             (see docs/plans/github-issues-backend.md, phase 6)"
        ),
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
            .map_err(|_| ProviderError::NotFound {
                key: key.to_string(),
            })?;
        let deps = self
            .gh
            .issue_dependencies(&self.repo, number)
            .unwrap_or_default();
        Ok(self.to_issue(key, info, deps))
    }

    fn create_issue(&self, _req: &NewTicket) -> Result<Issue, ProviderError> {
        Err(not_yet_implemented("create_issue"))
    }

    fn add_remote_link(&self, _key: &str, _link: &RemoteLinkRequest) -> Result<(), ProviderError> {
        Err(not_yet_implemented("add_remote_link"))
    }

    fn transitions(&self, key: &str) -> Result<Vec<Transition>, ProviderError> {
        let number = parse_issue_number(key)?;
        let info = self
            .gh
            .issue_view(&self.repo, number)
            .map_err(|_| ProviderError::NotFound {
                key: key.to_string(),
            })?;
        let closed = matches!(info.state, IssueState::Closed);
        Ok(synthesize_transitions(closed, &info.labels))
    }

    fn transition(&self, key: &str, transition_id: &str) -> Result<(), ProviderError> {
        let number = parse_issue_number(key)?;
        let info = self
            .gh
            .issue_view(&self.repo, number)
            .map_err(|_| ProviderError::NotFound {
                key: key.to_string(),
            })?;
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
                // No local rank table yet (deferred to phase 6 -- see the
                // module doc comment and docs/plans/github-issues-backend.md's
                // phase 5 report); unranked issues sort by issue number
                // ascending, matching the design doc's "unranked issues
                // falling to the end by issue number" for the one-sided case
                // where nothing is ranked at all.
                let mut issues = self.list_and_map(IssueListFilter {
                    state: IssueListState::Open,
                    ..Default::default()
                })?;
                issues.sort_by_key(|issue| parse_issue_number(&issue.key).unwrap_or(u64::MAX));
                issues
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
                let mut issues: Vec<Issue> = self
                    .list_and_map(IssueListFilter {
                        state: IssueListState::Open,
                        assignee: Some(login),
                        ..Default::default()
                    })?
                    .into_iter()
                    .filter(|issue| issue.fields.status.status_category.key == "new")
                    .collect();
                issues.sort_by_key(|issue| parse_issue_number(&issue.key).unwrap_or(u64::MAX));
                issues
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

    fn assign(&self, _key: &str, _account_id: Option<&str>) -> Result<(), ProviderError> {
        Err(not_yet_implemented("assign"))
    }

    fn rank(&self, _keys: &[String], _anchor: RankAnchor) -> Result<(), ProviderError> {
        Err(not_yet_implemented("rank"))
    }

    fn create_link(&self, _req: &CreateLinkRequest) -> Result<(), ProviderError> {
        Err(not_yet_implemented("create_link"))
    }

    fn delete_link(&self, _link_id: &str) -> Result<(), ProviderError> {
        Err(not_yet_implemented("delete_link"))
    }

    fn update_description(&self, _key: &str, _description: &str) -> Result<(), ProviderError> {
        Err(not_yet_implemented("update_description"))
    }

    fn add_comment(&self, _key: &str, _body: &str) -> Result<(), ProviderError> {
        Err(not_yet_implemented("add_comment"))
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
    fn create_issue_is_not_yet_implemented() {
        let fake = FakeGhCli::new();
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        let err = provider
            .create_issue(&NewTicket {
                project_key: "GH".to_string(),
                summary: "New".to_string(),
                description: "".to_string(),
                issue_type_name: "Task".to_string(),
                assignee_account_id: None,
            })
            .expect_err("should fail");

        assert!(matches!(err, ProviderError::Api { status: 501, .. }));
    }

    #[test]
    fn every_write_path_stub_returns_a_distinct_not_implemented_error() {
        let fake = FakeGhCli::new();
        let provider = GithubProvider::new(&fake, "jowi-dev/tskmstr".to_string());

        assert!(matches!(
            provider.add_remote_link(
                "GH-1",
                &RemoteLinkRequest {
                    url: "u".to_string(),
                    title: "t".to_string()
                }
            ),
            Err(ProviderError::Api { status: 501, .. })
        ));
        assert!(matches!(
            provider.assign("GH-1", None),
            Err(ProviderError::Api { status: 501, .. })
        ));
        assert!(matches!(
            provider.rank(&[], RankAnchor::Before("GH-2".to_string())),
            Err(ProviderError::Api { status: 501, .. })
        ));
        assert!(matches!(
            provider.create_link(&CreateLinkRequest {
                blocker_key: "GH-1".to_string(),
                blocked_key: "GH-2".to_string()
            }),
            Err(ProviderError::Api { status: 501, .. })
        ));
        assert!(matches!(
            provider.delete_link("gh-dep-1"),
            Err(ProviderError::Api { status: 501, .. })
        ));
        assert!(matches!(
            provider.update_description("GH-1", "text"),
            Err(ProviderError::Api { status: 501, .. })
        ));
        assert!(matches!(
            provider.add_comment("GH-1", "text"),
            Err(ProviderError::Api { status: 501, .. })
        ));
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
