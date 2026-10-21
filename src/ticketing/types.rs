//! Provider-owned read-path types: the shapes every [`super::provider::TicketProvider`]
//! method returns or accepts, independent of which backend produced them.
//!
//! Today only [`crate::jira`] populates these (via
//! [`crate::ticketing::provider::JiraProvider`], mapping Jira's wire
//! responses onto exactly these shapes with no field renaming — see the
//! phase 5 prep entry in `docs/plans/github-issues-backend.md` for why the
//! move was done this way, to keep `FakeJiraClient`-based tests compiling
//! unchanged). A future `GithubProvider` maps `gh`/GraphQL responses onto
//! the same types instead of inventing its own. No module outside
//! [`crate::jira`] should need to name a Jira-specific type to talk about a
//! ticket, an issue link, a transition, or a user — these are it.
//!
//! [`crate::jira::types`] still defines the Jira-only wire-request shapes
//! ([`crate::jira::types::CreateIssueRequest`]) and the `to_payload()`
//! methods that turn [`RemoteLinkRequest`]/[`CreateLinkRequest`] into the
//! exact JSON Jira's REST API expects — that mapping is genuinely
//! Jira-specific, so it stays there, built on top of the plain data
//! structs defined here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The authenticated user, as returned by `GET /rest/api/3/myself`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Myself {
    /// Account ID of the authenticated user.
    pub account_id: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Email address, if the user's profile exposes one.
    pub email_address: Option<String>,
}

/// A ticket, as returned by issue-fetch and search endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Issue {
    /// Issue key, e.g. `PROJ-123`.
    pub key: String,
    /// The issue's field values.
    pub fields: IssueFields,
}

/// Fields of a ticket relevant to tskmstr.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueFields {
    /// One-line issue summary.
    pub summary: String,
    /// Current workflow status.
    pub status: Status,
    /// Issue description. For the Jira backend this is Atlassian Document
    /// Format (ADF) JSON, converted to/from plain Markdown at the
    /// [`super::provider::TicketProvider`] boundary
    /// ([`super::provider::TicketProvider::description_text`],
    /// [`crate::jira::adf`]).
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
    /// The epic this ticket sits under, if any.
    ///
    /// Jira also sets `parent` on a sub-task, pointing at an ordinary task.
    /// That is not an epic, so deserialization drops it (see
    /// [`deserialize_epic_parent`]) and a sub-task reads as having none.
    ///
    /// `#[serde(default)]` for the same reasons as
    /// [`IssueFields::issue_links`]: a search that didn't request `parent`
    /// omits it, and older fixtures predate it.
    #[serde(default, deserialize_with = "deserialize_epic_parent")]
    pub parent: Option<ParentRef>,
    /// Label names on this issue. The github backend reads its `tm:epic`
    /// label from here ([`super::provider::TicketProvider::is_epic`]).
    ///
    /// `#[serde(default)]` for the same reasons as
    /// [`IssueFields::issue_links`]; Jira's field of the same name is also a
    /// list of strings, so a search that does request it deserializes as is.
    #[serde(default)]
    pub labels: Vec<String>,
    /// The ticket's type, if the backend reports one. The Jira backend
    /// reads it to tell epics from ordinary tickets
    /// ([`super::provider::TicketProvider::is_epic`]).
    ///
    /// `#[serde(default)]` for the same reasons as
    /// [`IssueFields::issue_links`]. Jira's field name is `issuetype`.
    #[serde(default, rename = "issuetype")]
    pub issue_type: Option<IssueType>,
}

/// A ticket's type, as embedded in [`IssueFields::issue_type`]. Only the
/// name is kept, e.g. `Task` or `Epic`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueType {
    /// The type's display name.
    pub name: String,
}

/// The epic a ticket sits under, as embedded in [`IssueFields::parent`].
///
/// Deliberately a separate type from [`IssueLink`]/[`LinkedIssue`]: a
/// parent means "this epic is made of this ticket" (decomposition), while
/// an issue link such as `Blocks` is a dependency between peers. Mixing the
/// two would let a parent leak into blocker readiness or the reverse.
///
/// Built from Jira's wire shape (`{"key": ..., "fields": {"summary": ...,
/// "issuetype": ...}}`, which `GET /issue/{key}` already returns for a
/// child ticket) by [`deserialize_epic_parent`], which flattens it to just
/// the key and summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentRef {
    /// The epic's key, e.g. `PROJ-1`.
    pub key: String,
    /// The epic's one-line summary.
    pub summary: String,
}

/// Jira's nested `parent` shape, flattened into [`ParentRef`].
#[derive(Deserialize)]
struct WireParent {
    key: String,
    fields: WireParentFields,
}

/// The subset of [`WireParent`]'s embedded fields [`ParentRef`] keeps,
/// plus the issue type [`deserialize_epic_parent`] checks.
#[derive(Deserialize)]
struct WireParentFields {
    summary: String,
    #[serde(default)]
    issuetype: Option<WireParentType>,
}

/// The parent's issue type, as Jira embeds it in `parent.fields`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireParentType {
    name: String,
    /// Jira's level for the type: 1 for epics (whatever the site renamed
    /// them to), 0 for standard types, -1 for sub-tasks.
    hierarchy_level: Option<i64>,
}

impl WireParentType {
    /// Whether this is the epic level. Uses `hierarchyLevel` when present,
    /// else falls back to the stock `Epic` type name.
    fn is_epic(&self) -> bool {
        match self.hierarchy_level {
            Some(level) => level == 1,
            None => self.name == "Epic",
        }
    }
}

/// Deserialize [`IssueFields::parent`], keeping it only when it is an
/// epic. A parent with no `issuetype` is dropped too, since nothing says
/// it is one.
fn deserialize_epic_parent<'de, D>(deserializer: D) -> Result<Option<ParentRef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let wire = Option::<WireParent>::deserialize(deserializer)?;
    Ok(wire
        .filter(|parent| {
            parent
                .fields
                .issuetype
                .as_ref()
                .is_some_and(WireParentType::is_epic)
        })
        .map(ParentRef::from))
}

impl From<WireParent> for ParentRef {
    fn from(wire: WireParent) -> Self {
        ParentRef {
            key: wire.key,
            summary: wire.fields.summary,
        }
    }
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
    /// The link's own identifier, distinct from either issue's key. Passed
    /// to [`super::provider::TicketProvider::delete_link`] to remove the
    /// link; every persisted link has one, so this is a plain `String`, not
    /// `Option`.
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
    /// The subset of the linked issue's fields embedded inline.
    pub fields: LinkedIssueFields,
}

/// Fields embedded inline for a [`LinkedIssue`].
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

/// A reference to a user, as embedded in issue fields.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRef {
    /// Account ID.
    pub account_id: String,
    /// Human-readable display name.
    pub display_name: String,
}

/// A user eligible to be assigned to an issue in a given project.
///
/// Deliberately narrower than [`Myself`] (no `emailAddress`): assignee
/// resolution only ever needs the account ID and display name, and omitting
/// the extra field keeps test fixtures minimal.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraUser {
    /// Account ID.
    pub account_id: String,
    /// Human-readable display name.
    pub display_name: String,
}

/// A workflow transition available on an issue.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Transition {
    /// Transition ID, passed back to the backend to perform the transition.
    pub id: String,
    /// Human-readable transition name, e.g. `Start Progress`.
    pub name: String,
    /// The status this transition leads to.
    pub to: Status,
}

/// Result of a ticket search.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    /// Issues matching the query on this page.
    pub issues: Vec<Issue>,
    /// Token to fetch the next page, absent when this is the last page.
    pub next_page_token: Option<String>,
}

/// A remote link to attach to a ticket (e.g. a GitHub pull request).
///
/// `to_payload()` (an inherent impl on this type, defined in
/// [`crate::jira::types`] since it builds Jira's exact wire JSON) turns
/// this into the request body
/// `POST /rest/api/3/issue/{key}/remotelink` expects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteLinkRequest {
    /// URL the remote link points to.
    pub url: String,
    /// Title shown for the link.
    pub title: String,
}

/// A request to create a `Blocks` dependency between two tickets, such that
/// `blocker_key` blocks `blocked_key`.
///
/// `to_payload()` (an inherent impl on this type, defined in
/// [`crate::jira::types`]) turns this into the exact JSON
/// `POST /rest/api/3/issueLink` expects — see that impl's doc comment for
/// the (counter to Atlassian's own docs) inward/outward direction Jira
/// Cloud actually uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateLinkRequest {
    /// Key of the issue that blocks `blocked_key`.
    pub blocker_key: String,
    /// Key of the issue that is blocked by `blocker_key`.
    pub blocked_key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn deserializes_issue_parent_from_jira_wire_shape() {
        let raw = r#"
        {
            "key": "PROJ-2",
            "fields": {
                "summary": "Child",
                "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                "description": null,
                "assignee": null,
                "parent": {
                    "id": "10001",
                    "key": "PROJ-1",
                    "fields": {
                        "summary": "The epic",
                        "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                        "issuetype": { "name": "Epic", "subtask": false, "hierarchyLevel": 1 }
                    }
                }
            }
        }
        "#;
        let issue: Issue = serde_json::from_str(raw).unwrap();
        assert_eq!(
            issue.fields.parent,
            Some(ParentRef {
                key: "PROJ-1".to_string(),
                summary: "The epic".to_string(),
            })
        );
    }

    /// Parse an issue whose `parent` has the given `issuetype` JSON.
    fn issue_with_parent_type(issuetype: &str) -> Issue {
        let raw = format!(
            r#"{{
                "key": "PROJ-5",
                "fields": {{
                    "summary": "Child",
                    "status": {{ "name": "To Do", "statusCategory": {{ "key": "new" }} }},
                    "description": null,
                    "assignee": null,
                    "parent": {{
                        "key": "PROJ-4",
                        "fields": {{ "summary": "Parent", "issuetype": {issuetype} }}
                    }}
                }}
            }}"#
        );
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn drops_a_sub_task_parent_that_is_not_an_epic() {
        let issue = issue_with_parent_type(r#"{ "name": "Task", "hierarchyLevel": 0 }"#);
        assert_eq!(issue.fields.parent, None);
    }

    #[test]
    fn keeps_a_renamed_epic_parent_by_hierarchy_level() {
        let issue = issue_with_parent_type(r#"{ "name": "Feature", "hierarchyLevel": 1 }"#);
        assert_eq!(
            issue.fields.parent.map(|p| p.key),
            Some("PROJ-4".to_string())
        );
    }

    #[test]
    fn falls_back_to_the_epic_type_name_without_a_hierarchy_level() {
        let epic = issue_with_parent_type(r#"{ "name": "Epic" }"#);
        assert_eq!(
            epic.fields.parent.map(|p| p.key),
            Some("PROJ-4".to_string())
        );
        let task = issue_with_parent_type(r#"{ "name": "Task" }"#);
        assert_eq!(task.fields.parent, None);
    }

    #[test]
    fn deserializes_issue_without_parent_as_none() {
        let raw = r#"
        {
            "key": "PROJ-2",
            "fields": {
                "summary": "Child",
                "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                "description": null,
                "assignee": null
            }
        }
        "#;
        let issue: Issue = serde_json::from_str(raw).unwrap();
        assert_eq!(issue.fields.parent, None);
    }

    #[test]
    fn deserializes_issue_type_name_from_jira_wire_shape() {
        let raw = r#"
        {
            "key": "PROJ-1",
            "fields": {
                "summary": "The epic",
                "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                "description": null,
                "assignee": null,
                "issuetype": { "id": "10000", "name": "Epic", "subtask": false }
            }
        }
        "#;
        let issue: Issue = serde_json::from_str(raw).unwrap();
        assert_eq!(
            issue.fields.issue_type,
            Some(IssueType {
                name: "Epic".to_string(),
            })
        );
    }

    #[test]
    fn deserializes_issue_without_issue_type_as_none() {
        let raw = r#"
        {
            "key": "PROJ-2",
            "fields": {
                "summary": "Child",
                "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                "description": null,
                "assignee": null
            }
        }
        "#;
        let issue: Issue = serde_json::from_str(raw).unwrap();
        assert_eq!(issue.fields.issue_type, None);
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
}
