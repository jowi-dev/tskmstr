//! `tm graph`: print the epic unlock graph built by
//! [`crate::graph::build_graph`] as text, JSON, or Mermaid.
//!
//! Every format writes each graph warning (such as an unlocks cycle) to
//! stderr as `warning: <message>`. Warnings never fail the command.

use std::io::{self, Write};

use thiserror::Error;

use crate::config::BackendIdentity;
use crate::graph::{GraphDocument, GraphError, build_graph, render_mermaid, render_text};
use crate::ticketing::provider::TicketProvider;

/// Which view of the graph [`run`] prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphFormat {
    /// One line per epic with its unlocks neighbors ([`render_text`]).
    Text,
    /// The versioned [`GraphDocument`], pretty-printed.
    Json,
    /// A Mermaid flowchart ([`render_mermaid`]).
    Mermaid,
}

/// Errors from [`run`].
#[derive(Debug, Error)]
pub enum GraphCliError {
    /// Building the graph failed, or the focus key is not an epic in it.
    #[error(transparent)]
    Graph(#[from] GraphError),

    /// Writing the output failed.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// Serializing the JSON document failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// `tm graph [EPIC]`: build the graph for `project_key` from `provider`,
/// scoped to `epic` when given, and write it to `out` in `format`. Each
/// warning goes to `err` first. `now_secs` (seconds since the Unix epoch)
/// stamps the JSON document's `generated_at`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    provider: &dyn TicketProvider,
    identity: &BackendIdentity,
    project_key: &str,
    epic: Option<&str>,
    format: GraphFormat,
    now_secs: i64,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), GraphCliError> {
    let graph = build_graph(provider, identity, project_key, epic)?;
    for warning in &graph.warnings {
        writeln!(err, "warning: {}", warning.message)?;
    }
    match format {
        GraphFormat::Text => out.write_all(render_text(&graph).as_bytes())?,
        GraphFormat::Mermaid => out.write_all(render_mermaid(&graph).as_bytes())?,
        GraphFormat::Json => {
            let document = GraphDocument::new(graph, identity, now_secs);
            serde_json::to_writer_pretty(&mut *out, &document)?;
            writeln!(out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jira::fake::FakeJiraClient;
    use crate::ticketing::types::{
        Issue, IssueFields, IssueLink, IssueLinkType, LinkedIssue, LinkedIssueFields, SearchResult,
        Status, StatusCategory,
    };

    fn status() -> Status {
        Status {
            name: "To Do".to_string(),
            status_category: StatusCategory {
                key: "new".to_string(),
            },
        }
    }

    /// A link on the issue it is attached to, saying it blocks `key`.
    fn blocks(key: &str) -> IssueLink {
        IssueLink {
            id: format!("out-{key}"),
            link_type: IssueLinkType {
                name: "Blocks".to_string(),
                inward: "is blocked by".to_string(),
                outward: "blocks".to_string(),
            },
            inward_issue: None,
            outward_issue: Some(LinkedIssue {
                key: key.to_string(),
                fields: LinkedIssueFields {
                    summary: format!("Summary for {key}"),
                    status: status(),
                },
            }),
        }
    }

    fn issue(key: &str, issue_links: Vec<IssueLink>) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: format!("Summary for {key}"),
                status: status(),
                description: None,
                assignee: None,
                issue_links,
                parent: None,
                labels: Vec::new(),
                issue_type: None,
            },
        }
    }

    /// A fake whose epic listing returns `epics`, each seeded as an epic.
    fn fake_with_epics(epics: Vec<Issue>) -> FakeJiraClient {
        let fake = epics.iter().fold(FakeJiraClient::new(), |fake, epic| {
            fake.with_epic(&epic.key, epic.clone())
        });
        fake.with_search_result(SearchResult {
            issues: epics,
            next_page_token: None,
        })
    }

    fn identity() -> BackendIdentity {
        BackendIdentity::Jira {
            base_url: "https://example.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        }
    }

    /// Run `tm graph` against `fake`, returning the result, stdout, and
    /// stderr.
    fn run_graph(
        fake: &FakeJiraClient,
        epic: Option<&str>,
        format: GraphFormat,
    ) -> (Result<(), GraphCliError>, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(
            fake,
            &identity(),
            "PROJ",
            epic,
            format,
            0,
            &mut out,
            &mut err,
        );
        (
            result,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn json_output_is_the_versioned_document() {
        let fake = fake_with_epics(vec![
            issue("PROJ-1", vec![blocks("PROJ-2")]),
            issue("PROJ-2", vec![]),
        ]);

        let (result, out, err) = run_graph(&fake, None, GraphFormat::Json);

        result.unwrap();
        assert!(err.is_empty(), "unexpected stderr: {err}");
        assert!(out.ends_with('\n'));
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["generated_at"], "1970-01-01T00:00:00Z");
        assert_eq!(json["source"]["project"], "PROJ");
        assert_eq!(
            json["links"],
            serde_json::json!([{"from": "PROJ-1", "to": "PROJ-2", "kind": "unlocks"}])
        );
    }

    #[test]
    fn cycle_warns_on_stderr_and_still_succeeds() {
        let fake = fake_with_epics(vec![
            issue("PROJ-1", vec![blocks("PROJ-2")]),
            issue("PROJ-2", vec![blocks("PROJ-1")]),
        ]);

        let (result, out, err) = run_graph(&fake, None, GraphFormat::Json);

        result.unwrap();
        assert_eq!(err, "warning: unlocks cycle among PROJ-1, PROJ-2\n");
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["warnings"][0]["kind"], "cycle");
    }

    #[test]
    fn cycle_warns_on_stderr_in_text_form_too() {
        let fake = fake_with_epics(vec![
            issue("PROJ-1", vec![blocks("PROJ-2")]),
            issue("PROJ-2", vec![blocks("PROJ-1")]),
        ]);

        let (result, out, err) = run_graph(&fake, None, GraphFormat::Text);

        result.unwrap();
        assert_eq!(err, "warning: unlocks cycle among PROJ-1, PROJ-2\n");
        assert!(out.contains("PROJ-1"));
    }

    #[test]
    fn text_output_is_the_text_view() {
        let fake = fake_with_epics(vec![issue("PROJ-1", vec![])]);

        let (result, out, _) = run_graph(&fake, None, GraphFormat::Text);

        result.unwrap();
        let graph = crate::graph::build_graph(&fake, &identity(), "PROJ", None).unwrap();
        assert_eq!(out, crate::graph::render_text(&graph));
    }

    #[test]
    fn mermaid_output_is_a_flowchart() {
        let fake = fake_with_epics(vec![issue("PROJ-1", vec![])]);

        let (result, out, _) = run_graph(&fake, None, GraphFormat::Mermaid);

        result.unwrap();
        assert!(out.starts_with("flowchart LR"), "got: {out}");
    }

    #[test]
    fn focus_on_a_non_epic_is_an_error() {
        let fake = fake_with_epics(vec![issue("PROJ-1", vec![blocks("PROJ-9")])]);

        let (result, out, _) = run_graph(&fake, Some("PROJ-9"), GraphFormat::Text);

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "PROJ-9 is not an epic");
        assert!(out.is_empty());
    }
}
