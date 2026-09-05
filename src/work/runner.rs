//! Process spawning for the lane runner, ported from devtools'
//! `~/devtools/work.ml`'s `run_lane`.
//!
//! [`ProcessSpawner`] is the trait callers depend on; [`StdProcessSpawner`]
//! is the `std::process::Command`-based implementation used in production,
//! following the same trait+fake seam as [`crate::work::git::GitOps`]/
//! [`crate::work::git::ShellGitOps`]. [`FakeProcessSpawner`] records the
//! invocation it was given and writes a canned JSON file to the requested
//! output path, for tests.
//!
//! Result parsing used to live here (`parse_run_outcome`) but has moved
//! behind [`crate::agent::AgentRunner::parse_outcome`] — see
//! [`crate::agent::claude::ClaudeRunner`]'s implementation for the
//! `claude`-specific field mapping and GitHub issue #17's phase 3. This
//! module now owns only spawning: what runs where, not what its output
//! means.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use thiserror::Error;

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
/// already-built [`crate::agent::AgentInvocation`]'s pieces, plus
/// the working directory and output destination that
/// [`crate::agent::AgentInvocation`] deliberately leaves out (see
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
    /// see [`crate::agent::AgentInvocation::env_remove`]).
    pub env_remove: &'a [String],
    /// Working directory for the spawned process, mirroring `work.ml`'s
    /// `cd '<wt_path>' && ...` shell prefix.
    pub current_dir: &'a Path,
    /// Where to write the spawned process's stdout, mirroring `work.ml`'s
    /// `> '<out_json>'` redirect. This is the file
    /// [`crate::agent::AgentRunner::parse_outcome`] later reads back.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRunner as _;
    use crate::agent::claude::ClaudeRunner;

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
        let outcome = ClaudeRunner.parse_outcome(&written).unwrap();
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
        let outcome = ClaudeRunner.parse_outcome(&written).unwrap();
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
