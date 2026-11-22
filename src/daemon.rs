//! The long-lived half: the socket, and the conductor behind it.

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

use crate::conductor::{self, Handle, Sink};
use crate::config::Config;
use crate::lines::{self, Line};
use crate::present::{Desktop, Presenter};
use crate::protocol::{self, FailReason, MAX_REQUEST_LINE, Reply, Request};
use crate::{environment, ipc};

/// A client gets this long to send its request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the daemon with the desktop presenter until SIGTERM or SIGINT.
pub async fn run(config: Config, socket: &Path) -> Result<()> {
    let answers = ipc::runtime_dir().join("hey-computer");
    let presenter: Arc<dyn Presenter> = Desktop::spawn(&config.present, answers);
    serve(config, socket, presenter, termination()).await
}

/// Serve on `socket` until `shutdown` completes.
pub async fn serve(
    config: Config,
    socket: &Path,
    presenter: Arc<dyn Presenter>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    // Once here, so that a skill directory that does not exist stops the
    // daemon at start instead of failing every agent start after it. The
    // conductor prepares it again before each start, to pick up changes.
    let paths = environment::prepare(&config).context("preparing the agent's directory")?;
    tracing::info!("agent directory {}", paths.agent_dir.display());
    let listener = bind(socket).await?;
    tracing::info!("listening on {}", socket.display());

    let (handle, conductor) = conductor::spawn(Arc::new(config), presenter);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let handle = handle.clone();
                    tokio::spawn(async move {
                        if let Err(e) = connection(&handle, stream).await {
                            tracing::debug!("client connection: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            () = &mut shutdown => break,
        }
    }

    tracing::info!("shutting down");
    handle.shutdown();
    let _ = conductor.await;
    if let Err(e) = std::fs::remove_file(socket) {
        tracing::warn!("cannot remove {}: {e}", socket.display());
    }
    Ok(())
}

async fn termination() {
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(signal) => signal,
        Err(e) => {
            tracing::error!("cannot watch for SIGTERM: {e}");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("got SIGTERM"),
        result = tokio::signal::ctrl_c() => match result {
            Ok(()) => tracing::info!("got SIGINT"),
            Err(e) => {
                tracing::error!("cannot watch for SIGINT: {e}");
                std::future::pending::<()>().await;
            }
        },
    }
}

/// Take over the socket, refusing to displace a daemon that is running.
async fn bind(path: &Path) -> Result<UnixListener> {
    if UnixStream::connect(path).await.is_ok() {
        bail!(
            "another hey-computer daemon is already listening on {}",
            path.display()
        );
    }
    // Nothing answered, so any socket here is stale.
    match std::fs::remove_file(path) {
        Ok(()) => tracing::debug!("removed the stale socket at {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("removing the stale socket {}", path.display()));
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {} to 0600", path.display()))?;
    Ok(listener)
}

/// One request in, one or more replies out.
async fn connection(handle: &Handle, stream: UnixStream) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    let line = tokio::time::timeout(REQUEST_TIMEOUT, lines::read(&mut reader, MAX_REQUEST_LINE))
        .await
        .context("the client sent no request")??;
    let request = match line {
        Line::Eof => return Ok(()),
        Line::Text(line) => {
            serde_json::from_str::<Request>(&line).map_err(|e| format!("not a request: {e}"))
        }
        Line::TooLong(n) => Err(format!(
            "the request is {n} bytes, more than the {MAX_REQUEST_LINE} allowed"
        )),
        Line::NotUtf8 => Err("the request is not UTF-8".to_string()),
    };
    let request = match request {
        Ok(request) => request,
        Err(message) => {
            tracing::warn!("refused a malformed request");
            return send(&mut write, &Reply::Error { message }).await;
        }
    };

    match request {
        Request::Status => send(&mut write, &Reply::Status(handle.status())).await,
        Request::Cancel => {
            let id = handle.cancel(None, FailReason::Cancelled);
            send(&mut write, &Reply::Cancelled { id }).await
        }
        Request::Ask { text, .. } if text.trim().is_empty() => {
            send(
                &mut write,
                &Reply::Error {
                    message: "the request is empty".to_string(),
                },
            )
            .await
        }
        Request::Ask {
            text,
            wait: false,
            new,
        } => {
            let id = handle.ask(text, new, Sink::Present);
            send(&mut write, &Reply::Accepted { id }).await
        }
        Request::Ask {
            text,
            wait: true,
            new,
        } => {
            let (tx, rx) = oneshot::channel();
            let id = handle.ask(text, new, Sink::Client(tx));
            send(&mut write, &Reply::Accepted { id }).await?;
            // The client waits for the answer; if it hangs up first, the
            // request is cancelled, since nobody would see the answer.
            let reply = tokio::select! {
                reply = rx => reply,
                () = hung_up(&mut reader) => {
                    tracing::debug!("the client of request {id} left");
                    handle.cancel(Some(id), FailReason::Cancelled);
                    return Ok(());
                }
            };
            match reply {
                Ok(reply) => send(&mut write, &reply).await,
                Err(_) => {
                    send(
                        &mut write,
                        &Reply::Failed {
                            id,
                            reason: FailReason::AgentDown,
                            message: "the daemon is shutting down".to_string(),
                        },
                    )
                    .await
                }
            }
        }
    }
}

/// Completes when the client closes its side (or sends more, which it
/// should not).
async fn hung_up(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) {
    let mut scratch = [0u8; 256];
    loop {
        match reader.read(&mut scratch).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, reply: &Reply) -> Result<()> {
    let mut line = protocol::encode(reply);
    line.push('\n');
    write.write_all(line.as_bytes()).await.context("replying")?;
    write.flush().await.context("replying")?;
    Ok(())
}
