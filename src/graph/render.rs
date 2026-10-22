//! Plain-text and Mermaid views of a [`Graph`], for `tm graph` and
//! `tm graph --mermaid`. Pure functions: they build a `String` and print
//! nothing.

use std::collections::HashSet;

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

/// The Mermaid view: a `flowchart LR` of the epics and their unlocks
/// links. Children show only as the `(done/total)` count in each epic's
/// label. A plain ticket at the end of an unlocks link is drawn with a
/// rounded stadium shape, an epic with a rectangle. Each node is styled by
/// its status category with the `todo`, `inprogress`, and `done` classes.
pub fn render_mermaid(graph: &Graph) -> String {
    let endpoints: HashSet<&str> = unlocks_links(graph)
        .flat_map(|l| [l.from.as_str(), l.to.as_str()])
        .collect();
    let mut out = String::from("flowchart LR\n");
    for (class, style) in MERMAID_CLASSES {
        out.push_str(&format!("    classDef {class} {style}\n"));
    }
    for node in &graph.nodes {
        let id = mermaid_id(&node.key);
        let class = mermaid_class(&node.status_category);
        match node.children {
            Some(progress) => {
                let label = format!(
                    "{}: {} ({}/{})",
                    node.key, node.title, progress.done, progress.total
                );
                out.push_str(&format!(
                    "    {id}[\"{}\"]:::{class}\n",
                    mermaid_label(&label)
                ));
            }
            None if endpoints.contains(node.key.as_str()) => {
                let label = format!("{}: {}", node.key, node.title);
                out.push_str(&format!(
                    "    {id}([\"{}\"]):::{class}\n",
                    mermaid_label(&label)
                ));
            }
            None => {}
        }
    }
    for link in unlocks_links(graph) {
        out.push_str(&format!(
            "    {} --> {}\n",
            mermaid_id(&link.from),
            mermaid_id(&link.to)
        ));
    }
    out
}

/// The Mermaid style classes, by name, for each status category.
const MERMAID_CLASSES: [(&str, &str); 3] = [
    ("todo", "fill:#f1f3f5,stroke:#868e96,color:#212529"),
    ("inprogress", "fill:#fff3bf,stroke:#f08c00,color:#212529"),
    ("done", "fill:#d3f9d8,stroke:#2b8a3e,color:#212529"),
];

/// The style class for a status category; anything unknown is `todo`.
fn mermaid_class(status_category: &str) -> &'static str {
    match status_category {
        "indeterminate" => "inprogress",
        "done" => "done",
        _ => "todo",
    }
}

/// A Mermaid node id for `key`: every character that is not an ASCII
/// letter or digit becomes `_`, so `owner/repo#12` is `owner_repo_12`.
fn mermaid_id(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// `text` made safe inside a double-quoted Mermaid label, using Mermaid's
/// entity codes. `#` goes first, since the quote's own code contains one.
/// Brackets and pipes are safe inside quotes and pass through.
fn mermaid_label(text: &str) -> String {
    text.replace('#', "#35;").replace('"', "#quot;")
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

    fn lines(rendered: &str) -> Vec<&str> {
        rendered.lines().map(str::trim).collect()
    }

    #[test]
    fn mermaid_draws_epics_plain_endpoints_and_unlocks_only() {
        let rendered = render_mermaid(&sample());

        let lines = lines(&rendered);
        assert_eq!(lines[0], "flowchart LR");
        assert!(lines.contains(&r#"PROJ_1["PROJ-1: Build the base (2/2)"]:::done"#));
        assert!(lines.contains(&r#"PROJ_2["PROJ-2: Ship the feature (0/1)"]:::inprogress"#));
        assert!(lines.contains(&r#"PROJ_3["PROJ-3: Hire the team (0/0)"]:::todo"#));
        assert!(lines.contains(&r#"PROJ_9(["PROJ-9: Spike the API"]):::todo"#));
        assert!(lines.contains(&"PROJ_1 --> PROJ_2"));
        assert!(lines.contains(&"PROJ_3 --> PROJ_2"));
        assert!(lines.contains(&"PROJ_1 --> PROJ_9"));
        assert!(!rendered.contains("PROJ_20"), "child nodes are not drawn");
        assert!(!rendered.contains("A child task"));
        for class in ["todo", "inprogress", "done"] {
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with(&format!("classDef {class} "))),
                "missing classDef {class}"
            );
        }
    }

    #[test]
    fn mermaid_sanitizes_owner_repo_keys_and_escapes_hash() {
        let graph = Graph {
            nodes: vec![
                node("GH-1", "Epic", "new", Some((0, 0))),
                node("owner/repo#12", "Elsewhere", "done", None),
            ],
            links: vec![link("owner/repo#12", "GH-1", LinkKind::Unlocks)],
            warnings: Vec::new(),
        };

        let rendered = render_mermaid(&graph);

        let lines = lines(&rendered);
        assert!(lines.contains(&r#"owner_repo_12(["owner/repo#35;12: Elsewhere"]):::done"#));
        assert!(lines.contains(&"owner_repo_12 --> GH_1"));
    }

    #[test]
    fn mermaid_escapes_quotes_and_keeps_brackets_and_pipes_inside_quotes() {
        let graph = Graph {
            nodes: vec![node(
                "PROJ-1",
                r#"Say "hi" [now] | later #1"#,
                "new",
                Some((0, 0)),
            )],
            links: Vec::new(),
            warnings: Vec::new(),
        };

        let rendered = render_mermaid(&graph);

        assert!(
            lines(&rendered).contains(
                &r#"PROJ_1["PROJ-1: Say #quot;hi#quot; [now] | later #35;1 (0/0)"]:::todo"#
            ),
            "{rendered}"
        );
    }

    #[test]
    fn mermaid_styles_an_unknown_status_category_as_todo() {
        let graph = Graph {
            nodes: vec![node("PROJ-1", "Odd", "undefined", Some((0, 0)))],
            links: Vec::new(),
            warnings: Vec::new(),
        };

        let rendered = render_mermaid(&graph);

        assert!(lines(&rendered).contains(&r#"PROJ_1["PROJ-1: Odd (0/0)"]:::todo"#));
    }
}
