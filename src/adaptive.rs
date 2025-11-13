use std::collections::VecDeque;
use chrono::Utc;

pub struct AdaptiveEngine {
    // Rolling stats
    recent_convergence: VecDeque<u64>,
    recent_orphans: VecDeque<bool>,
    last_stitch: i64,
    config: super::config::Config,
}

impl AdaptiveEngine {
    pub fn new(config: super::config::Config) -> Self {
        Self {
            recent_convergence: VecDeque::with_capacity(100),
            recent_orphans: VecDeque::with_capacity(3600),
            last_stitch: 0,
            config,
        }
    }

    pub fn update_block(&mut self, block: &kaspa_consensus_core::block::Block, is_orphan: bool) {
        // Track convergence time
        if let Some(parent) = block.header.direct_parents.first() {
            let parent_time = /* fetch parent timestamp */;
            let conv = block.header.timestamp.saturating_sub(parent_time);
            self.recent_convergence.push_back(conv);
            if self.recent_convergence.len() > 100 {
                self.recent_convergence.pop_front();
            }
        }
        self.recent_orphans.push_back(is_orphan);
        if self.recent_orphans.len() > 3600 {
            self.recent_orphans.pop_front();
        }
    }

    pub fn sus(&self, blue_delta: u64, bps: f64) -> f64 {
        let avg_conv = self.recent_convergence.iter().sum::<u64>() as f64 / self.recent_convergence.len() as f64;
        let orphan_rate = self.recent_orphans.iter().filter(|&&o| o).count() as f64 / 3600.0;
        let load = bps / 10.0;

        (blue_delta as f64 / 1000.0)
            * (avg_conv / 10.0)
            * (orphan_rate * 100.0)
            * (1.0 + load)
    }

    pub fn should_stitch(&mut self, blue_delta: u64, bps: f64) -> bool {
        if !self.config.adaptive { return true; }

        let now = Utc::now().timestamp();
        let sus = self.sus(blue_delta, bps);
        let min_delta = (self.config.base_min_delta as f64 / (1.0 + sus)) as u64;
        let rate_limit = (self.config.base_rate_limit as f64 / (1.0 + sus)) as i64;

        blue_delta >= min_delta && now - self.last_stitch >= rate_limit
    }

    pub fn reward(&self, sus: f64) -> u64 {
        let base = self.config.base_reward_sompi;
        let bonus = (base as f64 * sus.min(5.0)) as u64;
        (base + bonus).min(self.config.max_reward_sompi)
    }

    pub fn record_stitch(&mut self) {
        self.last_stitch = Utc::now().timestamp();
    }
}
