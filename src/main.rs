//! StitchBot — a real-time Kaspa DAG-health monitor.
//!
//! Polls a node over gRPC (get_block_dag_info + get_blocks), maintains a rolling DAG, computes
//! tip-width / blue-delta / BPS / a stress index, and serves a live dashboard + JSON API.

mod config;
mod engine;
mod http;

use std::sync::Arc;
use anyhow::{anyhow, Result};
use tokio::sync::RwLock;

use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_rpc_core::RpcHash;

use engine::{BlockNode, Engine, Snapshot};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cfg = config::Config::from_file("config.toml")?;

    // Shared snapshot the HTTP server reads and the poll loop writes.
    let state = Arc::new(RwLock::new(Snapshot::default()));
    {
        let (st, port) = (state.clone(), cfg.http_port);
        tokio::spawn(async move {
            if let Err(e) = http::serve(port, st).await {
                log::error!("http server: {e}");
            }
        });
    }

    let url = format!("grpc://{}", cfg.rpc_url);
    log::info!("connecting to node at {url} ...");
    let client = GrpcClient::connect(url.clone()).await.map_err(|e| anyhow!("connect {url}: {e}"))?;
    log::info!("connected — polling every {} ms; dashboard on :{}", cfg.poll_ms, cfg.http_port);

    let mut eng = Engine::new(cfg.dag_window);
    let mut low_hash: Option<RpcHash> = None;
    let mut last_count: Option<(u64, f64)> = None; // (block_count, unix_secs)

    loop {
        match poll_once(&client, &mut eng, &mut low_hash, &mut last_count, &cfg).await {
            Ok(snap) => {
                log_metrics(&cfg.log_path, &snap);
                *state.write().await = snap;
            }
            Err(e) => {
                log::warn!("poll error: {e}");
                state.write().await.connected = false;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(cfg.poll_ms)).await;
    }
}

/// Append one compact JSONL record per poll — the measurement dataset (scalars only, no viz nodes).
fn log_metrics(path: &str, s: &Snapshot) {
    use std::io::Write;
    if !s.connected {
        return;
    }
    let rec = serde_json::json!({
        "t": s.updated_ms, "net": s.network, "tips": s.tip_width, "peak_tips": s.peak_tip_width,
        "bps": s.bps, "blue_delta": s.blue_delta, "max_parents": s.max_parents, "avg_parents": s.avg_parents,
        "tip_excess": s.tip_excess, "red_rate": s.red_rate, "reds": s.reds_window, "blues": s.blues_window,
        "fracture": s.fracture, "fracture_secs": s.fracture_secs, "daa": s.virtual_daa, "blocks": s.block_count,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{rec}");
    }
}

async fn poll_once(
    client: &GrpcClient,
    eng: &mut Engine,
    low_hash: &mut Option<RpcHash>,
    last_count: &mut Option<(u64, f64)>,
    cfg: &config::Config,
) -> Result<Snapshot> {
    let info = client.get_block_dag_info().await?;

    // Pull recent blocks: from the previous sink, or from the current sink on the first pass.
    let low = low_hash.or(Some(info.sink));
    let resp = client.get_blocks(low, true, false).await?;
    for b in &resp.blocks {
        let parents = b
            .header
            .parents_by_level
            .first()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|h| h.to_string())
            .collect();
        let vd = b.verbose_data.as_ref();
        eng.ingest(BlockNode {
            hash: b.header.hash.to_string(),
            blue_score: b.header.blue_score,
            daa: b.header.daa_score,
            timestamp: b.header.timestamp,
            parents,
            is_chain: vd.map(|v| v.is_chain_block).unwrap_or(false),
            blues: vd.map(|v| v.merge_set_blues_hashes.len() as u32).unwrap_or(0),
            reds: vd.map(|v| v.merge_set_reds_hashes.len() as u32).unwrap_or(0),
        });
    }
    *low_hash = Some(info.sink);

    // BPS from the node's total block-count delta over wall time.
    let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
    let bps = match *last_count {
        Some((pc, pt)) => (info.block_count.saturating_sub(pc)) as f64 / (now - pt).max(0.001),
        None => 0.0,
    };
    *last_count = Some((info.block_count, now));

    let tips: Vec<String> = info.tip_hashes.iter().map(|h| h.to_string()).collect();
    Ok(eng.snapshot(
        info.network.to_string(),
        info.sink.to_string(),
        info.virtual_daa_score,
        info.block_count,
        info.header_count,
        info.difficulty,
        bps,
        &tips,
        cfg.fracture_tip_width,
        cfg.base_min_delta,
    ))
}
