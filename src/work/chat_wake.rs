//! Waking idle lanes that have unread thatch chat mail (GitHub issue #224).
//!
//! A Claude Code session reads its thatch chat mail only when its next
//! prompt arrives: thatch prints the "you have unread mail" line from its
//! `UserPromptSubmit` hook. An autonomous lane gets one prompt at launch and
//! none after, so mail sent to it while it sits idle waits until a human
//! types something. [`sweep`] types that something: a fixed one-line
//! [`WAKE_PROMPT`] into the lane's tmux pane.
//!
//! Waking is tm's job because tm owns the process and its pane; reading the
//! mail stays thatch's (ADR-0011). tm learns about the mail only through
//! `thatch chat list` and `thatch chat tail` output.
//!
//! # Who gets woken
//!
//! A run in the dispatched scope that is:
//!
//! - **idle**: running, and its latest event is the `await` the session
//!   hooks write when the agent stops and waits for input
//!   ([`crate::runs::is_awaiting_input`]). A run mid-turn is never typed
//!   into.
//! - **a session that reads mail at its next prompt**: thatch's `mcp` rows
//!   ([`Mailbox::reads_at_next_prompt`]). thatch's opencode plugin wakes
//!   opencode sessions itself.
//! - **holding new mail**: its unread count is above zero and its newest
//!   unread message is not the one the last wake was for. One batch of
//!   mail wakes a lane once, even when the agent goes idle again without
//!   reading it.
//! - **in a known pane**: it has a tmux session and a
//!   [`crate::work::hibernate::LaunchRecord`] naming its window.
//!
//! Each wake records a [`WAKE_EVENT`] on the run, whose detail is the id of
//! the newest unread message it was for. The event also replaces the
//! `await` as the run's latest event, so the run stops reading as idle
//! until the agent waits again.

use crate::runs::overview::OverviewRun;
use crate::runs::{RunStatus, RunStore, RunStoreError};
use crate::work::hibernate::launch_record;
use crate::work::thatch_chat::{ThatchRunner, mailboxes, newest_unread_to, tail};
use crate::work::tmux::TmuxOps;

/// `run_events.kind` of the event a wake records.
pub const WAKE_EVENT: &str = "chat_wake";

/// What a wake types into the lane's pane.
pub const WAKE_PROMPT: &str = "You have unread thatch chat mail; read it and continue.";

/// What [`sweep`] types into panes with and reads mail through.
pub struct WakeDeps<'a> {
    /// Types the prompt into the lane's pane.
    pub tmux: &'a dyn TmuxOps,
    /// Reads unread counts and messages.
    pub thatch: &'a dyn ThatchRunner,
}

/// One lane [`sweep`] woke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatWake {
    /// Run row id.
    pub run_id: i64,
    /// Ticket key.
    pub ticket: String,
}

/// Wake every idle lane in `scope` that has new unread chat mail; see the
/// module docs for the rule. `runs` is the tick's
/// [`RunStore::overview_runs`] snapshot.
///
/// thatch is asked at most twice per sweep (one roster lookup for every
/// idle lane, one tail snapshot when any has mail), and not at all when no
/// lane is idle. A missing `thatch` or a failing tmux call wakes nothing
/// and is not an error.
///
/// # Errors
///
/// A [`RunStore`] failure.
pub fn sweep(
    store: &RunStore,
    tmux: &dyn TmuxOps,
    thatch: &dyn ThatchRunner,
    runs: &[OverviewRun],
    scope: &str,
) -> Result<Vec<ChatWake>, RunStoreError> {
    let idle: Vec<(&OverviewRun, &str)> = runs
        .iter()
        .filter(|run| run.scope == scope && run.status == RunStatus::Running && run.awaiting_input)
        .filter_map(|run| Some((run, run.session_id.as_deref()?)))
        .collect();
    if idle.is_empty() {
        return Ok(Vec::new());
    }
    let host_ids: Vec<String> = idle.iter().map(|(_, id)| id.to_string()).collect();
    let boxes = mailboxes(thatch, &host_ids);
    let with_mail: Vec<(&OverviewRun, &str)> = idle
        .into_iter()
        .filter_map(|(run, id)| {
            let mailbox = boxes.get(id)?;
            (mailbox.reads_at_next_prompt() && mailbox.unread > 0)
                .then_some((run, mailbox.name.as_str()))
        })
        .collect();
    if with_mail.is_empty() {
        return Ok(Vec::new());
    }

    let messages = tail(thatch, None);
    let mut woken = Vec::new();
    for (run, name) in with_mail {
        // Mail older than the tail snapshot has no id to tell batches
        // apart; 0 still wakes the lane once for it.
        let batch = newest_unread_to(name, &messages).unwrap_or(0);
        if last_woken_batch(store, run.id)? == Some(batch) {
            continue;
        }
        let Some(session) = store.run_by_id(run.id)?.and_then(|r| r.tmux_session) else {
            continue;
        };
        let Some(record) = launch_record(store, run.id)? else {
            continue;
        };
        if tmux
            .send_keys(&session, &record.window, WAKE_PROMPT)
            .is_err()
        {
            continue;
        }
        store.add_event(run.id, WAKE_EVENT, Some(&batch.to_string()))?;
        woken.push(ChatWake {
            run_id: run.id,
            ticket: run.ticket.clone(),
        });
    }
    Ok(woken)
}

/// The message id the run's latest [`WAKE_EVENT`] was for, if it has one.
fn last_woken_batch(store: &RunStore, run_id: i64) -> Result<Option<i64>, RunStoreError> {
    Ok(store
        .events_for_run(run_id)?
        .iter()
        .rev()
        .find(|event| event.kind == WAKE_EVENT)
        .and_then(|event| event.detail.as_deref()?.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ResumeSpec;
    use crate::runs::StartRun;
    use crate::work::hibernate::{LaunchRecord, record_launch};
    use crate::work::thatch_chat::FakeThatchRunner;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall};
    use tempfile::tempdir;

    const SCOPE: &str = "github:jowi-dev/tskmstr";
    const SESSION: &str = "tm-tskmstr-gh-1";

    fn open_store(dir: &std::path::Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).unwrap()
    }

    /// A launch-recorded lane run hosted in [`SESSION`]'s `work` window,
    /// registered as agent session `sess-1`, whose latest event is
    /// `last_event`.
    fn lane(store: &RunStore, scope: &str, last_event: &str) -> i64 {
        let id = store
            .start_run(&StartRun {
                ticket: "GH-1".to_string(),
                scope: scope.to_string(),
                lane: "dev".to_string(),
                worktree: "/wt/gh-1".to_string(),
                branch: None,
                pid: Some(4242),
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();
        record_launch(
            store,
            id,
            &LaunchRecord {
                window: "work".to_string(),
                resume: ResumeSpec {
                    program: "claude".to_string(),
                    resume_flag: "--resume".to_string(),
                    args: Vec::new(),
                    env_remove: Vec::new(),
                },
            },
        )
        .unwrap();
        store.update_session_id(id, "sess-1").unwrap();
        store.update_tmux_session(id, SESSION).unwrap();
        store.add_event(id, last_event, None).unwrap();
        id
    }

    /// thatch output serving both the roster lookup and the tail snapshot
    /// (each parser skips the other's lines): `sess-1` is chat session
    /// `npc-1` of `host_kind`, with one unread message per id in `unread`.
    fn thatch(host_kind: &str, unread: &[i64]) -> FakeThatchRunner {
        let mut out = format!(
            r#"{{"name":"npc-1","session_id":"x","host_kind":"{host_kind}","project":"jowi-dev/tskmstr","topic":null,"status":"idle","last_seen":"2026-11-19T00:00:00Z","unread":{},"host_id":"sess-1"}}"#,
            unread.len()
        );
        out.push('\n');
        for id in unread {
            out.push_str(&format!(
                r#"{{"event":"sent","at":"2026-11-19T00:00:0{id}Z","id":{id},"from":"boss","to":"npc-1","broadcast":false,"body":"hi"}}"#
            ));
            out.push('\n');
        }
        FakeThatchRunner::returning(&out)
    }

    fn sent_keys(tmux: &FakeTmuxOps) -> Vec<TmuxCall> {
        tmux.calls()
            .into_iter()
            .filter(|call| matches!(call, TmuxCall::SendKeys { .. }))
            .collect()
    }

    #[test]
    fn an_idle_lane_with_unread_mail_gets_the_wake_prompt_in_its_pane() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = lane(&store, SCOPE, "await");
        let tmux = FakeTmuxOps::new();

        let woken = sweep(
            &store,
            &tmux,
            &thatch("mcp", &[7]),
            &store.overview_runs().unwrap(),
            SCOPE,
        )
        .unwrap();

        assert_eq!(
            woken,
            vec![ChatWake {
                run_id: id,
                ticket: "GH-1".to_string()
            }]
        );
        assert_eq!(
            sent_keys(&tmux),
            vec![TmuxCall::SendKeys {
                name: SESSION.to_string(),
                window: "work".to_string(),
                text: WAKE_PROMPT.to_string(),
            }]
        );
        let last = store.events_for_run(id).unwrap().pop().unwrap();
        assert_eq!(last.kind, WAKE_EVENT);
        assert_eq!(last.detail.as_deref(), Some("7"));
    }

    #[test]
    fn a_lane_mid_turn_is_never_typed_into() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        lane(&store, SCOPE, "tool");
        let tmux = FakeTmuxOps::new();
        let runner = thatch("mcp", &[7]);

        let woken = sweep(
            &store,
            &tmux,
            &runner,
            &store.overview_runs().unwrap(),
            SCOPE,
        )
        .unwrap();

        assert!(woken.is_empty());
        assert!(sent_keys(&tmux).is_empty());
        assert!(
            runner.calls.borrow().is_empty(),
            "no idle lane, so thatch is not asked"
        );
    }

    #[test]
    fn one_batch_of_mail_wakes_a_lane_once_and_new_mail_wakes_it_again() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        let id = lane(&store, SCOPE, "await");
        let tmux = FakeTmuxOps::new();
        let wake = |runner: &FakeThatchRunner| {
            sweep(
                &store,
                &tmux,
                runner,
                &store.overview_runs().unwrap(),
                SCOPE,
            )
            .unwrap()
            .len()
        };

        assert_eq!(wake(&thatch("mcp", &[7])), 1);
        // The agent took a turn without reading the mail, and waits again.
        store.add_event(id, "await", None).unwrap();
        assert_eq!(wake(&thatch("mcp", &[7])), 0, "same batch, no second wake");
        assert_eq!(wake(&thatch("mcp", &[7, 8])), 1, "a new message wakes it");
        assert_eq!(sent_keys(&tmux).len(), 2);
    }

    #[test]
    fn a_lane_that_just_got_woken_is_not_woken_again_before_it_waits() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        lane(&store, SCOPE, "await");
        let tmux = FakeTmuxOps::new();

        let first = sweep(
            &store,
            &tmux,
            &thatch("mcp", &[7]),
            &store.overview_runs().unwrap(),
            SCOPE,
        )
        .unwrap();
        let second = sweep(
            &store,
            &tmux,
            &thatch("mcp", &[7, 8]),
            &store.overview_runs().unwrap(),
            SCOPE,
        )
        .unwrap();

        assert_eq!(first.len(), 1);
        assert!(second.is_empty(), "the wake event replaced the await");
    }

    #[test]
    fn opencode_lanes_and_lanes_without_mail_are_left_alone() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        lane(&store, SCOPE, "await");
        let tmux = FakeTmuxOps::new();
        let runs = store.overview_runs().unwrap();

        assert!(
            sweep(&store, &tmux, &thatch("opencode", &[7]), &runs, SCOPE)
                .unwrap()
                .is_empty()
        );
        assert!(
            sweep(&store, &tmux, &thatch("mcp", &[]), &runs, SCOPE)
                .unwrap()
                .is_empty()
        );
        assert!(sent_keys(&tmux).is_empty());
    }

    #[test]
    fn lanes_in_other_scopes_and_without_thatch_are_left_alone() {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path());
        lane(&store, "github:other/repo", "await");
        let tmux = FakeTmuxOps::new();
        let runs = store.overview_runs().unwrap();

        assert!(
            sweep(&store, &tmux, &thatch("mcp", &[7]), &runs, SCOPE)
                .unwrap()
                .is_empty()
        );
        assert!(
            sweep(
                &store,
                &tmux,
                &FakeThatchRunner::missing(),
                &runs,
                "github:other/repo"
            )
            .unwrap()
            .is_empty()
        );
        assert!(sent_keys(&tmux).is_empty());
    }
}
