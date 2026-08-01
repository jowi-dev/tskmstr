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

#[cfg(test)]
mod tests {
    use super::{
        assignee_tickets_jql, everyone_tickets_jql, my_open_tickets_jql, unassigned_tickets_jql,
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
}
