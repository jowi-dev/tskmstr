//! Server mode (GitHub issue #194): a headless opencode lane run as a
//! session on the user's opencode 2.x background server, driven by a small
//! tm process instead of an `opencode run` client per lane.
//!
//! `docs/plans/gh-194-opencode-server-mode.md` holds the verified server
//! API and the design. In short:
//!
//! - [`server_mode_invocation`] rewrites the adapter's headless
//!   `opencode run ...` invocation into `<tm> agent-driver opencode run ...`
//!   with the same argv and environment, so the supervisor still spawns
//!   one process per lane and reads one NDJSON stdout file.
//! - [`drive`] is that process. It creates a session at its cwd, copies its
//!   own environment into the session, sends the prompt, then polls the
//!   server: it answers permission requests the way `opencode run` would
//!   and writes each completed assistant message to stdout as the same
//!   `step_start`/`text`/`tool_use`/`step_finish`/`error` events
//!   `opencode run --format json` writes. `OpencodeRunner::parse_outcome`
//!   reads the result unchanged.
//! - Every server call goes through [`ServerApi`]. [`CliServerApi`] runs
//!   `opencode api`, opencode's own authenticated client, so tm never reads
//!   the server's password.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::agent::AgentInvocation;

/// The hidden `tm` subcommand that runs a runner's server-mode driver. Its
/// first argument names the runner; the rest is that runner's own headless
/// argv. Must match the subcommand name in `src/cli/mod.rs`.
pub const DRIVER_SUBCOMMAND: &str = "agent-driver";

/// The runner name [`server_mode_invocation`] passes as the driver's first
/// argument.
pub const RUNNER_ARG: &str = "opencode";

/// How often [`drive`] polls the server. Each poll costs two or three
/// short-lived `opencode api` calls (about 50 ms each).
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Consecutive failed status polls after which [`drive`] gives up on an
/// unreachable server.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// Page size for assistant-message listing.
const MESSAGE_PAGE: usize = 100;

/// Rewrites a headless `opencode run ...` invocation into the server-mode
/// driver invocation: `tm_exe agent-driver opencode run ...`, keeping the
/// argv after the program and the environment deltas as they were.
pub fn server_mode_invocation(headless: &AgentInvocation, tm_exe: &Path) -> AgentInvocation {
    let mut args = vec![DRIVER_SUBCOMMAND.to_string(), RUNNER_ARG.to_string()];
    args.extend(headless.args.iter().cloned());
    AgentInvocation {
        program: tm_exe.to_string_lossy().into_owned(),
        args,
        env_set: headless.env_set.clone(),
        env_remove: headless.env_remove.clone(),
    }
}

/// What the driver took from the `opencode run` argv it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveArgs {
    /// `--model provider/model[#variant]`, if given.
    pub model: Option<String>,
    /// `--auto` (or its alias `--dangerously-skip-permissions`): approve
    /// every permission request rather than reject it.
    pub auto: bool,
    /// The prompt after `--`.
    pub prompt: String,
}

/// Parses the headless argv the adapter builds (`run --format json --model
/// <m> [--dangerously-skip-permissions|--auto] -- <prompt>`). Unknown flags
/// are an error so a future adapter change cannot be silently dropped.
pub fn parse_run_args(args: &[String]) -> Result<DriveArgs, String> {
    let mut iter = args.iter();
    match iter.next().map(String::as_str) {
        Some("run") => {}
        other => return Err(format!("expected `run`, got {other:?}")),
    }
    let mut model = None;
    let mut auto = false;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--format" => {
                let value = iter.next().ok_or("--format needs a value")?;
                if value != "json" {
                    return Err(format!("unsupported --format {value}"));
                }
            }
            "--model" | "-m" => {
                model = Some(iter.next().ok_or("--model needs a value")?.clone());
            }
            "--auto" | "--dangerously-skip-permissions" | "--yolo" => auto = true,
            "--" => {
                let prompt: Vec<&str> = iter.map(String::as_str).collect();
                return Ok(DriveArgs {
                    model,
                    auto,
                    prompt: prompt.join(" "),
                });
            }
            other => return Err(format!("unsupported argument `{other}`")),
        }
    }
    Err("missing `-- <prompt>`".to_string())
}

/// Splits `provider/model[#variant]` into the server's `Model.Ref`.
fn model_ref(model: &str) -> Value {
    let (model, variant) = match model.split_once('#') {
        Some((model, variant)) => (model, Some(variant)),
        None => (model, None),
    };
    let (provider, id) = model.split_once('/').unwrap_or(("", model));
    let mut value = json!({ "providerID": provider, "id": id });
    if let Some(variant) = variant {
        value["variant"] = json!(variant);
    }
    value
}

/// A failed server call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The server answered 404 (for a session: it no longer exists).
    NotFound,
    /// Anything else: the server is unreachable, rejected the request, or
    /// the client could not run.
    Failed(String),
}

/// One request to the opencode server, returning the response body.
pub trait ServerApi {
    /// `method` is an HTTP method, `path` the request path with any query
    /// string inline, `body` a JSON request body.
    fn call(&self, method: &str, path: &str, body: Option<&str>) -> Result<String, ApiError>;
}

/// [`ServerApi`] over `opencode api`, which authenticates against the
/// user's background service the same way `opencode run` does.
pub struct CliServerApi;

impl ServerApi for CliServerApi {
    fn call(&self, method: &str, path: &str, body: Option<&str>) -> Result<String, ApiError> {
        let mut command = std::process::Command::new("opencode");
        command.args(["api", method, path]);
        if let Some(body) = body {
            command.args(["-d", body]);
        }
        let output = command
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|err| ApiError::Failed(format!("could not run opencode api: {err}")))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            return Ok(stdout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("404") {
            return Err(ApiError::NotFound);
        }
        let detail = format!("{} {}", stderr.trim(), stdout.trim());
        Err(ApiError::Failed(format!(
            "opencode api {method} {path} failed: {}",
            detail.trim()
        )))
    }
}

/// Everything [`drive`] reads from its surroundings, injected for tests.
pub struct DriveDeps<'a> {
    /// The server.
    pub api: &'a dyn ServerApi,
    /// The session's location: the lane worktree (the driver's cwd).
    pub cwd: &'a Path,
    /// The environment copied into the session: the driver's own.
    pub env: Vec<(String, String)>,
    /// The session title.
    pub title: String,
    /// Waits one poll interval, returning early once `stop` would be true.
    pub sleep: &'a dyn Fn(),
    /// Whether the driver was asked to stop (a caught signal).
    pub stop: &'a dyn Fn() -> bool,
}

/// Exit code for a session that ended `interrupted`, matching a process
/// killed by `SIGINT`.
pub const EXIT_INTERRUPTED: i32 = 130;

/// Runs one server-mode lane session to its end, writing
/// `opencode run --format json`-compatible NDJSON to `out`. Returns the
/// process exit code: 0 when the session succeeded, 1 when it failed or the
/// server could not be driven, [`EXIT_INTERRUPTED`] when it was
/// interrupted.
pub fn drive(args: &DriveArgs, deps: &DriveDeps, out: &mut dyn Write) -> i32 {
    match Driver::start(args, deps, out) {
        Ok(mut driver) => driver.run(),
        Err(message) => {
            write_event(out, &error_event(None, "DriverError", &message));
            1
        }
    }
}

struct Driver<'a, 'o> {
    args: &'a DriveArgs,
    deps: &'a DriveDeps<'a>,
    out: &'o mut dyn Write,
    session: String,
    prompt_created: f64,
    cursor: Option<String>,
    emitted: HashSet<String>,
    errored: bool,
}

impl<'a, 'o> Driver<'a, 'o> {
    fn start(
        args: &'a DriveArgs,
        deps: &'a DriveDeps<'a>,
        out: &'o mut dyn Write,
    ) -> Result<Self, String> {
        let api = deps.api;
        let mut create = json!({
            "title": deps.title,
            "location": { "directory": deps.cwd.to_string_lossy() },
        });
        if let Some(model) = args.model.as_deref() {
            create["model"] = model_ref(model);
        }
        let created = api
            .call("POST", "/api/session", Some(&create.to_string()))
            .map_err(|err| describe("create a session", &err))?;
        let session = parse(&created)?
            .pointer("/data/id")
            .and_then(Value::as_str)
            .ok_or("session create returned no id")?
            .to_string();

        let variables: serde_json::Map<String, Value> = deps
            .env
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect();
        api.call(
            "PUT",
            &format!("/api/session/{session}/environment"),
            Some(&json!({ "variables": variables }).to_string()),
        )
        .map_err(|err| describe("set the session environment", &err))?;

        let prompted = api
            .call(
                "POST",
                &format!("/api/session/{session}/prompt"),
                Some(&json!({ "text": args.prompt }).to_string()),
            )
            .map_err(|err| describe("send the prompt", &err))?;
        let prompt_created = parse(&prompted)?
            .pointer("/data/time/created")
            .and_then(Value::as_f64)
            .ok_or("prompt returned no creation time")?;

        Ok(Driver {
            args,
            deps,
            out,
            session,
            prompt_created,
            cursor: None,
            emitted: HashSet::new(),
            errored: false,
        })
    }

    fn run(&mut self) -> i32 {
        let mut failures = 0;
        loop {
            if (self.deps.stop)() {
                let _ = self.deps.api.call(
                    "POST",
                    &format!("/api/session/{}/interrupt", self.session),
                    None,
                );
                self.emit_messages();
                return EXIT_INTERRUPTED;
            }
            self.answer_permissions();
            self.emit_messages();
            match self
                .deps
                .api
                .call("GET", &format!("/api/session/{}", self.session), None)
            {
                Ok(body) => {
                    failures = 0;
                    if let Some(outcome) = self.finished_outcome(&body) {
                        self.emit_messages();
                        return self.finish(&outcome);
                    }
                }
                Err(ApiError::NotFound) => {
                    self.error(
                        "SessionNotFound",
                        "the session no longer exists on the server",
                    );
                    return 1;
                }
                Err(ApiError::Failed(message)) => {
                    failures += 1;
                    if failures >= MAX_CONSECUTIVE_FAILURES {
                        self.error("ServerUnreachable", &message);
                        return 1;
                    }
                }
            }
            (self.deps.sleep)();
        }
    }

    /// The session's outcome once it has gone idle after the prompt.
    fn finished_outcome(&self, body: &str) -> Option<String> {
        let info = serde_json::from_str::<Value>(body).ok()?;
        let data = info.get("data")?;
        let idle = data.pointer("/time/idle").and_then(Value::as_f64)?;
        if idle < self.prompt_created {
            return None;
        }
        data.get("outcome")?.as_str().map(str::to_string)
    }

    fn finish(&mut self, outcome: &str) -> i32 {
        match outcome {
            "succeeded" => 0,
            "interrupted" => EXIT_INTERRUPTED,
            _ => {
                if !self.errored {
                    let (name, message) = self.idle_error().unwrap_or_else(|| {
                        ("SessionFailed".to_string(), format!("session {outcome}"))
                    });
                    self.error(&name, &message);
                }
                1
            }
        }
    }

    /// The error on the session's latest idle message, if any.
    fn idle_error(&self) -> Option<(String, String)> {
        let body = self
            .deps
            .api
            .call(
                "GET",
                &format!(
                    "/api/session/{}/message?type=idle&order=desc&limit=1",
                    self.session
                ),
                None,
            )
            .ok()?;
        let value = serde_json::from_str::<Value>(&body).ok()?;
        structured_error(value.pointer("/data/0/error")?)
    }

    fn answer_permissions(&mut self) {
        let path = format!("/api/session/{}/permission", self.session);
        let Ok(body) = self.deps.api.call("GET", &path, None) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(&body) else {
            return;
        };
        let requests = value.get("data").and_then(Value::as_array);
        for request in requests.into_iter().flatten() {
            let Some(id) = request.get("id").and_then(Value::as_str) else {
                continue;
            };
            let reply = if self.args.auto {
                json!({ "decision": "once" })
            } else {
                json!({
                    "decision": "reject",
                    "message": "headless tm lane without permission bypass: rejected",
                })
            };
            let _ = self.deps.api.call(
                "POST",
                &format!("{path}/{id}/reply"),
                Some(&reply.to_string()),
            );
        }
    }

    /// Writes every newly completed assistant message, oldest first.
    fn emit_messages(&mut self) {
        loop {
            let path = match &self.cursor {
                None => format!(
                    "/api/session/{}/message?type=assistant&order=asc&limit={MESSAGE_PAGE}",
                    self.session
                ),
                Some(cursor) => format!(
                    "/api/session/{}/message?type=assistant&cursor={cursor}&limit={MESSAGE_PAGE}",
                    self.session
                ),
            };
            let Ok(body) = self.deps.api.call("GET", &path, None) else {
                return;
            };
            let Ok(page) = serde_json::from_str::<Value>(&body) else {
                return;
            };
            let messages = page
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut complete = true;
            for message in &messages {
                let Some(id) = message.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if self.emitted.contains(id) {
                    continue;
                }
                if message.pointer("/time/completed").is_none() {
                    complete = false;
                    break;
                }
                self.emit_message(message);
                self.emitted.insert(id.to_string());
            }
            // Advance past this page only once every message on it has been
            // written; otherwise re-read it next poll, skipping what was
            // already written.
            if !complete || messages.len() < MESSAGE_PAGE {
                return;
            }
            match page.pointer("/cursor/next").and_then(Value::as_str) {
                Some(next) => {
                    self.cursor = Some(next.to_string());
                    self.emitted.clear();
                }
                None => return,
            }
        }
    }

    fn emit_message(&mut self, message: &Value) {
        let session = self.session.clone();
        let timestamp = message
            .pointer("/time/created")
            .cloned()
            .unwrap_or(Value::Null);
        let event = |kind: &str, part: Value| {
            json!({
                "type": kind,
                "timestamp": timestamp,
                "sessionID": session,
                "part": part,
            })
        };
        write_event(
            self.out,
            &event("step_start", json!({ "type": "step-start" })),
        );
        let content = message.get("content").and_then(Value::as_array);
        for part in content.into_iter().flatten() {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = part.get("text").cloned().unwrap_or(Value::Null);
                    write_event(
                        self.out,
                        &event("text", json!({ "type": "text", "text": text })),
                    );
                }
                Some("tool") => {
                    let tool = json!({
                        "type": "tool",
                        "tool": part.get("name").cloned().unwrap_or(Value::Null),
                        "state": part.get("state").cloned().unwrap_or(Value::Null),
                    });
                    write_event(self.out, &event("tool_use", tool));
                }
                _ => {}
            }
        }
        if let Some((name, text)) = message.get("error").and_then(structured_error) {
            self.error(&name, &text);
        }
        let finish = json!({
            "type": "step-finish",
            "reason": message.get("finish").cloned().unwrap_or(Value::Null),
            "cost": message.get("cost").cloned().unwrap_or(json!(0)),
            "tokens": message.get("tokens").cloned().unwrap_or(Value::Null),
        });
        write_event(self.out, &event("step_finish", finish));
    }

    fn error(&mut self, name: &str, message: &str) {
        self.errored = true;
        write_event(self.out, &error_event(Some(&self.session), name, message));
    }
}

/// A server `Session.StructuredError` as `(name, message)`, with the HTTP
/// status folded into the message so the rate-limit classifier sees `429`.
fn structured_error(error: &Value) -> Option<(String, String)> {
    let name = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("Error")
        .to_string();
    let mut message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some(status) = error.get("status").and_then(Value::as_u64) {
        message = format!("{message} (HTTP {status})");
    }
    Some((name, message))
}

fn error_event(session: Option<&str>, name: &str, message: &str) -> Value {
    json!({
        "type": "error",
        "sessionID": session,
        "error": { "name": name, "data": { "message": message } },
    })
}

fn write_event(out: &mut dyn Write, event: &Value) {
    let _ = writeln!(out, "{event}");
    let _ = out.flush();
}

fn parse(body: &str) -> Result<Value, String> {
    serde_json::from_str(body).map_err(|err| format!("unparseable server response: {err}"))
}

fn describe(action: &str, err: &ApiError) -> String {
    match err {
        ApiError::NotFound => format!("could not {action}: not found"),
        ApiError::Failed(message) => format!(
            "could not {action}: {message} (is the opencode service running? `opencode service start`)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use super::*;
    use crate::agent::AgentRunner;
    use crate::agent::opencode::OpencodeRunner;

    /// Scripted [`ServerApi`]: each route is a method and a path whose
    /// query string, if any, must prefix the request's, with a queue of
    /// responses; the last response repeats once the queue is down to one.
    /// Routes are matched in insertion order, so list routes with a query
    /// before the bare path. Unrouted calls answer `{"data":[]}`.
    /// A method, a path (with optional query prefix) and its responses.
    type Route = (String, String, VecDeque<Result<String, ApiError>>);

    #[derive(Default)]
    struct FakeApi {
        routes: RefCell<Vec<Route>>,
        calls: RefCell<Vec<(String, String, Option<String>)>>,
    }

    impl FakeApi {
        fn on(self, method: &str, prefix: &str, responses: Vec<Result<String, ApiError>>) -> Self {
            self.routes.borrow_mut().push((
                method.to_string(),
                prefix.to_string(),
                responses.into(),
            ));
            self
        }

        fn calls_to(&self, method: &str, prefix: &str) -> Vec<Option<String>> {
            self.calls
                .borrow()
                .iter()
                .filter(|(m, p, _)| m == method && p.starts_with(prefix))
                .map(|(_, _, body)| body.clone())
                .collect()
        }
    }

    impl ServerApi for FakeApi {
        fn call(&self, method: &str, path: &str, body: Option<&str>) -> Result<String, ApiError> {
            self.calls.borrow_mut().push((
                method.to_string(),
                path.to_string(),
                body.map(str::to_string),
            ));
            let mut routes = self.routes.borrow_mut();
            let base = path.split('?').next().unwrap_or(path);
            for (m, route, queue) in routes.iter_mut() {
                let route_base = route.split('?').next().unwrap_or(route);
                if m == method && base == route_base && path.starts_with(route.as_str()) {
                    return if queue.len() > 1 {
                        queue.pop_front().unwrap()
                    } else {
                        queue.front().cloned().unwrap_or(Ok("{}".to_string()))
                    };
                }
            }
            Ok(r#"{"data":[]}"#.to_string())
        }
    }

    const SID: &str = "ses_abc";

    fn ok(value: Value) -> Result<String, ApiError> {
        Ok(value.to_string())
    }

    fn busy() -> Result<String, ApiError> {
        ok(json!({"data": {"id": SID, "time": {"created": 1, "busy": 10}}}))
    }

    fn idle(outcome: &str) -> Result<String, ApiError> {
        ok(
            json!({"data": {"id": SID, "outcome": outcome, "time": {"created": 1, "busy": 10, "idle": 20}}}),
        )
    }

    fn base_api() -> FakeApi {
        FakeApi::default()
            .on(
                "POST",
                "/api/session/ses_abc/prompt",
                vec![ok(
                    json!({"data": {"id": "msg_u", "time": {"created": 10}}}),
                )],
            )
            .on(
                "POST",
                "/api/session/ses_abc/interrupt",
                vec![ok(json!({"interrupted": true}))],
            )
            .on(
                "POST",
                "/api/session",
                vec![ok(json!({"data": {"id": SID}}))],
            )
            .on(
                "PUT",
                "/api/session/ses_abc/environment",
                vec![Ok(String::new())],
            )
    }

    fn assistant(id: &str, completed: bool, content: Value, extra: Value) -> Value {
        let mut message = json!({
            "id": id,
            "type": "assistant",
            "time": {"created": 11},
            "finish": "stop",
            "cost": 0.5,
            "tokens": {"input": 3, "output": 4, "reasoning": 0, "cache": {"read": 0, "write": 0}},
            "content": content,
        });
        if completed {
            message["time"]["completed"] = json!(12);
        }
        for (key, value) in extra.as_object().cloned().unwrap_or_default() {
            message[key] = value;
        }
        message
    }

    fn page(messages: Vec<Value>) -> Result<String, ApiError> {
        ok(json!({"data": messages, "cursor": {"next": "c1", "previous": "c0"}}))
    }

    fn args(auto: bool) -> DriveArgs {
        DriveArgs {
            model: Some("venice/glm#fast".to_string()),
            auto,
            prompt: "do the ticket".to_string(),
        }
    }

    fn run_driver(api: &FakeApi, args: &DriveArgs, stop_after: Option<usize>) -> (i32, String) {
        let polls = RefCell::new(0usize);
        let sleep = || *polls.borrow_mut() += 1;
        let stop = || stop_after.is_some_and(|n| *polls.borrow() >= n);
        let cwd = PathBuf::from("/wt/lane");
        let deps = DriveDeps {
            api,
            cwd: &cwd,
            env: vec![
                ("HOME".to_string(), "/home/me".to_string()),
                ("TSKMSTR_RUN_ID".to_string(), "42".to_string()),
            ],
            title: "tm lane run 42".to_string(),
            sleep: &sleep,
            stop: &stop,
        };
        let mut out = Vec::new();
        let code = drive(args, &deps, &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    fn events(out: &str) -> Vec<Value> {
        out.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn server_mode_invocation_runs_tm_with_the_driver_subcommand_and_the_same_argv() {
        let headless = AgentInvocation {
            program: "opencode".to_string(),
            args: vec!["run".to_string(), "--".to_string(), "hi".to_string()],
            env_set: vec![("TSKMSTR_RUN_ID".to_string(), "7".to_string())],
            env_remove: vec!["X".to_string()],
        };

        let driven = server_mode_invocation(&headless, Path::new("/bin/tm"));

        assert_eq!(driven.program, "/bin/tm");
        assert_eq!(
            driven.args,
            vec!["agent-driver", "opencode", "run", "--", "hi"]
        );
        assert_eq!(driven.env_set, headless.env_set);
        assert_eq!(driven.env_remove, headless.env_remove);
    }

    #[test]
    fn parse_run_args_reads_the_adapter_headless_argv() {
        let argv: Vec<String> = [
            "run",
            "--format",
            "json",
            "--model",
            "venice/glm",
            "--dangerously-skip-permissions",
            "--",
            "fix it",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        assert_eq!(
            parse_run_args(&argv).unwrap(),
            DriveArgs {
                model: Some("venice/glm".to_string()),
                auto: true,
                prompt: "fix it".to_string(),
            }
        );
    }

    #[test]
    fn parse_run_args_rejects_an_unknown_flag_and_a_missing_prompt() {
        let unknown: Vec<String> = ["run", "--fork", "--", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_run_args(&unknown).unwrap_err().contains("--fork"));
        let no_prompt: Vec<String> = ["run", "--auto"].iter().map(|s| s.to_string()).collect();
        assert!(parse_run_args(&no_prompt).is_err());
    }

    #[test]
    fn model_ref_splits_provider_model_and_variant() {
        assert_eq!(
            model_ref("venice/z-ai/glm#fast"),
            json!({"providerID": "venice", "id": "z-ai/glm", "variant": "fast"})
        );
        assert_eq!(
            model_ref("venice/glm"),
            json!({"providerID": "venice", "id": "glm"})
        );
    }

    #[test]
    fn drive_creates_the_session_at_cwd_sends_the_environment_and_the_prompt() {
        let api = base_api().on("GET", "/api/session/ses_abc", vec![idle("succeeded")]);

        let (code, _) = run_driver(&api, &args(true), None);

        assert_eq!(code, 0);
        let create: Value =
            serde_json::from_str(api.calls_to("POST", "/api/session")[0].as_deref().unwrap())
                .unwrap();
        assert_eq!(create["location"]["directory"], "/wt/lane");
        assert_eq!(
            create["model"],
            json!({"providerID": "venice", "id": "glm", "variant": "fast"})
        );
        assert_eq!(create["title"], "tm lane run 42");
        let env: Value = serde_json::from_str(
            api.calls_to("PUT", "/api/session/ses_abc/environment")[0]
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            env,
            json!({"variables": {"HOME": "/home/me", "TSKMSTR_RUN_ID": "42"}}),
            "the whole environment, since the server replaces it"
        );
        let prompt: Value = serde_json::from_str(
            api.calls_to("POST", "/api/session/ses_abc/prompt")[0]
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(prompt, json!({"text": "do the ticket"}));
    }

    #[test]
    fn drive_waits_until_the_session_goes_idle_after_the_prompt() {
        let stale_idle = ok(
            json!({"data": {"id": SID, "outcome": "succeeded", "time": {"created": 1, "idle": 5}}}),
        );
        let api = base_api().on(
            "GET",
            "/api/session/ses_abc",
            vec![stale_idle, busy(), idle("succeeded")],
        );
        let polls = RefCell::new(0);
        let sleep = || *polls.borrow_mut() += 1;
        let cwd = PathBuf::from("/wt");
        let deps = DriveDeps {
            api: &api,
            cwd: &cwd,
            env: vec![],
            title: String::new(),
            sleep: &sleep,
            stop: &|| false,
        };

        let code = drive(&args(true), &deps, &mut Vec::new());

        assert_eq!(code, 0);
        assert_eq!(
            *polls.borrow(),
            2,
            "an idle time before the prompt does not count"
        );
    }

    #[test]
    fn drive_output_parses_into_cost_turns_result_and_session_id() {
        let first = assistant(
            "msg_1",
            true,
            json!([{"type": "reasoning", "text": "hmm"}, {"type": "tool", "name": "shell", "state": {"status": "completed", "input": {"command": "ls"}}}]),
            json!({"finish": "tool-calls"}),
        );
        let second = assistant(
            "msg_2",
            true,
            json!([{"type": "text", "text": "PR: https://github.com/o/r/pull/9"}]),
            json!({}),
        );
        let api = base_api()
            .on(
                "GET",
                "/api/session/ses_abc/message",
                vec![page(vec![first, second])],
            )
            .on("GET", "/api/session/ses_abc", vec![idle("succeeded")]);

        let (code, out) = run_driver(&api, &args(true), None);

        assert_eq!(code, 0);
        let outcome = OpencodeRunner.parse_outcome(&out).unwrap();
        assert_eq!(outcome.session_id, SID);
        assert_eq!(outcome.cost_usd, Some(1.0));
        assert_eq!(outcome.num_turns, Some(2));
        assert_eq!(outcome.is_error, Some(false));
        assert_eq!(
            outcome.result.as_deref(),
            Some("PR: https://github.com/o/r/pull/9")
        );
        let tool_line = out.lines().find(|l| l.contains("tool_use")).unwrap();
        assert_eq!(
            OpencodeRunner.progress_lines(tool_line),
            OpencodeRunner.progress_lines(
                r#"{"type":"tool_use","part":{"tool":"shell","state":{"input":{"command":"ls"}}}}"#
            )
        );
    }

    #[test]
    fn drive_writes_each_message_once_and_waits_for_incomplete_ones() {
        let done = assistant(
            "msg_1",
            true,
            json!([{"type": "text", "text": "one"}]),
            json!({}),
        );
        let running = assistant("msg_2", false, json!([]), json!({}));
        let finished = assistant(
            "msg_2",
            true,
            json!([{"type": "text", "text": "two"}]),
            json!({}),
        );
        let api = base_api()
            .on(
                "GET",
                "/api/session/ses_abc/message",
                vec![
                    page(vec![done.clone(), running]),
                    page(vec![done.clone(), finished.clone()]),
                    page(vec![done, finished]),
                ],
            )
            .on(
                "GET",
                "/api/session/ses_abc",
                vec![busy(), idle("succeeded")],
            );

        let (_, out) = run_driver(&api, &args(true), None);

        let texts: Vec<Value> = events(&out)
            .into_iter()
            .filter(|e| e["type"] == "text")
            .map(|e| e["part"]["text"].clone())
            .collect();
        assert_eq!(texts, vec![json!("one"), json!("two")]);
    }

    #[test]
    fn drive_follows_the_cursor_past_a_full_page() {
        let full: Vec<Value> = (0..MESSAGE_PAGE)
            .map(|n| assistant(&format!("msg_{n}"), true, json!([]), json!({})))
            .collect();
        let last = assistant("msg_last", true, json!([]), json!({}));
        let api = base_api()
            .on(
                "GET",
                "/api/session/ses_abc/message?type=assistant&cursor=c1",
                vec![page(vec![last])],
            )
            .on("GET", "/api/session/ses_abc/message", vec![page(full)])
            .on("GET", "/api/session/ses_abc", vec![idle("succeeded")]);

        let (_, out) = run_driver(&api, &args(true), None);

        let turns = OpencodeRunner.parse_outcome(&out).unwrap().num_turns;
        assert_eq!(turns, Some(MESSAGE_PAGE as u64 + 1));
    }

    #[test]
    fn drive_approves_permission_requests_with_auto() {
        let api = base_api()
            .on(
                "GET",
                "/api/session/ses_abc/permission",
                vec![
                    ok(json!({"data": [{"id": "per_1", "action": "edit", "resources": []}]})),
                    ok(json!({"data": []})),
                ],
            )
            .on(
                "GET",
                "/api/session/ses_abc",
                vec![busy(), idle("succeeded")],
            );

        run_driver(&api, &args(true), None);

        let replies = api.calls_to("POST", "/api/session/ses_abc/permission/per_1/reply");
        assert_eq!(replies.len(), 1);
        let reply: Value = serde_json::from_str(replies[0].as_deref().unwrap()).unwrap();
        assert_eq!(reply["decision"], "once");
    }

    #[test]
    fn drive_rejects_permission_requests_without_auto() {
        let api = base_api()
            .on(
                "GET",
                "/api/session/ses_abc/permission",
                vec![
                    ok(json!({"data": [{"id": "per_1", "action": "edit", "resources": []}]})),
                    ok(json!({"data": []})),
                ],
            )
            .on(
                "GET",
                "/api/session/ses_abc",
                vec![busy(), idle("succeeded")],
            );

        run_driver(&api, &args(false), None);

        let replies = api.calls_to("POST", "/api/session/ses_abc/permission/per_1/reply");
        let reply: Value = serde_json::from_str(replies[0].as_deref().unwrap()).unwrap();
        assert_eq!(reply["decision"], "reject");
    }

    #[test]
    fn drive_reports_a_failed_session_with_its_rate_limit_error() {
        let failed = assistant(
            "msg_1",
            true,
            json!([]),
            json!({"finish": "error", "error": {"type": "APIError", "message": "Too many requests", "status": 429}}),
        );
        let api = base_api()
            .on(
                "GET",
                "/api/session/ses_abc/message",
                vec![page(vec![failed])],
            )
            .on("GET", "/api/session/ses_abc", vec![idle("failed")]);

        let (code, out) = run_driver(&api, &args(true), None);

        assert_eq!(code, 1);
        let outcome = OpencodeRunner.parse_outcome(&out).unwrap();
        assert_eq!(outcome.is_error, Some(true));
        assert!(
            outcome.rate_limit.is_some(),
            "429 must reach the GH-54 classifier"
        );
        assert_eq!(
            events(&out).iter().filter(|e| e["type"] == "error").count(),
            1,
            "no second error from the idle message"
        );
    }

    #[test]
    fn drive_falls_back_to_the_idle_message_error_when_no_step_failed() {
        let api = base_api()
            .on(
                "GET",
                "/api/session/ses_abc/message?type=idle",
                vec![ok(json!({"data": [{"id": "msg_i", "type": "idle", "outcome": "failed", "error": {"type": "ProviderError", "message": "out of credits"}}]}))],
            )
            .on("GET", "/api/session/ses_abc", vec![idle("failed")]);

        let (code, out) = run_driver(&api, &args(true), None);

        assert_eq!(code, 1);
        let error = events(&out)
            .into_iter()
            .find(|e| e["type"] == "error")
            .unwrap();
        assert_eq!(error["error"]["name"], "ProviderError");
        assert_eq!(error["error"]["data"]["message"], "out of credits");
        assert_eq!(error["sessionID"], SID);
    }

    #[test]
    fn drive_interrupts_the_session_when_asked_to_stop() {
        let api = base_api().on("GET", "/api/session/ses_abc", vec![busy()]);

        let (code, _) = run_driver(&api, &args(true), Some(2));

        assert_eq!(code, EXIT_INTERRUPTED);
        assert_eq!(
            api.calls_to("POST", "/api/session/ses_abc/interrupt").len(),
            1
        );
    }

    #[test]
    fn drive_exits_interrupted_when_the_session_was_interrupted_elsewhere() {
        let api = base_api().on("GET", "/api/session/ses_abc", vec![idle("interrupted")]);

        let (code, _) = run_driver(&api, &args(true), None);

        assert_eq!(code, EXIT_INTERRUPTED);
    }

    #[test]
    fn drive_fails_when_the_session_disappears() {
        let api = base_api().on(
            "GET",
            "/api/session/ses_abc",
            vec![busy(), Err(ApiError::NotFound)],
        );

        let (code, out) = run_driver(&api, &args(true), None);

        assert_eq!(code, 1);
        let error = events(&out)
            .into_iter()
            .find(|e| e["type"] == "error")
            .unwrap();
        assert_eq!(error["error"]["name"], "SessionNotFound");
    }

    #[test]
    fn drive_gives_up_after_consecutive_server_failures() {
        let api = base_api().on(
            "GET",
            "/api/session/ses_abc",
            vec![Err(ApiError::Failed("connection refused".to_string()))],
        );

        let (code, out) = run_driver(&api, &args(true), None);

        assert_eq!(code, 1);
        let polls = api
            .calls
            .borrow()
            .iter()
            .filter(|(m, p, _)| m == "GET" && p == "/api/session/ses_abc")
            .count();
        assert_eq!(polls, MAX_CONSECUTIVE_FAILURES as usize);
        let error = events(&out)
            .into_iter()
            .find(|e| e["type"] == "error")
            .unwrap();
        assert_eq!(error["error"]["name"], "ServerUnreachable");
    }

    #[test]
    fn drive_reports_a_session_that_could_not_be_created() {
        let api = FakeApi::default().on(
            "POST",
            "/api/session",
            vec![Err(ApiError::Failed("connection refused".to_string()))],
        );

        let (code, out) = run_driver(&api, &args(true), None);

        assert_eq!(code, 1);
        let error = events(&out).into_iter().next().unwrap();
        assert_eq!(error["type"], "error");
        assert!(
            error["error"]["data"]["message"]
                .as_str()
                .unwrap()
                .contains("opencode service start")
        );
    }
}
