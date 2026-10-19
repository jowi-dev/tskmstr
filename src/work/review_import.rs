//! Importing a pull request's GitHub review threads into vdiff's comment
//! store (GitHub issue #120), so a review left on github.com flows through
//! the same unchanged `tm review fix` path as `V` comments and
//! `tm review auto` findings. See `docs/plans/gh-120-import-github-review.md`.
//!
//! [`import`] does one pass:
//!
//! 1. [`plan_imports`] picks the threads to import: unresolved, with at
//!    least one comment that is not from a review bot, not an echo of a
//!    comment tm or vdiff published, and not already recorded in the dedup
//!    sidecar.
//! 2. Each picked thread becomes one comment-store entry, written through
//!    [`comment_store::append`]. Its text starts with `[github · @author]`,
//!    folds in the replies, and ends with the thread URL. An outdated thread
//!    (no line on the PR head) is anchored at its original line and says so.
//! 3. Every imported GitHub comment id is recorded in the sidecar,
//!    [`sidecar_path`], against the local `c<n>` id and the thread's node
//!    id, so a later poll never imports it again.
//!
//! **Echo detection.** `vdiff --publish-comments` posts a local comment's
//! `text` verbatim as the GitHub comment body, and its own
//! `published-comments.json` sidecar records only local ids, never the
//! GitHub ids. So an echo is recognised by its body instead: a comment whose
//! body equals the text of any comment in the store or in a round archive
//! (`vdiff/archive/*.json`), or that starts with a tm provenance prefix
//! (`[auto-review ·` or `[github ·`), is never imported.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::github::bot_findings::bot_login_matches;
use crate::github::review_threads::{FullReviewThread, ThreadComment};
use crate::work::comment_store::{self, CommentStoreError, NewComment};

/// Text prefix of every imported comment, followed by the author's login.
pub const IMPORT_PREFIX: &str = "[github · @";

/// Event kind `tm review import` records on the ticket's lane run. Its
/// detail is `<n> imported`, `fix`, `fix-failed` or `capped`, and the
/// overview shows the newest one on the ticket's row.
pub const IMPORT_EVENT: &str = "review_import";

/// Text prefix `tm review auto` gives its findings (see
/// [`crate::work::auto_review::Finding::to_comment`]).
const AUTO_REVIEW_PREFIX: &str = "[auto-review ·";

/// `<git_dir>/vdiff/imported-threads.json`, the dedup sidecar.
pub fn sidecar_path(git_dir: &Path) -> PathBuf {
    git_dir.join("vdiff").join("imported-threads.json")
}

/// What the sidecar records for one imported GitHub comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedEntry {
    /// The local `c<n>` id the comment's thread was imported as.
    pub local_id: String,
    /// The thread's GraphQL node id, for replying and resolving.
    pub thread_id: String,
    /// Whether tm has resolved the thread on GitHub (`resolve_imported`).
    #[serde(default)]
    pub resolved: bool,
}

/// The dedup sidecar: GitHub comment id (as a string, since JSON object keys
/// are strings) to what it was imported as.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedThreads {
    /// Every imported GitHub comment.
    pub comments: BTreeMap<String, ImportedEntry>,
}

impl ImportedThreads {
    /// Has GitHub comment `id` been imported?
    pub fn contains(&self, id: u64) -> bool {
        self.comments.contains_key(&id.to_string())
    }

    /// Thread node ids imported but not yet resolved by tm, sorted and
    /// deduplicated.
    pub fn unresolved_threads(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .comments
            .values()
            .filter(|e| !e.resolved)
            .map(|e| e.thread_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Marks every comment of `thread_id` resolved.
    pub fn mark_resolved(&mut self, thread_id: &str) {
        for entry in self.comments.values_mut() {
            if entry.thread_id == thread_id {
                entry.resolved = true;
            }
        }
    }
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> CommentStoreError + '_ {
    move |source| CommentStoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Reads the sidecar. An absent sidecar is empty.
///
/// # Errors
///
/// [`CommentStoreError::Io`] if it can't be read;
/// [`CommentStoreError::Malformed`] if it isn't the sidecar's shape. A
/// malformed sidecar is an error rather than empty, since treating it as
/// empty would re-import every thread.
pub fn load_sidecar(git_dir: &Path) -> Result<ImportedThreads, CommentStoreError> {
    let path = sidecar_path(git_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| CommentStoreError::Malformed {
            path: path.clone(),
            message: e.to_string(),
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ImportedThreads::default()),
        Err(err) => Err(io_err(&path)(err)),
    }
}

/// Writes the sidecar pretty-printed, creating `<git_dir>/vdiff/` if needed.
///
/// # Errors
///
/// [`CommentStoreError::Io`] if the write fails.
pub fn save_sidecar(git_dir: &Path, sidecar: &ImportedThreads) -> Result<(), CommentStoreError> {
    let path = sidecar_path(git_dir);
    let dir = path.parent().expect("sidecar path has a parent");
    std::fs::create_dir_all(dir).map_err(io_err(dir))?;
    let body = serde_json::to_string_pretty(sidecar).expect("the sidecar always serializes");
    std::fs::write(&path, format!("{body}\n")).map_err(io_err(&path))
}

/// The trimmed text of every comment in the store and in every round
/// archive under `<git_dir>/vdiff/archive/`: the bodies an echo of a
/// published local comment would have. Unreadable archives are skipped.
///
/// # Errors
///
/// See [`comment_store::load`].
pub fn local_texts(git_dir: &Path) -> Result<HashSet<String>, CommentStoreError> {
    let mut texts: HashSet<String> = comment_store::load(git_dir)?
        .iter()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .map(|t| t.trim().to_string())
        .collect();
    let archive_dir = git_dir.join("vdiff").join("archive");
    if let Ok(entries) = std::fs::read_dir(&archive_dir) {
        for entry in entries.flatten() {
            let Ok(raw) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            texts.extend(
                items
                    .iter()
                    .filter_map(|c| c.get("text").and_then(Value::as_str))
                    .map(|t| t.trim().to_string()),
            );
        }
    }
    Ok(texts)
}

/// Whether the store holds any imported GitHub comment, which is what makes
/// a `tm review import --fix` start a fix pass.
///
/// # Errors
///
/// See [`comment_store::load`].
pub fn store_has_imports(git_dir: &Path) -> Result<bool, CommentStoreError> {
    Ok(comment_store::load(git_dir)?.iter().any(|c| {
        c.get("text")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with(IMPORT_PREFIX))
    }))
}

/// Is `body` a comment tm or vdiff published, rather than a human's?
fn is_echo(body: &str, local_texts: &HashSet<String>) -> bool {
    let body = body.trim();
    body.starts_with(AUTO_REVIEW_PREFIX)
        || body.starts_with(IMPORT_PREFIX)
        || local_texts.contains(body)
}

fn is_bot(comment: &ThreadComment, bots: &[String]) -> bool {
    comment
        .author_login
        .as_deref()
        .is_some_and(|author| bots.iter().any(|bot| bot_login_matches(bot, author)))
}

/// One thread [`plan_imports`] picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedImport {
    /// The thread's node id.
    pub thread_id: String,
    /// The GitHub ids of the comments folded into `comment`.
    pub comment_ids: Vec<u64>,
    /// The comment-store entry to append.
    pub comment: NewComment,
}

/// Picks the threads in `threads` to import and builds their comment-store
/// entries. See the module doc for the rules; `bots` is the configured
/// `review_bots`, whose threads `tm pr watch` handles instead.
///
/// A thread with a new reply after an earlier import is imported again as a
/// fresh entry with every human comment folded in: the earlier entry was
/// archived by the fix pass it fed, so the new one is the only copy the next
/// fix pass sees.
pub fn plan_imports(
    threads: &[FullReviewThread],
    bots: &[String],
    sidecar: &ImportedThreads,
    local_texts: &HashSet<String>,
) -> Vec<PlannedImport> {
    threads
        .iter()
        .filter(|t| !t.is_resolved)
        .filter_map(|thread| {
            let human: Vec<&ThreadComment> = thread
                .comments
                .iter()
                .filter(|c| !is_bot(c, bots) && !is_echo(&c.body, local_texts))
                .collect();
            if human.iter().all(|c| sidecar.contains(c.id)) {
                return None;
            }
            Some(PlannedImport {
                thread_id: thread.id.clone(),
                comment_ids: human.iter().map(|c| c.id).collect(),
                comment: to_comment(thread, &human),
            })
        })
        .collect()
}

fn author(comment: &ThreadComment) -> &str {
    comment.author_login.as_deref().unwrap_or("ghost")
}

/// Builds the entry for `thread` from its non-empty `human` comments.
fn to_comment(thread: &FullReviewThread, human: &[&ThreadComment]) -> NewComment {
    let first = human[0];
    let (start_line, end_line, outdated_note) = match thread.line {
        Some(line) => (thread.start_line.unwrap_or(line), line, None),
        None => {
            let line = thread.original_line.unwrap_or(1);
            let start = thread.original_start_line.unwrap_or(line);
            let sha = first
                .original_commit
                .as_deref()
                .map(|sha| &sha[..sha.len().min(7)])
                .unwrap_or("unknown");
            (
                start,
                line,
                Some(format!("(outdated: was L{line} @ {sha})")),
            )
        }
    };
    let mut text = format!("{IMPORT_PREFIX}{}] {}", author(first), first.body.trim());
    for reply in &human[1..] {
        text.push_str(&format!("\n\n↳ @{}: {}", author(reply), reply.body.trim()));
    }
    if let Some(note) = outdated_note {
        text.push_str(&format!("\n\n{note}"));
    }
    text.push_str(&format!("\n\n{}", first.url));
    NewComment {
        path: thread.path.clone(),
        start_line,
        end_line: end_line.max(start_line),
        text,
    }
}

/// What one [`import`] pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// `(thread node id, local c<n> id)` of every thread imported, in
    /// import order.
    pub imported: Vec<(String, String)>,
}

/// Imports `threads` into the store under `git_dir` and records them in the
/// sidecar. See the module doc. `created_at` stamps the new comments.
///
/// # Errors
///
/// Any store or sidecar read or write failure. The store is written before
/// the sidecar, so a failed sidecar write can at worst import a thread
/// twice, never lose one.
pub fn import(
    git_dir: &Path,
    threads: &[FullReviewThread],
    bots: &[String],
    created_at: &str,
) -> Result<ImportReport, CommentStoreError> {
    let mut sidecar = load_sidecar(git_dir)?;
    let texts = local_texts(git_dir)?;
    let planned = plan_imports(threads, bots, &sidecar, &texts);
    if planned.is_empty() {
        return Ok(ImportReport::default());
    }
    let comments: Vec<NewComment> = planned.iter().map(|p| p.comment.clone()).collect();
    let ids = comment_store::append(git_dir, &comments, created_at)?;
    let mut report = ImportReport::default();
    for (plan, local_id) in planned.iter().zip(ids) {
        for comment_id in &plan.comment_ids {
            sidecar.comments.insert(
                comment_id.to_string(),
                ImportedEntry {
                    local_id: local_id.clone(),
                    thread_id: plan.thread_id.clone(),
                    resolved: false,
                },
            );
        }
        report.imported.push((plan.thread_id.clone(), local_id));
    }
    save_sidecar(git_dir, &sidecar)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::review_threads::parse_review_threads_full;
    use tempfile::TempDir;

    const RECORDED: &str = include_str!("../github/fixtures/review_threads_import.json");

    fn recorded() -> Vec<FullReviewThread> {
        parse_review_threads_full(RECORDED, 140).unwrap().threads
    }

    fn bots() -> Vec<String> {
        vec!["cursor[bot]".to_string()]
    }

    fn comment(id: u64, author: &str, body: &str) -> ThreadComment {
        ThreadComment {
            id,
            author_login: Some(author.to_string()),
            body: body.to_string(),
            url: format!("https://github.com/o/r/pull/1#discussion_r{id}"),
            created_at: "2026-10-18T10:00:00Z".to_string(),
            original_commit: Some("abcdef0123".to_string()),
        }
    }

    fn thread(id: &str, comments: Vec<ThreadComment>) -> FullReviewThread {
        FullReviewThread {
            id: id.to_string(),
            is_resolved: false,
            is_outdated: false,
            path: "src/a.rs".to_string(),
            line: Some(5),
            start_line: None,
            original_line: Some(5),
            original_start_line: None,
            comments,
        }
    }

    #[test]
    fn plans_unresolved_threads_from_the_recorded_response() {
        let planned = plan_imports(
            &recorded(),
            &bots(),
            &ImportedThreads::default(),
            &HashSet::new(),
        );

        let ids: Vec<&str> = planned.iter().map(|p| p.thread_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "PRRT_kwDOTpcxIM5abc001",
                "PRRT_kwDOTpcxIM5abc002",
                "PRRT_kwDOTpcxIM5abc004"
            ],
            "the resolved thread is skipped"
        );
    }

    #[test]
    fn maps_a_multi_line_thread_with_replies_folded_and_the_url() {
        let planned = plan_imports(
            &recorded(),
            &bots(),
            &ImportedThreads::default(),
            &HashSet::new(),
        );

        let first = &planned[0];
        assert_eq!(first.comment_ids, [2_401_001, 2_401_002]);
        assert_eq!(first.comment.path, "src/work/merge.rs");
        assert_eq!((first.comment.start_line, first.comment.end_line), (40, 42));
        assert_eq!(
            first.comment.text,
            "[github · @teammate] This swallows the rebase error.\n\n\
             ↳ @jowi-dev: Good catch, it should bubble up.\n\n\
             https://github.com/jowi-dev/tskmstr/pull/140#discussion_r2401001"
        );
    }

    #[test]
    fn anchors_an_outdated_thread_at_its_original_line_with_a_note() {
        let planned = plan_imports(
            &recorded(),
            &bots(),
            &ImportedThreads::default(),
            &HashSet::new(),
        );

        let outdated = &planned[1];
        assert_eq!(
            (outdated.comment.start_line, outdated.comment.end_line),
            (118, 118)
        );
        assert!(
            outdated
                .comment
                .text
                .contains("(outdated: was L118 @ 2222222)"),
            "{}",
            outdated.comment.text
        );
    }

    #[test]
    fn a_deleted_author_reads_as_ghost() {
        let planned = plan_imports(
            &recorded(),
            &bots(),
            &ImportedThreads::default(),
            &HashSet::new(),
        );

        assert!(planned[2].comment.text.starts_with("[github · @ghost] "));
    }

    #[test]
    fn bot_threads_are_never_imported_but_human_replies_to_them_are() {
        let threads = vec![
            thread("T1", vec![comment(1, "cursor", "bot finding")]),
            thread(
                "T2",
                vec![
                    comment(2, "cursor", "bot finding"),
                    comment(3, "teammate", "agree, fix it"),
                ],
            ),
        ];

        let planned = plan_imports(
            &threads,
            &bots(),
            &ImportedThreads::default(),
            &HashSet::new(),
        );

        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].thread_id, "T2");
        assert_eq!(planned[0].comment_ids, [3]);
        assert!(!planned[0].comment.text.contains("bot finding"));
    }

    #[test]
    fn echoes_of_published_comments_are_never_imported() {
        let local: HashSet<String> = ["Rename this.".to_string()].into();
        let threads = vec![
            thread("T1", vec![comment(1, "jowi-dev", "Rename this.\n")]),
            thread(
                "T2",
                vec![comment(
                    2,
                    "jowi-dev",
                    "[auto-review · state-flow · MEDIUM] x",
                )],
            ),
            thread("T3", vec![comment(3, "jowi-dev", "[github · @teammate] y")]),
        ];

        let planned = plan_imports(&threads, &bots(), &ImportedThreads::default(), &local);

        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn import_writes_once_and_a_repoll_imports_nothing() {
        let tmp = TempDir::new().unwrap();

        let first = import(tmp.path(), &recorded(), &bots(), "2026-10-18T12:00:00Z").unwrap();
        let again = import(tmp.path(), &recorded(), &bots(), "2026-10-18T12:01:00Z").unwrap();

        assert_eq!(first.imported.len(), 3);
        assert!(again.imported.is_empty());
        let stored = comment_store::load(tmp.path()).unwrap();
        assert_eq!(stored.len(), 3);
        assert!(
            stored
                .iter()
                .all(|c| c["text"].as_str().unwrap().starts_with(IMPORT_PREFIX))
        );
        let sidecar = load_sidecar(tmp.path()).unwrap();
        assert_eq!(
            sidecar.comments.len(),
            4,
            "every folded comment id is recorded"
        );
        assert_eq!(
            sidecar.comments["2401002"].thread_id,
            "PRRT_kwDOTpcxIM5abc001"
        );
    }

    #[test]
    fn import_skips_comments_already_in_an_archive_as_echoes() {
        let tmp = TempDir::new().unwrap();
        comment_store::append(
            tmp.path(),
            &[NewComment {
                path: "src/a.rs".to_string(),
                start_line: 5,
                end_line: 5,
                text: "Human V comment".to_string(),
            }],
            "2026-10-18T12:00:00Z",
        )
        .unwrap();
        comment_store::archive(tmp.path(), 1).unwrap();
        let threads = vec![thread(
            "T1",
            vec![comment(1, "jowi-dev", "Human V comment")],
        )];

        let report = import(tmp.path(), &threads, &bots(), "2026-10-18T12:00:00Z").unwrap();

        assert!(report.imported.is_empty());
    }

    #[test]
    fn a_new_reply_reimports_the_thread_with_every_human_comment() {
        let tmp = TempDir::new().unwrap();
        let mut threads = vec![thread("T1", vec![comment(1, "teammate", "first")])];
        import(tmp.path(), &threads, &bots(), "2026-10-18T12:00:00Z").unwrap();
        comment_store::archive(tmp.path(), 1).unwrap();
        threads[0]
            .comments
            .push(comment(2, "teammate", "still broken"));

        let report = import(tmp.path(), &threads, &bots(), "2026-10-18T12:05:00Z").unwrap();

        assert_eq!(report.imported.len(), 1);
        let stored = comment_store::load(tmp.path()).unwrap();
        let text = stored[0]["text"].as_str().unwrap();
        assert!(
            text.contains("first") && text.contains("still broken"),
            "{text}"
        );
    }

    #[test]
    fn unresolved_threads_lists_each_thread_once_until_marked() {
        let tmp = TempDir::new().unwrap();
        import(tmp.path(), &recorded(), &bots(), "2026-10-18T12:00:00Z").unwrap();
        let mut sidecar = load_sidecar(tmp.path()).unwrap();

        assert_eq!(sidecar.unresolved_threads().len(), 3);
        sidecar.mark_resolved("PRRT_kwDOTpcxIM5abc001");
        assert_eq!(sidecar.unresolved_threads().len(), 2);
    }

    #[test]
    fn a_malformed_sidecar_is_an_error_not_a_reimport() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("vdiff")).unwrap();
        std::fs::write(sidecar_path(tmp.path()), "[1, 2]").unwrap();

        assert!(matches!(
            import(tmp.path(), &recorded(), &bots(), "2026-10-18T12:00:00Z"),
            Err(CommentStoreError::Malformed { .. })
        ));
    }
}
