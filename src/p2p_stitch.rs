// src/p2p_stitch.rs
use anyhow::Result;
use async_trait::async_trait;
use bincode::{deserialize, serialize};
use blake2s_simd::Params;
use chrono::Utc;
use secp256k1::{ecdsa::Signature, PublicKey, SecretKey};
use std::{future::Future, pin::Pin, sync::Arc};
use secp256k1::Message;
use serde::{Deserialize, Serialize};

use kaspa_p2p_flows::{flow_context::FlowContext, flow_trait::Flow};
use kaspa_p2p_lib::{
    make_message,
    common::{ProtocolError, DEFAULT_TIMEOUT},
    Adaptor, IncomingRoute, Router,
    // Hub is re-exported on Adaptor; use Adaptor.hub() when needed.
};

const STITCH_MSG_ID: u8 = 0xF0;

#[derive(Clone, Serialize, Deserialize)]
pub struct StitchRequest {
    pub weak_block: String,
    pub tip_hashes: Vec<String>,
    pub reward_sompi: u64,
    pub expiry: u64,
    pub signature: Vec<u8>,
    pub pubkey: Vec<u8>,
}

impl StitchRequest {
    pub fn new(weak: &str, tips: &[String], reward: u64, sk: &SecretKey) -> Self {
        let pubkey = PublicKey::from_secret_key(sk);
        let mut req = Self {
            weak_block: weak.to_string(),
            tip_hashes: tips.to_vec(),
            reward_sompi: reward,
            expiry: (Utc::now().timestamp() as u64) + 30,
            signature: vec![],
            pubkey: pubkey.serialize().to_vec(),
        };
        req.signature = req.sign(sk);
        req
    }

    fn hash(&self) -> Message {
        let data =
            serialize(&(&self.weak_block, &self.tip_hashes, self.reward_sompi, self.expiry))
                .unwrap();
        let hash = Params::new().hash_length(32).hash(&data);
        Message::from_slice(hash.as_bytes()).unwrap()
    }

    fn sign(&self, sk: &SecretKey) -> Vec<u8> {
        let msg = self.hash();
        sk.sign_ecdsa(&msg).serialize_compact().to_vec()
    }

    pub fn verify(&self) -> bool {
        let Ok(pubkey) = PublicKey::from_slice(&self.pubkey) else { return false; };
        let Ok(sig) = Signature::from_compact(&self.signature) else { return false; };
        let msg = self.hash();
        pubkey.verify(&msg, &sig).is_ok()
    }
}

/// Broadcast a stitch request to peers via the Adaptor's hub.
/// Uses `make_message!` helper (preferred with the current kaspa p2p lib).
pub async fn broadcast_stitch(
    adaptor: &Adaptor,
    weak: &str,
    tips: &[String],
    reward: u64,
    sk: &SecretKey,
) -> Result<()> {
    let req = StitchRequest::new(weak, tips, reward, sk);
    let payload = serialize(&req)?;
    // `make_message!` creates a pb::KaspadMessage and wraps payload correctly
    let msg = make_message!(STITCH_MSG_ID, payload);
    adaptor.hub().broadcast(msg).await?;
    Ok(())
}

/// Create an adaptor and connect to configured bootstrap peers.
pub async fn setup_p2p(cfg: &super::config::Config) -> Result<Adaptor> {
    let initializer = Arc::new(StitchInitializer {});
    let mut adaptor = Adaptor::new(initializer);
    for peer in &cfg.p2p_bootstrap_peers {
        adaptor.connect(peer).await?;
    }
    Ok(adaptor)
}

/// Connection initializer: new method signature required by current kaspa_p2p_lib.
/// Implement the async initialize_connection method and register our flow.
struct StitchInitializer {}

#[async_trait]
impl kaspa_p2p_lib::ConnectionInitializer for StitchInitializer {
    async fn initialize_connection(&self, new_router: Arc<Router>) -> Result<(), ProtocolError> {
        // Register a flow instance for stitch messages
        // Router::register_flow expects a factory/flow instance per current API
        new_router.register_flow(STITCH_MSG_ID, Arc::new(StitchFlow::new(new_router.clone())))?;
        Ok(())
    }
}

/// The Flow implementation that listens for stitch messages on a router.
/// This mirrors patterns used in kaspa-p2p-flows (txrelay, ibd etc).
pub struct StitchFlow {
    ctx: FlowContext,
    router: Arc<Router>,
    // incoming route for all messages matching this flow
    incoming: IncomingRoute,
}

impl StitchFlow {
    pub fn new(router: Arc<Router>) -> Self {
        // create a FlowContext from the router (API in flows expects FlowContext being created
        // by router/register_flow machinery in some implementations; this constructor is a convenience)
        // If router.register_flow already constructs the FlowContext for you, adapt accordingly.
        Self {
            ctx: FlowContext::default(),
            router: router.clone(),
            incoming: IncomingRoute::default(),
        }
    }
}

impl Flow for StitchFlow {
    fn router(&self) -> Option<Arc<Router>> {
        Some(self.router.clone())
    }

    fn start(&mut self, mut ctx: FlowContext) {
        // spawn background async task for this flow
        tokio::spawn(async move {
            // use incoming route from ctx if available; if FlowContext has a specific API to take the IncomingRoute,
            // adapt accordingly. Below is a generic loop that listens for incoming messages
            loop {
                match ctx.recv().await {
                    Some(msg) => {
                        if msg.id == STITCH_MSG_ID {
                            if let Ok(req) = deserialize::<StitchRequest>(&msg.payload) {
                                if req.verify() && req.expiry > Utc::now().timestamp() as u64 {
                                    log::info!(
                                        "Valid stitch request: {} tips, reward: {}",
                                        req.tip_hashes.len(),
                                        req.reward_sompi
                                    );
                                    // TODO: trigger local miner signaling, forward to wallet to pay out,
                                    // or write to a local DB / metrics. This is where "real stuff" goes.
                                }
                            }
                        }
                    }
                    None => {
                        // If channel closed, exit loop
                        break;
                    }
                }
            }
        });
    }
}
