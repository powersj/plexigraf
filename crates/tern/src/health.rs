//! Component status, the `health_check` extension, and the telemetry
//! endpoint. Both endpoints are tiny HTTP/1.1 servers on loopback by default.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::bindings::types::Status;

#[derive(Default)]
pub struct Health {
    components: Mutex<BTreeMap<String, (Status, Option<String>)>>,
}

impl Health {
    pub fn set(&self, id: &str, status: Status, detail: Option<String>) {
        self.components.lock().unwrap().insert(id.to_string(), (status, detail));
    }

    fn status_name(s: Status) -> &'static str {
        match s {
            Status::Starting => "starting",
            Status::Ok => "ok",
            Status::RecoverableError => "recoverable_error",
            Status::PermanentError => "permanent_error",
            Status::Stopping => "stopping",
        }
    }

    /// Healthy unless a component has failed for good.
    pub fn report(&self) -> (bool, String) {
        let map = self.components.lock().unwrap();
        let healthy = !map.values().any(|(s, _)| *s == Status::PermanentError);
        let components: serde_json::Map<String, serde_json::Value> = map
            .iter()
            .map(|(id, (s, d))| {
                (
                    id.clone(),
                    serde_json::json!({ "status": Self::status_name(*s), "detail": d }),
                )
            })
            .collect();
        let body = serde_json::json!({ "status": if healthy { "ok" } else { "unhealthy" }, "components": components });
        (healthy, body.to_string())
    }
}

/// Parse an endpoint and refuse anything but loopback, since Tern 0.1 has no
/// TLS or auth for its own endpoints (threat S11).
pub fn loopback_addr(endpoint: &str) -> Result<SocketAddr> {
    let addr = endpoint.replace("localhost", "127.0.0.1");
    let addr: SocketAddr = addr
        .parse()
        .with_context(|| format!("invalid endpoint {endpoint:?}; use host:port"))?;
    if !addr.ip().is_loopback() {
        bail!(
            "endpoint {endpoint} is not loopback; non-loopback endpoints need TLS and auth, which are not implemented yet"
        );
    }
    Ok(addr)
}

/// Serve `respond(path)` until the listener is dropped with the task.
pub async fn serve(
    addr: SocketAddr,
    name: &'static str,
    respond: impl Fn(&str) -> (u16, &'static str, String) + Send + Sync + 'static,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {name} on {addr}"))?;
    tracing::info!(%addr, "{name} listening");
    let respond = std::sync::Arc::new(respond);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            let respond = respond.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let Ok(Ok(n)) = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut buf)).await
                else {
                    return;
                };
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let (code, ctype, body) = respond(path);
                let reason = match code {
                    200 => "OK",
                    404 => "Not Found",
                    _ => "Service Unavailable",
                };
                let resp = format!(
                    "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    Ok(())
}
