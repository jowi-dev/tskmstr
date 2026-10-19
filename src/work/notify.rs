//! Autopilot desktop notifications (GitHub issue #121).
//!
//! A dispatch tick ([`crate::work::dispatch::tick`]) asks three questions of
//! the run store and notifies the operator when the answer changes:
//!
//! - a ticket entered "awaiting review" ([`awaiting_review`]): the machine
//!   is done and it is the operator's turn;
//! - a ticket hit the fix-round cap: auto-review stopped at its round cap,
//!   or the ticket has `[dispatch] max_fix_rounds` fix passes;
//! - dispatch has refused for the same reason for longer than
//!   `[dispatch] stall_notify_mins`.
//!
//! Each [`Notice`] carries a transition key. [`send`] claims the key in
//! runs.db before notifying ([`RunStore::claim_notification`]), so a
//! transition notifies once however many ticks, or hosts, see it. A ticket
//! that re-enters a state after new runs gets a new key and notifies again.
//!
//! The backend is a [`Notifier`]. Production uses [`DesktopNotifier`],
//! which runs `notify-send` (Linux) or `osascript` (macOS) and does nothing
//! when neither is installed.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::runs::overview::{
    ACTIVE_SCOPE_WINDOW_SECS, OverviewRun, REVIEW_FIX_KIND, awaiting_review,
};
use crate::runs::{RefusalStreak, RunStatus, RunStore, RunStoreError};
use crate::work::auto_review::RoundVerdict;

/// [`Notice::reason`] of a ticket entering awaiting review.
pub const AWAITING_REVIEW: &str = "awaiting_review";
/// [`Notice::reason`] of a ticket hitting the fix-round cap.
pub const FIX_CAP: &str = "fix_cap";
/// [`Notice::reason`] of a stalled dispatcher.
pub const STALL: &str = "stall";

/// Shows one desktop notification.
pub trait Notifier {
    /// Shows `title` and `body`. Failures are swallowed: a missing or
    /// broken notification daemon must never fail a dispatch tick.
    fn notify(&self, title: &str, body: &str);
}

/// A [`Notifier`] that shows nothing.
pub struct NoNotifier;

impl Notifier for NoNotifier {
    fn notify(&self, _title: &str, _body: &str) {}
}

/// The program [`DesktopNotifier`] runs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Backend {
    /// libnotify's `notify-send TITLE BODY`.
    NotifySend(PathBuf),
    /// `osascript -e 'display notification ...'`.
    Osascript(PathBuf),
}

/// [`Notifier`] over the platform's notification command, found on `PATH`
/// once at construction. With no command found, [`Notifier::notify`] does
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopNotifier {
    backend: Option<Backend>,
}

impl DesktopNotifier {
    /// The notifier for this process's `PATH` and platform.
    pub fn detect() -> Self {
        Self::from_path(
            std::env::var_os("PATH").as_deref(),
            cfg!(target_os = "macos"),
        )
    }

    /// The notifier for `path` (a `PATH` value): `osascript` when `macos`,
    /// `notify-send` otherwise.
    pub fn from_path(path: Option<&OsStr>, macos: bool) -> Self {
        let program = if macos { "osascript" } else { "notify-send" };
        let found = path.and_then(|path| {
            std::env::split_paths(path)
                .map(|dir| dir.join(program))
                .find(|candidate| candidate.is_file())
        });
        let backend = found.map(|program| {
            if macos {
                Backend::Osascript(program)
            } else {
                Backend::NotifySend(program)
            }
        });
        DesktopNotifier { backend }
    }

    /// Whether a notification command was found.
    pub fn available(&self) -> bool {
        self.backend.is_some()
    }
}

impl Notifier for DesktopNotifier {
    fn notify(&self, title: &str, body: &str) {
        let Some(backend) = &self.backend else {
            return;
        };
        let (program, args): (&Path, Vec<String>) = match backend {
            Backend::NotifySend(program) => (
                program,
                vec![
                    "--app-name=tm".to_string(),
                    title.to_string(),
                    body.to_string(),
                ],
            ),
            Backend::Osascript(program) => (
                program,
                vec![
                    "-e".to_string(),
                    format!(
                        "display notification {} with title {}",
                        applescript_string(body),
                        applescript_string(title)
                    ),
                ],
            ),
        };
        let _ = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// `text` as an AppleScript string literal.
fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// One notification a tick may send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// The ticket it is about, or `""` for a scope-wide one.
    pub ticket: String,
    /// [`AWAITING_REVIEW`], [`FIX_CAP`] or [`STALL`].
    pub reason: &'static str,
    /// The transition's key: it is the same on every tick that sees the
    /// same transition, and changes when the state is entered again.
    pub key: String,
    /// The notification's title.
    pub title: String,
    /// The notification's body.
    pub body: String,
}

/// The awaiting-review and fix-cap notices for `scope`'s tickets, from
/// [`RunStore::overview_runs`] (newest first).
///
/// A ticket's key is its newest run's id: no run starts while a ticket
/// waits on the operator, so the key holds still, and the next round of
/// work makes a new one. Tickets whose newest run last changed state more
/// than [`ACTIVE_SCOPE_WINDOW_SECS`] ago are skipped, so turning
/// notifications on doesn't announce every old ticket.
pub fn ticket_notices(runs: &[OverviewRun], scope: &str, max_fix_rounds: u32) -> Vec<Notice> {
    let mut by_ticket: Vec<(&str, Vec<&OverviewRun>)> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for run in runs.iter().filter(|run| run.scope == scope) {
        let at = *index.entry(&run.ticket).or_insert_with(|| {
            by_ticket.push((&run.ticket, Vec::new()));
            by_ticket.len() - 1
        });
        by_ticket[at].1.push(run);
    }

    let capped = format!(" {}", RoundVerdict::Capped.as_str());
    let mut notices = Vec::new();
    for (ticket, runs) in by_ticket {
        let newest = runs[0];
        if newest.state_age_secs > ACTIVE_SCOPE_WINDOW_SECS {
            continue;
        }
        let key = newest.id.to_string();
        let pr = runs.iter().find_map(|run| run.pr_url.as_deref());
        if awaiting_review(&runs) {
            notices.push(Notice {
                ticket: ticket.to_string(),
                reason: AWAITING_REVIEW,
                key: key.clone(),
                title: format!("tm: {ticket} is awaiting your review"),
                body: pr.unwrap_or("PR open, bots reviewed").to_string(),
            });
        }

        let live = runs.iter().any(|run| {
            matches!(
                run.status,
                RunStatus::Running | RunStatus::Queued | RunStatus::Hibernated
            )
        });
        if live || pr.is_none() || runs.iter().any(|run| run.pr_closed) {
            continue;
        }
        let fix_rounds = runs
            .iter()
            .filter(|run| run.kind == REVIEW_FIX_KIND)
            .count();
        let auto_capped = runs
            .iter()
            .find_map(|run| run.auto_review_round.as_deref())
            .is_some_and(|detail| detail.ends_with(&capped));
        if auto_capped || fix_rounds >= max_fix_rounds as usize {
            notices.push(Notice {
                ticket: ticket.to_string(),
                reason: FIX_CAP,
                key,
                title: format!("tm: {ticket} hit the fix-round cap"),
                body: format!(
                    "{fix_rounds} fix rounds; [dispatch] max_fix_rounds is {max_fix_rounds}"
                ),
            });
        }
    }
    notices
}

/// The stall notice for `scope`, when its newest dispatch events are
/// refusals for one reason ([`RunStore::refusal_streak`]) that began at
/// least `stall_secs` before `now` (unix seconds). The key is the streak's
/// first refusal, so one stall notifies once.
pub fn stall_notice(
    scope: &str,
    streak: Option<&RefusalStreak>,
    now: i64,
    stall_secs: i64,
) -> Option<Notice> {
    let streak = streak?;
    let stalled_for = now - streak.since_unix;
    if stalled_for < stall_secs {
        return None;
    }
    Some(Notice {
        ticket: String::new(),
        reason: STALL,
        key: streak.first_id.to_string(),
        title: format!("tm autopilot stalled for {} min: {scope}", stalled_for / 60),
        body: streak.detail.clone(),
    })
}

/// Sends each of `notices` whose transition `scope` hasn't notified yet,
/// claiming it first so no other tick sends it again. Returns the titles
/// sent, in order.
///
/// # Errors
///
/// Returns the store's error when a claim fails.
pub fn send(
    store: &RunStore,
    notifier: &dyn Notifier,
    scope: &str,
    notices: &[Notice],
) -> Result<Vec<String>, RunStoreError> {
    let mut sent = Vec::new();
    for notice in notices {
        if store.claim_notification(scope, &notice.ticket, notice.reason, &notice.key)? {
            notifier.notify(&notice.title, &notice.body);
            sent.push(notice.title.clone());
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::runs::{FinishRun, StartRun};
    use crate::work::auto_review::ROUND_EVENT;

    const SCOPE: &str = "github:o/r";

    /// Records every notification it is asked to show.
    #[derive(Default)]
    struct Recorder(RefCell<Vec<(String, String)>>);

    impl Notifier for Recorder {
        fn notify(&self, title: &str, body: &str) {
            self.0
                .borrow_mut()
                .push((title.to_string(), body.to_string()));
        }
    }

    fn open_store() -> (TempDir, RunStore) {
        let dir = tempdir().unwrap();
        let store = RunStore::open(&dir.path().join("runs.db")).unwrap();
        (dir, store)
    }

    fn start(store: &RunStore, ticket: &str, kind: &str) -> i64 {
        store
            .start_run(&StartRun {
                ticket: ticket.to_string(),
                scope: SCOPE.to_string(),
                lane: "backend".to_string(),
                worktree: format!("/wt/{ticket}"),
                branch: None,
                pid: None,
                kind: kind.to_string(),
                log_path: None,
            })
            .unwrap()
    }

    fn finish(store: &RunStore, id: i64, status: RunStatus, pr_url: Option<&str>) {
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
                    pr_url: pr_url.map(str::to_string),
                    transcript: None,
                    model_usage: None,
                    findings_count: None,
                },
            )
            .unwrap();
    }

    /// A lane that opened a PR and a watch that finished clean: awaiting
    /// review. Returns the watch run's id.
    fn awaiting(store: &RunStore, ticket: &str) -> i64 {
        let lane = start(store, ticket, "lane");
        finish(store, lane, RunStatus::Done, Some("https://gh/pr/1"));
        let watch = start(store, ticket, "review-watch");
        finish(store, watch, RunStatus::Done, None);
        watch
    }

    fn reasons(notices: &[Notice]) -> Vec<(&str, &str)> {
        notices
            .iter()
            .map(|n| (n.ticket.as_str(), n.reason))
            .collect()
    }

    #[test]
    fn a_ticket_awaiting_review_gets_one_notice_keyed_by_its_newest_run() {
        let (_dir, store) = open_store();
        let watch = awaiting(&store, "GH-1");
        let lane = start(&store, "GH-2", "lane");
        finish(&store, lane, RunStatus::Done, Some("https://gh/pr/2"));

        let notices = ticket_notices(&store.overview_runs().unwrap(), SCOPE, 3);

        assert_eq!(reasons(&notices), vec![("GH-1", AWAITING_REVIEW)]);
        assert_eq!(notices[0].key, watch.to_string());
        assert_eq!(notices[0].body, "https://gh/pr/1");
        assert!(notices[0].title.contains("GH-1"));
    }

    #[test]
    fn other_scopes_are_ignored() {
        let (_dir, store) = open_store();
        awaiting(&store, "GH-1");

        assert!(ticket_notices(&store.overview_runs().unwrap(), "github:o/x", 3).is_empty());
    }

    #[test]
    fn a_ticket_at_max_fix_rounds_gets_a_fix_cap_notice() {
        let (_dir, store) = open_store();
        let lane = start(&store, "GH-1", "lane");
        finish(&store, lane, RunStatus::Done, Some("https://gh/pr/1"));
        for _ in 0..2 {
            let fix = start(&store, "GH-1", REVIEW_FIX_KIND);
            finish(&store, fix, RunStatus::Done, None);
        }
        let runs = store.overview_runs().unwrap();

        assert!(ticket_notices(&runs, SCOPE, 3).is_empty());
        let notices = ticket_notices(&runs, SCOPE, 2);
        assert_eq!(reasons(&notices), vec![("GH-1", FIX_CAP)]);
        assert_eq!(
            notices[0].body,
            "2 fix rounds; [dispatch] max_fix_rounds is 2"
        );
    }

    #[test]
    fn a_capped_auto_review_round_gets_a_fix_cap_notice() {
        let (_dir, store) = open_store();
        let lane = start(&store, "GH-1", "lane");
        finish(&store, lane, RunStatus::Done, Some("https://gh/pr/1"));
        let review = start(&store, "GH-1", "auto-review");
        store
            .add_event(review, ROUND_EVENT, Some("2/2 capped"))
            .unwrap();
        finish(&store, review, RunStatus::Done, None);

        let notices = ticket_notices(&store.overview_runs().unwrap(), SCOPE, 3);

        assert_eq!(reasons(&notices), vec![("GH-1", FIX_CAP)]);
    }

    #[test]
    fn a_live_or_closed_ticket_gets_no_fix_cap_notice() {
        let (_dir, store) = open_store();
        let lane = start(&store, "GH-1", "lane");
        finish(&store, lane, RunStatus::Done, Some("https://gh/pr/1"));
        start(&store, "GH-1", REVIEW_FIX_KIND);

        assert!(
            ticket_notices(&store.overview_runs().unwrap(), SCOPE, 1).is_empty(),
            "a fix pass still running"
        );

        let (_dir2, closed) = open_store();
        let store = closed;
        let lane = start(&store, "GH-1", "lane");
        finish(&store, lane, RunStatus::Done, Some("https://gh/pr/1"));
        let fix = start(&store, "GH-1", REVIEW_FIX_KIND);
        finish(&store, fix, RunStatus::Done, None);
        store.add_event(fix, "pr_closed", None).unwrap();

        assert!(
            ticket_notices(&store.overview_runs().unwrap(), SCOPE, 1).is_empty(),
            "the PR merged or closed"
        );
    }

    fn streak(first_id: i64, since_unix: i64) -> RefusalStreak {
        RefusalStreak {
            reason: "max_lanes".to_string(),
            first_id,
            since_unix,
            detail: "not launching: 1 lanes running".to_string(),
        }
    }

    #[test]
    fn a_stall_notifies_once_it_outlasts_the_threshold() {
        assert_eq!(stall_notice(SCOPE, None, 10_000, 600), None);
        assert_eq!(
            stall_notice(SCOPE, Some(&streak(4, 10_000 - 599)), 10_000, 600),
            None
        );

        let notice = stall_notice(SCOPE, Some(&streak(4, 10_000 - 1_800)), 10_000, 600).unwrap();
        assert_eq!(notice.ticket, "");
        assert_eq!(notice.reason, STALL);
        assert_eq!(notice.key, "4");
        assert_eq!(notice.title, "tm autopilot stalled for 30 min: github:o/r");
        assert_eq!(notice.body, "not launching: 1 lanes running");
    }

    #[test]
    fn send_notifies_each_transition_once() {
        let (_dir, store) = open_store();
        let recorder = Recorder::default();
        let notice = |key: &str| Notice {
            ticket: "GH-1".to_string(),
            reason: AWAITING_REVIEW,
            key: key.to_string(),
            title: format!("t{key}"),
            body: "b".to_string(),
        };

        assert_eq!(
            send(&store, &recorder, SCOPE, &[notice("1")]).unwrap(),
            vec!["t1"]
        );
        assert!(
            send(&store, &recorder, SCOPE, &[notice("1")])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            send(&store, &recorder, SCOPE, &[notice("2")]).unwrap(),
            vec!["t2"]
        );
        assert_eq!(
            *recorder.0.borrow(),
            vec![
                ("t1".to_string(), "b".to_string()),
                ("t2".to_string(), "b".to_string())
            ]
        );
    }

    /// A fake notification command in `dir` that writes its arguments, one
    /// per line, to `dir/args`.
    fn fake_command(dir: &Path, name: &str) {
        let script = dir.join(name);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
                dir.join("args").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_desktop_notifier_runs_notify_send_from_path() {
        let dir = tempdir().unwrap();
        fake_command(dir.path(), "notify-send");
        let notifier = DesktopNotifier::from_path(Some(dir.path().as_os_str()), false);

        assert!(notifier.available());
        notifier.notify("GH-1 ready", "https://gh/pr/1");

        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        assert_eq!(args, "--app-name=tm\nGH-1 ready\nhttps://gh/pr/1\n");
    }

    #[test]
    fn the_desktop_notifier_runs_osascript_on_macos() {
        let dir = tempdir().unwrap();
        fake_command(dir.path(), "osascript");
        let notifier = DesktopNotifier::from_path(Some(dir.path().as_os_str()), true);

        notifier.notify("say \"hi\"", "a\\b");

        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        assert_eq!(
            args,
            "-e\ndisplay notification \"a\\\\b\" with title \"say \\\"hi\\\"\"\n"
        );
    }

    #[test]
    fn the_desktop_notifier_is_a_no_op_without_a_command() {
        let dir = tempdir().unwrap();
        for path in [None, Some(dir.path().as_os_str())] {
            let notifier = DesktopNotifier::from_path(path, false);
            assert!(!notifier.available());
            notifier.notify("title", "body");
        }
    }
}
