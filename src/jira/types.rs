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
    /// Issue links (dependencies) attached to this issue.
    ///
    /// `#[serde(default)]` is required for two reasons: search responses
    /// only include fields that were explicitly requested (so an issue
    /// fetched by a query that didn't ask for `issuelinks` simply omits the
    /// key), and many existing fixtures predate link support entirely. Note
    /// the Jira field name is `issuelinks`, all lowercase, unlike the
    /// camelCase used elsewhere in this struct.
    #[serde(default, rename = "issuelinks")]
    pub issue_links: Vec<IssueLink>,
}

/// A single issue-link entry, as embedded in [`IssueFields::issue_links`].
///
/// Jira models a link as directional relative to the issue it's embedded
/// in, using exactly one of `inwardIssue`/`outwardIssue` per entry (never
/// both, in practice, but both are modeled as `Option` since nothing in the
/// API schema guarantees exactly one). The direction is easy to get
/// backwards, so spell it out: for an entry on issue X,
/// - `inward_issue: Some(Y)` means "X `<link_type.inward>` Y" — for the
///   `Blocks` link type, "X is blocked by Y", i.e. Y blocks X.
/// - `outward_issue: Some(Y)` means "X `<link_type.outward>` Y" — for the
///   `Blocks` link type, "X blocks Y".
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueLink {
    /// Jira's identifier for this specific link entry, distinct from either
    /// issue's key. Passed to [`crate::jira::client::JiraClient::delete_link`]
    /// to remove the link; every persisted link has one, so this is a plain
    /// `String`, not `Option`.
    pub id: String,
    /// The kind of relationship this link represents.
    #[serde(rename = "type")]
    pub link_type: IssueLinkType,
    /// Present when this entry describes an inward relationship (see
    /// [`IssueLink`] docs for what "inward" means).
    pub inward_issue: Option<LinkedIssue>,
    /// Present when this entry describes an outward relationship (see
    /// [`IssueLink`] docs for what "outward" means).
    pub outward_issue: Option<LinkedIssue>,
}

/// The kind of relationship an [`IssueLink`] represents, e.g. `Blocks`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueLinkType {
    /// Link type name, e.g. `Blocks`.
    pub name: String,
    /// Description used when this issue is the inward side of the link,
    /// e.g. `is blocked by`.
    pub inward: String,
    /// Description used when this issue is the outward side of the link,
    /// e.g. `blocks`.
    pub outward: String,
}

/// The other issue named by an [`IssueLink`], with enough of its fields to
/// render a summary without a follow-up fetch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LinkedIssue {
    /// Issue key, e.g. `PROJ-123`.
    pub key: String,
    /// The subset of the linked issue's fields Jira embeds inline.
    pub fields: LinkedIssueFields,
}

/// Fields Jira embeds inline for a [`LinkedIssue`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LinkedIssueFields {
    /// One-line issue summary.
    pub summary: String,
    /// Current workflow status.
    pub status: Status,
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

/// A user eligible to be assigned to an issue in a given project, as
/// returned by `GET /rest/api/3/user/assignable/search`.
///
/// Deliberately narrower than [`Myself`] (no `emailAddress`): assignee
/// resolution only ever needs the account ID and display name, and omitting
/// the extra field keeps test fixtures minimal.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraUser {
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

/// Request body for `POST /rest/api/3/issueLink` (create an issue-link
/// dependency between two issues).
///
/// tskmstr only ever creates `Blocks`-type links, so the link type name is
/// hardcoded rather than taking a parameter. Per Atlassian's documented
/// contract, the *outward* issue is the one the link type's outward
/// description applies to: for `Blocks`, `outwardIssue` is the blocker and
/// `inwardIssue` is the blocked issue (`outwardIssue` "blocks" `inwardIssue`,
/// `inwardIssue` "is blocked by" `outwardIssue`). This is the mirror image
/// of how it reads in English, so it's easy to swap the two fields by
/// accident — double check against this doc comment, not intuition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateLinkRequest {
    /// Key of the issue that blocks `blocked_key`; serialized as
    /// `outwardIssue`.
    pub blocker_key: String,
    /// Key of the issue that is blocked by `blocker_key`; serialized as
    /// `inwardIssue`.
    pub blocked_key: String,
}

impl CreateLinkRequest {
    /// Serialize this request into the exact JSON body Jira expects for
    /// `POST /rest/api/3/issueLink`.
    pub fn to_payload(&self) -> Value {
        serde_json::json!({
            "type": { "name": "Blocks" },
            "inwardIssue": { "key": self.blocked_key },
            "outwardIssue": { "key": self.blocker_key },
        })
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
    fn deserializes_jira_user() {
        let raw = r#"
        {
            "accountId": "acct-789",
            "displayName": "Grace Hopper"
        }
        "#;
        let user: JiraUser = serde_json::from_str(raw).unwrap();
        assert_eq!(user.account_id, "acct-789");
        assert_eq!(user.display_name, "Grace Hopper");
    }

    #[test]
    fn deserializes_jira_user_list() {
        let raw = r#"
        [
            { "accountId": "acct-1", "displayName": "Ada Lovelace" },
            { "accountId": "acct-2", "displayName": "Grace Hopper" }
        ]
        "#;
        let users: Vec<JiraUser> = serde_json::from_str(raw).unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].display_name, "Ada Lovelace");
        assert_eq!(users[1].account_id, "acct-2");
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
    fn deserializes_issue_links_with_inward_and_outward_entries() {
        let raw = r#"
        {
            "key": "PROJ-1",
            "fields": {
                "summary": "Fix the thing",
                "status": {
                    "name": "To Do",
                    "statusCategory": { "key": "new" }
                },
                "description": null,
                "assignee": null,
                "issuelinks": [
                    {
                        "id": "10001",
                        "type": {
                            "name": "Blocks",
                            "inward": "is blocked by",
                            "outward": "blocks"
                        },
                        "inwardIssue": {
                            "key": "PROJ-2",
                            "fields": {
                                "summary": "Blocker ticket",
                                "status": {
                                    "name": "In Progress",
                                    "statusCategory": { "key": "indeterminate" }
                                }
                            }
                        }
                    },
                    {
                        "id": "10002",
                        "type": {
                            "name": "Blocks",
                            "inward": "is blocked by",
                            "outward": "blocks"
                        },
                        "outwardIssue": {
                            "key": "PROJ-3",
                            "fields": {
                                "summary": "Blocked ticket",
                                "status": {
                                    "name": "To Do",
                                    "statusCategory": { "key": "new" }
                                }
                            }
                        }
                    }
                ]
            }
        }
        "#;
        let issue: Issue = serde_json::from_str(raw).unwrap();
        assert_eq!(issue.fields.issue_links.len(), 2);

        let blocker_entry = &issue.fields.issue_links[0];
        assert_eq!(blocker_entry.id, "10001");
        assert_eq!(blocker_entry.link_type.name, "Blocks");
        let blocker = blocker_entry
            .inward_issue
            .as_ref()
            .expect("inward issue present");
        assert_eq!(blocker.key, "PROJ-2");
        assert_eq!(blocker.fields.summary, "Blocker ticket");
        assert_eq!(blocker.fields.status.status_category.key, "indeterminate");
        assert!(blocker_entry.outward_issue.is_none());

        let blocked_entry = &issue.fields.issue_links[1];
        let blocked = blocked_entry
            .outward_issue
            .as_ref()
            .expect("outward issue present");
        assert_eq!(blocked.key, "PROJ-3");
        assert!(blocked_entry.inward_issue.is_none());
    }

    #[test]
    fn deserializes_issue_without_issuelinks_key_as_empty_vec() {
        let raw = r#"
        {
            "key": "PROJ-1",
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
        assert!(issue.fields.issue_links.is_empty());
    }

    #[test]
    fn serializes_create_link_request() {
        let req = CreateLinkRequest {
            blocker_key: "PROJ-1".to_string(),
            blocked_key: "PROJ-2".to_string(),
        };
        let payload = req.to_payload();
        assert_eq!(
            payload,
            json!({
                "type": { "name": "Blocks" },
                "inwardIssue": { "key": "PROJ-2" },
                "outwardIssue": { "key": "PROJ-1" }
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
