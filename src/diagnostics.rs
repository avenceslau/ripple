use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<PathBuf>,
}

pub fn cycles<T>(graph: &BTreeMap<T, BTreeSet<T>>) -> Vec<Vec<T>>
where
    T: Clone + Ord,
{
    struct State<T> {
        next_index: usize,
        indices: BTreeMap<T, usize>,
        low_links: BTreeMap<T, usize>,
        stack: Vec<T>,
        on_stack: BTreeSet<T>,
        components: Vec<Vec<T>>,
    }

    fn visit<T>(node: T, graph: &BTreeMap<T, BTreeSet<T>>, state: &mut State<T>)
    where
        T: Clone + Ord,
    {
        let index = state.next_index;
        state.next_index += 1;
        state.indices.insert(node.clone(), index);
        state.low_links.insert(node.clone(), index);
        state.stack.push(node.clone());
        state.on_stack.insert(node.clone());

        for dependency in graph.get(&node).into_iter().flatten() {
            if !state.indices.contains_key(dependency) {
                visit(dependency.clone(), graph, state);
                let low = state.low_links[&node].min(state.low_links[dependency]);
                state.low_links.insert(node.clone(), low);
            } else if state.on_stack.contains(dependency) {
                let low = state.low_links[&node].min(state.indices[dependency]);
                state.low_links.insert(node.clone(), low);
            }
        }

        if state.low_links[&node] == state.indices[&node] {
            let mut component = Vec::new();
            loop {
                let member = state
                    .stack
                    .pop()
                    .expect("cycle stack must contain current node");
                state.on_stack.remove(&member);
                component.push(member.clone());
                if member == node {
                    break;
                }
            }

            let self_cycle = component.len() == 1
                && graph
                    .get(&component[0])
                    .is_some_and(|dependencies| dependencies.contains(&component[0]));
            if component.len() > 1 || self_cycle {
                component.sort();
                state.components.push(component);
            }
        }
    }

    let mut state = State {
        next_index: 0,
        indices: BTreeMap::new(),
        low_links: BTreeMap::new(),
        stack: Vec::new(),
        on_stack: BTreeSet::new(),
        components: Vec::new(),
    };
    let nodes: BTreeSet<_> = graph
        .keys()
        .cloned()
        .chain(graph.values().flatten().cloned())
        .collect();
    for node in nodes {
        if !state.indices.contains_key(&node) {
            visit(node, graph, &mut state);
        }
    }
    state.components.sort();
    state.components
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_strongly_connected_components() {
        let graph = BTreeMap::from([
            ("a", BTreeSet::from(["b"])),
            ("b", BTreeSet::from(["a", "c"])),
            ("c", BTreeSet::new()),
        ]);

        assert_eq!(cycles(&graph), vec![vec!["a", "b"]]);
    }
}
