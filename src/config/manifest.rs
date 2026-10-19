//! The asset/config schema versioning primitive: what "up to date" means for
//! a `tm init`-onboarded repo.
//!
//! `tm init` decides what needs setup purely by presence — is `.tskmstr.toml`
//! there, is `[work.audit]` there, and so on. That leaves no way to answer
//! "has this repo been onboarded by a tskmstr new enough to have today's
//! expected assets?" This module defines the stamp that answers it:
//! [`CURRENT_SCHEMA_VERSION`], the revision `tm init` writes into
//! `.tskmstr.toml`'s top-level `schema_version` key (see
//! [`crate::config::RawConfig::schema_version`]), and [`stamp_status`], which
//! compares a found stamp against it.
//!
//! `tm check` (a later, separate task) is the consumer that actually reads a
//! repo's stamp and reports [`StampStatus`] to the user — this module only
//! defines the shared primitive both `tm init` and `tm check` build on.
//!
//! It also holds [`EXPECTED_CONFIG_KEYS`] (GitHub issue #74): the config keys
//! the running tskmstr expects an onboarded repo to set, so `tm check` can
//! name a specific missing key rather than only a stale stamp.

/// The asset/config schema revision this binary expects.
///
/// Bump this when a newer tskmstr adds a new *expected* asset kind that an
/// existing onboarded repo won't have — a new default `[work.*]` session
/// type (mirroring how `[work.audit]`/`[work.create]`/`[work.manual]` were
/// each added), a newly-expected skill under `.claude/skills/`, or a new
/// config key/table that existing repos must gain for `tm init` (or a later
/// `tm check`) to consider them current.
///
/// The presence of the current stamp, together with the structural presence
/// checks `tm init` already runs, is the entire definition of "up to date" —
/// there is no separate content-diffing pass. In particular, user-editable
/// asset *content* (lane prompts, skill bodies) is intentionally never
/// compared: scaffolded assets are meant to be edited after `tm init` writes
/// them (see GitHub issue #30's agent-assisted setup, which edits them on
/// purpose), so a content diff would treat normal customization as drift.
/// Bumping this constant is a decision that a *structural* gap exists, not
/// that any file's content changed.
pub const CURRENT_SCHEMA_VERSION: i64 = 2;

/// What a found (or missing) `schema_version` stamp means for a repo,
/// relative to [`CURRENT_SCHEMA_VERSION`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StampStatus {
    /// The stamp matches [`CURRENT_SCHEMA_VERSION`] exactly: this repo was
    /// last onboarded (or re-onboarded) by a tskmstr expecting exactly
    /// today's set of assets.
    Current,
    /// No `schema_version` key was found at all: either the repo was
    /// onboarded by a tskmstr that predates stamping, or it was never run
    /// through `tm init` in the first place.
    Missing,
    /// The stamp is older than [`CURRENT_SCHEMA_VERSION`] (the wrapped value
    /// is the stamp that was found): an older tskmstr wrote this file, and a
    /// newer one now expects assets this repo may not have. Run
    /// `tm update` to catch it up.
    Stale(i64),
    /// The stamp is newer than [`CURRENT_SCHEMA_VERSION`] (the wrapped value
    /// is the stamp that was found): this repo was stamped by a tskmstr
    /// binary newer than the one currently running. Usually means the
    /// running binary itself is the one that's out of date.
    Newer(i64),
}

/// Compare a found `schema_version` stamp (`None` when the key was absent)
/// against [`CURRENT_SCHEMA_VERSION`], producing the [`StampStatus`] that
/// describes the repo's state relative to this binary.
pub fn stamp_status(found: Option<i64>) -> StampStatus {
    match found {
        None => StampStatus::Missing,
        Some(v) if v == CURRENT_SCHEMA_VERSION => StampStatus::Current,
        Some(v) if v < CURRENT_SCHEMA_VERSION => StampStatus::Stale(v),
        Some(v) => StampStatus::Newer(v),
    }
}

/// How much a missing [`ExpectedConfigKey`] matters (GitHub issue #74).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum KeySeverity {
    /// A feature breaks or misbehaves without the key. A missing required
    /// key is drift `tm check` exits non-zero for, and the `tm check --quiet`
    /// shell-entry nudge surfaces it.
    Required,
    /// A default is assumed when the key is absent, but setting it
    /// explicitly is better. Reported by `tm check` as advisory only: it
    /// never makes `tm check` exit non-zero and never reaches the quiet
    /// nudge.
    Recommended,
}

impl KeySeverity {
    /// The lowercase word `tm check` renders for this severity.
    pub fn as_str(self) -> &'static str {
        match self {
            KeySeverity::Required => "required",
            KeySeverity::Recommended => "recommended",
        }
    }
}

/// A config key a newer tskmstr expects an onboarded repo to set (GitHub
/// issue #74). Registered in [`EXPECTED_CONFIG_KEYS`] — the one place a
/// feature that ships a new key declares it, the way
/// [`CURRENT_SCHEMA_VERSION`] centralizes the stamp — so `tm check` can name
/// exactly which keys a repo is missing and `tm update` can write their
/// suggested defaults additively.
#[derive(Debug)]
pub struct ExpectedConfigKey {
    /// Table path plus key name, e.g. `["status_on_pr"]` for a top-level key
    /// or `["work", "merge", "model"]` for `[work.merge].model`.
    pub path: &'static [&'static str],
    /// The [`CURRENT_SCHEMA_VERSION`] at which tskmstr started expecting this
    /// key. Informational (rendered in the finding); presence alone decides
    /// whether the key is reported.
    pub since_schema_version: i64,
    /// Whether a missing key is drift or advisory. See [`KeySeverity`].
    pub severity: KeySeverity,
    /// The value `tm update` writes for a repo on the given backend,
    /// or `None` when there is no safe value to assume (e.g. a Jira
    /// workflow's status names are arbitrary). A recommended key with no
    /// default for the repo's backend isn't reported at all — there is
    /// nothing actionable to suggest; a required one is reported for the
    /// operator to set by hand.
    pub suggested_default: fn(super::BackendKind) -> Option<DefaultValue>,
}

/// A typed [`ExpectedConfigKey::suggested_default`], so `tm update` writes a
/// boolean key as `true` rather than the string `"true"`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DefaultValue {
    /// A TOML string.
    Str(&'static str),
    /// A TOML boolean.
    Bool(bool),
}

impl std::fmt::Display for DefaultValue {
    /// Renders the value as a TOML literal: `"In Review"`, `true`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DefaultValue::Str(s) => write!(f, "\"{s}\""),
            DefaultValue::Bool(b) => write!(f, "{b}"),
        }
    }
}

impl ExpectedConfigKey {
    /// The key's dotted path, e.g. `work.merge.model`.
    pub fn dotted(&self) -> String {
        self.path.join(".")
    }
}

/// Every config key the running tskmstr expects an onboarded repo to set.
/// A feature that adds a key registers it here.
///
/// The GitHub defaults name the workflow statuses whose `tm:status/*` labels
/// `tm init` creates, so they're safe to assume; a Jira workflow's status
/// names are the board's own, so Jira gets no default.
pub const EXPECTED_CONFIG_KEYS: &[ExpectedConfigKey] = &[
    ExpectedConfigKey {
        path: &["status_on_pr"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |backend| match backend {
            super::BackendKind::Github => Some(DefaultValue::Str("In Review")),
            super::BackendKind::Jira => None,
        },
    },
    ExpectedConfigKey {
        path: &["status_on_run_start"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |backend| match backend {
            super::BackendKind::Github => Some(DefaultValue::Str("In Progress")),
            super::BackendKind::Jira => None,
        },
    },
    // GitHub issue #112: absent means `true` already; writing it makes the
    // audit gate visible in the repo's config. The gate defaults on for
    // Jira too; only the nag is GitHub-only, matching the keys above.
    ExpectedConfigKey {
        path: &["ready", "require_audit"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |backend| match backend {
            super::BackendKind::Github => Some(DefaultValue::Bool(true)),
            super::BackendKind::Jira => None,
        },
    },
    // GitHub issue #118. Opt-in, so there is no default to suggest: an
    // unattended watcher (and, with `on_bots_done = "launch"`, a cleanup
    // session) is a choice, not drift. With no default it is never reported;
    // it is registered so the key is declared in one place.
    ExpectedConfigKey {
        path: &["work", "review_watch", "auto_arm"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |_| None,
    },
    // GitHub issue #124. `tm review auto` is opt-in and every key has a
    // built-in default, so none is suggested and none is ever reported.
    ExpectedConfigKey {
        path: &["work", "auto_review", "prompt_file"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |_| None,
    },
    ExpectedConfigKey {
        path: &["work", "auto_review", "fix_threshold"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |_| None,
    },
    ExpectedConfigKey {
        path: &["work", "auto_review", "max_review_rounds"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |_| None,
    },
    ExpectedConfigKey {
        path: &["work", "auto_review", "publish"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |_| None,
    },
    ExpectedConfigKey {
        path: &["work", "auto_review", "auto"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |_| None,
    },
    // GitHub issue #114. Opt-in spend, so there is no default to suggest.
    // The `[agent.<runner>]` usage keys (ceilings, snapshot max age,
    // billing, token budget) aren't registered: each has a built-in
    // default, so a repo without them hasn't drifted, and their paths
    // would name a runner outside `src/agent/`.
    ExpectedConfigKey {
        path: &["dispatch", "allow_paid_fallback"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |_| None,
    },
    // GitHub issue #120. The fix-round cap has a built-in default, and
    // resolving imported threads on GitHub is opt-in, so neither is ever
    // reported.
    ExpectedConfigKey {
        path: &["dispatch", "max_fix_rounds"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |_| None,
    },
    ExpectedConfigKey {
        path: &["review", "resolve_imported"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |_| None,
    },
    // GitHub issue #115. The built-in default of one lane applies when
    // unset, so there is nothing to suggest.
    ExpectedConfigKey {
        path: &["dispatch", "max_lanes"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |_| None,
    },
    // GitHub issue #116. The TUI host's tick interval has a built-in
    // default of a minute.
    ExpectedConfigKey {
        path: &["dispatch", "tick_secs"],
        since_schema_version: 2,
        severity: KeySeverity::Recommended,
        suggested_default: |_| None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendKind;

    fn expected(path: &[&str]) -> &'static ExpectedConfigKey {
        EXPECTED_CONFIG_KEYS
            .iter()
            .find(|k| k.path == path)
            .unwrap_or_else(|| panic!("{path:?} registered"))
    }

    #[test]
    fn status_on_pr_is_recommended_with_a_github_only_default() {
        let key = expected(&["status_on_pr"]);
        assert_eq!(key.severity, KeySeverity::Recommended);
        assert_eq!(
            (key.suggested_default)(BackendKind::Github),
            Some(DefaultValue::Str("In Review"))
        );
        assert_eq!((key.suggested_default)(BackendKind::Jira), None);
    }

    #[test]
    fn status_on_run_start_is_recommended_with_a_github_only_default() {
        let key = expected(&["status_on_run_start"]);
        assert_eq!(key.severity, KeySeverity::Recommended);
        assert_eq!(
            (key.suggested_default)(BackendKind::Github),
            Some(DefaultValue::Str("In Progress"))
        );
        assert_eq!((key.suggested_default)(BackendKind::Jira), None);
    }

    #[test]
    fn review_watch_auto_arm_is_registered_without_a_default() {
        let key = expected(&["work", "review_watch", "auto_arm"]);
        assert_eq!(key.severity, KeySeverity::Recommended);
        assert_eq!((key.suggested_default)(BackendKind::Github), None);
        assert_eq!((key.suggested_default)(BackendKind::Jira), None);
    }

    #[test]
    fn ready_require_audit_is_recommended_true_on_github_only() {
        let key = expected(&["ready", "require_audit"]);
        assert_eq!(key.severity, KeySeverity::Recommended);
        assert_eq!(
            (key.suggested_default)(BackendKind::Github),
            Some(DefaultValue::Bool(true))
        );
        assert_eq!((key.suggested_default)(BackendKind::Jira), None);
    }

    #[test]
    fn default_value_renders_as_a_toml_literal() {
        assert_eq!(DefaultValue::Str("In Review").to_string(), "\"In Review\"");
        assert_eq!(DefaultValue::Bool(true).to_string(), "true");
    }

    #[test]
    fn auto_review_keys_are_registered_without_defaults() {
        for key in [
            "prompt_file",
            "fix_threshold",
            "max_review_rounds",
            "publish",
            "auto",
        ] {
            let key = expected(&["work", "auto_review", key]);
            assert_eq!(key.severity, KeySeverity::Recommended);
            assert_eq!((key.suggested_default)(BackendKind::Github), None);
            assert_eq!((key.suggested_default)(BackendKind::Jira), None);
        }
    }

    #[test]
    fn dispatch_allow_paid_fallback_is_registered_without_a_default() {
        let key = expected(&["dispatch", "allow_paid_fallback"]);
        assert_eq!(key.severity, KeySeverity::Recommended);
        assert_eq!((key.suggested_default)(BackendKind::Github), None);
        assert_eq!((key.suggested_default)(BackendKind::Jira), None);
    }

    #[test]
    fn dispatch_max_lanes_is_registered_without_a_default() {
        let key = expected(&["dispatch", "max_lanes"]);
        assert_eq!(key.severity, KeySeverity::Recommended);
        assert_eq!((key.suggested_default)(BackendKind::Github), None);
        assert_eq!((key.suggested_default)(BackendKind::Jira), None);
    }

    #[test]
    fn dispatch_tick_secs_is_registered_without_a_default() {
        let key = expected(&["dispatch", "tick_secs"]);
        assert_eq!(key.severity, KeySeverity::Recommended);
        assert_eq!((key.suggested_default)(BackendKind::Github), None);
        assert_eq!((key.suggested_default)(BackendKind::Jira), None);
    }

    #[test]
    fn no_registered_key_claims_a_schema_version_newer_than_this_binary() {
        for key in EXPECTED_CONFIG_KEYS {
            assert!(
                key.since_schema_version <= CURRENT_SCHEMA_VERSION,
                "{:?} since {}",
                key.path,
                key.since_schema_version
            );
        }
    }

    #[test]
    fn dotted_joins_the_key_path() {
        let key = ExpectedConfigKey {
            path: &["work", "merge", "model"],
            since_schema_version: 1,
            severity: KeySeverity::Recommended,
            suggested_default: |_| None,
        };
        assert_eq!(key.dotted(), "work.merge.model");
    }

    #[test]
    fn missing_when_no_stamp_found() {
        assert_eq!(stamp_status(None), StampStatus::Missing);
    }

    #[test]
    fn current_when_stamp_matches() {
        assert_eq!(
            stamp_status(Some(CURRENT_SCHEMA_VERSION)),
            StampStatus::Current
        );
    }

    #[test]
    fn stale_when_stamp_is_older() {
        assert_eq!(
            stamp_status(Some(CURRENT_SCHEMA_VERSION - 1)),
            StampStatus::Stale(CURRENT_SCHEMA_VERSION - 1)
        );
    }

    #[test]
    fn newer_when_stamp_is_newer() {
        assert_eq!(
            stamp_status(Some(CURRENT_SCHEMA_VERSION + 1)),
            StampStatus::Newer(CURRENT_SCHEMA_VERSION + 1)
        );
    }
}
