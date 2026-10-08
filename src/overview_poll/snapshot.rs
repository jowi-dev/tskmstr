//! One scope's tracker and PR state, and the batched fetch that builds it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::github::gh_cli::GhCli;
use crate::github::pr::find_issue_key;
use crate::runs::{RunStatus, ScopeActivity};
use crate::ticketing::drift::{Drift, DriftInput, TicketCategory, detect_drift};
use crate::ticketing::provider::{TicketProvider, TicketQuery};
use crate::ticketing::ready_tickets;

use super::signal::{PrSignal, pr_signal};

/// How many "Not started" tickets a scope contributes at most (ADR-0009
/// decision 4): the overview shows the top of the ready list, never the
/// backlog.
pub const READY_CAP: usize = 5;

/// One scope to poll: where its repo lives and which tickets it tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeTarget {
    /// The ticket scope (see [`crate::runs::StartRun::scope`]).
    pub scope: String,
    /// The repo's main checkout, which `gh` runs from.
    pub repo_root: PathBuf,
    /// Tickets with live or recent runs, whose tracker status is fetched.
    pub tracked: Vec<String>,
}

/// Turn [`crate::runs::RunStore::recent_scope_activity`] into poll targets,
/// resolving each scope's repo root from its newest worktree that
/// `resolve_root` can resolve. Returns the targets and the scopes no
/// worktree resolved for; those keep their runs-only rows and are never
/// polled.
pub fn targets_from_activity(
    activity: &[ScopeActivity],
    resolve_root: &dyn Fn(&str) -> Option<PathBuf>,
) -> (Vec<ScopeTarget>, Vec<String>) {
    let mut targets = Vec::new();
    let mut unresolved = Vec::new();
    for entry in activity {
        match entry.worktrees.iter().find_map(|w| resolve_root(w)) {
            Some(repo_root) => targets.push(ScopeTarget {
                scope: entry.scope.clone(),
                repo_root,
                tracked: entry.tickets.clone(),
            }),
            None => unresolved.push(entry.scope.clone()),
        }
    }
    (targets, unresolved)
}

/// One open PR, as the overview needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStatus {
    /// PR number.
    pub number: u64,
    /// PR web URL.
    pub url: String,
    /// The stage it puts its ticket in.
    pub signal: PrSignal,
}

/// A tracked ticket's tracker status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerStatus {
    /// Status name, e.g. `In Progress`.
    pub name: String,
    /// Its status category.
    pub category: TicketCategory,
}

/// A ready ticket with no run, for the "Not started" stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyTicket {
    /// Ticket key.
    pub key: String,
    /// One-line summary.
    pub summary: String,
}

/// The last good fetch of one scope. Keys are stored uppercased; look them
/// up through the accessors, which normalize.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeSnapshot {
    /// Open PRs by ticket key.
    pub prs: HashMap<String, PrStatus>,
    /// Tracker status of each tracked ticket.
    pub statuses: HashMap<String, TrackerStatus>,
    /// The top of the operator's ready list, minus tracked tickets, in
    /// rank order, at most [`READY_CAP`].
    pub ready: Vec<ReadyTicket>,
}

impl ScopeSnapshot {
    /// The open PR for `key`, if any.
    pub fn pr(&self, key: &str) -> Option<&PrStatus> {
        self.prs.get(&key.to_uppercase())
    }

    /// `key`'s tracker status, if fetched.
    pub fn status(&self, key: &str) -> Option<&TrackerStatus> {
        self.statuses.get(&key.to_uppercase())
    }

    /// Status drift for `key` given its latest lane run (#80), for the
    /// overview's Stuck stage. `None` when its tracker status wasn't
    /// fetched. Only open PRs are in the snapshot, so
    /// [`Drift::MergedNotDone`] is never reported here; `tm drift` remains
    /// the full audit.
    pub fn drift(
        &self,
        key: &str,
        run: Option<RunStatus>,
        run_age_hours: Option<i64>,
        stall_hours: i64,
    ) -> Option<Drift> {
        let status = self.status(key)?;
        let input = DriftInput {
            category: status.category,
            run,
            run_age_hours,
            pr: self
                .pr(key)
                .map(|_| crate::github::gh_cli::PrLifecycle::Open),
        };
        detect_drift(&input, stall_hours)
    }
}

/// Errors fetching one scope. Any one of its calls failing fails the
/// scope, which keeps its last good snapshot and is marked stale.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// `gh pr list` failed.
    #[error("gh: {0}")]
    Gh(#[from] crate::github::gh_cli::GhError),
    /// The tracker search failed.
    #[error("tracker: {0}")]
    Provider(#[from] crate::ticketing::error::ProviderError),
    /// The ready-list query failed.
    #[error("ready list: {0}")]
    Ready(#[from] crate::ticketing::TicketingError),
}

/// Fetch `target`'s snapshot: one `gh pr list` with review state, one
/// [`TicketQuery::Keys`] search for the tracked tickets (skipped when there
/// are none), and the `tm ready` query, capped at [`READY_CAP`]. Never one
/// call per ticket.
pub fn fetch_scope(
    target: &ScopeTarget,
    gh: &dyn GhCli,
    provider: &dyn TicketProvider,
    gh_timeout: Duration,
) -> Result<ScopeSnapshot, FetchError> {
    let accept = |token: &str| provider.is_ticket_key(token);
    let mut snapshot = ScopeSnapshot::default();
    for state in gh.pr_list_review_state(&target.repo_root, gh_timeout)? {
        if let Some(key) = find_issue_key(&state.pr, &accept) {
            snapshot.prs.insert(
                key.to_uppercase(),
                PrStatus {
                    number: state.pr.number,
                    url: state.pr.url.clone(),
                    signal: pr_signal(&state),
                },
            );
        }
    }

    if !target.tracked.is_empty() {
        let issues = provider
            .search(&TicketQuery::Keys {
                keys: target.tracked.clone(),
            })?
            .issues;
        for issue in issues {
            snapshot.statuses.insert(
                issue.key.to_uppercase(),
                TrackerStatus {
                    category: TicketCategory::from_key(&issue.fields.status.status_category.key),
                    name: issue.fields.status.name,
                },
            );
        }
    }

    let tracked: Vec<String> = target.tracked.iter().map(|k| k.to_uppercase()).collect();
    snapshot.ready = ready_tickets(provider)?
        .ready
        .into_iter()
        .filter(|issue| !tracked.contains(&issue.key.to_uppercase()))
        .take(READY_CAP)
        .map(|issue| ReadyTicket {
            key: issue.key,
            summary: issue.fields.summary,
        })
        .collect();
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::{
        ChecksState, FakeGhCli, GhError, IssueInfo, IssueState, PrReviewState, ReviewDecision,
    };
    use crate::github::pr::PrInfo;
    use crate::ticketing::github_provider::GithubProvider;

    fn issue(number: u64, labels: &[&str]) -> IssueInfo {
        IssueInfo {
            number,
            url: String::new(),
            title: format!("Issue {number}"),
            body: String::new(),
            state: IssueState::Open,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            assignees: vec!["me".to_string()],
        }
    }

    fn approved_pr(title: &str) -> PrReviewState {
        PrReviewState {
            pr: PrInfo {
                number: 40,
                url: "https://github.com/o/r/pull/40".to_string(),
                title: title.to_string(),
                body: String::new(),
                head_ref_name: "me/some-branch".to_string(),
                base_ref_name: "main".to_string(),
            },
            review_decision: ReviewDecision::Approved,
            merge_state_status: "CLEAN".to_string(),
            checks: ChecksState::Passing,
        }
    }

    fn target(tracked: &[&str]) -> ScopeTarget {
        ScopeTarget {
            scope: "github:o/r".to_string(),
            repo_root: PathBuf::from("/repo"),
            tracked: tracked.iter().map(|k| k.to_string()).collect(),
        }
    }

    #[test]
    fn fetch_scope_joins_prs_statuses_and_capped_ready_list() {
        let gh = FakeGhCli::new()
            .with_current_user_login(Ok(Some("me".to_string())))
            .with_pr_list_review_state(Ok(vec![approved_pr("GH-1: the thing")]))
            .with_issue_list(Ok((1..=9)
                .map(|n| {
                    if n == 1 {
                        issue(n, &["tm:status/in-progress"])
                    } else {
                        issue(n, &[])
                    }
                })
                .collect()));
        let provider = GithubProvider::new(&gh, "o/r".to_string());

        let snapshot = fetch_scope(
            &target(&["GH-1", "GH-2"]),
            &gh,
            &provider,
            Duration::from_secs(5),
        )
        .unwrap();

        assert_eq!(
            snapshot.pr("gh-1"),
            Some(&PrStatus {
                number: 40,
                url: "https://github.com/o/r/pull/40".to_string(),
                signal: PrSignal::ReadyToMerge,
            })
        );
        assert_eq!(
            snapshot.status("GH-1").map(|s| s.category),
            Some(TicketCategory::InProgress)
        );
        assert_eq!(
            snapshot.status("GH-2").map(|s| s.category),
            Some(TicketCategory::New)
        );
        assert_eq!(snapshot.status("GH-3"), None, "only tracked keys");
        assert_eq!(
            snapshot
                .ready
                .iter()
                .map(|t| t.key.as_str())
                .collect::<Vec<_>>(),
            ["GH-3", "GH-4", "GH-5", "GH-6", "GH-7"],
            "tracked tickets are dropped and the list is capped"
        );
        assert_eq!(gh.pr_list_review_state_calls(), [PathBuf::from("/repo")]);
    }

    #[test]
    fn fetch_scope_skips_the_keys_search_when_nothing_is_tracked() {
        let gh = FakeGhCli::new().with_current_user_login(Ok(Some("me".to_string())));
        let provider = GithubProvider::new(&gh, "o/r".to_string());

        fetch_scope(&target(&[]), &gh, &provider, Duration::from_secs(5)).unwrap();

        assert_eq!(gh.issue_list_calls().len(), 1, "only the ready-list query");
    }

    #[test]
    fn fetch_scope_fails_when_the_pr_list_fails() {
        let gh = FakeGhCli::new().with_pr_list_review_state(Err(GhError::Timeout {
            command: "gh pr list".to_string(),
            seconds: 5,
        }));
        let provider = GithubProvider::new(&gh, "o/r".to_string());

        let result = fetch_scope(&target(&["GH-1"]), &gh, &provider, Duration::from_secs(5));

        assert!(matches!(result, Err(FetchError::Gh(_))));
    }

    #[test]
    fn targets_resolve_each_scope_from_its_newest_resolvable_worktree() {
        let activity = vec![
            ScopeActivity {
                scope: "github:o/a".to_string(),
                tickets: vec!["GH-1".to_string()],
                worktrees: vec!["/wt/gone".to_string(), "/wt/a".to_string()],
            },
            ScopeActivity {
                scope: "github:o/b".to_string(),
                tickets: vec!["GH-2".to_string()],
                worktrees: vec!["/wt/also-gone".to_string()],
            },
        ];
        let resolve = |worktree: &str| (worktree == "/wt/a").then(|| PathBuf::from("/repo/a"));

        let (targets, unresolved) = targets_from_activity(&activity, &resolve);

        assert_eq!(
            targets,
            vec![ScopeTarget {
                scope: "github:o/a".to_string(),
                repo_root: PathBuf::from("/repo/a"),
                tracked: vec!["GH-1".to_string()],
            }]
        );
        assert_eq!(unresolved, ["github:o/b"]);
    }

    #[test]
    fn drift_uses_the_snapshot_status_and_open_pr() {
        let mut snapshot = ScopeSnapshot::default();
        snapshot.statuses.insert(
            "GH-1".to_string(),
            TrackerStatus {
                name: "To Do".to_string(),
                category: TicketCategory::New,
            },
        );

        assert_eq!(
            snapshot.drift("gh-1", Some(RunStatus::Running), Some(1), 24),
            Some(Drift::RunningStillNew)
        );
        assert_eq!(
            snapshot.drift("GH-9", Some(RunStatus::Running), Some(1), 24),
            None,
            "unknown status reports nothing"
        );
    }
}
