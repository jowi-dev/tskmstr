//! Configuration loading with global/repo override precedence.
//!
//! tskmstr reads a global config file (`~/.config/tskmstr/config.toml`) and,
//! optionally, a repo-local override file (`.tskmstr.toml`) in the current
//! repository root. Fields present in the repo file take precedence over the
//! global file. The merged result must supply all required fields or
//! [`ConfigError::MissingField`] is returned.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Raw, partially-specified configuration as parsed directly from TOML.
///
/// Every field is optional because either the global or the repo file alone
/// may omit any given setting; the two are merged by [`merge`].
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RawConfig {
    /// Base URL of the Jira instance, e.g. `https://example.atlassian.net`.
    pub jira_base_url: Option<String>,
    /// Email address used for Jira basic auth.
    pub jira_email: Option<String>,
    /// Default Jira project key used when none is specified explicitly.
    pub default_project_key: Option<String>,
    /// Default Jira account ID to assign new tickets to.
    pub default_assignee_account_id: Option<String>,
}

/// Fully validated configuration ready for use by the rest of the
/// application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Base URL of the Jira instance, e.g. `https://example.atlassian.net`.
    pub jira_base_url: String,
    /// Email address used for Jira basic auth.
    pub jira_email: String,
    /// Default Jira project key used when none is specified explicitly.
    pub default_project_key: String,
    /// Default Jira account ID to assign new tickets to, if configured.
    pub default_assignee_account_id: Option<String>,
}

/// Locations to read configuration from.
///
/// `repo` is optional: a repository-local override file need not exist.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// Path to the global config file (typically under `$HOME/.config`).
    pub global: PathBuf,
    /// Path to an optional repo-local override file.
    pub repo: Option<PathBuf>,
}

/// Errors that can occur while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The global config file could not be read from disk.
    #[error("failed to read global config file {path}: {source}")]
    ReadGlobal {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// The repo config file could not be read from disk.
    #[error("failed to read repo config file {path}: {source}")]
    ReadRepo {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// A config file's contents were not valid TOML.
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        /// Path whose contents failed to parse.
        path: PathBuf,
        /// Underlying TOML parse error.
        #[source]
        source: toml::de::Error,
    },

    /// A required field was missing after merging global and repo config.
    #[error(
        "missing required config field `{field}`; set it in {expected_path} \
         (or in a repo-local .tskmstr.toml)"
    )]
    MissingField {
        /// Name of the missing field.
        field: &'static str,
        /// Path to the expected global config file, shown to guide the user.
        expected_path: PathBuf,
    },

    /// A parent directory for a config file could not be created.
    #[error("failed to create config directory {path}: {source}")]
    CreateDir {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// A config file could not be written to disk.
    #[error("failed to write config file {path}: {source}")]
    Write {
        /// Path that could not be written.
        path: PathBuf,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// A config value could not be serialized to TOML.
    #[error("failed to serialize config for {path}: {source}")]
    Serialize {
        /// Path the value was destined for.
        path: PathBuf,
        /// Underlying TOML serialization error.
        #[source]
        source: toml::ser::Error,
    },
}

/// Seed values used to bootstrap a brand-new global config file.
///
/// Distinct from [`RawConfig`] because it requires every field: a freshly
/// bootstrapped config should never be missing the fields tskmstr can't
/// function without.
#[derive(Debug, Clone, Serialize)]
pub struct GlobalConfigSeed {
    /// Base URL of the Jira instance, e.g. `https://example.atlassian.net`.
    pub jira_base_url: String,
    /// Email address used for Jira basic auth.
    pub jira_email: String,
    /// Default Jira project key used when none is specified explicitly.
    pub default_project_key: String,
}

/// Write a brand-new global config file at `path`, creating parent
/// directories as needed.
///
/// Used by `tm auth login` to bootstrap `~/.config/tskmstr/config.toml` when
/// it doesn't exist yet. Overwrites any existing file at `path`.
pub fn write_global(path: &Path, seed: &GlobalConfigSeed) -> Result<(), ConfigError> {
    write_raw_config(path, &to_raw(seed))
}

/// Convert a [`GlobalConfigSeed`] into the [`RawConfig`] shape written to disk.
fn to_raw(seed: &GlobalConfigSeed) -> RawConfig {
    RawConfig {
        jira_base_url: Some(seed.jira_base_url.clone()),
        jira_email: Some(seed.jira_email.clone()),
        default_project_key: Some(seed.default_project_key.clone()),
        default_assignee_account_id: None,
    }
}

/// Read the existing global config at `path`, set its
/// `default_assignee_account_id`, and write it back.
///
/// Used by `tm auth login` once a token has been validated against Jira, so
/// subsequent ticket creation has a default assignee without the user having
/// to edit the config file by hand.
pub fn set_assignee_account_id(path: &Path, account_id: &str) -> Result<(), ConfigError> {
    let mut raw = read_raw_config(path, |path, source| ConfigError::ReadGlobal {
        path: path.to_path_buf(),
        source,
    })?;
    raw.default_assignee_account_id = Some(account_id.to_string());
    write_raw_config(path, &raw)
}

/// Serialize `raw` as TOML and write it to `path`, creating parent
/// directories as needed.
fn write_raw_config(path: &Path, raw: &RawConfig) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let contents = toml::to_string_pretty(raw).map_err(|source| ConfigError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;

    std::fs::write(path, contents).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// Merge a repo-local override on top of a global config, then validate that
/// all required fields are present.
///
/// Fields set in `repo` win over `global`; fields left `None` in `repo` (or
/// when `repo` is absent entirely) fall back to `global`.
pub fn merge(global: RawConfig, repo: Option<RawConfig>) -> Result<Config, ConfigError> {
    let repo = repo.unwrap_or_default();

    let jira_base_url = repo.jira_base_url.or(global.jira_base_url);
    let jira_email = repo.jira_email.or(global.jira_email);
    let default_project_key = repo.default_project_key.or(global.default_project_key);
    let default_assignee_account_id = repo
        .default_assignee_account_id
        .or(global.default_assignee_account_id);

    let expected_path = default_global_config_path();

    Ok(Config {
        jira_base_url: require_field(jira_base_url, "jira_base_url", &expected_path)?,
        jira_email: require_field(jira_email, "jira_email", &expected_path)?,
        default_project_key: require_field(
            default_project_key,
            "default_project_key",
            &expected_path,
        )?,
        default_assignee_account_id,
    })
}

/// Return `value`, or a [`ConfigError::MissingField`] naming `field` and the
/// expected global config path.
fn require_field(
    value: Option<String>,
    field: &'static str,
    expected_path: &Path,
) -> Result<String, ConfigError> {
    value.ok_or_else(|| ConfigError::MissingField {
        field,
        expected_path: expected_path.to_path_buf(),
    })
}

/// The conventional global config path, used only for error messages when a
/// caller didn't go through [`load`]/[`default_paths`].
fn default_global_config_path() -> PathBuf {
    dirs_home().join(".config/tskmstr/config.toml")
}

/// Best-effort home directory lookup for error-message purposes only.
fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"))
}

/// Load and merge configuration from the given paths.
///
/// Reads and parses the global file (required to exist) and, if present, the
/// repo file, then merges them via [`merge`].
pub fn load(paths: &ConfigPaths) -> Result<Config, ConfigError> {
    let global = read_raw_config(&paths.global, |path, source| ConfigError::ReadGlobal {
        path: path.to_path_buf(),
        source,
    })?;

    let repo = match &paths.repo {
        Some(repo_path) => match std::fs::read_to_string(repo_path) {
            Ok(contents) => Some(parse_raw_config(repo_path, &contents)?),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(ConfigError::ReadRepo {
                    path: repo_path.clone(),
                    source,
                });
            }
        },
        None => None,
    };

    merge(global, repo).map_err(|err| match err {
        ConfigError::MissingField { field, .. } => ConfigError::MissingField {
            field,
            expected_path: paths.global.clone(),
        },
        other => other,
    })
}

/// Read and parse a required config file, mapping IO errors via `on_read_err`.
fn read_raw_config(
    path: &Path,
    on_read_err: impl FnOnce(&Path, std::io::Error) -> ConfigError,
) -> Result<RawConfig, ConfigError> {
    let contents = std::fs::read_to_string(path).map_err(|source| on_read_err(path, source))?;
    parse_raw_config(path, &contents)
}

/// Parse TOML content into a [`RawConfig`], attaching `path` to any error.
fn parse_raw_config(path: &Path, contents: &str) -> Result<RawConfig, ConfigError> {
    toml::from_str(contents).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// Build the default [`ConfigPaths`] for a given home directory and optional
/// repository root.
///
/// The global path is `<home>/.config/tskmstr/config.toml`; the repo path,
/// when a repo root is given, is `<repo_root>/.tskmstr.toml`.
pub fn default_paths(home: &Path, repo_root: Option<&Path>) -> ConfigPaths {
    ConfigPaths {
        global: home.join(".config/tskmstr/config.toml"),
        repo: repo_root.map(|root| root.join(".tskmstr.toml")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn raw_full() -> RawConfig {
        RawConfig {
            jira_base_url: Some("https://global.atlassian.net".into()),
            jira_email: Some("global@example.com".into()),
            default_project_key: Some("GLOBAL".into()),
            default_assignee_account_id: Some("acct-global".into()),
        }
    }

    #[test]
    fn merge_global_only_produces_config() {
        let cfg = merge(raw_full(), None).expect("should merge");
        assert_eq!(cfg.jira_base_url, "https://global.atlassian.net");
        assert_eq!(cfg.jira_email, "global@example.com");
        assert_eq!(cfg.default_project_key, "GLOBAL");
        assert_eq!(cfg.default_assignee_account_id, Some("acct-global".into()));
    }

    #[test]
    fn merge_repo_overrides_global_field_by_field() {
        let repo = RawConfig {
            jira_base_url: None,
            jira_email: None,
            default_project_key: Some("REPO".into()),
            default_assignee_account_id: None,
        };
        let cfg = merge(raw_full(), Some(repo)).expect("should merge");
        // Overridden field wins.
        assert_eq!(cfg.default_project_key, "REPO");
        // Non-overridden fields fall back to global.
        assert_eq!(cfg.jira_base_url, "https://global.atlassian.net");
        assert_eq!(cfg.jira_email, "global@example.com");
        assert_eq!(cfg.default_assignee_account_id, Some("acct-global".into()));
    }

    #[test]
    fn merge_missing_required_field_errors_with_field_name_and_path() {
        let global = RawConfig {
            jira_base_url: None,
            ..raw_full()
        };
        let err = merge(global, None).expect_err("should fail");
        let message = err.to_string();
        assert!(
            message.contains("jira_base_url"),
            "error should name the missing field: {message}"
        );
        assert!(
            message.contains("config.toml"),
            "error should mention the expected config file path: {message}"
        );
    }

    #[test]
    fn load_global_only() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://only-global.atlassian.net"
            jira_email = "only-global@example.com"
            default_project_key = "ONLY"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.jira_base_url, "https://only-global.atlassian.net");
        assert_eq!(cfg.default_project_key, "ONLY");
        assert_eq!(cfg.default_assignee_account_id, None);
    }

    #[test]
    fn load_global_plus_repo_override() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"
            "#,
        )
        .unwrap();

        let repo_path = dir.path().join(".tskmstr.toml");
        fs::write(&repo_path, r#"default_project_key = "REPO""#).unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.default_project_key, "REPO");
        assert_eq!(cfg.jira_base_url, "https://global.atlassian.net");
    }

    #[test]
    fn load_repo_file_absent_is_fine() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(dir.path().join("does-not-exist.toml")),
        };
        let err = load(&paths);
        // Absence of the repo file should NOT cause an error; the loader
        // should treat it the same as `repo: None`.
        assert!(
            err.is_ok(),
            "missing repo file should be treated as absent, got {err:?}"
        );
    }

    #[test]
    fn load_malformed_toml_errors() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(&global_path, "this is not : valid toml ===").unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let err = load(&paths).expect_err("should fail to parse");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn default_paths_builds_expected_locations() {
        let home = Path::new("/home/alice");
        let repo_root = Path::new("/repo/checkout");
        let paths = default_paths(home, Some(repo_root));
        assert_eq!(
            paths.global,
            PathBuf::from("/home/alice/.config/tskmstr/config.toml")
        );
        assert_eq!(
            paths.repo,
            Some(PathBuf::from("/repo/checkout/.tskmstr.toml"))
        );
    }

    #[test]
    fn default_paths_without_repo_root_has_no_repo_path() {
        let home = Path::new("/home/alice");
        let paths = default_paths(home, None);
        assert_eq!(paths.repo, None);
    }

    #[test]
    fn write_global_creates_parent_dirs_and_writes_loadable_config() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/tskmstr/config.toml");
        let seed = GlobalConfigSeed {
            jira_base_url: "https://home-solutions.atlassian.net".to_string(),
            jira_email: "joe.williams@homesolutions.com".to_string(),
            default_project_key: "AX".to_string(),
        };

        write_global(&path, &seed).expect("should write");

        let paths = ConfigPaths {
            global: path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load written config");
        assert_eq!(cfg.jira_base_url, "https://home-solutions.atlassian.net");
        assert_eq!(cfg.jira_email, "joe.williams@homesolutions.com");
        assert_eq!(cfg.default_project_key, "AX");
        assert_eq!(cfg.default_assignee_account_id, None);
    }

    #[test]
    fn write_global_overwrites_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "jira_base_url = \"https://old.atlassian.net\"").unwrap();

        let seed = GlobalConfigSeed {
            jira_base_url: "https://new.atlassian.net".to_string(),
            jira_email: "new@example.com".to_string(),
            default_project_key: "NEW".to_string(),
        };
        write_global(&path, &seed).expect("should write");

        let paths = ConfigPaths {
            global: path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.jira_base_url, "https://new.atlassian.net");
    }

    #[test]
    fn set_assignee_account_id_updates_field_and_preserves_others() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let seed = GlobalConfigSeed {
            jira_base_url: "https://home-solutions.atlassian.net".to_string(),
            jira_email: "joe.williams@homesolutions.com".to_string(),
            default_project_key: "AX".to_string(),
        };
        write_global(&path, &seed).expect("should write");

        set_assignee_account_id(&path, "acct-123").expect("should update");

        let paths = ConfigPaths {
            global: path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(
            cfg.default_assignee_account_id,
            Some("acct-123".to_string())
        );
        assert_eq!(cfg.jira_base_url, "https://home-solutions.atlassian.net");
        assert_eq!(cfg.default_project_key, "AX");
    }

    #[test]
    fn set_assignee_account_id_missing_file_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");

        let err = set_assignee_account_id(&path, "acct-123").expect_err("should fail");
        assert!(matches!(err, ConfigError::ReadGlobal { .. }));
    }
}
