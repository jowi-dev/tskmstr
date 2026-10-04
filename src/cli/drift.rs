//! `tm drift [--fix]`: list tickets whose tracker status disagrees with
//! what their lane runs and PRs actually did, across every known project
//! (GitHub issue #80).
//!
//! Read-only by default. For each project this gathers the open tickets
//! (one provider search), the open PRs and every PR's lifecycle (two `gh`
//! calls), and each ticket's latest lane run from the run store, then
//! classifies every ticket with [`crate::ticketing::drift::detect_drift`].
//! `--fix` applies each finding's suggested transition through
//! [`crate::ticketing::reconcile_status`], the same advisory path `tm
//! merge` uses, so a ticket already in the target status is reported as
//! such rather than warned about.
//! [`Drift::Stalled`] never has a suggested transition: whether a stalled
//! ticket should be resumed, re-queued, or closed is a human call.
//!
//! See `docs/plans/gh-80-status-drift.md` for the drift matrix this
//! command is the safety net for.

use std::io::{self, Write};
use std::path::PathBuf;

use thiserror::Error;

use crate::config::{BackendIdentity, Config};
use crate::github::gh_cli::{GhCli, GhError, PrLifecycle, PrSummary};
use crate::github::pr::find_pr_for_ticket;
use crate::runs::{Run, RunStatus, RunStore, RunStoreError};
use crate::ticketing::drift::{Drift, DriftInput, TicketCategory, detect_drift};
use crate::ticketing::error::ProviderError;
use crate::ticketing::provider::{TicketProvider, TicketQuery};
use crate::ticketing::{StatusTransition, reconcile_status};

/// Default [`Drift::Stalled`] threshold, in hours, for `--stall-hours`.
pub const DEFAULT_STALL_HOURS: i64 = 24;

/// One project `tm drift` audits: a repo and the ticket backend its
/// effective config selects.
pub struct DriftProject<'a> {
    /// Display name, e.g. the repo directory's name.
    pub name: String,
    /// Repo root `gh` runs from.
    pub repo_dir: PathBuf,
    /// The repo's effective config (global layered with its
    /// `.tskmstr.toml`).
    pub config: &'a Config,
    /// Ticket provider for `config.backend`.
    pub provider: &'a dyn TicketProvider,
}

/// `tm drift`'s flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriftOptions<'a> {
    /// Apply each finding's suggested transition.
    pub fix: bool,
    /// [`Drift::Stalled`] threshold in hours.
    pub stall_hours: i64,
    /// Restrict listing (and `fix`) to these ticket keys, compared
    /// case-insensitively; empty means every drifted ticket. Lets an
    /// operator fix the findings they agree with and leave deliberate
    /// exceptions (a merged PR that intentionally doesn't finish its
    /// ticket) alone.
    pub keys: &'a [String],
}

/// One drifted ticket, as found by [`collect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftedTicket {
    /// Ticket key.
    pub key: String,
    /// The ticket's current tracker status name.
    pub status: String,
    /// What kind of drift it shows.
    pub drift: Drift,
    /// Status of its latest lane run, if any.
    pub run: Option<RunStatus>,
    /// Lifecycle of its PR, if one was found.
    pub pr: Option<PrLifecycle>,
    /// Status `--fix` would move it to; `None` for [`Drift::Stalled`].
    pub target: Option<String>,
}

/// Errors gathering one project's drift.
#[derive(Debug, Error)]
pub enum DriftError {
    /// Listing the project's open tickets failed.
    #[error("ticket search failed: {0}")]
    Provider(#[from] ProviderError),
    /// Listing the project's PRs failed.
    #[error("gh failed: {0}")]
    Gh(#[from] GhError),
    /// Reading the run store failed.
    #[error("run store: {0}")]
    Runs(#[from] RunStoreError),
}

/// The status `--fix` moves a ticket showing `drift` to: the configured
/// `status_on_*` key for the hook that should have fired, falling back to
/// the GitHub backend's own vocabulary (which Jira-shaped targets
/// normalize onto anyway) when the key is unset. `None` for
/// [`Drift::Stalled`].
pub fn suggested_target(drift: Drift, config: &Config) -> Option<String> {
    let (configured, fallback) = match drift {
        Drift::MergedNotDone => (&config.status_on_merge, "Done"),
        Drift::FinishedStillNew => (&config.status_on_pr, "In Review"),
        Drift::RunningStillNew => (&config.status_on_run_start, "In Progress"),
        Drift::Stalled => return None,
    };
    Some(configured.clone().unwrap_or_else(|| fallback.to_string()))
}

/// Find every drifted ticket in `project`.
///
/// Candidates are the project's open tickets ([`TicketQuery::Everyone`]):
/// every drift rule concerns a ticket that is not yet in a done-category
/// status. A ticket's PR is its open PR by key ([`find_pr_for_ticket`],
/// which also catches PRs opened outside `tm pr create`); failing that,
/// any PR (of any state) whose head branch or number matches its latest
/// lane run's recorded branch or PR URL, which is how a merged PR is
/// found. Known gap: a merged PR opened by hand from a branch no lane run
/// recorded is invisible here.
pub fn collect(
    project: &DriftProject,
    gh: &dyn GhCli,
    store: &RunStore,
    stall_hours: i64,
) -> Result<Vec<DriftedTicket>, DriftError> {
    let scope = BackendIdentity::from_config(project.config).scope();
    let issues = project
        .provider
        .search(&TicketQuery::Everyone {
            project_key: project.config.default_project_key.clone(),
        })?
        .issues;
    let open_prs = gh.pr_list(&project.repo_dir)?;
    let all_prs = gh.pr_list_all(&project.repo_dir)?;
    let accept = |token: &str| project.provider.is_ticket_key(token);

    let mut drifted = Vec::new();
    for issue in issues {
        let run = store.latest_run_for_ticket_kind(Some(&scope), &issue.key, Some("lane"))?;
        let pr = if find_pr_for_ticket(&open_prs, &issue.key, &accept).is_some() {
            Some(PrLifecycle::Open)
        } else {
            run.as_ref().and_then(|run| run_pr_lifecycle(run, &all_prs))
        };
        let input = DriftInput {
            category: TicketCategory::from_key(&issue.fields.status.status_category.key),
            run: run.as_ref().map(|run| run.status),
            run_age_hours: run.as_ref().map(|run| run.age_secs / 3600),
            pr,
        };
        if let Some(drift) = detect_drift(&input, stall_hours) {
            drifted.push(DriftedTicket {
                key: issue.key,
                status: issue.fields.status.name,
                drift,
                run: input.run,
                pr,
                target: suggested_target(drift, project.config),
            });
        }
    }
    Ok(drifted)
}

/// Group `runs` (newest first, as [`RunStore::all_runs`] returns them) into
/// one list of lane worktree paths per ticket scope: the run store is the
/// only cross-project registry tskmstr has, since lanes are configured in
/// each repo's own `.tskmstr.toml`. Scopes come out in order of their
/// newest lane run, and each list newest first and deduplicated, so a
/// caller can resolve a project's repo root from its first worktree still
/// on disk. Legacy unscoped rows (`""`) and non-lane runs are skipped.
pub fn project_worktrees(runs: &[Run]) -> Vec<Vec<String>> {
    let mut scopes: Vec<(&str, Vec<String>)> = Vec::new();
    for run in runs
        .iter()
        .filter(|run| run.kind == "lane" && !run.scope.is_empty())
    {
        let index = match scopes.iter().position(|(scope, _)| *scope == run.scope) {
            Some(index) => index,
            None => {
                scopes.push((&run.scope, Vec::new()));
                scopes.len() - 1
            }
        };
        let worktrees = &mut scopes[index].1;
        if !worktrees.contains(&run.worktree) {
            worktrees.push(run.worktree.clone());
        }
    }
    scopes.into_iter().map(|(_, worktrees)| worktrees).collect()
}

/// The lifecycle of the PR `run` opened, matched by the run's recorded
/// branch or by the number at the end of its recorded PR URL. The most
/// recently updated match wins.
fn run_pr_lifecycle(run: &Run, prs: &[PrSummary]) -> Option<PrLifecycle> {
    let number = run
        .pr_url
        .as_deref()
        .and_then(|url| url.rsplit('/').next())
        .and_then(|tail| tail.parse::<u64>().ok());
    prs.iter()
        .filter(|pr| {
            Some(pr.number) == number || run.branch.as_deref() == Some(pr.head_ref_name.as_str())
        })
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at))
        .map(|pr| pr.lifecycle)
}

/// `tm drift`: print every project's drifted tickets and, with
/// `opts.fix`, apply their suggested transitions. A project whose lookups
/// fail is reported as a warning and skipped, so one broken repo doesn't
/// hide drift in the others; the closing summary counts only the projects
/// actually checked, so a clean result says how much it covered. Returns
/// the number of drifted tickets found.
pub fn run(
    projects: &[DriftProject],
    gh: &dyn GhCli,
    store: &RunStore,
    opts: DriftOptions<'_>,
    out: &mut dyn Write,
) -> io::Result<usize> {
    let mut total = 0;
    let mut checked = 0;
    for project in projects {
        let mut drifted = match collect(project, gh, store, opts.stall_hours) {
            Ok(drifted) => drifted,
            Err(err) => {
                writeln!(out, "warning: {}: {err}", project.name)?;
                continue;
            }
        };
        checked += 1;
        if !opts.keys.is_empty() {
            drifted.retain(|t| opts.keys.iter().any(|k| k.eq_ignore_ascii_case(&t.key)));
        }
        if drifted.is_empty() {
            continue;
        }
        total += drifted.len();
        writeln!(out, "{}", project.name)?;
        for ticket in &drifted {
            print_ticket(ticket, out)?;
            if opts.fix
                && let Some(target) = &ticket.target
            {
                match reconcile_status(project.provider, &ticket.key, target) {
                    StatusTransition::Applied(status) => {
                        writeln!(out, "    moved {} to {status}", ticket.key)?
                    }
                    StatusTransition::AlreadyInStatus(status) => {
                        writeln!(out, "    {} already in {status}", ticket.key)?
                    }
                    StatusTransition::Warning(message) => writeln!(out, "    warning: {message}")?,
                }
            }
        }
    }

    match (total, opts.fix) {
        (0, _) => writeln!(
            out,
            "No drifted tickets across {checked} project(s) checked."
        )?,
        (n, false) => writeln!(
            out,
            "{n} drifted ticket(s) across {checked} project(s). Run `tm drift --fix` to apply the suggested transitions."
        )?,
        (n, true) => writeln!(out, "{n} drifted ticket(s) across {checked} project(s).")?,
    }
    Ok(total)
}

fn print_ticket(ticket: &DriftedTicket, out: &mut dyn Write) -> io::Result<()> {
    let run = ticket.run.map_or("none", RunStatus::as_str);
    let pr = match ticket.pr {
        Some(PrLifecycle::Open) => "open",
        Some(PrLifecycle::Merged) => "merged",
        Some(PrLifecycle::Closed) => "closed",
        None => "none",
    };
    let suggestion = ticket
        .target
        .as_deref()
        .map_or("needs a human".to_string(), |target| format!("-> {target}"));
    writeln!(
        out,
        "  {} [{}] {} (run: {run}, pr: {pr}) {suggestion}",
        ticket.key,
        ticket.status,
        ticket.drift.describe()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::FakeGhCli;
    use crate::github::pr::PrInfo;
    use crate::jira::fake::FakeJiraClient;
    use crate::runs::{FinishRun, StartRun};
    use crate::ticketing::types::{
        Issue, IssueFields, SearchResult, Status, StatusCategory, Transition,
    };

    fn config() -> Config {
        Config {
            backend: crate::config::BackendKind::Jira,
            jira_base_url: "https://example.atlassian.net".to_string(),
            jira_email: "ada@example.com".to_string(),
            default_project_key: "PROJ".to_string(),
            github_repo: None,
            default_assignee_account_id: None,
            status_on_pr: None,
            status_on_create: None,
            status_on_merge: None,
            status_on_run_start: None,
            run_db_path: None,
            review_bots: Vec::new(),
            board_column_order: Vec::new(),
            work: crate::config::WorkConfig::default(),
            agent: crate::config::AgentKind::Claude,
            agent_fallbacks: Vec::new(),
        }
    }

    fn issue(key: &str, status: &str, category: &str) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: format!("Summary for {key}"),
                status: Status {
                    name: status.to_string(),
                    status_category: StatusCategory {
                        key: category.to_string(),
                    },
                },
                description: None,
                assignee: None,
                issue_links: vec![],
            },
        }
    }

    fn search(issues: Vec<Issue>) -> SearchResult {
        SearchResult {
            issues,
            next_page_token: None,
        }
    }

    fn open_pr(number: u64, key: &str) -> PrInfo {
        PrInfo {
            number,
            url: String::new(),
            title: format!("{key}: work"),
            body: String::new(),
            head_ref_name: format!("me/{}-work", key.to_lowercase()),
            base_ref_name: "main".to_string(),
        }
    }

    fn summary(number: u64, branch: &str, lifecycle: PrLifecycle) -> PrSummary {
        PrSummary {
            number,
            head_ref_name: branch.to_string(),
            lifecycle,
            updated_at: "2026-10-01T00:00:00Z".to_string(),
        }
    }

    fn store() -> (tempfile::TempDir, RunStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::open(&dir.path().join("runs.db")).unwrap();
        (dir, store)
    }

    fn lane_run(store: &RunStore, key: &str, branch: &str, status: RunStatus) {
        let id = store
            .start_run(&StartRun {
                ticket: key.to_string(),
                scope: BackendIdentity::from_config(&config()).scope(),
                lane: "lane".to_string(),
                worktree: "/tmp/wt".to_string(),
                branch: Some(branch.to_string()),
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        if status != RunStatus::Running {
            store
                .finish_run(
                    id,
                    &FinishRun {
                        status,
                        exit_code: None,
                        session_id: None,
                        cost_usd: None,
                        num_turns: None,
                        blocker: None,
                        pr_url: None,
                        transcript: None,
                        model_usage: None,
                        findings_count: None,
                    },
                )
                .unwrap();
        }
    }

    fn project<'a>(config: &'a Config, provider: &'a dyn TicketProvider) -> DriftProject<'a> {
        DriftProject {
            name: "proj".to_string(),
            repo_dir: PathBuf::from("/repo"),
            config,
            provider,
        }
    }

    #[test]
    fn collect_flags_done_run_ticket_still_in_to_do() {
        let config = config();
        let jira = FakeJiraClient::new().with_search_result(search(vec![
            issue("PROJ-1", "To Do", "new"),
            issue("PROJ-2", "To Do", "new"),
        ]));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![]))
            .with_pr_list_all(Ok(vec![]));
        let (_dir, store) = store();
        lane_run(&store, "PROJ-1", "me/proj-1", RunStatus::Done);

        let drifted = collect(&project(&config, &jira), &gh, &store, 24).unwrap();

        assert_eq!(
            drifted,
            vec![DriftedTicket {
                key: "PROJ-1".to_string(),
                status: "To Do".to_string(),
                drift: Drift::FinishedStillNew,
                run: Some(RunStatus::Done),
                pr: None,
                target: Some("In Review".to_string()),
            }]
        );
    }

    #[test]
    fn collect_flags_open_pr_opened_outside_tm_with_no_run() {
        let config = config();
        let jira =
            FakeJiraClient::new().with_search_result(search(vec![issue("PROJ-3", "To Do", "new")]));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![open_pr(7, "PROJ-3")]))
            .with_pr_list_all(Ok(vec![]));
        let (_dir, store) = store();

        let drifted = collect(&project(&config, &jira), &gh, &store, 24).unwrap();

        assert_eq!(drifted.len(), 1);
        assert_eq!(drifted[0].drift, Drift::FinishedStillNew);
        assert_eq!(drifted[0].pr, Some(PrLifecycle::Open));
    }

    #[test]
    fn collect_finds_merged_pr_by_run_branch_and_uses_status_on_merge() {
        let config = Config {
            status_on_merge: Some("Shipped".to_string()),
            ..config()
        };
        let jira = FakeJiraClient::new().with_search_result(search(vec![issue(
            "PROJ-4",
            "In Progress",
            "indeterminate",
        )]));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![]))
            .with_pr_list_all(Ok(vec![
                summary(8, "me/other", PrLifecycle::Open),
                summary(9, "me/proj-4", PrLifecycle::Merged),
            ]));
        let (_dir, store) = store();
        lane_run(&store, "PROJ-4", "me/proj-4", RunStatus::Done);

        let drifted = collect(&project(&config, &jira), &gh, &store, 24).unwrap();

        assert_eq!(drifted.len(), 1);
        assert_eq!(drifted[0].drift, Drift::MergedNotDone);
        assert_eq!(drifted[0].pr, Some(PrLifecycle::Merged));
        assert_eq!(drifted[0].target, Some("Shipped".to_string()));
    }

    #[test]
    fn collect_skips_tickets_without_drift() {
        let config = config();
        let jira = FakeJiraClient::new().with_search_result(search(vec![issue(
            "PROJ-5",
            "In Progress",
            "indeterminate",
        )]));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![]))
            .with_pr_list_all(Ok(vec![]));
        let (_dir, store) = store();
        lane_run(&store, "PROJ-5", "me/proj-5", RunStatus::Running);

        let drifted = collect(&project(&config, &jira), &gh, &store, 24).unwrap();

        assert!(drifted.is_empty());
    }

    #[test]
    fn project_worktrees_groups_lane_worktrees_by_scope_newest_first() {
        let (_dir, store) = store();
        let start = |ticket: &str, scope: &str, worktree: &str, kind: &str| {
            store
                .start_run(&StartRun {
                    ticket: ticket.to_string(),
                    scope: scope.to_string(),
                    lane: "lane".to_string(),
                    worktree: worktree.to_string(),
                    branch: None,
                    pid: None,
                    kind: kind.to_string(),
                    log_path: None,
                })
                .unwrap()
        };
        start("GH-1", "github:a/one", "/wt/one/gh-1", "lane");
        start("GH-2", "github:b/two", "/wt/two/gh-2", "lane");
        start("GH-3", "github:a/one", "/wt/one/gh-3", "lane");
        start("GH-3", "github:a/one", "/wt/one/gh-3", "lane");
        start("GH-4", "github:a/one", "/wt/one/audit", "audit");
        start("GH-5", "", "/wt/legacy", "lane");
        // Same-second starts tie on started_at; ids break the tie.
        let runs = store.all_runs().unwrap();

        assert_eq!(
            project_worktrees(&runs),
            vec![
                vec!["/wt/one/gh-3".to_string(), "/wt/one/gh-1".to_string()],
                vec!["/wt/two/gh-2".to_string()],
            ]
        );
    }

    #[test]
    fn suggested_target_prefers_configured_keys_and_never_fixes_stalled() {
        let config = Config {
            status_on_pr: Some("Code Review".to_string()),
            status_on_run_start: Some("Doing".to_string()),
            ..config()
        };
        assert_eq!(
            suggested_target(Drift::FinishedStillNew, &config).as_deref(),
            Some("Code Review")
        );
        assert_eq!(
            suggested_target(Drift::RunningStillNew, &config).as_deref(),
            Some("Doing")
        );
        assert_eq!(
            suggested_target(Drift::MergedNotDone, &config).as_deref(),
            Some("Done")
        );
        assert_eq!(suggested_target(Drift::Stalled, &config), None);
    }

    #[test]
    fn run_is_read_only_without_fix() {
        let config = config();
        let jira =
            FakeJiraClient::new().with_search_result(search(vec![issue("PROJ-1", "To Do", "new")]));
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![]))
            .with_pr_list_all(Ok(vec![]));
        let (_dir, store) = store();
        lane_run(&store, "PROJ-1", "me/proj-1", RunStatus::Done);
        let mut out = Vec::new();

        let total = run(
            &[project(&config, &jira)],
            &gh,
            &store,
            DriftOptions {
                fix: false,
                stall_hours: 24,
                keys: &[],
            },
            &mut out,
        )
        .unwrap();

        assert_eq!(total, 1);
        assert!(jira.transition_calls().is_empty());
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "proj\n  PROJ-1 [To Do] work finished, ticket still not started \
             (run: done, pr: none) -> In Review\n\
             1 drifted ticket(s) across 1 project(s). Run `tm drift --fix` to apply the suggested transitions.\n"
        );
    }

    #[test]
    fn run_restricts_listing_and_fix_to_given_keys() {
        let config = config();
        let jira = FakeJiraClient::new()
            .with_search_result(search(vec![
                issue("PROJ-1", "To Do", "new"),
                issue("PROJ-2", "To Do", "new"),
            ]))
            .with_issue("PROJ-2", issue("PROJ-2", "To Do", "new"))
            .with_transitions(
                "PROJ-2",
                vec![Transition {
                    id: "21".to_string(),
                    name: "Review".to_string(),
                    to: Status {
                        name: "In Review".to_string(),
                        status_category: StatusCategory {
                            key: "indeterminate".to_string(),
                        },
                    },
                }],
            );
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![]))
            .with_pr_list_all(Ok(vec![]));
        let (_dir, store) = store();
        lane_run(&store, "PROJ-1", "me/proj-1", RunStatus::Done);
        lane_run(&store, "PROJ-2", "me/proj-2", RunStatus::Done);
        let mut out = Vec::new();

        let total = run(
            &[project(&config, &jira)],
            &gh,
            &store,
            DriftOptions {
                fix: true,
                stall_hours: 24,
                keys: &["proj-2".to_string()],
            },
            &mut out,
        )
        .unwrap();

        assert_eq!(total, 1);
        assert_eq!(
            jira.transition_calls(),
            vec![("PROJ-2".to_string(), "21".to_string())]
        );
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("PROJ-1"), "{out}");
    }

    #[test]
    fn run_with_fix_applies_suggested_transition() {
        let config = config();
        let jira = FakeJiraClient::new()
            .with_search_result(search(vec![issue("PROJ-1", "To Do", "new")]))
            .with_issue("PROJ-1", issue("PROJ-1", "To Do", "new"))
            .with_transitions(
                "PROJ-1",
                vec![Transition {
                    id: "21".to_string(),
                    name: "Review".to_string(),
                    to: Status {
                        name: "In Review".to_string(),
                        status_category: StatusCategory {
                            key: "indeterminate".to_string(),
                        },
                    },
                }],
            );
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![]))
            .with_pr_list_all(Ok(vec![]));
        let (_dir, store) = store();
        lane_run(&store, "PROJ-1", "me/proj-1", RunStatus::Review);
        let mut out = Vec::new();

        run(
            &[project(&config, &jira)],
            &gh,
            &store,
            DriftOptions {
                fix: true,
                stall_hours: 24,
                keys: &[],
            },
            &mut out,
        )
        .unwrap();

        assert_eq!(
            jira.transition_calls(),
            vec![("PROJ-1".to_string(), "21".to_string())]
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("    moved PROJ-1 to In Review\n")
        );
    }

    #[test]
    fn run_reports_a_failing_project_and_continues() {
        let config = config();
        let jira = FakeJiraClient::new().with_search_error(500, "boom");
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![]))
            .with_pr_list_all(Ok(vec![]));
        let (_dir, store) = store();
        let mut out = Vec::new();

        let total = run(
            &[project(&config, &jira)],
            &gh,
            &store,
            DriftOptions {
                fix: false,
                stall_hours: 24,
                keys: &[],
            },
            &mut out,
        )
        .unwrap();

        assert_eq!(total, 0);
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.starts_with("warning: proj: ticket search failed:"),
            "{out}"
        );
        assert!(out.ends_with("No drifted tickets across 0 project(s) checked.\n"));
    }
}
