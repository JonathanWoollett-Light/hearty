//! Stable topological sort over the union of two DAGs, processed
//! component-by-component. Output is fully deterministic and idempotent.

use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::unionfind::UnionFind;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::hash::Hash;

/// Linearizes the union of `graph1` and `graph2` honoring every edge in both.
///
/// Weakly connected components of the union are emitted one at a time, in order
/// of their earliest (lowest node-index) member. Within each component a stable
/// topological sort is used: among the nodes whose dependencies are already
/// satisfied, the one with the lowest node-index is always emitted next.
///
/// Node indices reflect the order in which the callers added nodes (file order
/// for focuses, natural-alphabetical order for events), so that order is the
/// tie-break. Because the sort is *stable*, it is a fixed point: feeding it an
/// already-sorted sequence reproduces that sequence unchanged, which makes
/// repeated formatting idempotent. Errors on a cycle in the union.
pub fn priority_toposort<N>(
    graph1: &DiGraph<N, ()>,
    graph2: &DiGraph<N, ()>,
) -> Result<Vec<N>, String>
where
    N: Clone + Eq + Hash,
{
    // 1. Build the combined graph by interning node labels. We iterate graph1
    //    first then graph2, each via node_indices() / edge_indices() which are
    //    Vec-backed and stable — so `combined` gets a deterministic node-index
    //    assignment that mirrors the callers' insertion order.
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

    // 2. Weakly connected components via union-find.
    let mut uf: UnionFind<usize> = UnionFind::new(combined.node_count());
    #[expect(
        clippy::unwrap_used,
        reason = "edge indices come from combined.edge_indices(), so edge_endpoints cannot fail"
    )]
    for e in combined.edge_indices() {
        let (a, b) = combined.edge_endpoints(e).unwrap();
        uf.union(a.index(), b.index());
    }

    // Group nodes by component root. Iterating node_indices() in order means
    // each component's Vec is built in ascending index order, so nodes[0] is
    // the component's lowest node-index.
    let mut component_by_root: HashMap<usize, Vec<NodeIndex>> = HashMap::new();
    for nx in combined.node_indices() {
        let root = uf.find_mut(nx.index());
        component_by_root.entry(root).or_default().push(nx);
    }

    // 3. Order components by their earliest member's node-index. After one
    //    sort each component occupies a contiguous index range, so its minimum
    //    index is stable across runs — keeping component order idempotent too.
    #[expect(
        clippy::indexing_slicing,
        reason = "each component is non-empty by construction: we only insert \
                  into component_by_root when there is a node to push"
    )]
    let mut ordered_components: Vec<(usize, Vec<NodeIndex>)> = component_by_root
        .into_values()
        .map(|nodes| (nodes[0].index(), nodes))
        .collect();
    ordered_components.sort_by_key(|&(min_idx, _)| min_idx);

    // 4. Stable topological sort per component (Kahn's algorithm). The heap key
    //    is the node-index, so among the ready (in-degree 0) nodes the lowest
    //    index is always taken next — the defining property of a stable
    //    topological sort, and the reason re-sorting is a no-op.
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
    for (_, nodes) in ordered_components {
        let mut heap: BinaryHeap<Reverse<usize>> = BinaryHeap::new();
        for &nx in &nodes {
            if in_degree[&nx] == 0 {
                heap.push(Reverse(nx.index()));
            }
        }

        while let Some(Reverse(raw)) = heap.pop() {
            let nx = NodeIndex::new(raw);
            result.push(combined[nx].clone());

            let successors: Vec<NodeIndex> = combined
                .neighbors_directed(nx, Direction::Outgoing)
                .collect();
            for m in successors {
                let d = in_degree.get_mut(&m).unwrap();
                *d -= 1;
                if *d == 0 {
                    heap.push(Reverse(m.index()));
                }
            }
        }
    }

    if result.len() != combined.node_count() {
        return Err("union of graph1 and graph2 contains a cycle".to_owned());
    }
    Ok(result)
}
