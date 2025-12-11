use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Directed;
use graphrs::algorithms::centrality::betweenness::betweenness_centrality;  // ← Correct import from graphrs
use kaspa_consensus_core::block::Block;
use std::collections::{HashMap, VecDeque};

pub type Dag = DiGraph<BlockInfo, (), Directed>;

#[derive(Clone, Debug)]
pub struct BlockInfo {
    pub hash: String,
    pub blue_score: u64,
    pub parents: Vec<String>,
    pub timestamp: u64,
}

pub struct RollingDag {
    pub graph: Dag,
    pub idx: HashMap<String, NodeIndex>,
    order: VecDeque<String>,
    capacity: usize,
}

impl RollingDag {
    pub fn new(capacity: usize) -> Self {
        Self {
            graph: DiGraph::new(),
            idx: HashMap::new(),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn add_block(&mut self, block: Block) -> bool {
        let info = BlockInfo {
            hash: block.hash().to_string(),
            blue_score: block.header.blue_score,
            parents: block.header.direct_parents.iter().map(|h| h.to_string()).collect(),
            timestamp: block.header.timestamp,
        };
        let hash = info.hash.clone();

        // Prune old blocks if over capacity
        if self.order.len() >= self.capacity {
            if let Some(old_hash) = self.order.pop_front() {
                if let Some(&node) = self.idx.get(&old_hash) {
                    self.graph.remove_node(node);
                }
                self.idx.remove(&old_hash);
            }
        }

        let node = self.graph.add_node(info.clone());
        self.idx.insert(hash.clone(), node);
        self.order.push_back(hash);

        // Add edges from parents
        for parent in &info.parents {
            if let Some(&p_node) = self.idx.get(parent) {
                self.graph.add_edge(p_node, node, ());
            }
        }

        true
    }

    /// Checks if a block is on the selected (blue) chain
    pub fn is_in_selected_chain(&self, block: &Block) -> bool {
        let hash = block.hash().to_string();
        let Some(&node_idx) = self.idx.get(&hash) else { return false };

        // Walk up to find highest blue score ancestor
        let mut current = node_idx;
        let mut max_blue = self.graph[node_idx].blue_score;
        let mut selected = node_idx;

        while let Some(parent) = self.graph.neighbors_directed(current, petgraph::Direction::Incoming).next() {
            let p_blue = self.graph[parent].blue_score;
            if p_blue > max_blue {
                max_blue = p_blue;
                selected = parent;
            }
            current = parent;
        }

        // Walk down from selected ancestor along max-blue children
        let mut current = selected;
        loop {
            if current == node_idx {
                return true;
            }
            let next = self.graph
                .neighbors_directed(current, petgraph::Direction::Outgoing)
                .max_by_key(|&c| self.graph[c].blue_score);

            match next {
                Some(n) => current = n,
                None => return false,
            }
        }
    }

    /// Find the best fracture to stitch: high betweenness + large blue delta
    pub fn find_fracture(&self, min_delta: u64) -> Option<(NodeIndex, Vec<NodeIndex>)> {
        if self.graph.node_count() < 10 {
            return None;
        }

        // Compute betweenness centrality (O(V^3) but fine for 10k nodes)
        let betweenness = betweenness_centrality(&self.graph, false, true);  // normalized=False, parallel=True

        let mut candidates = Vec::new();

        for node in self.graph.node_indices() {
            let children: Vec<_> = self.graph.neighbors_directed(node, petgraph::Direction::Outgoing).collect();
            if children.len() < 2 {
                continue;
            }

            let info = &self.graph[node];
            let mut max_child_blue = 0;
            for &child in &children {
                let child_blue = self.graph[child].blue_score;
                if child_blue > max_child_blue {
                    max_child_blue = child_blue;
                }
            }

            let delta = max_child_blue.saturating_sub(info.blue_score);
            if delta < min_delta {
                continue;
            }

            let centrality = betweenness[node.index()];
            candidates.push((node, centrality, delta, children));
        }

        // Sort by betweenness (high) + delta (low) = most critical fracture first
        candidates.sort_by_key(|&(_, centrality, delta, _)| {
            std::cmp::Reverse(
                ((centrality * 1_000_000.0) + (1_000_000.0 / (delta as f64 + 1.0))) as u64
            )
        });

        candidates.into_iter().next().map(|(node, _, _, tips)| (node, tips))
    }
}
