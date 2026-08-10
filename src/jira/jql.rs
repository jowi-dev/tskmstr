//! JQL query construction.

/// The JQL query used to list the current user's open tickets on the board.
///
/// Pinned to an exact string: any change to this query is a deliberate,
/// reviewable change to what the board shows.
pub fn my_open_tickets_jql() -> String {
    "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC".to_string()
}

/// The JQL query used by the board's assignee filter to list every unassigned
/// open ticket in `project_key`.
pub fn unassigned_tickets_jql(project_key: &str) -> String {
    format!(
        "project = {project_key} AND assignee is EMPTY AND statusCategory != Done ORDER BY updated DESC"
    )
}

/// The JQL query used by the board's assignee filter to list every open
/// ticket in `project_key`, regardless of assignee.
pub fn everyone_tickets_jql(project_key: &str) -> String {
    format!("project = {project_key} AND statusCategory != Done ORDER BY updated DESC")
}

/// The JQL query used by the board's assignee filter to list every open
/// ticket in `project_key` assigned to `account_id`.
pub fn assignee_tickets_jql(project_key: &str, account_id: &str) -> String {
    format!(
        "project = {project_key} AND assignee = \"{account_id}\" AND statusCategory != Done ORDER BY updated DESC"
    )
}

/// The JQL query used by `tm ticket rank` (and, in future, a stack-rank TUI
/// screen) to list `project_key`'s open tickets in Jira's native backlog
/// rank order, driven by the same `Rank` field [`JiraClient::rank`] moves.
pub fn ranked_tickets_jql(project_key: &str) -> String {
    format!("project = {project_key} AND statusCategory != Done ORDER BY Rank ASC")
}

/// The JQL query used by `tm ready` to list the current user's candidate
/// tickets: assigned to them and still in the "To Do" status category.
/// Restricted to "To Do" rather than every open status (unlike
/// [`my_open_tickets_jql`]'s `statusCategory != Done`) because a ticket
/// already `In Progress` has already been picked up, not "ready to be picked
/// up". Ordered by `Rank ASC` so the caller can filter out blocked tickets
/// client-side while preserving backlog order.
pub fn ready_candidates_jql() -> String {
    "assignee = currentUser() AND statusCategory = \"To Do\" ORDER BY Rank ASC".to_string()
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

#[cfg(test)]
mod tests {
    use super::{
        assignee_tickets_jql, everyone_tickets_jql, my_open_tickets_jql, ranked_tickets_jql,
        ready_candidates_jql, ticket_search_jql, unassigned_tickets_jql,
    };

    #[test]
    fn returns_exact_pinned_jql() {
        assert_eq!(
            my_open_tickets_jql(),
            "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"
        );
    }

    #[test]
    fn unassigned_tickets_jql_scopes_to_project_and_excludes_assignee() {
        assert_eq!(
            unassigned_tickets_jql("PROJ"),
            "project = PROJ AND assignee is EMPTY AND statusCategory != Done ORDER BY updated DESC"
        );
    }

    #[test]
    fn everyone_tickets_jql_scopes_to_project_with_no_assignee_clause() {
        assert_eq!(
            everyone_tickets_jql("PROJ"),
            "project = PROJ AND statusCategory != Done ORDER BY updated DESC"
        );
    }

    #[test]
    fn assignee_tickets_jql_scopes_to_project_and_account_id() {
        assert_eq!(
            assignee_tickets_jql("PROJ", "acct-1"),
            "project = PROJ AND assignee = \"acct-1\" AND statusCategory != Done ORDER BY updated DESC"
        );
    }

    #[test]
    fn ranked_tickets_jql_scopes_to_project_and_orders_by_rank() {
        assert_eq!(
            ranked_tickets_jql("PROJ"),
            "project = PROJ AND statusCategory != Done ORDER BY Rank ASC"
        );
    }

    #[test]
    fn ready_candidates_jql_scopes_to_current_user_and_to_do_category() {
        assert_eq!(
            ready_candidates_jql(),
            "assignee = currentUser() AND statusCategory = \"To Do\" ORDER BY Rank ASC"
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
