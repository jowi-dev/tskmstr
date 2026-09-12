//! Reads a configured `prompt_file` into the opening prompt text handed to a
//! launched interactive session — the shared half of GitHub issue #42's
//! "config selectable prompt" feature. [`crate::config::AuditConfig`],
//! [`crate::config::CreateConfig`], and [`crate::config::ReviewWatchConfig`]
//! each carry a `prompt_file: Option<String>` (already merge-time resolved
//! against the defining repo when relative — see those fields' doc
//! comments), so the launchers ([`crate::work::audit::launch_audit`],
//! [`crate::work::create::launch_create`], and
//! [`crate::work::bugbot::launch_cleanup`]) only need one shared read: expand
//! a leading `~`, then read the file, turning a missing/unreadable path into
//! an actionable error rather than an empty prompt.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::work::naming::expand_tilde;

/// A configured `prompt_file` could not be read. Surfaced by each launcher's
/// `PromptFile` error variant (e.g.
/// [`crate::work::audit::AuditLaunchError::PromptFile`]) rather than falling
/// back to an empty prompt.
#[derive(Debug, Error)]
#[error("failed to read {field} {path}: {source}", path = path.display())]
pub struct PromptFileError {
    /// The config key `raw_path` came from, e.g. `[work.create].prompt_file`.
    pub field: &'static str,
    /// The `~`-expanded path that was actually read.
    pub path: PathBuf,
    /// The underlying read failure.
    #[source]
    pub source: std::io::Error,
}

/// Reads `raw_path` (`~`-expanded against `home` via
/// [`crate::work::naming::expand_tilde`], the same idiom every other
/// `~`-expanding config caller in this codebase uses) into a `String` for
/// use as a launched session's opening prompt.
///
/// `field` names the config key `raw_path` came from (e.g.
/// `[work.create].prompt_file`) purely for [`PromptFileError`]'s message —
/// it plays no role in resolution.
pub fn read_prompt_file(
    raw_path: &str,
    home: &Path,
    field: &'static str,
) -> Result<String, PromptFileError> {
    let path = expand_tilde(raw_path, home);
    std::fs::read_to_string(&path).map_err(|source| PromptFileError {
        field,
        path,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn read_prompt_file_reads_contents_of_a_real_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("prompt.md");
        std::fs::write(&path, "/my-custom-prompt").unwrap();
        let home = PathBuf::from("/irrelevant");

        let contents =
            read_prompt_file(&path.to_string_lossy(), &home, "[work.create].prompt_file")
                .expect("read should succeed");

        assert_eq!(contents, "/my-custom-prompt");
    }

    #[test]
    fn read_prompt_file_errors_with_field_and_path_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing-prompt.md");
        let home = PathBuf::from("/irrelevant");

        let err = read_prompt_file(&path.to_string_lossy(), &home, "[work.create].prompt_file")
            .expect_err("missing file should error");

        let message = err.to_string();
        assert!(
            message.contains("[work.create].prompt_file"),
            "expected field name in error, got: {message}"
        );
        assert!(
            message.contains(&path.to_string_lossy().to_string()),
            "expected path in error, got: {message}"
        );
    }
}
