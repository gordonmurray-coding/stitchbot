//! Rolling DAG store + health-metric snapshot builder.
//!
//! Deliberately RPC-agnostic: `main` converts `RpcBlock` into plain values and feeds them here,
//! so the metric logic has no dependency on the node client and is trivially testable.

use std::collections::{HashMap, VecDeque};
use serde::Serialize;

/// Network-delay bound used in the stress index (Kaspa ≈ 0.9 s in practice; see README).
const NET_DELAY_S: f64 = 0.9;
/// How many recent samples to keep for the sparklines.
const HISTORY: usize = 120;

#[derive(Clone)]
pub struct BlockNode {
    pub hash: String,
    pub blue_score: u64,
    pub daa: u64,
    #[allow(dead_code)] // kept for future convergence-time metrics
    pub timestamp: u64, // ms
    pub parents: Vec<String>,
}

pub struct Engine {
    blocks: HashMap<String, BlockNode>,
    order: VecDeque<String>, // insertion order ≈ blue order (get_blocks returns blue-sorted)
    capacity: usize,
    tip_history: VecDeque<usize>,
    bps_history: VecDeque<f64>,
    stress_peak: f64,
    fracture_events: u64,
    was_fractured: bool,
}

impl Engine {
    pub fn new(capacity: usize) -> Self {
        Self {
            blocks: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(16),
            tip_history: VecDeque::new(),
            bps_history: VecDeque::new(),
            stress_peak: 0.0,
            fracture_events: 0,
            was_fractured: false,
        }
    }

    pub fn ingest(&mut self, node: BlockNode) {
        if !self.blocks.contains_key(&node.hash) {
            self.order.push_back(node.hash.clone());
        }
        self.blocks.insert(node.hash.clone(), node);
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.blocks.remove(&old);
            }
        }
    }

    /// Build a JSON-serializable snapshot for the dashboard.
    #[allow(clippy::too_many_arguments)]
    pub fn snapshot(
        &mut self,
        network: String,
        sink: String,
        virtual_daa: u64,
        block_count: u64,
        header_count: u64,
        difficulty: f64,
        bps: f64,
        tips: &[String],
        fracture_tip_width: usize,
        min_delta: u64,
    ) -> Snapshot {
        let tip_width = tips.len();

        // Blue-score spread across the tips we can see in the window.
        let tip_blues: Vec<u64> =
            tips.iter().filter_map(|t| self.blocks.get(t)).map(|b| b.blue_score).collect();
        let blue_min = tip_blues.iter().copied().min().unwrap_or(0);
        let blue_max = tip_blues.iter().copied().max().unwrap_or(0);
        let blue_delta = blue_max.saturating_sub(blue_min);

        // Stress index Φ ≈ λ²·Δ·W  (honest derived index, not a claimed reorg probability).
        let stress = bps * bps * NET_DELAY_S * (tip_width.max(1) as f64);
        // ignore the startup transient (first BPS sample can spike on a large count delta / tiny dt)
        if stress > self.stress_peak && self.blocks.len() > 24 {
            self.stress_peak = stress;
        }

        let fracture = tip_width >= fracture_tip_width || blue_delta >= min_delta;
        if fracture && !self.was_fractured {
            self.fracture_events += 1;
        }
        self.was_fractured = fracture;

        // sparkline history
        self.tip_history.push_back(tip_width);
        self.bps_history.push_back(bps);
        while self.tip_history.len() > HISTORY {
            self.tip_history.pop_front();
        }
        while self.bps_history.len() > HISTORY {
            self.bps_history.pop_front();
        }

        let tip_set: std::collections::HashSet<&String> = tips.iter().collect();
        let mut nodes: Vec<VizNode> = self
            .blocks
            .values()
            .map(|b| VizNode {
                id: short(&b.hash),
                blue: b.blue_score,
                daa: b.daa,
                is_tip: tip_set.contains(&b.hash),
                // only keep parent links that are present in the window (so the viz has both ends)
                parents: b
                    .parents
                    .iter()
                    .filter(|p| self.blocks.contains_key(*p))
                    .map(|p| short(p))
                    .collect(),
            })
            .collect();
        nodes.sort_by_key(|n| n.blue);

        Snapshot {
            connected: true,
            network,
            sink: short(&sink),
            tip_width,
            bps: round2(bps),
            virtual_daa,
            block_count,
            header_count,
            difficulty: round2(difficulty),
            blue_min,
            blue_max,
            blue_delta,
            stress: round2(stress),
            stress_peak: round2(self.stress_peak),
            fracture,
            fracture_events: self.fracture_events,
            window: self.blocks.len(),
            nodes,
            tips: tips.iter().map(|t| short(t)).collect(),
            tip_history: self.tip_history.iter().copied().collect(),
            bps_history: self.bps_history.iter().map(|&b| round2(b)).collect(),
            updated_ms: chrono::Utc::now().timestamp_millis(),
        }
    }
}

fn short(h: &str) -> String {
    h.chars().take(10).collect()
}
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[derive(Serialize, Clone, Default)]
pub struct Snapshot {
    pub connected: bool,
    pub network: String,
    pub sink: String,
    pub tip_width: usize,
    pub bps: f64,
    pub virtual_daa: u64,
    pub block_count: u64,
    pub header_count: u64,
    pub difficulty: f64,
    pub blue_min: u64,
    pub blue_max: u64,
    pub blue_delta: u64,
    pub stress: f64,
    pub stress_peak: f64,
    pub fracture: bool,
    pub fracture_events: u64,
    pub window: usize,
    pub nodes: Vec<VizNode>,
    pub tips: Vec<String>,
    pub tip_history: Vec<usize>,
    pub bps_history: Vec<f64>,
    pub updated_ms: i64,
}

#[derive(Serialize, Clone)]
pub struct VizNode {
    pub id: String,
    pub blue: u64,
    pub daa: u64,
    pub is_tip: bool,
    pub parents: Vec<String>,
}
