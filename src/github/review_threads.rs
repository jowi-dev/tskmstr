//! Full pull request review threads, for importing a PR's GitHub review
//! into the vdiff comment store (GitHub issue #120).
//!
//! [`super::gh_cli::GhCli::pr_review_threads`] and
//! [`super::gh_cli::GhCli::pr_bot_finding_details`] only read each thread's
//! first comment. An import needs every comment in a thread (replies are
//! folded into one stored comment), each comment's id (for the dedup
//! sidecar), and the thread's anchor on both the PR head and the commit it
//! was left on (for outdated threads). So this is its own query and its own
//! types, like the bot-finding query is.

use serde::Deserialize;

use super::gh_cli::GhError;

/// GraphQL query for [`super::gh_cli::GhCli::pr_review_threads_full`].
///
/// Fetches the first 100 threads and the first 50 comments of each; neither
/// page is followed, the same single-page limit as
/// [`super::gh_cli::GhCli::pr_review_threads`].
pub const REVIEW_THREADS_FULL_QUERY: &str =
    "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      author { login }
      reviewThreads(first: 100) {
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          startLine
          originalLine
          originalStartLine
          comments(first: 50) {
            nodes {
              databaseId
              author { login }
              body
              url
              createdAt
              originalCommit { oid }
            }
          }
        }
      }
    }
  }
}";

/// GraphQL mutation replying to a review thread.
pub const ADD_THREAD_REPLY_MUTATION: &str = "mutation($threadId: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: {pullRequestReviewThreadId: $threadId, body: $body}) {
    comment { id }
  }
}";

/// GraphQL mutation resolving a review thread.
pub const RESOLVE_THREAD_MUTATION: &str = "mutation($threadId: ID!) {
  resolveReviewThread(input: {threadId: $threadId}) {
    thread { id }
  }
}";

/// One comment in a review thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadComment {
    /// GitHub's numeric comment id (`databaseId`), the dedup key.
    pub id: u64,
    /// Author login, `None` for a deleted account. GraphQL reports bot
    /// logins without the `[bot]` suffix.
    pub author_login: Option<String>,
    /// Markdown body.
    pub body: String,
    /// Web URL of the comment.
    pub url: String,
    /// ISO-8601 creation time.
    pub created_at: String,
    /// The commit the comment was left on, when GitHub still knows it.
    pub original_commit: Option<String>,
}

/// One review thread on a pull request, with every comment in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullReviewThread {
    /// GraphQL node id, which the reply and resolve mutations take.
    pub id: String,
    /// Whether the thread is resolved.
    pub is_resolved: bool,
    /// Whether the thread's lines have changed since it was left.
    pub is_outdated: bool,
    /// Repo-relative path.
    pub path: String,
    /// Last line on the PR head; `None` when the code moved (outdated).
    pub line: Option<u64>,
    /// First line on the PR head for a multi-line thread.
    pub start_line: Option<u64>,
    /// Last line on the commit the thread was left on.
    pub original_line: Option<u64>,
    /// First line on that commit for a multi-line thread.
    pub original_start_line: Option<u64>,
    /// Every comment, oldest first.
    pub comments: Vec<ThreadComment>,
}

/// A pull request's review threads and its author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrReviewThreads {
    /// The PR author's login, `None` for a deleted account.
    pub pr_author: Option<String>,
    /// Every review thread (first page).
    pub threads: Vec<FullReviewThread>,
}

#[derive(Debug, Deserialize)]
struct RawResponse {
    data: Option<RawData>,
}

#[derive(Debug, Deserialize)]
struct RawData {
    repository: Option<RawRepository>,
}

#[derive(Debug, Deserialize)]
struct RawRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<RawPullRequest>,
}

#[derive(Debug, Deserialize)]
struct RawPullRequest {
    author: Option<RawAuthor>,
    #[serde(rename = "reviewThreads")]
    review_threads: RawNodes<RawThread>,
}

#[derive(Debug, Deserialize)]
struct RawNodes<T> {
    nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    id: String,
    is_resolved: bool,
    #[serde(default)]
    is_outdated: bool,
    path: String,
    line: Option<u64>,
    start_line: Option<u64>,
    original_line: Option<u64>,
    original_start_line: Option<u64>,
    comments: RawNodes<RawComment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComment {
    database_id: u64,
    author: Option<RawAuthor>,
    body: String,
    url: String,
    created_at: String,
    original_commit: Option<RawCommit>,
}

#[derive(Debug, Deserialize)]
struct RawAuthor {
    login: String,
}

#[derive(Debug, Deserialize)]
struct RawCommit {
    oid: String,
}

/// Parses `gh api graphql`'s stdout for [`REVIEW_THREADS_FULL_QUERY`] on
/// pull request `number`.
///
/// # Errors
///
/// [`GhError::Parse`] if the body isn't the expected shape, or if
/// `data.repository.pullRequest` is null (no such pull request).
pub fn parse_review_threads_full(stdout: &str, number: u64) -> Result<PrReviewThreads, GhError> {
    let parse_err = |message: String| GhError::Parse {
        command: "gh api graphql".to_string(),
        message,
    };
    let response: RawResponse =
        serde_json::from_str(stdout).map_err(|err| parse_err(err.to_string()))?;
    let pr = response
        .data
        .and_then(|data| data.repository)
        .and_then(|repo| repo.pull_request)
        .ok_or_else(|| parse_err(format!("pull request #{number} not found")))?;
    Ok(PrReviewThreads {
        pr_author: pr.author.map(|a| a.login),
        threads: pr
            .review_threads
            .nodes
            .into_iter()
            .map(|t| FullReviewThread {
                id: t.id,
                is_resolved: t.is_resolved,
                is_outdated: t.is_outdated,
                path: t.path,
                line: t.line,
                start_line: t.start_line,
                original_line: t.original_line,
                original_start_line: t.original_start_line,
                comments: t
                    .comments
                    .nodes
                    .into_iter()
                    .map(|c| ThreadComment {
                        id: c.database_id,
                        author_login: c.author.map(|a| a.login),
                        body: c.body,
                        url: c.url,
                        created_at: c.created_at,
                        original_commit: c.original_commit.map(|commit| commit.oid),
                    })
                    .collect(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORDED: &str = include_str!("fixtures/review_threads_import.json");

    #[test]
    fn parses_a_recorded_response_with_every_comment_and_anchor() {
        let parsed = parse_review_threads_full(RECORDED, 140).unwrap();

        assert_eq!(parsed.pr_author.as_deref(), Some("jowi-dev"));
        assert_eq!(parsed.threads.len(), 4);
        let first = &parsed.threads[0];
        assert_eq!(first.id, "PRRT_kwDOTpcxIM5abc001");
        assert!(!first.is_resolved);
        assert!(!first.is_outdated);
        assert_eq!(first.path, "src/work/merge.rs");
        assert_eq!((first.start_line, first.line), (Some(40), Some(42)));
        assert_eq!(first.comments.len(), 2);
        assert_eq!(first.comments[0].id, 2_401_001);
        assert_eq!(first.comments[0].author_login.as_deref(), Some("teammate"));
        assert_eq!(first.comments[1].body, "Good catch, it should bubble up.");
        assert_eq!(
            first.comments[0].original_commit.as_deref(),
            Some("1111111aaaaaaa")
        );
    }

    #[test]
    fn keeps_an_outdated_threads_original_anchor() {
        let parsed = parse_review_threads_full(RECORDED, 140).unwrap();

        let outdated = &parsed.threads[1];
        assert!(outdated.is_outdated);
        assert_eq!(outdated.line, None);
        assert_eq!(outdated.original_line, Some(118));
    }

    #[test]
    fn a_deleted_author_and_unknown_commit_parse_as_none() {
        let parsed = parse_review_threads_full(RECORDED, 140).unwrap();

        let comment = &parsed.threads[3].comments[0];
        assert_eq!(comment.author_login, None);
        assert_eq!(comment.original_commit, None);
    }

    #[test]
    fn a_missing_pull_request_is_a_parse_error_naming_it() {
        let err = parse_review_threads_full(r#"{"data":{"repository":{"pullRequest":null}}}"#, 9)
            .unwrap_err();
        assert!(err.to_string().contains("#9"), "{err}");
    }
}
