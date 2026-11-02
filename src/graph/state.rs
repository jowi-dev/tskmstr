//! Planning state for the epic graph (GitHub issue #177): which epics can
//! start now, how far away the rest are, what each one waits on, and how
//! much work finishing it frees.
//!
//! [`graph_state`] reads a built [`Graph`] and returns data only. Every
//! graph view reads its answers from here, so the views cannot disagree.
//!
//! An epic's status decides whether it is done. An epic whose children
//! are all done but whose status is still In Progress is unfinished.

use super::{DONE_CATEGORY, Graph, GraphNode, strongly_connected, unlocks_edges};
use crate::blocker_stacking::unblock_counts_by_key;

/// Whether an epic can be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// The epic's status category is `done`.
    Done,
    /// Unfinished, and every ticket that unlocks it is done.
    Available,
    /// Unfinished, and waiting on an unfinished ticket that unlocks it, or
    /// part of an unlocks cycle.
    Locked,
}

/// One epic's planning state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpicState {
    /// The epic's key.
    pub key: String,
    /// Whether the epic can be started.
    pub availability: Availability,
    /// How many unfinished tickets stand in line ahead of the epic: the
    /// longest chain of unfinished unlockers, counting plain tickets as
    /// well as epics. `0` for a done or available epic. Finishing an epic
    /// moves the epics after it into earlier waves.
    ///
    /// The members of one unlocks cycle share a wave, at least `1`, since
    /// a cycle never becomes available on its own.
    pub wave: usize,
    /// Every epic this one can be reached from against unlocks links,
    /// done or not, in graph order. The walk passes through epics only.
    pub upstream: Vec<String>,
    /// Every epic reachable from this one along unlocks links, done or
    /// not, in graph order. The walk passes through epics only.
    pub downstream: Vec<String>,
    /// How many unfinished epics finishing this one frees, directly or
    /// transitively, by the same rule `tm ready` orders dispatch with
    /// ([`unblock_counts_by_key`]). Done epics break the chain: an epic
    /// after a done one is not waiting on anything before it. `0` for a
    /// done epic.
    pub downstream_count: usize,
}

/// The planning state of every epic in a [`Graph`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GraphState {
    /// Epics with at least one unlocks link, in graph order.
    pub linked: Vec<EpicState>,
    /// Epics with no unlocks link at all, in graph order, for views that
    /// show them in their own strip. Each is wave `0` with empty sets.
    pub standalone: Vec<EpicState>,
}

impl GraphState {
    /// The state of the epic `key`, linked or standalone.
    pub fn epic(&self, key: &str) -> Option<&EpicState> {
        self.linked
            .iter()
            .chain(&self.standalone)
            .find(|e| e.key == key)
    }
}

/// Compute the planning state of every epic in `graph`: each node with
/// child progress is an epic. Always terminates, cycles included.
pub fn graph_state(graph: &Graph) -> GraphState {
    let nodes = &graph.nodes;
    let is_epic = |i: usize| nodes[i].children.is_some();
    let done: Vec<bool> = nodes
        .iter()
        .map(|n| n.status_category == DONE_CATEGORY)
        .collect();
    let edges = unlocks_edges(graph);
    let waves = waves(nodes.len(), &edges, &done);

    // Upstream and downstream walk epic-to-epic links only, done or not.
    let mut forward = vec![Vec::new(); nodes.len()];
    let mut backward = vec![Vec::new(); nodes.len()];
    for &(from, to) in edges.iter().filter(|&&(f, t)| is_epic(f) && is_epic(t)) {
        forward[from].push(to);
        backward[to].push(from);
    }

    let unfinished: Vec<&str> = (0..nodes.len())
        .filter(|&i| is_epic(i) && !done[i])
        .map(|i| nodes[i].key.as_str())
        .collect();
    let counts = unblock_counts_by_key(
        &unfinished,
        edges
            .iter()
            .map(|&(f, t)| (nodes[f].key.as_str(), nodes[t].key.as_str())),
    )
    .counts;

    let mut state = GraphState::default();
    for i in (0..nodes.len()).filter(|&i| is_epic(i)) {
        let key = &nodes[i].key;
        let availability = match (done[i], waves[i]) {
            (true, _) => Availability::Done,
            (false, 0) => Availability::Available,
            (false, _) => Availability::Locked,
        };
        let epic = EpicState {
            key: key.clone(),
            availability,
            wave: waves[i],
            upstream: reachable_keys(nodes, &backward, i),
            downstream: reachable_keys(nodes, &forward, i),
            downstream_count: counts.get(key).copied().unwrap_or(0),
        };
        if edges.iter().any(|&(f, t)| f == i || t == i) {
            state.linked.push(epic);
        } else {
            state.standalone.push(epic);
        }
    }
    state
}

/// Each node's wave: the longest chain of unfinished unlockers ahead of
/// it, over links whose ends are both unfinished. A cycle's members share
/// one wave, at least `1`.
fn waves(count: usize, edges: &[(usize, usize)], done: &[bool]) -> Vec<usize> {
    let mut active = vec![Vec::new(); count];
    let mut unlockers = vec![Vec::new(); count];
    for &(from, to) in edges.iter().filter(|&&(f, t)| !done[f] && !done[t]) {
        active[from].push(to);
        unlockers[to].push(from);
    }

    let components = strongly_connected(&active);
    let mut component_of = vec![0; count];
    for (c, members) in components.iter().enumerate() {
        for &m in members {
            component_of[m] = c;
        }
    }

    let mut wave = vec![0; count];
    // Reversed, components come in topological order, so every unlocker
    // outside a component has its wave before the component is reached.
    for (c, members) in components.iter().enumerate().rev() {
        let cyclic = members.len() > 1 || active[members[0]].contains(&members[0]);
        let ahead = members
            .iter()
            .flat_map(|&m| &unlockers[m])
            .filter(|&&u| component_of[u] != c)
            .map(|&u| wave[u] + 1)
            .max()
            .unwrap_or(0);
        let w = if cyclic { ahead.max(1) } else { ahead };
        for &m in members {
            wave[m] = w;
        }
    }
    wave
}

/// The keys of every node reachable from `start` along `adjacency`, not
/// counting `start` itself, in graph order.
fn reachable_keys(nodes: &[GraphNode], adjacency: &[Vec<usize>], start: usize) -> Vec<String> {
    let mut seen = vec![false; nodes.len()];
    let mut stack = adjacency[start].clone();
    while let Some(i) = stack.pop() {
        if !seen[i] {
            seen[i] = true;
            stack.extend(&adjacency[i]);
        }
    }
    seen[start] = false;
    (0..nodes.len())
        .filter(|&i| seen[i])
        .map(|i| nodes[i].key.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocker_stacking::unblock_counts;
    use crate::graph::{ChildProgress, GraphLink, LinkKind};
    use crate::ticketing::types::{
        Issue, IssueFields, IssueLink, IssueLinkType, LinkedIssue, LinkedIssueFields, Status,
        StatusCategory,
    };

    fn node(key: &str, category: &str, is_epic: bool) -> GraphNode {
        GraphNode {
            key: key.to_string(),
            backend: "jira:https://example.atlassian.net:PROJ".to_string(),
            title: format!("Title of {key}"),
            status: category.to_string(),
            status_category: category.to_string(),
            url: format!("https://example.atlassian.net/browse/{key}"),
            is_epic,
            children: is_epic.then_some(ChildProgress::default()),
            assignee: None,
        }
    }

    fn epic(key: &str, category: &str) -> GraphNode {
        node(key, category, true)
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

    fn graph(nodes: Vec<GraphNode>, links: Vec<GraphLink>) -> Graph {
        Graph {
            nodes,
            links,
            warnings: Vec::new(),
        }
    }

    /// Shaped like a real project:
    /// - a five-epic chain A1 -> A2 -> A3 -> A4 -> A5
    /// - B is unlocked by three: A3, the available C, and the plain ticket P
    /// - a chain with a done epic in the middle: D1 -> D2 (done) -> D3 -> D4
    /// - standalone epics S1 and S2, S1 with a child
    fn project() -> Graph {
        graph(
            vec![
                epic("A1", "indeterminate"),
                epic("A2", "new"),
                epic("A3", "new"),
                epic("A4", "new"),
                epic("A5", "new"),
                epic("B", "new"),
                epic("C", "new"),
                epic("D1", "new"),
                epic("D2", "done"),
                epic("D3", "new"),
                epic("D4", "new"),
                epic("S1", "new"),
                epic("S2", "indeterminate"),
                node("P", "new", false),
                node("S1-1", "new", false),
            ],
            vec![
                unlocks("A1", "A2"),
                unlocks("A2", "A3"),
                unlocks("A3", "A4"),
                unlocks("A4", "A5"),
                unlocks("A3", "B"),
                unlocks("C", "B"),
                unlocks("P", "B"),
                unlocks("D1", "D2"),
                unlocks("D2", "D3"),
                unlocks("D3", "D4"),
                parent("S1", "S1-1"),
            ],
        )
    }

    fn waves(state: &GraphState) -> Vec<(&str, usize, Availability)> {
        state
            .linked
            .iter()
            .map(|e| (e.key.as_str(), e.wave, e.availability))
            .collect()
    }

    fn keys(state: &[EpicState]) -> Vec<&str> {
        state.iter().map(|e| e.key.as_str()).collect()
    }

    #[test]
    fn waves_and_availability_match_a_project_shaped_fixture() {
        use Availability::*;

        let state = graph_state(&project());

        assert_eq!(
            waves(&state),
            [
                ("A1", 0, Available),
                ("A2", 1, Locked),
                ("A3", 2, Locked),
                ("A4", 3, Locked),
                ("A5", 4, Locked),
                ("B", 3, Locked),
                ("C", 0, Available),
                ("D1", 0, Available),
                ("D2", 0, Done),
                ("D3", 0, Available),
                ("D4", 1, Locked),
            ]
        );
    }

    #[test]
    fn standalone_epics_are_reported_separately() {
        let state = graph_state(&project());

        assert_eq!(keys(&state.standalone), ["S1", "S2"]);
        let s1 = state.epic("S1").unwrap();
        assert_eq!(s1.wave, 0);
        assert_eq!(s1.availability, Availability::Available);
        assert!(s1.upstream.is_empty() && s1.downstream.is_empty());
        assert!(state.epic("P").is_none());
    }

    #[test]
    fn upstream_and_downstream_sets_follow_unlocks_through_epics() {
        let state = graph_state(&project());

        let a3 = state.epic("A3").unwrap();
        assert_eq!(a3.upstream, ["A1", "A2"]);
        assert_eq!(a3.downstream, ["A4", "A5", "B"]);
        let b = state.epic("B").unwrap();
        assert_eq!(b.upstream, ["A1", "A2", "A3", "C"]);
        assert!(b.downstream.is_empty());
        // Sets are structural: the done D2 is named and walked through.
        assert_eq!(state.epic("D1").unwrap().downstream, ["D2", "D3", "D4"]);
        assert_eq!(state.epic("D4").unwrap().upstream, ["D1", "D2", "D3"]);
    }

    #[test]
    fn downstream_counts_free_unfinished_epics_only() {
        let state = graph_state(&project());
        let count = |key: &str| state.epic(key).unwrap().downstream_count;

        assert_eq!(count("A1"), 5);
        assert_eq!(count("A3"), 3);
        assert_eq!(count("C"), 1);
        assert_eq!(count("B"), 0);
        // D2 is done, so D3 is not waiting on D1.
        assert_eq!(count("D1"), 0);
        assert_eq!(count("D2"), 0);
        assert_eq!(count("D3"), 1);
    }

    #[test]
    fn finishing_an_epic_moves_later_epics_into_earlier_waves() {
        let mut graph = project();
        graph.nodes[0].status_category = "done".to_string();

        let state = graph_state(&graph);

        assert_eq!(state.epic("A1").unwrap().availability, Availability::Done);
        assert_eq!(state.epic("A2").unwrap().wave, 0);
        assert_eq!(state.epic("A5").unwrap().wave, 3);
        assert_eq!(state.epic("B").unwrap().wave, 2);
    }

    #[test]
    fn a_two_epic_cycle_shares_a_locked_wave() {
        let state = graph_state(&graph(
            vec![epic("X", "new"), epic("Y", "new"), epic("Z", "new")],
            vec![unlocks("X", "Y"), unlocks("Y", "X"), unlocks("Y", "Z")],
        ));

        assert_eq!(
            waves(&state),
            [
                ("X", 1, Availability::Locked),
                ("Y", 1, Availability::Locked),
                ("Z", 2, Availability::Locked),
            ]
        );
        assert_eq!(state.epic("X").unwrap().upstream, ["Y"]);
        assert_eq!(state.epic("X").unwrap().downstream, ["Y", "Z"]);
        assert_eq!(state.epic("X").unwrap().downstream_count, 2);
    }

    #[test]
    fn a_cycle_after_a_chain_sits_past_its_unlocker() {
        let state = graph_state(&graph(
            vec![epic("W", "new"), epic("X", "new"), epic("Y", "new")],
            vec![unlocks("W", "X"), unlocks("X", "Y"), unlocks("Y", "X")],
        ));

        assert_eq!(state.epic("X").unwrap().wave, 1);
        assert_eq!(state.epic("Y").unwrap().wave, 1);
    }

    #[test]
    fn a_self_link_is_locked_and_terminates() {
        let state = graph_state(&graph(
            vec![epic("X", "new"), epic("Y", "new")],
            vec![unlocks("X", "X"), unlocks("X", "Y")],
        ));

        assert_eq!(
            waves(&state),
            [
                ("X", 1, Availability::Locked),
                ("Y", 2, Availability::Locked)
            ]
        );
        assert!(state.epic("X").unwrap().upstream.is_empty());
        assert_eq!(state.epic("X").unwrap().downstream, ["Y"]);
    }

    #[test]
    fn a_done_epic_in_a_cycle_does_not_lock_it() {
        let state = graph_state(&graph(
            vec![epic("X", "done"), epic("Y", "new")],
            vec![unlocks("X", "Y"), unlocks("Y", "X")],
        ));

        assert_eq!(
            waves(&state),
            [
                ("X", 0, Availability::Done),
                ("Y", 0, Availability::Available)
            ]
        );
    }

    fn status(category: &str) -> Status {
        Status {
            name: category.to_string(),
            status_category: StatusCategory {
                key: category.to_string(),
            },
        }
    }

    /// `key` as `tm ready` sees it: an issue with one inward `Blocks` link
    /// per blocker.
    fn ready_issue(key: &str, blockers: &[&str]) -> Issue {
        let issue_links = blockers
            .iter()
            .map(|blocker| IssueLink {
                id: format!("{blocker}-{key}"),
                link_type: IssueLinkType {
                    name: "Blocks".to_string(),
                    inward: "is blocked by".to_string(),
                    outward: "blocks".to_string(),
                },
                inward_issue: Some(LinkedIssue {
                    key: blocker.to_string(),
                    fields: LinkedIssueFields {
                        summary: String::new(),
                        status: status("new"),
                    },
                }),
                outward_issue: None,
            })
            .collect();
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: String::new(),
                status: status("new"),
                description: None,
                assignee: None,
                issue_links,
                parent: None,
                labels: Vec::new(),
                issue_type: None,
            },
        }
    }

    #[test]
    fn downstream_counts_agree_with_tm_ready_on_the_same_links() {
        let graph = project();
        let unfinished: Vec<&GraphNode> = graph
            .nodes
            .iter()
            .filter(|n| n.children.is_some() && n.status_category != DONE_CATEGORY)
            .collect();
        let issues: Vec<Issue> = unfinished
            .iter()
            .map(|n| {
                let blockers: Vec<&str> = graph
                    .links
                    .iter()
                    .filter(|l| l.kind == LinkKind::Unlocks && l.to == n.key)
                    .map(|l| l.from.as_str())
                    .collect();
                ready_issue(&n.key, &blockers)
            })
            .collect();

        let ready = unblock_counts(&issues);
        let state = graph_state(&graph);

        for n in unfinished {
            let epic = state.epic(&n.key).unwrap();
            assert_eq!(
                Some(&epic.downstream_count),
                ready.counts.get(&n.key),
                "{}",
                n.key
            );
        }
    }
}
