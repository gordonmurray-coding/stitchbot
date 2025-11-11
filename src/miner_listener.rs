mod config;
mod p2p_stitch;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cfg = config::Config::from_file("miner_config.toml")?;
    let adaptor = p2p_stitch::setup_p2p(&cfg).await?;
    log::info!("StitchListener running — waiting for requests...");
    adaptor.run().await?;
    Ok(())
}
