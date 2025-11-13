use keyring::Entry;
use zeroize::Zeroize;
use kaspa_wallet_core::{wallet::Wallet, storage::InMemoryStorage};
use anyhow::Result;

const SERVICE: &str = "stitchbot";
const USER: &str = "kaspa-stitcher";

pub async fn load_or_create_wallet(rpc_url: &str) -> Result<Wallet<InMemoryStorage>> {
    let entry = Entry::new(SERVICE, USER)?;
    let storage = std::sync::Arc::new(tokio::sync::Mutex::new(InMemoryStorage::new()));

    let seed = match entry.get_password() {
        Ok(pass) => hex::decode(pass)?,
        Err(keyring::Error::NoEntry) => {
            eprintln!("Creating new wallet for StitchBot...");
            let seed = kaspa_wallet_core::prelude::Secret::new(rand::random::<[u8; 32]>());
            let hex_seed = hex::encode(seed.as_ref());
            entry.set_password(&hex_seed)?;
            println!("Wallet secured in system keyring.");
            seed.to_vec()
        }
        Err(e) => return Err(e.into()),
    };

    let mut secret = kaspa_wallet_core::prelude::Secret::from(seed);
    let wallet = Wallet::new(storage.clone(), secret.clone())?;
    secret.zeroize();

    let rpc = kaspa_rpc_core::client::RpcClient::new(rpc_url)?;
    wallet.sync(&rpc).await?;
    Ok(wallet)
}
