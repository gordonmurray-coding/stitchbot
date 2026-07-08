use serde::Deserialize;

/// Config for the DAG-health monitor. `#[serde(default)]` fields let an old/minimal
/// config.toml still load with sensible defaults.
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    /// Kaspa node gRPC endpoint, host:port (scheme added in code).
    pub rpc_url: String,
    /// Port the dashboard + JSON API are served on.
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// How often to poll the node, milliseconds.
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// Number of recent blocks kept in the rolling DAG for metrics (bump for high BPS).
    #[serde(default = "default_window")]
    pub dag_window: usize,
    /// Max nodes sent to the dashboard canvas (kept small so the browser stays smooth at 100 BPS).
    #[serde(default = "default_viz_cap")]
    pub viz_cap: usize,
    /// Tip-width above this flags a fracture in the UI.
    #[serde(default = "default_fracture_tips")]
    pub fracture_tip_width: usize,
    /// Blue-score spread across tips above this also flags a fracture.
    #[serde(default = "default_min_delta")]
    pub base_min_delta: u64,
    /// File the JSONL metrics dataset is appended to (the measurement PoC output).
    #[serde(default = "default_log_path")]
    pub log_path: String,
    /// Merge-depth ceiling in blue-score rounds (= target_bps × 3600). Blocks not merged within this
    /// are permanently orphaned. 36000 at 10 BPS mainnet; 360000 at 100 BPS.
    #[serde(default = "default_merge_depth")]
    pub merge_depth: u64,
    /// Blue-score depth used by the confirmation-time proxy (rounds below the DAG frontier to count a
    /// block "confirmed"). Not the protocol's finality — a tunable security proxy for the correlation.
    #[serde(default = "default_conf_depth")]
    pub conf_depth: u64,
}

fn default_merge_depth() -> u64 { 36_000 }
fn default_conf_depth() -> u64 { 120 }

fn default_log_path() -> String { "stitchbot_metrics.jsonl".to_string() }

fn default_http_port() -> u16 { 8899 }
fn default_poll_ms() -> u64 { 1000 }
fn default_window() -> usize { 1500 }
fn default_viz_cap() -> usize { 600 }
fn default_fracture_tips() -> usize { 8 }
fn default_min_delta() -> u64 { 500 }

impl Config {
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut cfg: Config = toml::from_str(&content)?;
        // Env override so you can flip nodes without editing config.toml:
        //   KASPA_RPC=192.168.4.33:16110 ./stitchbot
        if let Ok(rpc) = std::env::var("KASPA_RPC") {
            let rpc = rpc.trim();
            if !rpc.is_empty() {
                cfg.rpc_url = rpc.to_string();
            }
        }
        Ok(cfg)
    }
}
