//! `tm review auto <KEY>` (GitHub issue #124): review rounds that write
//! thatch findings into vdiff's comment store and drive `tm review fix`
//! until the review is clean or a round cap is hit. See
//! `docs/plans/gh-124-auto-review.md`.
//!
//! [`run_loop`] is the whole loop. Everything that runs an agent, posts to
//! GitHub or touches the run store goes through [`AutoReviewOps`], so the
//! loop itself is tested without any of them.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::config::{AutoReviewConfig, Severity};
use crate::work::comment_store::{self, CommentStoreError, NewComment};

/// Run `kind` of each review round's agent run.
pub const AUTO_REVIEW_KIND: &str = "auto-review";

/// Event kind recorded on each round's run row. Its detail is
/// `<round>/<max> <verdict>`; see [`RoundVerdict`].
pub const ROUND_EVENT: &str = "auto_review_round";

/// Skill a first round reviews with.
pub const FIRST_ROUND_SKILL: &str = "thatch-code-review";

/// Skill every later round reviews with, against the previous round's
/// archived comments.
pub const FOLLOWUP_SKILL: &str = "thatch-review-followup";

/// Built-in round prompt, used when `[work.auto_review]` sets neither
/// `prompt` nor `prompt_file`. Placeholders are those of [`render_prompt`].
pub const DEFAULT_PROMPT: &str = "\
Review the changes on this branch for {key} (auto-review round {round}) \
with the `{skill}` skill. This is unattended: do not ask questions, do not \
post anything to GitHub, and do not change any code.

{prior}When the review is done, write every CONFIRMED finding to \
`{findings_file}` as a JSON array, one object per finding:

    {\"path\": \"src/lib.rs\", \"start_line\": 10, \"end_line\": 12,
     \"severity\": \"MEDIUM\", \"specialist\": \"state-flow\",
     \"body\": \"What is wrong, why, and what to change.\"}

`path` is repo-relative; lines are 1-based and inclusive, anchored to the \
head SHA. `severity` is one of LOW, MEDIUM, HIGH, BLOCKING. Write `[]` when \
there are no findings. Write the file even if the review is clean.
";

/// Errors from [`run_loop`].
#[derive(Debug, Error)]
pub enum AutoReviewError {
    /// Reading or writing the comment store failed.
    #[error(transparent)]
    Store(#[from] CommentStoreError),

    /// A round's findings file was missing or not a JSON array of findings.
    #[error("auto-review findings file {path} is unusable: {message}")]
    Findings {
        /// The findings file.
        path: PathBuf,
        /// What was wrong with it.
        message: String,
    },

    /// One of [`AutoReviewOps`]' steps failed.
    #[error("auto-review {step} failed: {message}")]
    Step {
        /// Which step: `review`, `fix`, `publish` or `record`.
        step: &'static str,
        /// The underlying error, rendered.
        message: String,
    },
}

impl AutoReviewError {
    /// Wraps any displayable error as a [`AutoReviewError::Step`].
    pub fn step(step: &'static str, err: impl std::fmt::Display) -> Self {
        AutoReviewError::Step {
            step,
            message: err.to_string(),
        }
    }
}

fn default_specialist() -> String {
    "review".to_string()
}

/// One confirmed finding, as a review round writes it to its findings file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Finding {
    /// Repo-relative path.
    pub path: String,
    /// 1-based, inclusive first line.
    pub start_line: u64,
    /// 1-based, inclusive last line; `start_line` when absent.
    #[serde(default)]
    pub end_line: Option<u64>,
    /// Severity name; see [`Finding::severity`].
    pub severity: String,
    /// The specialist lens that found it, e.g. `state-flow`.
    #[serde(default = "default_specialist")]
    pub specialist: String,
    /// The finding itself.
    pub body: String,
}

impl Finding {
    /// The parsed severity. An unknown name counts as [`Severity::Low`], so
    /// a malformed finding is still shown but never forces a fix.
    pub fn severity(&self) -> Severity {
        Severity::parse(&self.severity).unwrap_or(Severity::Low)
    }

    /// The comment-store entry for this finding, its text prefixed with
    /// provenance and severity so it reads apart from a human's comment.
    pub fn to_comment(&self) -> NewComment {
        NewComment {
            path: self.path.clone(),
            start_line: self.start_line,
            end_line: self
                .end_line
                .unwrap_or(self.start_line)
                .max(self.start_line),
            text: format!(
                "[auto-review · {} · {}] {}",
                self.specialist,
                self.severity().as_str(),
                self.body
            ),
        }
    }
}

/// Reads a round's findings file.
///
/// # Errors
///
/// [`AutoReviewError::Findings`] if the file is missing or isn't a JSON
/// array of [`Finding`]s.
pub fn read_findings(path: &Path) -> Result<Vec<Finding>, AutoReviewError> {
    let unusable = |message: String| AutoReviewError::Findings {
        path: path.to_path_buf(),
        message,
    };
    let raw = std::fs::read_to_string(path).map_err(|e| unusable(e.to_string()))?;
    serde_json::from_str(&raw).map_err(|e| unusable(e.to_string()))
}

/// Where round `round` writes its findings:
/// `<git_dir>/vdiff/auto-review/findings-<round>.json`. Under the git dir,
/// like the comment store, so it never shows in `git status`.
pub fn findings_path(git_dir: &Path, round: u32) -> PathBuf {
    git_dir
        .join("vdiff")
        .join("auto-review")
        .join(format!("findings-{round}.json"))
}

/// Renders a round's prompt from `template`, replacing `{key}`, `{round}`,
/// `{skill}`, `{findings_file}`, `{archive}` (empty on round 1) and
/// `{prior}` (a sentence pointing a follow-up round at `archive`, empty on
/// round 1).
pub fn render_prompt(
    template: &str,
    key: &str,
    round: u32,
    findings_file: &Path,
    archive: Option<&Path>,
) -> String {
    let skill = if round <= 1 {
        FIRST_ROUND_SKILL
    } else {
        FOLLOWUP_SKILL
    };
    let archive_text = archive
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prior = match archive {
        Some(path) => format!(
            "Your prior round's findings are in `{}` (vdiff comment-store \
             JSON; yours are the entries whose `text` starts with \
             `[auto-review · ...]`, any others are a human's). They were never \
             posted to GitHub: use this file, not the PR, as your prior review \
             comments, and verify whether each was addressed since.\n\n",
            path.display()
        ),
        None => String::new(),
    };
    template
        .replace("{key}", key)
        .replace("{round}", &round.to_string())
        .replace("{skill}", skill)
        .replace("{findings_file}", &findings_file.to_string_lossy())
        .replace("{archive}", &archive_text)
        .replace("{prior}", &prior)
}

/// Everything [`run_loop`] needs done outside the comment store.
pub trait AutoReviewOps {
    /// Runs one review round's agent with `prompt`, to completion, and
    /// returns its run row id. The agent writes its findings to the
    /// `findings_file` the prompt names.
    fn review(&self, round: u32, prompt: String) -> Result<i64, AutoReviewError>;
    /// Runs `tm review fix` over the current store, to completion. `false`
    /// when the fix run finished but failed.
    fn fix(&self) -> Result<bool, AutoReviewError>;
    /// Posts the store with `vdiff --publish-comments <pr>`.
    fn publish(&self) -> Result<(), AutoReviewError>;
    /// Records an event on run `run_id`.
    fn record(&self, run_id: i64, kind: &str, detail: &str) -> Result<(), AutoReviewError>;
    /// Flags run `run_id` for a human: the loop hit its cap.
    fn flag(&self, run_id: i64) -> Result<(), AutoReviewError>;
    /// Whether the ticket has used up `[dispatch] max_fix_rounds`, the fix
    /// cap shared by every automatic trigger (GitHub issue #120).
    fn fix_cap_reached(&self) -> Result<bool, AutoReviewError>;
    /// Seconds since the Unix epoch, for comments' `created_at`.
    fn now_unix_secs(&self) -> i64;
}

/// How one round ended, recorded as the tail of its [`ROUND_EVENT`]
/// detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundVerdict {
    /// Nothing at or above the threshold: the loop is done.
    Clean,
    /// Findings at or above the threshold went to a fix round.
    Fix,
    /// Findings at or above the threshold, but this was the last round.
    Capped,
    /// The fix round failed; the loop stopped.
    FixFailed,
}

impl RoundVerdict {
    /// The word recorded in the event detail.
    pub fn as_str(self) -> &'static str {
        match self {
            RoundVerdict::Clean => "clean",
            RoundVerdict::Fix => "fix",
            RoundVerdict::Capped => "capped",
            RoundVerdict::FixFailed => "fix-failed",
        }
    }
}

/// How [`run_loop`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopOutcome {
    /// Rounds reviewed.
    pub rounds: u32,
    /// The last round's verdict: never [`RoundVerdict::Fix`].
    pub verdict: RoundVerdict,
}

/// Runs review rounds until one is clean, the fix round fails, or
/// `cfg.max_review_rounds` is reached. See the plan doc for the round
/// sequence; `git_dir` is the worktree's resolved git directory.
///
/// # Errors
///
/// Any step's error stops the loop and is returned as is.
pub fn run_loop(
    ops: &dyn AutoReviewOps,
    cfg: &AutoReviewConfig,
    template: &str,
    key: &str,
    git_dir: &Path,
) -> Result<LoopOutcome, AutoReviewError> {
    let max = cfg.max_review_rounds.max(1);
    let mut archive: Option<PathBuf> = None;
    for round in 1..=max {
        let findings_file = findings_path(git_dir, round);
        if let Some(dir) = findings_file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| AutoReviewError::step("review", e))?;
        }
        // A stale file from an earlier loop must never stand in for this
        // round's output.
        let _ = std::fs::remove_file(&findings_file);

        let prompt = render_prompt(template, key, round, &findings_file, archive.as_deref());
        let run_id = ops.review(round, prompt)?;
        let findings = read_findings(&findings_file)?;
        let comments: Vec<NewComment> = findings.iter().map(Finding::to_comment).collect();
        if !comments.is_empty() {
            let created_at = comment_store::iso8601_utc(ops.now_unix_secs());
            comment_store::append(git_dir, &comments, &created_at)?;
        }
        if cfg.publish && !comments.is_empty() {
            ops.publish()?;
        }

        let needs_fix = findings.iter().any(|f| f.severity() >= cfg.fix_threshold);
        let verdict = match (needs_fix, round == max) {
            (false, _) => RoundVerdict::Clean,
            (true, true) => RoundVerdict::Capped,
            (true, false) if ops.fix_cap_reached()? => RoundVerdict::Capped,
            (true, false) => RoundVerdict::Fix,
        };
        if verdict != RoundVerdict::Fix {
            if verdict == RoundVerdict::Capped {
                ops.flag(run_id)?;
            }
            ops.record(run_id, ROUND_EVENT, &round_detail(round, max, verdict))?;
            return Ok(LoopOutcome {
                rounds: round,
                verdict,
            });
        }

        let fixed = ops.fix()?;
        let verdict = if fixed {
            RoundVerdict::Fix
        } else {
            RoundVerdict::FixFailed
        };
        ops.record(run_id, ROUND_EVENT, &round_detail(round, max, verdict))?;
        if !fixed {
            return Ok(LoopOutcome {
                rounds: round,
                verdict,
            });
        }
        archive = comment_store::archive(git_dir, round)?;
    }
    unreachable!("the last round always returns")
}

fn round_detail(round: u32, max: u32, verdict: RoundVerdict) -> String {
    format!("{round}/{max} {}", verdict.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tempfile::TempDir;

    /// Scripted ops: round `n`'s agent "writes" `findings[n - 1]` to the
    /// findings file the prompt names, and every call is logged.
    struct FakeOps {
        git_dir: PathBuf,
        findings: Vec<&'static str>,
        fix_results: RefCell<Vec<bool>>,
        log: RefCell<Vec<String>>,
        prompts: RefCell<Vec<String>>,
        store_at_fix: RefCell<Vec<usize>>,
        fix_cap_reached: bool,
    }

    impl FakeOps {
        fn new(git_dir: &Path, findings: Vec<&'static str>) -> Self {
            FakeOps {
                git_dir: git_dir.to_path_buf(),
                findings,
                fix_results: RefCell::new(Vec::new()),
                log: RefCell::new(Vec::new()),
                prompts: RefCell::new(Vec::new()),
                store_at_fix: RefCell::new(Vec::new()),
                fix_cap_reached: false,
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
    }

    impl AutoReviewOps for FakeOps {
        fn review(&self, round: u32, prompt: String) -> Result<i64, AutoReviewError> {
            self.log.borrow_mut().push(format!("review {round}"));
            self.prompts.borrow_mut().push(prompt);
            let body = self.findings[(round - 1) as usize];
            std::fs::write(findings_path(&self.git_dir, round), body).unwrap();
            Ok(100 + i64::from(round))
        }

        fn fix(&self) -> Result<bool, AutoReviewError> {
            self.log.borrow_mut().push("fix".to_string());
            self.store_at_fix
                .borrow_mut()
                .push(comment_store::load(&self.git_dir).unwrap().len());
            let mut results = self.fix_results.borrow_mut();
            Ok(if results.is_empty() {
                true
            } else {
                results.remove(0)
            })
        }

        fn publish(&self) -> Result<(), AutoReviewError> {
            self.log.borrow_mut().push("publish".to_string());
            Ok(())
        }

        fn record(&self, run_id: i64, kind: &str, detail: &str) -> Result<(), AutoReviewError> {
            self.log
                .borrow_mut()
                .push(format!("record {run_id} {kind} {detail}"));
            Ok(())
        }

        fn flag(&self, run_id: i64) -> Result<(), AutoReviewError> {
            self.log.borrow_mut().push(format!("flag {run_id}"));
            Ok(())
        }

        fn fix_cap_reached(&self) -> Result<bool, AutoReviewError> {
            Ok(self.fix_cap_reached)
        }

        fn now_unix_secs(&self) -> i64 {
            1_787_054_400
        }
    }

    const MEDIUM: &str = r#"[{"path": "src/a.rs", "start_line": 4, "end_line": 6,
        "severity": "MEDIUM", "specialist": "state-flow", "body": "race"}]"#;
    const LOW: &str = r#"[{"path": "src/a.rs", "start_line": 9,
        "severity": "low", "specialist": "pedantic", "body": "typo"}]"#;

    fn cfg() -> AutoReviewConfig {
        AutoReviewConfig::default()
    }

    #[test]
    fn finding_to_comment_prefixes_provenance_and_severity() {
        let finding = Finding {
            path: "src/a.rs".to_string(),
            start_line: 4,
            end_line: None,
            severity: "high".to_string(),
            specialist: "economy".to_string(),
            body: "simplify".to_string(),
        };

        let comment = finding.to_comment();

        assert_eq!(comment.text, "[auto-review · economy · HIGH] simplify");
        assert_eq!((comment.start_line, comment.end_line), (4, 4));
    }

    #[test]
    fn an_unknown_severity_counts_as_low() {
        let finding: Vec<Finding> =
            serde_json::from_str(r#"[{"path":"a","start_line":1,"severity":"??","body":"b"}]"#)
                .unwrap();
        assert_eq!(finding[0].severity(), Severity::Low);
        assert_eq!(finding[0].specialist, "review");
    }

    #[test]
    fn read_findings_rejects_a_missing_file() {
        let tmp = TempDir::new().unwrap();
        assert!(matches!(
            read_findings(&tmp.path().join("nope.json")),
            Err(AutoReviewError::Findings { .. })
        ));
    }

    #[test]
    fn render_prompt_uses_code_review_first_and_followup_with_the_archive_after() {
        let first = render_prompt(DEFAULT_PROMPT, "GH-1", 1, Path::new("/g/f-1.json"), None);
        assert!(first.contains("`thatch-code-review`"));
        assert!(first.contains("/g/f-1.json"));
        assert!(first.contains("GH-1"));
        assert!(!first.contains("prior round"));
        for placeholder in [
            "{{",
            "{key}",
            "{round}",
            "{skill}",
            "{findings_file}",
            "{prior}",
        ] {
            assert!(
                !first.contains(placeholder),
                "{placeholder} left in: {first}"
            );
        }

        let second = render_prompt(
            DEFAULT_PROMPT,
            "GH-1",
            2,
            Path::new("/g/f-2.json"),
            Some(Path::new("/g/archive/1.json")),
        );
        assert!(second.contains("`thatch-review-followup`"));
        assert!(second.contains("/g/archive/1.json"));
    }

    #[test]
    fn a_clean_first_round_writes_low_findings_and_stops_without_a_fix() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![LOW]);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path()).unwrap();

        assert_eq!(
            outcome,
            LoopOutcome {
                rounds: 1,
                verdict: RoundVerdict::Clean
            }
        );
        assert_eq!(
            ops.log(),
            vec!["review 1", "record 101 auto_review_round 1/2 clean"]
        );
        let store = comment_store::load(tmp.path()).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store[0]["text"], "[auto-review · pedantic · LOW] typo");
        assert_eq!(store[0]["created_at"], "2026-08-18T12:00:00Z");
    }

    #[test]
    fn a_finding_at_the_threshold_fixes_archives_and_runs_a_followup_round() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path()).unwrap();

        assert_eq!(
            outcome,
            LoopOutcome {
                rounds: 2,
                verdict: RoundVerdict::Clean
            }
        );
        assert_eq!(
            ops.log(),
            vec![
                "review 1",
                "fix",
                "record 101 auto_review_round 1/2 fix",
                "review 2",
                "record 102 auto_review_round 2/2 clean",
            ]
        );
        assert_eq!(
            *ops.store_at_fix.borrow(),
            vec![1],
            "fix sees round 1's comment"
        );
        assert!(comment_store::load(tmp.path()).unwrap().is_empty());
        assert!(comment_store::archive_path(tmp.path(), 1).exists());
        let prompts = ops.prompts.borrow();
        assert!(prompts[1].contains(FOLLOWUP_SKILL));
        assert!(
            prompts[1].contains(&*comment_store::archive_path(tmp.path(), 1).to_string_lossy())
        );
    }

    #[test]
    fn hitting_the_cap_flags_the_round_and_leaves_the_store() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM, MEDIUM]);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path()).unwrap();

        assert_eq!(
            outcome,
            LoopOutcome {
                rounds: 2,
                verdict: RoundVerdict::Capped
            }
        );
        assert_eq!(
            ops.log()[3..],
            [
                "review 2",
                "flag 102",
                "record 102 auto_review_round 2/2 capped"
            ]
        );
        assert_eq!(comment_store::load(tmp.path()).unwrap().len(), 1);
    }

    #[test]
    fn the_shared_fix_round_cap_stops_the_loop_before_a_fix() {
        let tmp = TempDir::new().unwrap();
        let mut ops = FakeOps::new(tmp.path(), vec![MEDIUM, MEDIUM]);
        ops.fix_cap_reached = true;

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path()).unwrap();

        assert_eq!(
            outcome,
            LoopOutcome {
                rounds: 1,
                verdict: RoundVerdict::Capped
            }
        );
        assert_eq!(
            ops.log(),
            [
                "review 1",
                "flag 101",
                "record 101 auto_review_round 1/2 capped"
            ]
        );
        assert_eq!(comment_store::load(tmp.path()).unwrap().len(), 1);
    }

    #[test]
    fn existing_human_comments_are_kept_and_fed_to_the_fix() {
        let tmp = TempDir::new().unwrap();
        comment_store::append(
            tmp.path(),
            &[NewComment {
                path: "src/z.rs".to_string(),
                start_line: 1,
                end_line: 1,
                text: "human".to_string(),
            }],
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);

        run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path()).unwrap();

        assert_eq!(*ops.store_at_fix.borrow(), vec![2]);
    }

    #[test]
    fn a_failed_fix_stops_the_loop() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);
        ops.fix_results.borrow_mut().push(false);

        let outcome = run_loop(&ops, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path()).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::FixFailed);
        assert_eq!(
            ops.log(),
            vec![
                "review 1",
                "fix",
                "record 101 auto_review_round 1/2 fix-failed"
            ]
        );
        assert_eq!(
            comment_store::load(tmp.path()).unwrap().len(),
            1,
            "an unconsumed store is not archived"
        );
    }

    #[test]
    fn publish_runs_after_each_round_with_findings_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM, "[]"]);
        let cfg = AutoReviewConfig {
            publish: true,
            ..cfg()
        };

        run_loop(&ops, &cfg, DEFAULT_PROMPT, "GH-1", tmp.path()).unwrap();

        assert_eq!(ops.log()[..2], ["review 1", "publish"]);
        assert_eq!(
            ops.log().iter().filter(|l| *l == "publish").count(),
            1,
            "an empty round has nothing to publish"
        );
    }

    #[test]
    fn a_higher_threshold_lets_medium_findings_through_clean() {
        let tmp = TempDir::new().unwrap();
        let ops = FakeOps::new(tmp.path(), vec![MEDIUM]);
        let cfg = AutoReviewConfig {
            fix_threshold: Severity::High,
            ..cfg()
        };

        let outcome = run_loop(&ops, &cfg, DEFAULT_PROMPT, "GH-1", tmp.path()).unwrap();

        assert_eq!(outcome.verdict, RoundVerdict::Clean);
        assert!(!ops.log().contains(&"fix".to_string()));
    }

    #[test]
    fn a_round_whose_agent_wrote_no_findings_file_is_an_error() {
        struct SilentOps;
        impl AutoReviewOps for SilentOps {
            fn review(&self, _: u32, _: String) -> Result<i64, AutoReviewError> {
                Ok(1)
            }
            fn fix(&self) -> Result<bool, AutoReviewError> {
                unreachable!()
            }
            fn publish(&self) -> Result<(), AutoReviewError> {
                unreachable!()
            }
            fn record(&self, _: i64, _: &str, _: &str) -> Result<(), AutoReviewError> {
                unreachable!()
            }
            fn flag(&self, _: i64) -> Result<(), AutoReviewError> {
                unreachable!()
            }
            fn fix_cap_reached(&self) -> Result<bool, AutoReviewError> {
                unreachable!()
            }
            fn now_unix_secs(&self) -> i64 {
                0
            }
        }
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(findings_path(tmp.path(), 1).parent().unwrap()).unwrap();
        std::fs::write(findings_path(tmp.path(), 1), "[]").unwrap();

        let err = run_loop(&SilentOps, &cfg(), DEFAULT_PROMPT, "GH-1", tmp.path()).unwrap_err();

        assert!(
            matches!(err, AutoReviewError::Findings { .. }),
            "a stale findings file is cleared before the round: {err}"
        );
    }
}
