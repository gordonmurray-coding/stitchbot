use anyhow::Result;
use keyring::Entry;
use kaspa_wallet_core::wallet::Wallet;
use kaspa_wallet_core::prelude::Mnemonic;  // ← Correct path for Mnemonic
use kaspa_wrpc_client::KaspaRpcClient;      // ← Correct struct name (KaspaRpcClient)
use tokio::sync::Mutex;

const SERVICE: &str = "stitchbot";
const USER: &str = "kaspa-stitcher";

pub async fn load_or_create_wallet(rpc_url: &str) -> Result<Wallet> {
    let entry = Entry::new(SERVICE, USER)?;
    let mnemonic = match entry.get_password() {
        Ok(pass) => pass,  // Assume stored as mnemonic string
        Err(keyring::Error::NoEntry) => {
            eprintln!("Creating new wallet for StitchBot...");
            let mnemonic = Mnemonic::generate(12)?;  // ← Using prelude Mnemonic::generate(12)
            entry.set_password(&mnemonic.to_string())?;
            println!("Wallet secured in system keyring.");
            mnemonic.to_string()
        }
        Err(e) => return Err(e.into()),
    };

    // Create wallet from mnemonic (default in-memory storage)
    let rpc = KaspaRpcClient::new(rpc_url)?;  // ← Correct constructor
    let wallet = Wallet::from_mnemonic(&mnemonic, &rpc).await?;  // ← Simple constructor, auto-syncs

    Ok(wallet)
}
