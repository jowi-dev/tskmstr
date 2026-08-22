//! [`ProviderError`], the backend-agnostic error every
//! [`super::provider::TicketProvider`] method returns.
//!
//! Before this module, [`TicketProvider`](super::provider::TicketProvider)
//! returned [`JiraError`] directly, so a non-Jira adapter would have had to
//! impersonate Jira's error shape just to report "not found" or "rate
//! limited." [`ProviderError`] mirrors every [`JiraError`] variant one for
//! one — same names, same fields, only the wording of the `#[error(...)]`
//! messages and the [`JiraError::Http`] payload (a formatted `String`
//! instead of a live `reqwest::Error`, so this type doesn't pull `reqwest`
//! into the provider layer for a backend that may never use it) changed —
//! so every existing `match`/`matches!` on a Jira-specific variant name
//! keeps expressing the same classification after a one-line rename to the
//! [`ProviderError`] path. [`super::provider::JiraProvider`] converts at the
//! boundary via the [`From<JiraError>`] impl below; nothing downstream of
//! [`super::provider::TicketProvider`] needs to know [`JiraError`] exists.

use crate::github::gh_cli::GhError;
use crate::jira::client::JiraError;
use thiserror::Error;

/// Errors that can occur while calling a [`super::provider::TicketProvider`]
/// method, regardless of which backend is configured.
///
/// See the module doc comment for how this relates to [`JiraError`]: every
/// variant here has a one-to-one [`JiraError`] counterpart, converted by
/// [`From<JiraError>`] at the [`super::provider::JiraProvider`] boundary.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The requested ticket does not exist.
    #[error("ticket not found: {key}")]
    NotFound {
        /// The ticket key that was not found.
        key: String,
    },

    /// [`super::provider::TicketProvider::assignable_users`] found no such
    /// project: unlike a ticket-key not-found, this doesn't mean "not found
    /// by key" (there is no key), it means the project itself doesn't
    /// exist or isn't visible to the authenticated user. Kept distinct
    /// from [`ProviderError::NotFound`] so the error text names a project
    /// and an assignable-user search, not a ticket.
    #[error("project not found while searching assignable users: {project}")]
    ProjectNotFound {
        /// The project key that was not found.
        project: String,
    },

    /// The request was rejected as unauthenticated or unauthorized.
    #[error("ticket provider request unauthorized; check the configured credentials")]
    Unauthorized,

    /// The request could not be sent, or the response could not be read.
    /// Carries the underlying transport error's `Display` text rather than
    /// the error itself, since a non-HTTP backend (e.g. a `gh` shell-out)
    /// has no `reqwest::Error` to report.
    #[error("ticket provider request failed: {0}")]
    Http(String),

    /// The backend returned an error response not otherwise categorized.
    #[error("ticket provider API error ({status}): {message}")]
    Api {
        /// HTTP status code (or backend-equivalent) returned.
        status: u16,
        /// Error message extracted from the response body, if any.
        message: String,
    },

    /// [`super::provider::TicketProvider::rank`] found no such ticket: a
    /// rank request names both the ticket(s) being moved and an anchor
    /// ticket, and the backend's error doesn't say which one is missing.
    /// Kept distinct from [`ProviderError::NotFound`] so the error text
    /// names every key involved instead of a single one.
    #[error("ticket not found while ranking {keys} relative to {anchor}")]
    RankNotFound {
        /// The ticket keys that were to be ranked, comma-joined.
        keys: String,
        /// The anchor ticket key.
        anchor: String,
    },

    /// [`super::provider::TicketProvider::rank`] partially failed: at least
    /// one ticket in the request could not be re-ranked. Treated as a hard
    /// error since tskmstr only ever ranks one ticket at a time and expects
    /// all-or-nothing success.
    #[error("ticket rank request partially failed: {message}")]
    RankPartialFailure {
        /// Detail extracted from the response body, if any.
        message: String,
    },

    /// [`super::provider::TicketProvider::create_link`] found no such
    /// ticket: like [`ProviderError::RankNotFound`], a link request names
    /// two tickets and the failure doesn't reliably say which one is
    /// missing. Kept distinct from [`ProviderError::NotFound`] so the error
    /// text names both keys involved instead of a single one.
    #[error("ticket not found while linking {blocker} as blocking {blocked}")]
    LinkNotFound {
        /// The ticket key that was to block `blocked`.
        blocker: String,
        /// The ticket key that was to be blocked by `blocker`.
        blocked: String,
    },

    /// [`super::provider::TicketProvider::delete_link`] found no such link
    /// id. The generic [`ProviderError::NotFound`] would report `link_id`
    /// using the `key` wording, misreporting a link id as a ticket key —
    /// the same reasoning as [`ProviderError::ProjectNotFound`] being kept
    /// distinct from `NotFound`.
    #[error("ticket link not found: {link_id}")]
    LinkIdNotFound {
        /// The issue-link id that was not found.
        link_id: String,
    },
}

impl From<JiraError> for ProviderError {
    /// Converts a Jira-specific error into its provider-level counterpart at
    /// the [`super::provider::JiraProvider`] boundary. One-to-one on
    /// variants and fields; see the module doc comment.
    fn from(err: JiraError) -> Self {
        match err {
            JiraError::NotFound { key } => ProviderError::NotFound { key },
            JiraError::ProjectNotFound { project } => ProviderError::ProjectNotFound { project },
            JiraError::Unauthorized => ProviderError::Unauthorized,
            JiraError::Http(err) => ProviderError::Http(err.to_string()),
            JiraError::Api { status, message } => ProviderError::Api { status, message },
            JiraError::RankNotFound { keys, anchor } => {
                ProviderError::RankNotFound { keys, anchor }
            }
            JiraError::RankPartialFailure { message } => {
                ProviderError::RankPartialFailure { message }
            }
            JiraError::LinkNotFound { blocker, blocked } => {
                ProviderError::LinkNotFound { blocker, blocked }
            }
            JiraError::LinkIdNotFound { link_id } => ProviderError::LinkIdNotFound { link_id },
        }
    }
}

impl From<GhError> for ProviderError {
    /// Converts a `gh` shell-out error into its provider-level counterpart at
    /// [`super::github_provider::GithubProvider`]'s boundary. Unlike
    /// [`JiraError`], [`GhError`] carries no structured "not found" or
    /// "unauthorized" classification -- it's always either a spawn/timeout
    /// failure or a `gh` subcommand's raw stderr -- so every variant maps to
    /// [`ProviderError::Api`] with a synthetic `status` of `0` (there is no
    /// real HTTP status for a CLI invocation) and the error's `Display` text
    /// as the message. Call sites that can say more (e.g. "this issue number
    /// doesn't exist") build a more specific [`ProviderError`] variant
    /// themselves instead of relying on this conversion.
    fn from(err: GhError) -> Self {
        ProviderError::Api {
            status: 0,
            message: err.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_not_found() {
        let err = ProviderError::from(JiraError::NotFound {
            key: "PROJ-1".to_string(),
        });
        assert!(matches!(err, ProviderError::NotFound { key } if key == "PROJ-1"));
    }

    #[test]
    fn converts_project_not_found() {
        let err = ProviderError::from(JiraError::ProjectNotFound {
            project: "PROJ".to_string(),
        });
        assert!(matches!(err, ProviderError::ProjectNotFound { project } if project == "PROJ"));
    }

    #[test]
    fn converts_unauthorized() {
        let err = ProviderError::from(JiraError::Unauthorized);
        assert!(matches!(err, ProviderError::Unauthorized));
    }

    #[test]
    fn converts_api_error() {
        let err = ProviderError::from(JiraError::Api {
            status: 500,
            message: "boom".to_string(),
        });
        assert!(matches!(
            err,
            ProviderError::Api { status, message } if status == 500 && message == "boom"
        ));
    }

    #[test]
    fn converts_rank_not_found() {
        let err = ProviderError::from(JiraError::RankNotFound {
            keys: "PROJ-1,PROJ-2".to_string(),
            anchor: "PROJ-3".to_string(),
        });
        assert!(matches!(
            err,
            ProviderError::RankNotFound { keys, anchor }
                if keys == "PROJ-1,PROJ-2" && anchor == "PROJ-3"
        ));
    }

    #[test]
    fn converts_rank_partial_failure() {
        let err = ProviderError::from(JiraError::RankPartialFailure {
            message: "partial".to_string(),
        });
        assert!(
            matches!(err, ProviderError::RankPartialFailure { message } if message == "partial")
        );
    }

    #[test]
    fn converts_link_not_found() {
        let err = ProviderError::from(JiraError::LinkNotFound {
            blocker: "PROJ-1".to_string(),
            blocked: "PROJ-2".to_string(),
        });
        assert!(matches!(
            err,
            ProviderError::LinkNotFound { blocker, blocked }
                if blocker == "PROJ-1" && blocked == "PROJ-2"
        ));
    }

    #[test]
    fn converts_link_id_not_found() {
        let err = ProviderError::from(JiraError::LinkIdNotFound {
            link_id: "10001".to_string(),
        });
        assert!(matches!(err, ProviderError::LinkIdNotFound { link_id } if link_id == "10001"));
    }

    #[test]
    fn converts_gh_error_to_api_with_synthetic_status() {
        let err = ProviderError::from(GhError::Command {
            command: "gh issue view".to_string(),
            exit_code: Some(1),
            stderr: "issue not found".to_string(),
        });
        match err {
            ProviderError::Api { status, message } => {
                assert_eq!(status, 0);
                assert!(message.contains("issue not found"));
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }
}
