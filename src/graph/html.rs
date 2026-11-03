//! The HTML view of a [`GraphDocument`], for `tm graph --html`: one
//! self-contained page that draws the epics and their unlocks links in the
//! browser, with no network access.
//!
//! The page embeds the JSON document unchanged, plus each epic's
//! availability and wave from [`graph_state`](super::state::graph_state),
//! so the page shows the same planning state as every other view instead
//! of working it out again in the browser.

use std::collections::BTreeMap;

use serde::Serialize;

use super::GraphDocument;
use super::state::{Availability, GraphState};

/// The page template. Its script lays out and draws the graph from the
/// data element, with no libraries.
const TEMPLATE: &str = include_str!("graph.html");

/// Where [`render_html`] puts the embedded data in [`TEMPLATE`].
const DATA_PLACEHOLDER: &str = "/*GRAPH_DATA*/";

/// What the page reads: the JSON document, plus each epic's planning
/// state by key.
#[derive(Serialize)]
struct PageData<'a> {
    document: &'a GraphDocument,
    epics: BTreeMap<&'a str, EpicSummary>,
}

/// The part of an epic's [`EpicState`](super::state::EpicState) the page
/// draws.
#[derive(Serialize)]
struct EpicSummary {
    /// `available`, `locked`, or `done`.
    availability: &'static str,
    /// See [`EpicState::wave`](super::state::EpicState::wave).
    wave: usize,
}

/// Render `document` as one HTML page, coloring each epic by its
/// availability in `state`. Every epic and unlocks link in `document` is
/// drawn; clicking a node opens its ticket URL. Fails only if serializing
/// the embedded data fails.
pub fn render_html(
    document: &GraphDocument,
    state: &GraphState,
) -> Result<String, serde_json::Error> {
    let epics = state
        .linked
        .iter()
        .chain(&state.standalone)
        .map(|e| {
            let availability = match e.availability {
                Availability::Available => "available",
                Availability::Locked => "locked",
                Availability::Done => "done",
            };
            (
                e.key.as_str(),
                EpicSummary {
                    availability,
                    wave: e.wave,
                },
            )
        })
        .collect();
    let json = serde_json::to_string(&PageData { document, epics })?;
    Ok(TEMPLATE.replacen(DATA_PLACEHOLDER, &script_safe(&json), 1))
}

/// `json` made safe inside a `<script>` element: `<`, `>`, and `&` become
/// JSON `\u` escapes, so no title can close the element or open a
/// comment. `JSON.parse` reads the escapes back as the same characters.
fn script_safe(json: &str) -> String {
    json.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendIdentity;
    use crate::graph::state::graph_state;
    use crate::graph::{ChildProgress, Graph, GraphLink, GraphNode, LinkKind};

    /// The `id` of the template's `<script type="application/json">`
    /// element, where [`DATA_PLACEHOLDER`] sits.
    const DATA_ELEMENT_ID: &str = "graph-data";

    fn node(key: &str, title: &str, category: &str, children: Option<(usize, usize)>) -> GraphNode {
        GraphNode {
            key: key.to_string(),
            backend: "jira:https://example.atlassian.net:PROJ".to_string(),
            title: title.to_string(),
            status: category.to_string(),
            status_category: category.to_string(),
            url: format!("https://example.atlassian.net/browse/{key}"),
            is_epic: children.is_some(),
            children: children.map(|(done, total)| ChildProgress { done, total }),
            assignee: None,
        }
    }

    fn unlocks(from: &str, to: &str) -> GraphLink {
        GraphLink {
            from: from.to_string(),
            to: to.to_string(),
            kind: LinkKind::Unlocks,
        }
    }

    /// PROJ-1 is done and unlocks PROJ-2, which unlocks PROJ-3.
    fn sample(title_of_2: &str) -> Graph {
        Graph {
            nodes: vec![
                node("PROJ-1", "Build the base", "done", Some((2, 2))),
                node("PROJ-2", title_of_2, "indeterminate", Some((1, 3))),
                node("PROJ-3", "Grow", "new", Some((0, 0))),
            ],
            links: vec![unlocks("PROJ-1", "PROJ-2"), unlocks("PROJ-2", "PROJ-3")],
            warnings: Vec::new(),
        }
    }

    fn render(graph: Graph) -> String {
        let state = graph_state(&graph);
        let identity = BackendIdentity::Jira {
            base_url: "https://example.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        };
        render_html(&GraphDocument::new(graph, &identity, 0), &state).unwrap()
    }

    /// The JSON inside the page's data `<script>` tag.
    fn embedded(page: &str) -> serde_json::Value {
        let open = format!("<script type=\"application/json\" id=\"{DATA_ELEMENT_ID}\">");
        let start = page.find(&open).expect("data script tag") + open.len();
        let end = start + page[start..].find("</script>").expect("closing tag");
        serde_json::from_str(&page[start..end]).unwrap()
    }

    #[test]
    fn page_is_a_complete_html_document() {
        let page = render(sample("Ship"));

        assert!(page.starts_with("<!DOCTYPE html>"), "got: {page}");
        assert!(page.trim_end().ends_with("</html>"));
    }

    #[test]
    fn page_embeds_the_json_document_unchanged() {
        let graph = sample("Ship");
        let identity = BackendIdentity::Jira {
            base_url: "https://example.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        };
        let document =
            serde_json::to_value(GraphDocument::new(graph.clone(), &identity, 0)).unwrap();

        let data = embedded(&render(graph));

        assert_eq!(data["document"], document);
    }

    #[test]
    fn page_embeds_each_epics_availability_and_wave() {
        let data = embedded(&render(sample("Ship")));

        assert_eq!(
            data["epics"],
            serde_json::json!({
                "PROJ-1": {"availability": "done", "wave": 0},
                "PROJ-2": {"availability": "available", "wave": 0},
                "PROJ-3": {"availability": "locked", "wave": 1},
            })
        );
    }

    #[test]
    fn a_title_cannot_close_the_data_script_tag() {
        let page = render(sample("</script><script>alert(1)</script>"));

        let data = embedded(&page);

        assert_eq!(
            data["document"]["nodes"][1]["title"],
            "</script><script>alert(1)</script>"
        );
        assert!(!page.contains("<script>alert(1)"));
    }

    #[test]
    fn page_loads_nothing_from_the_network() {
        let page = render(sample("Ship"));

        for needle in ["src=\"http", "href=\"http", "@import", "url(http"] {
            assert!(!page.contains(needle), "page references {needle}");
        }
    }
}
