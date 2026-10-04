//! `tm backend init-labels` and `tm backend clean-status-labels`.

use std::collections::BTreeMap;
use std::io::Write;

use thiserror::Error;

use crate::config::BackendKind;
use crate::github::gh_cli::{GhCli, IssueEditRequest, IssueListFilter, IssueListState, IssueState};
use crate::ticketing::github_provider::STATUS_LABEL_PREFIX;

/// `--limit` for each closed-issue listing in [`clean_status_labels`]:
/// closed issues accumulate forever, so this is well above
/// [`IssueListFilter`]'s open-issue default.
const CLEAN_LIST_LIMIT: u32 = 1000;

/// The `tm:status/*` label taxonomy (`docs/plans/github-issues-backend.md`)
/// as `(name, color, description)` triples. Colors are plain hex without a
/// leading `#`, matching `gh label create --color`'s expected format.
const STATUS_LABELS: &[(&str, &str, &str)] = &[
    ("tm:status/todo", "ededed", "tm board: To Do"),
    ("tm:status/in-progress", "fbca04", "tm board: In Progress"),
    ("tm:status/in-review", "0e8a16", "tm board: In Review"),
    ("tm:status/blocked", "d73a4a", "tm board: Blocked"),
];

/// Errors surfaced by `tm backend` subcommands.
#[derive(Debug, Error)]
pub enum BackendCliError {
    /// A `tm backend` label subcommand was run under a backend with no
    /// label taxonomy (currently, anything but GitHub).
    #[error(
        "{command} only applies to the github backend; \
         this repo is configured for {provider}"
    )]
    NotGithubBackend {
        /// The subcommand that was run, e.g. `"tm backend init-labels"`.
        command: &'static str,
        /// The configured provider's name, e.g. `"jira"`.
        provider: &'static str,
    },

    /// Listing or editing issues failed.
    #[error(transparent)]
    Gh(#[from] crate::github::gh_cli::GhError),

    /// Creating one of the labels failed.
    #[error("failed to create label `{label}`: {source}")]
    LabelCreate {
        /// The label name that failed to create.
        label: &'static str,
        /// The underlying `gh` error.
        #[source]
        source: crate::github::gh_cli::GhError,
    },

    /// A prompt or output write failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Idempotently create every `tm:status/*` label (see [`STATUS_LABELS`]) in
/// `repo`, printing one confirmation line per label. Returns
/// [`BackendCliError::NotGithubBackend`] without calling `gh` at all when
/// `backend` isn't [`BackendKind::Github`] — there's no label taxonomy to
/// create under any other provider.
pub fn init_labels(
    backend: BackendKind,
    repo: &str,
    gh: &dyn GhCli,
    out: &mut dyn Write,
) -> Result<(), BackendCliError> {
    if backend != BackendKind::Github {
        return Err(BackendCliError::NotGithubBackend {
            command: "tm backend init-labels",
            provider: backend.as_str(),
        });
    }

    for (name, color, description) in STATUS_LABELS {
        gh.label_create(repo, name, color, description)
            .map_err(|source| BackendCliError::LabelCreate {
                label: name,
                source,
            })?;
        writeln!(out, "Created label {name} in {repo}")?;
    }

    Ok(())
}

/// Strip every `tm:status/*` label from every closed issue in `repo`,
/// printing one line per issue cleaned (or, with `dry_run`, per issue that
/// would be). Open issues are never touched, even if a listing returns one.
///
/// Backfills repos whose issues GitHub closed via a merged PR's closing
/// keyword before `tm merge` swept labels itself, and catches the race
/// where GitHub closes the issue only after `tm merge` already looked
/// (GitHub issue #75). `gh issue list --label` ANDs its labels, so this
/// issues one closed-issue listing per status label and dedupes by number.
/// Returns [`BackendCliError::NotGithubBackend`] without calling `gh` when
/// `backend` isn't [`BackendKind::Github`].
pub fn clean_status_labels(
    backend: BackendKind,
    repo: &str,
    dry_run: bool,
    gh: &dyn GhCli,
    out: &mut dyn Write,
) -> Result<(), BackendCliError> {
    if backend != BackendKind::Github {
        return Err(BackendCliError::NotGithubBackend {
            command: "tm backend clean-status-labels",
            provider: backend.as_str(),
        });
    }

    let mut stale: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for (name, ..) in STATUS_LABELS {
        let filter = IssueListFilter {
            state: IssueListState::Closed,
            labels: vec![name.to_string()],
            limit: CLEAN_LIST_LIMIT,
            ..Default::default()
        };
        for issue in gh.issue_list(repo, &filter)? {
            if !matches!(issue.state, IssueState::Closed) {
                continue;
            }
            stale.entry(issue.number).or_insert_with(|| {
                issue
                    .labels
                    .into_iter()
                    .filter(|label| label.starts_with(STATUS_LABEL_PREFIX))
                    .collect()
            });
        }
    }
    stale.retain(|_, labels| !labels.is_empty());

    if stale.is_empty() {
        writeln!(out, "No closed issues carry tm:status/* labels in {repo}")?;
        return Ok(());
    }

    for (number, labels) in stale {
        let joined = labels.join(", ");
        if dry_run {
            writeln!(out, "Would clear {joined} from closed GH-{number}")?;
            continue;
        }
        let req = IssueEditRequest {
            remove_labels: labels,
            ..Default::default()
        };
        gh.issue_edit(repo, number, &req)?;
        writeln!(out, "Cleared {joined} from closed GH-{number}")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::{FakeGhCli, GhError, IssueInfo, IssueListState, IssueState};

    #[test]
    fn init_labels_creates_every_status_label() {
        let fake = FakeGhCli::new();
        let mut out = Vec::new();

        init_labels(BackendKind::Github, "jowi-dev/tskmstr", &fake, &mut out).unwrap();

        let calls = fake.label_create_calls();
        assert_eq!(calls.len(), 4);
        let names: Vec<&str> = calls.iter().map(|(_, name, ..)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "tm:status/todo",
                "tm:status/in-progress",
                "tm:status/in-review",
                "tm:status/blocked",
            ]
        );
        for (repo, ..) in &calls {
            assert_eq!(repo, "jowi-dev/tskmstr");
        }
    }

    #[test]
    fn init_labels_prints_one_confirmation_per_label() {
        let fake = FakeGhCli::new();
        let mut out = Vec::new();

        init_labels(BackendKind::Github, "jowi-dev/tskmstr", &fake, &mut out).unwrap();

        let printed = String::from_utf8(out).unwrap();
        assert_eq!(printed.lines().count(), 4);
        assert!(printed.contains("Created label tm:status/todo in jowi-dev/tskmstr"));
    }

    #[test]
    fn init_labels_under_jira_backend_is_an_error_and_calls_gh_nothing() {
        let fake = FakeGhCli::new();
        let mut out = Vec::new();

        let err = init_labels(BackendKind::Jira, "jowi-dev/tskmstr", &fake, &mut out)
            .expect_err("should fail");

        assert!(
            matches!(err, BackendCliError::NotGithubBackend { provider, .. } if provider == "jira")
        );
        assert!(fake.label_create_calls().is_empty());
        assert!(out.is_empty());
    }

    #[test]
    fn init_labels_stops_on_first_failure() {
        let fake = FakeGhCli::new().with_label_create_result(Err(GhError::Command {
            command: "gh label create".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));
        let mut out = Vec::new();

        let err = init_labels(BackendKind::Github, "jowi-dev/tskmstr", &fake, &mut out)
            .expect_err("should fail");

        assert!(matches!(
            err,
            BackendCliError::LabelCreate {
                label: "tm:status/todo",
                ..
            }
        ));
        // Only the first (failing) label was attempted.
        assert_eq!(fake.label_create_calls().len(), 1);
    }

    // --- clean_status_labels ---

    fn closed(number: u64, labels: &[&str]) -> IssueInfo {
        IssueInfo {
            number,
            url: format!("https://github.com/jowi-dev/tskmstr/issues/{number}"),
            title: format!("Issue {number}"),
            body: String::new(),
            state: IssueState::Closed,
            labels: labels.iter().map(|label| label.to_string()).collect(),
            assignees: Vec::new(),
        }
    }

    #[test]
    fn lists_closed_issues_once_per_status_label() {
        // gh's --label filter is an AND, so one query per label is the only
        // way to find issues carrying *any* status label.
        let fake = FakeGhCli::new();
        let mut out = Vec::new();

        clean_status_labels(
            BackendKind::Github,
            "jowi-dev/tskmstr",
            false,
            &fake,
            &mut out,
        )
        .unwrap();

        let calls = fake.issue_list_calls();
        let labels: Vec<Vec<String>> = calls.iter().map(|(_, f)| f.labels.clone()).collect();
        assert_eq!(
            labels,
            vec![
                vec!["tm:status/todo".to_string()],
                vec!["tm:status/in-progress".to_string()],
                vec!["tm:status/in-review".to_string()],
                vec!["tm:status/blocked".to_string()],
            ]
        );
        for (repo, filter) in &calls {
            assert_eq!(repo, "jowi-dev/tskmstr");
            assert_eq!(filter.state, IssueListState::Closed);
        }
    }

    #[test]
    fn strips_every_status_label_from_each_closed_issue_once() {
        // The fake returns the same listing for every label query; each
        // issue must still be edited exactly once.
        let fake = FakeGhCli::new().with_issue_list(Ok(vec![
            closed(55, &["bug", "tm:status/in-review"]),
            closed(61, &["tm:status/in-progress", "tm:status/blocked"]),
        ]));
        let mut out = Vec::new();

        clean_status_labels(
            BackendKind::Github,
            "jowi-dev/tskmstr",
            false,
            &fake,
            &mut out,
        )
        .unwrap();

        let edits = fake.issue_edit_calls();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].1, 55);
        assert_eq!(edits[0].2.remove_labels, vec!["tm:status/in-review"]);
        assert_eq!(edits[0].2.state, None);
        assert_eq!(edits[1].1, 61);
        assert_eq!(
            edits[1].2.remove_labels,
            vec!["tm:status/in-progress", "tm:status/blocked"]
        );
        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.contains("Cleared tm:status/in-review from closed GH-55"),
            "{printed}"
        );
        assert!(
            printed.contains("Cleared tm:status/in-progress, tm:status/blocked from closed GH-61"),
            "{printed}"
        );
    }

    #[test]
    fn never_touches_an_open_issue_even_if_listed() {
        let mut open = closed(27, &["tm:status/in-review"]);
        open.state = IssueState::Open;
        let fake = FakeGhCli::new().with_issue_list(Ok(vec![open]));
        let mut out = Vec::new();

        clean_status_labels(
            BackendKind::Github,
            "jowi-dev/tskmstr",
            false,
            &fake,
            &mut out,
        )
        .unwrap();

        assert!(fake.issue_edit_calls().is_empty());
    }

    #[test]
    fn dry_run_reports_without_editing() {
        let fake = FakeGhCli::new().with_issue_list(Ok(vec![closed(55, &["tm:status/in-review"])]));
        let mut out = Vec::new();

        clean_status_labels(
            BackendKind::Github,
            "jowi-dev/tskmstr",
            true,
            &fake,
            &mut out,
        )
        .unwrap();

        assert!(fake.issue_edit_calls().is_empty());
        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.contains("Would clear tm:status/in-review from closed GH-55"),
            "{printed}"
        );
    }

    #[test]
    fn reports_when_nothing_is_stale() {
        let fake = FakeGhCli::new();
        let mut out = Vec::new();

        clean_status_labels(
            BackendKind::Github,
            "jowi-dev/tskmstr",
            false,
            &fake,
            &mut out,
        )
        .unwrap();

        let printed = String::from_utf8(out).unwrap();
        assert_eq!(
            printed,
            "No closed issues carry tm:status/* labels in jowi-dev/tskmstr\n"
        );
    }

    #[test]
    fn under_jira_backend_is_an_error_and_calls_gh_nothing() {
        let fake = FakeGhCli::new();
        let mut out = Vec::new();

        let err = clean_status_labels(
            BackendKind::Jira,
            "jowi-dev/tskmstr",
            false,
            &fake,
            &mut out,
        )
        .expect_err("should fail");

        assert!(
            err.to_string().contains("tm backend clean-status-labels"),
            "{err}"
        );
        assert!(fake.issue_list_calls().is_empty());
    }

    #[test]
    fn list_failure_is_an_error() {
        let fake = FakeGhCli::new().with_issue_list(Err(GhError::Command {
            command: "gh issue list".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));
        let mut out = Vec::new();

        let err = clean_status_labels(
            BackendKind::Github,
            "jowi-dev/tskmstr",
            false,
            &fake,
            &mut out,
        )
        .expect_err("should fail");

        assert!(matches!(err, BackendCliError::Gh(_)));
    }
}
