//! Tiny GET-only HTTP server: `/` serves the dashboard, `/api` serves the current snapshot as JSON.
//! Intentionally dependency-free (raw tokio) — it only ever answers two routes on a trusted LAN.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use crate::engine::Snapshot;

const DASHBOARD: &str = include_str!("dashboard.html");

pub async fn serve(port: u16, state: Arc<RwLock<Snapshot>>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    log::info!("dashboard live → http://localhost:{port}/  (bind 0.0.0.0)");
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                log::warn!("accept error: {e}");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let n = match sock.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");

            let (ctype, body) = if path.starts_with("/api") {
                let snap = state.read().await;
                ("application/json", serde_json::to_string(&*snap).unwrap_or_else(|_| "{}".into()))
            } else {
                ("text/html; charset=utf-8", DASHBOARD.to_string())
            };

            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}
