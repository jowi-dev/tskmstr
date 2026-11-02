//! Readable rendering of a Claude Code session transcript (GitHub issue
//! #155).
//!
//! Interactive claude runs have no detached log file (`runs.log_path` is
//! null), so `tm runs logs` falls back to the session's own transcript:
//! the JSONL file Claude Code appends to at
//! `<claude_dir>/projects/<escaped-cwd>/<session_id>.jsonl`. [`locate`]
//! finds that file from a session id, and [`render`] turns its entries
//! into plain text lines: user and assistant messages verbatim, tool calls
//! and their results as one-line briefs. Everything else in the file
//! (summaries, system entries, snapshots, thinking blocks) is skipped.
//! [`ClaudeTranscripts`] packages both as the claude adapter's
//! [`crate::agent::Transcripts`].

use std::path::{Path, PathBuf};

use crate::agent::Transcripts;

/// Claude Code's transcripts under one config dir (`$CLAUDE_CONFIG_DIR`,
/// default `~/.claude`): [`crate::agent::AgentRunner::transcripts`] for
/// [`super::ClaudeRunner`].
#[derive(Debug, Clone)]
pub struct ClaudeTranscripts {
    config_dir: PathBuf,
}

impl ClaudeTranscripts {
    /// Reads transcripts under `config_dir`, the Claude Code config dir.
    pub fn new(config_dir: PathBuf) -> Self {
        ClaudeTranscripts { config_dir }
    }
}

impl Transcripts for ClaudeTranscripts {
    /// See [`locate`].
    fn locate(&self, session_id: &str) -> Option<PathBuf> {
        locate(&self.config_dir, session_id)
    }

    /// See [`render`].
    fn render(&self, raw: &str) -> Vec<String> {
        render(raw)
    }
}

/// Finds the transcript Claude Code wrote for `session_id` under
/// `claude_dir` (the Claude Code config dir, normally `~/.claude`).
///
/// Scans every subdirectory of `<claude_dir>/projects` and returns the first
/// `<session_id>.jsonl` that is a regular file. Returns `None` when the
/// session id is empty or contains `/` or `..` (it is spliced into a path),
/// when the projects dir is unreadable, or when no project has the file.
pub fn locate(claude_dir: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains('/') || session_id.contains("..") {
        return None;
    }
    let file_name = format!("{session_id}.jsonl");
    std::fs::read_dir(claude_dir.join("projects"))
        .ok()?
        .flatten()
        .map(|project| project.path().join(&file_name))
        .find(|candidate| candidate.is_file())
}

/// Renders a Claude Code JSONL transcript as readable text lines.
///
/// Only `user` and `assistant` entries are shown; unparsable lines, every
/// other entry type, and `isMeta` entries are skipped. Text becomes
/// `user: ...` / `assistant: ...` (continuation lines of multi-line text
/// are indented by two spaces), a tool call becomes `  -> <tool> <brief>`
/// and a tool result `  <- result: <brief>`, where a brief is the first
/// line of the relevant value truncated to 120 characters. Thinking blocks
/// and unknown block types are skipped.
pub fn render(jsonl: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let speaker = match entry.get("type").and_then(|t| t.as_str()) {
            Some(speaker @ ("user" | "assistant")) => speaker,
            _ => continue,
        };
        if entry.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
            continue;
        }
        match entry.pointer("/message/content") {
            Some(serde_json::Value::String(text)) => push_text(&mut out, speaker, text),
            Some(serde_json::Value::Array(blocks)) => {
                for block in blocks {
                    render_block(&mut out, speaker, block);
                }
            }
            _ => {}
        }
    }
    out
}

/// The longest brief (in characters) a tool call or result is shown with.
const BRIEF_MAX_CHARS: usize = 120;

/// Input fields that best summarise a tool call, in preference order.
const BRIEF_INPUT_FIELDS: &[&str] = &[
    "command",
    "file_path",
    "path",
    "pattern",
    "url",
    "description",
    "prompt",
];

/// Renders one content block of a `speaker` entry. `text` blocks are shown
/// for both speakers; `tool_use` only for the assistant and `tool_result`
/// only for the user, matching where Claude Code writes them.
fn render_block(out: &mut Vec<String>, speaker: &str, block: &serde_json::Value) {
    match (speaker, block.get("type").and_then(|t| t.as_str())) {
        (_, Some("text")) => {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                push_text(out, speaker, text);
            }
        }
        ("assistant", Some("tool_use")) => {
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let input = block.get("input").cloned().unwrap_or_default();
            let brief = BRIEF_INPUT_FIELDS
                .iter()
                .find_map(|field| input.get(*field).and_then(|v| v.as_str()))
                .map(str::to_string)
                .unwrap_or_else(|| input.to_string());
            let brief = brief_of(&brief);
            if brief.is_empty() {
                out.push(format!("  -> {name}"));
            } else {
                out.push(format!("  -> {name} {brief}"));
            }
        }
        ("user", Some("tool_result")) => {
            let content = match block.get("content") {
                Some(serde_json::Value::String(text)) => text.clone(),
                Some(serde_json::Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            let brief = brief_of(&content);
            if brief.is_empty() {
                out.push("  <- result".to_string());
            } else {
                out.push(format!("  <- result: {brief}"));
            }
        }
        _ => {}
    }
}

/// Pushes `text` as `<speaker>: <first line>` followed by its remaining
/// lines indented by two spaces. Text that is blank after trimming is
/// dropped.
fn push_text(out: &mut Vec<String>, speaker: &str, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let mut lines = text.lines();
    if let Some(first) = lines.next() {
        out.push(format!("{speaker}: {first}"));
    }
    out.extend(lines.map(|line| format!("  {line}").trim_end().to_string()));
}

/// The first non-blank line of `value`, trimmed and cut to
/// [`BRIEF_MAX_CHARS`] characters with a trailing `…` when longer. Empty
/// when `value` has no non-blank line.
fn brief_of(value: &str) -> String {
    let line = value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    if line.chars().count() > BRIEF_MAX_CHARS {
        let cut: String = line.chars().take(BRIEF_MAX_CHARS).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(value: serde_json::Value) -> String {
        format!("{value}\n")
    }

    #[test]
    fn render_shows_user_string_content() {
        let jsonl = entry(serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "fix the bug" }
        }));
        assert_eq!(render(&jsonl), ["user: fix the bug"]);
    }

    #[test]
    fn render_shows_user_text_blocks() {
        let jsonl = entry(serde_json::json!({
            "type": "user",
            "message": { "content": [{ "type": "text", "text": "hello" }] }
        }));
        assert_eq!(render(&jsonl), ["user: hello"]);
    }

    #[test]
    fn render_shows_assistant_text_blocks() {
        let jsonl = entry(serde_json::json!({
            "type": "assistant",
            "message": { "content": [{ "type": "text", "text": "on it" }] }
        }));
        assert_eq!(render(&jsonl), ["assistant: on it"]);
    }

    #[test]
    fn render_indents_continuation_lines_of_multi_line_text() {
        let jsonl = entry(serde_json::json!({
            "type": "assistant",
            "message": { "content": [{ "type": "text", "text": "first\nsecond\nthird" }] }
        }));
        assert_eq!(render(&jsonl), ["assistant: first", "  second", "  third"]);
    }

    #[test]
    fn render_skips_text_that_is_empty_after_trimming() {
        let jsonl = entry(serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "text", "text": "  \n " },
                { "type": "text", "text": "kept" }
            ] }
        }));
        assert_eq!(render(&jsonl), ["assistant: kept"]);
    }

    #[test]
    fn render_briefs_tool_use_by_its_command() {
        let jsonl = entry(serde_json::json!({
            "type": "assistant",
            "message": { "content": [{
                "type": "tool_use",
                "name": "Bash",
                "input": { "description": "run tests", "command": "cargo test\necho done" }
            }] }
        }));
        assert_eq!(render(&jsonl), ["  -> Bash cargo test"]);
    }

    #[test]
    fn render_briefs_tool_use_by_the_first_present_field() {
        let jsonl = entry(serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "tool_use", "name": "Read", "input": { "file_path": "/a/b.rs" } },
                { "type": "tool_use", "name": "Grep", "input": { "pattern": "fn main", "path": "src" } },
                { "type": "tool_use", "name": "WebFetch", "input": { "url": "https://x.test", "prompt": "p" } },
                { "type": "tool_use", "name": "Agent", "input": { "description": "explore", "prompt": "p" } },
                { "type": "tool_use", "name": "Ask", "input": { "prompt": "why" } }
            ] }
        }));
        assert_eq!(
            render(&jsonl),
            [
                "  -> Read /a/b.rs",
                "  -> Grep src",
                "  -> WebFetch https://x.test",
                "  -> Agent explore",
                "  -> Ask why",
            ]
        );
    }

    #[test]
    fn render_falls_back_to_compact_json_input_for_unknown_tools() {
        let jsonl = entry(serde_json::json!({
            "type": "assistant",
            "message": { "content": [{
                "type": "tool_use", "name": "TodoWrite", "input": { "todos": [1] }
            }] }
        }));
        assert_eq!(render(&jsonl), [r#"  -> TodoWrite {"todos":[1]}"#]);
    }

    #[test]
    fn render_omits_the_trailing_space_when_the_tool_brief_is_empty() {
        let jsonl = entry(serde_json::json!({
            "type": "assistant",
            "message": { "content": [{
                "type": "tool_use", "name": "Bash", "input": { "command": "" }
            }] }
        }));
        assert_eq!(render(&jsonl), ["  -> Bash"]);
    }

    #[test]
    fn render_truncates_long_briefs_to_120_chars() {
        let long = "x".repeat(200);
        let jsonl = entry(serde_json::json!({
            "type": "assistant",
            "message": { "content": [{
                "type": "tool_use", "name": "Bash", "input": { "command": long }
            }] }
        }));
        let expected = format!("  -> Bash {}…", "x".repeat(120));
        assert_eq!(render(&jsonl), [expected]);
    }

    #[test]
    fn render_briefs_string_tool_results_by_first_non_empty_line() {
        let jsonl = entry(serde_json::json!({
            "type": "user",
            "message": { "content": [{
                "type": "tool_result", "tool_use_id": "t1", "content": "\n  \nok: 3 passed\nmore"
            }] }
        }));
        assert_eq!(render(&jsonl), ["  <- result: ok: 3 passed"]);
    }

    #[test]
    fn render_briefs_array_tool_results_by_their_text_blocks() {
        let jsonl = entry(serde_json::json!({
            "type": "user",
            "message": { "content": [{
                "type": "tool_result",
                "content": [{ "type": "text", "text": "line one\nline two" }]
            }] }
        }));
        assert_eq!(render(&jsonl), ["  <- result: line one"]);
    }

    #[test]
    fn render_shows_a_bare_marker_for_empty_tool_results() {
        let jsonl = entry(serde_json::json!({
            "type": "user",
            "message": { "content": [{ "type": "tool_result", "content": "" }] }
        }));
        assert_eq!(render(&jsonl), ["  <- result"]);
    }

    #[test]
    fn render_skips_thinking_blocks() {
        let jsonl = entry(serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "thinking", "thinking": "hmm" },
                { "type": "text", "text": "done" }
            ] }
        }));
        assert_eq!(render(&jsonl), ["assistant: done"]);
    }

    #[test]
    fn render_skips_meta_entries_other_types_and_unparsable_lines() {
        let mut jsonl = String::from("not json\n\n");
        jsonl += &entry(serde_json::json!({ "type": "summary", "summary": "s" }));
        jsonl += &entry(serde_json::json!({ "type": "system", "content": "sys" }));
        jsonl += &entry(serde_json::json!({ "type": "file-history-snapshot" }));
        jsonl += &entry(serde_json::json!({
            "type": "user", "isMeta": true, "message": { "content": "caveat" }
        }));
        jsonl += &entry(serde_json::json!({
            "type": "user", "message": { "content": "real" }
        }));
        assert_eq!(render(&jsonl), ["user: real"]);
    }

    #[test]
    fn locate_finds_the_session_file_in_any_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        std::fs::create_dir_all(projects.join("-a")).unwrap();
        std::fs::create_dir_all(projects.join("-b")).unwrap();
        let file = projects.join("-b/sess-1.jsonl");
        std::fs::write(&file, "").unwrap();
        assert_eq!(locate(dir.path(), "sess-1"), Some(file));
    }

    #[test]
    fn locate_ignores_a_directory_named_like_the_session_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects/-a/sess-1.jsonl")).unwrap();
        assert_eq!(locate(dir.path(), "sess-1"), None);
    }

    #[test]
    fn locate_rejects_empty_and_path_like_session_ids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects/-a")).unwrap();
        std::fs::write(dir.path().join("projects/-a/.jsonl"), "").unwrap();
        std::fs::write(dir.path().join("projects/x.jsonl"), "").unwrap();
        assert_eq!(locate(dir.path(), ""), None);
        assert_eq!(locate(dir.path(), "../x"), None);
        assert_eq!(locate(dir.path(), "a/b"), None);
    }

    #[test]
    fn claude_transcripts_locate_under_their_config_dir_and_render() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects/-a");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("sess-1.jsonl"), "").unwrap();
        let transcripts = ClaudeTranscripts::new(dir.path().to_path_buf());

        assert_eq!(
            transcripts.locate("sess-1"),
            Some(project.join("sess-1.jsonl"))
        );
        let jsonl = entry(serde_json::json!({
            "type": "user", "message": { "content": "hi" }
        }));
        assert_eq!(transcripts.render(&jsonl), ["user: hi"]);
    }

    #[test]
    fn locate_returns_none_when_the_projects_dir_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(locate(dir.path(), "sess-1"), None);
    }
}
