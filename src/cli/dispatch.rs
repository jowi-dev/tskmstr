//! `tm dispatch`: host the autopilot dispatcher from the command line, so a
//! systemd timer (or launchd agent) keeps autopilot running with no TUI open
//! (GitHub issue #117).
//!
//! - `tm dispatch tick` runs one [`crate::work::dispatch::tick`] per scope.
//!   A refusal or a skip is a normal outcome, not an error.
//! - `tm dispatch on|off` writes the scope's runs.db `autopilot` flag, the
//!   same flag the TUI toggle writes. `on` in a repo also records the repo's
//!   main checkout, which is how `tick --all` finds the repo's config later
//!   from any directory.
//! - `tm dispatch status` lists every scope runs.db knows about.
//!
//! Every command takes a scope selector: the current repo by default,
//! `--scope S`, or `--all` (every scope with an `autopilot` row). `tick
//! --all` only visits scopes that are on, and each tick re-reads the flag
//! under the scope lock, so `off --all` stops the next tick from launching
//! anything. The lock is the one the TUI host takes too.
//!
//! `src/main.rs` builds the real per-scope dependencies behind
//! [`ScopeTicker`]; this module only selects scopes and prints results.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::json;
use thiserror::Error;

use crate::config::{BackendIdentity, BackendIdentityResolver, Config, compatible_lane_names};
use crate::runs::{AutopilotScope, RunStatus, RunStore, RunStoreError};
use crate::work::dispatch::{LaneChoice, REFUSED_EVENT, Skip, TickReport};

/// Which scopes a `tm dispatch` command acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeSelector {
    /// The current repo's scope.
    Current,
    /// One named scope, e.g. `github:o/r`.
    Scope(String),
    /// Every scope with an `autopilot` row.
    All,
}

impl ScopeSelector {
    /// The selector for `--scope S` / `--all` / neither. clap rejects both
    /// flags together.
    pub fn from_flags(scope: Option<String>, all: bool) -> Self {
        match (scope, all) {
            (_, true) => ScopeSelector::All,
            (Some(scope), false) => ScopeSelector::Scope(scope),
            (None, false) => ScopeSelector::Current,
        }
    }
}

/// The repo `tm dispatch` runs in: its scope and main checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentRepo {
    /// [`BackendIdentity::scope`] of the repo's config.
    pub scope: String,
    /// The main checkout's root.
    pub repo_dir: PathBuf,
}

/// One scope a tick will visit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The scope.
    pub scope: String,
    /// The repo to load its config from and launch lanes in. `None` when
    /// runs.db has no repo for the scope.
    pub repo_dir: Option<PathBuf>,
}

/// Runs one tick for a scope with real dependencies. `src/main.rs`
/// implements it; tests use a fake.
pub trait ScopeTicker {
    /// Ticks `scope`, loading config from `repo_dir`.
    fn tick(&self, scope: &str, repo_dir: &Path) -> Result<TickReport, String>;
}

/// Errors that stop a `tm dispatch` command.
#[derive(Debug, Error)]
pub enum DispatchCliError {
    /// The run store couldn't be read or written.
    #[error(transparent)]
    Store(#[from] RunStoreError),
    /// Writing output failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The command needs the current repo, and there is none.
    #[error("not in a tskmstr repo: run this inside a repo, or pass --scope or --all")]
    NoCurrentRepo,
}

/// The scopes `tm dispatch tick` visits for `selector`. `--all` takes only
/// the scopes that are on; the tick itself re-checks the flag under the
/// lock. A named scope that is the current repo's uses the current
/// checkout; any other uses the repo runs.db recorded for it.
pub fn tick_targets(
    rows: &[AutopilotScope],
    selector: &ScopeSelector,
    current: Option<&CurrentRepo>,
) -> Result<Vec<Target>, DispatchCliError> {
    let recorded = |scope: &str| {
        rows.iter()
            .find(|row| row.scope == scope)
            .and_then(|row| row.repo_dir.as_ref())
            .map(PathBuf::from)
    };
    Ok(match selector {
        ScopeSelector::Current => {
            let current = current.ok_or(DispatchCliError::NoCurrentRepo)?;
            vec![Target {
                scope: current.scope.clone(),
                repo_dir: Some(current.repo_dir.clone()),
            }]
        }
        ScopeSelector::Scope(scope) => {
            let repo_dir = match current {
                Some(current) if current.scope == *scope => Some(current.repo_dir.clone()),
                _ => recorded(scope),
            };
            vec![Target {
                scope: scope.clone(),
                repo_dir,
            }]
        }
        ScopeSelector::All => rows
            .iter()
            .filter(|row| row.enabled)
            .map(|row| Target {
                scope: row.scope.clone(),
                repo_dir: row.repo_dir.as_ref().map(PathBuf::from),
            })
            .collect(),
    })
}

/// What happened to one scope in `tm dispatch tick`.
enum TickOutcome {
    Ran(TickReport),
    Failed(String),
}

/// `tm dispatch tick`: ticks each target in order and prints what each one
/// did, as text or (`json`) one JSON array. A scope that fails (its config
/// won't load, runs.db has no repo for it) is reported and the rest still
/// tick. Returns whether every scope ticked; refusals and skips count as
/// ticked.
pub fn tick(
    targets: &[Target],
    ticker: &dyn ScopeTicker,
    json: bool,
    out: &mut dyn Write,
) -> io::Result<bool> {
    let outcomes: Vec<(&str, TickOutcome)> = targets
        .iter()
        .map(|target| {
            let outcome = match &target.repo_dir {
                None => TickOutcome::Failed(format!(
                    "no repo recorded for {}; run `tm dispatch on` inside it once",
                    target.scope
                )),
                Some(dir) => match ticker.tick(&target.scope, dir) {
                    Ok(report) => TickOutcome::Ran(report),
                    Err(err) => TickOutcome::Failed(err),
                },
            };
            (target.scope.as_str(), outcome)
        })
        .collect();
    let all_ok = outcomes
        .iter()
        .all(|(_, outcome)| matches!(outcome, TickOutcome::Ran(_)));

    if json {
        let rows: Vec<_> = outcomes
            .iter()
            .map(|(scope, outcome)| tick_json(scope, outcome))
            .collect();
        writeln!(out, "{}", serde_json::Value::Array(rows))?;
        return Ok(all_ok);
    }
    if outcomes.is_empty() {
        writeln!(out, "no scope has autopilot on")?;
    }
    for (scope, outcome) in &outcomes {
        write_tick_text(scope, outcome, out)?;
    }
    Ok(all_ok)
}

fn skip_name(skip: Skip) -> &'static str {
    match skip {
        Skip::Locked => "locked",
        Skip::Disabled => "disabled",
    }
}

fn tick_json(scope: &str, outcome: &TickOutcome) -> serde_json::Value {
    match outcome {
        TickOutcome::Ran(report) => json!({
            "scope": scope,
            "launched": report.launched.iter().map(|launch| json!({
                "ticket": launch.ticket,
                "lane": launch.lane,
                "stacked_on": launch.stacked_on,
            })).collect::<Vec<_>>(),
            "refused": report.refused.as_ref().map(ToString::to_string),
            "skipped": report.skipped.map(skip_name),
            "notified": report.notified,
            "error": null,
        }),
        TickOutcome::Failed(err) => json!({
            "scope": scope,
            "launched": [],
            "refused": null,
            "skipped": null,
            "notified": [],
            "error": err,
        }),
    }
}

fn write_tick_text(scope: &str, outcome: &TickOutcome, out: &mut dyn Write) -> io::Result<()> {
    let report = match outcome {
        TickOutcome::Failed(err) => return writeln!(out, "{scope}: error: {err}"),
        TickOutcome::Ran(report) => report,
    };
    match report.skipped {
        Some(Skip::Locked) => {
            return writeln!(out, "{scope}: skipped: another host is ticking this scope");
        }
        Some(Skip::Disabled) => return writeln!(out, "{scope}: skipped: autopilot is off"),
        None => {}
    }
    for launch in &report.launched {
        match &launch.stacked_on {
            Some(base) => writeln!(
                out,
                "{scope}: launched {} in lane {} (stacked on {base})",
                launch.ticket, launch.lane
            )?,
            None => writeln!(
                out,
                "{scope}: launched {} in lane {}",
                launch.ticket, launch.lane
            )?,
        }
    }
    if let Some(refusal) = &report.refused {
        writeln!(out, "{scope}: {refusal}")?;
    } else if report.launched.is_empty() {
        writeln!(out, "{scope}: nothing to launch")?;
    }
    for title in &report.notified {
        writeln!(out, "{scope}: notified: {title}")?;
    }
    Ok(())
}

/// `tm dispatch on|off`: sets the autopilot flag for the selected scopes.
/// For the current repo it also records the repo's main checkout. `--all`
/// covers every scope with an `autopilot` row, so `off --all` is the kill
/// switch.
pub fn set(
    store: &RunStore,
    selector: &ScopeSelector,
    enabled: bool,
    current: Option<&CurrentRepo>,
    out: &mut dyn Write,
) -> Result<(), DispatchCliError> {
    let word = if enabled { "on" } else { "off" };
    match selector {
        ScopeSelector::Current => {
            let current = current.ok_or(DispatchCliError::NoCurrentRepo)?;
            let dir = current.repo_dir.to_string_lossy();
            store.set_autopilot_in(&current.scope, enabled, Some(&dir))?;
            writeln!(out, "autopilot {word} for {} ({dir})", current.scope)?;
        }
        ScopeSelector::Scope(scope) => {
            store.set_autopilot(scope, enabled)?;
            writeln!(out, "autopilot {word} for {scope}")?;
        }
        ScopeSelector::All => {
            let rows = store.autopilot_scopes()?;
            if rows.is_empty() {
                writeln!(out, "no scopes registered; run `tm dispatch on` in a repo")?;
            }
            for row in rows {
                store.set_autopilot(&row.scope, enabled)?;
                writeln!(out, "autopilot {word} for {}", row.scope)?;
            }
        }
    }
    Ok(())
}

/// `tm dispatch status`: one line per scope with an `autopilot` row: on or
/// off, its running lanes, when it last ticked, and its last refusal.
pub fn status(store: &RunStore, out: &mut dyn Write) -> Result<(), DispatchCliError> {
    let rows = store.autopilot_scopes()?;
    if rows.is_empty() {
        writeln!(out, "no scopes registered; run `tm dispatch on` in a repo")?;
        return Ok(());
    }
    for row in rows {
        let running = store
            .list_runs_filtered(Some(&row.scope), Some("lane"))?
            .iter()
            .filter(|run| run.status == RunStatus::Running)
            .count();
        let state = if row.enabled { "on" } else { "off" };
        let last_tick = row
            .last_tick_at
            .as_deref()
            .map(trim_timestamp)
            .unwrap_or("never");
        writeln!(
            out,
            "{}: {state}, {running} lanes running, last tick {last_tick}",
            row.scope
        )?;
        if let Some(repo_dir) = &row.repo_dir {
            writeln!(out, "  repo: {repo_dir}")?;
        }
        if let Some(event) = store.last_dispatch_event(&row.scope, REFUSED_EVENT)? {
            writeln!(
                out,
                "  last refusal {}: {}",
                trim_timestamp(&event.created_at),
                event.detail
            )?;
        }
    }
    Ok(())
}

/// A runs.db timestamp (`2026-10-18T14:03:22.123Z`) without its fraction
/// of a second.
fn trim_timestamp(ts: &str) -> &str {
    ts.split_once('.').map_or(ts, |(whole, _)| whole)
}

/// The lanes a tick in `config`'s repo may launch into: the configured
/// lanes whose repo resolves to the same backend as `config`, in lane-name
/// order, with each lane's memory-admission repo key.
pub fn lane_choices(config: &Config, resolver: &dyn BackendIdentityResolver) -> Vec<LaneChoice> {
    let current = BackendIdentity::from_config(config);
    let (names, _hidden) = compatible_lane_names(&current, &config.work.lanes, resolver);
    names
        .into_iter()
        .map(|name| {
            let repo = crate::work::run::lane_repo_key(&name, &config.work.lanes[&name]);
            LaneChoice { name, repo }
        })
        .collect()
}

/// The runner an unattended launch from `config` prefers, or `None` when
/// every configured runner is metered and paid fallback is off. Matches
/// what `tm work run --unattended` picks.
pub fn unattended_agent(config: &Config) -> Option<&'static str> {
    crate::agent::routing::configured_unattended_order(
        config.agent,
        &config.agent_fallbacks,
        &config.usage,
        &config.dispatch,
    )
    .first()
    .map(|kind| kind.as_str())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::cli::ready::ListDecision;
    use crate::config::{Billing, FakeBackendIdentityResolver, LaneConfig, MemoryConfig};
    use crate::runs::StartRun;
    use crate::work::admission::UsageProber;
    use crate::work::dispatch::{
        Candidate, CandidateSource, FileScopeLock, Launch, Launcher, Refusal, ScopeLock,
        TickConfig, TickDeps,
    };
    use tempfile::{TempDir, tempdir};

    const ONE: &str = "github:o/one";
    const TWO: &str = "github:o/two";

    fn store() -> (TempDir, RunStore) {
        let dir = tempdir().unwrap();
        let store = RunStore::open(&dir.path().join("runs.db")).unwrap();
        (dir, store)
    }

    fn row(scope: &str, enabled: bool, repo_dir: Option<&str>) -> AutopilotScope {
        AutopilotScope {
            scope: scope.to_string(),
            enabled,
            repo_dir: repo_dir.map(str::to_string),
            updated_at: String::new(),
            last_tick_at: None,
        }
    }

    fn current(scope: &str, dir: &str) -> CurrentRepo {
        CurrentRepo {
            scope: scope.to_string(),
            repo_dir: PathBuf::from(dir),
        }
    }

    fn text(f: impl FnOnce(&mut Vec<u8>)) -> String {
        let mut out = Vec::new();
        f(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn selector_from_flags() {
        assert_eq!(
            ScopeSelector::from_flags(None, false),
            ScopeSelector::Current
        );
        assert_eq!(ScopeSelector::from_flags(None, true), ScopeSelector::All);
        assert_eq!(
            ScopeSelector::from_flags(Some(ONE.to_string()), false),
            ScopeSelector::Scope(ONE.to_string())
        );
    }

    // --- tick_targets ---

    #[test]
    fn the_current_selector_needs_a_current_repo() {
        let err = tick_targets(&[], &ScopeSelector::Current, None).unwrap_err();
        assert!(matches!(err, DispatchCliError::NoCurrentRepo));
    }

    #[test]
    fn the_current_selector_ticks_the_current_checkout() {
        let here = current(ONE, "/repos/one");
        let targets = tick_targets(&[], &ScopeSelector::Current, Some(&here)).unwrap();
        assert_eq!(
            targets,
            vec![Target {
                scope: ONE.to_string(),
                repo_dir: Some(PathBuf::from("/repos/one")),
            }]
        );
    }

    #[test]
    fn all_visits_only_scopes_that_are_on() {
        let rows = [
            row(ONE, true, Some("/repos/one")),
            row(TWO, false, Some("/repos/two")),
            row("github:o/three", true, None),
        ];
        let targets = tick_targets(&rows, &ScopeSelector::All, None).unwrap();
        assert_eq!(
            targets,
            vec![
                Target {
                    scope: ONE.to_string(),
                    repo_dir: Some(PathBuf::from("/repos/one")),
                },
                Target {
                    scope: "github:o/three".to_string(),
                    repo_dir: None,
                },
            ]
        );
    }

    #[test]
    fn a_named_scope_prefers_the_current_checkout_then_the_recorded_repo() {
        let rows = [
            row(ONE, true, Some("/old/one")),
            row(TWO, true, Some("/repos/two")),
        ];
        let here = current(ONE, "/repos/one");

        let mine = tick_targets(&rows, &ScopeSelector::Scope(ONE.to_string()), Some(&here));
        let other = tick_targets(&rows, &ScopeSelector::Scope(TWO.to_string()), Some(&here));

        assert_eq!(mine.unwrap()[0].repo_dir, Some(PathBuf::from("/repos/one")));
        assert_eq!(
            other.unwrap()[0].repo_dir,
            Some(PathBuf::from("/repos/two"))
        );
    }

    // --- tick output, with a canned ticker ---

    struct CannedTicker {
        reports: Vec<(&'static str, Result<TickReport, String>)>,
        calls: RefCell<Vec<(String, PathBuf)>>,
    }

    impl ScopeTicker for CannedTicker {
        fn tick(&self, scope: &str, repo_dir: &Path) -> Result<TickReport, String> {
            self.calls
                .borrow_mut()
                .push((scope.to_string(), repo_dir.to_path_buf()));
            self.reports
                .iter()
                .find(|(s, _)| *s == scope)
                .map(|(_, r)| r.clone())
                .unwrap()
        }
    }

    fn target(scope: &str, dir: Option<&str>) -> Target {
        Target {
            scope: scope.to_string(),
            repo_dir: dir.map(PathBuf::from),
        }
    }

    fn launched_report() -> TickReport {
        TickReport {
            launched: vec![
                Launch {
                    ticket: "GH-1".to_string(),
                    lane: "backend".to_string(),
                    stacked_on: None,
                },
                Launch {
                    ticket: "GH-2".to_string(),
                    lane: "backend".to_string(),
                    stacked_on: Some("gh-1-branch".to_string()),
                },
            ],
            refused: Some(Refusal::MaxLanes { running: 2, max: 2 }),
            skipped: None,
            notified: vec!["tm: GH-0 is awaiting your review".to_string()],
        }
    }

    #[test]
    fn tick_prints_launches_refusals_skips_and_errors_per_scope() {
        let ticker = CannedTicker {
            reports: vec![
                (ONE, Ok(launched_report())),
                (
                    TWO,
                    Ok(TickReport {
                        skipped: Some(Skip::Locked),
                        ..TickReport::default()
                    }),
                ),
                ("github:o/idle", Ok(TickReport::default())),
                ("github:o/broken", Err("config did not load".to_string())),
            ],
            calls: RefCell::new(Vec::new()),
        };
        let targets = [
            target(ONE, Some("/r/one")),
            target(TWO, Some("/r/two")),
            target("github:o/idle", Some("/r/idle")),
            target("github:o/broken", Some("/r/broken")),
            target("github:o/norepo", None),
        ];

        let mut ok = true;
        let out = text(|out| ok = tick(&targets, &ticker, false, out).unwrap());

        assert!(!ok);
        assert_eq!(
            out,
            "github:o/one: launched GH-1 in lane backend\n\
             github:o/one: launched GH-2 in lane backend (stacked on gh-1-branch)\n\
             github:o/one: not launching: 2 lanes running, [dispatch] max_lanes is 2\n\
             github:o/one: notified: tm: GH-0 is awaiting your review\n\
             github:o/two: skipped: another host is ticking this scope\n\
             github:o/idle: nothing to launch\n\
             github:o/broken: error: config did not load\n\
             github:o/norepo: error: no repo recorded for github:o/norepo; run `tm dispatch on` inside it once\n"
        );
        assert_eq!(ticker.calls.borrow().len(), 4, "no tick without a repo");
    }

    #[test]
    fn a_refused_or_skipped_tick_still_counts_as_ticked() {
        let ticker = CannedTicker {
            reports: vec![(
                ONE,
                Ok(TickReport {
                    skipped: Some(Skip::Disabled),
                    ..TickReport::default()
                }),
            )],
            calls: RefCell::new(Vec::new()),
        };
        let mut ok = false;
        let out = text(|out| ok = tick(&[target(ONE, Some("/r"))], &ticker, false, out).unwrap());

        assert!(ok);
        assert_eq!(out, "github:o/one: skipped: autopilot is off\n");
    }

    #[test]
    fn tick_with_no_targets_says_so() {
        let ticker = CannedTicker {
            reports: Vec::new(),
            calls: RefCell::new(Vec::new()),
        };
        let out = text(|out| assert!(tick(&[], &ticker, false, out).unwrap()));
        assert_eq!(out, "no scope has autopilot on\n");
        let out = text(|out| assert!(tick(&[], &ticker, true, out).unwrap()));
        assert_eq!(out, "[]\n");
    }

    #[test]
    fn tick_json_has_one_object_per_scope() {
        let ticker = CannedTicker {
            reports: vec![(ONE, Ok(launched_report()))],
            calls: RefCell::new(Vec::new()),
        };
        let targets = [target(ONE, Some("/r/one")), target(TWO, None)];
        let out = text(|out| {
            tick(&targets, &ticker, true, out).unwrap();
        });

        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            value,
            json!([
                {
                    "scope": ONE,
                    "launched": [
                        {"ticket": "GH-1", "lane": "backend", "stacked_on": null},
                        {"ticket": "GH-2", "lane": "backend", "stacked_on": "gh-1-branch"},
                    ],
                    "refused": "not launching: 2 lanes running, [dispatch] max_lanes is 2",
                    "skipped": null,
                    "notified": ["tm: GH-0 is awaiting your review"],
                    "error": null,
                },
                {
                    "scope": TWO,
                    "launched": [],
                    "refused": null,
                    "skipped": null,
                    "notified": [],
                    "error": "no repo recorded for github:o/two; run `tm dispatch on` inside it once",
                },
            ])
        );
    }

    // --- tick --all against the real dispatcher core ---

    struct OneReady;

    impl CandidateSource for OneReady {
        fn candidates(&self) -> Result<Vec<Candidate>, String> {
            Ok(vec![Candidate {
                key: "GH-1".to_string(),
                decision: ListDecision::Ready,
                unblocks: 0,
            }])
        }
    }

    struct RecordingLauncher<'a> {
        store: &'a RunStore,
        scope: String,
        launched: &'a RefCell<Vec<String>>,
    }

    impl Launcher for RecordingLauncher<'_> {
        fn launch(&self, lane: &str, ticket: &str) -> Result<(), String> {
            self.store
                .start_run(&StartRun {
                    ticket: ticket.to_string(),
                    scope: self.scope.clone(),
                    lane: lane.to_string(),
                    worktree: format!("/wt/{ticket}"),
                    branch: None,
                    pid: None,
                    kind: "lane".to_string(),
                    log_path: None,
                })
                .unwrap();
            self.launched
                .borrow_mut()
                .push(format!("{}/{ticket}", self.scope));
            Ok(())
        }
    }

    struct NoProbe;

    impl UsageProber for NoProbe {
        fn supports(&self, _agent: &str) -> bool {
            false
        }

        fn probe(&self, _agent: &str) -> Result<crate::agent::UsageSnapshot, String> {
            unreachable!("no probe expected")
        }
    }

    /// Runs the real [`crate::work::dispatch::tick`] with one ready ticket
    /// per scope and a launcher that records what it launched.
    struct CoreTicker<'a> {
        store: &'a RunStore,
        lock: &'a FileScopeLock,
        launched: RefCell<Vec<String>>,
    }

    impl ScopeTicker for CoreTicker<'_> {
        fn tick(&self, scope: &str, _repo_dir: &Path) -> Result<TickReport, String> {
            let launcher = RecordingLauncher {
                store: self.store,
                scope: scope.to_string(),
                launched: &self.launched,
            };
            let pressure = || crate::runs::footprint::MemoryPressure::Normal;
            let deps = TickDeps {
                store: self.store,
                lock: self.lock,
                candidates: &OneReady,
                launcher: &launcher,
                memory_pressure: &pressure,
                usage_prober: &NoProbe,
                now: 1_000_000,
                notifier: &crate::work::notify::NoNotifier,
            };
            let lanes = [LaneChoice {
                name: "backend".to_string(),
                repo: "r".to_string(),
            }];
            let memory = MemoryConfig::default();
            let usage = crate::config::UsageConfig::default();
            let cfg = TickConfig {
                scope,
                max_lanes: 10,
                max_launches_per_day: 100,
                max_fix_rounds: 3,
                stall_notify_mins: 30,
                lanes: &lanes,
                agent: Some("claude"),
                memory: &memory,
                usage: &usage,
            };
            crate::work::dispatch::tick(&deps, &cfg).map_err(|err| err.to_string())
        }
    }

    fn tick_all(store: &RunStore, ticker: &CoreTicker) -> String {
        let targets = tick_targets(
            &store.autopilot_scopes().unwrap(),
            &ScopeSelector::All,
            None,
        )
        .unwrap();
        text(|out| {
            tick(&targets, ticker, false, out).unwrap();
        })
    }

    #[test]
    fn tick_all_respects_each_scopes_flag_and_the_shared_lock() {
        let (dir, store) = store();
        store.set_autopilot_in(ONE, true, Some("/r/one")).unwrap();
        store.set_autopilot_in(TWO, true, Some("/r/two")).unwrap();
        store
            .set_autopilot_in("github:o/off", false, Some("/r/off"))
            .unwrap();
        let lock = FileScopeLock {
            dir: dir.path().join("locks"),
        };
        // The TUI host is mid-tick on TWO.
        let held = lock.try_lock(TWO).unwrap().unwrap();
        let ticker = CoreTicker {
            store: &store,
            lock: &lock,
            launched: RefCell::new(Vec::new()),
        };

        let out = tick_all(&store, &ticker);

        assert_eq!(*ticker.launched.borrow(), vec![format!("{ONE}/GH-1")]);
        assert_eq!(
            out,
            "github:o/one: launched GH-1 in lane backend\n\
             github:o/two: skipped: another host is ticking this scope\n"
        );
        drop(held);
    }

    #[test]
    fn off_all_is_a_kill_switch_for_the_next_tick() {
        let (dir, store) = store();
        store.set_autopilot_in(ONE, true, Some("/r/one")).unwrap();
        store.set_autopilot_in(TWO, true, Some("/r/two")).unwrap();
        let lock = FileScopeLock {
            dir: dir.path().join("locks"),
        };
        let ticker = CoreTicker {
            store: &store,
            lock: &lock,
            launched: RefCell::new(Vec::new()),
        };

        text(|out| set(&store, &ScopeSelector::All, false, None, out).unwrap());
        let out = tick_all(&store, &ticker);
        // A tick already holding a target list still re-reads the flag.
        let stale = [target(ONE, Some("/r/one"))];
        let stale_out = text(|out| {
            tick(&stale, &ticker, false, out).unwrap();
        });

        assert!(ticker.launched.borrow().is_empty());
        assert_eq!(out, "no scope has autopilot on\n");
        assert_eq!(stale_out, "github:o/one: skipped: autopilot is off\n");
    }

    // --- on / off ---

    #[test]
    fn on_in_a_repo_records_its_checkout() {
        let (_dir, store) = store();
        let here = current(ONE, "/repos/one");

        let out = text(|out| set(&store, &ScopeSelector::Current, true, Some(&here), out).unwrap());

        assert_eq!(out, "autopilot on for github:o/one (/repos/one)\n");
        let rows = store.autopilot_scopes().unwrap();
        assert!(rows[0].enabled);
        assert_eq!(rows[0].repo_dir.as_deref(), Some("/repos/one"));
    }

    #[test]
    fn on_outside_a_repo_needs_a_selector() {
        let (_dir, store) = store();
        let err = set(&store, &ScopeSelector::Current, true, None, &mut Vec::new()).unwrap_err();
        assert!(matches!(err, DispatchCliError::NoCurrentRepo));
    }

    #[test]
    fn a_named_scope_toggles_only_that_scope() {
        let (_dir, store) = store();
        store.set_autopilot_in(ONE, true, Some("/r/one")).unwrap();
        store.set_autopilot(TWO, true).unwrap();

        let out = text(|out| {
            set(
                &store,
                &ScopeSelector::Scope(ONE.to_string()),
                false,
                None,
                out,
            )
            .unwrap()
        });

        assert_eq!(out, "autopilot off for github:o/one\n");
        assert!(!store.autopilot_enabled(ONE).unwrap());
        assert!(store.autopilot_enabled(TWO).unwrap());
        assert_eq!(
            store.autopilot_scopes().unwrap()[0].repo_dir.as_deref(),
            Some("/r/one")
        );
    }

    #[test]
    fn off_all_turns_every_scope_off() {
        let (_dir, store) = store();
        store.set_autopilot(ONE, true).unwrap();
        store.set_autopilot(TWO, true).unwrap();

        let out = text(|out| set(&store, &ScopeSelector::All, false, None, out).unwrap());

        assert_eq!(
            out,
            "autopilot off for github:o/one\nautopilot off for github:o/two\n"
        );
        assert!(!store.autopilot_enabled(ONE).unwrap());
        assert!(!store.autopilot_enabled(TWO).unwrap());
    }

    // --- status ---

    #[test]
    fn status_with_no_scopes_says_how_to_start() {
        let (_dir, store) = store();
        let out = text(|out| status(&store, out).unwrap());
        assert_eq!(
            out,
            "no scopes registered; run `tm dispatch on` in a repo\n"
        );
    }

    #[test]
    fn status_lists_each_scope_with_lanes_last_tick_and_last_refusal() {
        let (_dir, store) = store();
        store.set_autopilot_in(ONE, true, Some("/r/one")).unwrap();
        store.set_autopilot(TWO, false).unwrap();
        store.stamp_autopilot_tick(ONE).unwrap();
        store
            .start_run(&StartRun {
                ticket: "GH-1".to_string(),
                scope: ONE.to_string(),
                lane: "backend".to_string(),
                worktree: "/wt/gh-1".to_string(),
                branch: None,
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        store
            .record_dispatch_event(ONE, None, REFUSED_EVENT, "not launching: max lanes")
            .unwrap();

        let out = text(|out| status(&store, out).unwrap());

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out}");
        assert!(
            lines[0].starts_with("github:o/one: on, 1 lanes running, last tick 20"),
            "{out}"
        );
        assert!(
            !lines[0].contains('.'),
            "timestamps lose their fraction: {out}"
        );
        assert_eq!(lines[1], "  repo: /r/one");
        assert!(lines[2].starts_with("  last refusal 20"), "{out}");
        assert!(lines[2].ends_with(": not launching: max lanes"), "{out}");
        assert_eq!(
            lines[3],
            "github:o/two: off, 0 lanes running, last tick never"
        );
    }

    // --- TickConfig parts ---

    fn config(repo: &str) -> Config {
        Config {
            backend: crate::config::BackendKind::Github,
            jira_base_url: String::new(),
            jira_email: String::new(),
            default_project_key: String::new(),
            github_repo: Some(repo.to_string()),
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
            require_audit: true,
            usage: Default::default(),
            dispatch: Default::default(),
            review: Default::default(),
        }
    }

    fn lane(repo: &str) -> LaneConfig {
        LaneConfig {
            repo: repo.to_string(),
            prompt_file: None,
            base_branch: None,
            model: None,
            max_turns: None,
            permission_mode: None,
        }
    }

    #[test]
    fn lane_choices_keep_lanes_on_the_same_backend_with_their_repo_keys() {
        let mut cfg = config("o/one");
        cfg.work
            .lanes
            .insert("a-other".to_string(), lane("/repos/two"));
        cfg.work
            .lanes
            .insert("b-main".to_string(), lane("/repos/one"));
        cfg.work
            .lanes
            .insert("c-also".to_string(), lane("/repos/one-too"));
        let github = |repo: &str| BackendIdentity::Github {
            repo: repo.to_string(),
        };
        let resolver = FakeBackendIdentityResolver::new()
            .with_identity("/repos/one", github("o/one"))
            .with_identity("/repos/one-too", github("o/one"))
            .with_identity("/repos/two", github("o/two"));

        let lanes = lane_choices(&cfg, &resolver);

        assert_eq!(
            lanes,
            vec![
                LaneChoice {
                    name: "b-main".to_string(),
                    repo: "one".to_string(),
                },
                LaneChoice {
                    name: "c-also".to_string(),
                    repo: "one-too".to_string(),
                },
            ]
        );
    }

    #[test]
    fn unattended_agent_skips_metered_runners() {
        let mut cfg = config("o/one");
        assert_eq!(unattended_agent(&cfg), Some("claude"));

        cfg.usage.claude.billing = Billing::Metered;
        assert_eq!(unattended_agent(&cfg), None);

        cfg.agent_fallbacks = vec![crate::config::AgentKind::Opencode];
        cfg.usage.opencode.billing = Billing::Subscription;
        assert_eq!(unattended_agent(&cfg), Some("opencode"));
    }
}
