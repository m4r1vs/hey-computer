//! Socket location and the client half of the protocol.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;

use crate::lines::{self, Line};
use crate::protocol::{self, Reply, Request};

/// The daemon answers every request at once (an ask with `accepted`); only
/// the answer itself may take long.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REPLY_LINE: usize = 64 * 1024 * 1024;

/// `--socket`, then `$HEY_COMPUTER_SOCKET`, then
/// `$XDG_RUNTIME_DIR/hey-computer.sock`, with a `/tmp` fallback for sessions
/// without a runtime directory.
pub fn socket_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("HEY_COMPUTER_SOCKET").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(runtime_dir()?.join("hey-computer.sock"))
}

/// `$XDG_RUNTIME_DIR`, or else `/tmp/hey-computer-UID`.
///
/// The fallback has a name anyone can predict, so it is used only if it is
/// a real directory of this user that nobody else can enter: another user
/// who created it first could otherwise put a socket of their own there and
/// receive the requests.
pub fn runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    // SAFETY: geteuid cannot fail and touches no memory.
    let uid = unsafe { libc::geteuid() };
    let dir = PathBuf::from(format!("/tmp/hey-computer-{uid}"));
    private_fallback(&dir, uid)?;
    Ok(dir)
}

fn private_fallback(dir: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }
    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("checking {}", dir.display()))?;
    if !meta.file_type().is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        bail!(
            "{} is not a private directory of this user; set XDG_RUNTIME_DIR, or remove it",
            dir.display()
        );
    }
    Ok(())
}

/// The daemon is not there to ask.
#[derive(Debug)]
pub struct NotRunning(pub String);

impl std::fmt::Display for NotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotRunning {}

/// An open exchange with the daemon.
pub struct Client {
    reader: BufReader<OwnedReadHalf>,
    // Kept open: closing it would tell the daemon a waiting client left.
    _writer: tokio::net::unix::OwnedWriteHalf,
}

impl Client {
    /// Connect and send one request.
    pub async fn send(path: &Path, request: &Request) -> Result<Self> {
        let stream = match UnixStream::connect(path).await {
            Ok(stream) => stream,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(NotRunning(format!(
                    "the hey-computer daemon is not running (nothing listens on {}); start it with `systemctl --user start hey-computer`",
                    path.display()
                ))
                .into());
            }
            Err(e) => return Err(e).with_context(|| format!("connecting to {}", path.display())),
        };
        let (read, mut write) = stream.into_split();
        let mut line = protocol::encode(request);
        line.push('\n');
        tokio::time::timeout(CLIENT_TIMEOUT, write.write_all(line.as_bytes()))
            .await
            .context("the daemon does not read")?
            .context("sending the request")?;
        Ok(Self {
            reader: BufReader::new(read),
            _writer: write,
        })
    }

    /// The next reply; `None` when `timeout` is given and runs out.
    pub async fn next(&mut self, timeout: Option<Duration>) -> Result<Reply> {
        let read = lines::read(&mut self.reader, MAX_REPLY_LINE);
        let line = match timeout {
            Some(timeout) => tokio::time::timeout(timeout, read)
                .await
                .with_context(|| format!("the daemon did not answer within {timeout:?}"))?,
            None => read.await,
        }
        .context("reading the daemon's reply")?;
        match line {
            Line::Text(line) => serde_json::from_str(&line)
                .with_context(|| format!("unreadable reply from the daemon: {line}")),
            Line::Eof => bail!("the daemon closed the connection without answering"),
            Line::TooLong(n) => bail!("the daemon sent a {n}-byte line"),
            Line::NotUtf8 => bail!("the daemon sent a line that is not UTF-8"),
        }
    }
}

/// Send a request that has exactly one quick reply.
pub async fn request(path: &Path, request: &Request) -> Result<Reply> {
    let mut client = Client::send(path, request).await?;
    client.next(Some(CLIENT_TIMEOUT)).await
}

/// Ask, and wait for the accepted reply.
pub async fn ask(path: &Path, text: &str, wait: bool, new: bool) -> Result<(u64, Client)> {
    ask_request(
        path,
        &Request::Ask {
            text: text.to_string(),
            wait,
            new,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: Vec::new(),
        },
    )
    .await
}

/// Send an `Ask` request and wait for the `Accepted` reply.
pub async fn ask_request(path: &Path, request: &Request) -> Result<(u64, Client)> {
    let mut client = Client::send(path, request).await?;
    match client.next(Some(CLIENT_TIMEOUT)).await? {
        Reply::Accepted { id } => Ok((id, client)),
        Reply::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_runtime_directory_must_be_private_and_ours() {
        use std::os::unix::fs::PermissionsExt;
        let uid = unsafe { libc::geteuid() };
        let dir = std::env::temp_dir().join(format!("hc-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        private_fallback(&dir, uid).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // Again, now that it exists.
        private_fallback(&dir, uid).unwrap();

        // Someone else's directory, or one others can enter, is refused.
        assert!(private_fallback(&dir, uid + 1).is_err());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_fallback(&dir, uid).is_err());
        std::fs::remove_dir_all(&dir).unwrap();

        // A link to a private directory is not one.
        let target = dir.with_extension("target");
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        assert!(private_fallback(&dir, uid).is_err());
        std::fs::remove_file(&dir).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
    }
}
