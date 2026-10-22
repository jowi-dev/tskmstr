//! Plain-text and Mermaid views of a [`Graph`], for `tm graph` and
//! `tm graph --mermaid`. Pure functions: they build a `String` and print
//! nothing.

use super::{Graph, GraphLink, GraphNode, LinkKind};

/// The text view: one line per epic, `KEY  STATUS  TITLE  (done/total)`,
/// followed by indented `unlocks:` and `unlocked by:` lines when the epic
/// has any such links. Plain tickets appear only inside those lists.
pub fn render_text(graph: &Graph) -> String {
    let mut out = String::new();
    for epic in epic_rows(graph) {
        let progress = epic.children.unwrap_or_default();
        out.push_str(&format!(
            "{}  {}  {}  ({}/{})\n",
            epic.key, epic.status, epic.title, progress.done, progress.total
        ));
        let unlocks: Vec<&str> = unlocks_links(graph)
            .filter(|l| l.from == epic.key)
            .map(|l| l.to.as_str())
            .collect();
        let unlocked_by: Vec<&str> = unlocks_links(graph)
            .filter(|l| l.to == epic.key)
            .map(|l| l.from.as_str())
            .collect();
        if !unlocks.is_empty() {
            out.push_str(&format!("  unlocks: {}\n", unlocks.join(", ")));
        }
        if !unlocked_by.is_empty() {
            out.push_str(&format!("  unlocked by: {}\n", unlocked_by.join(", ")));
        }
    }
    out
}

/// The graph's epic set: the nodes carrying child progress, in node order.
fn epic_rows(graph: &Graph) -> impl Iterator<Item = &GraphNode> {
    graph.nodes.iter().filter(|n| n.children.is_some())
}

/// The graph's unlocks links, in link order.
fn unlocks_links(graph: &Graph) -> impl Iterator<Item = &GraphLink> {
    graph.links.iter().filter(|l| l.kind == LinkKind::Unlocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::ChildProgress;

    fn node(key: &str, title: &str, category: &str, children: Option<(usize, usize)>) -> GraphNode {
        let status = match category {
            "new" => "To Do",
            "indeterminate" => "In Progress",
            "done" => "Done",
            other => other,
        };
        GraphNode {
            key: key.to_string(),
            backend: "jira:https://example.atlassian.net:PROJ".to_string(),
            title: title.to_string(),
            status: status.to_string(),
            status_category: category.to_string(),
            url: format!("https://example.atlassian.net/browse/{key}"),
            is_epic: children.is_some(),
            children: children.map(|(done, total)| ChildProgress { done, total }),
        }
    }

    fn link(from: &str, to: &str, kind: LinkKind) -> GraphLink {
        GraphLink {
            from: from.to_string(),
            to: to.to_string(),
            kind,
        }
    }

    /// PROJ-1 and PROJ-3 unlock PROJ-2, PROJ-1 unlocks the plain ticket
    /// PROJ-9, and PROJ-2 has the child PROJ-20.
    fn sample() -> Graph {
        Graph {
            nodes: vec![
                node("PROJ-1", "Build the base", "done", Some((2, 2))),
                node("PROJ-2", "Ship the feature", "indeterminate", Some((0, 1))),
                node("PROJ-3", "Hire the team", "new", Some((0, 0))),
                node("PROJ-9", "Spike the API", "new", None),
                node("PROJ-20", "A child task", "new", None),
            ],
            links: vec![
                link("PROJ-1", "PROJ-2", LinkKind::Unlocks),
                link("PROJ-3", "PROJ-2", LinkKind::Unlocks),
                link("PROJ-1", "PROJ-9", LinkKind::Unlocks),
                link("PROJ-2", "PROJ-20", LinkKind::Parent),
            ],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn text_lists_each_epic_with_its_unlocks_both_ways() {
        assert_eq!(
            render_text(&sample()),
            "PROJ-1  Done  Build the base  (2/2)\n\
             \x20 unlocks: PROJ-2, PROJ-9\n\
             PROJ-2  In Progress  Ship the feature  (0/1)\n\
             \x20 unlocked by: PROJ-1, PROJ-3\n\
             PROJ-3  To Do  Hire the team  (0/0)\n\
             \x20 unlocks: PROJ-2\n"
        );
    }
}
