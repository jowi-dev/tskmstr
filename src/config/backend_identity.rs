//! Backend-identity resolution for a directory: what ticket backend (and,
//! for that backend, which project) a directory's effective tskmstr config
//! selects, and whether two directories' resolved backends match. See
//! GitHub issue #5's design: `docs/plans/issue-5-lane-backend-routing.md`.
//!
//! A lane whose repo resolves to a different [`BackendIdentity`] than the
//! board/CLI's own invoking repo cannot serve the ticket being launched —
//! its cwd-driven backend resolution would talk to the wrong provider (or
//! the right provider, wrong project). [`compatible_lane_names`] filters the
//! board's lane picker, `tm work run`'s preflight
//! (`crate::work::run::prepare_run_lane`) uses the same comparison as a hard
//! error, and [`resolve_audit_host_dir`] falls back to the current repo
//! instead of refusing, per the plan's audit-dir design.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::{BackendKind, Config, ConfigError, LaneConfig, default_paths, load};

/// What ticket backend (and, within that backend, which project) a
/// directory's effective config selects. Two directories are *compatible*
/// iff their identities are equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendIdentity {
    /// A Jira backend, identified by base URL + default project key.
    Jira {
        /// See [`Config::jira_base_url`].
        base_url: String,
        /// See [`Config::default_project_key`].
        project_key: String,
    },
    /// A GitHub Issues backend, identified by the `"owner/name"` repo slug.
    Github {
        /// See [`Config::github_repo`].
        repo: String,
    },
}

impl BackendIdentity {
    /// Derives the identity a resolved [`Config`] selects. Infallible:
    /// [`Config`] already guarantees the fields each [`BackendKind`] variant
    /// needs are populated (empty under the other backend, never the
    /// selected one).
    pub fn from_config(config: &Config) -> Self {
        match config.backend {
            BackendKind::Jira => BackendIdentity::Jira {
                base_url: config.jira_base_url.clone(),
                project_key: config.default_project_key.clone(),
            },
            BackendKind::Github => BackendIdentity::Github {
                repo: config.github_repo.clone().unwrap_or_default(),
            },
        }
    }
}

impl std::fmt::Display for BackendIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendIdentity::Jira {
                base_url,
                project_key,
            } => write!(f, "jira ({base_url}, project {project_key})"),
            BackendIdentity::Github { repo } => write!(f, "github ({repo})"),
        }
    }
}

/// Resolves the [`BackendIdentity`] a directory's effective tskmstr config
/// selects. Behind a trait so board/reducer/lane-run tests can stay
/// hermetic (see [`FakeBackendIdentityResolver`]) — the real implementation
/// hits the filesystem (and, for a `[backend.github]` repo with no explicit
/// `repo` set, `git config --get remote.origin.url`) exactly the way
/// [`load`] already does for the process's own cwd.
pub trait BackendIdentityResolver {
    /// Loads `dir`'s effective config (the global config layered with
    /// `dir`'s own `.tskmstr.toml`, if any) and derives its
    /// [`BackendIdentity`].
    fn resolve(&self, dir: &Path) -> Result<BackendIdentity, ConfigError>;
}

/// Production [`BackendIdentityResolver`]: loads `dir`'s effective config
/// the same way [`load`] resolves the process's own cwd, using `home` for
/// the global config path.
#[derive(Debug, Clone)]
pub struct FsBackendIdentityResolver {
    /// The invoking user's home directory (`~/.config/tskmstr/config.toml`
    /// is the global config file this resolver reads).
    pub home: PathBuf,
}

impl BackendIdentityResolver for FsBackendIdentityResolver {
    fn resolve(&self, dir: &Path) -> Result<BackendIdentity, ConfigError> {
        let paths = default_paths(&self.home, Some(dir));
        let config = load(&paths)?;
        Ok(BackendIdentity::from_config(&config))
    }
}

/// Test double for [`BackendIdentityResolver`]: returns a canned identity
/// for each directory it's told about via [`with_identity`](Self::with_identity),
/// and a [`ConfigError::MissingField`] (arbitrary — callers only care that
/// it's an `Err`) for anything else.
#[derive(Debug, Default, Clone)]
pub struct FakeBackendIdentityResolver {
    identities: HashMap<PathBuf, BackendIdentity>,
}

impl FakeBackendIdentityResolver {
    /// A resolver with no known directories; every
    /// [`resolve`](BackendIdentityResolver::resolve) call errors until
    /// [`with_identity`](Self::with_identity) is used.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `dir` as resolving to `identity`.
    pub fn with_identity(mut self, dir: impl Into<PathBuf>, identity: BackendIdentity) -> Self {
        self.identities.insert(dir.into(), identity);
        self
    }
}

impl BackendIdentityResolver for FakeBackendIdentityResolver {
    fn resolve(&self, dir: &Path) -> Result<BackendIdentity, ConfigError> {
        self.identities
            .get(dir)
            .cloned()
            .ok_or_else(|| ConfigError::MissingField {
                field: "backend_identity (fake resolver has no entry for this directory)",
                expected_path: dir.to_path_buf(),
            })
    }
}

/// Partitions `lanes` into repo-compatible lane names (in `lanes`'
/// iteration order, i.e. lane-name order, since it's a [`BTreeMap`]) and a
/// count of lanes hidden because their repo's resolved backend identity
/// doesn't match `current` — including any lane whose repo identity
/// couldn't be resolved at all, which is treated as incompatible rather
/// than silently offered.
pub fn compatible_lane_names(
    current: &BackendIdentity,
    lanes: &BTreeMap<String, LaneConfig>,
    resolver: &dyn BackendIdentityResolver,
) -> (Vec<String>, usize) {
    let mut names = Vec::new();
    let mut hidden = 0usize;
    for (name, lane) in lanes {
        match resolver.resolve(Path::new(&lane.repo)) {
            Ok(identity) if identity == *current => names.push(name.clone()),
            _ => hidden += 1,
        }
    }
    (names, hidden)
}

/// Resolves the directory a launched audit session should host in:
/// `configured_dir` when its resolved backend identity matches `current`,
/// or `fallback_dir` (the board/CLI's own current repo) otherwise —
/// including when `configured_dir`'s identity can't be resolved at all.
/// Returns whether the fallback was used, so the caller can surface it (see
/// `docs/plans/issue-5-lane-backend-routing.md`'s "audit dir falls back...
/// instead of refusing" decision).
pub fn resolve_audit_host_dir(
    configured_dir: &Path,
    current: &BackendIdentity,
    fallback_dir: &Path,
    resolver: &dyn BackendIdentityResolver,
) -> (PathBuf, bool) {
    match resolver.resolve(configured_dir) {
        Ok(identity) if identity == *current => (configured_dir.to_path_buf(), false),
        _ => (fallback_dir.to_path_buf(), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jira(base_url: &str, project_key: &str) -> BackendIdentity {
        BackendIdentity::Jira {
            base_url: base_url.to_string(),
            project_key: project_key.to_string(),
        }
    }

    fn github(repo: &str) -> BackendIdentity {
        BackendIdentity::Github {
            repo: repo.to_string(),
        }
    }

    fn lane(repo: &str) -> LaneConfig {
        LaneConfig {
            repo: repo.to_string(),
            prompt_file: None,
            base_branch: None,
            model: None,
            max_turns: None,
            permission_mode: None,
        }
    }

    #[test]
    fn compatible_lane_names_keeps_matching_lanes() {
        let current = jira("https://x.atlassian.net", "PROJ");
        let mut lanes = BTreeMap::new();
        lanes.insert("axiom".to_string(), lane("/repo/axiom"));
        let resolver = FakeBackendIdentityResolver::new()
            .with_identity("/repo/axiom", jira("https://x.atlassian.net", "PROJ"));

        let (names, hidden) = compatible_lane_names(&current, &lanes, &resolver);
        assert_eq!(names, vec!["axiom".to_string()]);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn compatible_lane_names_hides_mismatched_lanes() {
        let current = github("jowi-dev/tskmstr");
        let mut lanes = BTreeMap::new();
        lanes.insert("axiom".to_string(), lane("/repo/axiom"));
        let resolver = FakeBackendIdentityResolver::new()
            .with_identity("/repo/axiom", jira("https://x.atlassian.net", "PROJ"));

        let (names, hidden) = compatible_lane_names(&current, &lanes, &resolver);
        assert!(names.is_empty());
        assert_eq!(hidden, 1);
    }

    #[test]
    fn compatible_lane_names_hides_unresolvable_lanes() {
        let current = github("jowi-dev/tskmstr");
        let mut lanes = BTreeMap::new();
        lanes.insert("axiom".to_string(), lane("/repo/axiom"));
        let resolver = FakeBackendIdentityResolver::new();

        let (names, hidden) = compatible_lane_names(&current, &lanes, &resolver);
        assert!(names.is_empty());
        assert_eq!(hidden, 1);
    }

    #[test]
    fn compatible_lane_names_keeps_multiple_compatible_lanes_in_order() {
        let current = github("jowi-dev/tskmstr");
        let mut lanes = BTreeMap::new();
        lanes.insert("frontend".to_string(), lane("/repo/frontend"));
        lanes.insert("backend".to_string(), lane("/repo/backend"));
        let resolver = FakeBackendIdentityResolver::new()
            .with_identity("/repo/frontend", github("jowi-dev/tskmstr"))
            .with_identity("/repo/backend", github("jowi-dev/tskmstr"));

        let (names, hidden) = compatible_lane_names(&current, &lanes, &resolver);
        assert_eq!(names, vec!["backend".to_string(), "frontend".to_string()]);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn resolve_audit_host_dir_keeps_configured_dir_when_compatible() {
        let current = jira("https://x.atlassian.net", "PROJ");
        let resolver = FakeBackendIdentityResolver::new()
            .with_identity("/repo/axiom", jira("https://x.atlassian.net", "PROJ"));

        let (dir, fell_back) = resolve_audit_host_dir(
            Path::new("/repo/axiom"),
            &current,
            Path::new("/repo/tskmstr"),
            &resolver,
        );
        assert_eq!(dir, PathBuf::from("/repo/axiom"));
        assert!(!fell_back);
    }

    #[test]
    fn resolve_audit_host_dir_falls_back_when_incompatible() {
        let current = github("jowi-dev/tskmstr");
        let resolver = FakeBackendIdentityResolver::new()
            .with_identity("/repo/axiom", jira("https://x.atlassian.net", "PROJ"));

        let (dir, fell_back) = resolve_audit_host_dir(
            Path::new("/repo/axiom"),
            &current,
            Path::new("/repo/tskmstr"),
            &resolver,
        );
        assert_eq!(dir, PathBuf::from("/repo/tskmstr"));
        assert!(fell_back);
    }

    #[test]
    fn resolve_audit_host_dir_falls_back_when_unresolvable() {
        let current = github("jowi-dev/tskmstr");
        let resolver = FakeBackendIdentityResolver::new();

        let (dir, fell_back) = resolve_audit_host_dir(
            Path::new("/repo/axiom"),
            &current,
            Path::new("/repo/tskmstr"),
            &resolver,
        );
        assert_eq!(dir, PathBuf::from("/repo/tskmstr"));
        assert!(fell_back);
    }

    #[test]
    fn fs_resolver_resolves_github_identity_from_repo_local_config() {
        use std::fs;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(home.join(".config/tskmstr")).unwrap();
        fs::write(
            home.join(".config/tskmstr/config.toml"),
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"
            "#,
        )
        .unwrap();

        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        fs::write(
            repo_dir.join(".tskmstr.toml"),
            r#"
            [backend]
            provider = "github"
            [backend.github]
            repo = "jowi-dev/tskmstr"
            "#,
        )
        .unwrap();

        let resolver = FsBackendIdentityResolver { home };
        let identity = resolver.resolve(&repo_dir).expect("should resolve");
        assert_eq!(identity, github("jowi-dev/tskmstr"));
    }

    #[test]
    fn fs_resolver_falls_back_to_global_identity_when_no_repo_config() {
        use std::fs;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(home.join(".config/tskmstr")).unwrap();
        fs::write(
            home.join(".config/tskmstr/config.toml"),
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"
            "#,
        )
        .unwrap();

        let repo_dir = dir.path().join("axiom");
        fs::create_dir_all(&repo_dir).unwrap();

        let resolver = FsBackendIdentityResolver { home };
        let identity = resolver.resolve(&repo_dir).expect("should resolve");
        assert_eq!(identity, jira("https://global.atlassian.net", "GLOBAL"));
    }
}
