//! The autopilot toggle on `tm board` and `tm overview` (GitHub issue
//! #116).
//!
//! `P` flips the runs.db autopilot flag for one scope: the board's own repo,
//! or the highlighted overview row's. The flag is read back from runs.db on
//! the view's poll ([`Cmd::LoadAutopilotStatus`]), so a flag another host
//! changed shows up here too. The host thread
//! ([`crate::work::autopilot::AutopilotHost`]) reports each tick as
//! [`Msg::AutopilotTicked`]: a failed tick's error is kept per scope until
//! the scope's next good tick, and launches are announced in the status
//! line.
//!
//! Pure like the rest of [`crate::tui::app`]: handlers take and return
//! [`App`] plus the [`Cmd`]s to run.
//!
//! [`Msg::AutopilotTicked`]: crate::tui::app::Msg::AutopilotTicked

use crate::runs::overview::OverviewRow;
use crate::tui::app::{App, Cmd, Screen};
use crate::work::autopilot::{AutopilotStatus, AutopilotTarget, TickOutcome};

/// `P`: ask to flip autopilot for the board's scope or the highlighted
/// overview row's.
pub fn toggle(mut app: App) -> (App, Vec<Cmd>) {
    let scope = match app.screen {
        Screen::Board => match &app.autopilot_scope {
            Some(scope) => scope.clone(),
            None => {
                app.status_line = "autopilot needs this repo's config to load".to_string();
                return (app, Vec::new());
            }
        },
        Screen::Overview => match app.selected_overview_row() {
            Some(row) if !row.key.scope.is_empty() => row.key.scope.clone(),
            Some(_) => {
                app.status_line = "autopilot: this row has no scope".to_string();
                return (app, Vec::new());
            }
            None => {
                app.status_line = "autopilot: no row selected".to_string();
                return (app, Vec::new());
            }
        },
        _ => return (app, Vec::new()),
    };
    let enabled = !status_for(&app, &scope).is_some_and(|status| status.enabled);
    (app, vec![Cmd::ToggleAutopilot { scope, enabled }])
}

/// [`Cmd::ToggleAutopilot`] finished: say so, show the new flag at once,
/// and reload the status.
pub fn toggled(
    mut app: App,
    scope: String,
    enabled: bool,
    result: Result<(), String>,
) -> (App, Vec<Cmd>) {
    let word = if enabled { "on" } else { "off" };
    if let Err(err) = result {
        app.status_line = format!("could not turn autopilot {word} for {scope}: {err}");
        return (app, Vec::new());
    }
    app.status_line = format!("autopilot {word} for {scope}");
    match app.autopilot.iter_mut().find(|s| s.scope == scope) {
        Some(status) => status.enabled = enabled,
        None => app.autopilot.push(AutopilotStatus {
            scope: scope.clone(),
            enabled,
            running_lanes: 0,
            max_lanes: 0,
            last_refusal: None,
        }),
    }
    if !enabled {
        app.autopilot_errors.remove(&scope);
    }
    let cmds = status_cmd(&app).into_iter().collect();
    (app, cmds)
}

/// [`Cmd::LoadAutopilotStatus`] finished.
pub fn status_loaded(mut app: App, result: Result<Vec<AutopilotStatus>, String>) -> App {
    match result {
        Ok(status) => app.autopilot = status,
        Err(err) => app.status_line = format!("could not read autopilot status: {err}"),
    }
    app
}

/// The host thread finished a tick: keep or clear the scope's error,
/// announce any launch, and reload the status after a tick that ran.
pub fn ticked(mut app: App, outcome: TickOutcome) -> (App, Vec<Cmd>) {
    let report = match outcome.result {
        Err(err) => {
            app.autopilot_errors.insert(outcome.scope, err);
            return (app, Vec::new());
        }
        Ok(report) => report,
    };
    app.autopilot_errors.remove(&outcome.scope);
    if report.skipped.is_some() {
        return (app, Vec::new());
    }
    if !report.launched.is_empty() {
        let launched: Vec<String> = report
            .launched
            .iter()
            .map(|launch| format!("{} (lane {})", launch.ticket, launch.lane))
            .collect();
        app.status_line = format!("autopilot launched {}", launched.join(", "));
    }
    let cmds = status_cmd(&app).into_iter().collect();
    (app, cmds)
}

/// The [`Cmd::LoadAutopilotStatus`] for the scopes the current view shows,
/// or `None` when it shows none.
pub fn status_cmd(app: &App) -> Option<Cmd> {
    let scopes = match app.screen {
        Screen::Board => app.autopilot_scope.iter().cloned().collect(),
        Screen::Overview => overview_scopes(&app.overview_rows),
        _ => Vec::new(),
    };
    (!scopes.is_empty()).then_some(Cmd::LoadAutopilotStatus { scopes })
}

/// After the overview's rows reload: the [`Cmd::SetAutopilotTargets`] for
/// their scopes, when those changed since the last one sent.
pub fn sync_targets(app: &mut App) -> Option<Cmd> {
    let targets = overview_targets(&app.overview_rows);
    if targets == app.autopilot_targets {
        return None;
    }
    app.autopilot_targets = targets.clone();
    Some(Cmd::SetAutopilotTargets(targets))
}

/// `scope`'s latest loaded status.
pub fn status_for<'a>(app: &'a App, scope: &str) -> Option<&'a AutopilotStatus> {
    app.autopilot.iter().find(|status| status.scope == scope)
}

/// `scope`'s status line, e.g. `autopilot on · lanes 1/2`, or `None` before
/// its first status load.
pub fn summary_for(app: &App, scope: &str) -> Option<String> {
    status_for(app, scope)
        .map(|status| status.summary(app.autopilot_errors.get(scope).map(String::as_str)))
}

/// The distinct, non-empty scopes of `rows`, sorted.
fn overview_scopes(rows: &[OverviewRow]) -> Vec<String> {
    let mut scopes: Vec<String> = rows
        .iter()
        .map(|row| row.key.scope.clone())
        .filter(|scope| !scope.is_empty())
        .collect();
    scopes.sort();
    scopes.dedup();
    scopes
}

/// One target per scope of `rows` with a resolved repo root, taking the
/// first row's root, sorted by scope. A scope with no resolved row can't
/// be ticked: the tick needs the repo's config.
fn overview_targets(rows: &[OverviewRow]) -> Vec<AutopilotTarget> {
    let mut targets: Vec<AutopilotTarget> = Vec::new();
    for row in rows {
        if row.key.scope.is_empty() || targets.iter().any(|t| t.scope == row.key.scope) {
            continue;
        }
        if let Some(root) = row.repo_root.path() {
            targets.push(AutopilotTarget {
                scope: row.key.scope.clone(),
                repo_root: root.to_path_buf(),
            });
        }
    }
    targets.sort_by(|a, b| a.scope.cmp(&b.scope));
    targets
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::runs::overview::{RepoRoot, Stage, TicketKey};
    use crate::tui::app::{Msg, update};
    use crate::work::dispatch::{Launch, Refusal, Skip, TickReport};

    fn row(scope: &str, ticket: &str, root: Option<&str>) -> OverviewRow {
        OverviewRow {
            key: TicketKey::new(scope, ticket),
            stage: Stage::Running,
            stage_age_secs: 60,
            run_id: Some(1),
            run_status: None,
            kind: Some("lane".to_string()),
            pr_url: None,
            worktree: None,
            repo_root: root.map_or(RepoRoot::Unresolved, |r| {
                RepoRoot::Recorded(PathBuf::from(r))
            }),
            auto_review_round: None,
            review_import: None,
            fix_rounds: 0,
        }
    }

    fn board_app(scope: Option<&str>) -> App {
        App {
            screen: Screen::Board,
            autopilot_scope: scope.map(str::to_string),
            ..App::new()
        }
    }

    fn overview_app(rows: Vec<OverviewRow>, selected: usize) -> App {
        App {
            screen: Screen::Overview,
            overview_rows: rows,
            overview_selected: selected,
            ..App::new()
        }
    }

    fn status(scope: &str, enabled: bool) -> AutopilotStatus {
        AutopilotStatus {
            scope: scope.to_string(),
            enabled,
            running_lanes: 1,
            max_lanes: 2,
            last_refusal: None,
        }
    }

    #[test]
    fn p_on_the_board_turns_its_scope_on_then_off() {
        let (app, cmds) = update(board_app(Some("github:o/r")), Msg::ToggleAutopilot);
        assert_eq!(
            cmds,
            [Cmd::ToggleAutopilot {
                scope: "github:o/r".to_string(),
                enabled: true,
            }]
        );

        let app = App {
            autopilot: vec![status("github:o/r", true)],
            ..app
        };
        let (_, cmds) = update(app, Msg::ToggleAutopilot);
        assert_eq!(
            cmds,
            [Cmd::ToggleAutopilot {
                scope: "github:o/r".to_string(),
                enabled: false,
            }]
        );
    }

    #[test]
    fn p_on_a_board_without_a_scope_explains_instead() {
        let (app, cmds) = update(board_app(None), Msg::ToggleAutopilot);
        assert!(cmds.is_empty());
        assert!(app.status_line.contains("config"), "{}", app.status_line);
    }

    #[test]
    fn p_on_the_overview_toggles_the_highlighted_rows_scope() {
        let rows = vec![
            row("github:o/a", "GH-1", None),
            row("github:o/b", "GH-2", None),
        ];
        let (_, cmds) = update(overview_app(rows, 1), Msg::ToggleAutopilot);
        assert_eq!(
            cmds,
            [Cmd::ToggleAutopilot {
                scope: "github:o/b".to_string(),
                enabled: true,
            }]
        );
    }

    #[test]
    fn p_on_an_unscoped_or_missing_overview_row_does_nothing() {
        let (app, cmds) = update(
            overview_app(vec![row("", "GH-1", None)], 0),
            Msg::ToggleAutopilot,
        );
        assert!(cmds.is_empty());
        assert!(app.status_line.contains("no scope"));

        let (app, cmds) = update(overview_app(Vec::new(), 0), Msg::ToggleAutopilot);
        assert!(cmds.is_empty());
        assert!(app.status_line.contains("no row"));
    }

    #[test]
    fn a_written_flag_shows_at_once_and_reloads_the_status() {
        let (app, cmds) = update(
            board_app(Some("s")),
            Msg::AutopilotToggled {
                scope: "s".to_string(),
                enabled: true,
                result: Ok(()),
            },
        );
        assert_eq!(app.status_line, "autopilot on for s");
        assert!(status_for(&app, "s").unwrap().enabled);
        assert_eq!(
            cmds,
            [Cmd::LoadAutopilotStatus {
                scopes: vec!["s".to_string()],
            }]
        );
    }

    #[test]
    fn turning_autopilot_off_clears_the_scopes_tick_error() {
        let mut app = board_app(Some("s"));
        app.autopilot = vec![status("s", true)];
        app.autopilot_errors
            .insert("s".to_string(), "gh: timed out".to_string());
        let (app, _) = update(
            app,
            Msg::AutopilotToggled {
                scope: "s".to_string(),
                enabled: false,
                result: Ok(()),
            },
        );
        assert!(!status_for(&app, "s").unwrap().enabled);
        assert!(app.autopilot_errors.is_empty());
    }

    #[test]
    fn a_failed_write_says_so_and_changes_nothing() {
        let (app, cmds) = update(
            board_app(Some("s")),
            Msg::AutopilotToggled {
                scope: "s".to_string(),
                enabled: true,
                result: Err("database is locked".to_string()),
            },
        );
        assert!(cmds.is_empty());
        assert!(app.autopilot.is_empty());
        assert_eq!(
            app.status_line,
            "could not turn autopilot on for s: database is locked"
        );
    }

    #[test]
    fn a_failed_tick_is_kept_until_the_next_good_one() {
        let mut app = board_app(Some("s"));
        app.autopilot = vec![status("s", true)];
        let (app, _) = update(
            app,
            Msg::AutopilotTicked(TickOutcome {
                scope: "s".to_string(),
                result: Err("gh: timed out".to_string()),
            }),
        );
        assert_eq!(
            summary_for(&app, "s").unwrap(),
            "autopilot on · lanes 1/2 · tick failed: gh: timed out"
        );

        let (app, _) = update(
            app,
            Msg::AutopilotTicked(TickOutcome {
                scope: "s".to_string(),
                result: Ok(TickReport {
                    refused: Some(Refusal::NoLane),
                    ..TickReport::default()
                }),
            }),
        );
        assert!(app.autopilot_errors.is_empty());
    }

    #[test]
    fn a_tick_that_launched_announces_it_and_reloads_the_status() {
        let (app, cmds) = update(
            board_app(Some("s")),
            Msg::AutopilotTicked(TickOutcome {
                scope: "s".to_string(),
                result: Ok(TickReport {
                    launched: vec![Launch {
                        ticket: "GH-7".to_string(),
                        lane: "a".to_string(),
                        stacked_on: None,
                    }],
                    ..TickReport::default()
                }),
            }),
        );
        assert_eq!(app.status_line, "autopilot launched GH-7 (lane a)");
        assert_eq!(
            cmds,
            [Cmd::LoadAutopilotStatus {
                scopes: vec!["s".to_string()],
            }]
        );
    }

    #[test]
    fn a_skipped_tick_reloads_nothing() {
        let (app, cmds) = update(
            board_app(Some("s")),
            Msg::AutopilotTicked(TickOutcome {
                scope: "s".to_string(),
                result: Ok(TickReport {
                    skipped: Some(Skip::Disabled),
                    ..TickReport::default()
                }),
            }),
        );
        assert!(cmds.is_empty());
        assert!(app.status_line.is_empty());
    }

    #[test]
    fn a_status_load_replaces_the_status_or_reports_the_error() {
        let (app, _) = update(
            board_app(Some("s")),
            Msg::AutopilotStatusLoaded(Ok(vec![status("s", true)])),
        );
        assert_eq!(app.autopilot, [status("s", true)]);

        let (app, _) = update(
            app,
            Msg::AutopilotStatusLoaded(Err("disk I/O error".to_string())),
        );
        assert_eq!(app.autopilot, [status("s", true)], "kept on failure");
        assert!(app.status_line.contains("disk I/O error"));
    }

    #[test]
    fn the_status_cmd_covers_the_boards_scope_or_the_overviews_scopes() {
        assert_eq!(status_cmd(&board_app(None)), None);
        assert_eq!(
            status_cmd(&board_app(Some("s"))),
            Some(Cmd::LoadAutopilotStatus {
                scopes: vec!["s".to_string()],
            })
        );
        let rows = vec![
            row("github:o/b", "GH-1", None),
            row("", "GH-2", None),
            row("github:o/a", "GH-3", None),
            row("github:o/b", "GH-4", None),
        ];
        assert_eq!(
            status_cmd(&overview_app(rows, 0)),
            Some(Cmd::LoadAutopilotStatus {
                scopes: vec!["github:o/a".to_string(), "github:o/b".to_string()],
            })
        );
    }

    #[test]
    fn the_status_reloads_every_eighth_tick_on_the_board_and_overview() {
        let app = App {
            watch_tick: 7,
            ..board_app(Some("s"))
        };
        let (_, cmds) = update(app, Msg::Tick);
        assert!(cmds.contains(&Cmd::LoadAutopilotStatus {
            scopes: vec!["s".to_string()],
        }));

        let app = App {
            watch_tick: 7,
            ..overview_app(vec![row("s", "GH-1", None)], 0)
        };
        let (_, cmds) = update(app, Msg::Tick);
        assert!(cmds.contains(&Cmd::LoadAutopilotStatus {
            scopes: vec!["s".to_string()],
        }));

        let app = App {
            watch_tick: 6,
            ..board_app(Some("s"))
        };
        let (_, cmds) = update(app, Msg::Tick);
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::LoadAutopilotStatus { .. }))
        );
    }

    #[test]
    fn overview_targets_are_sent_only_when_they_change() {
        let rows = vec![
            row("github:o/b", "GH-1", None),
            row("github:o/b", "GH-2", Some("/src/b")),
            row("github:o/a", "GH-3", Some("/src/a")),
            row("github:o/c", "GH-4", None),
        ];
        let mut app = overview_app(rows, 0);
        let expected = vec![
            AutopilotTarget {
                scope: "github:o/a".to_string(),
                repo_root: PathBuf::from("/src/a"),
            },
            AutopilotTarget {
                scope: "github:o/b".to_string(),
                repo_root: PathBuf::from("/src/b"),
            },
        ];
        assert_eq!(
            sync_targets(&mut app),
            Some(Cmd::SetAutopilotTargets(expected))
        );
        assert_eq!(sync_targets(&mut app), None);
    }
}
