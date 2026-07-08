//! Rolling DAG store + health-metric snapshot builder.
//!
//! Deliberately RPC-agnostic: `main` converts `RpcBlock` into plain values and feeds them here,
//! so the metric logic has no dependency on the node client and is trivially testable.
//!
//! Tuned for the high-BPS regime: at 100 BPS tip width runs far past the 16-parent merge cap, so the
//! decisive metric is *merge latency* — how many blue-score rounds a block waits before a chain block
//! merges it. That directly tests the core devs' "every block merged after log rounds" conjecture.

use std::collections::{HashMap, VecDeque};
use serde::Serialize;

/// Network-delay bound used in the stress index (Kaspa ≈ 0.9 s in practice; see README).
const NET_DELAY_S: f64 = 0.9;
/// Samples kept for the sparklines.
const HISTORY: usize = 120;
/// Merge-latency samples kept for the distribution (large — many merges per poll at high BPS).
const MERGE_SAMPLES: usize = 4000;

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
    viz_cap: usize,
    tip_history: VecDeque<usize>,
    bps_history: VecDeque<f64>,
    red_history: VecDeque<f64>,
    lat_history: VecDeque<f64>,
    merge_latencies: VecDeque<u64>,
    max_merge_latency: u64,
    stress_peak: f64,
    fracture_events: u64,
    was_fractured: bool,
    fracture_start_ms: Option<i64>,
    last_fracture_secs: f64,
    max_fracture_secs: f64,
    peak_tip_width: usize,
}

impl Engine {
    pub fn new(capacity: usize, viz_cap: usize) -> Self {
        Self {
            blocks: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(16),
            viz_cap: viz_cap.max(50),
            tip_history: VecDeque::new(),
            bps_history: VecDeque::new(),
            red_history: VecDeque::new(),
            lat_history: VecDeque::new(),
            merge_latencies: VecDeque::new(),
            max_merge_latency: 0,
            stress_peak: 0.0,
            fracture_events: 0,
            was_fractured: false,
            fracture_start_ms: None,
            last_fracture_secs: 0.0,
            max_fracture_secs: 0.0,
            peak_tip_width: 0,
        }
    }

    /// Ingest a block. `merged` is this block's mergeset (blue + red hashes); for a *new chain block*
    /// we record the merge latency of each merged block still in the window.
    pub fn ingest(&mut self, node: BlockNode, merged: &[String]) {
        let is_new = !self.blocks.contains_key(&node.hash);
        if is_new && node.is_chain {
            for h in merged {
                if let Some(b) = self.blocks.get(h) {
                    let lat = node.blue_score.saturating_sub(b.blue_score);
                    push_bounded(&mut self.merge_latencies, lat, MERGE_SAMPLES);
                    if lat > self.max_merge_latency {
                        self.max_merge_latency = lat;
                    }
                }
            }
        }
        if is_new {
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

        // Merge behaviour: max parents ≈ the effective cap; tips beyond it can't be merged this round.
        let pcounts: Vec<usize> = self.blocks.values().map(|b| b.parents.len()).collect();
        let max_parents = pcounts.iter().copied().max().unwrap_or(0);
        let avg_parents =
            if pcounts.is_empty() { 0.0 } else { pcounts.iter().sum::<usize>() as f64 / pcounts.len() as f64 };
        let tip_excess = tip_width.saturating_sub(max_parents.max(1));

        // Orphan / red rate — each merged block is counted once by the chain block that merged it.
        let (mut blues_w, mut reds_w) = (0u64, 0u64);
        for b in self.blocks.values() {
            if b.is_chain {
                blues_w += b.blues as u64;
                reds_w += b.reds as u64;
            }
        }
        let red_rate = if blues_w + reds_w > 0 { reds_w as f64 / (blues_w + reds_w) as f64 } else { 0.0 };

        // Merge-latency distribution (blue-score rounds a block waited to be merged) — the conjecture test.
        let (lat_mean, lat_p95, lat_max) = latency_stats(&self.merge_latencies, self.max_merge_latency);

        let stress = bps * bps * NET_DELAY_S * (tip_width.max(1) as f64);
        if stress > self.stress_peak && self.blocks.len() > 24 {
            self.stress_peak = stress;
        }

        // Fracture state + duration.
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

        push_bounded(&mut self.tip_history, tip_width, HISTORY);
        push_bounded(&mut self.bps_history, bps, HISTORY);
        push_bounded(&mut self.red_history, red_rate * 100.0, HISTORY);
        push_bounded(&mut self.lat_history, lat_mean, HISTORY);

        // Viz: send only the most-recent `viz_cap` nodes (highest blue) so the browser stays snappy
        // even when the metric window holds thousands of blocks at 100 BPS.
        let total = self.blocks.len();
        let tip_set: std::collections::HashSet<&String> = tips.iter().collect();
        let mut all: Vec<&BlockNode> = self.blocks.values().collect();
        all.sort_by_key(|b| b.blue_score);
        let shown = &all[all.len().saturating_sub(self.viz_cap)..];
        let shown_ids: std::collections::HashSet<&str> = shown.iter().map(|b| b.hash.as_str()).collect();
        let nodes: Vec<VizNode> = shown
            .iter()
            .map(|b| VizNode {
                id: short(&b.hash),
                blue: b.blue_score,
                daa: b.daa,
                is_tip: tip_set.contains(&b.hash),
                red: b.is_chain && b.reds > 0,
                parents: b
                    .parents
                    .iter()
                    .filter(|p| shown_ids.contains(p.as_str()))
                    .map(|p| short(p))
                    .collect(),
            })
            .collect();

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
            merge_lat_mean: round2(lat_mean),
            merge_lat_p95: round2(lat_p95),
            merge_lat_max: lat_max,
            stress: round2(stress),
            stress_peak: round2(self.stress_peak),
            fracture,
            fracture_secs: round2(fracture_secs),
            max_fracture_secs: round2(self.max_fracture_secs),
            fracture_events: self.fracture_events,
            window: total,
            viz_shown: nodes.len(),
            nodes,
            tips: tips.iter().map(|t| short(t)).collect(),
            tip_history: self.tip_history.iter().copied().collect(),
            bps_history: self.bps_history.iter().map(|&b| round2(b)).collect(),
            red_history: self.red_history.iter().map(|&r| round2(r)).collect(),
            lat_history: self.lat_history.iter().map(|&l| round2(l)).collect(),
            updated_ms: now_ms,
        }
    }
}

fn latency_stats(q: &VecDeque<u64>, max: u64) -> (f64, f64, u64) {
    if q.is_empty() {
        return (0.0, 0.0, 0);
    }
    let mean = q.iter().sum::<u64>() as f64 / q.len() as f64;
    let mut v: Vec<u64> = q.iter().copied().collect();
    v.sort_unstable();
    let p95 = v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)] as f64;
    (mean, p95, max)
}

fn push_bounded<T>(q: &mut VecDeque<T>, v: T, limit: usize) {
    q.push_back(v);
    while q.len() > limit {
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
    pub merge_lat_mean: f64,
    pub merge_lat_p95: f64,
    pub merge_lat_max: u64,
    pub stress: f64,
    pub stress_peak: f64,
    pub fracture: bool,
    pub fracture_secs: f64,
    pub max_fracture_secs: f64,
    pub fracture_events: u64,
    pub window: usize,
    pub viz_shown: usize,
    pub nodes: Vec<VizNode>,
    pub tips: Vec<String>,
    pub tip_history: Vec<usize>,
    pub bps_history: Vec<f64>,
    pub red_history: Vec<f64>,
    pub lat_history: Vec<f64>,
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
