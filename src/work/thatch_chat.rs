//! `thatch chat` seam: the board's broadcast key (GitHub issue #225) and
//! the lane chat shown in the run detail window and the overview (GitHub
//! issue #223).
//!
//! `thatch chat broadcast <msg> --project P` delivers one message from the
//! human at the terminal to every live agent session registered under
//! project `P`. thatch labels a session's project with the `owner/name` slug
//! of its repo's `origin` remote, so [`detect_project`] derives the same
//! slug for the board's repo.
//!
//! Lane chat is read-only and reaches thatch only through its CLI output
//! (ADR-0011): `thatch chat list --json --host-id ID` maps a run's agent
//! session id to its chat name without tm knowing how thatch keys sessions,
//! and `thatch chat tail --once` lists the messages. With no `thatch` on
//! `PATH` every lookup comes back empty, so views show no chat at all.
//!
//! [`ThatchRunner`] is the trait callers depend on; [`ShellThatchRunner`]
//! shells out to the real `thatch` binary, following the same trait+fake
//! seam as [`crate::work::vdiff::VdiffOps`]. [`FakeThatchRunner`] is the
//! test double.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

use thiserror::Error;

/// The program name every [`ShellThatchRunner`] call spawns.
const THATCH_PROGRAM: &str = "thatch";

/// Errors from [`ThatchRunner`] calls.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ThatchError {
    /// `thatch` isn't installed, or isn't on `PATH`.
    #[error("thatch not found on PATH; broadcast not sent")]
    NotFound,

    /// `thatch` could not be spawned for a reason other than "not found".
    #[error("failed to run thatch: {message}")]
    Spawn {
        /// The underlying I/O error message.
        message: String,
    },

    /// `thatch` ran but exited nonzero. thatch prints its refusal reasons
    /// (chat disabled, message too long) on stderr.
    #[error("thatch failed: {stderr}")]
    Command {
        /// Captured stderr, trimmed.
        stderr: String,
    },
}

/// Runs `thatch` with the given arguments. Kept to one generic method so a
/// test double can record the exact command line.
pub trait ThatchRunner {
    /// Whether `thatch` can be run at all (it is on `PATH`).
    fn available(&self) -> bool;

    /// Run `thatch <args>`, returning its stdout on a zero exit.
    fn run(&self, args: &[String]) -> Result<String, ThatchError>;
}

/// [`ThatchRunner`] that spawns the real `thatch` binary.
#[derive(Debug, Default)]
pub struct ShellThatchRunner;

impl ThatchRunner for ShellThatchRunner {
    fn available(&self) -> bool {
        on_path(std::env::var_os("PATH").as_deref(), THATCH_PROGRAM)
    }

    fn run(&self, args: &[String]) -> Result<String, ThatchError> {
        let output = Command::new(THATCH_PROGRAM)
            .args(args)
            .output()
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    ThatchError::NotFound
                } else {
                    ThatchError::Spawn {
                        message: err.to_string(),
                    }
                }
            })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(ThatchError::Command {
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }
}

/// Whether `program` is a file in one of `path`'s directories.
fn on_path(path: Option<&OsStr>, program: &str) -> bool {
    path.is_some_and(|path| std::env::split_paths(path).any(|dir| dir.join(program).is_file()))
}

/// Test double for [`ThatchRunner`]: returns a canned result and records
/// every argument list it was called with.
#[derive(Debug)]
pub struct FakeThatchRunner {
    /// What [`ThatchRunner::available`] answers.
    pub available: bool,
    /// What every [`ThatchRunner::run`] call returns.
    pub result: Result<String, ThatchError>,
    /// Every argument list passed to [`ThatchRunner::run`], in call order.
    pub calls: RefCell<Vec<Vec<String>>>,
}

impl FakeThatchRunner {
    /// An available runner whose every call prints `stdout` and exits 0.
    pub fn returning(stdout: &str) -> Self {
        Self {
            available: true,
            result: Ok(stdout.to_string()),
            calls: RefCell::new(Vec::new()),
        }
    }

    /// A runner standing in for a machine without `thatch` installed.
    pub fn missing() -> Self {
        Self {
            available: false,
            result: Err(ThatchError::NotFound),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl ThatchRunner for FakeThatchRunner {
    fn available(&self) -> bool {
        self.available
    }

    fn run(&self, args: &[String]) -> Result<String, ThatchError> {
        self.calls.borrow_mut().push(args.to_vec());
        self.result.clone()
    }
}

/// How long [`CachedThatchRunner`] reuses a read result. The run detail
/// window and the overview reload every half second on the TUI's event
/// loop; chat is human-paced, so a few seconds of lag costs nothing and
/// saves several `thatch` spawns per reload.
pub const CHAT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// When a cached call ran, and what it returned.
type CachedResult = (std::time::Instant, Result<String, ThatchError>);

/// [`ThatchRunner`] that reuses the result of a read-only call (`chat list`,
/// `chat tail`) made with the same arguments within `ttl`. Every other
/// call, such as `chat broadcast`, always runs.
pub struct CachedThatchRunner {
    inner: Box<dyn ThatchRunner>,
    ttl: std::time::Duration,
    cache: RefCell<std::collections::HashMap<Vec<String>, CachedResult>>,
    inner_calls: std::cell::Cell<usize>,
}

impl CachedThatchRunner {
    /// Wrap `inner`, reusing read results for `ttl`.
    pub fn new(inner: Box<dyn ThatchRunner>, ttl: std::time::Duration) -> Self {
        Self {
            inner,
            ttl,
            cache: RefCell::new(std::collections::HashMap::new()),
            inner_calls: std::cell::Cell::new(0),
        }
    }

    /// How many calls reached the wrapped runner.
    #[cfg(test)]
    fn inner_calls(&self) -> usize {
        self.inner_calls.get()
    }

    fn run_inner(&self, args: &[String]) -> Result<String, ThatchError> {
        self.inner_calls.set(self.inner_calls.get() + 1);
        self.inner.run(args)
    }
}

/// Whether `args` is a read-only `thatch chat` call safe to cache.
fn is_cacheable(args: &[String]) -> bool {
    matches!(
        args,
        [chat, sub, ..] if chat == "chat" && (sub == "list" || sub == "tail")
    )
}

impl ThatchRunner for CachedThatchRunner {
    fn available(&self) -> bool {
        self.inner.available()
    }

    fn run(&self, args: &[String]) -> Result<String, ThatchError> {
        if !is_cacheable(args) {
            return self.run_inner(args);
        }
        let now = std::time::Instant::now();
        if let Some((at, result)) = self.cache.borrow().get(args)
            && now.duration_since(*at) < self.ttl
        {
            return result.clone();
        }
        let result = self.run_inner(args);
        self.cache
            .borrow_mut()
            .insert(args.to_vec(), (now, result.clone()));
        result
    }
}

/// The thatch project label for the repo at `dir`: the `owner/name` slug of
/// its `origin` remote, which is what thatch itself records for sessions
/// started there. `None` when the repo has no GitHub `origin` remote.
pub fn detect_project(dir: &Path) -> Option<String> {
    crate::config::detect_origin_repo(dir)
}

/// The arguments for `thatch chat broadcast` sending `message` to `project`.
/// The message goes after `--` so text starting with `--` is sent rather
/// than parsed as a flag.
pub fn broadcast_args(project: &str, message: &str) -> Vec<String> {
    vec![
        "chat".to_string(),
        "broadcast".to_string(),
        "--project".to_string(),
        project.to_string(),
        "--".to_string(),
        message.to_string(),
    ]
}

/// Who a broadcast reached, parsed from `thatch chat broadcast`'s report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BroadcastReport {
    /// Names of the sessions the message was delivered to.
    pub delivered: Vec<String>,
    /// Names of the sessions thatch skipped, each with thatch's reason
    /// (for example `stale`).
    pub skipped: Vec<(String, String)>,
}

impl BroadcastReport {
    /// A one-line summary for the board's status line, for example
    /// `broadcast delivered to 2 sessions (a, b); skipped 1 (c: stale)`.
    pub fn summary(&self) -> String {
        let count = self.delivered.len();
        let noun = if count == 1 { "session" } else { "sessions" };
        let mut line = if count == 0 {
            "broadcast delivered to 0 sessions".to_string()
        } else {
            format!(
                "broadcast delivered to {count} {noun} ({})",
                self.delivered.join(", ")
            )
        };
        if !self.skipped.is_empty() {
            let skipped: Vec<String> = self
                .skipped
                .iter()
                .map(|(name, reason)| format!("{name}: {reason}"))
                .collect();
            line.push_str(&format!(
                "; skipped {} ({})",
                self.skipped.len(),
                skipped.join(", ")
            ));
        }
        line
    }
}

/// Parse `thatch chat broadcast`'s stdout. Each recipient is a line of the
/// form `  delivered: NAME (PROJECT) [LABEL]`, and each skipped session
/// `  skipped (REASON): NAME (PROJECT)`. Other lines (the header, the
/// out-of-scope and dedup notes) are ignored.
pub fn parse_broadcast_output(stdout: &str) -> BroadcastReport {
    let mut report = BroadcastReport::default();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("delivered: ") {
            report.delivered.push(session_name(rest));
        } else if let Some(rest) = line.strip_prefix("skipped (")
            && let Some((reason, name)) = rest.split_once("): ")
        {
            report
                .skipped
                .push((session_name(name), reason.to_string()));
        }
    }
    report
}

/// The session name at the start of `rest`, before its ` (PROJECT)` suffix.
/// Session names never contain spaces.
fn session_name(rest: &str) -> String {
    rest.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Send `message` to every live session in `project` through `thatch`,
/// returning who received it (see [`BroadcastReport::summary`]).
pub fn broadcast(
    runner: &dyn ThatchRunner,
    project: &str,
    message: &str,
) -> Result<BroadcastReport, ThatchError> {
    let stdout = runner.run(&broadcast_args(project, message))?;
    Ok(parse_broadcast_output(&stdout))
}

/// How many messages each `thatch chat tail --once` call asks for. A lane's
/// detail window shows at most this many sent and this many received; the
/// overview scans this many across every session.
pub const TAIL_LIMIT: usize = 100;

/// One chat message, built from `thatch chat tail`'s `sent` event plus its
/// `read` event when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// thatch's message id, unique across the chat.
    pub id: i64,
    /// When the message was sent, as thatch's ISO-8601 UTC string.
    pub at: String,
    /// The sender's chat name.
    pub from: String,
    /// The recipient's chat name.
    pub to: String,
    /// The message text.
    pub body: String,
    /// Whether the message was one recipient's copy of a broadcast.
    pub broadcast: bool,
    /// Whether the recipient has read it.
    pub read: bool,
}

/// A run's chat: its session's chat name and the messages it sent and
/// received, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneChat {
    /// The session's thatch chat name, for example `npc-4-00008`.
    pub name: String,
    /// Messages to or from `name`, oldest first.
    pub messages: Vec<ChatMessage>,
}

impl LaneChat {
    /// Whether the lane is waiting on an answer; see [`awaiting_reply`].
    pub fn awaiting_reply(&self) -> bool {
        awaiting_reply(&self.name, &self.messages)
    }
}

/// The arguments for looking up the chat rows of `host_ids` (agent session
/// ids as tm records them) in one `thatch chat list` call.
pub fn roster_args(host_ids: &[String]) -> Vec<String> {
    let mut args = vec!["chat".to_string(), "list".to_string(), "--json".to_string()];
    for id in host_ids {
        args.push("--host-id".to_string());
        args.push(id.clone());
    }
    args
}

/// Parse `thatch chat list --json --host-id ...` output into a map from
/// each looked-up host id (thatch echoes it as `host_id`) to its chat name.
/// Lines that are not roster rows, or rows without `host_id`, are skipped.
pub fn parse_roster(stdout: &str) -> BTreeMap<String, String> {
    parse_mailboxes(stdout)
        .into_iter()
        .map(|(id, mailbox)| (id, mailbox.name))
        .collect()
}

/// Map agent session ids to thatch chat names. Empty when `host_ids` is
/// empty, `thatch` is not installed, or the lookup fails: chat is an extra,
/// so a thatch problem never breaks the view asking for it.
pub fn chat_names(runner: &dyn ThatchRunner, host_ids: &[String]) -> BTreeMap<String, String> {
    mailboxes(runner, host_ids)
        .into_iter()
        .map(|(id, mailbox)| (id, mailbox.name))
        .collect()
}

/// One session's roster row as the chat wake reads it (GitHub issue #224).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    /// The session's thatch chat name.
    pub name: String,
    /// thatch's `host_kind`: `mcp` for a Claude Code session, `opencode`
    /// for an opencode one.
    pub host_kind: String,
    /// How many messages to the session it has not read yet.
    pub unread: u64,
}

impl Mailbox {
    /// Whether the session reads its mail only when its next prompt
    /// arrives, so an idle one needs tm to type that prompt. True for
    /// thatch's `mcp` rows (Claude Code). thatch's opencode plugin wakes
    /// its own sessions when mail lands, so those are left alone.
    pub fn reads_at_next_prompt(&self) -> bool {
        self.host_kind == "mcp"
    }
}

/// Parse `thatch chat list --json --host-id ...` output into a map from
/// each looked-up host id to its [`Mailbox`]. Lines that are not roster
/// rows, or rows without `host_id`, are skipped.
pub fn parse_mailboxes(stdout: &str) -> BTreeMap<String, Mailbox> {
    #[derive(serde::Deserialize)]
    struct Row {
        name: String,
        host_id: Option<String>,
        #[serde(default)]
        host_kind: String,
        #[serde(default)]
        unread: u64,
    }
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Row>(line.trim()).ok())
        .filter_map(|row| {
            let mailbox = Mailbox {
                name: row.name,
                host_kind: row.host_kind,
                unread: row.unread,
            };
            row.host_id.map(|id| (id, mailbox))
        })
        .collect()
}

/// Map agent session ids to their [`Mailbox`] in one roster call. Empty
/// when `host_ids` is empty, `thatch` is not installed, or the lookup
/// fails.
pub fn mailboxes(runner: &dyn ThatchRunner, host_ids: &[String]) -> BTreeMap<String, Mailbox> {
    if host_ids.is_empty() || !runner.available() {
        return BTreeMap::new();
    }
    runner
        .run(&roster_args(host_ids))
        .map(|stdout| parse_mailboxes(&stdout))
        .unwrap_or_default()
}

/// The id of the newest unread message addressed to `name` in `messages`,
/// or `None` when it has none there. The chat wake records it so one batch
/// of mail wakes a lane once.
pub fn newest_unread_to(name: &str, messages: &[ChatMessage]) -> Option<i64> {
    messages
        .iter()
        .filter(|message| message.to == name && !message.read)
        .map(|message| message.id)
        .max()
}

/// The arguments for one `thatch chat tail --once` snapshot of the last
/// [`TAIL_LIMIT`] messages, narrowed by `filter` (`("--from", NAME)` or
/// `("--to", NAME)`) when given.
pub fn tail_args(filter: Option<(&str, &str)>) -> Vec<String> {
    let mut args = vec![
        "chat".to_string(),
        "tail".to_string(),
        "--once".to_string(),
        "--limit".to_string(),
        TAIL_LIMIT.to_string(),
    ];
    if let Some((flag, name)) = filter {
        args.push(flag.to_string());
        args.push(name.to_string());
    }
    args
}

/// Parse `thatch chat tail` JSONL into messages in the order they were
/// sent. A `read` event marks its message read; lines that are not events
/// are skipped.
pub fn parse_tail(stdout: &str) -> Vec<ChatMessage> {
    #[derive(serde::Deserialize)]
    #[serde(tag = "event", rename_all = "lowercase")]
    enum Event {
        Sent {
            at: String,
            id: i64,
            from: String,
            to: String,
            #[serde(default)]
            broadcast: bool,
            body: String,
        },
        Read {
            id: i64,
        },
    }
    let mut messages: Vec<ChatMessage> = Vec::new();
    for line in stdout.lines() {
        match serde_json::from_str::<Event>(line.trim()) {
            Ok(Event::Sent {
                at,
                id,
                from,
                to,
                broadcast,
                body,
            }) => messages.push(ChatMessage {
                id,
                at,
                from,
                to,
                body,
                broadcast,
                read: false,
            }),
            Ok(Event::Read { id }) => {
                if let Some(message) = messages.iter_mut().find(|m| m.id == id) {
                    message.read = true;
                }
            }
            Err(_) => {}
        }
    }
    messages
}

/// Run one tail snapshot (the last [`TAIL_LIMIT`] messages, narrowed by
/// `filter` as in [`tail_args`]), empty on any failure.
pub fn tail(runner: &dyn ThatchRunner, filter: Option<(&str, &str)>) -> Vec<ChatMessage> {
    runner
        .run(&tail_args(filter))
        .map(|stdout| parse_tail(&stdout))
        .unwrap_or_default()
}

/// Every message `name` sent or received, oldest first. thatch's `--from`
/// and `--to` filters match substrings and cannot be combined as "either",
/// so this runs one call per direction and keeps only exact-name matches.
pub fn lane_messages(runner: &dyn ThatchRunner, name: &str) -> Vec<ChatMessage> {
    let mut by_id: BTreeMap<i64, ChatMessage> = BTreeMap::new();
    for flag in ["--from", "--to"] {
        for message in tail(runner, Some((flag, name))) {
            if message.from == name || message.to == name {
                by_id.insert(message.id, message);
            }
        }
    }
    let mut messages: Vec<ChatMessage> = by_id.into_values().collect();
    messages.sort_by(|a, b| a.at.cmp(&b.at).then(a.id.cmp(&b.id)));
    messages
}

/// The chat for the agent session `session_id`, or `None` when `thatch` is
/// not installed or the session never joined chat.
pub fn lane_chat(runner: &dyn ThatchRunner, session_id: &str) -> Option<LaneChat> {
    let name = chat_names(runner, &[session_id.to_string()]).remove(session_id)?;
    let messages = lane_messages(runner, &name);
    Some(LaneChat { name, messages })
}

/// Whether `name` sent a message that its recipient has not answered: for
/// some peer, the last message between the two (in `messages`, oldest
/// first) went from `name` to that peer. A reply from a different peer
/// does not count as an answer.
pub fn awaiting_reply(name: &str, messages: &[ChatMessage]) -> bool {
    let mut last_from_lane: BTreeMap<&str, bool> = BTreeMap::new();
    for message in messages {
        if message.from == name {
            last_from_lane.insert(message.to.as_str(), true);
        } else if message.to == name {
            last_from_lane.insert(message.from.as_str(), false);
        }
    }
    last_from_lane.values().any(|&from_lane| from_lane)
}

/// The subset of `host_ids` whose chat session is waiting on an answer
/// (see [`awaiting_reply`]), for the overview's attention signal. One
/// roster lookup plus one unfiltered tail snapshot covers every lane, so
/// the cost does not grow with the lane count. Empty without `thatch`.
pub fn lanes_awaiting_reply(runner: &dyn ThatchRunner, host_ids: &[String]) -> BTreeSet<String> {
    let names = chat_names(runner, host_ids);
    if names.is_empty() {
        return BTreeSet::new();
    }
    let messages = tail(runner, None);
    names
        .into_iter()
        .filter(|(_, name)| awaiting_reply(name, &messages))
        .map(|(host_id, _)| host_id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `thatch chat broadcast` report (thatch 0.3.4), one line of
    /// each shape.
    const REPORT: &str = "\
Broadcast from jowi-dev to 2 of 4 registered sessions (projects: jowi-dev/tskmstr):
  delivered: npc-4-00008 (jowi-dev/tskmstr) [mcp, reads at next prompt]
  delivered: rex-protocol-00007 (jowi-dev/tskmstr) [open]
  skipped (stale): vera-antimatter-00005 (jowi-dev/tskmstr)
  out of scope (project filter): kernel-panic-00006 (other/repo)
  duplicate suppressed for 1 recipient (an identical unread broadcast stands)
";

    #[test]
    fn broadcast_runs_thatch_chat_broadcast_with_the_project() {
        let runner = FakeThatchRunner::returning(REPORT);
        broadcast(&runner, "jowi-dev/tskmstr", "main moved, rebase").unwrap();
        assert_eq!(
            runner.calls.borrow().as_slice(),
            [vec![
                "chat".to_string(),
                "broadcast".to_string(),
                "--project".to_string(),
                "jowi-dev/tskmstr".to_string(),
                "--".to_string(),
                "main moved, rebase".to_string(),
            ]]
        );
    }

    #[test]
    fn broadcast_reports_delivered_and_skipped_sessions() {
        let runner = FakeThatchRunner::returning(REPORT);
        let report = broadcast(&runner, "jowi-dev/tskmstr", "hi").unwrap();
        assert_eq!(
            report,
            BroadcastReport {
                delivered: vec!["npc-4-00008".to_string(), "rex-protocol-00007".to_string()],
                skipped: vec![("vera-antimatter-00005".to_string(), "stale".to_string())],
            }
        );
        assert_eq!(
            report.summary(),
            "broadcast delivered to 2 sessions (npc-4-00008, rex-protocol-00007); \
             skipped 1 (vera-antimatter-00005: stale)"
        );
    }

    #[test]
    fn summary_with_no_recipients_says_zero_sessions() {
        let report = parse_broadcast_output(
            "Broadcast from jowi-dev to 0 of 0 registered sessions (projects: a/b):\n",
        );
        assert_eq!(report.summary(), "broadcast delivered to 0 sessions");
    }

    #[test]
    fn summary_uses_the_singular_for_one_session() {
        let report = BroadcastReport {
            delivered: vec!["a".to_string()],
            skipped: Vec::new(),
        };
        assert_eq!(report.summary(), "broadcast delivered to 1 session (a)");
    }

    #[test]
    fn broadcast_passes_a_missing_thatch_through() {
        let runner = FakeThatchRunner::missing();
        let err = broadcast(&runner, "a/b", "hi").unwrap_err();
        assert_eq!(err, ThatchError::NotFound);
        assert_eq!(
            err.to_string(),
            "thatch not found on PATH; broadcast not sent"
        );
    }

    /// `thatch chat list --json --host-id` output (thatch 0.3.5): one row
    /// per matched host id, echoing it in `host_id`.
    const ROSTER: &str = "\
{\"name\":\"npc-4-00008\",\"session_id\":\"mcp_836e4299fff9\",\"host_kind\":\"mcp\",\"project\":\"jowi-dev/tskmstr\",\"topic\":null,\"status\":\"idle\",\"last_seen\":\"2026-11-04T21:23:17Z\",\"unread\":0,\"host_id\":\"sess-a\"}
{\"name\":\"rex-protocol-00007\",\"session_id\":\"ses_raw\",\"host_kind\":\"opencode\",\"project\":\"jowi-dev/tskmstr\",\"topic\":\"GH-1\",\"status\":\"open\",\"last_seen\":\"2026-11-04T21:23:17Z\",\"unread\":2,\"host_id\":\"ses_raw\"}
";

    /// `thatch chat tail --once` output (thatch 0.3.5): sent and read
    /// events linked by message id.
    fn tail(lines: &[&str]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    fn sent(id: i64, at: &str, from: &str, to: &str, body: &str) -> String {
        format!(
            r#"{{"event":"sent","at":"{at}","id":{id},"from":"{from}","from_topic":null,"to":"{to}","to_topic":null,"broadcast":false,"body":"{body}"}}"#
        )
    }

    fn read(id: i64, at: &str, reader: &str, from: &str) -> String {
        format!(
            r#"{{"event":"read","at":"{at}","id":{id},"reader":"{reader}","reader_topic":null,"from":"{from}","from_topic":null}}"#
        )
    }

    fn msg(id: i64, at: &str, from: &str, to: &str, body: &str, read: bool) -> ChatMessage {
        ChatMessage {
            id,
            at: at.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            body: body.to_string(),
            broadcast: false,
            read,
        }
    }

    #[test]
    fn roster_args_look_up_every_host_id_as_json() {
        assert_eq!(
            roster_args(&["a".to_string(), "b".to_string()]),
            ["chat", "list", "--json", "--host-id", "a", "--host-id", "b"]
        );
    }

    #[test]
    fn parse_roster_maps_each_echoed_host_id_to_its_chat_name() {
        let names = parse_roster(ROSTER);
        assert_eq!(names.get("sess-a").map(String::as_str), Some("npc-4-00008"));
        assert_eq!(
            names.get("ses_raw").map(String::as_str),
            Some("rex-protocol-00007")
        );
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn parse_roster_skips_rows_without_a_host_id_and_junk_lines() {
        let stdout = "[thatch] a banner line\n{\"name\":\"x\",\"session_id\":\"s\"}\n";
        assert!(parse_roster(stdout).is_empty());
    }

    #[test]
    fn parse_mailboxes_reads_each_rows_unread_count_and_host_kind() {
        let boxes = parse_mailboxes(ROSTER);
        assert_eq!(
            boxes.get("sess-a"),
            Some(&Mailbox {
                name: "npc-4-00008".to_string(),
                host_kind: "mcp".to_string(),
                unread: 0,
            })
        );
        assert_eq!(boxes.get("ses_raw").map(|b| b.unread), Some(2));
        assert_eq!(boxes.len(), 2);
    }

    #[test]
    fn only_mcp_sessions_wait_for_their_next_prompt_to_read_mail() {
        let boxes = parse_mailboxes(ROSTER);
        assert!(boxes["sess-a"].reads_at_next_prompt());
        assert!(
            !boxes["ses_raw"].reads_at_next_prompt(),
            "thatch's opencode plugin wakes opencode sessions itself"
        );
    }

    #[test]
    fn mailboxes_is_empty_without_thatch_and_never_runs_it() {
        let runner = FakeThatchRunner::missing();
        assert!(mailboxes(&runner, &["sess-a".to_string()]).is_empty());
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn newest_unread_to_picks_the_latest_unread_message_addressed_to_the_lane() {
        let messages = [
            msg(1, "t1", "boss", "lane", "old", true),
            msg(2, "t2", "boss", "lane", "new", false),
            msg(3, "t3", "lane", "boss", "mine", false),
            msg(4, "t4", "boss", "other", "not ours", false),
        ];
        assert_eq!(newest_unread_to("lane", &messages), Some(2));
        assert_eq!(newest_unread_to("nobody", &messages), None);
    }

    #[test]
    fn chat_names_resolves_through_the_roster() {
        let runner = FakeThatchRunner::returning(ROSTER);
        let names = chat_names(&runner, &["sess-a".to_string()]);
        assert_eq!(names.get("sess-a").map(String::as_str), Some("npc-4-00008"));
        assert_eq!(
            runner.calls.borrow().as_slice(),
            [roster_args(&["sess-a".to_string()])]
        );
    }

    #[test]
    fn chat_names_is_empty_without_thatch_and_never_runs_it() {
        let runner = FakeThatchRunner::missing();
        assert!(chat_names(&runner, &["sess-a".to_string()]).is_empty());
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn chat_names_with_no_ids_never_runs_thatch() {
        let runner = FakeThatchRunner::returning(ROSTER);
        assert!(chat_names(&runner, &[]).is_empty());
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn parse_tail_builds_messages_and_marks_read_ones() {
        let stdout = tail(&[
            &sent(1, "2026-11-19T10:00:00Z", "lane", "coord", "which base?"),
            &read(1, "2026-11-19T10:01:00Z", "coord", "lane"),
            &sent(2, "2026-11-19T10:02:00Z", "coord", "lane", "main"),
            "not json",
        ]);
        assert_eq!(
            parse_tail(&stdout),
            vec![
                msg(
                    1,
                    "2026-11-19T10:00:00Z",
                    "lane",
                    "coord",
                    "which base?",
                    true
                ),
                msg(2, "2026-11-19T10:02:00Z", "coord", "lane", "main", false),
            ]
        );
    }

    #[test]
    fn lane_chat_merges_sent_and_received_messages_for_the_exact_name() {
        // thatch's --from/--to filters match substrings, so the `-000070`
        // row must be dropped by tm's exact-name filter.
        let stdout = tail(&[
            &sent(1, "2026-11-19T10:00:00Z", "lane-00007", "coord", "q"),
            &sent(2, "2026-11-19T10:01:00Z", "coord", "lane-000070", "other"),
            &sent(3, "2026-11-19T10:02:00Z", "coord", "lane-00007", "a"),
        ]);
        let runner = FakeThatchRunner::returning(&stdout);
        let messages = lane_messages(&runner, "lane-00007");
        assert_eq!(
            messages.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![1, 3],
            "deduplicated across the --from and --to calls, oldest first"
        );
        assert_eq!(
            runner.calls.borrow().as_slice(),
            [
                tail_args(Some(("--from", "lane-00007"))),
                tail_args(Some(("--to", "lane-00007"))),
            ]
        );
    }

    #[test]
    fn lane_chat_is_none_without_thatch() {
        let runner = FakeThatchRunner::missing();
        assert_eq!(lane_chat(&runner, "sess-a"), None);
    }

    #[test]
    fn lane_chat_is_none_when_the_session_has_no_chat_name() {
        let runner = FakeThatchRunner::returning("");
        assert_eq!(lane_chat(&runner, "sess-a"), None);
    }

    #[test]
    fn awaiting_reply_when_the_lanes_last_message_to_a_peer_is_unanswered() {
        let messages = vec![
            msg(1, "t1", "coord", "lane", "start", true),
            msg(2, "t2", "lane", "coord", "which base?", false),
        ];
        assert!(awaiting_reply("lane", &messages));
    }

    #[test]
    fn a_reply_from_the_peer_clears_awaiting_reply() {
        let messages = vec![
            msg(1, "t1", "lane", "coord", "which base?", true),
            msg(2, "t2", "coord", "lane", "main", false),
        ];
        assert!(!awaiting_reply("lane", &messages));
    }

    #[test]
    fn a_reply_from_another_peer_does_not_clear_awaiting_reply() {
        let messages = vec![
            msg(1, "t1", "lane", "coord", "which base?", true),
            msg(2, "t2", "human", "lane", "rebase", false),
        ];
        assert!(awaiting_reply("lane", &messages));
    }

    #[test]
    fn no_messages_means_not_awaiting_reply() {
        assert!(!awaiting_reply("lane", &[]));
    }

    #[test]
    fn lanes_awaiting_reply_maps_host_ids_through_one_roster_and_one_tail() {
        let stdout_roster = ROSTER;
        let stdout_tail = tail(&[
            &sent(1, "t1", "npc-4-00008", "coord", "q?"),
            &sent(2, "t2", "rex-protocol-00007", "coord", "q?"),
            &sent(3, "t3", "coord", "rex-protocol-00007", "a"),
        ]);
        let runner = ScriptedThatchRunner::new(vec![stdout_roster.to_string(), stdout_tail]);
        let waiting = lanes_awaiting_reply(&runner, &["sess-a".to_string(), "ses_raw".to_string()]);
        assert_eq!(waiting, BTreeSet::from(["sess-a".to_string()]));
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1], tail_args(None));
    }

    #[test]
    fn lanes_awaiting_reply_is_empty_without_thatch() {
        let runner = FakeThatchRunner::missing();
        assert!(lanes_awaiting_reply(&runner, &["sess-a".to_string()]).is_empty());
    }

    #[test]
    fn cached_runner_reuses_a_fresh_read_result() {
        let cached = CachedThatchRunner::new(
            Box::new(FakeThatchRunner::returning("out")),
            std::time::Duration::from_secs(60),
        );
        let args = tail_args(None);
        assert_eq!(cached.run(&args).unwrap(), "out");
        assert_eq!(cached.run(&args).unwrap(), "out");
        assert_eq!(cached.inner_calls(), 1);
    }

    #[test]
    fn cached_runner_reruns_once_the_result_expires() {
        let cached = CachedThatchRunner::new(
            Box::new(FakeThatchRunner::returning("out")),
            std::time::Duration::ZERO,
        );
        let args = roster_args(&["a".to_string()]);
        cached.run(&args).unwrap();
        cached.run(&args).unwrap();
        assert_eq!(cached.inner_calls(), 2);
    }

    #[test]
    fn cached_runner_never_caches_a_broadcast() {
        let cached = CachedThatchRunner::new(
            Box::new(FakeThatchRunner::returning(REPORT)),
            std::time::Duration::from_secs(60),
        );
        let args = broadcast_args("a/b", "hi");
        cached.run(&args).unwrap();
        cached.run(&args).unwrap();
        assert_eq!(cached.inner_calls(), 2);
    }

    /// A runner returning each canned stdout in turn.
    struct ScriptedThatchRunner {
        outputs: RefCell<Vec<String>>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl ScriptedThatchRunner {
        fn new(mut outputs: Vec<String>) -> Self {
            outputs.reverse();
            Self {
                outputs: RefCell::new(outputs),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl ThatchRunner for ScriptedThatchRunner {
        fn available(&self) -> bool {
            true
        }

        fn run(&self, args: &[String]) -> Result<String, ThatchError> {
            self.calls.borrow_mut().push(args.to_vec());
            Ok(self.outputs.borrow_mut().pop().unwrap_or_default())
        }
    }

    #[test]
    fn on_path_finds_a_program_file_in_a_path_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("thatch"), "").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert!(on_path(Some(&path), "thatch"));
        assert!(!on_path(Some(&path), "vdiff"));
        assert!(!on_path(None, "thatch"));
    }
}
