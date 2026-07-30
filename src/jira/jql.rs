//! JQL query construction.

/// The JQL query used to list the current user's open tickets on the board.
///
/// Pinned to an exact string: any change to this query is a deliberate,
/// reviewable change to what the board shows.
pub fn my_open_tickets_jql() -> String {
    "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC".to_string()
}

#[cfg(test)]
mod tests {
    use super::my_open_tickets_jql;

    #[test]
    fn returns_exact_pinned_jql() {
        assert_eq!(
            my_open_tickets_jql(),
            "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"
        );
    }
}
