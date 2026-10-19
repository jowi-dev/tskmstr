//! The fix-round cap shared by every automatic fix trigger (GitHub issue
//! #120): `tm review auto`'s loop and `tm review import --fix` both stop
//! starting `tm review fix` passes for a ticket once it has
//! `[dispatch] max_fix_rounds` of them. Every `review-fix` run counts,
//! whatever started it, including a human's `F`; only the automatic
//! triggers stop at the cap.

use crate::runs::{RunStore, RunStoreError};

/// Run `kind` of a `tm review fix` pass.
pub const REVIEW_FIX_KIND: &str = "review-fix";

/// How many `review-fix` runs `ticket` has in `scope`.
///
/// # Errors
///
/// Any run-store query failure.
pub fn fix_rounds_used(store: &RunStore, scope: &str, ticket: &str) -> Result<u32, RunStoreError> {
    let used = store
        .runs_for_ticket(Some(scope), ticket)?
        .iter()
        .filter(|run| run.kind == REVIEW_FIX_KIND)
        .count();
    Ok(u32::try_from(used).unwrap_or(u32::MAX))
}

/// Whether `ticket` has used up `max_fix_rounds`.
///
/// # Errors
///
/// Any run-store query failure.
pub fn fix_cap_reached(
    store: &RunStore,
    scope: &str,
    ticket: &str,
    max_fix_rounds: u32,
) -> Result<bool, RunStoreError> {
    Ok(fix_rounds_used(store, scope, ticket)? >= max_fix_rounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::StartRun;
    use tempfile::TempDir;

    fn start(store: &RunStore, scope: &str, ticket: &str, kind: &str) {
        store
            .start_run(&StartRun {
                scope: scope.to_string(),
                ticket: ticket.to_string(),
                lane: "default".to_string(),
                kind: kind.to_string(),
                worktree: "/wt".to_string(),
                branch: None,
                pid: None,
                log_path: None,
            })
            .unwrap();
    }

    #[test]
    fn counts_only_this_tickets_review_fix_runs_in_scope() {
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        start(&store, "gh:o/r", "GH-1", "lane");
        start(&store, "gh:o/r", "GH-1", REVIEW_FIX_KIND);
        start(&store, "gh:o/r", "GH-1", REVIEW_FIX_KIND);
        start(&store, "gh:o/r", "GH-2", REVIEW_FIX_KIND);
        start(&store, "gh:x/y", "GH-1", REVIEW_FIX_KIND);

        assert_eq!(fix_rounds_used(&store, "gh:o/r", "GH-1").unwrap(), 2);
        assert!(!fix_cap_reached(&store, "gh:o/r", "GH-1", 3).unwrap());
        assert!(fix_cap_reached(&store, "gh:o/r", "GH-1", 2).unwrap());
    }
}
