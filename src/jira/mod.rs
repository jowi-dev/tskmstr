//! Jira Cloud REST API client support: request/response types, the JQL used
//! to find a user's open tickets, and a plain-text-to-ADF converter for issue
//! descriptions.

pub mod adf;
pub mod client;
pub mod fake;
pub mod jql;
pub mod types;
