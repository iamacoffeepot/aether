//! Time-sliced corridor graph over a cost-to-reach field (issue 1858).
//! The pure core the `build_corridor_graph` transform (ADR-0048) wraps,
//! kept beside the reachability solver so the follow-on passes (path-snap,
//! the validation harness) reuse the exact per-tick component labeler for
//! id parity.
//!
//! Given the solved cost-to-reach field `V` (a [`ScalarField`], issue
//! 1857) and a budget `B`, the corridor graph is the connectivity
//! skeleton of `V`: per tick, the connected components of the affordable
//! set `{cell : V(cell, tick) <= B}` (the nodes); intra-tick "punch" edges
//! between two components a sub-budget barrier separates, priced at the
//! sublevel-filtration threshold of `V` at which raising `B` fuses them;
//! and inter-tick "flow" edges linking a component at tick `t` to one at
//! `t + 1` when an affordable one-tick stencil step bridges them.
//!
//! Each tick runs one iterative union-find sublevel filtration over the
//! tick's `V` slice: visit cells in ascending `V`, union each with its
//! already-visited stencil neighbors, and record the `V` threshold at the
//! moment two distinct affordable components first connect. The partition
//! restricted to cells with `V <= B` is the tick's components; a merge
//! event whose threshold exceeds `B` and joins two distinct `<= B`
//! components is a punch edge priced at that threshold. Union-find stays
//! iterative (find is a loop with path compression, union by rank — no
//! recursion, per the load-bearing-code rule), so a given `V` + `B`
//! replays byte-identically.

use std::collections::BTreeMap;

use aether_kinds::{
    CorridorEdge, CorridorGraph, CorridorNode, EdgeKind, ForkDepth, ResolutionDepth, ScalarField,
    StencilOffset,
};

use crate::reachability::UNREACHABLE;

/// An iterative disjoint-set forest (union by rank, path-compressing
/// find). No recursion: `find` walks parent links in a loop. `rank` is
/// the usual union-by-rank height bound. Cells are `usize` grid indices.
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
            rank: vec![0u8; len],
        }
    }

    /// Representative of `x`'s set, compressing the path to the root.
    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    /// Union the sets containing `a` and `b`, returning the surviving
    /// representative. A no-op (returns the shared root) when they already
    /// share one.
    fn union(&mut self, a: usize, b: usize) -> usize {
        let root_a = self.find(a);
        let root_b = self.find(b);
        if root_a == root_b {
            return root_a;
        }
        let (keep, drop) = if self.rank[root_a] < self.rank[root_b] {
            (root_b, root_a)
        } else {
            (root_a, root_b)
        };
        self.parent[drop] = keep;
        if self.rank[keep] == self.rank[drop] {
            self.rank[keep] += 1;
        }
        keep
    }
}

/// Cell indices with a finite `V`, ordered ascending by `(value, index)` —
/// the visit order of the sublevel filtration, stable across builds.
fn ascending_finite_cells(slice: &[u32]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..slice.len())
        .filter(|&i| slice[i] != UNREACHABLE)
        .collect();
    order.sort_by_key(|&i| (slice[i], i));
    order
}

/// In-bounds cell `(x + dx, y + dy)` of cell `index`, or `None` if it
/// falls off the grid. The signed deltas carry both the forward stencil
/// step (intra-tick connectivity, the forward flow landing) and the
/// reverse step (a flow landing's predecessor).
fn offset_neighbor(
    index: usize,
    dx: isize,
    dy: isize,
    width: usize,
    height: usize,
) -> Option<usize> {
    let x = index % width;
    let y = index / width;
    let nx = x.checked_add_signed(dx)?;
    let ny = y.checked_add_signed(dy)?;
    if nx >= width || ny >= height {
        return None;
    }
    Some(ny * width + nx)
}

/// Forward in-bounds stencil neighbor of `index`, or `None` for the zero
/// offset (a cell is not its own stencil neighbor for connectivity) or an
/// off-grid step.
fn stencil_neighbor(
    index: usize,
    offset: StencilOffset,
    width: usize,
    height: usize,
) -> Option<usize> {
    if offset.dx == 0 && offset.dy == 0 {
        return None;
    }
    offset_neighbor(index, offset.dx as isize, offset.dy as isize, width, height)
}

/// The per-tick component labeling of one `V` slice (issue 1858). Exposed
/// crate-wide so the path-snap pass reuses the exact same labeling for id
/// parity.
pub struct TickComponents {
    /// Per-cell component id (`Some(id)` for an affordable cell, `None`
    /// for an above-budget or unreachable cell). Length `width * height`.
    pub label: Vec<Option<u32>>,
    /// The component summary nodes for this tick, ordered by `component`.
    pub nodes: Vec<CorridorNode>,
    /// Intra-tick punch merges `(component_a, component_b, threshold)`,
    /// `component_a < component_b`, deduplicated to the minimum threshold.
    pub punches: Vec<(u32, u32, u32)>,
}

/// Label one tick's affordable components and detect its punch merges.
///
/// `slice` is the tick's row-major `V` values (length `width * height`).
/// Affordable cells (`V <= budget`, never the [`UNREACHABLE`] sentinel)
/// are partitioned into connected components under the stencil adjacency;
/// component ids are assigned in row-major first-encounter order. A
/// sublevel filtration over every finite cell then records, for each pair
/// of components a barrier separates, the minimum `V` threshold at which
/// they merge — a punch edge.
pub fn label_tick_components(
    slice: &[u32],
    width: usize,
    height: usize,
    stencil: &[StencilOffset],
    budget: u32,
    tick: u32,
) -> TickComponents {
    let plane = slice.len();
    let affordable = |i: usize| slice[i] != UNREACHABLE && slice[i] <= budget;

    // Components: union affordable cells with their affordable stencil
    // neighbors. Connectivity is order-independent, so a single row-major
    // sweep suffices; the filtration below reuses the same adjacency.
    let mut comp_uf = UnionFind::new(plane);
    for i in 0..plane {
        if !affordable(i) {
            continue;
        }
        for &offset in stencil {
            if let Some(n) = stencil_neighbor(i, offset, width, height)
                && affordable(n)
            {
                comp_uf.union(i, n);
            }
        }
    }

    // Assign per-tick component ids in row-major first-encounter order so
    // the labeling is deterministic and content-addressable.
    let mut root_to_comp: BTreeMap<usize, u32> = BTreeMap::new();
    let mut label: Vec<Option<u32>> = vec![None; plane];
    let mut nodes: Vec<CorridorNode> = Vec::new();
    for i in 0..plane {
        if !affordable(i) {
            continue;
        }
        let root = comp_uf.find(i);
        let comp = *root_to_comp.entry(root).or_insert_with(|| {
            let id = u32::try_from(nodes.len()).expect("component count fits u32");
            nodes.push(CorridorNode {
                tick,
                component: id,
                cell_count: 0,
                min_cost: UNREACHABLE,
            });
            id
        });
        label[i] = Some(comp);
        let node = &mut nodes[comp as usize];
        node.cell_count = node.cell_count.saturating_add(1);
        node.min_cost = node.min_cost.min(slice[i]);
    }

    // Punch edges: a sublevel filtration over every finite cell. Visiting
    // ascending, each newly-active cell unions with its already-active
    // neighbors; the threshold of a union is the current (larger) cell's
    // `V`. `root_comp[root]` carries the minimum component id among the
    // affordable cells in that filtration set, so a union above the budget
    // that joins two distinct components is the punch — priced at the
    // threshold at which raising the budget would fuse them.
    let mut punch_map: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    // A punch joins two distinct affordable components, so a tick with
    // fewer than two has none — skip the filtration sweep entirely (the
    // common single-basin case).
    if nodes.len() >= 2 {
        let order = ascending_finite_cells(slice);
        let mut filt = UnionFind::new(plane);
        let mut active = vec![false; plane];
        let mut root_comp: Vec<Option<u32>> = vec![None; plane];
        for &cell in &order {
            active[cell] = true;
            root_comp[filt.find(cell)] = label[cell];
            let threshold = slice[cell];
            for &offset in stencil {
                let Some(n) = stencil_neighbor(cell, offset, width, height) else {
                    continue;
                };
                if !active[n] {
                    continue;
                }
                let root_cell = filt.find(cell);
                let root_neighbor = filt.find(n);
                if root_cell == root_neighbor {
                    continue;
                }
                let comp_cell = root_comp[root_cell];
                let comp_neighbor = root_comp[root_neighbor];
                if threshold > budget
                    && let (Some(left), Some(right)) = (comp_cell, comp_neighbor)
                    && left != right
                {
                    let key = if left < right {
                        (left, right)
                    } else {
                        (right, left)
                    };
                    punch_map
                        .entry(key)
                        .and_modify(|t| *t = (*t).min(threshold))
                        .or_insert(threshold);
                }
                let root = filt.union(cell, n);
                // Carry the minimum component id of the merged set forward.
                let merged = match (comp_cell, comp_neighbor) {
                    (Some(left), Some(right)) => Some(left.min(right)),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                };
                root_comp[root] = merged;
            }
        }
    }

    let punches: Vec<(u32, u32, u32)> = punch_map
        .into_iter()
        .map(|((a, b), threshold)| (a, b, threshold))
        .collect();

    TickComponents {
        label,
        nodes,
        punches,
    }
}

/// Inter-tick flow edges between adjacent ticks' affordable components.
///
/// A flow links a component at the previous tick to one at this tick when
/// an affordable cell of the former can stencil-step onto an affordable
/// cell of the latter. `overlap_width` is the count of distinct affordable
/// landing cells the bridge reaches for that component pair — how wide the
/// pinch/branch is — so each landing cell counts once per distinct source
/// component that reaches it (#1869 reads it as the scrub datum).
///
/// Counted from the landing side: for each affordable cell in `curr`,
/// gather the distinct source components among its stencil predecessors in
/// `prev` (the reverse step `landing - offset`, plus the zero "stay put"
/// step so a held component links to itself), and credit the cell once to
/// each. Returns `((from_comp, to_comp), overlap_width)` sorted by pair.
fn flow_edges(
    prev: &[Option<u32>],
    curr: &[Option<u32>],
    width: usize,
    height: usize,
    stencil: &[StencilOffset],
) -> Vec<((u32, u32), u32)> {
    let mut counts: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    // Distinct source components reaching the current landing cell; the
    // stencil is small, so a reused scratch Vec dedups faster than a set.
    let mut sources: Vec<u32> = Vec::new();
    for (landing, to) in curr.iter().enumerate() {
        let Some(to_comp) = *to else { continue };
        sources.clear();
        for &offset in stencil {
            let source_cell = if offset.dx == 0 && offset.dy == 0 {
                Some(landing)
            } else {
                offset_neighbor(
                    landing,
                    -(offset.dx as isize),
                    -(offset.dy as isize),
                    width,
                    height,
                )
            };
            let Some(source_cell) = source_cell else {
                continue;
            };
            if let Some(from_comp) = prev[source_cell]
                && !sources.contains(&from_comp)
            {
                sources.push(from_comp);
            }
        }
        for &from_comp in &sources {
            *counts.entry((from_comp, to_comp)).or_insert(0) += 1;
        }
    }
    counts.into_iter().collect()
}

/// Build the corridor graph of a cost-to-reach field `V` under a fixed
/// budget (issue 1858).
///
/// `field` is the solved [`ScalarField`] `V` (row-major `(tick, y, x)`,
/// [`UNREACHABLE`] marking a cell no path reaches); `stencil` is the
/// one-tick movement offset set shared with the solver; `budget` is the
/// affordability threshold `B`. The result is a time-layered DAG: nodes
/// are the per-tick connected components of `{cell : V <= B}`, ordered by
/// `(tick, component)`; edges are the inter-tick `Flow` links and the
/// intra-tick `Punch` merges, emitted sorted by endpoints then kind so the
/// encoded output is byte-stable. A malformed (too-short) field stops at
/// the first truncated tick rather than panicking.
pub fn build_corridor_graph_core(
    field: &ScalarField,
    stencil: &[StencilOffset],
    budget: u32,
) -> CorridorGraph {
    let width = field.width as usize;
    let height = field.height as usize;
    let ticks = field.ticks as usize;
    let plane = width.saturating_mul(height);

    let mut nodes: Vec<CorridorNode> = Vec::new();
    let mut edges: Vec<CorridorEdge> = Vec::new();
    if plane == 0 || ticks == 0 {
        return CorridorGraph { nodes, edges };
    }

    let mut prev_label: Vec<Option<u32>> = Vec::new();
    let mut prev_node_base: u32 = 0;
    let mut prev_has_components = false;

    for t in 0..ticks {
        let base = t.saturating_mul(plane);
        let slice = match field.values.get(base..base.saturating_add(plane)) {
            Some(s) if s.len() == plane => s,
            // A truncated field can't be sliced cleanly past here; stop
            // rather than fabricate components from a short read.
            _ => break,
        };

        let curr_node_base = u32::try_from(nodes.len()).expect("node count fits u32");
        let tick = u32::try_from(t).expect("tick index fits u32");
        let TickComponents {
            label,
            nodes: tick_nodes,
            punches,
        } = label_tick_components(slice, width, height, stencil, budget, tick);
        let curr_has_components = !tick_nodes.is_empty();
        nodes.extend(tick_nodes);

        for (a, b, threshold) in punches {
            edges.push(CorridorEdge {
                from: curr_node_base + a,
                to: curr_node_base + b,
                kind: EdgeKind::Punch,
                price: threshold,
                overlap_width: 0,
            });
        }

        if t > 0 && prev_has_components && curr_has_components {
            for ((a, b), overlap_width) in flow_edges(&prev_label, &label, width, height, stencil) {
                edges.push(CorridorEdge {
                    from: prev_node_base + a,
                    to: curr_node_base + b,
                    kind: EdgeKind::Flow,
                    price: 0,
                    overlap_width,
                });
            }
        }

        prev_label = label;
        prev_node_base = curr_node_base;
        prev_has_components = curr_has_components;
    }

    // Byte-stable output: sort edges by endpoints then kind (flow before
    // punch), with price / overlap as final tiebreakers.
    edges.sort_by(|x, y| {
        let kind_ord = |k: &EdgeKind| match k {
            EdgeKind::Flow => 0u8,
            EdgeKind::Punch => 1u8,
        };
        (x.from, x.to, kind_ord(&x.kind), x.price, x.overlap_width).cmp(&(
            y.from,
            y.to,
            kind_ord(&y.kind),
            y.price,
            y.overlap_width,
        ))
    });

    CorridorGraph { nodes, edges }
}

/// The final time layer of a [`CorridorGraph`] — the maximum `tick` over
/// its nodes — or `None` when the graph has no node. A node at this tick
/// is a terminus a live path must reach.
fn final_tick(graph: &CorridorGraph) -> Option<u32> {
    graph.nodes.iter().map(|n| n.tick).max()
}

/// Mark the **live** nodes of a [`CorridorGraph`]: a node is live when a
/// chain of `Flow` edges reaches a final-tick node. `Flow` edges point
/// forward in time (`from` at tick `t`, `to` at tick `t + 1`), so liveness
/// propagates *backward*: seed the final-tick nodes, then walk each `Flow`
/// edge from `to` to `from`, marking `from` live whenever `to` is live.
///
/// A node in the graph (reachable-affordable) that is not live is a
/// **dead-end**: it has no affordable one-tick continuation that reaches
/// the final tick. The result is indexed parallel to `graph.nodes`.
///
/// The graph is a time-layered DAG with ticks strictly decreasing along the
/// backward walk, so the worklist drains in bounded iterations with no
/// recursion (per the load-bearing-code rule). The per-node `live` flag
/// guards re-enqueue, so each node is processed once.
pub fn live_nodes(graph: &CorridorGraph) -> Vec<bool> {
    let node_count = graph.nodes.len();
    let mut live = vec![false; node_count];
    let Some(last) = final_tick(graph) else {
        return live;
    };

    // Predecessor adjacency over `Flow` edges only: for each node, the
    // `from` endpoints of the flow edges that land on it. Liveness flows
    // from a node to its flow predecessors.
    let mut flow_predecessors: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    for edge in &graph.edges {
        if edge.kind != EdgeKind::Flow {
            continue;
        }
        let from = edge.from as usize;
        let to = edge.to as usize;
        if from < node_count && to < node_count {
            flow_predecessors[to].push(from);
        }
    }

    let mut worklist: Vec<usize> = Vec::new();
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.tick == last {
            live[idx] = true;
            worklist.push(idx);
        }
    }

    while let Some(node) = worklist.pop() {
        for &pred in &flow_predecessors[node] {
            if !live[pred] {
                live[pred] = true;
                worklist.push(pred);
            }
        }
    }

    live
}

/// Compute the per-fork resolution depth of a [`CorridorGraph`] given its
/// [`live_nodes`] mask. A **fork** is a live node with a `Flow` edge into a
/// dead-end node; its resolution depth is the longest `Flow`-path within
/// the dead-end subtree hanging off it — how many ticks the dead-end region
/// stays affordable past the fork before it terminates. That depth is
/// exactly how far past the fork a bounded-horizon traversal must see to
/// tell the dead-end apart from a through branch.
///
/// Longest dead-end path is a topological DP over the DAG. Because `Flow`
/// edges always advance the tick by one, longest-path-in-ticks from a
/// dead-end node `n` is `1 + max` over its dead-end flow successors (`0`
/// when `n` has none), so processing nodes in descending tick order fills
/// the table in one pass with no recursion. A fork's depth is `1 + max`
/// over the longest dead-end paths of the dead-end nodes its flow edges
/// enter (the `+ 1` counts the fork's own step into the dead-end region).
///
/// Returns the forks in ascending `node_index` order so the output is
/// byte-stable and content-addressable.
pub fn fork_resolution_depths(graph: &CorridorGraph, live: &[bool]) -> Vec<ForkDepth> {
    let node_count = graph.nodes.len();
    if node_count == 0 {
        return Vec::new();
    }

    // Dead-end flow successors of each dead-end node: a `Flow` edge whose
    // `from` and `to` are both dead-end nodes stays within the dead-end
    // subtree. (A flow edge out of a dead-end node can only reach another
    // dead-end node — a flow successor of a dead-end is never live, since
    // liveness would have propagated backward to the predecessor.)
    let mut dead_successors: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    // Forward dead-end flow targets of every node (live or dead): the
    // dead-end nodes a node's flow edges enter. A live node with a
    // non-empty list is a fork.
    let mut dead_flow_targets: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    for edge in &graph.edges {
        if edge.kind != EdgeKind::Flow {
            continue;
        }
        let from = edge.from as usize;
        let to = edge.to as usize;
        if from >= node_count || to >= node_count || live[to] {
            continue;
        }
        // `to` is a dead-end node.
        dead_flow_targets[from].push(to);
        if !live[from] {
            dead_successors[from].push(to);
        }
    }

    // Longest `Flow`-path length (in ticks) within the dead-end subtree
    // rooted at each dead-end node. Topological DP: process dead-end nodes
    // in descending tick order so every flow successor (tick `+ 1`) is
    // already resolved. Indices into a node list sorted by descending tick.
    let mut order: Vec<usize> = (0..node_count).filter(|&i| !live[i]).collect();
    order.sort_by(|&a, &b| graph.nodes[b].tick.cmp(&graph.nodes[a].tick));

    let mut longest_dead_path = vec![0u32; node_count];
    for &node in &order {
        let mut best = 0u32;
        for &succ in &dead_successors[node] {
            best = best.max(longest_dead_path[succ].saturating_add(1));
        }
        longest_dead_path[node] = best;
    }

    let mut forks: Vec<ForkDepth> = Vec::new();
    for (idx, &is_live) in live.iter().enumerate() {
        if !is_live {
            continue;
        }
        let mut depth = 0u32;
        for &target in &dead_flow_targets[idx] {
            // `+ 1` counts the fork's own step into the dead-end region;
            // `longest_dead_path[target]` is the rest of the branch.
            depth = depth.max(longest_dead_path[target].saturating_add(1));
        }
        if depth > 0 {
            forks.push(ForkDepth {
                node_index: u32::try_from(idx).expect("node index fits u32"),
                depth,
            });
        }
    }

    // Ascending `node_index` keeps the output byte-stable; the source loop
    // already visits nodes in ascending order, so this is a no-op guard
    // against future reordering.
    forks.sort_by_key(|f| f.node_index);
    forks
}

/// The fork resolution depth of a [`CorridorGraph`]: the maximum lookahead
/// a bounded-horizon traversal must use before committing to an affordable
/// one-tick step. Two passes — [`live_nodes`] then [`fork_resolution_depths`]
/// — over the landed graph; `max_resolution_depth` is the `max` fork depth
/// (`0` when the graph has no fork).
pub fn corridor_resolution_depth_core(graph: &CorridorGraph) -> ResolutionDepth {
    let live = live_nodes(graph);
    let forks = fork_resolution_depths(graph, &live);
    let max_resolution_depth = forks.iter().map(|f| f.depth).max().unwrap_or(0);
    ResolutionDepth {
        max_resolution_depth,
        forks,
    }
}

#[cfg(test)]
mod tests {
    use super::build_corridor_graph_core;
    use crate::test_support::{flow_in, flow_out, stencil_4way};
    use aether_kinds::{EdgeKind, ScalarField};

    const UNREACHABLE: u32 = u32::MAX;

    /// One uniform-cost affordable basin per tick → exactly one node per
    /// tick and a flow edge per tick step.
    #[test]
    fn single_basin_is_one_node_per_tick() {
        let width = 3;
        let height = 1;
        let ticks = 3;
        let field = ScalarField {
            width,
            height,
            ticks,
            values: vec![1u32; (width * height * ticks) as usize],
        };
        let graph = build_corridor_graph_core(&field, &stencil_4way(), 10);
        assert_eq!(graph.nodes.len(), 3, "one node per tick");
        for (t, node) in graph.nodes.iter().enumerate() {
            assert_eq!(node.tick, u32::try_from(t).expect("tick fits u32"));
            assert_eq!(node.component, 0);
            assert_eq!(node.cell_count, 3);
            assert_eq!(node.min_cost, 1);
        }
        // Two tick steps → two flow edges, no punches.
        assert_eq!(graph.edges.len(), 2);
        assert!(graph.edges.iter().all(|e| e.kind == EdgeKind::Flow));
        assert_eq!(graph.edges[0].from, 0);
        assert_eq!(graph.edges[0].to, 1);
        assert_eq!(graph.edges[1].from, 1);
        assert_eq!(graph.edges[1].to, 2);
    }

    /// Two affordable basins split by a one-cell ridge whose `V` exceeds
    /// the budget → two components per tick plus a punch edge priced at the
    /// ridge `V` (the threshold at which raising the budget fuses them).
    #[test]
    fn two_basins_split_by_ridge_yield_a_punch_at_ridge_cost() {
        // 5×1: cells 0,1 affordable (V=1), cell 2 the ridge (V=7), cells
        // 3,4 affordable (V=1). Budget 5 keeps the ridge above budget.
        let field = ScalarField {
            width: 5,
            height: 1,
            ticks: 1,
            values: vec![1, 1, 7, 1, 1],
        };
        let graph = build_corridor_graph_core(&field, &stencil_4way(), 5);
        assert_eq!(
            graph.nodes.len(),
            2,
            "two components either side of the ridge"
        );
        assert_eq!(graph.nodes[0].component, 0);
        assert_eq!(graph.nodes[0].cell_count, 2);
        assert_eq!(graph.nodes[1].component, 1);
        assert_eq!(graph.nodes[1].cell_count, 2);
        assert_eq!(graph.edges.len(), 1, "one punch edge");
        let punch = &graph.edges[0];
        assert_eq!(punch.kind, EdgeKind::Punch);
        assert_eq!(punch.from, 0);
        assert_eq!(punch.to, 1);
        assert_eq!(punch.price, 7, "priced at the ridge V");
        assert_eq!(punch.overlap_width, 0);
    }

    /// A tick with no affordable cell contributes no node and no edge.
    #[test]
    fn all_unaffordable_tick_has_no_nodes() {
        let field = ScalarField {
            width: 2,
            height: 1,
            ticks: 1,
            values: vec![20, 20],
        };
        let graph = build_corridor_graph_core(&field, &stencil_4way(), 5);
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
    }

    /// An `UNREACHABLE` cell never joins a component even when the budget
    /// is the maximum value.
    #[test]
    fn unreachable_cell_never_enters_a_component() {
        let field = ScalarField {
            width: 3,
            height: 1,
            ticks: 1,
            values: vec![1, UNREACHABLE, 1],
        };
        let graph = build_corridor_graph_core(&field, &stencil_4way(), u32::MAX);
        // Cells 0 and 2 are affordable but separated by the unreachable
        // cell 1, which is never affordable → two single-cell components,
        // and no finite barrier to punch through (the sentinel is not a
        // ridge), so no punch edge.
        assert_eq!(graph.nodes.len(), 2);
        assert!(graph.nodes.iter().all(|n| n.cell_count == 1));
        assert!(graph.edges.is_empty());
    }

    /// A persisting region links tick to tick with exactly one flow edge
    /// per step.
    #[test]
    fn persisting_region_links_each_tick_step() {
        let width = 2;
        let height = 2;
        let ticks = 4;
        let field = ScalarField {
            width,
            height,
            ticks,
            values: vec![1u32; (width * height * ticks) as usize],
        };
        let graph = build_corridor_graph_core(&field, &stencil_4way(), 10);
        assert_eq!(graph.nodes.len(), 4);
        let flow = graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Flow)
            .count();
        assert_eq!(flow, 3, "one flow edge per tick step");
        assert!(graph.edges.iter().all(|e| e.kind == EdgeKind::Flow));
        // Each flow edge carries a positive overlap width.
        assert!(graph.edges.iter().all(|e| e.overlap_width > 0));
    }

    /// A region that splits into two then re-merges: out-degree 2 at the
    /// branch (a node with two forward flow edges) and a join back to one.
    #[test]
    fn split_then_merge_branches_then_joins() {
        // 3×1 grid over 3 ticks.
        //   t0: [1, 1, 1]      one component (cells 0,1,2)
        //   t1: [1, 9, 1]      cell 1 a sub-budget barrier → two components
        //   t2: [1, 1, 1]      one component again
        // Budget 5: the middle cell at t1 is above budget, splitting the
        // row into {0} and {2}; the flow from t0's single component
        // branches to both, then both rejoin into t2's single component.
        let field = ScalarField {
            width: 3,
            height: 1,
            ticks: 3,
            values: vec![
                1, 1, 1, //
                1, 9, 1, //
                1, 1, 1, //
            ],
        };
        let graph = build_corridor_graph_core(&field, &stencil_4way(), 5);
        // t0: 1 node (index 0); t1: 2 nodes (1, 2); t2: 1 node (index 3).
        assert_eq!(graph.nodes.len(), 4);
        assert_eq!(graph.nodes[0].tick, 0);
        assert_eq!(graph.nodes[1].tick, 1);
        assert_eq!(graph.nodes[2].tick, 1);
        assert_eq!(graph.nodes[3].tick, 2);

        // The t0 component branches to both t1 components (out-degree 2).
        assert_eq!(
            flow_out(&graph.edges, 0),
            2,
            "branch: out-degree equals branch count"
        );
        // The two t1 components both flow into the single t2 component.
        assert_eq!(flow_in(&graph.edges, 3), 2, "join: both branches re-merge");
    }

    /// Determinism: the same `V` + budget produces byte-identical output.
    #[test]
    fn build_is_deterministic() {
        use aether_data::Kind;
        let field = ScalarField {
            width: 5,
            height: 1,
            ticks: 3,
            values: vec![
                1, 1, 7, 1, 1, //
                1, 1, 7, 1, 1, //
                1, 1, 1, 1, 1, //
            ],
        };
        let a = build_corridor_graph_core(&field, &stencil_4way(), 5);
        let b = build_corridor_graph_core(&field, &stencil_4way(), 5);
        assert_eq!(a, b);
        assert_eq!(a.encode_into_bytes(), b.encode_into_bytes());
    }

    /// The canonical 64×64 × 1800-tick field encodes well under the 64MB
    /// transform output cap and round-trips byte-stable.
    #[test]
    fn canonical_field_encodes_under_cap_and_round_trips() {
        use aether_data::Kind;
        const CAP: usize = 64 * 1024 * 1024;
        let width = 64u32;
        let height = 64u32;
        let ticks = 1800u32;
        let plane = (width * height) as usize;
        // One affordable basin everywhere → a tiny skeleton (one node per
        // tick, one flow edge per step), far under the cap.
        let field = ScalarField {
            width,
            height,
            ticks,
            values: vec![1u32; plane * ticks as usize],
        };
        let graph = build_corridor_graph_core(&field, &stencil_4way(), 10);
        assert_eq!(graph.nodes.len(), ticks as usize);
        let bytes = graph.encode_into_bytes();
        assert!(
            bytes.len() < CAP,
            "encoded corridor graph is {} bytes, over the {CAP}-byte cap",
            bytes.len()
        );
        let back = aether_kinds::CorridorGraph::decode_from_bytes(&bytes)
            .expect("corridor graph round-trips");
        assert_eq!(graph, back);
    }
}

#[cfg(test)]
mod resolution_depth_tests {
    use super::{corridor_resolution_depth_core, fork_resolution_depths, live_nodes};
    use aether_kinds::{CorridorEdge, CorridorGraph, CorridorNode, EdgeKind};

    /// A node at `tick` (the other summary fields don't affect liveness or
    /// fork depth, which read only `tick` and the flow topology).
    fn node(tick: u32) -> CorridorNode {
        CorridorNode {
            tick,
            component: 0,
            cell_count: 1,
            min_cost: 1,
        }
    }

    /// A `Flow` edge `from -> to` (forward one tick).
    fn flow(from: u32, to: u32) -> CorridorEdge {
        CorridorEdge {
            from,
            to,
            kind: EdgeKind::Flow,
            price: 0,
            overlap_width: 0,
        }
    }

    /// A `Punch` edge (intra-tick), which liveness and fork depth ignore.
    fn punch(from: u32, to: u32) -> CorridorEdge {
        CorridorEdge {
            from,
            to,
            kind: EdgeKind::Punch,
            price: 3,
            overlap_width: 0,
        }
    }

    /// A through corridor — one node per tick, a flow edge per step — is
    /// fully live: every node reaches the final tick.
    #[test]
    fn through_corridor_is_fully_live() {
        let graph = CorridorGraph {
            nodes: vec![node(0), node(1), node(2)],
            edges: vec![flow(0, 1), flow(1, 2)],
        };
        assert_eq!(live_nodes(&graph), vec![true, true, true]);
        assert!(fork_resolution_depths(&graph, &live_nodes(&graph)).is_empty());
    }

    /// A branch that terminates before the final tick is dead-end: it is in
    /// the graph but has no flow continuation to the end.
    #[test]
    fn branch_terminating_early_is_dead_end() {
        // tick 0: node 0 (start). tick 1: node 1 (through) + node 2
        // (dead-end, no successor). tick 2: node 3 (terminus).
        let graph = CorridorGraph {
            nodes: vec![node(0), node(1), node(1), node(2)],
            edges: vec![flow(0, 1), flow(0, 2), flow(1, 3)],
        };
        // node 2 dead-ends at tick 1; everything else reaches tick 2.
        assert_eq!(live_nodes(&graph), vec![true, true, false, true]);
    }

    /// Every tick reachable to the end → all nodes live, no forks.
    #[test]
    fn all_affordable_corridor_has_no_forks() {
        let graph = CorridorGraph {
            nodes: vec![node(0), node(1), node(2), node(3)],
            edges: vec![flow(0, 1), flow(1, 2), flow(2, 3)],
        };
        assert!(live_nodes(&graph).iter().all(|&l| l));
        let depths = corridor_resolution_depth_core(&graph);
        assert_eq!(depths.max_resolution_depth, 0);
        assert!(depths.forks.is_empty());
    }

    /// The wall-trap graph: a depth-5 cheap-then-walled dead-end branch
    /// beside a through branch. The fork at the split must look 5 ticks
    /// ahead to tell the dead-end apart from the through branch.
    #[test]
    fn wall_trap_yields_max_depth_five() {
        // Through spine: nodes 0..=7 at ticks 0..=7 (reaches the final
        // tick 7). At tick 1, node 1 forks into a dead-end chain — nodes
        // 8..=12 occupying ticks 2..=6 — that never rejoins the spine and
        // terminates one tick *before* the final tick (so it is genuinely a
        // dead-end, not a through branch). The dead-end persists five ticks
        // past the fork, so the fork (node 1) demands depth 5.
        //
        // Spine: 0(t0) 1(t1) 2(t2) 3(t3) 4(t4) 5(t5) 6(t6) 7(t7)
        // Dead :            8(t2) 9(t3) 10(t4) 11(t5) 12(t6)
        let mut nodes = Vec::new();
        for t in 0..=7 {
            nodes.push(node(t));
        }
        for t in 2..=6 {
            nodes.push(node(t)); // dead-end chain at ticks 2..=6
        }
        let edges = vec![
            // spine
            flow(0, 1),
            flow(1, 2),
            flow(2, 3),
            flow(3, 4),
            flow(4, 5),
            flow(5, 6),
            flow(6, 7),
            // dead-end chain branching off node 1 (tick 1) into node 8 (tick 2)
            flow(1, 8),
            flow(8, 9),
            flow(9, 10),
            flow(10, 11),
            flow(11, 12),
        ];
        let graph = CorridorGraph { nodes, edges };
        let live = live_nodes(&graph);
        // Spine (0..=7) live; dead-end chain (8..=12) dead-end.
        assert_eq!(&live[0..8], &[true; 8]);
        assert_eq!(&live[8..13], &[false; 5]);

        let depths = corridor_resolution_depth_core(&graph);
        // The only fork is node 1; its dead-end branch persists five ticks.
        assert_eq!(depths.forks.len(), 1);
        assert_eq!(depths.forks[0].node_index, 1);
        assert_eq!(depths.forks[0].depth, 5);
        assert_eq!(depths.max_resolution_depth, 5);
    }

    /// Nested dead-ends: a fork's depth is the longest path through its
    /// dead-end subtree, not the shortest.
    #[test]
    fn nested_dead_ends_take_the_longest_path() {
        // Spine: 0(t0) 1(t1) ... 6(t6) reaching the final tick 6.
        // Off node 0 (tick 0) a dead-end branch enters node 7 (tick 1),
        // which splits into a short stub node 8 (tick 2, terminates) and a
        // longer chain node 9(t2) 10(t3) 11(t4) — both terminating before
        // the final tick 6. The fork is node 0; its longest dead-end path is
        // 0 -> 7 -> 9 -> 10 -> 11 = depth 4, not the depth-2 stub.
        let nodes = vec![
            node(0), // 0 spine t0
            node(1), // 1 spine t1
            node(2), // 2 spine t2
            node(3), // 3 spine t3
            node(4), // 4 spine t4
            node(5), // 5 spine t5
            node(6), // 6 spine t6 (final tick)
            node(1), // 7 dead t1
            node(2), // 8 dead t2 (stub)
            node(2), // 9 dead t2
            node(3), // 10 dead t3
            node(4), // 11 dead t4
        ];
        let edges = vec![
            // spine
            flow(0, 1),
            flow(1, 2),
            flow(2, 3),
            flow(3, 4),
            flow(4, 5),
            flow(5, 6),
            // dead-end subtree off node 0
            flow(0, 7),
            flow(7, 8),
            flow(7, 9),
            flow(9, 10),
            flow(10, 11),
        ];
        let graph = CorridorGraph { nodes, edges };
        let live = live_nodes(&graph);
        assert_eq!(&live[0..7], &[true; 7]);
        assert_eq!(&live[7..12], &[false; 5]);

        let depths = corridor_resolution_depth_core(&graph);
        assert_eq!(depths.forks.len(), 1);
        assert_eq!(depths.forks[0].node_index, 0);
        // 0 -> 7 (+1) -> 9 (+1) -> 10 (+1) -> 11 (+1) = 4.
        assert_eq!(depths.forks[0].depth, 4);
        assert_eq!(depths.max_resolution_depth, 4);
    }

    /// Punch edges are ignored by liveness and fork detection: a node that
    /// reaches the end only via a punch (not a flow chain) is still
    /// dead-end, and a punch into a dead-end is not a fork.
    #[test]
    fn punch_edges_do_not_carry_liveness() {
        // tick 0: node 0. tick 1: node 1 (through, flows to terminus) and
        // node 2 (dead-end). A punch joins node 1 and node 2 at tick 1.
        // tick 2: node 3 terminus, reached from node 1 by flow.
        let graph = CorridorGraph {
            nodes: vec![node(0), node(1), node(1), node(2)],
            edges: vec![flow(0, 1), flow(0, 2), punch(1, 2), flow(1, 3)],
        };
        // The punch does not make node 2 live.
        assert_eq!(live_nodes(&graph), vec![true, true, false, true]);
        let depths = corridor_resolution_depth_core(&graph);
        // node 0 forks into dead-end node 2: depth 1 (one tick of dead-end).
        assert_eq!(
            depths.forks,
            vec![aether_kinds::ForkDepth {
                node_index: 0,
                depth: 1
            }]
        );
        assert_eq!(depths.max_resolution_depth, 1);
    }

    /// An empty graph (no nodes) yields no forks and zero depth.
    #[test]
    fn empty_graph_has_zero_depth() {
        let graph = CorridorGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
        };
        assert!(live_nodes(&graph).is_empty());
        let depths = corridor_resolution_depth_core(&graph);
        assert_eq!(depths.max_resolution_depth, 0);
        assert!(depths.forks.is_empty());
    }
}
