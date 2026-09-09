//! Kill-safety classification for tmux sessions (GitHub issue #26).
//!
//! The devtools session picker's kill key needs to know, before destroying a
//! session and its worktree, whether that would interrupt live work — and
//! only tm can answer that: run liveness lives in the runs store, PR merge
//! state behind `gh`, and the session-naming scheme in
//! [`crate::work::naming`]. This module is the producer side of the contract
//! the picker consumes (devtools#15) via `tm runs kill-safety <SESSION>`;
//! the tiers and output format are pinned in
//! `docs/decisions/0005-kill-safety-classification.md`.
//!
//! Classification is deliberately conservative: every ambiguous case lands
//! in [`KillSafetyTier::Unknown`], which the picker treats exactly like a
//! live run (prompt before killing). Only a positively-verified "the PR is
//! merged and nothing is running" earns [`KillSafetyTier::Safe`].

use std::path::Path;

use crate::config::session_slug_from_scope;
use crate::github::gh_cli::{GhCli, PrLifecycle};
use crate::runs::{Run, RunStatus, RunStore, RunStoreError};
use crate::work::naming::{create_session_name, ticket_session_name};
use crate::work::tmux::TmuxOps;

/// How dangerous killing a given tmux session would be, most to least: the
/// tokens the session picker branches its confirmation behavior on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillSafetyTier {
    /// An agent run is live in this session (starting, running, or waiting
    /// for input) — killing it interrupts work in flight.
    LiveRun,
    /// This session is some session's `@root_session` target: a per-project
    /// hub session (the one the board runs in), not a disposable ticket
    /// session.
    RootSession,
    /// The ticket's PR is merged and no agent run is alive: the worktree and
    /// session carry nothing unrecoverable.
    Safe,
    /// tm cannot vouch for the session — not one of its sessions, no
    /// recorded branch, an unmerged PR, or a gh/worktree failure. The picker
    /// should treat this like a live run and prompt.
    Unknown,
}

impl KillSafetyTier {
    /// The token printed as `tm runs kill-safety`'s first output line —
    /// the machine-readable half of the picker contract.
    pub fn as_str(self) -> &'static str {
        match self {
            KillSafetyTier::LiveRun => "live-run",
            KillSafetyTier::RootSession => "root-session",
            KillSafetyTier::Safe => "safe",
            KillSafetyTier::Unknown => "unknown",
        }
    }
}

/// A classified session: the tier plus a one-line human explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillSafetyVerdict {
    /// The tier the picker branches on.
    pub tier: KillSafetyTier,
    /// Why, for the human reading the confirmation prompt.
    pub detail: String,
}

/// Whether `run` is (or was) hosted in tmux session `session`.
///
/// Three ways a run maps onto a session name, tried cheapest first: the
/// launcher-recorded [`Run::tmux_session`]; the ticket session its scope and
/// ticket compute to ([`ticket_session_name`], covering runs adopted via `tm
/// runs register`, which have no recorded session); and, for `create` runs,
/// the scope's keyless creation session ([`create_session_name`]).
fn run_hosted_in(run: &Run, session: &str) -> bool {
    if run.tmux_session.as_deref() == Some(session) {
        return true;
    }
    let Some(slug) = session_slug_from_scope(&run.scope) else {
        return false;
    };
    if ticket_session_name(&slug, &run.ticket) == session {
        return true;
    }
    run.kind == "create" && create_session_name(&slug) == session
}

/// Whether `run` is live right now: still `running` in the store and not
/// provably dead. A recorded pid is probed; a pid-less running row (a
/// pre-adoption interactive launch) counts as live — the conservative
/// reading, since the picker prompts for live runs.
fn run_is_live(run: &Run, pid_alive: &dyn Fn(u32) -> bool) -> bool {
    run.status == RunStatus::Running && run.pid.is_none_or(pid_alive)
}

/// Classify tmux session `session` for the picker's kill key.
///
/// Tier decision, in order:
///
/// 1. `session` is a `@root_session` target → [`KillSafetyTier::RootSession`].
/// 2. Any run hosted in `session` is live → [`KillSafetyTier::LiveRun`].
/// 3. The newest hosted run with a recorded branch has a **merged** PR for
///    that branch (via `gh pr list` in the run's worktree) and none open →
///    [`KillSafetyTier::Safe`].
/// 4. Everything else → [`KillSafetyTier::Unknown`].
///
/// Only a [`RunStoreError`] propagates (a broken runs DB is a real failure
/// the caller reports); tmux and gh errors degrade to their conservative
/// tier instead.
pub fn classify_session(
    session: &str,
    store: &RunStore,
    tmux: &dyn TmuxOps,
    gh: &dyn GhCli,
    pid_alive: &dyn Fn(u32) -> bool,
) -> Result<KillSafetyVerdict, RunStoreError> {
    if tmux
        .root_session_targets()
        .unwrap_or_default()
        .iter()
        .any(|target| target == session)
    {
        return Ok(KillSafetyVerdict {
            tier: KillSafetyTier::RootSession,
            detail: format!("{session} is a project root session (an @root_session target)"),
        });
    }

    let runs = store.all_runs()?;
    let hosted: Vec<&Run> = runs
        .iter()
        .filter(|run| run_hosted_in(run, session))
        .collect();

    if let Some(live) = hosted.iter().find(|run| run_is_live(run, pid_alive)) {
        return Ok(KillSafetyVerdict {
            tier: KillSafetyTier::LiveRun,
            detail: format!(
                "run {} ({} {}) is live in this session",
                live.id, live.kind, live.ticket
            ),
        });
    }

    // `all_runs` is newest-first, so the first hosted run with a branch is
    // the one whose PR decides safety.
    let Some(run) = hosted.iter().find(|run| run.branch.is_some()) else {
        return Ok(KillSafetyVerdict {
            tier: KillSafetyTier::Unknown,
            detail: format!("no recorded run maps {session} to a branch"),
        });
    };
    let branch = run.branch.as_deref().expect("filtered on branch.is_some()");

    let worktree = Path::new(&run.worktree);
    if !worktree.is_dir() {
        return Ok(KillSafetyVerdict {
            tier: KillSafetyTier::Unknown,
            detail: format!("worktree {} is gone, cannot query PRs", run.worktree),
        });
    }

    let Ok(prs) = gh.pr_list_all(worktree) else {
        return Ok(KillSafetyVerdict {
            tier: KillSafetyTier::Unknown,
            detail: format!("gh failed listing PRs for {branch}"),
        });
    };

    let matching: Vec<_> = prs.iter().filter(|pr| pr.head_ref_name == branch).collect();
    if matching.iter().any(|pr| pr.lifecycle == PrLifecycle::Open) {
        return Ok(KillSafetyVerdict {
            tier: KillSafetyTier::Unknown,
            detail: format!("{branch} has an open PR"),
        });
    }
    if let Some(merged) = matching
        .iter()
        .find(|pr| pr.lifecycle == PrLifecycle::Merged)
    {
        return Ok(KillSafetyVerdict {
            tier: KillSafetyTier::Safe,
            detail: format!(
                "PR #{} for {branch} is merged and no run is live",
                merged.number
            ),
        });
    }

    Ok(KillSafetyVerdict {
        tier: KillSafetyTier::Unknown,
        detail: format!("no merged PR found for {branch}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::{FakeGhCli, PrSummary};
    use crate::runs::StartRun;
    use crate::work::tmux::FakeTmuxOps;
    use tempfile::tempdir;

    fn open_store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).unwrap()
    }

    fn start(worktree: &str, ticket: &str, kind: &str, branch: Option<&str>) -> StartRun {
        StartRun {
            scope: "github:jowi-dev/tskmstr".to_string(),
            ticket: ticket.to_string(),
            lane: "backend".to_string(),
            worktree: worktree.to_string(),
            branch: branch.map(str::to_string),
            pid: None,
            kind: kind.to_string(),
            log_path: None,
        }
    }

    fn pr(number: u64, branch: &str, lifecycle: PrLifecycle) -> PrSummary {
        PrSummary {
            number,
            head_ref_name: branch.to_string(),
            lifecycle,
            updated_at: "2026-09-08T00:00:00Z".to_string(),
        }
    }

    fn alive(_pid: u32) -> bool {
        true
    }

    fn dead(_pid: u32) -> bool {
        false
    }

    #[test]
    fn a_root_session_target_classifies_as_root_session() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let tmux = FakeTmuxOps::new()
            .with_root_session_targets(Ok(vec!["tskmstr".to_string(), "vdiff".to_string()]));

        let verdict =
            classify_session("tskmstr", &store, &tmux, &FakeGhCli::new(), &alive).unwrap();

        assert_eq!(verdict.tier, KillSafetyTier::RootSession);
    }

    #[test]
    fn a_running_run_with_a_recorded_session_classifies_as_live_run() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let id = store
            .start_run(&start("/tmp/wt", "GH-26", "lane", None))
            .unwrap();
        store.update_tmux_session(id, "tm-x-custom").unwrap();

        let verdict = classify_session(
            "tm-x-custom",
            &store,
            &FakeTmuxOps::new(),
            &FakeGhCli::new(),
            &alive,
        )
        .unwrap();

        assert_eq!(verdict.tier, KillSafetyTier::LiveRun);
        assert!(verdict.detail.contains("GH-26"));
    }

    /// Runs adopted via `tm runs register` (audits, registered lane runs)
    /// record no tmux_session; they map onto the ticket session their scope
    /// and ticket compute to.
    #[test]
    fn a_running_run_matches_its_computed_ticket_session_name() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        store
            .start_run(&start("/tmp/wt", "GH-26", "audit", None))
            .unwrap();

        let verdict = classify_session(
            "tm-jowi-dev-tskmstr-gh-26",
            &store,
            &FakeTmuxOps::new(),
            &FakeGhCli::new(),
            &alive,
        )
        .unwrap();

        assert_eq!(verdict.tier, KillSafetyTier::LiveRun);
    }

    #[test]
    fn a_create_run_matches_its_scopes_creation_session() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        store
            .start_run(&start("/tmp/wt", "GH-99", "create", None))
            .unwrap();

        let verdict = classify_session(
            "tm-jowi-dev-tskmstr-create",
            &store,
            &FakeTmuxOps::new(),
            &FakeGhCli::new(),
            &alive,
        )
        .unwrap();

        assert_eq!(verdict.tier, KillSafetyTier::LiveRun);
    }

    #[test]
    fn a_merged_pr_with_no_live_run_classifies_as_safe() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let worktree = tmp.path().join("wt");
        std::fs::create_dir(&worktree).unwrap();
        let id = store
            .start_run(&start(
                &worktree.to_string_lossy(),
                "GH-26",
                "lane",
                Some("jowi-dev/gh-26-fix"),
            ))
            .unwrap();
        store
            .finish_run(id, &crate::runs::FinishRun::default())
            .unwrap();
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr(
            12,
            "jowi-dev/gh-26-fix",
            PrLifecycle::Merged,
        )]));

        let verdict = classify_session(
            "tm-jowi-dev-tskmstr-gh-26",
            &store,
            &FakeTmuxOps::new(),
            &gh,
            &alive,
        )
        .unwrap();

        assert_eq!(verdict.tier, KillSafetyTier::Safe);
        assert!(verdict.detail.contains("PR #12"));
    }

    /// A running row whose recorded pid is dead is not live — the killed
    /// process must not force a prompt forever; safety falls through to the
    /// PR check.
    #[test]
    fn a_dead_pid_run_does_not_read_as_live() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let worktree = tmp.path().join("wt");
        std::fs::create_dir(&worktree).unwrap();
        let id = store
            .start_run(&StartRun {
                pid: Some(4242),
                ..start(
                    &worktree.to_string_lossy(),
                    "GH-26",
                    "lane",
                    Some("jowi-dev/gh-26-fix"),
                )
            })
            .unwrap();
        store.update_tmux_session(id, "tm-x-custom").unwrap();
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr(
            12,
            "jowi-dev/gh-26-fix",
            PrLifecycle::Merged,
        )]));

        let verdict =
            classify_session("tm-x-custom", &store, &FakeTmuxOps::new(), &gh, &dead).unwrap();

        assert_eq!(verdict.tier, KillSafetyTier::Safe);
    }

    #[test]
    fn an_open_pr_classifies_as_unknown() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let worktree = tmp.path().join("wt");
        std::fs::create_dir(&worktree).unwrap();
        let id = store
            .start_run(&start(
                &worktree.to_string_lossy(),
                "GH-26",
                "lane",
                Some("jowi-dev/gh-26-fix"),
            ))
            .unwrap();
        store
            .finish_run(id, &crate::runs::FinishRun::default())
            .unwrap();
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr(
            12,
            "jowi-dev/gh-26-fix",
            PrLifecycle::Open,
        )]));

        let verdict = classify_session(
            "tm-jowi-dev-tskmstr-gh-26",
            &store,
            &FakeTmuxOps::new(),
            &gh,
            &alive,
        )
        .unwrap();

        assert_eq!(verdict.tier, KillSafetyTier::Unknown);
        assert!(verdict.detail.contains("open PR"));
    }

    #[test]
    fn a_session_tm_knows_nothing_about_classifies_as_unknown() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());

        let verdict = classify_session(
            "someones-scratch-session",
            &store,
            &FakeTmuxOps::new(),
            &FakeGhCli::new(),
            &alive,
        )
        .unwrap();

        assert_eq!(verdict.tier, KillSafetyTier::Unknown);
    }

    #[test]
    fn a_missing_worktree_classifies_as_unknown() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let id = store
            .start_run(&start(
                "/nonexistent/wt",
                "GH-26",
                "lane",
                Some("jowi-dev/gh-26-fix"),
            ))
            .unwrap();
        store
            .finish_run(id, &crate::runs::FinishRun::default())
            .unwrap();

        let verdict = classify_session(
            "tm-jowi-dev-tskmstr-gh-26",
            &store,
            &FakeTmuxOps::new(),
            &FakeGhCli::new(),
            &alive,
        )
        .unwrap();

        assert_eq!(verdict.tier, KillSafetyTier::Unknown);
        assert!(verdict.detail.contains("worktree"));
    }
}
