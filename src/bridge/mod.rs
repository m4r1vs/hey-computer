//! Strictly read-only WhatsApp & Signal messenger bridge (`hey-computer-bridge`).
//!
//! Supervises `baileys-reader` (WhatsApp) and `signal-cli` (Signal over a
//! private `0700` UNIX socket), maintains an in-memory rolling per-chat context
//! cache (`me` + `them`), and exposes a read-only JSON-lines socket at
//! `$XDG_RUNTIME_DIR/hey-computer-bridge.sock` (`0600`).

pub mod config;
pub mod context;
pub mod protocol;
pub mod signal;
pub mod whatsapp;

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::bridge::config::BridgeConfig;
use crate::bridge::protocol::{BridgeEvent, BridgeRequest, MAX_BRIDGE_LINE};
use crate::lines::{self, Line};

const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Run the bridge daemon until SIGTERM or SIGINT.
pub async fn run_daemon(config: BridgeConfig, socket: &Path) -> Result<()> {
    serve(config, socket, termination()).await
}

/// Serve the bridge daemon on `socket` until `shutdown` resolves.
pub async fn serve(
    config: BridgeConfig,
    socket: &Path,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    context::serve(config, socket, shutdown).await
}

/// Send a single read-only request (`Status` or `GetContext`) and read one reply.
pub async fn query(socket: &Path, request: &BridgeRequest) -> Result<BridgeEvent> {
    let stream = UnixStream::connect(socket).await.with_context(|| {
        format!(
            "cannot connect to hey-computer-bridge at {} (is the service running?)",
            socket.display()
        )
    })?;
    let (read, mut write) = stream.into_split();
    let mut line = protocol::encode(request);
    line.push('\n');
    tokio::time::timeout(CLIENT_TIMEOUT, write.write_all(line.as_bytes()))
        .await
        .context("bridge did not accept request in time")??;
    write.flush().await?;

    let mut reader = BufReader::new(read);
    let reply = tokio::time::timeout(CLIENT_TIMEOUT, lines::read(&mut reader, MAX_BRIDGE_LINE))
        .await
        .context("bridge did not answer in time")??;
    match reply {
        Line::Text(raw) => serde_json::from_str(&raw).context("invalid JSON from bridge"),
        Line::Eof => bail!("bridge closed connection without answering"),
        Line::TooLong(n) => bail!("bridge returned oversized {n}-byte line"),
        Line::NotUtf8 => bail!("bridge returned non-UTF-8 line"),
    }
}

/// Live streaming subscriber client for `hey-computer-bridge.sock`.
pub struct Subscriber {
    reader: BufReader<OwnedReadHalf>,
    _writer: OwnedWriteHalf,
}

impl Subscriber {
    /// Connect, send `{"type":"subscribe"}`, and return the initial `Ready` event.
    pub async fn connect(socket: &Path) -> Result<(BridgeEvent, Self)> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to bridge at {}", socket.display()))?;
        let (read, mut write) = stream.into_split();
        let mut line = protocol::encode(&BridgeRequest::Subscribe);
        line.push('\n');
        tokio::time::timeout(CLIENT_TIMEOUT, write.write_all(line.as_bytes()))
            .await
            .context("sending subscribe")??;
        write.flush().await?;

        let mut sub = Self {
            reader: BufReader::new(read),
            _writer: write,
        };
        let ready = sub.next(Some(CLIENT_TIMEOUT)).await?;
        Ok((ready, sub))
    }

    /// Read the next `BridgeEvent` from the stream.
    pub async fn next(&mut self, timeout: Option<Duration>) -> Result<BridgeEvent> {
        let fut = lines::read(&mut self.reader, MAX_BRIDGE_LINE);
        let line = match timeout {
            Some(dur) => tokio::time::timeout(dur, fut)
                .await
                .with_context(|| format!("timed out waiting for bridge event after {dur:?}"))??,
            None => fut.await?,
        };
        match line {
            Line::Text(raw) => serde_json::from_str(&raw).context("invalid bridge event JSON"),
            Line::Eof => bail!("bridge closed subscriber stream"),
            Line::TooLong(n) => bail!("bridge sent oversized {n}-byte event"),
            Line::NotUtf8 => bail!("bridge sent non-UTF-8 event"),
        }
    }
}

async fn termination() {
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(sig) => sig,
        Err(e) => {
            tracing::error!("cannot watch for SIGTERM: {e}");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        res = tokio::signal::ctrl_c() => match res {
            Ok(()) => tracing::info!("received SIGINT"),
            Err(e) => {
                tracing::error!("cannot watch for SIGINT: {e}");
                std::future::pending::<()>().await;
            }
        }
    }
}
