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
    // verbose (ghostdag) data — the efficiency signal
    pub is_chain: bool,
    pub blues: u32, // mergeset blue count (work that counted)
    pub reds: u32,  // mergeset red count (work that was wasted / orphaned)
}

pub struct Engine {
    blocks: HashMap<String, BlockNode>,
    order: VecDeque<String>, // insertion order ≈ blue order (get_blocks returns blue-sorted)
    capacity: usize,
    tip_history: VecDeque<usize>,
    bps_history: VecDeque<f64>,
    red_history: VecDeque<f64>,
    stress_peak: f64,
    fracture_events: u64,
    was_fractured: bool,
    fracture_start_ms: Option<i64>,
    last_fracture_secs: f64,
    max_fracture_secs: f64,
    peak_tip_width: usize,
}

impl Engine {
    pub fn new(capacity: usize) -> Self {
        Self {
            blocks: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(16),
            tip_history: VecDeque::new(),
            bps_history: VecDeque::new(),
            red_history: VecDeque::new(),
            stress_peak: 0.0,
            fracture_events: 0,
            was_fractured: false,
            fracture_start_ms: None,
            last_fracture_secs: 0.0,
            max_fracture_secs: 0.0,
            peak_tip_width: 0,
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
        if tip_width > self.peak_tip_width {
            self.peak_tip_width = tip_width;
        }

        // Blue-score spread across the tips we can see in the window.
        let tip_blues: Vec<u64> =
            tips.iter().filter_map(|t| self.blocks.get(t)).map(|b| b.blue_score).collect();
        let blue_min = tip_blues.iter().copied().min().unwrap_or(0);
        let blue_max = tip_blues.iter().copied().max().unwrap_or(0);
        let blue_delta = blue_max.saturating_sub(blue_min);

        // Merge behaviour: how many parents blocks actually reference. The max observed ≈ the node's
        // effective parent cap; tips beyond it can't be merged this round → they risk becoming red.
        let pcounts: Vec<usize> = self.blocks.values().map(|b| b.parents.len()).collect();
        let max_parents = pcounts.iter().copied().max().unwrap_or(0);
        let avg_parents =
            if pcounts.is_empty() { 0.0 } else { pcounts.iter().sum::<usize>() as f64 / pcounts.len() as f64 };
        let tip_excess = tip_width.saturating_sub(max_parents.max(1));

        // Orphan / red rate — the real efficiency cost. Each merged block is counted once (as blue or
        // red) by the chain block that merged it, so summing chain-block mergesets covers the window.
        let (mut blues_w, mut reds_w) = (0u64, 0u64);
        for b in self.blocks.values() {
            if b.is_chain {
                blues_w += b.blues as u64;
                reds_w += b.reds as u64;
            }
        }
        let red_rate = if blues_w + reds_w > 0 { reds_w as f64 / (blues_w + reds_w) as f64 } else { 0.0 };

        // Stress index Φ ≈ λ²·Δ·W (honest derived index, not a claimed reorg probability).
        let stress = bps * bps * NET_DELAY_S * (tip_width.max(1) as f64);
        if stress > self.stress_peak && self.blocks.len() > 24 {
            self.stress_peak = stress;
        }

        // Fracture state + duration tracking.
        let fracture = tip_width >= fracture_tip_width || blue_delta >= min_delta;
        let now_ms = chrono::Utc::now().timestamp_millis();
        if fracture {
            if !self.was_fractured {
                self.fracture_events += 1;
            }
            if self.fracture_start_ms.is_none() {
                self.fracture_start_ms = Some(now_ms);
            }
        } else if let Some(start) = self.fracture_start_ms.take() {
            self.last_fracture_secs = (now_ms - start) as f64 / 1000.0;
            if self.last_fracture_secs > self.max_fracture_secs {
                self.max_fracture_secs = self.last_fracture_secs;
            }
        }
        self.was_fractured = fracture;
        let fracture_secs = self.fracture_start_ms.map(|s| (now_ms - s) as f64 / 1000.0).unwrap_or(0.0);

        push_bounded(&mut self.tip_history, tip_width);
        push_bounded(&mut self.bps_history, bps);
        push_bounded(&mut self.red_history, red_rate * 100.0);

        let tip_set: std::collections::HashSet<&String> = tips.iter().collect();
        let mut nodes: Vec<VizNode> = self
            .blocks
            .values()
            .map(|b| VizNode {
                id: short(&b.hash),
                blue: b.blue_score,
                daa: b.daa,
                is_tip: tip_set.contains(&b.hash),
                red: b.is_chain && b.reds > 0,
                parents: b.parents.iter().filter(|p| self.blocks.contains_key(*p)).map(|p| short(p)).collect(),
            })
            .collect();
        nodes.sort_by_key(|n| n.blue);

        Snapshot {
            connected: true,
            network,
            sink: short(&sink),
            tip_width,
            peak_tip_width: self.peak_tip_width,
            bps: round2(bps),
            virtual_daa,
            block_count,
            header_count,
            difficulty: round2(difficulty),
            blue_min,
            blue_max,
            blue_delta,
            max_parents,
            avg_parents: round2(avg_parents),
            tip_excess,
            red_rate: round4(red_rate),
            reds_window: reds_w,
            blues_window: blues_w,
            stress: round2(stress),
            stress_peak: round2(self.stress_peak),
            fracture,
            fracture_secs: round2(fracture_secs),
            max_fracture_secs: round2(self.max_fracture_secs),
            fracture_events: self.fracture_events,
            window: self.blocks.len(),
            nodes,
            tips: tips.iter().map(|t| short(t)).collect(),
            tip_history: self.tip_history.iter().copied().collect(),
            bps_history: self.bps_history.iter().map(|&b| round2(b)).collect(),
            red_history: self.red_history.iter().map(|&r| round2(r)).collect(),
            updated_ms: now_ms,
        }
    }
}

fn push_bounded<T>(q: &mut VecDeque<T>, v: T) {
    q.push_back(v);
    while q.len() > HISTORY {
        q.pop_front();
    }
}
fn short(h: &str) -> String {
    h.chars().take(10).collect()
}
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}
fn round4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

#[derive(Serialize, Clone, Default)]
pub struct Snapshot {
    pub connected: bool,
    pub network: String,
    pub sink: String,
    pub tip_width: usize,
    pub peak_tip_width: usize,
    pub bps: f64,
    pub virtual_daa: u64,
    pub block_count: u64,
    pub header_count: u64,
    pub difficulty: f64,
    pub blue_min: u64,
    pub blue_max: u64,
    pub blue_delta: u64,
    pub max_parents: usize,
    pub avg_parents: f64,
    pub tip_excess: usize,
    pub red_rate: f64,
    pub reds_window: u64,
    pub blues_window: u64,
    pub stress: f64,
    pub stress_peak: f64,
    pub fracture: bool,
    pub fracture_secs: f64,
    pub max_fracture_secs: f64,
    pub fracture_events: u64,
    pub window: usize,
    pub nodes: Vec<VizNode>,
    pub tips: Vec<String>,
    pub tip_history: Vec<usize>,
    pub bps_history: Vec<f64>,
    pub red_history: Vec<f64>,
    pub updated_ms: i64,
}

#[derive(Serialize, Clone)]
pub struct VizNode {
    pub id: String,
    pub blue: u64,
    pub daa: u64,
    pub is_tip: bool,
    pub red: bool,
    pub parents: Vec<String>,
}
