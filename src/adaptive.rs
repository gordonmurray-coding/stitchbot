use std::collections::VecDeque;
use chrono::Utc;
use kaspa_rpc_core::client::RpcClient;
use kaspa_consensus_core::block::Block;
use anyhow::Result;

#[derive(Clone)]
pub struct Config {
    pub adaptive: bool,
    pub base_min_delta: u64,
    pub base_rate_limit: u64,
    pub base_reward_sompi: u64,
    pub max_reward_sompi: u64,
    pub min_rate_limit: u64,
    pub rpc_url: String,
}

pub struct AdaptiveEngine {
    recent_convergence: VecDeque<u64>,
    recent_orphans: VecDeque<bool>,
    last_stitch: i64,
    config: Config,
    rpc: RpcClient,
}

impl AdaptiveEngine {
    pub fn new(config: Config) -> Self {
        let rpc_url = config.rpc_url.replace("ws", "http");
        let rpc = RpcClient::new(&rpc_url).expect("RPC connect failed");
        Self {
            recent_convergence: VecDeque::with_capacity(100),
            recent_orphans: VecDeque::with_capacity(3600),
            last_stitch: 0,
            config,
            rpc,
        }
    }

    pub async fn update_block(&mut self, block: &Block, is_orphan: bool) -> Result<()> {
        if let Some(parent_hash) = block.header.direct_parents.first() {
            if let Ok(parent) = self.rpc.get_block(parent_hash).await {
                let conv = block.header.timestamp.saturating_sub(parent.header.timestamp);
                self.recent_convergence.push_back(conv);
                if self.recent_convergence.len() > 100 {
                    self.recent_convergence.pop_front();
                }
            }
        }
        self.recent_orphans.push_back(is_orphan);
        if self.recent_orphans.len() > 3600 {
            self.recent_orphans.pop_front();
        }
        Ok(())
    }

    pub fn sus(&self, blue_delta: u64, bps: f64) -> f64 {
        let avg_conv = if self.recent_convergence.is_empty() { 1.0 } else {
            self.recent_convergence.iter().sum::<u64>() as f64 / self.recent_convergence.len() as f64
        };
        let orphan_rate = self.recent_orphans.iter().filter(|&&o| o).count() as f64 / 3600.0;
        let load = bps / 10.0;
        (blue_delta as f64 / 1000.0) * (avg_conv / 10.0) * (orphan_rate * 100.0) * (1.0 + load)
    }

    pub fn should_stitch(&self, blue_delta: u64, bps: f64, now: i64) -> bool {
        let sus = self.sus(blue_delta, bps);
        let min_delta = (self.config.base_min_delta as f64 / (1.0 + sus)) as u64;
        let rate_limit = (self.config.base_rate_limit as f64 / (1.0 + sus)) as i64;
        blue_delta >= min_delta && now - self.last_stitch >= rate_limit.max(self.config.min_rate_limit as i64)
    }

    pub fn reward(&self, sus: f64) -> u64 {
        let base = self.config.base_reward_sompi;
        let bonus = (base as f64 * sus.min(5.0)) as u64;
        (base + bonus).min(self.config.max_reward_sompi)
    }

    pub fn orphan_rate(&self) -> f64 {
        if self.recent_orphans.is_empty() { 0.0 } else {
            self.recent_orphans.iter().filter(|&&o| o).count() as f64 / self.recent_orphans.len() as f64
        }
    }

    pub fn record_stitch(&mut self) {
        self.last_stitch = Utc::now().timestamp();
    }
}
