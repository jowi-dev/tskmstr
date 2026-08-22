//! `tm backend init-labels`.

use std::io::Write;

use thiserror::Error;

use crate::config::BackendKind;
use crate::github::gh_cli::GhCli;

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
    /// `tm backend init-labels` was run under a backend with no label
    /// taxonomy to create (currently, anything but GitHub).
    #[error(
        "tm backend init-labels only applies to the github backend; \
         this repo is configured for {provider}"
    )]
    NotGithubBackend {
        /// The configured provider's name, e.g. `"jira"`.
        provider: &'static str,
    },

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::gh_cli::{FakeGhCli, GhError};

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
            matches!(err, BackendCliError::NotGithubBackend { provider } if provider == "jira")
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
}
