//! Serde type definitions for the Jira Cloud REST API (v3).
//!
//! These types cover only the subset of the API tskmstr needs: fetching the
//! current user, reading/searching issues, listing and applying transitions,
//! creating issues, and creating remote links. The search types model the
//! newer `POST /rest/api/3/search/jql` endpoint, which paginates with
//! `nextPageToken` rather than the legacy `startAt`/`maxResults` scheme.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The authenticated user, as returned by `GET /rest/api/3/myself`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Myself {
    /// Jira account ID of the authenticated user.
    pub account_id: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Email address, if the user's profile exposes one.
    pub email_address: Option<String>,
}

/// A Jira issue, as returned by issue-fetch and search endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Issue {
    /// Issue key, e.g. `PROJ-123`.
    pub key: String,
    /// The issue's field values.
    pub fields: IssueFields,
}

/// Fields of a Jira issue relevant to tskmstr.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueFields {
    /// One-line issue summary.
    pub summary: String,
    /// Current workflow status.
    pub status: Status,
    /// Issue description, stored as Atlassian Document Format (ADF) JSON.
    pub description: Option<Value>,
    /// Assigned user, if any.
    pub assignee: Option<UserRef>,
}

/// A workflow status.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// Status name, e.g. `In Progress`.
    pub name: String,
    /// Status category, used to distinguish "done" states from others.
    pub status_category: StatusCategory,
}

/// A status category.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StatusCategory {
    /// Category key, e.g. `new`, `indeterminate`, or `done`.
    pub key: String,
}

/// A reference to a Jira user, as embedded in issue fields.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRef {
    /// Jira account ID.
    pub account_id: String,
    /// Human-readable display name.
    pub display_name: String,
}

/// A workflow transition available on an issue, as returned by
/// `GET /rest/api/3/issue/{key}/transitions`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Transition {
    /// Transition ID, passed back to Jira to perform the transition.
    pub id: String,
    /// Human-readable transition name, e.g. `Start Progress`.
    pub name: String,
    /// The status this transition leads to.
    pub to: Status,
}

/// Result of a JQL search via `POST /rest/api/3/search/jql`.
///
/// This endpoint paginates with `nextPageToken` rather than the legacy
/// `startAt`/`maxResults` scheme used by the deprecated search endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    /// Issues matching the query on this page.
    pub issues: Vec<Issue>,
    /// Token to fetch the next page, absent when this is the last page.
    pub next_page_token: Option<String>,
}

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

/// Request body for `POST /rest/api/3/issue/{key}/remotelink` (create a
/// remote link, e.g. pointing at a GitHub pull request).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteLinkRequest {
    /// URL the remote link points to.
    pub url: String,
    /// Title shown for the link in the Jira UI.
    pub title: String,
}

/// Wraps [`RemoteLinkRequest`] fields under the `object` key Jira expects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct RemoteLinkObject<'a> {
    object: &'a RemoteLinkRequest,
}

impl RemoteLinkRequest {
    /// Serialize this request into the exact JSON body Jira expects for
    /// `POST /rest/api/3/issue/{key}/remotelink`.
    pub fn to_payload(&self) -> Value {
        serde_json::to_value(RemoteLinkObject { object: self })
            .expect("RemoteLinkRequest serialization cannot fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_myself() {
        let raw = r#"
        {
            "accountId": "abc123",
            "displayName": "Ada Lovelace",
            "emailAddress": "ada@example.com"
        }
        "#;
        let myself: Myself = serde_json::from_str(raw).unwrap();
        assert_eq!(myself.account_id, "abc123");
        assert_eq!(myself.display_name, "Ada Lovelace");
        assert_eq!(myself.email_address, Some("ada@example.com".to_string()));
    }

    #[test]
    fn deserializes_myself_without_email() {
        let raw = r#"
        {
            "accountId": "abc123",
            "displayName": "Ada Lovelace",
            "emailAddress": null
        }
        "#;
        let myself: Myself = serde_json::from_str(raw).unwrap();
        assert_eq!(myself.email_address, None);
    }

    #[test]
    fn deserializes_issue_with_null_assignee_and_description() {
        let raw = r#"
        {
            "key": "PROJ-123",
            "fields": {
                "summary": "Fix the thing",
                "status": {
                    "name": "To Do",
                    "statusCategory": { "key": "new" }
                },
                "description": null,
                "assignee": null
            }
        }
        "#;
        let issue: Issue = serde_json::from_str(raw).unwrap();
        assert_eq!(issue.key, "PROJ-123");
        assert_eq!(issue.fields.summary, "Fix the thing");
        assert_eq!(issue.fields.status.name, "To Do");
        assert_eq!(issue.fields.status.status_category.key, "new");
        assert_eq!(issue.fields.description, None);
        assert_eq!(issue.fields.assignee, None);
    }

    #[test]
    fn deserializes_issue_with_assignee_and_description() {
        let raw = r#"
        {
            "key": "PROJ-456",
            "fields": {
                "summary": "Add a feature",
                "status": {
                    "name": "In Progress",
                    "statusCategory": { "key": "indeterminate" }
                },
                "description": {
                    "type": "doc",
                    "version": 1,
                    "content": []
                },
                "assignee": {
                    "accountId": "acct-789",
                    "displayName": "Grace Hopper"
                }
            }
        }
        "#;
        let issue: Issue = serde_json::from_str(raw).unwrap();
        let assignee = issue.fields.assignee.expect("assignee present");
        assert_eq!(assignee.account_id, "acct-789");
        assert_eq!(assignee.display_name, "Grace Hopper");
        assert!(issue.fields.description.is_some());
    }

    #[test]
    fn deserializes_transition_list() {
        let raw = r#"
        [
            {
                "id": "11",
                "name": "Start Progress",
                "to": { "name": "In Progress", "statusCategory": { "key": "indeterminate" } }
            },
            {
                "id": "31",
                "name": "Done",
                "to": { "name": "Done", "statusCategory": { "key": "done" } }
            }
        ]
        "#;
        let transitions: Vec<Transition> = serde_json::from_str(raw).unwrap();
        assert_eq!(transitions.len(), 2);
        assert_eq!(transitions[0].id, "11");
        assert_eq!(transitions[0].name, "Start Progress");
        assert_eq!(transitions[0].to.status_category.key, "indeterminate");
        assert_eq!(transitions[1].to.status_category.key, "done");
    }

    #[test]
    fn deserializes_search_result_with_next_page_token() {
        let raw = r#"
        {
            "issues": [
                {
                    "key": "PROJ-1",
                    "fields": {
                        "summary": "First",
                        "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                        "description": null,
                        "assignee": null
                    }
                }
            ],
            "nextPageToken": "abc-token"
        }
        "#;
        let result: SearchResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.next_page_token, Some("abc-token".to_string()));
    }

    #[test]
    fn deserializes_search_result_without_next_page_token() {
        let raw = r#"
        {
            "issues": []
        }
        "#;
        let result: SearchResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.issues.len(), 0);
        assert_eq!(result.next_page_token, None);
    }

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
