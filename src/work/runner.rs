//! Process spawn + result parsing for the lane runner, ported from
//! devtools' `~/devtools/work.ml`'s `run_lane`.
//!
//! [`ProcessSpawner`] is the trait callers depend on; [`StdProcessSpawner`]
//! is the `std::process::Command`-based implementation used in production,
//! following the same trait+fake seam as [`crate::work::git::GitOps`]/
//! [`crate::work::git::ShellGitOps`]. [`FakeProcessSpawner`] records the
//! invocation it was given and writes a canned JSON file to the requested
//! output path, for tests.
//!
//! [`parse_run_outcome`] is the single result-parsing path called by §4 of
//! `docs/plans/runner-port.md`: `work.ml` duplicates this logic between the
//! `--fg` path (inline `jq` calls) and the detached path (a generated shell
//! wrapper re-reading the same file via `jq`). This port has one function,
//! called by both the foreground (step 9) and detached (step 10) paths.
//!
//! ## Field-by-field mapping from `work.ml`'s `jq` calls
//!
//! `run_lane` (lines ~524-563) and the detached wrapper script (lines
//! ~615-622) read the same `out_json` file with these `jq` expressions, fed
//! into these `tm runs finish` flags:
//!
//! | `jq` expression | `RunOutcome` field | `tm runs finish` flag |
//! |---|---|---|
//! | `.session_id // empty` | `session_id: String` (required) | `--session-id` |
//! | `.num_turns // empty` | `num_turns: Option<u64>` | (printed in `--fg` summary; not a finish flag) |
//! | `.total_cost_usd // empty` | `cost_usd: Option<f64>` | (printed in `--fg` summary; not a finish flag) |
//! | `.is_error` | `is_error: Option<bool>` (absence is kept, not defaulted) | drives the `done`/`failed`/`interrupted` status passed to `finish_run` |
//! | `.result // empty` / `.result // "no result field"` | `result: Option<String>` | scraped for the PR-URL fallback, not passed directly |
//! | `.modelUsage // empty` | `model_usage: Option<ModelUsageMap>` | `--model-usage` (only when present and non-empty, per the wrapper's `[ -n "$MODEL_USAGE" ]` guard) |
//!
//! The wrapper also passes `--transcript <out_json>` (the file path itself,
//! not a parsed field) — that's a step 9/10 concern (they already have the
//! output path), not something [`RunOutcome`] needs to carry.
//!
//! `jq`'s `// empty` and `// false` are the OCaml side's way of tolerating
//! absent fields: a missing field becomes an empty string (falsy in a shell
//! `[ -n ... ]` test) or `false`, never a hard error. This port keeps that
//! tolerance for every field except `session_id`: `work.ml` never checks
//! whether `session_id` came back non-empty before using it (e.g. printing
//! `claude --resume %s`), so an empty/missing session id would already be a
//! silently-broken run on the OCaml side. Rust can do better than silently
//! propagating an empty string, so [`parse_run_outcome`] treats a missing or
//! empty `session_id` as a hard [`RunOutcomeError`] instead — a run with no
//! session id to resume isn't a usable outcome, only an unparseable one.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use thiserror::Error;

use crate::runs::ModelUsageMap;

/// Errors that can occur while spawning `claude` (or any process) via a
/// [`ProcessSpawner`].
#[derive(Debug, Error)]
pub enum SpawnError {
    /// The output file (`out_json`'s destination) could not be created.
    #[error("failed to create output file {path}: {message}")]
    OutputFile {
        /// The path that could not be created.
        path: PathBuf,
        /// The underlying I/O error message.
        message: String,
    },

    /// The program could not be spawned at all (not found, permission
    /// denied, etc.) — distinct from the program running and exiting
    /// nonzero, which is a normal [`ExitStatus`], not an error.
    #[error("failed to spawn `{program}`: {message}")]
    Spawn {
        /// The program that could not be spawned.
        program: String,
        /// The underlying I/O error message.
        message: String,
    },

    /// Waiting on the spawned child failed.
    #[error("failed to wait for `{program}`: {message}")]
    Wait {
        /// The program that was being waited on.
        program: String,
        /// The underlying I/O error message.
        message: String,
    },
}

/// What a [`ProcessSpawner`] needs to run one `claude -p` invocation: the
/// already-built [`crate::work::claude::ClaudeInvocation`]'s pieces, plus
/// the working directory and output destination that
/// [`crate::work::claude::ClaudeInvocation`] deliberately leaves out (see
/// that module's doc comment: "Output redirection is a spawn-time concern,
/// not an argv concern").
pub struct SpawnRequest<'a> {
    /// The program to spawn, e.g. `"claude"`.
    pub program: &'a str,
    /// Full argv (excluding the program name).
    pub args: &'a [String],
    /// Environment variables to set before spawning.
    pub env_set: &'a [(String, String)],
    /// Environment variables to remove before spawning (billing safety —
    /// see [`crate::work::claude::ClaudeInvocation::env_remove`]).
    pub env_remove: &'a [String],
    /// Working directory for the spawned process, mirroring `work.ml`'s
    /// `cd '<wt_path>' && ...` shell prefix.
    pub current_dir: &'a Path,
    /// Where to write the spawned process's stdout, mirroring `work.ml`'s
    /// `> '<out_json>'` redirect. This is the file [`parse_run_outcome`]
    /// later reads back.
    pub stdout_path: &'a Path,
}

/// Process execution, wrapping `std::process::Command`. Kept minimal — just
/// what steps 9/10 need (spawn one `claude -p` invocation, redirect its
/// stdout to a file, get back the exit status). Not a general-purpose
/// process abstraction.
pub trait ProcessSpawner {
    /// Spawn `request`, wait for it to exit, and return its [`ExitStatus`].
    /// The spawned process's stdout is written to `request.stdout_path`
    /// (created/truncated), matching `work.ml`'s `> '<out_json>'` redirect —
    /// note this is `>`, not `>>`: each run gets a fresh output file, unlike
    /// the adjacent `.log` file which `work.ml` appends to.
    fn spawn(&self, request: SpawnRequest<'_>) -> Result<ExitStatus, SpawnError>;
}

/// Production [`ProcessSpawner`] backed by `std::process::Command`.
pub struct StdProcessSpawner;

impl ProcessSpawner for StdProcessSpawner {
    fn spawn(&self, request: SpawnRequest<'_>) -> Result<ExitStatus, SpawnError> {
        let stdout_file =
            std::fs::File::create(request.stdout_path).map_err(|err| SpawnError::OutputFile {
                path: request.stdout_path.to_path_buf(),
                message: err.to_string(),
            })?;

        let mut command = Command::new(request.program);
        command
            .args(request.args)
            .current_dir(request.current_dir)
            .stdout(Stdio::from(stdout_file));

        for var in request.env_remove {
            command.env_remove(var);
        }
        for (key, value) in request.env_set {
            command.env(key, value);
        }

        let mut child = command.spawn().map_err(|err| SpawnError::Spawn {
            program: request.program.to_string(),
            message: err.to_string(),
        })?;

        child.wait().map_err(|err| SpawnError::Wait {
            program: request.program.to_string(),
            message: err.to_string(),
        })
    }
}

/// A recorded [`SpawnRequest`], owned so it outlives the borrowed call in
/// [`FakeProcessSpawner`]'s test assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedSpawn {
    /// The program that was spawned.
    pub program: String,
    /// The argv it was spawned with.
    pub args: Vec<String>,
    /// The environment variables that were set.
    pub env_set: Vec<(String, String)>,
    /// The environment variables that were removed.
    pub env_remove: Vec<String>,
    /// The working directory it was spawned in.
    pub current_dir: PathBuf,
    /// The stdout destination it was given.
    pub stdout_path: PathBuf,
}

/// Test double for [`ProcessSpawner`]: records the [`SpawnRequest`] it was
/// given (for assertions on argv/env/cwd) and writes a canned JSON string to
/// `stdout_path`, standing in for what a real `claude -p --output-format
/// json` invocation would have written there.
pub struct FakeProcessSpawner {
    /// The canned JSON to write to `stdout_path` on every call.
    pub canned_json: String,
    /// The [`ExitStatus`] to report back. There is no public constructor
    /// for a synthetic `ExitStatus` in stable `std`, so tests build one via
    /// [`FakeProcessSpawner::success`] (exit 0) or by running a trivial
    /// real command for a specific nonzero code.
    exit_status: ExitStatus,
    /// Every [`SpawnRequest`] passed to [`ProcessSpawner::spawn`], in call
    /// order.
    pub recorded: std::sync::Mutex<Vec<RecordedSpawn>>,
}

impl FakeProcessSpawner {
    /// A fake that reports success (exit 0) and writes `canned_json`.
    pub fn success(canned_json: impl Into<String>) -> Self {
        Self {
            canned_json: canned_json.into(),
            exit_status: real_exit_status(0),
            recorded: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// A fake that reports the given exit code and writes `canned_json`.
    pub fn with_exit_code(canned_json: impl Into<String>, code: i32) -> Self {
        Self {
            canned_json: canned_json.into(),
            exit_status: real_exit_status(code),
            recorded: std::sync::Mutex::new(Vec::new()),
        }
    }
}

/// Builds a real [`ExitStatus`] with the given exit code by actually
/// running a trivial `sh -c 'exit <code>'` — the only portable way to
/// construct one on stable `std`, which exposes no public constructor.
fn real_exit_status(code: i32) -> ExitStatus {
    Command::new("sh")
        .arg("-c")
        .arg(format!("exit {code}"))
        .status()
        .expect("failed to run `sh` to synthesize an ExitStatus")
}

impl ProcessSpawner for FakeProcessSpawner {
    fn spawn(&self, request: SpawnRequest<'_>) -> Result<ExitStatus, SpawnError> {
        self.recorded
            .lock()
            .expect("FakeProcessSpawner mutex poisoned")
            .push(RecordedSpawn {
                program: request.program.to_string(),
                args: request.args.to_vec(),
                env_set: request.env_set.to_vec(),
                env_remove: request.env_remove.to_vec(),
                current_dir: request.current_dir.to_path_buf(),
                stdout_path: request.stdout_path.to_path_buf(),
            });

        let mut file =
            std::fs::File::create(request.stdout_path).map_err(|err| SpawnError::OutputFile {
                path: request.stdout_path.to_path_buf(),
                message: err.to_string(),
            })?;
        file.write_all(self.canned_json.as_bytes())
            .map_err(|err| SpawnError::OutputFile {
                path: request.stdout_path.to_path_buf(),
                message: err.to_string(),
            })?;

        Ok(self.exit_status)
    }
}

/// Errors from [`parse_run_outcome`].
#[derive(Debug, Error)]
pub enum RunOutcomeError {
    /// `out_json`'s contents did not parse as JSON at all.
    #[error("failed to parse claude result JSON: {0}")]
    Malformed(#[from] serde_json::Error),

    /// The JSON parsed, but `session_id` was missing, empty, or not a
    /// string. See this module's doc comment for why `session_id` is the
    /// one field this port treats as required where `work.ml` did not.
    #[error("claude result JSON is missing a non-empty session_id")]
    MissingSessionId,
}

/// The typed result of one `claude -p --output-format json` invocation,
/// parsed from the JSON it wrote to its stdout (`out_json` in `work.ml`'s
/// naming). See this module's doc comment for the exact `jq` fields this
/// mirrors and which `tm runs finish` flags each one feeds.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    /// `.session_id`, required — see [`RunOutcomeError::MissingSessionId`].
    /// Feeds `tm runs finish --session-id`.
    pub session_id: String,
    /// `.total_cost_usd`, absent when the field is missing (mirrors `jq`'s
    /// `// empty`).
    pub cost_usd: Option<f64>,
    /// `.num_turns`, absent when the field is missing.
    pub num_turns: Option<u64>,
    /// `.is_error`, verbatim — `None` when the field is entirely absent from
    /// the result JSON, distinct from an explicit `false`.
    ///
    /// This diverges from `work.ml`'s `jq '.is_error // false'` (which this
    /// port originally mirrored, defaulting absence to `false`): an absent
    /// `is_error` turned out to be exactly the shape a mid-run event like a
    /// usage-limit forced model switch can leave behind — the turn ends
    /// gracefully (`claude` exits 0) but never writes an `is_error` field at
    /// all. Defaulting that to `false` silently misclassified an ambiguous
    /// outcome as a confirmed success. The caller
    /// ([`crate::work::run::run_claude_and_finish`]) now treats `None` here
    /// as suspicious — `RunStatus::Interrupted`, not `Done` — while
    /// `Some(true)`/`Some(false)` still drive `Failed`/`Done` exactly as
    /// before.
    pub is_error: Option<bool>,
    /// `.result`, the free-text summary/response. Absent when missing,
    /// distinct from an explicit empty string.
    pub result: Option<String>,
    /// `.modelUsage`, parsed via [`ModelUsageMap`] when present and
    /// non-empty. Feeds `tm runs finish --model-usage` (only passed by the
    /// caller when this is `Some`, mirroring the wrapper's `[ -n
    /// "$MODEL_USAGE" ]` guard).
    pub model_usage: Option<ModelUsageMap>,
}

/// Raw shape of the `claude -p --output-format json` result, deserialized
/// leniently: every field is optional at this layer so that any one missing
/// field doesn't fail the whole parse. [`parse_run_outcome`] is the only
/// place that decides which absences are fatal.
#[derive(Debug, serde::Deserialize)]
struct RawResult {
    session_id: Option<String>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    num_turns: Option<u64>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default, rename = "modelUsage")]
    model_usage: Option<ModelUsageMap>,
}

/// Parses a `claude -p --output-format json` result (`json`, the raw file
/// contents) into a [`RunOutcome`]. The single parsing path §4 of
/// `docs/plans/runner-port.md` calls for: both the foreground (step 9) and
/// detached (step 10) run paths call this instead of duplicating `jq`-style
/// field extraction.
///
/// Hard errors on unparseable JSON ([`RunOutcomeError::Malformed`]) or a
/// missing/empty `session_id` ([`RunOutcomeError::MissingSessionId`]).
/// Every other field is tolerant of absence, matching `jq`'s `// empty`/`//
/// false` fallbacks in `work.ml`.
pub fn parse_run_outcome(json: &str) -> Result<RunOutcome, RunOutcomeError> {
    let raw: RawResult = serde_json::from_str(json)?;

    let session_id = raw
        .session_id
        .filter(|id| !id.is_empty())
        .ok_or(RunOutcomeError::MissingSessionId)?;

    let model_usage = raw.model_usage.filter(|models| !models.is_empty());

    Ok(RunOutcome {
        session_id,
        cost_usd: raw.total_cost_usd,
        num_turns: raw.num_turns,
        is_error: raw.is_error,
        result: raw.result,
        model_usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_canned_json() -> String {
        r#"{
            "session_id": "sess-123",
            "total_cost_usd": 1.5,
            "num_turns": 12,
            "is_error": false,
            "result": "opened https://github.com/example/repo/pull/42",
            "modelUsage": {
                "claude-fable-5": {
                    "inputTokens": 100,
                    "outputTokens": 200,
                    "cacheReadInputTokens": 0,
                    "cacheCreationInputTokens": 0,
                    "costUSD": 1.5
                }
            }
        }"#
        .to_string()
    }

    #[test]
    fn parse_run_outcome_parses_all_fields() {
        let outcome = parse_run_outcome(&full_canned_json()).expect("should parse");

        assert_eq!(outcome.session_id, "sess-123");
        assert_eq!(outcome.cost_usd, Some(1.5));
        assert_eq!(outcome.num_turns, Some(12));
        assert_eq!(outcome.is_error, Some(false));
        assert_eq!(
            outcome.result,
            Some("opened https://github.com/example/repo/pull/42".to_string())
        );
        let model_usage = outcome.model_usage.expect("model usage should be present");
        assert_eq!(model_usage["claude-fable-5"].input_tokens, 100);
        assert_eq!(model_usage["claude-fable-5"].cost_usd, Some(1.5));
    }

    #[test]
    fn parse_run_outcome_tolerates_missing_optional_fields() {
        let json = r#"{"session_id": "sess-abc"}"#;

        let outcome = parse_run_outcome(json).expect("should parse");

        assert_eq!(outcome.session_id, "sess-abc");
        assert_eq!(outcome.cost_usd, None);
        assert_eq!(outcome.num_turns, None);
        assert_eq!(outcome.is_error, None);
        assert_eq!(outcome.result, None);
        assert_eq!(outcome.model_usage, None);
    }

    #[test]
    fn parse_run_outcome_leaves_is_error_none_when_absent() {
        // An absent `is_error` is distinct from an explicit `false`: this is
        // exactly the shape a mid-run usage-limit model switch can leave
        // behind (the turn ends gracefully with no `is_error` field at all),
        // and the caller (run_claude_and_finish) must be able to tell the
        // two apart to avoid misclassifying it as a successful `Done` run.
        // See RunStatus::Interrupted's doc comment.
        let json = r#"{"session_id": "sess-abc"}"#;
        assert_eq!(parse_run_outcome(json).unwrap().is_error, None);
    }

    #[test]
    fn parse_run_outcome_honors_is_error_explicit_false() {
        let json = r#"{"session_id": "sess-abc", "is_error": false}"#;
        assert_eq!(parse_run_outcome(json).unwrap().is_error, Some(false));
    }

    #[test]
    fn parse_run_outcome_honors_is_error_true() {
        let json = r#"{"session_id": "sess-abc", "is_error": true}"#;
        assert_eq!(parse_run_outcome(json).unwrap().is_error, Some(true));
    }

    #[test]
    fn parse_run_outcome_errors_on_malformed_json() {
        let err = parse_run_outcome("not json").unwrap_err();
        assert!(matches!(err, RunOutcomeError::Malformed(_)));
    }

    #[test]
    fn parse_run_outcome_errors_on_missing_session_id() {
        let json = r#"{"num_turns": 3}"#;
        let err = parse_run_outcome(json).unwrap_err();
        assert!(matches!(err, RunOutcomeError::MissingSessionId));
    }

    #[test]
    fn parse_run_outcome_errors_on_empty_session_id() {
        // Mirrors the doc comment: work.ml's jq `// empty` turns a missing
        // field into an empty string, which this port treats the same as
        // absent, not as a usable (empty) session id.
        let json = r#"{"session_id": ""}"#;
        let err = parse_run_outcome(json).unwrap_err();
        assert!(matches!(err, RunOutcomeError::MissingSessionId));
    }

    #[test]
    fn parse_run_outcome_treats_empty_model_usage_map_as_none() {
        let json = r#"{"session_id": "sess-abc", "modelUsage": {}}"#;
        assert_eq!(parse_run_outcome(json).unwrap().model_usage, None);
    }

    #[test]
    fn fake_spawner_writes_canned_json_to_stdout_path() {
        let dir = std::env::temp_dir().join(format!("tm-runner-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out_path = dir.join("out.json");

        let spawner = FakeProcessSpawner::success(full_canned_json());
        let env_set = vec![("TSKMSTR_RUN_ID".to_string(), "run-1".to_string())];
        let env_remove = vec!["ANTHROPIC_API_KEY".to_string()];
        let args = vec!["-p".to_string(), "hello".to_string()];

        let status = spawner
            .spawn(SpawnRequest {
                program: "claude",
                args: &args,
                env_set: &env_set,
                env_remove: &env_remove,
                current_dir: &dir,
                stdout_path: &out_path,
            })
            .expect("fake spawn should succeed");

        assert!(status.success());
        let written = std::fs::read_to_string(&out_path).unwrap();
        let outcome = parse_run_outcome(&written).unwrap();
        assert_eq!(outcome.session_id, "sess-123");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fake_spawner_records_program_args_env_and_cwd() {
        let dir =
            std::env::temp_dir().join(format!("tm-runner-test-record-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out_path = dir.join("out.json");

        let spawner = FakeProcessSpawner::success(full_canned_json());
        let env_set = vec![("TSKMSTR_RUN_ID".to_string(), "run-1".to_string())];
        let env_remove = vec![
            "ANTHROPIC_API_KEY".to_string(),
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            "CLAUDECODE".to_string(),
        ];
        let args = vec!["-p".to_string(), "do the thing".to_string()];

        spawner
            .spawn(SpawnRequest {
                program: "claude",
                args: &args,
                env_set: &env_set,
                env_remove: &env_remove,
                current_dir: &dir,
                stdout_path: &out_path,
            })
            .unwrap();

        let recorded = spawner.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let call = &recorded[0];
        assert_eq!(call.program, "claude");
        assert_eq!(call.args, args);
        assert_eq!(call.env_set, env_set);
        assert_eq!(call.env_remove, env_remove);
        assert_eq!(call.current_dir, dir);
        assert_eq!(call.stdout_path, out_path);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fake_spawner_reports_configured_exit_code() {
        let dir = std::env::temp_dir().join(format!("tm-runner-test-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out_path = dir.join("out.json");

        let spawner = FakeProcessSpawner::with_exit_code("{}", 3);
        let status = spawner
            .spawn(SpawnRequest {
                program: "claude",
                args: &[],
                env_set: &[],
                env_remove: &[],
                current_dir: &dir,
                stdout_path: &out_path,
            })
            .unwrap();

        assert_eq!(status.code(), Some(3));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn real_spawner_redirects_stdout_to_file_and_reports_exit_status() {
        let dir = std::env::temp_dir().join(format!("tm-runner-test-real-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out_path = dir.join("out.json");

        let spawner = StdProcessSpawner;
        let args = vec![
            "-c".to_string(),
            r#"printf '{"session_id":"sess-real"}'"#.to_string(),
        ];

        let status = spawner
            .spawn(SpawnRequest {
                program: "sh",
                args: &args,
                env_set: &[],
                env_remove: &[],
                current_dir: &dir,
                stdout_path: &out_path,
            })
            .expect("real spawn should succeed");

        assert!(status.success());
        let written = std::fs::read_to_string(&out_path).unwrap();
        let outcome = parse_run_outcome(&written).unwrap();
        assert_eq!(outcome.session_id, "sess-real");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn real_spawner_removes_billing_unsafe_env_vars() {
        let dir = std::env::temp_dir().join(format!("tm-runner-test-envrm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out_path = dir.join("out.json");

        let spawner = StdProcessSpawner;
        let args = vec![
            "-c".to_string(),
            r#"printf '{"session_id":"%s"}' "${ANTHROPIC_API_KEY:-absent}""#.to_string(),
        ];
        let env_remove = vec!["ANTHROPIC_API_KEY".to_string()];

        // SAFETY: single-threaded test setting a var before spawning a
        // child that reads it; no concurrent access to this process's env.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "sk-should-not-leak");
        }

        let status = spawner
            .spawn(SpawnRequest {
                program: "sh",
                args: &args,
                env_set: &[],
                env_remove: &env_remove,
                current_dir: &dir,
                stdout_path: &out_path,
            })
            .unwrap();

        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }

        assert!(status.success());
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert_eq!(written, r#"{"session_id":"absent"}"#);

        std::fs::remove_dir_all(&dir).ok();
    }
}
