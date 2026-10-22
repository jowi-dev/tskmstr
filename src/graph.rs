//! The epic unlock graph behind `tm graph` (GitHub issue #154).
//!
//! An epic graph has two kinds of link. A `parent` link says an epic is
//! made of a ticket. An `unlocks` link says finishing one ticket makes
//! another possible: it is a `Blocks` issue link, drawn from the blocker
//! to the blocked ticket. The structure is a graph, not a tree: an epic
//! can be unlocked by several others and can unlock several others.
//!
//! [`build_graph`] reads everything through [`TicketProvider`], so it works
//! the same on every backend: one epic listing, one batched dependency
//! fetch, then one [`TicketProvider::children`] call per epic in the graph.
//! It never follows links one fetch at a time.

mod render;

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use thiserror::Error;

use crate::config::BackendIdentity;
use crate::ticketing::error::ProviderError;
use crate::ticketing::provider::{TicketProvider, TicketQuery};
use crate::ticketing::types::{Issue, LinkedIssue, Status};
use crate::work::comment_store::iso8601_utc;

pub use render::{render_mermaid, render_text};

/// The status category key for finished work.
const DONE_CATEGORY: &str = "done";

/// The issue link type name that [`LinkKind::Unlocks`] links come from.
const BLOCKS_LINK_TYPE: &str = "Blocks";

/// The version of the JSON format [`GraphDocument`] serializes to. Its own
/// number, separate from the repo config schema version. Adding an
/// optional field keeps it; removing, renaming, or changing the meaning of
/// a field bumps it.
pub const GRAPH_SCHEMA_VERSION: u32 = 1;

/// The `tm graph --json` document: a [`Graph`] plus the version, time, and
/// backend it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphDocument {
    /// Always [`GRAPH_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// When the graph was built, as `YYYY-MM-DDTHH:MM:SSZ`.
    pub generated_at: String,
    /// The backend and project the graph was read from.
    pub source: GraphSource,
    /// See [`Graph::nodes`].
    pub nodes: Vec<GraphNode>,
    /// See [`Graph::links`].
    pub links: Vec<GraphLink>,
    /// See [`Graph::warnings`].
    pub warnings: Vec<GraphWarning>,
}

impl GraphDocument {
    /// Wrap `graph`, read from the backend `identity`, as built at
    /// `now_secs` seconds since the Unix epoch.
    pub fn new(graph: Graph, identity: &BackendIdentity, now_secs: i64) -> Self {
        let source = match identity {
            BackendIdentity::Jira { project_key, .. } => GraphSource {
                provider: "jira".to_string(),
                project: project_key.clone(),
            },
            BackendIdentity::Github { repo } => GraphSource {
                provider: "github".to_string(),
                project: repo.clone(),
            },
        };
        Self {
            schema_version: GRAPH_SCHEMA_VERSION,
            generated_at: iso8601_utc(now_secs),
            source,
            nodes: graph.nodes,
            links: graph.links,
            warnings: graph.warnings,
        }
    }
}

/// Where a [`GraphDocument`] was read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphSource {
    /// The backend: `jira` or `github`.
    pub provider: String,
    /// The Jira project key, or the GitHub `owner/repo`.
    pub project: String,
}

/// An epic graph held in memory: what [`build_graph`] returns and what
/// the renderers read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Graph {
    /// Every node, epics in listing order first, then other tickets in the
    /// order they were first seen. Each key appears once.
    pub nodes: Vec<GraphNode>,
    /// Every link, in the order it was first seen. Each
    /// `(from, to, kind)` appears once.
    pub links: Vec<GraphLink>,
    /// Problems found in the graph's shape, such as unlocks cycles.
    pub warnings: Vec<GraphWarning>,
}

/// One ticket in the graph: an epic, or a plain ticket that is an epic's
/// child or the far end of an epic's unlocks link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphNode {
    /// Ticket key, e.g. `PROJ-12`, `GH-12`, or `owner/repo#12`.
    pub key: String,
    /// The backend the key belongs to, as [`BackendIdentity::scope`].
    pub backend: String,
    /// One-line summary.
    pub title: String,
    /// Workflow status name, e.g. `In Progress`.
    pub status: String,
    /// Status category key: `new`, `indeterminate`, or `done`.
    pub status_category: String,
    /// Browsable URL for the ticket.
    pub url: String,
    /// Whether the backend says this ticket is an epic.
    pub is_epic: bool,
    /// Child progress for an epic in the graph's epic set; `None` (JSON
    /// `null`) for every other node.
    pub children: Option<ChildProgress>,
}

/// How many of an epic's children are done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ChildProgress {
    /// Children whose status category is `done`.
    pub done: usize,
    /// All children.
    pub total: usize,
}

/// A directed link between two nodes, by key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct GraphLink {
    /// For [`LinkKind::Parent`], the epic. For [`LinkKind::Unlocks`], the
    /// blocker.
    pub from: String,
    /// For [`LinkKind::Parent`], the child. For [`LinkKind::Unlocks`], the
    /// ticket it unlocks.
    pub to: String,
    /// What the link means.
    pub kind: LinkKind,
}

/// The meaning of a [`GraphLink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// `from` is an epic made of the ticket `to`.
    Parent,
    /// Finishing `from` makes `to` possible (`from` blocks `to`).
    Unlocks,
}

/// A problem found in the graph's shape. The graph is still complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphWarning {
    /// Machine-readable kind. Today always `cycle`.
    pub kind: String,
    /// The keys involved, sorted.
    pub keys: Vec<String>,
    /// A one-line explanation for people.
    pub message: String,
}

/// Errors from [`build_graph`].
#[derive(Debug, Error)]
pub enum GraphError {
    /// A ticket provider call failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// The focus key names a ticket that is not an epic, or no ticket.
    #[error("{key} is not an epic")]
    NotAnEpic {
        /// The focus key as given.
        key: String,
    },

    /// The focus key names a Done epic that the graph leaves out.
    #[error("{key} is a Done epic with no unlocks link to an open epic, so it is not in the graph")]
    DoneEpicNotInGraph {
        /// The epic's key.
        key: String,
    },
}

/// Build the epic graph for `project_key`.
///
/// The graph holds every open epic, plus every Done epic with an unlocks
/// link to or from an open epic. Each epic's `Blocks` links become unlocks
/// links; a link whose far end is a plain ticket adds that ticket as a
/// leaf node built from the link's own data. Each epic's children become
/// nodes with a parent link, and set the epic's [`ChildProgress`].
///
/// With `focus`, only the part of the graph connected to that epic
/// through unlocks links (in either direction, through epics only) is
/// kept, plus those epics' children. A focus key that is not an epic in
/// the graph is a [`GraphError::NotAnEpic`] or
/// [`GraphError::DoneEpicNotInGraph`].
///
/// Unlocks cycles do not fail the build: each one becomes a
/// [`GraphWarning`].
pub fn build_graph(
    provider: &dyn TicketProvider,
    identity: &BackendIdentity,
    project_key: &str,
    focus: Option<&str>,
) -> Result<Graph, GraphError> {
    let mut epics: Vec<Issue> = provider
        .search(&TicketQuery::Epics {
            project_key: project_key.to_string(),
        })?
        .issues
        .into_iter()
        .filter(|issue| provider.is_epic(issue))
        .collect();
    provider.hydrate_dependencies(&mut epics)?;

    let included = included_epics(&epics);
    let mut builder = GraphBuilder::new(provider, identity);
    for epic in epics.iter().filter(|e| included.contains(e.key.as_str())) {
        builder.add_node(builder.node_from_issue(epic, true));
    }
    let listed: HashSet<&str> = epics.iter().map(|e| e.key.as_str()).collect();
    for epic in epics.iter().filter(|e| included.contains(e.key.as_str())) {
        for (other, epic_blocks_other) in blocks_links(epic) {
            if listed.contains(other.key.as_str()) {
                if !included.contains(other.key.as_str()) {
                    continue;
                }
            } else {
                builder.add_node(builder.node_from_linked(other));
            }
            let (from, to) = if epic_blocks_other {
                (&epic.key, &other.key)
            } else {
                (&other.key, &epic.key)
            };
            builder.add_link(from, to, LinkKind::Unlocks);
        }
    }
    for epic in epics.iter().filter(|e| included.contains(e.key.as_str())) {
        let children = provider.children(&epic.key)?;
        let done = children
            .iter()
            .filter(|c| is_done(&c.fields.status))
            .count();
        builder.set_progress(
            &epic.key,
            ChildProgress {
                done,
                total: children.len(),
            },
        );
        for child in &children {
            builder.add_child_node(builder.node_from_issue(child, provider.is_epic(child)));
            builder.add_link(&epic.key, &child.key, LinkKind::Parent);
        }
    }
    let graph = builder.finish();
    let mut graph = match focus {
        Some(key) => focus_on(graph, provider, key)?,
        None => graph,
    };
    graph.warnings = cycle_warnings(&graph);
    Ok(graph)
}

/// One `cycle` warning per loop of unlocks links: each strongly connected
/// component with more than one member, and each ticket that unlocks
/// itself. Sorted by first key.
fn cycle_warnings(graph: &Graph) -> Vec<GraphWarning> {
    let mut warnings: Vec<GraphWarning> = unlocks_components(graph)
        .into_iter()
        .filter(|component| {
            component.len() > 1
                || graph.links.iter().any(|l| {
                    l.kind == LinkKind::Unlocks && l.from == component[0] && l.to == component[0]
                })
        })
        .map(|mut keys| {
            keys.sort();
            GraphWarning {
                kind: "cycle".to_string(),
                message: format!("unlocks cycle among {}", keys.join(", ")),
                keys,
            }
        })
        .collect();
    warnings.sort_by(|a, b| a.keys.cmp(&b.keys));
    warnings
}

/// The strongly connected components of the graph's unlocks links, by
/// Tarjan's algorithm with an explicit stack so deep chains cannot
/// overflow the call stack.
fn unlocks_components(graph: &Graph) -> Vec<Vec<String>> {
    let index_of: HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.key.as_str(), i))
        .collect();
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
    for link in graph.links.iter().filter(|l| l.kind == LinkKind::Unlocks) {
        if let (Some(&from), Some(&to)) = (
            index_of.get(link.from.as_str()),
            index_of.get(link.to.as_str()),
        ) {
            edges[from].push(to);
        }
    }

    let count = graph.nodes.len();
    let mut order: Vec<Option<usize>> = vec![None; count];
    let mut low = vec![0; count];
    let mut on_stack = vec![false; count];
    let mut stack = Vec::new();
    let mut next_order = 0;
    let mut components = Vec::new();
    for root in 0..count {
        if order[root].is_some() {
            continue;
        }
        // Each frame is a node and how many of its edges are explored.
        let mut frames = vec![(root, 0)];
        order[root] = Some(next_order);
        low[root] = next_order;
        next_order += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (node, ref mut explored)) = frames.last_mut() {
            if let Some(&next) = edges[node].get(*explored) {
                *explored += 1;
                match order[next] {
                    None => {
                        order[next] = Some(next_order);
                        low[next] = next_order;
                        next_order += 1;
                        stack.push(next);
                        on_stack[next] = true;
                        frames.push((next, 0));
                    }
                    Some(next_order_seen) if on_stack[next] => {
                        low[node] = low[node].min(next_order_seen);
                    }
                    Some(_) => {}
                }
                continue;
            }
            frames.pop();
            if let Some(&(caller, _)) = frames.last() {
                low[caller] = low[caller].min(low[node]);
            }
            if Some(low[node]) == order[node] {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component.push(graph.nodes[member].key.clone());
                    if member == node {
                        break;
                    }
                }
                components.push(component);
            }
        }
    }
    components
}

/// Keep only the part of `graph` connected to the epic `key` (matched
/// case-insensitively) through unlocks links in either direction, plus
/// the kept epics' children. The walk passes through epics only: a plain
/// ticket next to a kept epic is kept, but its other links are not
/// followed.
fn focus_on(graph: Graph, provider: &dyn TicketProvider, key: &str) -> Result<Graph, GraphError> {
    let Some(start) = graph
        .nodes
        .iter()
        .find(|n| n.children.is_some() && n.key.eq_ignore_ascii_case(key))
    else {
        return Err(missing_focus_error(provider, key));
    };
    let epic_set: HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.children.is_some())
        .map(|n| n.key.as_str())
        .collect();
    let mut neighbors: HashMap<&str, Vec<&str>> = HashMap::new();
    for link in graph.links.iter().filter(|l| l.kind == LinkKind::Unlocks) {
        neighbors.entry(&link.from).or_default().push(&link.to);
        neighbors.entry(&link.to).or_default().push(&link.from);
    }
    let mut kept: HashSet<&str> = HashSet::from([start.key.as_str()]);
    let mut queue = vec![start.key.as_str()];
    while let Some(current) = queue.pop() {
        for &next in neighbors.get(current).into_iter().flatten() {
            if kept.insert(next) && epic_set.contains(next) {
                queue.push(next);
            }
        }
    }
    for link in graph.links.iter().filter(|l| l.kind == LinkKind::Parent) {
        if kept.contains(link.from.as_str()) && epic_set.contains(link.from.as_str()) {
            kept.insert(&link.to);
        }
    }
    let kept: HashSet<String> = kept.into_iter().map(str::to_string).collect();
    Ok(Graph {
        nodes: graph
            .nodes
            .into_iter()
            .filter(|n| kept.contains(&n.key))
            .collect(),
        links: graph
            .links
            .into_iter()
            .filter(|l| kept.contains(&l.from) && kept.contains(&l.to))
            .collect(),
        warnings: graph.warnings,
    })
}

/// Why the focus `key` is not an epic in the graph: a Done epic the graph
/// leaves out, or not an epic at all. A key no ticket has is not an epic.
fn missing_focus_error(provider: &dyn TicketProvider, key: &str) -> GraphError {
    match provider.get_issue(key) {
        Ok(issue) if provider.is_epic(&issue) => GraphError::DoneEpicNotInGraph { key: issue.key },
        Ok(_) | Err(ProviderError::NotFound { .. }) => GraphError::NotAnEpic {
            key: key.to_string(),
        },
        Err(err) => GraphError::Provider(err),
    }
}

/// Whether a status counts as finished.
fn is_done(status: &Status) -> bool {
    status.status_category.key == DONE_CATEGORY
}

/// Every `Blocks` link on `issue`, as the far end plus whether `issue` is
/// the blocker (`true`) or the blocked ticket (`false`).
fn blocks_links(issue: &Issue) -> impl Iterator<Item = (&LinkedIssue, bool)> {
    issue
        .fields
        .issue_links
        .iter()
        .filter(|link| link.link_type.name == BLOCKS_LINK_TYPE)
        .flat_map(|link| {
            let blocked_by = link.inward_issue.as_ref().map(|other| (other, false));
            let blocks = link.outward_issue.as_ref().map(|other| (other, true));
            blocked_by.into_iter().chain(blocks)
        })
}

/// The keys of the epics the graph shows: every open epic, plus every
/// Done epic with a `Blocks` link to or from an open epic. The link may
/// be listed on either end, so both ends' link lists are checked.
fn included_epics(epics: &[Issue]) -> HashSet<&str> {
    let open: HashSet<&str> = epics
        .iter()
        .filter(|e| !is_done(&e.fields.status))
        .map(|e| e.key.as_str())
        .collect();
    let mut included = open.clone();
    for epic in epics {
        for (other, _) in blocks_links(epic) {
            let other_key = other.key.as_str();
            if open.contains(epic.key.as_str()) {
                if let Some(listed) = epics.iter().find(|e| e.key == other_key) {
                    included.insert(listed.key.as_str());
                }
            } else if open.contains(other_key) {
                included.insert(epic.key.as_str());
            }
        }
    }
    included
}

/// Collects nodes and links, deduplicating both and keeping first-seen
/// order.
struct GraphBuilder<'a> {
    provider: &'a dyn TicketProvider,
    backend: String,
    nodes: Vec<GraphNode>,
    node_index: HashMap<String, usize>,
    links: Vec<GraphLink>,
    link_set: HashSet<GraphLink>,
}

impl<'a> GraphBuilder<'a> {
    fn new(provider: &'a dyn TicketProvider, identity: &BackendIdentity) -> Self {
        Self {
            provider,
            backend: identity.scope(),
            nodes: Vec::new(),
            node_index: HashMap::new(),
            links: Vec::new(),
            link_set: HashSet::new(),
        }
    }

    fn node(&self, key: &str, title: &str, status: &Status, is_epic: bool) -> GraphNode {
        GraphNode {
            key: key.to_string(),
            backend: self.backend.clone(),
            title: title.to_string(),
            status: status.name.clone(),
            status_category: status.status_category.key.clone(),
            url: self.provider.issue_url(key),
            is_epic,
            children: None,
        }
    }

    fn node_from_issue(&self, issue: &Issue, is_epic: bool) -> GraphNode {
        self.node(
            &issue.key,
            &issue.fields.summary,
            &issue.fields.status,
            is_epic,
        )
    }

    fn node_from_linked(&self, linked: &LinkedIssue) -> GraphNode {
        self.node(
            &linked.key,
            &linked.fields.summary,
            &linked.fields.status,
            false,
        )
    }

    /// Add `node` unless a node with its key already exists.
    fn add_node(&mut self, node: GraphNode) {
        if !self.node_index.contains_key(&node.key) {
            self.node_index.insert(node.key.clone(), self.nodes.len());
            self.nodes.push(node);
        }
    }

    /// Add a child's `node`. A child fetched in full carries more than a
    /// link's embedded data, so it replaces an existing plain-ticket node
    /// with the same key in place; an existing epic node is kept as is.
    fn add_child_node(&mut self, node: GraphNode) {
        match self.node_index.get(&node.key) {
            Some(&index) if !self.nodes[index].is_epic => self.nodes[index] = node,
            Some(_) => {}
            None => self.add_node(node),
        }
    }

    fn set_progress(&mut self, key: &str, progress: ChildProgress) {
        if let Some(&index) = self.node_index.get(key) {
            self.nodes[index].children = Some(progress);
        }
    }

    fn add_link(&mut self, from: &str, to: &str, kind: LinkKind) {
        let link = GraphLink {
            from: from.to_string(),
            to: to.to_string(),
            kind,
        };
        if self.link_set.insert(link.clone()) {
            self.links.push(link);
        }
    }

    fn finish(self) -> Graph {
        Graph {
            nodes: self.nodes,
            links: self.links,
            warnings: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jira::fake::FakeJiraClient;
    use crate::ticketing::types::{
        IssueFields, IssueLink, IssueLinkType, LinkedIssueFields, SearchResult, StatusCategory,
    };

    fn status(category: &str) -> Status {
        let name = match category {
            "new" => "To Do",
            "indeterminate" => "In Progress",
            _ => "Done",
        };
        Status {
            name: name.to_string(),
            status_category: StatusCategory {
                key: category.to_string(),
            },
        }
    }

    fn issue(key: &str, category: &str, issue_links: Vec<IssueLink>) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: format!("Summary for {key}"),
                status: status(category),
                description: None,
                assignee: None,
                issue_links,
                parent: None,
                labels: Vec::new(),
                issue_type: None,
            },
        }
    }

    fn linked(key: &str, category: &str) -> LinkedIssue {
        LinkedIssue {
            key: key.to_string(),
            fields: LinkedIssueFields {
                summary: format!("Summary for {key}"),
                status: status(category),
            },
        }
    }

    fn blocks_type() -> IssueLinkType {
        IssueLinkType {
            name: "Blocks".to_string(),
            inward: "is blocked by".to_string(),
            outward: "blocks".to_string(),
        }
    }

    /// A link on the issue it is attached to, saying `key` blocks it.
    fn blocked_by(key: &str, category: &str) -> IssueLink {
        IssueLink {
            id: format!("in-{key}"),
            link_type: blocks_type(),
            inward_issue: Some(linked(key, category)),
            outward_issue: None,
        }
    }

    /// A link on the issue it is attached to, saying it blocks `key`.
    fn blocks(key: &str, category: &str) -> IssueLink {
        IssueLink {
            id: format!("out-{key}"),
            link_type: blocks_type(),
            inward_issue: None,
            outward_issue: Some(linked(key, category)),
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

    fn jira_identity() -> BackendIdentity {
        BackendIdentity::Jira {
            base_url: "https://example.atlassian.net".to_string(),
            project_key: "PROJ".to_string(),
        }
    }

    fn build(fake: &FakeJiraClient, focus: Option<&str>) -> Result<Graph, GraphError> {
        build_graph(fake, &jira_identity(), "PROJ", focus)
    }

    fn keys(graph: &Graph) -> Vec<&str> {
        graph.nodes.iter().map(|n| n.key.as_str()).collect()
    }

    fn unlocks(from: &str, to: &str) -> GraphLink {
        GraphLink {
            from: from.to_string(),
            to: to.to_string(),
            kind: LinkKind::Unlocks,
        }
    }

    fn parent(from: &str, to: &str) -> GraphLink {
        GraphLink {
            from: from.to_string(),
            to: to.to_string(),
            kind: LinkKind::Parent,
        }
    }

    /// Seed `child` as a plain ticket under `epic`.
    fn with_child(fake: FakeJiraClient, epic: &str, child: Issue) -> FakeJiraClient {
        let key = child.key.clone();
        fake.with_issue(&key, child).with_parent(&key, epic)
    }

    fn node<'a>(graph: &'a Graph, key: &str) -> &'a GraphNode {
        graph
            .nodes
            .iter()
            .find(|n| n.key == key)
            .unwrap_or_else(|| panic!("no node {key}"))
    }

    #[test]
    fn epic_unlocked_by_two_epics_has_both_incoming_links() {
        let fake = fake_with_epics(vec![
            issue("PROJ-1", "new", vec![blocks("PROJ-3", "new")]),
            issue("PROJ-2", "new", vec![blocks("PROJ-3", "new")]),
            issue(
                "PROJ-3",
                "new",
                vec![blocked_by("PROJ-1", "new"), blocked_by("PROJ-2", "new")],
            ),
        ]);

        let graph = build(&fake, None).unwrap();

        assert_eq!(keys(&graph), ["PROJ-1", "PROJ-2", "PROJ-3"]);
        assert_eq!(
            graph.links,
            vec![unlocks("PROJ-1", "PROJ-3"), unlocks("PROJ-2", "PROJ-3")]
        );
        let epic = node(&graph, "PROJ-1");
        assert!(epic.is_epic);
        assert_eq!(epic.backend, "jira:https://example.atlassian.net:PROJ");
        assert_eq!(epic.url, "https://example.atlassian.net/browse/PROJ-1");
        assert_eq!(epic.title, "Summary for PROJ-1");
        assert_eq!(epic.status, "To Do");
        assert_eq!(epic.status_category, "new");
    }

    #[test]
    fn done_epic_is_kept_only_when_linked_to_an_open_epic() {
        let fake = fake_with_epics(vec![
            issue("PROJ-1", "done", vec![blocks("PROJ-2", "new")]),
            issue("PROJ-2", "new", vec![blocked_by("PROJ-1", "done")]),
            issue("PROJ-3", "done", vec![]),
            issue("PROJ-4", "done", vec![blocks("PROJ-3", "done")]),
        ]);

        let graph = build(&fake, None).unwrap();

        assert_eq!(keys(&graph), ["PROJ-1", "PROJ-2"]);
        assert_eq!(graph.links, vec![unlocks("PROJ-1", "PROJ-2")]);
    }

    #[test]
    fn done_epic_linked_only_from_the_open_epic_side_is_kept() {
        let fake = fake_with_epics(vec![
            issue("PROJ-1", "done", vec![]),
            issue("PROJ-2", "new", vec![blocked_by("PROJ-1", "done")]),
        ]);

        let graph = build(&fake, None).unwrap();

        assert_eq!(keys(&graph), ["PROJ-1", "PROJ-2"]);
        assert_eq!(graph.links, vec![unlocks("PROJ-1", "PROJ-2")]);
    }

    #[test]
    fn plain_ticket_endpoint_is_a_leaf_node_from_link_data() {
        let fake = fake_with_epics(vec![issue(
            "PROJ-1",
            "new",
            vec![
                blocked_by("PROJ-9", "indeterminate"),
                blocks("PROJ-8", "done"),
            ],
        )]);

        let graph = build(&fake, None).unwrap();

        assert_eq!(keys(&graph), ["PROJ-1", "PROJ-9", "PROJ-8"]);
        assert_eq!(
            graph.links,
            vec![unlocks("PROJ-9", "PROJ-1"), unlocks("PROJ-1", "PROJ-8")]
        );
        let plain = node(&graph, "PROJ-9");
        assert!(!plain.is_epic);
        assert_eq!(plain.children, None);
        assert_eq!(plain.title, "Summary for PROJ-9");
        assert_eq!(plain.status, "In Progress");
        assert_eq!(plain.status_category, "indeterminate");
        assert_eq!(plain.url, "https://example.atlassian.net/browse/PROJ-9");
    }

    #[test]
    fn children_get_parent_links_and_set_epic_progress() {
        let fake = fake_with_epics(vec![
            issue("PROJ-1", "new", vec![]),
            issue("PROJ-2", "indeterminate", vec![]),
        ]);
        let fake = with_child(fake, "PROJ-1", issue("PROJ-10", "done", vec![]));
        let fake = with_child(fake, "PROJ-1", issue("PROJ-11", "new", vec![]));

        let graph = build(&fake, None).unwrap();

        assert_eq!(keys(&graph), ["PROJ-1", "PROJ-2", "PROJ-10", "PROJ-11"]);
        assert_eq!(
            graph.links,
            vec![parent("PROJ-1", "PROJ-10"), parent("PROJ-1", "PROJ-11")]
        );
        assert_eq!(
            node(&graph, "PROJ-1").children,
            Some(ChildProgress { done: 1, total: 2 })
        );
        assert_eq!(
            node(&graph, "PROJ-2").children,
            Some(ChildProgress { done: 0, total: 0 })
        );
        let child = node(&graph, "PROJ-10");
        assert!(!child.is_epic);
        assert_eq!(child.children, None);
        assert_eq!(child.status_category, "done");
    }

    #[test]
    fn child_that_is_also_an_unlocks_endpoint_is_one_node_with_both_links() {
        let fake = fake_with_epics(vec![
            issue("PROJ-1", "new", vec![]),
            issue(
                "PROJ-2",
                "new",
                vec![blocked_by("PROJ-10", "indeterminate")],
            ),
        ]);
        let mut child = issue("PROJ-10", "indeterminate", vec![]);
        child.fields.summary = "Full child title".to_string();
        let fake = with_child(fake, "PROJ-1", child);

        let graph = build(&fake, None).unwrap();

        assert_eq!(keys(&graph), ["PROJ-1", "PROJ-2", "PROJ-10"]);
        assert_eq!(node(&graph, "PROJ-10").title, "Full child title");
        assert_eq!(
            graph.links,
            vec![unlocks("PROJ-10", "PROJ-2"), parent("PROJ-1", "PROJ-10")]
        );
    }

    /// PROJ-1 unlocks PROJ-2, PROJ-3 unlocks PROJ-2, PROJ-1 and PROJ-5
    /// both unlock the plain ticket PROJ-9, and PROJ-4 stands alone with a
    /// child. PROJ-6 is a Done epic with no links.
    fn focus_fixture() -> FakeJiraClient {
        let fake = fake_with_epics(vec![
            issue(
                "PROJ-1",
                "new",
                vec![blocks("PROJ-2", "new"), blocks("PROJ-9", "new")],
            ),
            issue(
                "PROJ-2",
                "new",
                vec![blocked_by("PROJ-1", "new"), blocked_by("PROJ-3", "new")],
            ),
            issue("PROJ-3", "new", vec![blocks("PROJ-2", "new")]),
            issue("PROJ-4", "new", vec![]),
            issue("PROJ-5", "new", vec![blocks("PROJ-9", "new")]),
            issue("PROJ-6", "done", vec![]),
        ]);
        let fake = with_child(fake, "PROJ-2", issue("PROJ-20", "new", vec![]));
        with_child(fake, "PROJ-4", issue("PROJ-40", "new", vec![]))
    }

    #[test]
    fn focus_keeps_epics_reachable_either_way_and_their_children() {
        let fake = focus_fixture();

        let graph = build(&fake, Some("proj-1")).unwrap();

        assert_eq!(
            keys(&graph),
            ["PROJ-1", "PROJ-2", "PROJ-3", "PROJ-9", "PROJ-20"]
        );
        assert_eq!(
            graph.links,
            vec![
                unlocks("PROJ-1", "PROJ-2"),
                unlocks("PROJ-1", "PROJ-9"),
                unlocks("PROJ-3", "PROJ-2"),
                parent("PROJ-2", "PROJ-20"),
            ]
        );
    }

    #[test]
    fn focus_on_a_plain_ticket_is_not_an_epic() {
        let fake = focus_fixture();

        let err = build(&fake, Some("PROJ-9")).unwrap_err();

        assert!(matches!(err, GraphError::NotAnEpic { .. }));
        assert_eq!(err.to_string(), "PROJ-9 is not an epic");
    }

    #[test]
    fn focus_on_an_unknown_key_is_not_an_epic() {
        let fake = focus_fixture();

        let err = build(&fake, Some("PROJ-404")).unwrap_err();

        assert_eq!(err.to_string(), "PROJ-404 is not an epic");
    }

    #[test]
    fn focus_on_an_excluded_done_epic_says_why_it_is_missing() {
        let fake = focus_fixture();

        let err = build(&fake, Some("PROJ-6")).unwrap_err();

        assert_eq!(
            err.to_string(),
            "PROJ-6 is a Done epic with no unlocks link to an open epic, so it is not in the graph"
        );
    }

    #[test]
    fn unlocks_cycle_finishes_with_one_warning() {
        let fake = fake_with_epics(vec![
            issue(
                "PROJ-2",
                "new",
                vec![blocks("PROJ-1", "new"), blocked_by("PROJ-1", "new")],
            ),
            issue(
                "PROJ-1",
                "new",
                vec![blocks("PROJ-2", "new"), blocked_by("PROJ-2", "new")],
            ),
            issue("PROJ-3", "new", vec![blocked_by("PROJ-1", "new")]),
        ]);

        let graph = build(&fake, None).unwrap();

        assert_eq!(
            graph.warnings,
            vec![GraphWarning {
                kind: "cycle".to_string(),
                keys: vec!["PROJ-1".to_string(), "PROJ-2".to_string()],
                message: "unlocks cycle among PROJ-1, PROJ-2".to_string(),
            }]
        );
    }

    #[test]
    fn longer_cycles_and_self_loops_each_warn_once() {
        let fake = fake_with_epics(vec![
            issue("PROJ-1", "new", vec![blocks("PROJ-2", "new")]),
            issue("PROJ-2", "new", vec![blocks("PROJ-3", "new")]),
            issue("PROJ-3", "new", vec![blocks("PROJ-1", "new")]),
            issue("PROJ-4", "new", vec![blocks("PROJ-4", "new")]),
            issue("PROJ-5", "new", vec![blocks("PROJ-1", "new")]),
        ]);

        let graph = build(&fake, None).unwrap();

        let cycles: Vec<Vec<String>> = graph.warnings.iter().map(|w| w.keys.clone()).collect();
        assert_eq!(
            cycles,
            vec![
                vec![
                    "PROJ-1".to_string(),
                    "PROJ-2".to_string(),
                    "PROJ-3".to_string()
                ],
                vec!["PROJ-4".to_string()],
            ]
        );
    }

    #[test]
    fn focus_drops_cycles_outside_the_kept_part() {
        let fake = fake_with_epics(vec![
            issue("PROJ-1", "new", vec![]),
            issue("PROJ-2", "new", vec![blocks("PROJ-3", "new")]),
            issue("PROJ-3", "new", vec![blocks("PROJ-2", "new")]),
        ]);

        let graph = build(&fake, Some("PROJ-1")).unwrap();

        assert!(graph.warnings.is_empty());
    }

    #[test]
    fn document_serializes_the_versioned_json_shape() {
        let fake = fake_with_epics(vec![issue(
            "PROJ-1",
            "new",
            vec![blocks("PROJ-9", "indeterminate")],
        )]);
        let fake = with_child(fake, "PROJ-1", issue("PROJ-10", "done", vec![]));
        let graph = build(&fake, None).unwrap();

        let doc = GraphDocument::new(graph, &jira_identity(), 0);
        let json = serde_json::to_value(&doc).unwrap();

        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["generated_at"], "1970-01-01T00:00:00Z");
        assert_eq!(
            json["source"],
            serde_json::json!({"provider": "jira", "project": "PROJ"})
        );
        assert_eq!(
            json["nodes"][0],
            serde_json::json!({
                "key": "PROJ-1",
                "backend": "jira:https://example.atlassian.net:PROJ",
                "title": "Summary for PROJ-1",
                "status": "To Do",
                "status_category": "new",
                "url": "https://example.atlassian.net/browse/PROJ-1",
                "is_epic": true,
                "children": {"done": 1, "total": 1},
            })
        );
        assert_eq!(json["nodes"][1]["key"], "PROJ-9");
        assert_eq!(json["nodes"][1]["is_epic"], false);
        assert!(json["nodes"][1]["children"].is_null());
        assert_eq!(
            json["links"],
            serde_json::json!([
                {"from": "PROJ-1", "to": "PROJ-9", "kind": "unlocks"},
                {"from": "PROJ-1", "to": "PROJ-10", "kind": "parent"},
            ])
        );
        assert_eq!(json["warnings"], serde_json::json!([]));
    }

    #[test]
    fn document_source_names_the_github_repo() {
        let identity = BackendIdentity::Github {
            repo: "owner/repo".to_string(),
        };

        let doc = GraphDocument::new(Graph::default(), &identity, 0);

        assert_eq!(doc.source.provider, "github");
        assert_eq!(doc.source.project, "owner/repo");
    }

    #[test]
    fn cycle_warning_serializes_with_kind_keys_and_message() {
        let warning = GraphWarning {
            kind: "cycle".to_string(),
            keys: vec!["PROJ-1".to_string(), "PROJ-2".to_string()],
            message: "unlocks cycle among PROJ-1, PROJ-2".to_string(),
        };

        assert_eq!(
            serde_json::to_value(&warning).unwrap(),
            serde_json::json!({
                "kind": "cycle",
                "keys": ["PROJ-1", "PROJ-2"],
                "message": "unlocks cycle among PROJ-1, PROJ-2",
            })
        );
    }

    #[test]
    fn github_outgoing_only_link_is_found_with_one_dependency_fetch() {
        use crate::github::gh_cli::{
            FakeGhCli, IssueDependencies, IssueInfo, IssueRef, IssueState,
        };
        use crate::ticketing::github_provider::GithubProvider;

        let epic = |number: u64| IssueInfo {
            number,
            url: format!("https://github.com/owner/repo/issues/{number}"),
            title: format!("Epic {number}"),
            body: String::new(),
            state: IssueState::Open,
            labels: vec!["tm:epic".to_string()],
            assignees: Vec::new(),
        };
        let fake = FakeGhCli::new()
            .with_issue_list(Ok(vec![epic(1), epic(2), epic(3)]))
            .with_issues_dependencies(
                1,
                IssueDependencies {
                    blocking: vec![IssueRef {
                        number: 2,
                        title: "Epic 2".to_string(),
                        state: IssueState::Open,
                        url: String::new(),
                    }],
                    ..Default::default()
                },
            );
        let provider = GithubProvider::new(&fake, "owner/repo".to_string());
        let identity = BackendIdentity::Github {
            repo: "owner/repo".to_string(),
        };

        let graph = build_graph(&provider, &identity, "owner/repo", None).unwrap();

        assert_eq!(keys(&graph), ["GH-1", "GH-2", "GH-3"]);
        assert_eq!(graph.links, vec![unlocks("GH-1", "GH-2")]);
        assert_eq!(node(&graph, "GH-2").backend, "github:owner/repo");
        assert_eq!(
            fake.issues_dependencies_calls(),
            vec![("owner/repo".to_string(), vec![1, 2, 3])]
        );
    }
}
