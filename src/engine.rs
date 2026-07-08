//! Rolling DAG store + health-metric snapshot builder.
//!
//! RPC-agnostic: `main` converts `RpcBlock` into `BlockNode`s and feeds them here; the metric logic
//! has no dependency on the node client. Tuned for the high-BPS regime where the decisive questions are
//! (1) how long blocks wait to be merged (merge latency vs the merge-depth ceiling) and (2) whether that
//! lag feeds through into confirmation time.

use std::collections::{HashMap, VecDeque};
use serde::Serialize;

const NET_DELAY_S: f64 = 0.9;
const HISTORY: usize = 120;
const MERGE_SAMPLES: usize = 4000;
const CONF_SAMPLES: usize = 4000;

/// Block data supplied by `main` (the RPC-facing input).
#[derive(Clone)]
pub struct BlockNode {
    pub hash: String,
    pub blue_score: u64,
    pub daa: u64,
    pub timestamp: u64, // ms — block production time (confirmation baseline)
    pub parents: Vec<String>,
    pub is_chain: bool,
    pub blues: u32, // mergeset blue count
    pub reds: u32,  // mergeset red count (wasted / orphaned)
}

/// Internal lifecycle wrapper — tracks when we first saw a block, when it was merged, and whether it
/// has reached the confirmation depth (so each block contributes to the merge/confirmation stats once).
struct Tracked {
    node: BlockNode,
    merge_lag: i64, // -1 = not yet merged; else blue-score rounds it waited
    confirmed: bool,
}

pub struct Engine {
    blocks: HashMap<String, Tracked>,
    order: VecDeque<String>,
    capacity: usize,
    viz_cap: usize,
    tip_history: VecDeque<usize>,
    bps_history: VecDeque<f64>,
    red_history: VecDeque<f64>,
    lat_history: VecDeque<f64>,
    merge_latencies: VecDeque<u64>,
    max_merge_latency: u64,
    conf_pairs: VecDeque<(f64, f64)>, // (merge_lag rounds, confirmation seconds)
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
            conf_pairs: VecDeque::new(),
            stress_peak: 0.0,
            fracture_events: 0,
            was_fractured: false,
            fracture_start_ms: None,
            last_fracture_secs: 0.0,
            max_fracture_secs: 0.0,
            peak_tip_width: 0,
        }
    }

    /// Ingest a block. `merged` is its mergeset (blue+red hashes). For a new chain block we record the
    /// merge latency of each block it merges, and stamp that block's own `merge_lag` (first merge wins).
    pub fn ingest(&mut self, node: BlockNode, merged: &[String]) {
        let is_new = !self.blocks.contains_key(&node.hash);

        if is_new && node.is_chain {
            for h in merged {
                let lat = match self.blocks.get(h) {
                    Some(t) => node.blue_score.saturating_sub(t.node.blue_score),
                    None => continue,
                };
                push_bounded(&mut self.merge_latencies, lat, MERGE_SAMPLES);
                if lat > self.max_merge_latency {
                    self.max_merge_latency = lat;
                }
                if let Some(t) = self.blocks.get_mut(h) {
                    if t.merge_lag < 0 {
                        t.merge_lag = lat as i64;
                    }
                }
            }
        }

        if is_new {
            self.order.push_back(node.hash.clone());
            self.blocks.insert(node.hash.clone(), Tracked { node, merge_lag: -1, confirmed: false });
        } else if let Some(t) = self.blocks.get_mut(&node.hash) {
            // refresh DAG data but preserve lifecycle stamps
            t.node = node;
        }

        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.blocks.remove(&old);
            }
        }
    }

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
        merge_depth: u64,
        conf_depth: u64,
    ) -> Snapshot {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let tip_width = tips.len();
        if tip_width > self.peak_tip_width {
            self.peak_tip_width = tip_width;
        }

        let frontier = self.blocks.values().map(|t| t.node.blue_score).max().unwrap_or(0);

        // Harvest confirmations: blocks now `conf_depth` below the frontier record (merge_lag, secs).
        let mut fresh: Vec<(f64, f64)> = Vec::new();
        for t in self.blocks.values_mut() {
            if !t.confirmed && t.merge_lag >= 0 && frontier.saturating_sub(t.node.blue_score) >= conf_depth {
                // baseline = block production time (not first-observed) to avoid observation-time bias
                fresh.push((t.merge_lag as f64, (now_ms - t.node.timestamp as i64) as f64 / 1000.0));
                t.confirmed = true;
            }
        }
        for p in fresh {
            self.conf_pairs.push_back(p);
            while self.conf_pairs.len() > CONF_SAMPLES {
                self.conf_pairs.pop_front();
            }
        }

        let tip_blues: Vec<u64> =
            tips.iter().filter_map(|t| self.blocks.get(t)).map(|t| t.node.blue_score).collect();
        let blue_min = tip_blues.iter().copied().min().unwrap_or(0);
        let blue_max = tip_blues.iter().copied().max().unwrap_or(0);
        let blue_delta = blue_max.saturating_sub(blue_min);

        let pcounts: Vec<usize> = self.blocks.values().map(|t| t.node.parents.len()).collect();
        let max_parents = pcounts.iter().copied().max().unwrap_or(0);
        let avg_parents =
            if pcounts.is_empty() { 0.0 } else { pcounts.iter().sum::<usize>() as f64 / pcounts.len() as f64 };
        let tip_excess = tip_width.saturating_sub(max_parents.max(1));

        let (mut blues_w, mut reds_w) = (0u64, 0u64);
        for t in self.blocks.values() {
            if t.node.is_chain {
                blues_w += t.node.blues as u64;
                reds_w += t.node.reds as u64;
            }
        }
        let red_rate = if blues_w + reds_w > 0 { reds_w as f64 / (blues_w + reds_w) as f64 } else { 0.0 };

        let (lat_mean, lat_p95, lat_max) = latency_stats(&self.merge_latencies, self.max_merge_latency);
        // headroom to the merge-depth cliff: how much of the budget the worst merge used.
        let depth_used_pct = if merge_depth > 0 { 100.0 * lat_max as f64 / merge_depth as f64 } else { 0.0 };

        // confirmation-time proxy + its correlation with merge lag.
        let conf_secs: Vec<f64> = self.conf_pairs.iter().map(|&(_, s)| s).collect();
        let conf_time_mean = mean(&conf_secs);
        let conf_time_p95 = p95(&conf_secs);
        let conf_corr = correlation(&self.conf_pairs);

        let stress = bps * bps * NET_DELAY_S * (tip_width.max(1) as f64);
        if stress > self.stress_peak && self.blocks.len() > 24 {
            self.stress_peak = stress;
        }

        let fracture = tip_width >= fracture_tip_width || blue_delta >= min_delta;
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

        // Viz: most-recent `viz_cap` nodes by blue score.
        let total = self.blocks.len();
        let tip_set: std::collections::HashSet<&String> = tips.iter().collect();
        let mut all: Vec<&Tracked> = self.blocks.values().collect();
        all.sort_by_key(|t| t.node.blue_score);
        let shown = &all[all.len().saturating_sub(self.viz_cap)..];
        let shown_ids: std::collections::HashSet<&str> = shown.iter().map(|t| t.node.hash.as_str()).collect();
        let nodes: Vec<VizNode> = shown
            .iter()
            .map(|t| VizNode {
                id: short(&t.node.hash),
                blue: t.node.blue_score,
                daa: t.node.daa,
                is_tip: tip_set.contains(&t.node.hash),
                red: t.node.is_chain && t.node.reds > 0,
                parents: t
                    .node
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
            merge_depth,
            depth_used_pct: round4(depth_used_pct),
            conf_depth,
            conf_time_mean: round2(conf_time_mean),
            conf_time_p95: round2(conf_time_p95),
            conf_corr: round4(conf_corr),
            conf_samples: self.conf_pairs.len(),
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
    let p = v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)] as f64;
    (mean, p, max)
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() { 0.0 } else { xs.iter().sum::<f64>() / xs.len() as f64 }
}
fn p95(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)]
}
/// Pearson correlation of (merge_lag, confirmation_secs).
fn correlation(pairs: &VecDeque<(f64, f64)>) -> f64 {
    let n = pairs.len();
    if n < 3 {
        return 0.0;
    }
    let (mx, my) = (
        pairs.iter().map(|p| p.0).sum::<f64>() / n as f64,
        pairs.iter().map(|p| p.1).sum::<f64>() / n as f64,
    );
    let mut cov = 0.0;
    let mut dx = 0.0;
    let mut dy = 0.0;
    for &(x, y) in pairs {
        cov += (x - mx) * (y - my);
        dx += (x - mx).powi(2);
        dy += (y - my).powi(2);
    }
    if dx == 0.0 || dy == 0.0 {
        0.0
    } else {
        cov / (dx.sqrt() * dy.sqrt())
    }
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
    pub merge_depth: u64,
    pub depth_used_pct: f64,
    pub conf_depth: u64,
    pub conf_time_mean: f64,
    pub conf_time_p95: f64,
    pub conf_corr: f64,
    pub conf_samples: usize,
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
