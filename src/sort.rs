//! Priority topological sort over the union of two DAGs, processed
//! component-by-component. Output is fully deterministic.

use petgraph::Direction;
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::unionfind::UnionFind;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::hash::Hash;

/// Linearizes the union of `graph1` and `graph2` honoring every edge in both.
///
/// Weakly connected components of the union are emitted one at a time, in
/// order of (min graph1-rank, min node-index). Within each component,
/// graph1's topological order wins any tie; node-index breaks remaining
/// ties so output is fully deterministic. Errors on a cycle in graph1 or
/// in the union.
pub fn priority_toposort<N>(
    graph1: &DiGraph<N, ()>,
    graph2: &DiGraph<N, ()>,
) -> Result<Vec<N>, String>
where
    N: Clone + Eq + Hash,
{
    // 1. Rank nodes by graph1's topological order. petgraph's toposort is
    //    deterministic because it walks node_indices() in order, so the
    //    ranks assigned here are stable across runs.
    let g1_order = toposort(graph1, None).map_err(|_cycle| "graph1 has a cycle".to_owned())?;
    let mut rank: HashMap<N, usize> = HashMap::new();
    for (i, nx) in g1_order.iter().enumerate() {
        rank.insert(graph1[*nx].clone(), i);
    }
    let sentinel = g1_order.len();

    // 2. Build the combined graph by interning node labels. We iterate
    //    graph1 first then graph2, each via node_indices() / edge_indices()
    //    which are Vec-backed and stable — so `combined` has a deterministic
    //    node-index assignment.
    let mut combined: DiGraph<N, ()> = DiGraph::new();
    let mut label_to_idx: HashMap<N, NodeIndex> = HashMap::new();
    #[expect(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        reason = "edge indices come from g.edge_indices() and every node label \
                  from both graphs was interned into label_to_idx in the loop \
                  above, so both lookups are infallible"
    )]
    for g in [graph1, graph2] {
        for nx in g.node_indices() {
            let label = &g[nx];
            if !label_to_idx.contains_key(label) {
                let new_idx = combined.add_node(label.clone());
                label_to_idx.insert(label.clone(), new_idx);
            }
        }
        for e in g.edge_indices() {
            let (a, b) = g.edge_endpoints(e).unwrap();
            let ia = label_to_idx[&g[a]];
            let ib = label_to_idx[&g[b]];
            combined.add_edge(ia, ib, ());
        }
    }

    // 3. Weakly connected components via union-find.
    let mut uf: UnionFind<usize> = UnionFind::new(combined.node_count());
    #[expect(
        clippy::unwrap_used,
        reason = "edge indices come from combined.edge_indices(), so edge_endpoints cannot fail"
    )]
    for e in combined.edge_indices() {
        let (a, b) = combined.edge_endpoints(e).unwrap();
        uf.union(a.index(), b.index());
    }

    // Group nodes by component root. Iterating node_indices() in order
    // means each component's Vec is built in ascending index order, which
    // is deterministic — the HashMap's iteration order never matters
    // because we extract values into a Vec and sort with a total key below.
    let mut component_by_root: HashMap<usize, Vec<NodeIndex>> = HashMap::new();
    for nx in combined.node_indices() {
        let root = uf.find_mut(nx.index());
        component_by_root.entry(root).or_default().push(nx);
    }

    // 4. Order components by (min_rank, first_node_index). Both fields are
    //    deterministic: min_rank comes from graph1's stable toposort, and
    //    first_node_index is the lowest NodeIndex in the component (the
    //    Vec is already in index-order, so nodes[0] is that minimum).
    #[expect(
        clippy::indexing_slicing,
        reason = "each component is non-empty by construction: we only insert \
                  into component_by_root when there is a node to push"
    )]
    let mut ordered_components: Vec<(usize, usize, Vec<NodeIndex>)> = component_by_root
        .into_values()
        .map(|nodes| {
            let min_rank = nodes
                .iter()
                .map(|&nx| rank.get(&combined[nx]).copied().unwrap_or(sentinel))
                .min()
                .unwrap_or(sentinel);
            let first_idx = nodes[0].index();
            (min_rank, first_idx, nodes)
        })
        .collect();
    ordered_components.sort_by_key(|&(r, i, _)| (r, i));

    // 5. Kahn's algorithm per component. Heap key is (rank, node_index),
    //    a deterministic total order — no two nodes share a node_index, so
    //    there are no true ties and pop order is fully determined.
    let mut in_degree: HashMap<NodeIndex, usize> = combined
        .node_indices()
        .map(|nx| {
            (
                nx,
                combined.neighbors_directed(nx, Direction::Incoming).count(),
            )
        })
        .collect();

    let mut result = Vec::with_capacity(combined.node_count());
    #[expect(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        reason = "in_degree was populated from every node in combined.node_indices(), \
                  so any node index produced by iteration or neighbor traversal is a key"
    )]
    for (_, _, nodes) in ordered_components {
        let mut heap: BinaryHeap<Reverse<(usize, usize)>> = BinaryHeap::new();
        for &nx in &nodes {
            if in_degree[&nx] == 0 {
                let r = rank.get(&combined[nx]).copied().unwrap_or(sentinel);
                heap.push(Reverse((r, nx.index())));
            }
        }

        while let Some(Reverse((_, raw))) = heap.pop() {
            let nx = NodeIndex::new(raw);
            result.push(combined[nx].clone());

            // neighbors_directed iterates in edge-insertion order — stable,
            // because we added edges in graph1-then-graph2 order above.
            let successors: Vec<NodeIndex> = combined
                .neighbors_directed(nx, Direction::Outgoing)
                .collect();
            for m in successors {
                let d = in_degree.get_mut(&m).unwrap();
                *d -= 1;
                if *d == 0 {
                    let r = rank.get(&combined[m]).copied().unwrap_or(sentinel);
                    heap.push(Reverse((r, m.index())));
                }
            }
        }
    }

    if result.len() != combined.node_count() {
        return Err("union of graph1 and graph2 contains a cycle".to_owned());
    }
    Ok(result)
}
