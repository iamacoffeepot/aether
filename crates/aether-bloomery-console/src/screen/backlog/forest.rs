//! Dependency DAG as an indented forest, roots at column zero.
//!
//! A cycle is an explicit band. The walk never drops a cyclic edge by
//! omitting it from the output.

use std::collections::{HashMap, HashSet};

use super::label::workpiece_key;

/// One row of the indented forest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForestRow {
    pub id: String,
    pub depth: usize,
}

/// Forest walk plus any cycles found among the listed workpieces.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Forest {
    pub rows: Vec<ForestRow>,
    pub cycles: Vec<Vec<String>>,
}

/// Build the forest from workpiece ids and each id's declared dependencies.
///
/// `dependencies[id]` lists workpieces `id` depends on. Edges whose other
/// end is not in `ids` are ignored. Sibling order is workpiece-id order.
#[must_use]
pub fn forest(ids: &[String], dependencies: &HashMap<String, Vec<String>>) -> Forest {
    let present: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut inbound: HashMap<&str, usize> = ids.iter().map(|id| (id.as_str(), 0usize)).collect();
    let mut edges: Vec<(&str, &str)> = Vec::new();

    for id in ids {
        let Some(deps) = dependencies.get(id) else {
            continue;
        };
        for dep in deps {
            if !present.contains(dep.as_str()) {
                continue;
            }
            edges.push((id.as_str(), dep.as_str()));
            children.entry(dep.as_str()).or_default().push(id.as_str());
            if let Some(count) = inbound.get_mut(id.as_str()) {
                *count += 1;
            }
        }
    }
    for kids in children.values_mut() {
        kids.sort_unstable_by_key(|id| workpiece_key(id));
        kids.dedup();
    }

    let sccs = strongly_connected(ids, &edges);
    let mut cycles = Vec::new();
    let mut cyclic: HashSet<&str> = HashSet::new();
    for scc in &sccs {
        if is_cyclic_scc(scc, &edges) {
            for member in scc {
                cyclic.insert(*member);
            }
            cycles.push(cycle_band(scc, &edges));
        }
    }

    let mut roots: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| inbound.get(id).copied().unwrap_or(0) == 0 || cyclic.contains(id))
        .collect();
    roots.sort_unstable_by_key(|id| workpiece_key(id));
    roots.dedup();

    let mut rows = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut stack: Vec<(&str, usize)> = roots.into_iter().rev().map(|id| (id, 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        rows.push(ForestRow { id: id.to_owned(), depth });
        let Some(kids) = children.get(id) else {
            continue;
        };
        for child in kids.iter().rev() {
            if cyclic.contains(id) && cyclic.contains(child) {
                continue;
            }
            if !seen.contains(child) {
                stack.push((*child, depth + 1));
            }
        }
    }
    for id in ids {
        if seen.contains(id.as_str()) {
            continue;
        }
        rows.push(ForestRow { id: id.clone(), depth: 0 });
    }

    Forest { rows, cycles }
}

fn is_cyclic_scc(scc: &[&str], edges: &[(&str, &str)]) -> bool {
    if scc.len() > 1 {
        return true;
    }
    let Some(id) = scc.first() else {
        return false;
    };
    edges.iter().any(|(from, to)| from == id && to == id)
}

fn cycle_band(scc: &[&str], edges: &[(&str, &str)]) -> Vec<String> {
    let set: HashSet<&str> = scc.iter().copied().collect();
    let mut next: HashMap<&str, &str> = HashMap::new();
    for (from, to) in edges {
        if set.contains(from) && set.contains(to) {
            next.entry(*from).or_insert(*to);
        }
    }
    let start = scc.iter().copied().min_by_key(|id| workpiece_key(id)).unwrap_or("");
    let mut band = vec![start.to_owned()];
    let mut cursor = start;
    let mut guard = 0;
    while let Some(step) = next.get(cursor).copied() {
        band.push(step.to_owned());
        if step == start {
            break;
        }
        cursor = step;
        guard += 1;
        if guard > scc.len() + 1 {
            break;
        }
    }
    if band.last().map(String::as_str) != Some(start) && !band.is_empty() {
        band.push(start.to_owned());
    }
    band
}

fn strongly_connected<'a>(ids: &'a [String], edges: &[(&'a str, &'a str)]) -> Vec<Vec<&'a str>> {
    enum Frame<'b> {
        Enter(&'b str),
        Finish(&'b str),
    }

    let mut index_of: HashMap<&str, usize> = HashMap::new();
    let mut lowlink: HashMap<&str, usize> = HashMap::new();
    let mut on_stack: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut index = 0;
    let mut sccs = Vec::new();

    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for (from, to) in edges {
        adj.entry(*from).or_default().push(*to);
    }

    for id in ids {
        if index_of.contains_key(id.as_str()) {
            continue;
        }
        let mut work = vec![Frame::Enter(id.as_str())];
        while let Some(frame) = work.pop() {
            match frame {
                Frame::Enter(node) => {
                    if index_of.contains_key(node) {
                        continue;
                    }
                    index_of.insert(node, index);
                    lowlink.insert(node, index);
                    index += 1;
                    stack.push(node);
                    on_stack.insert(node);
                    let neighbors = adj.get(node).map_or(&[][..], Vec::as_slice);
                    work.push(Frame::Finish(node));
                    for neighbor in neighbors.iter().rev() {
                        work.push(Frame::Enter(neighbor));
                    }
                }
                Frame::Finish(node) => {
                    for neighbor in adj.get(node).into_iter().flatten() {
                        if on_stack.contains(neighbor) {
                            let neighbor_low = lowlink.get(neighbor).copied().unwrap_or(usize::MAX);
                            if let Some(low) = lowlink.get_mut(node) {
                                *low = (*low).min(neighbor_low);
                            }
                        }
                    }
                    if lowlink.get(node) == index_of.get(node) {
                        let mut scc = Vec::new();
                        while let Some(popped) = stack.pop() {
                            on_stack.remove(popped);
                            scc.push(popped);
                            if popped == node {
                                break;
                            }
                        }
                        scc.sort_unstable_by_key(|id| workpiece_key(id));
                        sccs.push(scc);
                    }
                }
            }
        }
    }
    sccs
}

/// One painted cycle band: `a → b → a`.
#[must_use]
pub fn cycle_line(cycle: &[String]) -> String {
    format!("cycle  {}", cycle.join(" → "))
}

#[cfg(test)]
mod tests {
    use super::{cycle_line, forest};
    use std::collections::HashMap;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn deps(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(id, listed)| ((*id).to_owned(), listed.iter().map(|dep| (*dep).to_owned()).collect()))
            .collect()
    }

    #[test]
    fn a_cyclic_fixture_renders_the_cycle_band() {
        // The plausible bug: a back-edge is dropped so the pane paints an
        // acyclic tree and the operator never sees the deadlock.
        let walked = forest(&ids(&["wp-a", "wp-b"]), &deps(&[("wp-a", &["wp-b"]), ("wp-b", &["wp-a"])]));
        assert_eq!(walked.cycles.len(), 1, "the two-node cycle must be a band: {walked:?}");
        let line = cycle_line(&walked.cycles[0]);
        assert!(line.contains("wp-a"), "{line}");
        assert!(line.contains("wp-b"), "{line}");
        assert!(line.starts_with("cycle  "), "{line}");
        assert!(line.contains('→'), "{line}");
        let listed: Vec<&str> = walked.rows.iter().map(|row| row.id.as_str()).collect();
        assert!(listed.contains(&"wp-a"), "{listed:?}");
        assert!(listed.contains(&"wp-b"), "{listed:?}");
    }

    #[test]
    fn an_acyclic_tree_indents_dependents_under_roots() {
        // The plausible bug: dependents paint at column zero, so the forest
        // is a flat list and the operator cannot see what blocks what.
        let walked = forest(&ids(&["root", "child", "other"]), &deps(&[("child", &["root"])]));
        assert!(walked.cycles.is_empty(), "{walked:?}");
        assert_eq!(
            walked.rows.iter().map(|row| (row.id.as_str(), row.depth)).collect::<Vec<_>>(),
            vec![("other", 0), ("root", 0), ("child", 1)]
        );
    }
}
