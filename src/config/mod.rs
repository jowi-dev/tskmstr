//! Configuration loading with global/repo override precedence.
//!
//! tskmstr reads a global config file (`~/.config/tskmstr/config.toml`) and,
//! optionally, a repo-local override file (`.tskmstr.toml`) in the current
//! repository root. Fields present in the repo file take precedence over the
//! global file. The merged result must supply all required fields or
//! [`ConfigError::MissingField`] is returned.
//!
//! An optional `[backend]` table selects which ticket provider a config
//! uses (see [`BackendKind`]); it defaults to Jira when absent, so an
//! existing config with no `[backend]` table keeps working unchanged.
//! [`merge`] validates the selected provider's own required fields — for
//! Jira, the top-level `jira_base_url`/`jira_email`/`default_project_key`
//! fields, unconditionally required before `[backend]` existed and now
//! required only when Jira is the selected provider. A provider name that
//! doesn't parse into a [`BackendKind`] at all is
//! [`ConfigError::InvalidProvider`]; a recognized name with no adapter
//! implementation yet (currently `"github"`, see GitHub issue #3) is
//! [`ConfigError::ProviderNotImplemented`] — [`load`]/[`merge`] never
//! panics or silently falls back to Jira for either case.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod backend_identity;
pub use backend_identity::{
    BackendIdentity, BackendIdentityResolver, FakeBackendIdentityResolver,
    FsBackendIdentityResolver, compatible_lane_names, resolve_audit_host_dir,
};

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
    /// Workflow status name to transition an auto-created ticket to, e.g.
    /// `"In Review"`.
    ///
    /// Applies to tickets auto-created by `tm pr create` /
    /// `tm pr status --auto-ticket`, and to a pre-existing ticket that
    /// `tm pr create` associates with a newly opened PR (unless that ticket
    /// is already in the target status). `tm ticket <KEY>` never
    /// transitions a ticket's status. When unset, tickets are left in
    /// whatever status they're already in.
    pub status_on_pr: Option<String>,
    /// Workflow status name to transition a ticket to right after
    /// `tm ticket create` makes it, e.g. `"In Progress"`.
    ///
    /// Jira's create-issue API can't set status directly, so without this
    /// setting a newly created ticket is left in the workflow's initial
    /// status.
    pub status_on_create: Option<String>,
    /// Override path for the run-state SQLite database used by `tm runs`.
    ///
    /// When unset, `tm runs` falls back to
    /// [`crate::runs::default_db_path`]. Set here (typically in a repo-local
    /// `.tskmstr.toml`) so that a project's runs stay in the project rather
    /// than the shared per-user default.
    pub run_db_path: Option<String>,
    /// GitHub bot logins whose PR review threads count as "bot findings",
    /// e.g. `cursor[bot]`.
    ///
    /// When unset in both global and repo config, defaults to
    /// `["cursor[bot]"]`.
    pub review_bots: Option<Vec<String>>,
    /// Workflow status names, in the order the board's columns should show
    /// them, e.g. `["To Do", "In Progress", "Code Review"]`.
    ///
    /// Matching against a ticket's status name is case-insensitive.
    /// Statuses not listed here keep the board's default ordering (status
    /// category, then alphabetically) and are shown after every listed
    /// column. When unset in both global and repo config, the board uses
    /// the default ordering for every column.
    pub board_column_order: Option<Vec<String>>,
    /// `tm work` settings: worktree/session defaults and per-lane
    /// definitions. See [`RawWorkConfig`].
    ///
    /// When unset in both global and repo config, `tm work` has no lanes to
    /// run and no defaults beyond its own hardcoded fallbacks.
    pub work: Option<RawWorkConfig>,
    /// `[backend]` settings: which ticket provider this config selects. See
    /// [`RawBackendConfig`].
    ///
    /// When unset in both global and repo config, defaults to
    /// [`BackendKind::Jira`], and the top-level `jira_base_url`/`jira_email`/
    /// `default_project_key` fields above are required exactly as before
    /// `[backend]` existed.
    pub backend: Option<RawBackendConfig>,
}

/// Raw, partially-specified `[backend]` section as parsed directly from
/// TOML.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RawBackendConfig {
    /// Which ticket provider to use: `"jira"` (default) or `"github"`. An
    /// unrecognized value is [`ConfigError::InvalidProvider`]; a recognized
    /// value with no adapter implementation yet (currently `"github"`) is
    /// [`ConfigError::ProviderNotImplemented`].
    pub provider: Option<String>,
    /// `[backend.jira]`: the canonical (but not yet required) location for
    /// Jira's own fields. See [`RawJiraBackendConfig`] — `merge` reads these
    /// fields here first, falling back to the legacy flat top-level
    /// `jira_base_url`/`jira_email`/`default_project_key` on [`RawConfig`]
    /// when this section (or the specific field within it) is absent, so an
    /// existing flat config keeps working bit-for-bit.
    pub jira: Option<RawJiraBackendConfig>,
    /// `[backend.github]`: settings for the GitHub Issues provider. See
    /// [`RawGithubBackendConfig`].
    pub github: Option<RawGithubBackendConfig>,
}

/// Raw, partially-specified `[backend.jira]` section as parsed directly from
/// TOML — the canonical location for Jira's fields (see
/// [`RawBackendConfig::jira`]'s doc comment for why the legacy flat keys on
/// [`RawConfig`] still work too).
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RawJiraBackendConfig {
    /// Base URL of the Jira instance, e.g. `https://example.atlassian.net`.
    pub jira_base_url: Option<String>,
    /// Email address used for Jira basic auth.
    pub jira_email: Option<String>,
    /// Default Jira project key used when none is specified explicitly.
    pub default_project_key: Option<String>,
}

/// Raw, partially-specified `[backend.github]` section as parsed directly
/// from TOML.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RawGithubBackendConfig {
    /// The `"owner/name"` GitHub repo slug the GitHub provider targets.
    ///
    /// When absent, [`load`] (not [`merge`], which stays a pure function of
    /// its `RawConfig` arguments) falls back to the checked-out repo's
    /// `origin` remote, per GitHub issue #3's design ("defaults to the
    /// origin remote"). See [`detect_origin_repo`].
    pub repo: Option<String>,
}

/// Which ticket provider a config selects.
///
/// This is the one enum a new adapter extends: add a variant here, add its
/// config validation as a new arm of the `match` in [`merge`], add its
/// [`crate::ticketing::provider::TicketProvider`] implementation, and wire
/// it into whichever construction site builds the live provider (currently
/// `main.rs`, unconditionally Jira until a later phase branches on this
/// enum). Nothing else — no board code, no `tm ticket`/`tm ready` command,
/// no other `match` on provider kind — needs to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// Jira, via [`crate::ticketing::provider::JiraProvider`]. The only
    /// adapter implemented so far, and the default when `[backend]` is
    /// absent.
    #[default]
    Jira,
    /// GitHub Issues. Recognized as a valid `provider` value (per GitHub
    /// issue #3's design) but not yet implemented — selecting it is
    /// [`ConfigError::ProviderNotImplemented`], not [`ConfigError::InvalidProvider`].
    Github,
}

impl BackendKind {
    /// The lowercase string stored in config for this provider.
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Jira => "jira",
            BackendKind::Github => "github",
        }
    }

    /// Parses a config `provider` string. Returns `None` for unrecognized
    /// values.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jira" => Some(BackendKind::Jira),
            "github" => Some(BackendKind::Github),
            _ => None,
        }
    }
}

/// Raw, partially-specified `[work]` section as parsed directly from TOML.
///
/// Mirrors [`RawConfig`]'s optional-everything shape: either the global or
/// the repo file may set any subset of these fields, merged by [`merge`].
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RawWorkConfig {
    /// Root directory under which `tm work` creates git worktrees for lanes,
    /// e.g. `~/Worktrees`. Tilde is not expanded by this module (consistent
    /// with [`RawConfig::run_db_path`]); expansion, if any, is the
    /// responsibility of the `tm work` caller.
    pub worktree_root: Option<String>,
    /// Default Claude model used for a lane's driver process when the lane
    /// itself doesn't set [`RawLaneConfig::model`].
    pub default_model: Option<String>,
    /// Default max-turns budget for a lane's driver process when the lane
    /// itself doesn't set [`RawLaneConfig::max_turns`].
    pub default_max_turns: Option<u32>,
    /// Default permission mode for a lane's driver process when the lane
    /// itself doesn't set [`RawLaneConfig::permission_mode`].
    pub default_permission_mode: Option<String>,
    /// Extra tmux window names created alongside the primary window when a
    /// lane's session is provisioned.
    pub tmux_windows: Option<Vec<String>>,
    /// Name of the tmux window considered "primary" (e.g. where the driver
    /// process runs) when a lane's session is provisioned.
    pub tmux_primary_window: Option<String>,
    /// Per-lane definitions, keyed by lane name.
    ///
    /// Merge precedence: a repo-local lane entry replaces the corresponding
    /// global lane entry *as a whole* rather than being merged field by
    /// field. There's no existing precedent in this module for deep-merging
    /// nested structures, and lanes are small enough that whole-lane
    /// replacement is the simplest rule a config author can reason about
    /// ("the repo-local `.tskmstr.toml` fully owns any lane it names").
    #[serde(default)]
    pub lanes: BTreeMap<String, RawLaneConfig>,
    /// `[work.audit]` settings for board-launched ticket-audit sessions. See
    /// [`RawAuditConfig`].
    ///
    /// When unset in both global and repo config, launching an audit session
    /// is disabled (see `docs/plans/board-audits.md`'s "Launch" design):
    /// [`AuditConfig::dir`] is required for [`crate::work::audit::launch_audit`]
    /// to do anything.
    pub audit: Option<RawAuditConfig>,
    /// `[work.review_watch]` settings for the bugbot-follow-through watcher
    /// and cleanup session. See [`RawReviewWatchConfig`].
    pub review_watch: Option<RawReviewWatchConfig>,
    /// `[work.manual]` settings for the board's manual-session key. See
    /// [`RawManualConfig`].
    ///
    /// When unset in both global and repo config, the manual-session board
    /// action is disabled: [`ManualConfig::windows`] must be non-empty for
    /// `crate::work::manual::ensure_manual_session` to do anything.
    pub manual: Option<RawManualConfig>,
}

/// Raw, partially-specified `[work.audit]` subsection as parsed directly from
/// TOML. See `docs/plans/board-audits.md`'s "Launch" design.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RawAuditConfig {
    /// Directory a launched ticket-audit session runs in (the repo whose
    /// `.claude/` provides the audit skill and hook settings), e.g.
    /// `~/Projects/axiom`. Tilde is not expanded by this module, matching
    /// [`RawWorkConfig::worktree_root`); expansion is the launching caller's
    /// responsibility. Required to enable launching at all.
    pub dir: Option<String>,
    /// Prompt template fed to `claude` on launch, with `{key}` replaced by
    /// the ticket key, e.g. `/ticket-audit {key}`. Defaults to
    /// `/ticket-audit {key}` when unset.
    pub prompt: Option<String>,
    /// Model alias passed to the launched session as `claude --model <model>`,
    /// e.g. `fable`. Deliberately separate from [`RawWorkConfig::default_model`],
    /// which only governs headless `tm work run` lanes: an audit is an
    /// interactive digging session whose model choice is worth setting
    /// independently of the lane default.
    ///
    /// When unset, `claude` picks its own default — which, under an
    /// enterprise-managed model pin, is whatever the policy pins rather than
    /// anything tskmstr configures. Set this to launch audits on a specific
    /// model regardless of that pin.
    pub model: Option<String>,
}

/// Raw, partially-specified `[work.manual]` subsection as parsed directly
/// from TOML.
///
/// Configures the board's manual-session key: the operator's default tmux
/// window layout for hand-working a ticket (e.g. code, fish, claude, server)
/// without any Claude-driven flow.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RawManualConfig {
    /// Working directory the manual windows open in. Tilde is not expanded
    /// by this module, matching [`RawAuditConfig::dir`]; expansion is the
    /// launching caller's responsibility. Required for the manual-session
    /// action to launch (alongside a non-empty `windows` list).
    pub dir: Option<String>,
    /// Windows to ensure in the ticket's tmux session, in order. Merge
    /// precedence: a repo-local list replaces the global list *as a whole*
    /// (like [`RawWorkConfig::tmux_windows`] and lanes), never element by
    /// element. Required (non-empty) to enable the action at all.
    pub windows: Option<Vec<RawManualWindow>>,
}

/// One window entry in [`RawManualConfig::windows`]: a window name plus an
/// optional command to run in it (absent = plain shell window).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawManualWindow {
    /// tmux window name, e.g. `code`.
    pub name: String,
    /// Command run in the window, handed to `$SHELL -c` by tmux. When unset,
    /// the window is a plain interactive shell.
    pub command: Option<String>,
}

/// Raw, partially-specified `[work.review_watch]` subsection as parsed
/// directly from TOML. See `docs/plans/bugbot-watch.md`'s "Config" design.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RawReviewWatchConfig {
    /// Directory the launched bugbot-cleanup session runs in. When unset,
    /// falls back to [`RawAuditConfig::dir`] (applied in [`merge_work`],
    /// not here — see [`ReviewWatchConfig::dir`]).
    pub dir: Option<String>,
    /// Prompt template fed to `claude` on cleanup-session launch, with
    /// `{key}` and `{findings_file}` replaced. Defaults to
    /// `/bugbot-triage {key} {findings_file}` when unset.
    pub prompt: Option<String>,
    /// Model alias passed to the launched cleanup session as
    /// `claude --model <model>`. When unset, falls back to
    /// [`RawAuditConfig::model`] (applied in [`merge_work`], not here — see
    /// [`ReviewWatchConfig::model`]).
    pub model: Option<String>,
    /// Seconds between PR polls while watching. Defaults to 45 when unset.
    pub poll_secs: Option<u64>,
    /// Minutes to keep watching before giving up. Defaults to 1440 (24h)
    /// when unset.
    pub max_wait_mins: Option<u64>,
    /// What to do once every configured bot has reviewed and left
    /// unresolved findings: `"notify"` or `"launch"`. Defaults to
    /// `"notify"` when unset; an unrecognized value is a [`ConfigError`].
    pub on_bots_done: Option<String>,
}

/// Raw, partially-specified `[work.lanes.<name>]` subsection as parsed
/// directly from TOML.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RawLaneConfig {
    /// Filesystem path to the repository this lane operates on. Required:
    /// a lane without a `repo` fails validation in [`merge`] rather than
    /// panicking later when `tm work` tries to use it.
    pub repo: Option<String>,
    /// Path to the prompt file fed to this lane's driver process.
    ///
    /// When unset, callers default to `~/.claude/prompts/<lane>.md`; that
    /// defaulting convention belongs to the `tm work` module, not config —
    /// this module only stores what was written.
    pub prompt_file: Option<String>,
    /// Base branch new lane branches are created from, overriding whatever
    /// default (e.g. `origin/HEAD`) the caller would otherwise use.
    pub base_branch: Option<String>,
    /// Claude model for this lane's driver process, overriding
    /// [`RawWorkConfig::default_model`].
    pub model: Option<String>,
    /// Max-turns budget for this lane's driver process, overriding
    /// [`RawWorkConfig::default_max_turns`].
    pub max_turns: Option<u32>,
    /// Permission mode for this lane's driver process, overriding
    /// [`RawWorkConfig::default_permission_mode`].
    pub permission_mode: Option<String>,
}

/// Fully validated configuration ready for use by the rest of the
/// application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Which ticket provider this config selects. See [`BackendKind`];
    /// defaults to [`BackendKind::Jira`] when `[backend]` is absent from
    /// both global and repo config. Since only the Jira adapter is
    /// implemented so far, this is always [`BackendKind::Jira`] on any
    /// [`Config`] that successfully merged — selecting a not-yet-implemented
    /// provider fails merging outright with [`ConfigError::ProviderNotImplemented`].
    pub backend: BackendKind,
    /// Base URL of the Jira instance, e.g. `https://example.atlassian.net`.
    /// Empty when `backend` is [`BackendKind::Github`] — this field, and the
    /// two below it, are Jira-only and only ever populated (and required) by
    /// [`merge`]'s [`BackendKind::Jira`] arm. See [`Config::github_repo`] for
    /// the GitHub-only counterpart.
    pub jira_base_url: String,
    /// Email address used for Jira basic auth. Empty under the GitHub
    /// backend — see [`Config::jira_base_url`].
    pub jira_email: String,
    /// Default Jira project key used when none is specified explicitly.
    /// Empty under the GitHub backend — see [`Config::jira_base_url`].
    pub default_project_key: String,
    /// The `"owner/name"` GitHub repo slug the GitHub provider targets.
    /// `None` when `backend` is [`BackendKind::Jira`]; always `Some` when
    /// `backend` is [`BackendKind::Github`], since [`merge`]'s
    /// [`BackendKind::Github`] arm requires it (explicitly configured, or
    /// defaulted from the origin remote by [`load`] — see
    /// [`RawGithubBackendConfig::repo`]).
    pub github_repo: Option<String>,
    /// Default Jira account ID to assign new tickets to, if configured.
    pub default_assignee_account_id: Option<String>,
    /// Workflow status name to transition an auto-created or pre-existing
    /// ticket to on `tm pr create`, if configured. See
    /// [`RawConfig::status_on_pr`] for semantics.
    pub status_on_pr: Option<String>,
    /// Workflow status name to transition a `tm ticket create`d ticket to,
    /// if configured. See [`RawConfig::status_on_create`] for semantics.
    pub status_on_create: Option<String>,
    /// Override path for the run-state SQLite database, if configured. See
    /// [`RawConfig::run_db_path`] for semantics.
    pub run_db_path: Option<String>,
    /// GitHub bot logins whose PR review threads count as "bot findings".
    /// See [`RawConfig::review_bots`] for semantics; defaults to
    /// `["cursor[bot]"]` when unset in both global and repo config.
    pub review_bots: Vec<String>,
    /// Configured board column order. See [`RawConfig::board_column_order`]
    /// for semantics; empty when unset in both global and repo config,
    /// which leaves the board's default ordering unchanged.
    pub board_column_order: Vec<String>,
    /// Validated `tm work` settings. See [`WorkConfig`]; empty (no lanes, no
    /// defaults set) when the `[work]` section is absent from both global
    /// and repo config.
    pub work: WorkConfig,
}

/// Fully validated `[work]` section.
///
/// Unlike [`Config`]'s top-level required fields, nothing here is required
/// at the `tm work`-defaults level: a config with no `[work]` section at all
/// produces a `WorkConfig` with every default `None` and no lanes. Lane
/// entries, however, must each have a `repo` (see [`LaneConfig`]) — that's
/// enforced at merge time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkConfig {
    /// See [`RawWorkConfig::worktree_root`].
    pub worktree_root: Option<String>,
    /// See [`RawWorkConfig::default_model`].
    pub default_model: Option<String>,
    /// See [`RawWorkConfig::default_max_turns`].
    pub default_max_turns: Option<u32>,
    /// See [`RawWorkConfig::default_permission_mode`].
    pub default_permission_mode: Option<String>,
    /// See [`RawWorkConfig::tmux_windows`]. Empty when unset in both global
    /// and repo config.
    pub tmux_windows: Vec<String>,
    /// See [`RawWorkConfig::tmux_primary_window`].
    pub tmux_primary_window: Option<String>,
    /// Validated per-lane definitions, keyed by lane name.
    pub lanes: BTreeMap<String, LaneConfig>,
    /// Validated `[work.audit]` settings. See [`AuditConfig`]; empty (no
    /// `dir`/`prompt` set, launching disabled) when the `[work.audit]`
    /// section is absent from both global and repo config.
    pub audit: AuditConfig,
    /// Validated `[work.review_watch]` settings. See [`ReviewWatchConfig`].
    pub review_watch: ReviewWatchConfig,
    /// Validated `[work.manual]` settings. See [`ManualConfig`]; empty (no
    /// windows, action disabled) when the `[work.manual]` section is absent
    /// from both global and repo config.
    pub manual: ManualConfig,
}

/// Fully validated `[work.audit]` subsection.
///
/// Unlike [`LaneConfig::repo`], `dir` is not required at merge time — an
/// absent `dir` means audit launching is disabled, which
/// [`crate::work::audit::launch_audit`] reports as a status-line error, not
/// a config-load failure (see `docs/plans/board-audits.md`'s "Launch"
/// design: "Launching without `[work.audit].dir` is a status-line error, not
/// a crash").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditConfig {
    /// See [`RawAuditConfig::dir`].
    pub dir: Option<String>,
    /// See [`RawAuditConfig::prompt`].
    pub prompt: Option<String>,
    /// See [`RawAuditConfig::model`].
    pub model: Option<String>,
}

/// Fully validated `[work.manual]` subsection.
///
/// Like [`AuditConfig::dir`], nothing here is required at merge time — an
/// empty `windows` list (or an absent `dir`) means the board's
/// manual-session action is disabled, which
/// `crate::work::manual::ensure_manual_session` reports as a status-line
/// error, not a config-load failure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManualConfig {
    /// See [`RawManualConfig::dir`].
    pub dir: Option<String>,
    /// See [`RawManualConfig::windows`]. Empty when unset in both global and
    /// repo config.
    pub windows: Vec<ManualWindow>,
}

/// Validated form of [`RawManualWindow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualWindow {
    /// See [`RawManualWindow::name`].
    pub name: String,
    /// See [`RawManualWindow::command`].
    pub command: Option<String>,
}

/// Fully validated `[work.review_watch]` subsection.
///
/// `dir` resolves through a two-step fallback: `review_watch.dir` then
/// `audit.dir`, applied once in [`merge_work`] after both subsections have
/// been merged (not in [`merge_review_watch`], which stays
/// `review_watch`-only, mirroring [`merge_audit`]'s "single section, no
/// whole-vs-field ambiguity" scoping). `poll_secs`/`max_wait_mins`/
/// `on_bots_done` always carry a concrete value — unlike `dir`/`prompt`,
/// the poll loop needs a usable number no matter what config supplies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewWatchConfig {
    /// See [`RawReviewWatchConfig::dir`]; falls back to [`AuditConfig::dir`]
    /// when unset in both global and repo `[work.review_watch]`.
    pub dir: Option<String>,
    /// See [`RawReviewWatchConfig::prompt`].
    pub prompt: Option<String>,
    /// See [`RawReviewWatchConfig::model`]; falls back to
    /// [`AuditConfig::model`] when unset in both global and repo
    /// `[work.review_watch]`.
    pub model: Option<String>,
    /// See [`RawReviewWatchConfig::poll_secs`]. Defaults to 45.
    pub poll_secs: u64,
    /// See [`RawReviewWatchConfig::max_wait_mins`]. Defaults to 1440 (24h).
    pub max_wait_mins: u64,
    /// See [`RawReviewWatchConfig::on_bots_done`]. Defaults to
    /// [`OnBotsDone::Notify`].
    pub on_bots_done: OnBotsDone,
}

impl Default for ReviewWatchConfig {
    fn default() -> Self {
        ReviewWatchConfig {
            dir: None,
            prompt: None,
            model: None,
            poll_secs: 45,
            max_wait_mins: 1440,
            on_bots_done: OnBotsDone::Notify,
        }
    }
}

/// What to do once a `tm pr watch` run finds every configured bot has
/// reviewed and left unresolved findings. See [`RawReviewWatchConfig::on_bots_done`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnBotsDone {
    /// Leave the board badge (`Review` run status) as the only signal; no
    /// cleanup session is launched automatically.
    #[default]
    Notify,
    /// Launch (or attach to) the bugbot-cleanup session automatically.
    Launch,
}

impl OnBotsDone {
    /// Returns the lowercase string stored in config for this value.
    pub fn as_str(self) -> &'static str {
        match self {
            OnBotsDone::Notify => "notify",
            OnBotsDone::Launch => "launch",
        }
    }

    /// Parses a config string. Returns `None` for unrecognized values.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "notify" => Some(OnBotsDone::Notify),
            "launch" => Some(OnBotsDone::Launch),
            _ => None,
        }
    }
}

/// Fully validated `[work.lanes.<name>]` subsection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneConfig {
    /// See [`RawLaneConfig::repo`]. Required — validated present at merge
    /// time.
    pub repo: String,
    /// See [`RawLaneConfig::prompt_file`].
    pub prompt_file: Option<String>,
    /// See [`RawLaneConfig::base_branch`].
    pub base_branch: Option<String>,
    /// See [`RawLaneConfig::model`].
    pub model: Option<String>,
    /// See [`RawLaneConfig::max_turns`].
    pub max_turns: Option<u32>,
    /// See [`RawLaneConfig::permission_mode`].
    pub permission_mode: Option<String>,
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

    /// A `[work.lanes.<name>]` entry was missing its required `repo` field
    /// after merging global and repo config.
    #[error("lane `{lane}` is missing required field `repo`; set it in [work.lanes.{lane}]")]
    MissingLaneField {
        /// Name of the lane missing the field.
        lane: String,
        /// Name of the missing field (currently always `"repo"`).
        field: &'static str,
    },

    /// `[work.review_watch].on_bots_done` was set to something other than
    /// `"notify"` or `"launch"`.
    #[error(
        "invalid [work.review_watch].on_bots_done value `{value}`; expected \"notify\" or \"launch\""
    )]
    InvalidOnBotsDone {
        /// The unrecognized value as written in config.
        value: String,
    },

    /// `[backend].provider` was set to a value that isn't a recognized
    /// provider name at all (contrast [`ConfigError::ProviderNotImplemented`],
    /// for a recognized name with no adapter yet).
    #[error("invalid [backend] provider `{value}`; expected \"jira\" or \"github\"")]
    InvalidProvider {
        /// The unrecognized value as written in config.
        value: String,
    },

    /// `[backend].provider` named a recognized provider that has no
    /// [`crate::ticketing::provider::TicketProvider`] adapter implemented
    /// yet.
    #[error("backend provider `{provider}` is not implemented yet")]
    ProviderNotImplemented {
        /// The recognized-but-unimplemented provider name.
        provider: &'static str,
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

    /// A relative (non-`~`, non-absolute) path was set for `field` by the
    /// *global* config, which has no repo directory of its own to resolve a
    /// relative path against. Only a repo-local `.tskmstr.toml` may use a
    /// relative path for a lane's `repo` or `[work.audit].dir` — see
    /// GitHub issue #5 phase 2: `docs/plans/issue-5-lane-backend-routing.md`.
    #[error(
        "relative path `{value}` for `{field}` is only allowed in a repo-local .tskmstr.toml \
         (the global config has no repo directory to resolve it against)"
    )]
    RelativePathRequiresRepoConfig {
        /// Dotted config path of the field that was set to a relative path,
        /// e.g. `work.lanes.tskmstr.repo` or `work.audit.dir`.
        field: String,
        /// The relative path as written in config.
        value: String,
    },
}

/// Resolves `raw` (a `[work.lanes.<name>].repo` or `[work.audit].dir`
/// value) against `defining_dir` when `raw` is a plain relative path
/// (neither `~`-prefixed nor absolute): `defining_dir` is the repo
/// directory of whichever config file actually set this value, `None` when
/// that was the global config, which has no repo directory of its own.
///
/// A leading `~` (see [`crate::work::naming::expand_tilde`]) and an already
/// absolute path are returned unchanged, exactly as before this function
/// existed — resolution is caller-side for those, same as every other
/// path-shaped config field's convention (see e.g.
/// [`RawWorkConfig::worktree_root`]'s doc comment).
fn resolve_repo_path(
    raw: &str,
    defining_dir: Option<&Path>,
    field: impl Into<String>,
) -> Result<String, ConfigError> {
    if raw.starts_with('~') || Path::new(raw).is_absolute() {
        return Ok(raw.to_string());
    }
    match defining_dir {
        // `raw == "."` is special-cased to avoid a literal trailing `/.`
        // component (`dir.join(".")` would otherwise produce one) — this is
        // the exact case the plan's design calls out: a repo-local
        // `.tskmstr.toml` setting `repo = "."` must resolve to that repo's
        // root, not `<root>/.`.
        Some(dir) if raw == "." => Ok(dir.to_string_lossy().into_owned()),
        Some(dir) => Ok(dir.join(raw).to_string_lossy().into_owned()),
        None => Err(ConfigError::RelativePathRequiresRepoConfig {
            field: field.into(),
            value: raw.to_string(),
        }),
    }
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
        status_on_pr: None,
        status_on_create: None,
        run_db_path: None,
        review_bots: None,
        board_column_order: None,
        work: None,
        backend: None,
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
/// when `repo` is absent entirely) fall back to `global`. Does not attempt
/// to default `[backend.github].repo` from a git checkout's origin remote —
/// that's [`load`]'s job via [`merge_with_repo_dir`], since it requires I/O
/// this pure function deliberately doesn't do. Every existing caller of
/// `merge` (including every test that constructs a `RawConfig` by hand) gets
/// exactly the same behavior as before: a `[backend.github]` with no `repo`
/// set is [`ConfigError::MissingField`], full stop.
pub fn merge(global: RawConfig, repo: Option<RawConfig>) -> Result<Config, ConfigError> {
    merge_with_repo_dir(global, repo, None)
}

/// Resolve a Jira field, preferring the canonical `[backend.jira]` location
/// over the legacy flat top-level key, within a single (not-yet-merged)
/// `RawConfig` -- callers then `.or()` the repo-level result over the
/// global-level result the same way every other field does. See the
/// carry-forward decision this implements: `[backend.jira]` is documented as
/// canonical, but the flat keys silently keep working, so a config with only
/// the flat key set behaves identically to before `[backend.jira]` existed.
fn resolved_jira_field(
    raw: &RawConfig,
    canonical: impl Fn(&RawJiraBackendConfig) -> Option<String>,
    flat: &Option<String>,
) -> Option<String> {
    raw.backend
        .as_ref()
        .and_then(|backend| backend.jira.as_ref())
        .and_then(canonical)
        .or_else(|| flat.clone())
}

/// Merge a repo-local override on top of a global config exactly like
/// [`merge`], except a `[backend.github]` with no `repo` field set (in
/// either file) is defaulted from `repo_dir`'s git `origin` remote, if one
/// is given and a remote can be resolved. [`load`] is the only caller that
/// passes `Some`; [`merge`] itself always passes `None`, so its behavior
/// (and every existing test's) is unaffected.
fn merge_with_repo_dir(
    global: RawConfig,
    repo: Option<RawConfig>,
    repo_dir: Option<&Path>,
) -> Result<Config, ConfigError> {
    let repo = repo.unwrap_or_default();

    let backend = merge_backend(global.backend.clone(), repo.backend.clone())?;

    let jira_base_url = resolved_jira_field(
        &repo,
        |jira| jira.jira_base_url.clone(),
        &repo.jira_base_url,
    )
    .or_else(|| {
        resolved_jira_field(
            &global,
            |jira| jira.jira_base_url.clone(),
            &global.jira_base_url,
        )
    });
    let jira_email = resolved_jira_field(&repo, |jira| jira.jira_email.clone(), &repo.jira_email)
        .or_else(|| {
            resolved_jira_field(&global, |jira| jira.jira_email.clone(), &global.jira_email)
        });
    let default_project_key = resolved_jira_field(
        &repo,
        |jira| jira.default_project_key.clone(),
        &repo.default_project_key,
    )
    .or_else(|| {
        resolved_jira_field(
            &global,
            |jira| jira.default_project_key.clone(),
            &global.default_project_key,
        )
    });
    let default_assignee_account_id = repo
        .default_assignee_account_id
        .clone()
        .or(global.default_assignee_account_id.clone());
    let status_on_pr = repo.status_on_pr.clone().or(global.status_on_pr.clone());
    let status_on_create = repo
        .status_on_create
        .clone()
        .or(global.status_on_create.clone());
    let run_db_path = repo.run_db_path.clone().or(global.run_db_path.clone());
    let review_bots = repo
        .review_bots
        .clone()
        .or(global.review_bots.clone())
        .unwrap_or_else(|| vec!["cursor[bot]".to_string()]);
    let board_column_order = repo
        .board_column_order
        .clone()
        .or(global.board_column_order.clone())
        .unwrap_or_default();
    let work = merge_work(global.work.clone(), repo.work.clone(), repo_dir)?;

    let expected_path = default_global_config_path();

    // The one match on provider kind: each arm validates and assembles the
    // fields that adapter needs. Adding a new adapter means adding a
    // variant to `BackendKind` and a new arm here (and nowhere else in this
    // module, or in the rest of tskmstr) — see `BackendKind`'s doc comment.
    match backend {
        BackendKind::Jira => Ok(Config {
            backend,
            jira_base_url: require_field(jira_base_url, "jira_base_url", &expected_path)?,
            jira_email: require_field(jira_email, "jira_email", &expected_path)?,
            default_project_key: require_field(
                default_project_key,
                "default_project_key",
                &expected_path,
            )?,
            github_repo: None,
            default_assignee_account_id,
            status_on_pr,
            status_on_create,
            run_db_path,
            review_bots,
            board_column_order,
            work,
        }),
        BackendKind::Github => {
            let configured_repo = repo
                .backend
                .as_ref()
                .and_then(|b| b.github.as_ref())
                .and_then(|g| g.repo.clone())
                .or_else(|| {
                    global
                        .backend
                        .as_ref()
                        .and_then(|b| b.github.as_ref())
                        .and_then(|g| g.repo.clone())
                })
                .or_else(|| repo_dir.and_then(detect_origin_repo));

            Ok(Config {
                backend,
                jira_base_url: String::new(),
                jira_email: String::new(),
                default_project_key: String::new(),
                github_repo: Some(require_field(
                    configured_repo,
                    "backend.github.repo",
                    &expected_path,
                )?),
                default_assignee_account_id,
                status_on_pr,
                status_on_create,
                run_db_path,
                review_bots,
                board_column_order,
                work,
            })
        }
    }
}

/// Merge a repo-local `[backend]` section on top of a global one, field by
/// field, then parse the resulting `provider` string (if any) into a
/// [`BackendKind`].
///
/// Absence in both global and repo config defaults to [`BackendKind::Jira`],
/// so an existing config with no `[backend]` table at all keeps working
/// unchanged. Does not check whether the resulting provider has an adapter
/// implemented yet — that's [`merge`]'s job, since it's the one place that
/// dispatches on provider kind.
fn merge_backend(
    global: Option<RawBackendConfig>,
    repo: Option<RawBackendConfig>,
) -> Result<BackendKind, ConfigError> {
    let global = global.unwrap_or_default();
    let repo = repo.unwrap_or_default();

    match repo.provider.or(global.provider) {
        Some(value) => BackendKind::parse(&value).ok_or(ConfigError::InvalidProvider { value }),
        None => Ok(BackendKind::default()),
    }
}

/// Merge a repo-local `[work]` section on top of a global one, field by
/// field, then validate that every resulting lane has a `repo`.
///
/// Lane maps merge by whole-lane replacement: a lane name present in `repo`
/// entirely replaces the global lane of the same name (see
/// [`RawWorkConfig::lanes`] for rationale), rather than being merged field by
/// field the way the top-level `[work]` scalars are.
///
/// `repo_dir`, when given, is the directory of the repo-local `.tskmstr.toml`
/// that `repo` was parsed from (see [`merge_with_repo_dir`]'s own doc
/// comment) — the "defining directory" a lane's `repo` (or
/// `[work.audit].dir`) resolves a relative path against, when that specific
/// value actually came from `repo` rather than falling back to `global` (see
/// [`resolve_repo_path`]). A value that falls back to the global config, and
/// is relative, is a [`ConfigError::RelativePathRequiresRepoConfig`]
/// regardless of `repo_dir` — the global config has no repo directory of its
/// own, no matter what directory happened to be passed in for other
/// purposes (e.g. `[backend.github].repo`'s origin-remote default).
fn merge_work(
    global: Option<RawWorkConfig>,
    repo: Option<RawWorkConfig>,
    repo_dir: Option<&Path>,
) -> Result<WorkConfig, ConfigError> {
    let global = global.unwrap_or_default();
    let repo = repo.unwrap_or_default();

    let worktree_root = repo.worktree_root.or(global.worktree_root);
    let default_model = repo.default_model.or(global.default_model);
    let default_max_turns = repo.default_max_turns.or(global.default_max_turns);
    let default_permission_mode = repo
        .default_permission_mode
        .or(global.default_permission_mode);
    let tmux_windows = repo
        .tmux_windows
        .or(global.tmux_windows)
        .unwrap_or_default();
    let tmux_primary_window = repo.tmux_primary_window.or(global.tmux_primary_window);
    let audit = merge_audit(global.audit, repo.audit.clone(), repo_dir)?;
    let manual = merge_manual(global.manual, repo.manual.clone(), repo_dir)?;
    let mut review_watch = merge_review_watch(global.review_watch, repo.review_watch)?;
    // Fallbacks applied here, not inside merge_review_watch: [work.audit] and
    // [work.review_watch] are otherwise merged independently, field by
    // field, within their own section; only `dir` and `model` reach across
    // sections, and only once both are fully merged.
    if review_watch.dir.is_none() {
        review_watch.dir = audit.dir.clone();
    }
    if review_watch.model.is_none() {
        review_watch.model = audit.model.clone();
    }

    // Captured before `repo.lanes` is consumed below: which lane names this
    // *specific* `repo` config defined, so a lane replaced wholesale from
    // `repo` resolves its relative `repo` path against `repo_dir`, while a
    // lane inherited unchanged from `global` (not present in `repo.lanes`)
    // has no defining repo directory at all — see [`resolve_repo_path`].
    let repo_lane_names: std::collections::BTreeSet<String> = repo.lanes.keys().cloned().collect();

    let mut raw_lanes = global.lanes;
    for (name, lane) in repo.lanes {
        raw_lanes.insert(name, lane);
    }

    let lanes = raw_lanes
        .into_iter()
        .map(|(name, raw)| {
            let repo_path = raw.repo.ok_or_else(|| ConfigError::MissingLaneField {
                lane: name.clone(),
                field: "repo",
            })?;
            let defining_dir = if repo_lane_names.contains(&name) {
                repo_dir
            } else {
                None
            };
            let repo_path =
                resolve_repo_path(&repo_path, defining_dir, format!("work.lanes.{name}.repo"))?;
            Ok((
                name,
                LaneConfig {
                    repo: repo_path,
                    prompt_file: raw.prompt_file,
                    base_branch: raw.base_branch,
                    model: raw.model,
                    max_turns: raw.max_turns,
                    permission_mode: raw.permission_mode,
                },
            ))
        })
        .collect::<Result<BTreeMap<String, LaneConfig>, ConfigError>>()?;

    Ok(WorkConfig {
        worktree_root,
        default_model,
        default_max_turns,
        default_permission_mode,
        tmux_windows,
        tmux_primary_window,
        lanes,
        audit,
        review_watch,
        manual,
    })
}

/// Merge a repo-local `[work.manual]` section on top of a global one.
/// `dir` merges field by field with relative-path resolution, exactly like
/// [`merge_audit`]'s `dir`; `windows` is replaced as a whole list (like
/// [`RawWorkConfig::tmux_windows`] and lanes) because element-wise merging
/// of an ordered layout has no sensible semantics.
fn merge_manual(
    global: Option<RawManualConfig>,
    repo: Option<RawManualConfig>,
    repo_dir: Option<&Path>,
) -> Result<ManualConfig, ConfigError> {
    let global = global.unwrap_or_default();
    let repo = repo.unwrap_or_default();

    let (dir, defining_dir) = match repo.dir {
        Some(dir) => (Some(dir), repo_dir),
        None => (global.dir, None),
    };
    let dir = match dir {
        Some(dir) => Some(resolve_repo_path(&dir, defining_dir, "work.manual.dir")?),
        None => None,
    };

    let windows = repo
        .windows
        .or(global.windows)
        .unwrap_or_default()
        .into_iter()
        .map(|raw| ManualWindow {
            name: raw.name,
            command: raw.command,
        })
        .collect();

    Ok(ManualConfig { dir, windows })
}

/// Merge a repo-local `[work.audit]` section on top of a global one, field by
/// field — the same scalar-merge rule as [`RawWorkConfig`]'s top-level
/// fields (`default_model`, `tmux_primary_window`, etc.), not the lane map's
/// whole-section replacement: `[work.audit]` is a single section, not a
/// keyed collection, so there's no "which entry" ambiguity for whole-section
/// replacement to resolve.
///
/// `repo_dir` is the defining directory `dir` resolves a relative path
/// against, but only when `dir` itself actually came from `repo` (not a
/// fallback to `global`) — see [`merge_work`]'s doc comment and
/// [`resolve_repo_path`].
fn merge_audit(
    global: Option<RawAuditConfig>,
    repo: Option<RawAuditConfig>,
    repo_dir: Option<&Path>,
) -> Result<AuditConfig, ConfigError> {
    let global = global.unwrap_or_default();
    let repo = repo.unwrap_or_default();

    let (dir, defining_dir) = match repo.dir {
        Some(dir) => (Some(dir), repo_dir),
        None => (global.dir, None),
    };
    let dir = match dir {
        Some(dir) => Some(resolve_repo_path(&dir, defining_dir, "work.audit.dir")?),
        None => None,
    };

    Ok(AuditConfig {
        dir,
        prompt: repo.prompt.or(global.prompt),
        model: repo.model.or(global.model),
    })
}

/// Merge a repo-local `[work.review_watch]` section on top of a global one,
/// field by field, exactly like [`merge_audit`] — same "single section, no
/// whole-vs-field ambiguity" rationale. Unlike `merge_audit`, this can fail:
/// an unrecognized `on_bots_done` value is a [`ConfigError`], not a silent
/// default, matching other enum-shaped config values' validation posture.
///
/// Does not apply the `dir`/`model`-fall-back-to-`[work.audit]` rules; see
/// [`ReviewWatchConfig::dir`], [`ReviewWatchConfig::model`], and
/// [`merge_work`], which applies them once after both subsections are merged.
fn merge_review_watch(
    global: Option<RawReviewWatchConfig>,
    repo: Option<RawReviewWatchConfig>,
) -> Result<ReviewWatchConfig, ConfigError> {
    let global = global.unwrap_or_default();
    let repo = repo.unwrap_or_default();

    let on_bots_done = match repo.on_bots_done.or(global.on_bots_done) {
        Some(value) => OnBotsDone::parse(&value).ok_or_else(|| ConfigError::InvalidOnBotsDone {
            value: value.clone(),
        })?,
        None => OnBotsDone::default(),
    };

    Ok(ReviewWatchConfig {
        dir: repo.dir.or(global.dir),
        prompt: repo.prompt.or(global.prompt),
        model: repo.model.or(global.model),
        poll_secs: repo
            .poll_secs
            .or(global.poll_secs)
            .unwrap_or(ReviewWatchConfig::default().poll_secs),
        max_wait_mins: repo
            .max_wait_mins
            .or(global.max_wait_mins)
            .unwrap_or(ReviewWatchConfig::default().max_wait_mins),
        on_bots_done,
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

/// Best-effort default for `[backend.github].repo`: run `git config --get
/// remote.origin.url` in `dir` and parse an `"owner/name"` slug out of
/// whatever URL shape it returns. `None` on any failure (not a git repo, no
/// `origin` remote, `git` not on `PATH`, an unrecognized URL host/shape) --
/// this is a convenience default, not a required capability, so [`merge`]'s
/// own [`ConfigError::MissingField`] is the fallback the caller sees when it
/// doesn't pan out.
///
/// Deliberately does not shell out to `gh repo view` (which
/// [`crate::github::gh_cli::ShellGhCli`]'s private `resolve_repo` already
/// does, for the `pr_*` methods): that requires `gh` to be authenticated,
/// which config loading shouldn't depend on just to resolve a default.
pub(crate) fn detect_origin_repo(dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout);
    parse_repo_slug_from_git_remote_url(url.trim())
}

/// Parse an `"owner/name"` slug out of a git remote URL, handling the two
/// shapes GitHub hands out: SSH (`git@github.com:owner/name.git`) and HTTPS
/// (`https://github.com/owner/name.git`, with or without the `.git` suffix).
/// `None` for anything else (a non-GitHub host, a malformed URL) rather than
/// guessing.
fn parse_repo_slug_from_git_remote_url(url: &str) -> Option<String> {
    let url = url.strip_suffix(".git").unwrap_or(url);
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        return Some(rest.to_string());
    }
    for prefix in ["https://github.com/", "http://github.com/"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            return Some(rest.to_string());
        }
    }
    None
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

    // Prefer the repo-local config file's directory as the git checkout to
    // default `[backend.github].repo` from, since that's the repo whose
    // `.tskmstr.toml` is actually selecting the GitHub backend; fall back to
    // the process's cwd when there's no repo-local config at all (a global
    // config alone can still select `provider = "github"`).
    let repo_dir = paths
        .repo
        .as_ref()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .or_else(|| std::env::current_dir().ok());

    merge_with_repo_dir(global, repo, repo_dir.as_deref()).map_err(|err| match err {
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
            status_on_pr: Some("In Review".into()),
            status_on_create: Some("In Progress".into()),
            run_db_path: Some("/global/runs.db".into()),
            review_bots: Some(vec!["cursor[bot]".into()]),
            board_column_order: Some(vec!["To Do".into(), "In Progress".into()]),
            work: None,
            backend: None,
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
            status_on_pr: None,
            status_on_create: None,
            run_db_path: None,
            review_bots: None,
            board_column_order: None,
            work: None,
            backend: None,
        };
        let cfg = merge(raw_full(), Some(repo)).expect("should merge");
        // Overridden field wins.
        assert_eq!(cfg.default_project_key, "REPO");
        // Non-overridden fields fall back to global.
        assert_eq!(cfg.jira_base_url, "https://global.atlassian.net");
        assert_eq!(cfg.jira_email, "global@example.com");
        assert_eq!(cfg.default_assignee_account_id, Some("acct-global".into()));
        assert_eq!(cfg.status_on_pr, Some("In Review".into()));
        assert_eq!(cfg.status_on_create, Some("In Progress".into()));
    }

    #[test]
    fn merge_repo_overrides_status_on_pr() {
        let repo = RawConfig {
            jira_base_url: None,
            jira_email: None,
            default_project_key: None,
            default_assignee_account_id: None,
            status_on_pr: Some("Ready for Review".into()),
            status_on_create: None,
            run_db_path: None,
            review_bots: None,
            board_column_order: None,
            work: None,
            backend: None,
        };
        let cfg = merge(raw_full(), Some(repo)).expect("should merge");
        assert_eq!(cfg.status_on_pr, Some("Ready for Review".into()));
    }

    #[test]
    fn merge_status_on_pr_absent_from_both_is_none() {
        let global = RawConfig {
            status_on_pr: None,
            ..raw_full()
        };
        let cfg = merge(global, None).expect("should merge");
        assert_eq!(cfg.status_on_pr, None);
    }

    #[test]
    fn merge_repo_overrides_status_on_create() {
        let repo = RawConfig {
            jira_base_url: None,
            jira_email: None,
            default_project_key: None,
            default_assignee_account_id: None,
            status_on_pr: None,
            status_on_create: Some("In Progress".into()),
            run_db_path: None,
            review_bots: None,
            board_column_order: None,
            work: None,
            backend: None,
        };
        let cfg = merge(raw_full(), Some(repo)).expect("should merge");
        assert_eq!(cfg.status_on_create, Some("In Progress".into()));
    }

    #[test]
    fn merge_status_on_create_absent_from_both_is_none() {
        let global = RawConfig {
            status_on_create: None,
            ..raw_full()
        };
        let cfg = merge(global, None).expect("should merge");
        assert_eq!(cfg.status_on_create, None);
    }

    #[test]
    fn merge_repo_overrides_run_db_path() {
        let repo = RawConfig {
            jira_base_url: None,
            jira_email: None,
            default_project_key: None,
            default_assignee_account_id: None,
            status_on_pr: None,
            status_on_create: None,
            run_db_path: Some("/repo/runs.db".into()),
            review_bots: None,
            board_column_order: None,
            work: None,
            backend: None,
        };
        let cfg = merge(raw_full(), Some(repo)).expect("should merge");
        assert_eq!(cfg.run_db_path, Some("/repo/runs.db".into()));
    }

    #[test]
    fn merge_run_db_path_absent_from_both_is_none() {
        let global = RawConfig {
            run_db_path: None,
            ..raw_full()
        };
        let cfg = merge(global, None).expect("should merge");
        assert_eq!(cfg.run_db_path, None);
    }

    #[test]
    fn merge_repo_overrides_review_bots() {
        let repo = RawConfig {
            jira_base_url: None,
            jira_email: None,
            default_project_key: None,
            default_assignee_account_id: None,
            status_on_pr: None,
            status_on_create: None,
            run_db_path: None,
            review_bots: Some(vec!["repo-bot[bot]".into()]),
            board_column_order: None,
            work: None,
            backend: None,
        };
        let cfg = merge(raw_full(), Some(repo)).expect("should merge");
        assert_eq!(cfg.review_bots, vec!["repo-bot[bot]".to_string()]);
    }

    #[test]
    fn merge_review_bots_absent_from_both_defaults_to_cursor_bot() {
        let global = RawConfig {
            review_bots: None,
            ..raw_full()
        };
        let cfg = merge(global, None).expect("should merge");
        assert_eq!(cfg.review_bots, vec!["cursor[bot]".to_string()]);
    }

    #[test]
    fn merge_repo_overrides_board_column_order() {
        let repo = RawConfig {
            jira_base_url: None,
            jira_email: None,
            default_project_key: None,
            default_assignee_account_id: None,
            status_on_pr: None,
            status_on_create: None,
            run_db_path: None,
            review_bots: None,
            board_column_order: Some(vec!["Code Review".into()]),
            work: None,
            backend: None,
        };
        let cfg = merge(raw_full(), Some(repo)).expect("should merge");
        assert_eq!(cfg.board_column_order, vec!["Code Review".to_string()]);
    }

    #[test]
    fn merge_board_column_order_absent_from_both_is_empty() {
        let global = RawConfig {
            board_column_order: None,
            ..raw_full()
        };
        let cfg = merge(global, None).expect("should merge");
        assert_eq!(cfg.board_column_order, Vec::<String>::new());
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
    fn load_global_with_status_on_pr_parses_field() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://only-global.atlassian.net"
            jira_email = "only-global@example.com"
            default_project_key = "ONLY"
            status_on_pr = "In Review"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.status_on_pr, Some("In Review".to_string()));
    }

    #[test]
    fn load_global_without_status_on_pr_is_none() {
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
        assert_eq!(cfg.status_on_pr, None);
    }

    #[test]
    fn load_repo_overrides_status_on_pr() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"
            status_on_pr = "In Review"
            "#,
        )
        .unwrap();

        let repo_path = dir.path().join(".tskmstr.toml");
        fs::write(&repo_path, r#"status_on_pr = "Ready for Review""#).unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.status_on_pr, Some("Ready for Review".to_string()));
    }

    #[test]
    fn load_global_with_status_on_create_parses_field() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://only-global.atlassian.net"
            jira_email = "only-global@example.com"
            default_project_key = "ONLY"
            status_on_create = "In Progress"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.status_on_create, Some("In Progress".to_string()));
    }

    #[test]
    fn load_global_without_status_on_create_is_none() {
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
        assert_eq!(cfg.status_on_create, None);
    }

    #[test]
    fn load_repo_overrides_status_on_create() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"
            status_on_create = "In Progress"
            "#,
        )
        .unwrap();

        let repo_path = dir.path().join(".tskmstr.toml");
        fs::write(&repo_path, r#"status_on_create = "Doing""#).unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.status_on_create, Some("Doing".to_string()));
    }

    #[test]
    fn load_global_with_run_db_path_parses_field() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://only-global.atlassian.net"
            jira_email = "only-global@example.com"
            default_project_key = "ONLY"
            run_db_path = "/global/runs.db"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.run_db_path, Some("/global/runs.db".to_string()));
    }

    #[test]
    fn load_global_without_run_db_path_is_none() {
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
        assert_eq!(cfg.run_db_path, None);
    }

    #[test]
    fn load_repo_overrides_run_db_path() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"
            run_db_path = "/global/runs.db"
            "#,
        )
        .unwrap();

        let repo_path = dir.path().join(".tskmstr.toml");
        fs::write(&repo_path, r#"run_db_path = "/repo/runs.db""#).unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.run_db_path, Some("/repo/runs.db".to_string()));
    }

    #[test]
    fn load_global_with_review_bots_parses_field() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://only-global.atlassian.net"
            jira_email = "only-global@example.com"
            default_project_key = "ONLY"
            review_bots = ["cursor[bot]", "other[bot]"]
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(
            cfg.review_bots,
            vec!["cursor[bot]".to_string(), "other[bot]".to_string()]
        );
    }

    #[test]
    fn load_global_without_review_bots_defaults_to_cursor_bot() {
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
        assert_eq!(cfg.review_bots, vec!["cursor[bot]".to_string()]);
    }

    #[test]
    fn load_repo_overrides_review_bots() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"
            review_bots = ["cursor[bot]"]
            "#,
        )
        .unwrap();

        let repo_path = dir.path().join(".tskmstr.toml");
        fs::write(&repo_path, r#"review_bots = ["repo-bot[bot]"]"#).unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.review_bots, vec!["repo-bot[bot]".to_string()]);
    }

    #[test]
    fn load_global_with_board_column_order_parses_field() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://only-global.atlassian.net"
            jira_email = "only-global@example.com"
            default_project_key = "ONLY"
            board_column_order = ["To Do", "In Progress", "Code Review"]
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(
            cfg.board_column_order,
            vec![
                "To Do".to_string(),
                "In Progress".to_string(),
                "Code Review".to_string()
            ]
        );
    }

    #[test]
    fn load_global_without_board_column_order_is_empty() {
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
        assert_eq!(cfg.board_column_order, Vec::<String>::new());
    }

    #[test]
    fn load_repo_overrides_board_column_order() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"
            board_column_order = ["To Do", "In Progress"]
            "#,
        )
        .unwrap();

        let repo_path = dir.path().join(".tskmstr.toml");
        fs::write(&repo_path, r#"board_column_order = ["Code Review"]"#).unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.board_column_order, vec!["Code Review".to_string()]);
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
            jira_base_url: "https://example.atlassian.net".to_string(),
            jira_email: "dev@example.com".to_string(),
            default_project_key: "PROJ".to_string(),
        };

        write_global(&path, &seed).expect("should write");

        let paths = ConfigPaths {
            global: path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load written config");
        assert_eq!(cfg.jira_base_url, "https://example.atlassian.net");
        assert_eq!(cfg.jira_email, "dev@example.com");
        assert_eq!(cfg.default_project_key, "PROJ");
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
            jira_base_url: "https://example.atlassian.net".to_string(),
            jira_email: "dev@example.com".to_string(),
            default_project_key: "PROJ".to_string(),
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
        assert_eq!(cfg.jira_base_url, "https://example.atlassian.net");
        assert_eq!(cfg.default_project_key, "PROJ");
    }

    #[test]
    fn set_assignee_account_id_missing_file_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");

        let err = set_assignee_account_id(&path, "acct-123").expect_err("should fail");
        assert!(matches!(err, ConfigError::ReadGlobal { .. }));
    }

    // --- `[backend]` section ---

    #[test]
    fn merge_backend_absent_from_both_defaults_to_jira() {
        let cfg = merge(raw_full(), None).expect("should merge");
        assert_eq!(cfg.backend, BackendKind::Jira);
    }

    #[test]
    fn merge_backend_provider_jira_explicit_succeeds() {
        let global = RawConfig {
            backend: Some(RawBackendConfig {
                provider: Some("jira".to_string()),
                ..Default::default()
            }),
            ..raw_full()
        };
        let cfg = merge(global, None).expect("should merge");
        assert_eq!(cfg.backend, BackendKind::Jira);
    }

    #[test]
    fn merge_backend_invalid_provider_errors() {
        let global = RawConfig {
            backend: Some(RawBackendConfig {
                provider: Some("bogus".to_string()),
                ..Default::default()
            }),
            ..raw_full()
        };
        let err = merge(global, None).expect_err("should fail");
        match err {
            ConfigError::InvalidProvider { value } => assert_eq!(value, "bogus"),
            other => panic!("expected InvalidProvider, got {other:?}"),
        }
    }

    #[test]
    fn merge_backend_github_provider_without_repo_is_missing_field() {
        let global = RawConfig {
            backend: Some(RawBackendConfig {
                provider: Some("github".to_string()),
                ..Default::default()
            }),
            ..raw_full()
        };
        let err = merge(global, None).expect_err("should fail");
        match err {
            ConfigError::MissingField { field, .. } => {
                assert_eq!(field, "backend.github.repo")
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn merge_backend_github_provider_does_not_require_jira_fields() {
        // Even with every Jira field absent, selecting `github` (with a
        // `repo` configured) succeeds -- demonstrating those fields are
        // validated only under the Jira adapter, not unconditionally.
        let global = RawConfig {
            jira_base_url: None,
            jira_email: None,
            default_project_key: None,
            backend: Some(RawBackendConfig {
                provider: Some("github".to_string()),
                github: Some(RawGithubBackendConfig {
                    repo: Some("jowi-dev/tskmstr".to_string()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = merge(global, None).expect("should succeed");
        assert_eq!(cfg.backend, BackendKind::Github);
        assert_eq!(cfg.github_repo, Some("jowi-dev/tskmstr".to_string()));
        assert_eq!(cfg.jira_base_url, "");
    }

    #[test]
    fn merge_backend_github_provider_with_repo_configured_succeeds() {
        let global = RawConfig {
            backend: Some(RawBackendConfig {
                provider: Some("github".to_string()),
                github: Some(RawGithubBackendConfig {
                    repo: Some("jowi-dev/tskmstr".to_string()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = merge(global, None).expect("should succeed");
        assert_eq!(cfg.github_repo, Some("jowi-dev/tskmstr".to_string()));
    }

    #[test]
    fn merge_backend_github_repo_from_repo_local_overrides_global() {
        let global = RawConfig {
            backend: Some(RawBackendConfig {
                provider: Some("github".to_string()),
                github: Some(RawGithubBackendConfig {
                    repo: Some("global-owner/global-repo".to_string()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let repo = RawConfig {
            backend: Some(RawBackendConfig {
                github: Some(RawGithubBackendConfig {
                    repo: Some("repo-owner/repo-repo".to_string()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = merge(global, Some(repo)).expect("should succeed");
        assert_eq!(cfg.github_repo, Some("repo-owner/repo-repo".to_string()));
    }

    #[test]
    fn merge_with_repo_dir_defaults_github_repo_from_origin_remote() {
        let dir = tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "git@github.com:jowi-dev/tskmstr.git",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();

        let global = RawConfig {
            backend: Some(RawBackendConfig {
                provider: Some("github".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = merge_with_repo_dir(global, None, Some(dir.path())).expect("should succeed");
        assert_eq!(cfg.github_repo, Some("jowi-dev/tskmstr".to_string()));
    }

    #[test]
    fn merge_without_repo_dir_does_not_default_github_repo() {
        // `merge` itself (as opposed to `merge_with_repo_dir`) never shells
        // out to git, so a config with no explicit repo still fails cleanly
        // even if the test happens to run inside a git checkout.
        let global = RawConfig {
            backend: Some(RawBackendConfig {
                provider: Some("github".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = merge(global, None).expect_err("should fail");
        assert!(matches!(err, ConfigError::MissingField { .. }));
    }

    #[test]
    fn parse_repo_slug_from_git_remote_url_handles_ssh_and_https() {
        assert_eq!(
            parse_repo_slug_from_git_remote_url("git@github.com:jowi-dev/tskmstr.git"),
            Some("jowi-dev/tskmstr".to_string())
        );
        assert_eq!(
            parse_repo_slug_from_git_remote_url("https://github.com/jowi-dev/tskmstr.git"),
            Some("jowi-dev/tskmstr".to_string())
        );
        assert_eq!(
            parse_repo_slug_from_git_remote_url("https://github.com/jowi-dev/tskmstr"),
            Some("jowi-dev/tskmstr".to_string())
        );
        assert_eq!(
            parse_repo_slug_from_git_remote_url("https://gitlab.com/jowi-dev/tskmstr.git"),
            None
        );
    }

    #[test]
    fn merge_backend_jira_provider_still_requires_jira_fields() {
        let global = RawConfig {
            jira_base_url: None,
            backend: Some(RawBackendConfig {
                provider: Some("jira".to_string()),
                ..Default::default()
            }),
            ..raw_full()
        };
        let err = merge(global, None).expect_err("should fail");
        assert!(matches!(
            err,
            ConfigError::MissingField {
                field: "jira_base_url",
                ..
            }
        ));
    }

    #[test]
    fn merge_backend_repo_overrides_global_provider() {
        let global = RawConfig {
            backend: Some(RawBackendConfig {
                provider: Some("jira".to_string()),
                ..Default::default()
            }),
            ..raw_full()
        };
        let repo = RawConfig {
            backend: Some(RawBackendConfig {
                provider: Some("github".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        // The repo override does take effect (selecting github over the
        // global jira setting) -- it just then fails on github's own
        // required field, `repo`, which this repo-local override doesn't
        // set either, rather than falling back to jira's validation.
        let err = merge(global, Some(repo)).expect_err("repo override should take effect");
        assert!(matches!(
            err,
            ConfigError::MissingField { field, .. } if field == "backend.github.repo"
        ));
    }

    #[test]
    fn merge_backend_repo_provider_absent_falls_back_to_global() {
        let global = RawConfig {
            backend: Some(RawBackendConfig {
                provider: Some("jira".to_string()),
                ..Default::default()
            }),
            ..raw_full()
        };
        let repo = RawConfig {
            default_project_key: Some("REPO".to_string()),
            ..Default::default()
        };
        let cfg = merge(global, Some(repo)).expect("should merge");
        assert_eq!(cfg.backend, BackendKind::Jira);
    }

    #[test]
    fn load_repo_local_tskmstr_toml_can_select_backend_provider() {
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
        fs::write(
            &repo_path,
            r#"
            [backend]
            provider = "github"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        // The tempdir has no `origin` remote to default `repo` from (it
        // isn't even a git checkout), so this fails on the missing
        // `backend.github.repo` field rather than an unimplemented-provider
        // error -- the provider itself is now implemented.
        let err = load(&paths).expect_err("github backend without a repo should fail cleanly");
        assert!(matches!(
            err,
            ConfigError::MissingField { field, .. } if field == "backend.github.repo"
        ));
    }

    #[test]
    fn load_repo_local_tskmstr_toml_can_select_github_backend_with_explicit_repo() {
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
        fs::write(
            &repo_path,
            r#"
            [backend]
            provider = "github"

            [backend.github]
            repo = "jowi-dev/tskmstr"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        let cfg = load(&paths).expect("should succeed");
        assert_eq!(cfg.backend, BackendKind::Github);
        assert_eq!(cfg.github_repo, Some("jowi-dev/tskmstr".to_string()));
    }

    #[test]
    fn load_repo_local_tskmstr_toml_jira_fields_under_backend_jira_table() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            [backend.jira]
            jira_base_url = "https://example.atlassian.net"
            jira_email = "dev@example.com"
            default_project_key = "PROJ"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let cfg = load(&paths).expect("should succeed reading canonical [backend.jira] fields");
        assert_eq!(cfg.jira_base_url, "https://example.atlassian.net");
        assert_eq!(cfg.jira_email, "dev@example.com");
        assert_eq!(cfg.default_project_key, "PROJ");
    }

    #[test]
    fn backend_jira_table_wins_over_legacy_flat_key_in_the_same_file() {
        let global = RawConfig {
            jira_base_url: Some("https://flat.atlassian.net".to_string()),
            backend: Some(RawBackendConfig {
                jira: Some(RawJiraBackendConfig {
                    jira_base_url: Some("https://canonical.atlassian.net".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..raw_full()
        };
        let cfg = merge(global, None).expect("should succeed");
        assert_eq!(cfg.jira_base_url, "https://canonical.atlassian.net");
    }

    #[test]
    fn backend_kind_parse_and_as_str_round_trip() {
        assert_eq!(BackendKind::parse("jira"), Some(BackendKind::Jira));
        assert_eq!(BackendKind::parse("github"), Some(BackendKind::Github));
        assert_eq!(BackendKind::parse("nonsense"), None);
        assert_eq!(BackendKind::Jira.as_str(), "jira");
        assert_eq!(BackendKind::Github.as_str(), "github");
    }

    #[test]
    fn backend_kind_default_is_jira() {
        assert_eq!(BackendKind::default(), BackendKind::Jira);
    }

    // --- `[work]` section ---

    #[test]
    fn merge_work_absent_from_both_produces_empty_defaults() {
        let cfg = merge(raw_full(), None).expect("should merge");
        assert_eq!(cfg.work.worktree_root, None);
        assert_eq!(cfg.work.default_model, None);
        assert_eq!(cfg.work.default_max_turns, None);
        assert_eq!(cfg.work.default_permission_mode, None);
        assert_eq!(cfg.work.tmux_windows, Vec::<String>::new());
        assert_eq!(cfg.work.tmux_primary_window, None);
        assert!(cfg.work.lanes.is_empty());
        assert_eq!(cfg.work.audit, AuditConfig::default());
    }

    #[test]
    fn load_full_work_section_parses_fields_and_lane() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"

            [work]
            worktree_root = "~/Worktrees"
            default_model = "fable"
            default_max_turns = 200
            default_permission_mode = "acceptEdits"
            tmux_windows = ["shell"]
            tmux_primary_window = "code"

            [work.audit]
            dir = "~/Projects/axiom"
            prompt = "/ticket-audit {key}"
            model = "opus"

            [work.lanes.partner-integrations]
            repo = "/Users/jowi/Projects/axiom"
            prompt_file = "~/.claude/prompts/partner-integrations.md"
            base_branch = "staging"
            model = "sonnet"
            max_turns = 300
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load");

        assert_eq!(cfg.work.worktree_root, Some("~/Worktrees".to_string()));
        assert_eq!(cfg.work.default_model, Some("fable".to_string()));
        assert_eq!(cfg.work.default_max_turns, Some(200));
        assert_eq!(
            cfg.work.default_permission_mode,
            Some("acceptEdits".to_string())
        );
        assert_eq!(cfg.work.tmux_windows, vec!["shell".to_string()]);
        assert_eq!(cfg.work.tmux_primary_window, Some("code".to_string()));
        assert_eq!(cfg.work.audit.dir, Some("~/Projects/axiom".to_string()));
        assert_eq!(
            cfg.work.audit.prompt,
            Some("/ticket-audit {key}".to_string())
        );
        assert_eq!(cfg.work.audit.model, Some("opus".to_string()));

        let lane = cfg
            .work
            .lanes
            .get("partner-integrations")
            .expect("lane should be present");
        assert_eq!(lane.repo, "/Users/jowi/Projects/axiom");
        assert_eq!(
            lane.prompt_file,
            Some("~/.claude/prompts/partner-integrations.md".to_string())
        );
        assert_eq!(lane.base_branch, Some("staging".to_string()));
        assert_eq!(lane.model, Some("sonnet".to_string()));
        assert_eq!(lane.max_turns, Some(300));
        assert_eq!(lane.permission_mode, None);
    }

    #[test]
    fn load_full_work_review_watch_section_parses_all_fields() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"

            [work.review_watch]
            dir = "~/Projects/axiom"
            prompt = "/bugbot-triage {key} {findings_file}"
            poll_secs = 30
            max_wait_mins = 600
            on_bots_done = "launch"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load");

        assert_eq!(
            cfg.work.review_watch.dir,
            Some("~/Projects/axiom".to_string())
        );
        assert_eq!(
            cfg.work.review_watch.prompt,
            Some("/bugbot-triage {key} {findings_file}".to_string())
        );
        assert_eq!(cfg.work.review_watch.poll_secs, 30);
        assert_eq!(cfg.work.review_watch.max_wait_mins, 600);
        assert_eq!(cfg.work.review_watch.on_bots_done, OnBotsDone::Launch);
    }

    #[test]
    fn load_review_watch_section_absent_uses_defaults() {
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
            repo: None,
        };
        let cfg = load(&paths).expect("should load");

        assert_eq!(cfg.work.review_watch, ReviewWatchConfig::default());
        assert_eq!(cfg.work.review_watch.poll_secs, 45);
        assert_eq!(cfg.work.review_watch.max_wait_mins, 1440);
        assert_eq!(cfg.work.review_watch.on_bots_done, OnBotsDone::Notify);
        assert_eq!(cfg.work.review_watch.prompt, None);
        assert_eq!(cfg.work.review_watch.dir, None);
    }

    #[test]
    fn load_review_watch_bad_on_bots_done_is_a_config_error() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"

            [work.review_watch]
            on_bots_done = "sometimes"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let err = load(&paths).expect_err("should reject bad on_bots_done value");
        assert!(matches!(err, ConfigError::InvalidOnBotsDone { .. }));
    }

    #[test]
    fn lane_with_repo_present_is_valid() {
        let mut lanes = BTreeMap::new();
        lanes.insert(
            "solo".to_string(),
            RawLaneConfig {
                repo: Some("/repo/solo".to_string()),
                ..Default::default()
            },
        );
        let global = RawWorkConfig {
            lanes,
            ..Default::default()
        };
        let cfg = merge_work(Some(global), None, None).expect("lane with repo should be valid");
        assert_eq!(cfg.lanes.get("solo").unwrap().repo, "/repo/solo");
    }

    #[test]
    fn lane_missing_repo_errors() {
        let mut lanes = BTreeMap::new();
        lanes.insert("solo".to_string(), RawLaneConfig::default());
        let global = RawWorkConfig {
            lanes,
            ..Default::default()
        };
        let err = merge_work(Some(global), None, None).expect_err("lane without repo should fail");
        match err {
            ConfigError::MissingLaneField { lane, field } => {
                assert_eq!(lane, "solo");
                assert_eq!(field, "repo");
            }
            other => panic!("expected MissingLaneField, got {other:?}"),
        }
    }

    #[test]
    fn lane_missing_repo_error_message_names_lane() {
        let mut lanes = BTreeMap::new();
        lanes.insert("solo".to_string(), RawLaneConfig::default());
        let global = RawWorkConfig {
            lanes,
            ..Default::default()
        };
        let err = merge_work(Some(global), None, None).expect_err("lane without repo should fail");
        let message = err.to_string();
        assert!(
            message.contains("solo"),
            "error should name the lane: {message}"
        );
        assert!(
            message.contains("repo"),
            "error should name the missing field: {message}"
        );
    }

    #[test]
    fn merge_work_repo_overrides_global_scalar_fields() {
        let global = RawWorkConfig {
            worktree_root: Some("~/Worktrees".to_string()),
            default_model: Some("fable".to_string()),
            default_max_turns: Some(200),
            default_permission_mode: Some("acceptEdits".to_string()),
            tmux_windows: Some(vec!["shell".to_string()]),
            tmux_primary_window: Some("code".to_string()),
            lanes: BTreeMap::new(),
            audit: None,
            review_watch: None,
            manual: None,
        };
        let repo = RawWorkConfig {
            worktree_root: Some("/repo/worktrees".to_string()),
            default_model: None,
            default_max_turns: None,
            default_permission_mode: None,
            tmux_windows: None,
            tmux_primary_window: None,
            lanes: BTreeMap::new(),
            audit: None,
            review_watch: None,
            manual: None,
        };
        let cfg = merge_work(Some(global), Some(repo), None).expect("should merge");
        // Overridden field wins.
        assert_eq!(cfg.worktree_root, Some("/repo/worktrees".to_string()));
        // Non-overridden fields fall back to global.
        assert_eq!(cfg.default_model, Some("fable".to_string()));
        assert_eq!(cfg.default_max_turns, Some(200));
        assert_eq!(cfg.default_permission_mode, Some("acceptEdits".to_string()));
        assert_eq!(cfg.tmux_windows, vec!["shell".to_string()]);
        assert_eq!(cfg.tmux_primary_window, Some("code".to_string()));
    }

    #[test]
    fn merge_work_repo_overrides_audit_dir_field_by_field() {
        let global = RawWorkConfig {
            audit: Some(RawAuditConfig {
                dir: Some("~/Projects/axiom".to_string()),
                prompt: Some("/global-audit {key}".to_string()),
                model: Some("opus".to_string()),
            }),
            ..Default::default()
        };
        let repo = RawWorkConfig {
            audit: Some(RawAuditConfig {
                dir: Some("/repo-local/axiom".to_string()),
                prompt: None,
                model: None,
            }),
            ..Default::default()
        };
        let cfg = merge_work(Some(global), Some(repo), None).expect("should merge");
        // Overridden field wins.
        assert_eq!(cfg.audit.dir, Some("/repo-local/axiom".to_string()));
        // Non-overridden fields fall back to global.
        assert_eq!(cfg.audit.prompt, Some("/global-audit {key}".to_string()));
        assert_eq!(cfg.audit.model, Some("opus".to_string()));
    }

    #[test]
    fn merge_work_audit_absent_from_both_is_default() {
        let cfg = merge_work(None, None, None).expect("should merge");
        assert_eq!(cfg.audit, AuditConfig::default());
    }

    #[test]
    fn load_full_work_manual_section_parses_dir_and_windows() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"

            [work.manual]
            dir = "~/Projects/axiom"
            windows = [
                { name = "code", command = "nvim" },
                { name = "fish" },
                { name = "claude", command = "claude" },
                { name = "server", command = "make server" },
            ]
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let cfg = load(&paths).expect("should load");

        assert_eq!(cfg.work.manual.dir, Some("~/Projects/axiom".to_string()));
        assert_eq!(
            cfg.work.manual.windows,
            vec![
                ManualWindow {
                    name: "code".to_string(),
                    command: Some("nvim".to_string()),
                },
                ManualWindow {
                    name: "fish".to_string(),
                    command: None,
                },
                ManualWindow {
                    name: "claude".to_string(),
                    command: Some("claude".to_string()),
                },
                ManualWindow {
                    name: "server".to_string(),
                    command: Some("make server".to_string()),
                },
            ]
        );
    }

    #[test]
    fn merge_work_manual_absent_from_both_is_default() {
        let cfg = merge_work(None, None, None).expect("should merge");
        assert_eq!(cfg.manual, ManualConfig::default());
        assert!(cfg.manual.windows.is_empty());
        assert_eq!(cfg.manual.dir, None);
    }

    #[test]
    fn merge_work_manual_dir_merges_field_by_field_windows_replace_wholesale() {
        let global = RawWorkConfig {
            manual: Some(RawManualConfig {
                dir: Some("~/Projects/axiom".to_string()),
                windows: Some(vec![
                    RawManualWindow {
                        name: "code".to_string(),
                        command: Some("nvim".to_string()),
                    },
                    RawManualWindow {
                        name: "fish".to_string(),
                        command: None,
                    },
                ]),
            }),
            ..Default::default()
        };
        let repo = RawWorkConfig {
            manual: Some(RawManualConfig {
                dir: None,
                windows: Some(vec![RawManualWindow {
                    name: "server".to_string(),
                    command: Some("make server".to_string()),
                }]),
            }),
            ..Default::default()
        };
        let cfg = merge_work(Some(global), Some(repo), None).expect("should merge");
        // dir falls back to global, field by field.
        assert_eq!(cfg.manual.dir, Some("~/Projects/axiom".to_string()));
        // windows are replaced as a whole list, not appended.
        assert_eq!(
            cfg.manual.windows,
            vec![ManualWindow {
                name: "server".to_string(),
                command: Some("make server".to_string()),
            }]
        );
    }

    #[test]
    fn merge_work_manual_relative_dir_resolves_against_repo_dir() {
        let repo = RawWorkConfig {
            manual: Some(RawManualConfig {
                dir: Some("subdir".to_string()),
                windows: None,
            }),
            ..Default::default()
        };
        let cfg = merge_work(None, Some(repo), Some(Path::new("/repo/root"))).expect("should merge");
        assert_eq!(cfg.manual.dir, Some("/repo/root/subdir".to_string()));
    }

    #[test]
    fn merge_work_repo_overrides_review_watch_fields_field_by_field() {
        let global = RawWorkConfig {
            review_watch: Some(RawReviewWatchConfig {
                dir: Some("~/Projects/axiom".to_string()),
                prompt: Some("/global-bugbot-triage {key} {findings_file}".to_string()),
                model: Some("fable".to_string()),
                poll_secs: Some(30),
                max_wait_mins: Some(600),
                on_bots_done: Some("launch".to_string()),
            }),
            ..Default::default()
        };
        let repo = RawWorkConfig {
            review_watch: Some(RawReviewWatchConfig {
                dir: Some("/repo-local/axiom".to_string()),
                prompt: None,
                model: None,
                poll_secs: None,
                max_wait_mins: Some(120),
                on_bots_done: None,
            }),
            ..Default::default()
        };
        let cfg = merge_work(Some(global), Some(repo), None).expect("should merge");
        // Overridden fields win.
        assert_eq!(cfg.review_watch.dir, Some("/repo-local/axiom".to_string()));
        assert_eq!(cfg.review_watch.max_wait_mins, 120);
        // Non-overridden fields fall back to global.
        assert_eq!(
            cfg.review_watch.prompt,
            Some("/global-bugbot-triage {key} {findings_file}".to_string())
        );
        assert_eq!(cfg.review_watch.poll_secs, 30);
        assert_eq!(cfg.review_watch.model, Some("fable".to_string()));
        assert_eq!(cfg.review_watch.on_bots_done, OnBotsDone::Launch);
    }

    #[test]
    fn merge_work_review_watch_absent_from_both_is_default() {
        let cfg = merge_work(None, None, None).expect("should merge");
        assert_eq!(cfg.review_watch, ReviewWatchConfig::default());
    }

    #[test]
    fn merge_work_review_watch_dir_falls_back_to_audit_dir_when_unset() {
        let global = RawWorkConfig {
            audit: Some(RawAuditConfig {
                dir: Some("~/Projects/axiom".to_string()),
                prompt: None,
                model: None,
            }),
            ..Default::default()
        };
        let cfg = merge_work(Some(global), None, None).expect("should merge");
        assert_eq!(
            cfg.review_watch.dir,
            Some("~/Projects/axiom".to_string()),
            "review_watch.dir should fall back to audit.dir when unset"
        );
    }

    #[test]
    fn merge_work_review_watch_dir_set_does_not_fall_back_to_audit_dir() {
        let global = RawWorkConfig {
            audit: Some(RawAuditConfig {
                dir: Some("~/Projects/axiom".to_string()),
                prompt: None,
                model: None,
            }),
            review_watch: Some(RawReviewWatchConfig {
                dir: Some("~/Projects/other".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = merge_work(Some(global), None, None).expect("should merge");
        assert_eq!(cfg.review_watch.dir, Some("~/Projects/other".to_string()));
    }

    #[test]
    fn merge_work_review_watch_model_falls_back_to_audit_model_when_unset() {
        let global = RawWorkConfig {
            audit: Some(RawAuditConfig {
                dir: Some("~/Projects/axiom".to_string()),
                prompt: None,
                model: Some("fable".to_string()),
            }),
            ..Default::default()
        };
        let cfg = merge_work(Some(global), None, None).expect("should merge");
        assert_eq!(
            cfg.review_watch.model,
            Some("fable".to_string()),
            "review_watch.model should fall back to audit.model when unset"
        );
    }

    #[test]
    fn merge_work_review_watch_model_set_does_not_fall_back_to_audit_model() {
        let global = RawWorkConfig {
            audit: Some(RawAuditConfig {
                dir: Some("~/Projects/axiom".to_string()),
                prompt: None,
                model: Some("fable".to_string()),
            }),
            review_watch: Some(RawReviewWatchConfig {
                model: Some("sonnet".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = merge_work(Some(global), None, None).expect("should merge");
        assert_eq!(cfg.review_watch.model, Some("sonnet".to_string()));
    }

    #[test]
    fn merge_work_review_watch_bad_on_bots_done_is_a_config_error() {
        let global = RawWorkConfig {
            review_watch: Some(RawReviewWatchConfig {
                on_bots_done: Some("sometimes".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = merge_work(Some(global), None, None).expect_err("should reject bad value");
        match err {
            ConfigError::InvalidOnBotsDone { value } => assert_eq!(value, "sometimes"),
            other => panic!("expected InvalidOnBotsDone, got {other:?}"),
        }
    }

    #[test]
    fn merge_work_repo_lane_replaces_global_lane_whole() {
        let mut global_lanes = BTreeMap::new();
        global_lanes.insert(
            "partner-integrations".to_string(),
            RawLaneConfig {
                repo: Some("/global/axiom".to_string()),
                base_branch: Some("main".to_string()),
                model: Some("fable".to_string()),
                ..Default::default()
            },
        );
        let global = RawWorkConfig {
            lanes: global_lanes,
            ..Default::default()
        };

        // The repo-local lane sets only `repo`; because lanes replace as a
        // whole rather than merging field by field, `base_branch`/`model`
        // from the global lane must NOT leak through.
        let mut repo_lanes = BTreeMap::new();
        repo_lanes.insert(
            "partner-integrations".to_string(),
            RawLaneConfig {
                repo: Some("/repo-local/axiom".to_string()),
                ..Default::default()
            },
        );
        let repo = RawWorkConfig {
            lanes: repo_lanes,
            ..Default::default()
        };

        let cfg = merge_work(Some(global), Some(repo), None).expect("should merge");
        let lane = cfg.lanes.get("partner-integrations").unwrap();
        assert_eq!(lane.repo, "/repo-local/axiom");
        assert_eq!(lane.base_branch, None);
        assert_eq!(lane.model, None);
    }

    #[test]
    fn merge_work_lanes_merge_by_name_keeping_global_lanes_not_overridden() {
        let mut global_lanes = BTreeMap::new();
        global_lanes.insert(
            "lane-a".to_string(),
            RawLaneConfig {
                repo: Some("/global/a".to_string()),
                ..Default::default()
            },
        );
        global_lanes.insert(
            "lane-b".to_string(),
            RawLaneConfig {
                repo: Some("/global/b".to_string()),
                ..Default::default()
            },
        );
        let global = RawWorkConfig {
            lanes: global_lanes,
            ..Default::default()
        };

        let mut repo_lanes = BTreeMap::new();
        repo_lanes.insert(
            "lane-b".to_string(),
            RawLaneConfig {
                repo: Some("/repo-local/b".to_string()),
                ..Default::default()
            },
        );
        let repo = RawWorkConfig {
            lanes: repo_lanes,
            ..Default::default()
        };

        let cfg = merge_work(Some(global), Some(repo), None).expect("should merge");
        assert_eq!(cfg.lanes.get("lane-a").unwrap().repo, "/global/a");
        assert_eq!(cfg.lanes.get("lane-b").unwrap().repo, "/repo-local/b");
    }

    #[test]
    fn load_repo_local_tskmstr_toml_overrides_work_section() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
            jira_base_url = "https://global.atlassian.net"
            jira_email = "global@example.com"
            default_project_key = "GLOBAL"

            [work]
            worktree_root = "~/Worktrees"

            [work.lanes.solo]
            repo = "/global/solo"
            "#,
        )
        .unwrap();

        let repo_path = dir.path().join(".tskmstr.toml");
        fs::write(
            &repo_path,
            r#"
            [work]
            worktree_root = "/repo/worktrees"

            [work.lanes.solo]
            repo = "/repo-local/solo"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(cfg.work.worktree_root, Some("/repo/worktrees".to_string()));
        assert_eq!(cfg.work.lanes.get("solo").unwrap().repo, "/repo-local/solo");
    }

    // --- relative lane `repo` / `[work.audit].dir` paths (GitHub issue #5
    // phase 2) ---

    #[test]
    fn load_repo_local_relative_lane_repo_resolves_against_repo_dir() {
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
        fs::write(
            &repo_path,
            r#"
            [work.lanes.tskmstr]
            repo = "."
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(
            cfg.work.lanes.get("tskmstr").unwrap().repo,
            dir.path().to_string_lossy()
        );
    }

    #[test]
    fn load_repo_local_relative_audit_dir_resolves_against_repo_dir() {
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
        fs::write(
            &repo_path,
            r#"
            [work.audit]
            dir = "."
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        let cfg = load(&paths).expect("should load");
        assert_eq!(
            cfg.work.audit.dir,
            Some(dir.path().to_string_lossy().into_owned())
        );
    }

    #[test]
    fn load_repo_local_relative_lane_repo_resolves_against_repo_dir_with_subpath() {
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
        fs::write(
            &repo_path,
            r#"
            [work.lanes.sibling]
            repo = "../other-repo"
            "#,
        )
        .unwrap();

        let paths = ConfigPaths {
            global: global_path,
            repo: Some(repo_path),
        };
        let cfg = load(&paths).expect("should load");
        let expected = dir.path().join("../other-repo");
        assert_eq!(
            cfg.work.lanes.get("sibling").unwrap().repo,
            expected.to_string_lossy()
        );
    }

    #[test]
    fn merge_work_relative_lane_repo_from_global_only_is_a_config_error() {
        let mut lanes = BTreeMap::new();
        lanes.insert(
            "solo".to_string(),
            RawLaneConfig {
                repo: Some("relative/solo".to_string()),
                ..Default::default()
            },
        );
        let global = RawWorkConfig {
            lanes,
            ..Default::default()
        };

        let err = merge_work(Some(global), None, None).expect_err("should reject relative path");
        match err {
            ConfigError::RelativePathRequiresRepoConfig { field, value } => {
                assert_eq!(field, "work.lanes.solo.repo");
                assert_eq!(value, "relative/solo");
            }
            other => panic!("expected RelativePathRequiresRepoConfig, got {other:?}"),
        }
    }

    #[test]
    fn merge_work_relative_audit_dir_from_global_only_is_a_config_error() {
        let global = RawWorkConfig {
            audit: Some(RawAuditConfig {
                dir: Some("relative/axiom".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = merge_work(Some(global), None, None).expect_err("should reject relative path");
        match err {
            ConfigError::RelativePathRequiresRepoConfig { field, value } => {
                assert_eq!(field, "work.audit.dir");
                assert_eq!(value, "relative/axiom");
            }
            other => panic!("expected RelativePathRequiresRepoConfig, got {other:?}"),
        }
    }

    #[test]
    fn merge_work_relative_lane_repo_with_repo_dir_resolves_against_it() {
        let mut lanes = BTreeMap::new();
        lanes.insert(
            "solo".to_string(),
            RawLaneConfig {
                repo: Some(".".to_string()),
                ..Default::default()
            },
        );
        let repo = RawWorkConfig {
            lanes,
            ..Default::default()
        };

        let cfg = merge_work(None, Some(repo), Some(Path::new("/repo/solo")))
            .expect("should resolve against repo_dir");
        assert_eq!(cfg.lanes.get("solo").unwrap().repo, "/repo/solo");
    }

    #[test]
    fn merge_work_lane_absolute_repo_is_unaffected_by_repo_dir() {
        let mut lanes = BTreeMap::new();
        lanes.insert(
            "solo".to_string(),
            RawLaneConfig {
                repo: Some("/absolute/solo".to_string()),
                ..Default::default()
            },
        );
        let repo = RawWorkConfig {
            lanes,
            ..Default::default()
        };

        let cfg = merge_work(None, Some(repo), Some(Path::new("/repo/dir")))
            .expect("absolute paths pass through unchanged");
        assert_eq!(cfg.lanes.get("solo").unwrap().repo, "/absolute/solo");
    }

    #[test]
    fn merge_work_lane_tilde_repo_is_unaffected_by_repo_dir() {
        let mut lanes = BTreeMap::new();
        lanes.insert(
            "solo".to_string(),
            RawLaneConfig {
                repo: Some("~/Projects/solo".to_string()),
                ..Default::default()
            },
        );
        let repo = RawWorkConfig {
            lanes,
            ..Default::default()
        };

        let cfg = merge_work(None, Some(repo), Some(Path::new("/repo/dir")))
            .expect("tilde paths pass through unexpanded and unresolved");
        assert_eq!(cfg.lanes.get("solo").unwrap().repo, "~/Projects/solo");
    }

    #[test]
    fn merge_work_relative_lane_inherited_from_global_with_repo_dir_present_still_errors() {
        // Even when merge_work is given a repo_dir (e.g. the repo-local
        // config's own directory, for a *different* section's relative
        // path), a lane that itself came only from the global config still
        // has no defining directory of its own -- repo_dir must not leak
        // across to a value it didn't define.
        let mut lanes = BTreeMap::new();
        lanes.insert(
            "solo".to_string(),
            RawLaneConfig {
                repo: Some("relative/solo".to_string()),
                ..Default::default()
            },
        );
        let global = RawWorkConfig {
            lanes,
            ..Default::default()
        };

        let err = merge_work(Some(global), None, Some(Path::new("/repo/dir")))
            .expect_err("a global-only lane has no defining repo dir even when one is given");
        assert!(matches!(
            err,
            ConfigError::RelativePathRequiresRepoConfig { .. }
        ));
    }
}
