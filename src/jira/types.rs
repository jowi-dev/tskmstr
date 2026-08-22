//! Jira-only wire-request shapes for the Jira Cloud REST API (v3).
//!
//! The read-path types every ticket carries — [`crate::ticketing::types::Issue`],
//! its fields, links, transitions, and users — moved to
//! [`crate::ticketing::types`] in the phase 5 prep refactor (see
//! `docs/plans/github-issues-backend.md`), since they're backend-neutral:
//! [`crate::ticketing::provider::JiraProvider`] maps Jira's HTTP responses
//! onto them directly, with no Jira-specific fields. What's left here is
//! genuinely Jira-specific: [`CreateIssueRequest`] (built from a
//! provider-level `NewTicket`, carrying an ADF description Jira's create-issue
//! endpoint requires) and the `to_payload()` methods that turn
//! [`crate::ticketing::types::RemoteLinkRequest`] and
//! [`crate::ticketing::types::CreateLinkRequest`] into the exact JSON Jira's
//! REST API expects.

use crate::ticketing::types::{CreateLinkRequest, RemoteLinkRequest};
use serde::Serialize;
use serde_json::Value;

/// Request body for `POST /rest/api/3/issue` (create issue).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateIssueRequest {
    /// Project key the issue is created under.
    pub project_key: String,
    /// One-line issue summary.
    pub summary: String,
    /// Issue description in Atlassian Document Format (ADF).
    pub description: Value,
    /// Issue type name, e.g. `Task`.
    pub issue_type_name: String,
    /// Account ID to assign the new issue to, if any.
    pub assignee_account_id: Option<String>,
}

impl CreateIssueRequest {
    /// Serialize this request into the exact JSON body Jira expects for
    /// `POST /rest/api/3/issue`.
    pub fn to_payload(&self) -> Value {
        let mut fields = serde_json::json!({
            "project": { "key": self.project_key },
            "summary": self.summary,
            "description": self.description,
            "issuetype": { "name": self.issue_type_name },
        });

        if let Some(account_id) = &self.assignee_account_id {
            fields["assignee"] = serde_json::json!({ "id": account_id });
        }

        serde_json::json!({ "fields": fields })
    }
}

/// Wraps [`RemoteLinkRequest`] fields under the `object` key Jira expects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct RemoteLinkObject<'a> {
    object: &'a RemoteLinkRequest,
}

impl RemoteLinkRequest {
    /// Serialize this request into the exact JSON body Jira expects for
    /// `POST /rest/api/3/issue/{key}/remotelink`.
    ///
    /// An inherent impl on a type defined in [`crate::ticketing::types`]:
    /// legal because inherent impls only require the type to live in the
    /// current crate, not the current module, and this mapping is
    /// genuinely Jira-specific wire shaping that has no business living
    /// next to the backend-neutral struct definition.
    pub fn to_payload(&self) -> Value {
        serde_json::to_value(RemoteLinkObject { object: self })
            .expect("RemoteLinkRequest serialization cannot fail")
    }
}

impl CreateLinkRequest {
    /// Serialize this request into the exact JSON body Jira expects for
    /// `POST /rest/api/3/issueLink`.
    ///
    /// tskmstr only ever creates `Blocks`-type links, so the link type name
    /// is hardcoded rather than taking a parameter. In this payload the
    /// *inward* issue is the blocker: `inwardIssue` "blocks" `outwardIssue`.
    /// This is the OPPOSITE of what Atlassian's own KB examples suggest
    /// ("the outward issue is the one the outward description applies to")
    /// — but it is what Jira Cloud actually does, verified end-to-end
    /// against a live instance on 2026-08-04 (link created with the blocker
    /// as `outwardIssue` rendered as "is blocked by" in the Jira UI; see
    /// also <https://github.com/atlassian/atlassian-mcp-server/issues/112>,
    /// which reports the same observed behavior). Consistency check: an
    /// issue's `issuelinks` field shows the OTHER issue under its stored
    /// role key, and an `inwardIssue` entry renders with the `type.inward`
    /// phrase ("is blocked by <other>") — so the stored inward end is the
    /// blocker, and this payload must put the blocker there too. Trust this
    /// doc comment and the payload test over Atlassian's examples.
    pub fn to_payload(&self) -> Value {
        serde_json::json!({
            "type": { "name": "Blocks" },
            "inwardIssue": { "key": self.blocker_key },
            "outwardIssue": { "key": self.blocked_key },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_create_issue_request_with_assignee() {
        let req = CreateIssueRequest {
            project_key: "PROJ".to_string(),
            summary: "Fix the thing".to_string(),
            description: json!({ "type": "doc", "version": 1, "content": [] }),
            issue_type_name: "Task".to_string(),
            assignee_account_id: Some("acct-1".to_string()),
        };
        let payload = req.to_payload();
        assert_eq!(
            payload,
            json!({
                "fields": {
                    "project": { "key": "PROJ" },
                    "summary": "Fix the thing",
                    "description": { "type": "doc", "version": 1, "content": [] },
                    "issuetype": { "name": "Task" },
                    "assignee": { "id": "acct-1" }
                }
            })
        );
    }

    #[test]
    fn serializes_create_issue_request_without_assignee() {
        let req = CreateIssueRequest {
            project_key: "PROJ".to_string(),
            summary: "Fix the thing".to_string(),
            description: json!({ "type": "doc", "version": 1, "content": [] }),
            issue_type_name: "Task".to_string(),
            assignee_account_id: None,
        };
        let payload = req.to_payload();
        assert_eq!(
            payload,
            json!({
                "fields": {
                    "project": { "key": "PROJ" },
                    "summary": "Fix the thing",
                    "description": { "type": "doc", "version": 1, "content": [] },
                    "issuetype": { "name": "Task" }
                }
            })
        );
    }

    #[test]
    fn serializes_create_link_request_with_blocker_as_inward_issue() {
        let req = CreateLinkRequest {
            blocker_key: "PROJ-1".to_string(),
            blocked_key: "PROJ-2".to_string(),
        };
        let payload = req.to_payload();
        // The blocker goes under `inwardIssue` — verified against live Jira
        // Cloud (see `CreateLinkRequest::to_payload`'s doc comment); do not
        // "fix" this to match Atlassian's KB examples, which describe the
        // opposite of what the API actually does.
        assert_eq!(
            payload,
            json!({
                "type": { "name": "Blocks" },
                "inwardIssue": { "key": "PROJ-1" },
                "outwardIssue": { "key": "PROJ-2" }
            })
        );
    }

    #[test]
    fn serializes_remote_link_request() {
        let req = RemoteLinkRequest {
            url: "https://github.com/example/repo/pull/1".to_string(),
            title: "example/repo#1".to_string(),
        };
        let payload = req.to_payload();
        assert_eq!(
            payload,
            json!({
                "object": {
                    "url": "https://github.com/example/repo/pull/1",
                    "title": "example/repo#1"
                }
            })
        );
    }
}
