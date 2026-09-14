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

#[cfg(test)]
mod tests {
    use super::*;

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
