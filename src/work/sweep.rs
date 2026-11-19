//! Mark-and-sweep cleanup planning (GitHub issue #253).
//!
//! `tm work clean KEY` finishes one ticket at a time. A sweep finishes many:
//! the operator marks tickets (by key, or in bulk by PR state), sees one
//! confirmation listing every marked ticket with its kill-safety tier and
//! any reason it will be skipped, and then each unskipped ticket goes
//! through the same `clean` path single cleanup uses.
//!
//! This module is the planning half: [`plan_sweep`] turns a
//! [`SweepSelection`] into one [`SweepCandidate`] per ticket, deciding the
//! skips up front so the confirmation shows exactly what will happen. It
//! removes nothing itself. The executing half lives with `clean` in
//! `crate::cli::work::sweep`, so a sweep never grows a second removal path.
//!
//! # Skips
//!
//! A candidate is skipped, never cleaned, when:
//!
//! - its session is a project root session ([`KillSafetyTier::RootSession`]),
//! - a run is live in its session ([`KillSafetyTier::LiveRun`]),
//! - its worktree has uncommitted changes, or its `git status` can't be read.
//!
//! An audit run's checkout needs no skip of its own: the `removable`
//! dependency is `clean`'s own worktree guard, which never yields a path
//! outside the worktree root, so such a ticket is planned with no worktree.
//!
//! # One `gh pr list` per repository
//!
//! Every PR lookup — the bulk mark's PR state and the kill-safety tier —
//! goes through one cache keyed by repository root, so a sweep over many
//! tickets of one repo shells out to `gh` once rather than once per ticket.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::BackendIdentity;
use crate::github::gh_cli::{GhCli, PrLifecycle, PrSummary};
use crate::runs::{Run, RunStore, RunStoreError};
use crate::work::git::GitOps;
use crate::work::kill_safety::{KillSafetyTier, KillSafetyVerdict, classify_session_with};
use crate::work::naming::ticket_session_name;
use crate::work::tmux::TmuxOps;

/// Which tickets to sweep. The three parts are unioned: explicit keys plus
/// every ticket whose PR is merged and/or closed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepSelection {
    /// Ticket keys marked by hand. Always planned, even when there is
    /// nothing left to clean for them.
    pub keys: Vec<String>,
    /// Mark every ticket of this scope whose PR is merged.
    pub merged: bool,
    /// Mark every ticket of this scope whose PR was closed without merging.
    pub closed: bool,
}

impl SweepSelection {
    /// Whether the selection marks nothing at all.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && !self.merged && !self.closed
    }
}

/// Why a planned ticket will not be cleaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The ticket's session is a project root session.
    RootSession,
    /// A run is live in the ticket's session.
    LiveRun,
    /// The ticket's worktree has uncommitted changes.
    DirtyWorktree,
    /// `git status` failed in the ticket's worktree, so tm cannot tell
    /// whether removing it would lose work.
    StatusUnreadable(String),
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::RootSession => write!(f, "root session"),
            SkipReason::LiveRun => write!(f, "live run"),
            SkipReason::DirtyWorktree => write!(f, "uncommitted changes"),
            SkipReason::StatusUnreadable(err) => write!(f, "cannot read git status: {err}"),
        }
    }
}

/// The ticket's PR as the sweep sees it: the newest branch's PR, open
/// winning over merged winning over closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TicketPr {
    /// The PR's number.
    pub number: u64,
    /// The PR's state.
    pub lifecycle: PrLifecycle,
}

/// One marked ticket and what the sweep will do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepCandidate {
    /// The ticket key, uppercased.
    pub ticket: String,
    /// The ticket's `tm-<scope>-<key>` session name.
    pub session: String,
    /// The lane worktree `clean` would remove, when one is recorded.
    pub worktree: Option<PathBuf>,
    /// The ticket's PR, when one was found.
    pub pr: Option<TicketPr>,
    /// The session's kill-safety classification.
    pub verdict: KillSafetyVerdict,
    /// Why this ticket will be skipped; `None` means it will be cleaned.
    pub skip: Option<SkipReason>,
}

/// `clean`'s worktree guard as [`plan_sweep`] consumes it: given a
/// ticket's runs (oldest first), the `(repo_root, worktree)` pair `clean`
/// may remove, or `None`.
pub type RemovableWorktree<'a> = dyn Fn(&[Run]) -> Option<(PathBuf, PathBuf)> + 'a;

/// What [`plan_sweep`] reads.
pub struct SweepPlanDeps<'a> {
    /// The run store: which tickets exist, and their worktrees and branches.
    pub store: &'a RunStore,
    /// tmux, for live sessions and root-session targets.
    pub tmux: &'a dyn TmuxOps,
    /// gh, for PR state.
    pub gh: &'a dyn GhCli,
    /// git, for repository roots and worktree cleanliness.
    pub git: &'a dyn GitOps,
    /// Process liveness probe for recorded pids.
    pub pid_alive: &'a dyn Fn(u32) -> bool,
    /// The scope being swept; only its tickets are marked in bulk.
    pub identity: &'a BackendIdentity,
    /// `clean`'s worktree guard: the `(repo_root, worktree)` it may remove
    /// for a ticket's runs (oldest first), or `None`.
    pub removable: &'a RemovableWorktree<'a>,
}

/// Plan a sweep: one [`SweepCandidate`] per marked ticket, explicit keys
/// first in the order given, then bulk-marked tickets sorted by key.
///
/// A bulk mark only picks tickets with something left to clean: a recorded
/// worktree still on disk, or a running session. Only a [`RunStoreError`]
/// fails the plan; tmux, gh, and git errors degrade to a conservative
/// tier or a skip.
pub fn plan_sweep(
    deps: &SweepPlanDeps<'_>,
    selection: &SweepSelection,
) -> Result<Vec<SweepCandidate>, RunStoreError> {
    let scope = deps.identity.scope();
    let slug = deps.identity.session_slug();
    let live_sessions: HashSet<String> = deps
        .tmux
        .list_sessions()
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.name)
        .collect();
    let mut prs = PrCache::new(deps.gh, deps.git);

    let mut tickets: Vec<String> = Vec::new();
    for key in &selection.keys {
        let key = key.to_uppercase();
        if !tickets.contains(&key) {
            tickets.push(key);
        }
    }

    if selection.merged || selection.closed {
        let scoped: BTreeSet<String> = deps
            .store
            .all_runs()?
            .into_iter()
            .filter(|run| run.scope == scope)
            .map(|run| run.ticket)
            .collect();
        for ticket in scoped {
            if tickets.contains(&ticket) {
                continue;
            }
            let runs = deps.store.runs_for_ticket(Some(&scope), &ticket)?;
            let removable = (deps.removable)(&runs);
            let has_worktree = removable.as_ref().is_some_and(|(_, wt)| wt.exists());
            let session = ticket_session_name(&slug, &ticket);
            if !has_worktree && !live_sessions.contains(&session) {
                continue;
            }
            let wanted = match ticket_pr(&runs, removable.as_ref(), &mut prs) {
                Some(pr) => match pr.lifecycle {
                    PrLifecycle::Merged => selection.merged,
                    PrLifecycle::Closed => selection.closed,
                    PrLifecycle::Open => false,
                },
                None => false,
            };
            if wanted {
                tickets.push(ticket);
            }
        }
    }

    tickets
        .into_iter()
        .map(|ticket| plan_ticket(deps, &scope, &slug, ticket, &mut prs))
        .collect()
}

/// Plan one ticket: its worktree, PR, tier, and skip.
fn plan_ticket(
    deps: &SweepPlanDeps<'_>,
    scope: &str,
    slug: &str,
    ticket: String,
    prs: &mut PrCache<'_>,
) -> Result<SweepCandidate, RunStoreError> {
    let session = ticket_session_name(slug, &ticket);
    let runs = deps.store.runs_for_ticket(Some(scope), &ticket)?;
    let removable = (deps.removable)(&runs);
    let pr = ticket_pr(&runs, removable.as_ref(), prs);
    let verdict = classify_session_with(
        &session,
        deps.store,
        deps.tmux,
        &mut |dir| prs.list(dir),
        deps.pid_alive,
    )?;
    let worktree = removable.map(|(_, wt)| wt);

    let skip = match verdict.tier {
        KillSafetyTier::RootSession => Some(SkipReason::RootSession),
        KillSafetyTier::LiveRun => Some(SkipReason::LiveRun),
        KillSafetyTier::Safe | KillSafetyTier::Unknown => match &worktree {
            Some(wt) if wt.exists() => match deps.git.status_is_clean(wt) {
                Ok(true) => None,
                Ok(false) => Some(SkipReason::DirtyWorktree),
                Err(err) => Some(SkipReason::StatusUnreadable(err.to_string())),
            },
            _ => None,
        },
    };

    Ok(SweepCandidate {
        ticket,
        session,
        worktree,
        pr,
        verdict,
        skip,
    })
}

/// The ticket's PR: the newest run with a recorded branch decides which
/// branch to look up, in the removable worktree's repo when there is one,
/// else in that run's own worktree.
fn ticket_pr(
    runs: &[Run],
    removable: Option<&(PathBuf, PathBuf)>,
    prs: &mut PrCache<'_>,
) -> Option<TicketPr> {
    let run = runs.iter().rev().find(|run| run.branch.is_some())?;
    let branch = run.branch.as_deref()?;
    let dir = match removable {
        Some((repo_root, _)) => repo_root.clone(),
        None => PathBuf::from(&run.worktree),
    };
    if !dir.is_dir() {
        return None;
    }
    let listed = prs.list(&dir)?;
    let matching: Vec<&PrSummary> = listed
        .iter()
        .filter(|pr| pr.head_ref_name == branch)
        .collect();
    [PrLifecycle::Open, PrLifecycle::Merged, PrLifecycle::Closed]
        .into_iter()
        .find_map(|lifecycle| {
            matching
                .iter()
                .find(|pr| pr.lifecycle == lifecycle)
                .map(|pr| TicketPr {
                    number: pr.number,
                    lifecycle,
                })
        })
}

/// `gh pr list --state all` results memoized by repository root.
struct PrCache<'a> {
    gh: &'a dyn GhCli,
    git: &'a dyn GitOps,
    by_repo: HashMap<PathBuf, Option<Vec<PrSummary>>>,
}

impl<'a> PrCache<'a> {
    fn new(gh: &'a dyn GhCli, git: &'a dyn GitOps) -> Self {
        Self {
            gh,
            git,
            by_repo: HashMap::new(),
        }
    }

    /// Every PR of the repository containing `dir`, or `None` when gh
    /// failed. A path git can't resolve is its own cache key.
    fn list(&mut self, dir: &Path) -> Option<Vec<PrSummary>> {
        let key = self
            .git
            .repo_root(dir)
            .unwrap_or_else(|_| dir.to_path_buf());
        if let Some(cached) = self.by_repo.get(&key) {
            return cached.clone();
        }
        let listed = self.gh.pr_list_all(&key).ok();
        self.by_repo.insert(key, listed.clone());
        listed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::FakeGhCli;
    use crate::runs::StartRun;
    use crate::work::git::{FakeGitOps, GitError};
    use crate::work::tmux::{FakeTmuxOps, TmuxSession};
    use tempfile::{TempDir, tempdir};

    const SCOPE_REPO: &str = "jowi-dev/tskmstr";

    fn identity() -> BackendIdentity {
        BackendIdentity::Github {
            repo: SCOPE_REPO.to_string(),
        }
    }

    fn session_for(ticket: &str) -> String {
        ticket_session_name(&identity().session_slug(), ticket)
    }

    /// A fixture repo root and worktree root under one temp dir.
    struct Fixture {
        tmp: TempDir,
        store: RunStore,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempdir().unwrap();
            std::fs::create_dir_all(tmp.path().join("repo")).unwrap();
            let store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
            Self { tmp, store }
        }

        fn repo(&self) -> PathBuf {
            self.tmp.path().join("repo")
        }

        fn worktree(&self, ticket: &str) -> PathBuf {
            self.tmp
                .path()
                .join("Worktrees/repo")
                .join(ticket.to_lowercase())
        }

        /// Record a lane run for `ticket` on `branch`, with its worktree on disk.
        fn lane_run(&self, ticket: &str, branch: &str, pid: Option<u32>) -> i64 {
            let wt = self.worktree(ticket);
            std::fs::create_dir_all(&wt).unwrap();
            self.store
                .start_run(&StartRun {
                    scope: identity().scope(),
                    ticket: ticket.to_string(),
                    lane: "backend".to_string(),
                    worktree: wt.to_string_lossy().into_owned(),
                    branch: Some(branch.to_string()),
                    pid,
                    kind: "lane".to_string(),
                    log_path: None,
                })
                .unwrap()
        }

        fn finish(&self, id: i64) {
            self.store
                .finish_run(id, &crate::runs::FinishRun::default())
                .unwrap();
        }

        /// `clean`'s guard, modeled: a worktree is removable only when it
        /// sits under `<tmp>/Worktrees/repo`.
        fn removable(&self) -> impl Fn(&[Run]) -> Option<(PathBuf, PathBuf)> + '_ {
            move |runs: &[Run]| {
                let root = self.tmp.path().join("Worktrees/repo");
                runs.iter().rev().find_map(|run| {
                    let wt = PathBuf::from(&run.worktree);
                    (wt.parent() == Some(root.as_path())).then(|| (self.repo(), wt))
                })
            }
        }
    }

    fn pr(number: u64, branch: &str, lifecycle: PrLifecycle) -> PrSummary {
        PrSummary {
            number,
            head_ref_name: branch.to_string(),
            head_ref_oid: String::new(),
            lifecycle,
            updated_at: "2026-11-19T00:00:00Z".to_string(),
        }
    }

    fn dead(_pid: u32) -> bool {
        false
    }

    fn alive(_pid: u32) -> bool {
        true
    }

    fn plan(
        fx: &Fixture,
        tmux: &FakeTmuxOps,
        gh: &FakeGhCli,
        git: &FakeGitOps,
        pid_alive: &dyn Fn(u32) -> bool,
        selection: &SweepSelection,
    ) -> Vec<SweepCandidate> {
        let identity = identity();
        let removable = fx.removable();
        let deps = SweepPlanDeps {
            store: &fx.store,
            tmux,
            gh,
            git,
            pid_alive,
            identity: &identity,
            removable: &removable,
        };
        plan_sweep(&deps, selection).unwrap()
    }

    fn by_ticket<'a>(plan: &'a [SweepCandidate], ticket: &str) -> &'a SweepCandidate {
        plan.iter()
            .find(|c| c.ticket == ticket)
            .unwrap_or_else(|| panic!("{ticket} not planned: {plan:?}"))
    }

    #[test]
    fn merged_marks_every_ticket_whose_pr_is_merged_and_nothing_else() {
        let fx = Fixture::new();
        for (ticket, branch) in [("GH-1", "b1"), ("GH-2", "b2"), ("GH-3", "b3")] {
            let id = fx.lane_run(ticket, branch, Some(1));
            fx.finish(id);
        }
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![
            pr(10, "b1", PrLifecycle::Merged),
            pr(20, "b2", PrLifecycle::Closed),
            pr(30, "b3", PrLifecycle::Open),
        ]));
        let git = FakeGitOps::new().with_repo_root(Ok(fx.repo()));

        let merged = plan(
            &fx,
            &FakeTmuxOps::new(),
            &gh,
            &git,
            &dead,
            &SweepSelection {
                merged: true,
                ..Default::default()
            },
        );
        let tickets: Vec<&str> = merged.iter().map(|c| c.ticket.as_str()).collect();
        assert_eq!(tickets, vec!["GH-1"]);
        assert_eq!(merged[0].verdict.tier, KillSafetyTier::Safe);
        assert_eq!(merged[0].skip, None);
        assert_eq!(gh.pr_list_all_calls().len(), 1, "one gh call per repo");

        let closed = plan(
            &fx,
            &FakeTmuxOps::new(),
            &gh,
            &git,
            &dead,
            &SweepSelection {
                closed: true,
                ..Default::default()
            },
        );
        let tickets: Vec<&str> = closed.iter().map(|c| c.ticket.as_str()).collect();
        assert_eq!(tickets, vec!["GH-2"]);
    }

    #[test]
    fn a_bulk_mark_ignores_other_scopes_and_tickets_with_nothing_left() {
        let fx = Fixture::new();
        let id = fx.lane_run("GH-1", "b1", None);
        fx.finish(id);
        // Worktree already gone and no session: nothing to clean.
        std::fs::remove_dir_all(fx.worktree("GH-1")).unwrap();
        // Another repo's ticket with a merged PR on the same branch name.
        let other = fx
            .store
            .start_run(&StartRun {
                scope: "github:someone/else".to_string(),
                ticket: "GH-2".to_string(),
                lane: "backend".to_string(),
                worktree: fx.worktree("GH-2").to_string_lossy().into_owned(),
                branch: Some("b1".to_string()),
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        fx.finish(other);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr(10, "b1", PrLifecycle::Merged)]));
        let git = FakeGitOps::new().with_repo_root(Ok(fx.repo()));

        let planned = plan(
            &fx,
            &FakeTmuxOps::new(),
            &gh,
            &git,
            &dead,
            &SweepSelection {
                merged: true,
                ..Default::default()
            },
        );
        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn a_ticket_with_only_a_live_session_is_still_bulk_marked() {
        let fx = Fixture::new();
        let id = fx.lane_run("GH-1", "b1", None);
        fx.finish(id);
        std::fs::remove_dir_all(fx.worktree("GH-1")).unwrap();
        let tmux = FakeTmuxOps::new().with_list_sessions(Ok(vec![TmuxSession {
            name: session_for("GH-1"),
            path: String::new(),
        }]));
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr(10, "b1", PrLifecycle::Merged)]));
        let git = FakeGitOps::new().with_repo_root(Ok(fx.repo()));

        let planned = plan(
            &fx,
            &tmux,
            &gh,
            &git,
            &dead,
            &SweepSelection {
                merged: true,
                ..Default::default()
            },
        );
        assert_eq!(planned.len(), 1, "{planned:?}");
        assert_eq!(planned[0].skip, None);
    }

    /// The acceptance criterion's mixed set: every skip reason, side by side
    /// with a ticket that is cleaned.
    #[test]
    fn a_mixed_set_skips_live_root_and_dirty_and_keeps_the_rest() {
        let fx = Fixture::new();
        let clean_id = fx.lane_run("GH-1", "b1", Some(1));
        fx.finish(clean_id);
        fx.lane_run("GH-2", "b2", Some(2)); // still running, pid alive
        let root_id = fx.lane_run("GH-3", "b3", Some(3));
        fx.finish(root_id);
        let dirty_id = fx.lane_run("GH-4", "b4", Some(4));
        fx.finish(dirty_id);
        let broken_id = fx.lane_run("GH-5", "b5", Some(5));
        fx.finish(broken_id);
        // An audit-only ticket: its run sits in the user's checkout.
        let audit_id = fx
            .store
            .start_run(&StartRun {
                scope: identity().scope(),
                ticket: "GH-6".to_string(),
                lane: "audit".to_string(),
                worktree: fx.repo().to_string_lossy().into_owned(),
                branch: None,
                pid: None,
                kind: "audit".to_string(),
                log_path: None,
            })
            .unwrap();
        fx.finish(audit_id);

        let tmux = FakeTmuxOps::new().with_root_session_targets(Ok(vec![session_for("GH-3")]));
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![
            pr(1, "b1", PrLifecycle::Merged),
            pr(3, "b3", PrLifecycle::Merged),
            pr(4, "b4", PrLifecycle::Merged),
            pr(5, "b5", PrLifecycle::Merged),
        ]));
        let git = FakeGitOps::new()
            .with_repo_root(Ok(fx.repo()))
            .with_status_is_clean(Ok(true))
            .with_status_is_clean_for(&fx.worktree("GH-4"), Ok(false))
            .with_status_is_clean_for(
                &fx.worktree("GH-5"),
                Err(GitError::Command {
                    command: "git status --porcelain".to_string(),
                    exit_code: Some(128),
                    stderr: "boom".to_string(),
                }),
            );

        let planned = plan(
            &fx,
            &tmux,
            &gh,
            &git,
            &alive,
            &SweepSelection {
                keys: ["gh-1", "GH-2", "GH-3", "GH-4", "GH-5", "GH-6"]
                    .map(str::to_string)
                    .to_vec(),
                ..Default::default()
            },
        );

        let tickets: Vec<&str> = planned.iter().map(|c| c.ticket.as_str()).collect();
        assert_eq!(
            tickets,
            vec!["GH-1", "GH-2", "GH-3", "GH-4", "GH-5", "GH-6"]
        );
        assert_eq!(by_ticket(&planned, "GH-1").skip, None);
        assert_eq!(by_ticket(&planned, "GH-2").skip, Some(SkipReason::LiveRun));
        assert_eq!(
            by_ticket(&planned, "GH-3").skip,
            Some(SkipReason::RootSession)
        );
        assert_eq!(
            by_ticket(&planned, "GH-4").skip,
            Some(SkipReason::DirtyWorktree)
        );
        assert!(matches!(
            by_ticket(&planned, "GH-5").skip,
            Some(SkipReason::StatusUnreadable(_))
        ));
        let audit = by_ticket(&planned, "GH-6");
        assert_eq!(
            audit.worktree, None,
            "an audit checkout is never a sweep target"
        );
        assert_eq!(audit.skip, None, "its session is still cleaned");
    }

    #[test]
    fn an_explicit_key_is_planned_even_with_nothing_recorded() {
        let fx = Fixture::new();
        let planned = plan(
            &fx,
            &FakeTmuxOps::new(),
            &FakeGhCli::new(),
            &FakeGitOps::new(),
            &dead,
            &SweepSelection {
                keys: vec!["gh-9".to_string(), "GH-9".to_string()],
                ..Default::default()
            },
        );
        assert_eq!(planned.len(), 1, "duplicate keys collapse: {planned:?}");
        assert_eq!(planned[0].ticket, "GH-9");
        assert_eq!(planned[0].worktree, None);
        assert_eq!(planned[0].verdict.tier, KillSafetyTier::Unknown);
        assert_eq!(planned[0].skip, None);
    }
}
