//! JQL query construction.

/// The JQL clause that keeps epics out of a board query: an issue type other
/// than `epic_issue_type` ([`crate::config::Config::epic_issue_type`]),
/// quoted and escaped via [`escape_jql_string`] since it comes from config.
/// An epic groups tickets rather than being worked itself, so no board
/// column, rank list, or `tm ready` candidate list shows one (GitHub issue
/// #200); [`open_epics_jql`] lists them for the board's epics view instead.
fn not_epic_clause(epic_issue_type: &str) -> String {
    format!("issuetype != \"{}\"", escape_jql_string(epic_issue_type))
}

/// The JQL query used to list the current user's open tickets on the board,
/// epics excluded (see [`not_epic_clause`]).
///
/// Pinned to an exact string: any change to this query is a deliberate,
/// reviewable change to what the board shows.
pub fn my_open_tickets_jql(epic_issue_type: &str) -> String {
    format!(
        "assignee = currentUser() AND {} AND statusCategory != Done ORDER BY updated DESC",
        not_epic_clause(epic_issue_type)
    )
}

/// The JQL query used by the board's assignee filter to list every unassigned
/// open ticket in `project_key`, epics excluded (see [`not_epic_clause`]).
pub fn unassigned_tickets_jql(project_key: &str, epic_issue_type: &str) -> String {
    format!(
        "project = {project_key} AND assignee is EMPTY AND {} AND statusCategory != Done ORDER BY updated DESC",
        not_epic_clause(epic_issue_type)
    )
}

/// The JQL query used by the board's assignee filter to list every open
/// ticket in `project_key`, regardless of assignee, epics excluded (see
/// [`not_epic_clause`]).
pub fn everyone_tickets_jql(project_key: &str, epic_issue_type: &str) -> String {
    format!(
        "project = {project_key} AND {} AND statusCategory != Done ORDER BY updated DESC",
        not_epic_clause(epic_issue_type)
    )
}

/// The JQL query used by the board's assignee filter to list every open
/// ticket in `project_key` assigned to `account_id`, epics excluded (see
/// [`not_epic_clause`]).
pub fn assignee_tickets_jql(project_key: &str, account_id: &str, epic_issue_type: &str) -> String {
    format!(
        "project = {project_key} AND assignee = \"{account_id}\" AND {} AND statusCategory != Done ORDER BY updated DESC",
        not_epic_clause(epic_issue_type)
    )
}

/// The JQL query used by `tm ticket rank` and the board's Rank screen to
/// list `project_key`'s open tickets in Jira's native backlog rank order,
/// driven by the same `Rank` field [`JiraClient::rank`] moves. Epics are
/// excluded (see [`not_epic_clause`]).
pub fn ranked_tickets_jql(project_key: &str, epic_issue_type: &str) -> String {
    format!(
        "project = {project_key} AND {} AND statusCategory != Done ORDER BY Rank ASC",
        not_epic_clause(epic_issue_type)
    )
}

/// The JQL query used by the board's epics view (GitHub issue #200) to list
/// `project_key`'s open epics, the ones [`not_epic_clause`] keeps off every
/// other board query. Unlike [`epics_jql`], Done epics are left out, the
/// same way the board hides done tickets. Ordered by `updated DESC`, like
/// [`everyone_tickets_jql`].
pub fn open_epics_jql(project_key: &str, epic_issue_type: &str) -> String {
    let escaped = escape_jql_string(epic_issue_type);
    format!(
        "project = {project_key} AND issuetype = \"{escaped}\" AND statusCategory != Done ORDER BY updated DESC"
    )
}

/// The JQL query the cross-project overview's background poller uses to
/// fetch the tracker status of every ticket a scope has runs for, in one
/// search (ADR-0009 decision 3). Keys are spliced unquoted: they come from
/// `runs.db`, where every key was already validated as a ticket key.
pub fn keys_jql(keys: &[String]) -> String {
    format!("key in ({})", keys.join(", "))
}

/// The JQL query used by `tm ready` to list the current user's candidate
/// tickets: assigned to them and still in the "To Do" status category.
/// Restricted to "To Do" rather than every open status (unlike
/// [`my_open_tickets_jql`]'s `statusCategory != Done`) because a ticket
/// already `In Progress` has already been picked up, not "ready to be picked
/// up". Ordered by `Rank ASC` so the caller can filter out blocked tickets
/// client-side while preserving backlog order. Epics are excluded (see
/// [`not_epic_clause`]): a lane never launches on one.
pub fn ready_candidates_jql(epic_issue_type: &str) -> String {
    format!(
        "assignee = currentUser() AND {} AND statusCategory = \"To Do\" ORDER BY Rank ASC",
        not_epic_clause(epic_issue_type)
    )
}

/// The lookback window, in days, for [`shipped_awaiting_retro_jql`]: how far
/// back "recently shipped" reaches. Named and pinned so changing the retro
/// board's queue depth is a deliberate, reviewable change rather than a
/// magic number buried in a query string. 30 days: long enough that a ticket
/// isn't dropped from the queue before someone gets to it, short enough that
/// the screen reads as a queue to clear rather than a wall of all-time
/// shipped history.
pub const RETRO_WINDOW_DAYS: u32 = 30;

/// The JQL query used by the TUI's retro board (`Screen::Retro`) to list
/// tickets in `project_key` that shipped (moved to a `Done`-category status)
/// within the last [`RETRO_WINDOW_DAYS`] days, most recently resolved first.
///
/// Bounded to a recent window rather than every `Done` ticket ever shipped,
/// per [`RETRO_WINDOW_DAYS`]'s doc comment. This query alone doesn't exclude
/// tickets that already have a recorded retro verdict -- that filtering
/// happens client-side against `RunStore::retro_verdicts_for_tickets`, the
/// same JQL-scopes/store-filters split every other board screen uses.
pub fn shipped_awaiting_retro_jql(project_key: &str) -> String {
    format!(
        "project = {project_key} AND statusCategory = Done AND resolutiondate >= -{RETRO_WINDOW_DAYS}d ORDER BY resolutiondate DESC"
    )
}

/// Escape `text` for embedding in a double-quoted JQL string literal:
/// backslashes first (so a later-escaped quote doesn't get re-escaped), then
/// double quotes. Without this, a search term containing `"` would either
/// break out of the JQL string literal or produce a syntax error from the
/// Jira API.
fn escape_jql_string(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The JQL query used by `tm ticket search <TEXT>` to find open tickets in
/// `project_key` whose text (summary, description, comments — whatever
/// Jira's `text ~` operator indexes) matches `text`.
///
/// `text` is escaped via [`escape_jql_string`] before being embedded in the
/// query's string literal, so search terms containing `"` or `\` can't break
/// out of it or produce a malformed query. Ordered by `updated DESC` so the
/// most recently touched matches surface first, the same rationale as
/// [`unassigned_tickets_jql`]/[`everyone_tickets_jql`]/[`assignee_tickets_jql`].
pub fn ticket_search_jql(project_key: &str, text: &str) -> String {
    let escaped = escape_jql_string(text);
    format!(
        "project = {project_key} AND statusCategory != Done AND text ~ \"{escaped}\" ORDER BY updated DESC"
    )
}

/// The JQL query used by `tm ticket children <EPIC>` to list the tickets
/// directly under `epic_key`, whatever their status: an epic's done children
/// are still part of it. `epic_key` is quoted and escaped via
/// [`escape_jql_string`], since it comes from the command line. Ordered by
/// `Rank ASC`, backlog order, like [`ranked_tickets_jql`].
pub fn children_jql(epic_key: &str) -> String {
    let escaped = escape_jql_string(epic_key);
    format!("parent = \"{escaped}\" ORDER BY Rank ASC")
}

/// The JQL query used by `tm graph` to list every epic in `project_key`,
/// Done ones included: a Done epic still matters when it unlocked an open
/// one. `epic_issue_type` is the project's epic type name
/// ([`crate::config::Config::epic_issue_type`]), quoted and escaped via
/// [`escape_jql_string`] since it comes from config. Ordered by `Rank ASC`,
/// backlog order, like [`ranked_tickets_jql`].
pub fn epics_jql(project_key: &str, epic_issue_type: &str) -> String {
    let escaped = escape_jql_string(epic_issue_type);
    format!("project = {project_key} AND issuetype = \"{escaped}\" ORDER BY Rank ASC")
}

#[cfg(test)]
mod tests {
    use super::{
        assignee_tickets_jql, children_jql, epics_jql, everyone_tickets_jql, keys_jql,
        my_open_tickets_jql, open_epics_jql, ranked_tickets_jql, ready_candidates_jql,
        shipped_awaiting_retro_jql, ticket_search_jql, unassigned_tickets_jql,
    };

    #[test]
    fn children_jql_selects_the_epics_children_in_rank_order() {
        assert_eq!(
            children_jql("PROJ-1"),
            "parent = \"PROJ-1\" ORDER BY Rank ASC"
        );
    }

    #[test]
    fn children_jql_escapes_the_epic_key() {
        assert_eq!(
            children_jql(r#"PROJ-1" OR project = X"#),
            r#"parent = "PROJ-1\" OR project = X" ORDER BY Rank ASC"#
        );
    }

    #[test]
    fn epics_jql_lists_every_epic_in_the_project_in_rank_order() {
        assert_eq!(
            epics_jql("PROJ", "Epic"),
            "project = PROJ AND issuetype = \"Epic\" ORDER BY Rank ASC"
        );
    }

    #[test]
    fn epics_jql_escapes_the_epic_issue_type() {
        assert_eq!(
            epics_jql("PROJ", r#"Big "Epic""#),
            r#"project = PROJ AND issuetype = "Big \"Epic\"" ORDER BY Rank ASC"#
        );
    }

    #[test]
    fn open_epics_jql_lists_the_projects_open_epics_newest_first() {
        assert_eq!(
            open_epics_jql("PROJ", "Epic"),
            "project = PROJ AND issuetype = \"Epic\" AND statusCategory != Done ORDER BY updated DESC"
        );
    }

    #[test]
    fn board_queries_escape_the_epic_issue_type() {
        assert_eq!(
            everyone_tickets_jql("PROJ", r#"Big "Epic""#),
            r#"project = PROJ AND issuetype != "Big \"Epic\"" AND statusCategory != Done ORDER BY updated DESC"#
        );
    }

    #[test]
    fn keys_jql_lists_every_key_in_one_clause() {
        assert_eq!(
            keys_jql(&["PROJ-1".to_string(), "PROJ-22".to_string()]),
            "key in (PROJ-1, PROJ-22)"
        );
    }

    #[test]
    fn returns_exact_pinned_jql() {
        assert_eq!(
            my_open_tickets_jql("Epic"),
            "assignee = currentUser() AND issuetype != \"Epic\" AND statusCategory != Done ORDER BY updated DESC"
        );
    }

    #[test]
    fn unassigned_tickets_jql_scopes_to_project_and_excludes_assignee() {
        assert_eq!(
            unassigned_tickets_jql("PROJ", "Epic"),
            "project = PROJ AND assignee is EMPTY AND issuetype != \"Epic\" AND statusCategory != Done ORDER BY updated DESC"
        );
    }

    #[test]
    fn everyone_tickets_jql_scopes_to_project_with_no_assignee_clause() {
        assert_eq!(
            everyone_tickets_jql("PROJ", "Epic"),
            "project = PROJ AND issuetype != \"Epic\" AND statusCategory != Done ORDER BY updated DESC"
        );
    }

    #[test]
    fn assignee_tickets_jql_scopes_to_project_and_account_id() {
        assert_eq!(
            assignee_tickets_jql("PROJ", "acct-1", "Epic"),
            "project = PROJ AND assignee = \"acct-1\" AND issuetype != \"Epic\" AND statusCategory != Done ORDER BY updated DESC"
        );
    }

    #[test]
    fn ranked_tickets_jql_scopes_to_project_and_orders_by_rank() {
        assert_eq!(
            ranked_tickets_jql("PROJ", "Epic"),
            "project = PROJ AND issuetype != \"Epic\" AND statusCategory != Done ORDER BY Rank ASC"
        );
    }

    #[test]
    fn ready_candidates_jql_scopes_to_current_user_and_to_do_category() {
        assert_eq!(
            ready_candidates_jql("Epic"),
            "assignee = currentUser() AND issuetype != \"Epic\" AND statusCategory = \"To Do\" ORDER BY Rank ASC"
        );
    }

    #[test]
    fn ticket_search_jql_scopes_to_project_and_excludes_done() {
        assert_eq!(
            ticket_search_jql("PROJ", "login bug"),
            "project = PROJ AND statusCategory != Done AND text ~ \"login bug\" ORDER BY updated DESC"
        );
    }

    #[test]
    fn ticket_search_jql_escapes_double_quotes_in_text() {
        assert_eq!(
            ticket_search_jql("PROJ", "the \"login\" bug"),
            "project = PROJ AND statusCategory != Done AND text ~ \"the \\\"login\\\" bug\" ORDER BY updated DESC"
        );
    }

    #[test]
    fn ticket_search_jql_escapes_backslashes_in_text() {
        assert_eq!(
            ticket_search_jql("PROJ", "C:\\path\\to\\file"),
            "project = PROJ AND statusCategory != Done AND text ~ \"C:\\\\path\\\\to\\\\file\" ORDER BY updated DESC"
        );
    }

    #[test]
    fn shipped_awaiting_retro_jql_scopes_to_project_done_and_window() {
        assert_eq!(
            shipped_awaiting_retro_jql("PROJ"),
            "project = PROJ AND statusCategory = Done AND resolutiondate >= -30d ORDER BY resolutiondate DESC"
        );
    }

    #[test]
    fn ticket_search_jql_escapes_backslash_before_quote_so_order_matters() {
        // A literal `\"` in the input must become `\\\"` (an escaped
        // backslash followed by an escaped quote), not `\\"` (which would
        // close the JQL string literal early). This only holds if
        // backslashes are escaped before quotes.
        assert_eq!(
            ticket_search_jql("PROJ", "quote: \\\""),
            "project = PROJ AND statusCategory != Done AND text ~ \"quote: \\\\\\\"\" ORDER BY updated DESC"
        );
    }
}
