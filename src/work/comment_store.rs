//! Writer for vdiff's review-comment store, `<git-dir>/vdiff/comments.json`
//! (vdiff `docs/comments-schema.md`, schema v1), so `tm review auto`
//! (GitHub issue #124) can leave its findings where `tm review fix` and
//! vdiff's `V` already read comments from.
//!
//! The schema's writer rules, all followed here:
//!
//! - ids are `c<n>`, one past the highest existing `n`;
//! - `created_at` is ISO-8601 UTC, set once;
//! - the array is re-sorted by `(path, start_line)` and pretty-printed on
//!   every save.
//!
//! Existing comments are carried as raw JSON objects rather than a typed
//! struct, so a field this module doesn't know about (a human comment's
//! `node`, or anything a newer vdiff.nvim adds) round-trips untouched.
//!
//! Callers resolve `git_dir` themselves (`git rev-parse --git-dir`, see
//! [`crate::work::git::GitOps::git_dir`]): joining `.git` onto a worktree
//! root breaks in a linked worktree, where `.git` is a file.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use thiserror::Error;

/// Errors reading or writing the comment store.
#[derive(Debug, Error)]
pub enum CommentStoreError {
    /// A filesystem operation failed.
    #[error("comment store io error at {path}: {source}")]
    Io {
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The store exists but isn't a JSON array of objects.
    #[error("comment store {path} is not a schema-v1 array: {message}")]
    Malformed {
        /// The store file.
        path: PathBuf,
        /// What was wrong with it.
        message: String,
    },
}

/// One comment to add. The store assigns its `id` and `created_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewComment {
    /// Repo-relative path the comment is anchored to.
    pub path: String,
    /// 1-based, inclusive first line.
    pub start_line: u64,
    /// 1-based, inclusive last line.
    pub end_line: u64,
    /// The comment body.
    pub text: String,
}

/// `<git_dir>/vdiff/comments.json`.
pub fn store_path(git_dir: &Path) -> PathBuf {
    git_dir.join("vdiff").join("comments.json")
}

/// `<git_dir>/vdiff/archive/<round>.json`, where [`archive`] moves a
/// round's consumed comments.
pub fn archive_path(git_dir: &Path, round: u32) -> PathBuf {
    git_dir
        .join("vdiff")
        .join("archive")
        .join(format!("{round}.json"))
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> CommentStoreError + '_ {
    move |source| CommentStoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Every comment in the store, as raw objects. An absent store is empty,
/// matching `vdiff --export-comments`.
///
/// # Errors
///
/// [`CommentStoreError::Io`] if the file can't be read;
/// [`CommentStoreError::Malformed`] if it isn't a JSON array of objects.
pub fn load(git_dir: &Path) -> Result<Vec<Map<String, Value>>, CommentStoreError> {
    let path = store_path(git_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_err(&path)(err)),
    };
    let malformed = |message: String| CommentStoreError::Malformed {
        path: path.clone(),
        message,
    };
    let value: Value = serde_json::from_str(&raw).map_err(|e| malformed(e.to_string()))?;
    let Value::Array(items) = value else {
        return Err(malformed("top level is not an array".to_string()));
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::Object(map) => Ok(map),
            other => Err(malformed(format!("entry is not an object: {other}"))),
        })
        .collect()
}

/// The `n` of a `c<n>` id, `None` for any other shape.
fn id_number(comment: &Map<String, Value>) -> Option<u64> {
    comment.get("id")?.as_str()?.strip_prefix('c')?.parse().ok()
}

/// Sorts by `(path, start_line)`, then writes the store pretty-printed,
/// creating `<git_dir>/vdiff/` if needed.
fn save(git_dir: &Path, mut comments: Vec<Map<String, Value>>) -> Result<(), CommentStoreError> {
    comments.sort_by(|a, b| {
        let key = |c: &Map<String, Value>| {
            (
                c.get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                c.get("start_line").and_then(Value::as_u64).unwrap_or(0),
            )
        };
        key(a).cmp(&key(b))
    });
    let path = store_path(git_dir);
    let dir = path.parent().expect("store path has a parent");
    std::fs::create_dir_all(dir).map_err(io_err(dir))?;
    let body = serde_json::to_string_pretty(&Value::Array(
        comments.into_iter().map(Value::Object).collect(),
    ))
    .expect("JSON values always serialize");
    std::fs::write(&path, format!("{body}\n")).map_err(io_err(&path))
}

/// Appends `new` to the store, leaving every existing comment as it was,
/// and returns the ids assigned, in `new`'s order.
///
/// `created_at` is stamped on every new comment; see [`iso8601_utc`].
///
/// # Errors
///
/// See [`load`]; also [`CommentStoreError::Io`] if the write fails.
pub fn append(
    git_dir: &Path,
    new: &[NewComment],
    created_at: &str,
) -> Result<Vec<String>, CommentStoreError> {
    let mut comments = load(git_dir)?;
    let first = comments.iter().filter_map(id_number).max().unwrap_or(0) + 1;
    let mut ids = Vec::with_capacity(new.len());
    for (n, comment) in (first..).zip(new) {
        let id = format!("c{n}");
        let Value::Object(map) = json!({
            "id": id,
            "path": comment.path,
            "start_line": comment.start_line,
            "end_line": comment.end_line,
            "text": comment.text,
            "created_at": created_at,
        }) else {
            unreachable!("json! of an object literal is an object");
        };
        comments.push(map);
        ids.push(id);
    }
    save(git_dir, comments)?;
    Ok(ids)
}

/// Moves the store's comments to [`archive_path`] for `round` and leaves an
/// empty store behind, so the next round's fix pass never re-reads comments
/// whose line anchors the last fix commit made stale. Returns the archive
/// path, or `None` when the store had nothing to archive.
///
/// # Errors
///
/// See [`load`]; also [`CommentStoreError::Io`] if a write fails.
pub fn archive(git_dir: &Path, round: u32) -> Result<Option<PathBuf>, CommentStoreError> {
    archive_to(git_dir, archive_path(git_dir, round))
}

/// `<git_dir>/vdiff/archive/<name>.json`, where [`archive_named`] moves the
/// store.
pub fn named_archive_path(git_dir: &Path, name: &str) -> PathBuf {
    git_dir
        .join("vdiff")
        .join("archive")
        .join(format!("{name}.json"))
}

/// Same as [`archive`], to [`named_archive_path`] for `name`, so a writer
/// other than `tm review auto` (whose round numbers restart with every
/// loop) never overwrites one of its archives.
///
/// # Errors
///
/// See [`archive`].
pub fn archive_named(git_dir: &Path, name: &str) -> Result<Option<PathBuf>, CommentStoreError> {
    archive_to(git_dir, named_archive_path(git_dir, name))
}

fn archive_to(git_dir: &Path, path: PathBuf) -> Result<Option<PathBuf>, CommentStoreError> {
    let comments = load(git_dir)?;
    if comments.is_empty() {
        return Ok(None);
    }
    let dir = path.parent().expect("archive path has a parent");
    std::fs::create_dir_all(dir).map_err(io_err(dir))?;
    let body = serde_json::to_string_pretty(&Value::Array(
        comments.into_iter().map(Value::Object).collect(),
    ))
    .expect("JSON values always serialize");
    std::fs::write(&path, format!("{body}\n")).map_err(io_err(&path))?;
    save(git_dir, Vec::new())?;
    Ok(Some(path))
}

/// `secs` since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`, the schema's
/// `created_at` format.
pub fn iso8601_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's days-to-civil algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn comment(path: &str, start: u64, text: &str) -> NewComment {
        NewComment {
            path: path.to_string(),
            start_line: start,
            end_line: start,
            text: text.to_string(),
        }
    }

    fn write_store(git_dir: &Path, body: &str) {
        std::fs::create_dir_all(git_dir.join("vdiff")).unwrap();
        std::fs::write(store_path(git_dir), body).unwrap();
    }

    #[test]
    fn load_treats_an_absent_store_as_empty() {
        let tmp = TempDir::new().unwrap();
        assert!(load(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn load_rejects_a_non_array_store() {
        let tmp = TempDir::new().unwrap();
        write_store(tmp.path(), r#"{"id": "c1"}"#);
        assert!(matches!(
            load(tmp.path()),
            Err(CommentStoreError::Malformed { .. })
        ));
    }

    #[test]
    fn append_to_an_absent_store_creates_it_with_c1_onward() {
        let tmp = TempDir::new().unwrap();

        let ids = append(
            tmp.path(),
            &[comment("src/a.rs", 3, "one"), comment("src/a.rs", 9, "two")],
            "2026-10-18T12:00:00Z",
        )
        .unwrap();

        assert_eq!(ids, vec!["c1", "c2"]);
        let stored = load(tmp.path()).unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0]["id"], "c1");
        assert_eq!(stored[0]["path"], "src/a.rs");
        assert_eq!(stored[0]["start_line"], 3);
        assert_eq!(stored[0]["end_line"], 3);
        assert_eq!(stored[0]["text"], "one");
        assert_eq!(stored[0]["created_at"], "2026-10-18T12:00:00Z");
        assert!(!stored[0].contains_key("node"), "node is omitted, not null");
    }

    #[test]
    fn append_preserves_existing_comments_and_allocates_after_the_highest_id() {
        let tmp = TempDir::new().unwrap();
        write_store(
            tmp.path(),
            r#"[
  {"id": "c7", "path": "src/z.rs", "start_line": 1, "end_line": 2,
   "text": "human", "node": "rust:z", "created_at": "2026-01-01T00:00:00Z",
   "future_field": true},
  {"id": "c2", "path": "src/b.rs", "start_line": 5, "end_line": 5,
   "text": "older", "created_at": "2026-01-01T00:00:00Z"}
]"#,
        );

        let ids = append(
            tmp.path(),
            &[comment("src/m.rs", 4, "agent")],
            "2026-10-18T12:00:00Z",
        )
        .unwrap();

        assert_eq!(ids, vec!["c8"]);
        let stored = load(tmp.path()).unwrap();
        let paths: Vec<&str> = stored.iter().map(|c| c["path"].as_str().unwrap()).collect();
        assert_eq!(paths, vec!["src/b.rs", "src/m.rs", "src/z.rs"]);
        let human = &stored[2];
        assert_eq!(human["node"], "rust:z");
        assert_eq!(human["future_field"], true);
        assert_eq!(human["created_at"], "2026-01-01T00:00:00Z");
    }

    #[test]
    fn append_sorts_by_path_then_start_line_and_pretty_prints() {
        let tmp = TempDir::new().unwrap();

        append(
            tmp.path(),
            &[
                comment("src/a.rs", 20, "late"),
                comment("src/a.rs", 3, "early"),
            ],
            "2026-10-18T12:00:00Z",
        )
        .unwrap();

        let raw = std::fs::read_to_string(store_path(tmp.path())).unwrap();
        assert!(raw.starts_with("[\n  {"), "pretty-printed: {raw}");
        assert!(raw.ends_with("]\n"));
        let stored = load(tmp.path()).unwrap();
        assert_eq!(stored[0]["text"], "early");
        assert_eq!(stored[0]["id"], "c2");
        assert_eq!(stored[1]["text"], "late");
    }

    #[test]
    fn archive_moves_the_store_and_leaves_it_empty() {
        let tmp = TempDir::new().unwrap();
        append(
            tmp.path(),
            &[comment("src/a.rs", 3, "one")],
            "2026-10-18T12:00:00Z",
        )
        .unwrap();

        let archived = archive(tmp.path(), 1).unwrap();

        assert_eq!(archived, Some(archive_path(tmp.path(), 1)));
        assert!(load(tmp.path()).unwrap().is_empty());
        let raw = std::fs::read_to_string(archive_path(tmp.path(), 1)).unwrap();
        let archived: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(archived[0]["text"], "one");
    }

    #[test]
    fn archive_of_an_empty_store_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(archive(tmp.path(), 1).unwrap(), None);
        assert!(!archive_path(tmp.path(), 1).exists());
    }

    #[test]
    fn iso8601_utc_formats_epoch_seconds() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_787_054_400), "2026-08-18T12:00:00Z");
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00Z");
    }
}
