//! Per-edge trajectory-density aggregation over a corridor graph (issue
//! 1865). The pure core the `aggregate_traffic` transform (ADR-0048)
//! wraps, kept beside the corridor builder so it reuses #1858's exact
//! per-tick component labeler ([`label_tick_components`]) for id parity.
//!
//! Given a [`CorridorGraph`] (#1858), the field `V` it was built from (a
//! [`ScalarField`], #1857), the movement stencil, the budget `B`, and a
//! set of paths ([`TrajectoryLog`]s, #1862), this snaps each path sample
//! to the corridor component it falls into and accumulates per-edge
//! traffic — a density over the graph. It surfaces the discrepancies the
//! join makes visible: edges with zero traffic (reachable but
//! untraveled), high-traffic branches, and the split of through-boundary
//! ("punch") traffic by whether the crossing was cheaper than the
//! affordable detour around it.
//!
//! The corridor graph is a *skeleton* — its nodes carry summaries, not
//! cell sets — so a path sample `(tick, x, y)` cannot be snapped from the
//! graph alone. The per-tick partition is **re-derived** from `V` + `B` +
//! the stencil with the same labeler #1858 runs, so the re-derived
//! component ids match the graph's node component ids by construction.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};

use aether_kinds::{
    CorridorEdge, CorridorGraph, EdgeKind, ScalarField, StencilOffset, TrafficDensity,
    TrajectoryLog,
};

use crate::corridor::{TickComponents, label_tick_components};

/// Stable u8 discriminant for an [`EdgeKind`], so an edge can be keyed by
/// `(from, to, kind)` in a `BTreeMap` without an `Ord` derive on the kind.
fn edge_kind_ord(kind: EdgeKind) -> u8 {
    match kind {
        EdgeKind::Flow => 0,
        EdgeKind::Punch => 1,
    }
}

/// A node index as the `u32` the corridor graph's edge endpoints carry.
/// The graph was built with `u32::try_from(nodes.len())`, so every node
/// index fits a `u32`; this mirrors #1858's own index→endpoint cast.
fn node_id(index: usize) -> u32 {
    u32::try_from(index).expect("node index fits u32")
}

/// The per-tick component labels, parallel to `field.ticks`. `labels[t]`
/// is the row-major per-cell `Option<u32>` component id of tick `t`
/// (`None` for an above-budget or unreachable cell), exactly as #1858's
/// labeler assigns them. Ticks the field is too short to slice are absent
/// (the labeler / builder both stop at the first truncated tick), so a
/// snap against a missing tick simply finds no component.
struct PerTickLabels {
    labels: Vec<Vec<Option<u32>>>,
}

impl PerTickLabels {
    /// Component id of `cell` at `tick`, or `None` when the tick is
    /// out of range, the cell is out of range, or the cell is not in an
    /// affordable component.
    fn component(&self, tick: u32, cell: usize) -> Option<u32> {
        let layer = self.labels.get(tick as usize)?;
        *layer.get(cell)?
    }
}

/// Re-derive the per-tick `<= budget` partition of `field` with the shared
/// #1858 labeler, so the snapped component ids match the graph's node ids.
fn rederive_labels(field: &ScalarField, stencil: &[StencilOffset], budget: u32) -> PerTickLabels {
    let width = field.width as usize;
    let height = field.height as usize;
    let ticks = field.ticks as usize;
    let plane = width.saturating_mul(height);
    let mut labels: Vec<Vec<Option<u32>>> = Vec::new();
    if plane == 0 {
        return PerTickLabels { labels };
    }
    for t in 0..ticks {
        let base = t.saturating_mul(plane);
        let slice = match field.values.get(base..base.saturating_add(plane)) {
            Some(s) if s.len() == plane => s,
            // A truncated field stops here, matching the builder.
            _ => break,
        };
        let tick = u32::try_from(t).expect("tick index fits u32");
        let TickComponents { label, .. } =
            label_tick_components(slice, width, height, stencil, budget, tick);
        labels.push(label);
    }
    PerTickLabels { labels }
}

/// `(tick, component) -> node index` over `graph.nodes`. The graph orders
/// nodes by `(tick, component)`, so the re-derived `(tick, component)` ids
/// map straight onto the node index a consumer joins by.
fn node_index_map(graph: &CorridorGraph) -> BTreeMap<(u32, u32), usize> {
    let mut map = BTreeMap::new();
    for (i, node) in graph.nodes.iter().enumerate() {
        map.insert((node.tick, node.component), i);
    }
    map
}

/// `(from, to, kind) -> edge index` over `graph.edges`, keyed on stable
/// edge identity (endpoints + kind) so it is robust to the `overlap_width`
/// / `price` payload fields. Edges are unique per identity in #1858's
/// output (deduplicated punch merges, landing-counted flow edges), so the
/// last-write-wins insert is exact.
fn edge_index_map(graph: &CorridorGraph) -> BTreeMap<(u32, u32, u8), usize> {
    let mut map = BTreeMap::new();
    for (i, edge) in graph.edges.iter().enumerate() {
        map.insert((edge.from, edge.to, edge_kind_ord(edge.kind)), i);
    }
    map
}

/// One snapped sample: the tick, the node index it landed on (`None` for
/// an above-budget / unreachable / out-of-grid sample), and the in-grid
/// row-major cell index (`None` only when off-grid). The cell is kept so a
/// punch's exit can be re-evaluated against the entry tick's partition.
struct Snapped {
    tick: u32,
    node: Option<usize>,
    cell: Option<usize>,
}

/// Backward flow-lineage adjacency: `to_node -> sorted from_nodes`. Used
/// to trace a punch's exit component back to the entry tick so the
/// (entry, exit) pair matches a single intra-tick punch-edge node pair.
fn flow_predecessors(graph: &CorridorGraph) -> BTreeMap<usize, Vec<usize>> {
    let mut preds: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for edge in &graph.edges {
        if matches!(edge.kind, EdgeKind::Flow) {
            preds
                .entry(edge.to as usize)
                .or_default()
                .push(edge.from as usize);
        }
    }
    for list in preds.values_mut() {
        list.sort_unstable();
        list.dedup();
    }
    preds
}

/// Trace a flow lineage backward from `exit_node` to `entry_tick`, landing
/// on a node at that tick. At each backward step pick the predecessor that
/// forms a punch edge with `entry_node` if one exists (so the attribution
/// lands on the matching punch-edge pair), else the smallest node index
/// (deterministic). Returns `None` if the lineage doesn't reach the entry
/// tick — the exit component descended from no affordable component there.
fn trace_to_entry_tick(
    graph: &CorridorGraph,
    preds: &BTreeMap<usize, Vec<usize>>,
    punch_partner: &BTreeMap<usize, Vec<usize>>,
    exit_node: usize,
    entry_node: usize,
    entry_tick: u32,
) -> Option<usize> {
    let mut current = exit_node;
    // The graph is a time-layered DAG and we only ever step backward in
    // time, so the walk is bounded by the tick count — no cycle, no
    // recursion. Guard the loop by tick count as a belt-and-braces cap.
    let max_steps = graph.nodes.len().saturating_add(1);
    for _ in 0..max_steps {
        let tick = graph.nodes[current].tick;
        match tick.cmp(&entry_tick) {
            Ordering::Equal => return Some(current),
            // Already at or before the entry tick without matching: the
            // exit didn't descend from an affordable component there.
            Ordering::Less => return None,
            Ordering::Greater => {}
        }
        let candidates = preds.get(&current)?;
        if candidates.is_empty() {
            return None;
        }
        // Prefer a predecessor that punch-partners the entry node — that's
        // the lineage that matches a real punch edge at the entry tick.
        let partners = punch_partner.get(&entry_node);
        let next = candidates
            .iter()
            .copied()
            .find(|p| partners.is_some_and(|set| set.binary_search(p).is_ok()))
            .or_else(|| candidates.first().copied())?;
        current = next;
    }
    None
}

/// Punch partners of each node: `node -> sorted partner nodes` over punch
/// edges (both directions, since a punch edge is undirected connectivity).
fn punch_partners(graph: &CorridorGraph) -> BTreeMap<usize, Vec<usize>> {
    let mut partners: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for edge in &graph.edges {
        if matches!(edge.kind, EdgeKind::Punch) {
            partners
                .entry(edge.from as usize)
                .or_default()
                .push(edge.to as usize);
            partners
                .entry(edge.to as usize)
                .or_default()
                .push(edge.from as usize);
        }
    }
    for list in partners.values_mut() {
        list.sort_unstable();
        list.dedup();
    }
    partners
}

/// A `(cost, node)` Dijkstra heap entry, ordered so the `BinaryHeap` (a
/// max-heap) pops the *minimum* cost first.
#[derive(PartialEq, Eq)]
struct HeapItem {
    cost: u64,
    node: usize,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse on cost (min-heap), then node index for a total order.
        other
            .cost
            .cmp(&self.cost)
            .then_with(|| other.node.cmp(&self.node))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Forward-flow distances from `source` over the graph's `Flow` edges,
/// weighted by the accumulated affordable cost via each destination node's
/// `min_cost`. Iterative Dijkstra over a max-heap inverted to pop the
/// minimum (no recursion). `dist[n]` is `Some(cost)` when `n` is
/// forward-flow-reachable from `source` (the source itself at cost `0`),
/// `None` otherwise.
fn flow_distances_from(
    graph: &CorridorGraph,
    flow_adj: &BTreeMap<usize, Vec<(usize, u64)>>,
    source: usize,
) -> Vec<Option<u64>> {
    let mut dist: Vec<Option<u64>> = vec![None; graph.nodes.len()];
    let mut heap = BinaryHeap::new();
    dist[source] = Some(0);
    heap.push(HeapItem {
        cost: 0,
        node: source,
    });
    while let Some(HeapItem { cost, node }) = heap.pop() {
        if dist[node].is_some_and(|d| cost > d) {
            continue;
        }
        let Some(neighbors) = flow_adj.get(&node) else {
            continue;
        };
        for &(next, weight) in neighbors {
            let next_cost = cost.saturating_add(weight);
            if dist[next].is_none_or(|d| next_cost < d) {
                dist[next] = Some(next_cost);
                heap.push(HeapItem {
                    cost: next_cost,
                    node: next,
                });
            }
        }
    }
    dist
}

/// Cheapest affordable forward-flow detour connecting the two components a
/// punch edge separates. The punch endpoints sit at the *same* tick, so a
/// forward-flow route never reaches one directly from the other (flow
/// strictly advances the tick); the two basins are instead connected by an
/// affordable detour iff they **reconverge** later — share a common
/// forward-flow descendant. The detour cost is the cheapest accumulated
/// affordable cost to route from one basin to the other through that
/// reconvergence: the minimum over common descendants `d` of
/// `dist_from[d] + dist_to[d]`. Returns `None` (read as `∞`) when the two
/// basins never reconverge — the barrier is the only connection.
fn flow_detour_cost(
    graph: &CorridorGraph,
    flow_adj: &BTreeMap<usize, Vec<(usize, u64)>>,
    from: usize,
    to: usize,
) -> Option<u64> {
    if from == to {
        return Some(0);
    }
    let dist_from = flow_distances_from(graph, flow_adj, from);
    let dist_to = flow_distances_from(graph, flow_adj, to);
    let mut best: Option<u64> = None;
    for d in 0..graph.nodes.len() {
        if let (Some(a), Some(b)) = (dist_from[d], dist_to[d]) {
            let total = a.saturating_add(b);
            best = Some(best.map_or(total, |cur| cur.min(total)));
        }
    }
    best
}

/// Forward flow adjacency for the detour Dijkstra: `from -> (to, weight)`,
/// weight = the destination node's `min_cost` (the accumulated affordable
/// cost increment; flow edges price `0`, so the metric is the node cost,
/// not the edge price).
fn flow_adjacency(graph: &CorridorGraph) -> BTreeMap<usize, Vec<(usize, u64)>> {
    let mut adj: BTreeMap<usize, Vec<(usize, u64)>> = BTreeMap::new();
    for edge in &graph.edges {
        if matches!(edge.kind, EdgeKind::Flow) {
            let weight = u64::from(graph.nodes[edge.to as usize].min_cost);
            adj.entry(edge.from as usize)
                .or_default()
                .push((edge.to as usize, weight));
        }
    }
    adj
}

/// Aggregate a set of paths onto a corridor graph, producing the per-edge
/// traffic density (issue 1865).
///
/// Snaps each [`TrajectoryLog`] sample to the corridor component it falls
/// into (re-deriving the per-tick partition from `field` + `budget` +
/// `stencil` with #1858's labeler so component ids match the graph's node
/// ids), then accumulates: per-node visit counts; per-flow-edge traffic
/// (one unit per affordable→affordable consecutive-tick step); and
/// per-punch-edge through-boundary crossings (a path leaving an affordable
/// component, traversing an above-budget run, and re-entering a different
/// one). The punch traffic is split by whether the punch `price` `τ` beat
/// the cheapest affordable flow detour around the barrier — the cost to
/// reconverge the two basins over forward flow edges (`∞` when they never
/// reconverge, so the barrier is the only connection).
pub fn aggregate_traffic_core(
    graph: &CorridorGraph,
    field: &ScalarField,
    stencil: &[StencilOffset],
    budget: u32,
    logs: &[TrajectoryLog],
) -> TrafficDensity {
    let width = field.width as usize;
    let height = field.height as usize;
    let plane = width.saturating_mul(height);

    let labels = rederive_labels(field, stencil, budget);
    let node_map = node_index_map(graph);
    let edge_map = edge_index_map(graph);
    let preds = flow_predecessors(graph);
    let partners = punch_partners(graph);

    let mut node_traffic = vec![0u32; graph.nodes.len()];
    let mut edge_traffic = vec![0u32; graph.edges.len()];

    for log in logs {
        // Snap every sample to its corridor node (tick-ordered as stored).
        let mut snapped: Vec<Snapped> = Vec::with_capacity(log.samples.len());
        for sample in &log.samples {
            let x = sample.x as usize;
            let y = sample.y as usize;
            let cell = if plane != 0 && x < width && y < height {
                Some(y * width + x)
            } else {
                None
            };
            let node = cell.and_then(|c| {
                labels
                    .component(sample.tick, c)
                    .and_then(|comp| node_map.get(&(sample.tick, comp)).copied())
            });
            if let Some(idx) = node {
                node_traffic[idx] = node_traffic[idx].saturating_add(1);
            }
            snapped.push(Snapped {
                tick: sample.tick,
                node,
                cell,
            });
        }

        accumulate_path(
            graph,
            &labels,
            &node_map,
            &edge_map,
            &preds,
            &partners,
            &snapped,
            &mut edge_traffic,
        );
    }

    // The split is a property of each punch edge, identical for every path
    // crossing it, so compute the verdict once per edge and partition the
    // already-accumulated punch traffic by it.
    let flow_adj = flow_adjacency(graph);
    let mut punch_crossing_cheaper = 0u32;
    let mut punch_detour_cheaper = 0u32;
    for (i, edge) in graph.edges.iter().enumerate() {
        if !matches!(edge.kind, EdgeKind::Punch) {
            continue;
        }
        let traffic = edge_traffic[i];
        if traffic == 0 {
            continue;
        }
        if crossing_beats_detour(graph, &flow_adj, edge) {
            punch_crossing_cheaper = punch_crossing_cheaper.saturating_add(traffic);
        } else {
            punch_detour_cheaper = punch_detour_cheaper.saturating_add(traffic);
        }
    }

    let untraveled_edges = edge_traffic
        .iter()
        .enumerate()
        .filter(|&(_, &t)| t == 0)
        .map(|(i, _)| u32::try_from(i).expect("edge index fits u32"))
        .collect();

    TrafficDensity {
        path_count: u32::try_from(logs.len()).expect("path count fits u32"),
        edge_traffic,
        node_traffic,
        untraveled_edges,
        punch_crossing_cheaper,
        punch_detour_cheaper,
    }
}

/// Whether punching `edge` beat the affordable detour around it: `τ`
/// (`edge.price`) strictly less than the cheapest forward-flow detour
/// cost (`∞` when no flow route connects the components — punching is then
/// the only crossing and trivially cheaper).
fn crossing_beats_detour(
    graph: &CorridorGraph,
    flow_adj: &BTreeMap<usize, Vec<(usize, u64)>>,
    edge: &CorridorEdge,
) -> bool {
    // A finite detour exists: punch is cheaper only when `τ` is strictly
    // below it (a tie favours the detour). No detour route (`None`, read as
    // `∞`) means the barrier is the only connection, so the crossing wins.
    flow_detour_cost(graph, flow_adj, edge.from as usize, edge.to as usize)
        .is_none_or(|detour| u64::from(edge.price) < detour)
}

/// Accumulate one path's flow and punch traffic from its snapped samples.
#[allow(clippy::too_many_arguments)]
fn accumulate_path(
    graph: &CorridorGraph,
    labels: &PerTickLabels,
    node_map: &BTreeMap<(u32, u32), usize>,
    edge_map: &BTreeMap<(u32, u32, u8), usize>,
    preds: &BTreeMap<usize, Vec<usize>>,
    partners: &BTreeMap<usize, Vec<usize>>,
    snapped: &[Snapped],
    edge_traffic: &mut [u32],
) {
    // The most recent affordable sample, carried across an above-budget run
    // so the punch's entry component and tick are known when the path
    // re-enters an affordable component.
    let mut last_affordable: Option<&Snapped> = None;

    for curr in snapped {
        let Some(curr_node) = curr.node else {
            // Above-budget / off-grid sample: part of a punch run, no flow
            // step. `last_affordable` is unchanged so the entry survives.
            continue;
        };

        if let Some(prev) = last_affordable {
            let Some(prev_node) = prev.node else {
                unreachable!("last_affordable always carries a snapped node");
            };
            if curr.tick == prev.tick.saturating_add(1) {
                // Consecutive-tick affordable→affordable step: one unit to
                // the matching flow edge (a held region's self-step too).
                let key = (
                    node_id(prev_node),
                    node_id(curr_node),
                    edge_kind_ord(EdgeKind::Flow),
                );
                if let Some(&edge_idx) = edge_map.get(&key) {
                    edge_traffic[edge_idx] = edge_traffic[edge_idx].saturating_add(1);
                }
            } else if curr_node != prev_node {
                // A gap (an above-budget run) bridged two distinct
                // affordable components: a punch. Attribute the crossing to
                // the intra-tick punch edge between the entry component and
                // the exit's component *at the entry tick*.
                attribute_punch(
                    graph,
                    labels,
                    node_map,
                    edge_map,
                    preds,
                    partners,
                    prev_node,
                    prev.tick,
                    curr,
                    edge_traffic,
                );
            }
        }
        last_affordable = Some(curr);
    }
}

/// Attribute one punch crossing (entry component `entry_node` at
/// `entry_tick`, re-entry at the snapped `exit` sample on a later tick) to
/// the matching intra-tick punch edge, crediting one unit of traffic.
///
/// The punch edge joins two components at the *entry* tick, so the exit
/// component is resolved at the entry tick two ways: first spatially —
/// re-evaluate the exit cell against the entry tick's partition (the
/// common case, a spatially stable basin); failing that (the exit cell is
/// above-budget at the entry tick, e.g. a moving basin) — trace the exit
/// component back through flow lineage to the entry tick.
#[allow(clippy::too_many_arguments)]
fn attribute_punch(
    graph: &CorridorGraph,
    labels: &PerTickLabels,
    node_map: &BTreeMap<(u32, u32), usize>,
    edge_map: &BTreeMap<(u32, u32, u8), usize>,
    preds: &BTreeMap<usize, Vec<usize>>,
    partners: &BTreeMap<usize, Vec<usize>>,
    entry_node: usize,
    entry_tick: u32,
    exit: &Snapped,
    edge_traffic: &mut [u32],
) {
    let Some(exit_node) = exit.node else {
        return;
    };
    // Primary: the exit cell's component at the entry tick — spatially
    // grounded and unambiguous when the exit basin persists in place.
    let spatial = exit.cell.and_then(|cell| {
        labels
            .component(entry_tick, cell)
            .and_then(|comp| node_map.get(&(entry_tick, comp)).copied())
    });
    let traced = spatial
        .or_else(|| trace_to_entry_tick(graph, preds, partners, exit_node, entry_node, entry_tick))
        .unwrap_or(exit_node);
    if traced == entry_node {
        // Same component at the entry tick — no barrier between them, so
        // there is no punch edge to attribute to.
        return;
    }
    // Punch edges are emitted with `from < to` (#1858 orders the merged
    // pair), so probe both orientations.
    let (a, b) = if entry_node <= traced {
        (node_id(entry_node), node_id(traced))
    } else {
        (node_id(traced), node_id(entry_node))
    };
    let punch = edge_kind_ord(EdgeKind::Punch);
    if let Some(&edge_idx) = edge_map
        .get(&(a, b, punch))
        .or_else(|| edge_map.get(&(b, a, punch)))
    {
        edge_traffic[edge_idx] = edge_traffic[edge_idx].saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::aggregate_traffic_core;
    use crate::corridor::build_corridor_graph_core;
    use crate::reachability::test_fields::stencil_offsets;
    use aether_kinds::{
        CorridorGraph, EdgeKind, ScalarField, TrafficDensity, TrajectoryEndReason, TrajectoryLog,
        TrajectorySampleEntry,
    };

    /// Sum the per-edge traffic accumulated on the graph's `Punch` edges —
    /// the total punch crossings attributed across a run.
    fn punch_traffic(graph: &CorridorGraph, density: &TrafficDensity) -> u32 {
        graph
            .edges
            .iter()
            .zip(&density.edge_traffic)
            .filter(|(e, _)| matches!(e.kind, EdgeKind::Punch))
            .map(|(_, &t)| t)
            .sum()
    }

    /// Build a `TrajectoryLog` from `(tick, x, y)` samples (value unused by
    /// the snap, so a constant placeholder).
    fn log(seed: u64, samples: &[(u32, u32, u32)]) -> TrajectoryLog {
        TrajectoryLog {
            seed,
            samples: samples
                .iter()
                .map(|&(tick, x, y)| TrajectorySampleEntry {
                    tick,
                    x,
                    y,
                    value: 0,
                })
                .collect(),
            end_reason: TrajectoryEndReason::Completed,
        }
    }

    /// A 3×1 uniform-cost field held across 3 ticks — one persisting
    /// component per tick (ids all `0`), flow edges chaining them.
    fn corridor_3x1_held() -> ScalarField {
        ScalarField {
            width: 3,
            height: 1,
            ticks: 3,
            values: vec![1; 9],
        }
    }

    #[test]
    fn path_in_a_persisting_region_visits_its_node_each_tick() {
        let field = corridor_3x1_held();
        let stencil = stencil_offsets();
        let graph = build_corridor_graph_core(&field, &stencil, 5);
        // One node per tick (component 0), three nodes total.
        assert_eq!(graph.nodes.len(), 3);
        let paths = [log(1, &[(0, 0, 0), (1, 1, 0), (2, 2, 0)])];
        let density = aggregate_traffic_core(&graph, &field, &stencil, 5, &paths);
        // The path snaps to component 0 at each of the three ticks.
        assert_eq!(density.node_traffic, vec![1, 1, 1]);
        assert_eq!(density.path_count, 1);
    }

    #[test]
    fn path_outside_the_affordable_set_accrues_no_traffic() {
        let field = corridor_3x1_held();
        let stencil = stencil_offsets();
        let graph = build_corridor_graph_core(&field, &stencil, 5);
        // Budget 5 makes every cell affordable, but the path sits off-grid
        // (x out of range), so nothing snaps.
        let paths = [log(1, &[(0, 9, 9), (1, 9, 9)])];
        let density = aggregate_traffic_core(&graph, &field, &stencil, 5, &paths);
        assert!(density.node_traffic.iter().all(|&t| t == 0));
        assert!(density.edge_traffic.iter().all(|&t| t == 0));
    }

    #[test]
    fn snapped_component_ids_match_graph_node_ids_on_a_two_basin_field() {
        // 5×2 field, a sub-budget ridge column at x = 2 across both ticks:
        // two affordable basins (left x < 2, right x > 2). The id-parity
        // guard: a path on the right basin must snap to the right-basin
        // node, not the left.
        let row = || vec![1u32, 1, 9, 1, 1];
        let tick = || {
            let mut t = Vec::new();
            t.extend(row()); // y = 0
            t.extend(row()); // y = 1
            t
        };
        let mut values = Vec::new();
        values.extend(tick());
        values.extend(tick());
        let field = ScalarField {
            width: 5,
            height: 2,
            ticks: 2,
            values,
        };
        let stencil = stencil_offsets();
        let graph = build_corridor_graph_core(&field, &stencil, 5);
        // Two components per tick → four nodes; component 0 is the left
        // basin (row-major first encounter), component 1 the right.
        assert_eq!(graph.nodes.len(), 4);
        // A path on the right basin (x = 4) for both ticks.
        let paths = [log(1, &[(0, 4, 0), (1, 4, 0)])];
        let density = aggregate_traffic_core(&graph, &field, &stencil, 5, &paths);
        // Right-basin nodes are component 1 at each tick.
        let right_nodes: Vec<usize> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.component == 1)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(right_nodes.len(), 2);
        for &i in &right_nodes {
            assert_eq!(density.node_traffic[i], 1, "right-basin node {i} visited");
        }
        // No traffic snapped onto a left-basin (component 0) node.
        for (i, n) in graph.nodes.iter().enumerate() {
            if n.component == 0 {
                assert_eq!(density.node_traffic[i], 0, "left-basin node {i} untouched");
            }
        }
    }

    #[test]
    fn persisting_region_accumulates_one_flow_unit_per_tick_step() {
        let field = corridor_3x1_held();
        let stencil = stencil_offsets();
        let graph = build_corridor_graph_core(&field, &stencil, 5);
        // Flow edges chain component 0 across the three ticks: two flow
        // edges (node 0→1, 1→2).
        let flow_count = graph
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::Flow))
            .count();
        assert_eq!(flow_count, 2);
        let paths = [log(1, &[(0, 0, 0), (1, 1, 0), (2, 2, 0)])];
        let density = aggregate_traffic_core(&graph, &field, &stencil, 5, &paths);
        // Each consecutive-tick affordable step credits one flow edge.
        let flow_traffic: u32 = graph
            .edges
            .iter()
            .zip(&density.edge_traffic)
            .filter(|(e, _)| matches!(e.kind, EdgeKind::Flow))
            .map(|(_, &t)| t)
            .sum();
        assert_eq!(flow_traffic, 2);
    }

    #[test]
    fn an_unused_flow_edge_is_reported_untraveled() {
        // 3×1, two basins split by a ridge column at x = 1, held 3 ticks:
        // a path that stays only in the left basin (x = 0) leaves every
        // right-basin flow edge untraveled.
        let mut values = Vec::new();
        for _ in 0..3 {
            values.extend(vec![1u32, 9, 1]);
        }
        let field = ScalarField {
            width: 3,
            height: 1,
            ticks: 3,
            values,
        };
        let stencil = stencil_offsets();
        let graph = build_corridor_graph_core(&field, &stencil, 5);
        let paths = [log(1, &[(0, 0, 0), (1, 0, 0), (2, 0, 0)])];
        let density = aggregate_traffic_core(&graph, &field, &stencil, 5, &paths);
        // The right-basin flow edges are never used → in untraveled_edges.
        assert!(!density.untraveled_edges.is_empty());
        for &i in &density.untraveled_edges {
            assert_eq!(density.edge_traffic[i as usize], 0);
        }
        // untraveled_edges is exactly the zero entries of edge_traffic.
        let zero_count = density.edge_traffic.iter().filter(|&&t| t == 0).count();
        assert_eq!(zero_count, density.untraveled_edges.len());
    }

    #[test]
    fn a_path_punching_a_ridge_with_no_detour_is_crossing_cheaper() {
        // 5×1 field, sub-budget ridge at x = 2 across 3 ticks: left basin
        // (x 0..2), right basin (x 3..5), connected only by the punch (no
        // affordable forward-flow detour between them). A path crossing the
        // ridge contributes punch traffic verdicted crossing_cheaper.
        let mut values = Vec::new();
        for _ in 0..3 {
            values.extend(vec![1u32, 1, 9, 1, 1]);
        }
        let field = ScalarField {
            width: 5,
            height: 1,
            ticks: 3,
            values,
        };
        let stencil = stencil_offsets();
        let graph = build_corridor_graph_core(&field, &stencil, 5);
        // The path enters left (x = 1, t = 0), sits on the ridge (x = 2,
        // t = 1, above budget — no node), re-enters right (x = 3, t = 2).
        let paths = [log(1, &[(0, 1, 0), (1, 2, 0), (2, 3, 0)])];
        let density = aggregate_traffic_core(&graph, &field, &stencil, 5, &paths);
        // One punch crossing attributed, verdicted crossing_cheaper (no
        // affordable forward-flow detour connects the two basins).
        assert_eq!(density.punch_crossing_cheaper, 1);
        assert_eq!(density.punch_detour_cheaper, 0);
        assert_eq!(punch_traffic(&graph, &density), 1);
    }

    #[test]
    fn a_barrier_with_a_cheaper_affordable_detour_is_detour_cheaper() {
        // 3×3 field. A full column ridge at x = 1 splits left (x = 0) and
        // right (x = 2) basins at ticks 0 and 1 (punch price 9 at each);
        // the field opens fully at tick 2, so both basins flow into the
        // single open component — an affordable forward-flow reconvergence
        // whose accumulated cost (node min_cost 1) is far below the punch
        // price. A path that punches the ridge is verdicted detour_cheaper.
        let mut values = Vec::new();
        values.extend(vec![1u32, 9, 1, 1, 9, 1, 1, 9, 1]); // tick 0: split
        values.extend(vec![1u32, 9, 1, 1, 9, 1, 1, 9, 1]); // tick 1: split
        values.extend(vec![1u32; 9]); // tick 2: open
        let field = ScalarField {
            width: 3,
            height: 3,
            ticks: 3,
            values,
        };
        let stencil = stencil_offsets();
        let graph = build_corridor_graph_core(&field, &stencil, 5);
        assert!(
            graph
                .edges
                .iter()
                .any(|e| matches!(e.kind, EdgeKind::Punch)),
            "expected a punch edge between the split basins"
        );
        // The path enters the left basin (x = 0, t = 0), sits on the ridge
        // (x = 1, t = 1, above budget — no node), then re-enters on the
        // right side (x = 2, t = 2): a punch bridging the left and right
        // basins of the entry tick.
        let paths = [log(1, &[(0, 0, 0), (1, 1, 0), (2, 2, 0)])];
        let density = aggregate_traffic_core(&graph, &field, &stencil, 5, &paths);
        // The crossing produced punch traffic, and it is verdicted
        // detour_cheaper (the cheap tick-2 reconvergence beats the price-9
        // barrier), never crossing_cheaper.
        assert_eq!(
            punch_traffic(&graph, &density),
            1,
            "the ridge crossing is one punch unit"
        );
        assert_eq!(density.punch_crossing_cheaper, 0);
        assert_eq!(density.punch_detour_cheaper, 1);
    }

    #[test]
    fn an_unreachable_region_never_contributes_punch_traffic() {
        // A path that never enters an affordable component (off-grid the
        // whole time) produces no punch traffic at all.
        let field = corridor_3x1_held();
        let stencil = stencil_offsets();
        let graph = build_corridor_graph_core(&field, &stencil, 5);
        let paths = [log(1, &[(0, 9, 9), (1, 9, 9), (2, 9, 9)])];
        let density = aggregate_traffic_core(&graph, &field, &stencil, 5, &paths);
        assert_eq!(density.punch_crossing_cheaper, 0);
        assert_eq!(density.punch_detour_cheaper, 0);
    }

    #[test]
    fn aggregate_is_deterministic_and_content_addressable() {
        use aether_data::Kind;
        let field = corridor_3x1_held();
        let stencil = stencil_offsets();
        let graph = build_corridor_graph_core(&field, &stencil, 5);
        let paths = [log(1, &[(0, 0, 0), (1, 1, 0), (2, 2, 0)])];
        let a = aggregate_traffic_core(&graph, &field, &stencil, 5, &paths);
        let b = aggregate_traffic_core(&graph, &field, &stencil, 5, &paths);
        assert_eq!(a, b);
        assert_eq!(a.encode_into_bytes(), b.encode_into_bytes());
    }
}
