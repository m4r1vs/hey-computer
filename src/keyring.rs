//! Native Freedesktop Secret Service (`org.freedesktop.secrets`) keyring
//! readiness watcher for `agent.wait_for_keyring`.
//!
//! Talks to the session D-Bus daemon directly over [`tokio::net::UnixStream`]
//! with zero external crates: performs SASL `EXTERNAL` authentication, sends
//! `org.freedesktop.DBus.Hello`, subscribes via `AddMatch` *without* a `path=`
//! filter on `PropertiesChanged` (because `gnome-keyring-daemon` emits signals
//! on canonical `/org/freedesktop/secrets/collection/login` rather than the
//! `/org/freedesktop/secrets/aliases/default` alias), and queries the read-only
//! `Locked` property (which never triggers `gcr-prompter`).

use std::fmt::Write as _;
use std::future::Future;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Notify;

/// Timeout for individual D-Bus connect/handshake/method-call steps.
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);
/// Safety fallback re-poll interval while the keyring remains locked.
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(15);
/// Delay before reconnecting after a broken D-Bus stream.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);
/// Delay before retrying after a transient D-Bus error.
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Upper bound on an incoming D-Bus message header/body to prevent OOM on corrupt frames.
const MAX_DBUS_SECTION_BYTES: usize = 1024 * 1024;
/// Upper bound on a SASL response line.
const MAX_SASL_LINE_BYTES: usize = 4096;

const MSG_METHOD_CALL: u8 = 1;
const MSG_METHOD_RETURN: u8 = 2;
const MSG_ERROR: u8 = 3;
const MSG_SIGNAL: u8 = 4;

const HDR_PATH: u8 = 1;
const HDR_INTERFACE: u8 = 2;
const HDR_MEMBER: u8 = 3;
const HDR_ERROR_NAME: u8 = 4;
const HDR_REPLY_SERIAL: u8 = 5;
const HDR_DESTINATION: u8 = 6;
const HDR_SIGNATURE: u8 = 8;

/// Match rules installed before the initial `Properties.Get` so no unlock
/// signal can be missed in the window between probe and subscribe.
///
/// Crucially, `PropertiesChanged` omits `path=` because `gnome-keyring-daemon`
/// emits it on `/org/freedesktop/secrets/collection/login`, not on the virtual
/// alias `/org/freedesktop/secrets/aliases/default`.
const MATCH_RULES: &[&str] = &[
    "type='signal',sender='org.freedesktop.secrets',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',arg0='org.freedesktop.Secret.Collection'",
    "type='signal',sender='org.freedesktop.secrets',interface='org.freedesktop.Secret.Service',member='CollectionChanged'",
    "type='signal',sender='org.freedesktop.secrets',interface='org.freedesktop.Secret.Service',member='CollectionCreated'",
    "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.freedesktop.secrets'",
];

/// D-Bus error names indicating no usable Secret Service / default collection
/// exists on this session bus (do not block agent start).
const UNAVAILABLE_ERRORS: &[&str] = &[
    "org.freedesktop.DBus.Error.ServiceUnknown",
    "org.freedesktop.DBus.Error.NameHasNoOwner",
    "org.freedesktop.DBus.Error.Spawn.ServiceNotFound",
    "org.freedesktop.DBus.Error.Spawn.ExecFailed",
    "org.freedesktop.DBus.Error.UnknownObject",
    "org.freedesktop.DBus.Error.UnknownInterface",
    "org.freedesktop.DBus.Error.InvalidArgs",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyringStatus {
    /// Default collection exists and `Locked == false`.
    Unlocked,
    /// Default collection exists and `Locked == true`.
    Locked,
    /// No session bus, `org.freedesktop.secrets` is not installed/activatable,
    /// or no `default` collection exists yet — do not block agent start.
    Unavailable,
}

pub trait KeyringWatcher: Send + Sync + 'static {
    /// One-shot probe of `org.freedesktop.secrets` `/org/freedesktop/secrets/aliases/default`.
    fn check_locked(&self) -> Pin<Box<dyn Future<Output = KeyringStatus> + Send + '_>>;

    /// Resolves once the keyring transitions to `Unlocked` or `Unavailable`.
    fn wait_unlocked(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Production [`KeyringWatcher`] backed by a direct pure-Rust D-Bus wire client
/// over `$DBUS_SESSION_BUS_ADDRESS` (or `$XDG_RUNTIME_DIR/bus`).
#[derive(Debug, Default, Clone, Copy)]
pub struct DbusKeyringWatcher;

impl KeyringWatcher for DbusKeyringWatcher {
    fn check_locked(&self) -> Pin<Box<dyn Future<Output = KeyringStatus> + Send + '_>> {
        Box::pin(async {
            let Some(bus_path) = session_bus_path() else {
                tracing::debug!("keyring probe: no session D-Bus socket; treating as unavailable");
                return KeyringStatus::Unavailable;
            };
            let stream = match connect_bus(&bus_path).await {
                Ok(s) => s,
                Err(ConnectErr::Unavailable(reason)) => {
                    tracing::debug!("keyring probe: session bus unavailable ({reason})");
                    return KeyringStatus::Unavailable;
                }
                Err(ConnectErr::Transient(reason)) => {
                    tracing::debug!("keyring probe: session bus connect failed ({reason})");
                    return KeyringStatus::Unavailable;
                }
            };
            match probe_on_stream(stream).await {
                Ok(status) => status,
                Err(reason) => {
                    tracing::debug!("keyring probe: transient D-Bus error ({reason})");
                    KeyringStatus::Unavailable
                }
            }
        })
    }

    fn wait_unlocked(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {
            loop {
                let Some(bus_path) = session_bus_path() else {
                    tracing::debug!("keyring wait: no session D-Bus socket; proceeding");
                    return;
                };
                let stream = match connect_bus(&bus_path).await {
                    Ok(s) => s,
                    Err(ConnectErr::Unavailable(reason)) => {
                        tracing::debug!("keyring wait: session bus unavailable ({reason})");
                        return;
                    }
                    Err(ConnectErr::Transient(reason)) => {
                        tracing::debug!("keyring wait: transient connect error ({reason})");
                        tokio::time::sleep(TRANSIENT_RETRY_DELAY).await;
                        continue;
                    }
                };
                match wait_unlocked_on_stream(stream, FALLBACK_POLL_INTERVAL).await {
                    Ok(()) => return,
                    Err(WaitErr::Disconnected(reason)) => {
                        tracing::debug!("keyring wait: bus disconnected ({reason}); reconnecting");
                        tokio::time::sleep(RECONNECT_DELAY).await;
                    }
                    Err(WaitErr::Transient(reason)) => {
                        tracing::debug!("keyring wait: transient error ({reason}); retrying");
                        tokio::time::sleep(TRANSIENT_RETRY_DELAY).await;
                    }
                }
            }
        })
    }
}

/// Deterministic mock [`KeyringWatcher`] for integration tests.
pub struct MockKeyringWatcher {
    locked: AtomicBool,
    unavailable: AtomicBool,
    probes: AtomicUsize,
    notify: Notify,
}

impl MockKeyringWatcher {
    pub fn locked() -> Arc<Self> {
        Arc::new(Self {
            locked: AtomicBool::new(true),
            unavailable: AtomicBool::new(false),
            probes: AtomicUsize::new(0),
            notify: Notify::new(),
        })
    }

    pub fn unlocked() -> Arc<Self> {
        Arc::new(Self {
            locked: AtomicBool::new(false),
            unavailable: AtomicBool::new(false),
            probes: AtomicUsize::new(0),
            notify: Notify::new(),
        })
    }

    pub fn unavailable() -> Arc<Self> {
        Arc::new(Self {
            locked: AtomicBool::new(false),
            unavailable: AtomicBool::new(true),
            probes: AtomicUsize::new(0),
            notify: Notify::new(),
        })
    }

    pub fn unlock(&self) {
        self.set_locked(false);
    }

    pub fn set_locked(&self, locked: bool) {
        self.unavailable.store(false, Ordering::SeqCst);
        self.locked.store(locked, Ordering::SeqCst);
        if !locked {
            self.notify.notify_waiters();
        }
    }

    pub fn probe_count(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
    }
}

impl KeyringWatcher for MockKeyringWatcher {
    fn check_locked(&self) -> Pin<Box<dyn Future<Output = KeyringStatus> + Send + '_>> {
        Box::pin(async move {
            self.probes.fetch_add(1, Ordering::SeqCst);
            if self.unavailable.load(Ordering::SeqCst) {
                KeyringStatus::Unavailable
            } else if self.locked.load(Ordering::SeqCst) {
                KeyringStatus::Locked
            } else {
                KeyringStatus::Unlocked
            }
        })
    }

    fn wait_unlocked(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            loop {
                let notified = self.notify.notified();
                if self.unavailable.load(Ordering::SeqCst) || !self.locked.load(Ordering::SeqCst) {
                    return;
                }
                notified.await;
            }
        })
    }
}

/// Resolve the session D-Bus Unix socket path from `$DBUS_SESSION_BUS_ADDRESS`
/// or, when unset/empty, from `$XDG_RUNTIME_DIR/bus`.
fn session_bus_path() -> Option<PathBuf> {
    if let Ok(addr) = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        && !addr.is_empty()
    {
        return parse_dbus_unix_path(&addr);
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?;
    let candidate = PathBuf::from(dir).join("bus");
    let meta = std::fs::metadata(&candidate).ok()?;
    meta.file_type().is_socket().then_some(candidate)
}

/// Extract the first `unix:path=...` entry from a D-Bus address string.
fn parse_dbus_unix_path(address: &str) -> Option<PathBuf> {
    for entry in address.split(';') {
        let Some(rest) = entry.strip_prefix("unix:") else {
            continue;
        };
        for pair in rest.split(',') {
            if let Some(raw_path) = pair.strip_prefix("path=")
                && !raw_path.is_empty()
            {
                let decoded = percent_decode(raw_path)?;
                if !decoded.is_empty() {
                    return Some(PathBuf::from(decoded));
                }
            }
        }
    }
    None
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_val(bytes[i + 1])?;
            let lo = hex_val(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

enum ConnectErr {
    Unavailable(String),
    Transient(String),
}

async fn connect_bus(path: &std::path::Path) -> Result<UnixStream, ConnectErr> {
    match tokio::time::timeout(DBUS_TIMEOUT, UnixStream::connect(path)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            Err(ConnectErr::Unavailable(e.to_string()))
        }
        Ok(Err(e)) => Err(ConnectErr::Transient(e.to_string())),
        Err(_) => Err(ConnectErr::Transient("connect timeout".to_string())),
    }
}

#[derive(Debug)]
enum WaitErr {
    Disconnected(String),
    Transient(String),
}

struct DbusConn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    next_serial: u32,
}

#[derive(Debug)]
struct DbusMessage {
    msg_type: u8,
    big_endian: bool,
    reply_serial: Option<u32>,
    error_name: Option<String>,
    body: Vec<u8>,
}

impl DbusConn {
    async fn handshake(stream: UnixStream) -> Result<Self, String> {
        let (read, write) = stream.into_split();
        let mut conn = Self {
            reader: BufReader::new(read),
            writer: write,
            next_serial: 1,
        };
        tokio::time::timeout(DBUS_TIMEOUT, conn.sasl_external())
            .await
            .map_err(|_| "SASL handshake timed out".to_string())??;
        let hello_serial = conn.alloc_serial();
        let hello_msg = encode_method_call(
            hello_serial,
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
            "org.freedesktop.DBus",
            None,
            &[],
        );
        tokio::time::timeout(
            DBUS_TIMEOUT,
            conn.send_and_wait_reply(hello_serial, &hello_msg),
        )
        .await
        .map_err(|_| "D-Bus Hello timed out".to_string())??;
        Ok(conn)
    }

    fn alloc_serial(&mut self) -> u32 {
        let s = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        s
    }

    async fn sasl_external(&mut self) -> Result<(), String> {
        // SAFETY: geteuid cannot fail and touches no memory.
        let uid = unsafe { libc::geteuid() };
        let auth = sasl_external_command(uid);
        self.writer
            .write_all(auth.as_bytes())
            .await
            .map_err(|e| format!("SASL write: {e}"))?;
        let line = read_sasl_line(&mut self.reader).await?;
        if !line.starts_with("OK ") {
            return Err(format!("SASL rejected: {}", line.trim()));
        }
        self.writer
            .write_all(b"BEGIN\r\n")
            .await
            .map_err(|e| format!("SASL BEGIN write: {e}"))?;
        Ok(())
    }

    async fn add_match(&mut self, rule: &str) -> Result<(), String> {
        let serial = self.alloc_serial();
        let mut body = Vec::with_capacity(rule.len() + 8);
        push_dbus_string(&mut body, rule);
        let msg = encode_method_call(
            serial,
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            "org.freedesktop.DBus",
            Some("s"),
            &body,
        );
        let reply = tokio::time::timeout(DBUS_TIMEOUT, self.send_and_wait_reply(serial, &msg))
            .await
            .map_err(|_| "AddMatch timed out".to_string())??;
        if reply.msg_type == MSG_ERROR {
            return Err(format!(
                "AddMatch error: {}",
                reply.error_name.as_deref().unwrap_or("unknown")
            ));
        }
        Ok(())
    }

    async fn get_default_locked(&mut self) -> Result<KeyringStatus, String> {
        let serial = self.alloc_serial();
        let mut body = Vec::with_capacity(64);
        push_dbus_string(&mut body, "org.freedesktop.Secret.Collection");
        pad_to(&mut body, 4);
        push_dbus_string(&mut body, "Locked");
        let msg = encode_method_call(
            serial,
            "/org/freedesktop/secrets/aliases/default",
            "org.freedesktop.DBus.Properties",
            "Get",
            "org.freedesktop.secrets",
            Some("ss"),
            &body,
        );
        let reply = tokio::time::timeout(DBUS_TIMEOUT, self.send_and_wait_reply(serial, &msg))
            .await
            .map_err(|_| "Properties.Get timed out".to_string())??;
        match reply.msg_type {
            MSG_METHOD_RETURN => parse_bool_variant(&reply.body, reply.big_endian),
            MSG_ERROR => {
                let err_name = reply.error_name.as_deref().unwrap_or("");
                if UNAVAILABLE_ERRORS.contains(&err_name) {
                    Ok(KeyringStatus::Unavailable)
                } else {
                    Err(format!("D-Bus error: {err_name}"))
                }
            }
            other => Err(format!("unexpected D-Bus reply type {other}")),
        }
    }

    async fn send_and_wait_reply(
        &mut self,
        serial: u32,
        frame: &[u8],
    ) -> Result<DbusMessage, String> {
        self.writer
            .write_all(frame)
            .await
            .map_err(|e| format!("D-Bus write: {e}"))?;
        loop {
            let msg = self.read_message().await?;
            if msg.reply_serial == Some(serial)
                && matches!(msg.msg_type, MSG_METHOD_RETURN | MSG_ERROR)
            {
                return Ok(msg);
            }
            // Ignore interleaved signals (e.g. NameAcquired or early unlock signals).
        }
    }

    async fn read_message(&mut self) -> Result<DbusMessage, String> {
        let mut fixed = [0u8; 16];
        self.reader
            .read_exact(&mut fixed)
            .await
            .map_err(|e| format!("D-Bus read header: {e}"))?;
        let big_endian = match fixed[0] {
            b'l' => false,
            b'B' => true,
            other => return Err(format!("invalid D-Bus endianness byte 0x{other:02x}")),
        };
        let msg_type = fixed[1];
        if fixed[3] != 1 {
            return Err(format!("unsupported D-Bus protocol version {}", fixed[3]));
        }
        let body_len = read_u32(&fixed[4..8], big_endian) as usize;
        let fields_len = read_u32(&fixed[12..16], big_endian) as usize;
        if fields_len > MAX_DBUS_SECTION_BYTES || body_len > MAX_DBUS_SECTION_BYTES {
            return Err(format!(
                "oversized D-Bus message (fields={fields_len}, body={body_len})"
            ));
        }
        let header_total = align_up(16 + fields_len, 8);
        let pad_len = header_total - (16 + fields_len);
        let mut rest = vec![0u8; fields_len + pad_len + body_len];
        self.reader
            .read_exact(&mut rest)
            .await
            .map_err(|e| format!("D-Bus read body: {e}"))?;

        let (reply_serial, error_name) = parse_header_fields(&rest[..fields_len], big_endian)?;
        let body = rest[fields_len + pad_len..].to_vec();
        Ok(DbusMessage {
            msg_type,
            big_endian,
            reply_serial,
            error_name,
            body,
        })
    }
}

async fn probe_on_stream(stream: UnixStream) -> Result<KeyringStatus, String> {
    let mut conn = DbusConn::handshake(stream).await?;
    conn.get_default_locked().await
}

async fn wait_unlocked_on_stream(
    stream: UnixStream,
    fallback_poll: Duration,
) -> Result<(), WaitErr> {
    let mut conn = DbusConn::handshake(stream)
        .await
        .map_err(WaitErr::Transient)?;
    for rule in MATCH_RULES {
        conn.add_match(rule).await.map_err(WaitErr::Transient)?;
    }
    loop {
        match conn.get_default_locked().await {
            Ok(KeyringStatus::Unlocked | KeyringStatus::Unavailable) => return Ok(()),
            Ok(KeyringStatus::Locked) => {}
            Err(e) => return Err(WaitErr::Transient(e)),
        }
        let poll_sleep = tokio::time::sleep(fallback_poll);
        tokio::pin!(poll_sleep);
        loop {
            tokio::select! {
                res = conn.read_message() => {
                    match res {
                        Ok(msg) if msg.msg_type == MSG_SIGNAL => break,
                        Ok(_) => continue,
                        Err(e) => return Err(WaitErr::Disconnected(e)),
                    }
                }
                () = &mut poll_sleep => break,
            }
        }
    }
}

fn sasl_external_command(uid: u32) -> String {
    let decimal = uid.to_string();
    let mut hex = String::with_capacity(decimal.len() * 2);
    for b in decimal.bytes() {
        let _ = write!(hex, "{b:02x}");
    }
    format!("\0AUTH EXTERNAL {hex}\r\n")
}

async fn read_sasl_line(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<String, String> {
    let mut buf = Vec::new();
    let n = reader
        .read_until(b'\n', &mut buf)
        .await
        .map_err(|e| format!("SASL read: {e}"))?;
    if n == 0 {
        return Err("SASL EOF".to_string());
    }
    if buf.len() > MAX_SASL_LINE_BYTES {
        return Err("SASL line too long".to_string());
    }
    String::from_utf8(buf).map_err(|_| "non-UTF-8 SASL response".to_string())
}

fn align_up(offset: usize, align: usize) -> usize {
    (offset + align - 1) & !(align - 1)
}

fn pad_to(buf: &mut Vec<u8>, align: usize) {
    let target = align_up(buf.len(), align);
    buf.resize(target, 0);
}

fn push_dbus_string(buf: &mut Vec<u8>, s: &str) {
    pad_to(buf, 4);
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

fn push_header_field_str(buf: &mut Vec<u8>, code: u8, sig_char: u8, value: &str) {
    pad_to(buf, 8);
    buf.push(code);
    buf.push(1);
    buf.push(sig_char);
    buf.push(0);
    push_dbus_string(buf, value);
}

fn push_header_field_sig(buf: &mut Vec<u8>, code: u8, sig: &str) {
    pad_to(buf, 8);
    buf.push(code);
    buf.push(1);
    buf.push(b'g');
    buf.push(0);
    buf.push(sig.len() as u8);
    buf.extend_from_slice(sig.as_bytes());
    buf.push(0);
}

fn encode_method_call(
    serial: u32,
    path: &str,
    interface: &str,
    member: &str,
    destination: &str,
    signature: Option<&str>,
    body: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128 + body.len());
    buf.push(b'l');
    buf.push(MSG_METHOD_CALL);
    buf.push(0); // flags (auto-start allowed)
    buf.push(1); // protocol version
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&serial.to_le_bytes());
    // Placeholder for header-fields array byte length.
    buf.extend_from_slice(&0u32.to_le_bytes());

    push_header_field_str(&mut buf, HDR_PATH, b'o', path);
    push_header_field_str(&mut buf, HDR_INTERFACE, b's', interface);
    push_header_field_str(&mut buf, HDR_MEMBER, b's', member);
    push_header_field_str(&mut buf, HDR_DESTINATION, b's', destination);
    if let Some(sig) = signature {
        push_header_field_sig(&mut buf, HDR_SIGNATURE, sig);
    }

    let fields_len = (buf.len() - 16) as u32;
    buf[12..16].copy_from_slice(&fields_len.to_le_bytes());
    pad_to(&mut buf, 8);
    buf.extend_from_slice(body);
    buf
}

fn read_u32(slice: &[u8], big_endian: bool) -> u32 {
    let bytes: [u8; 4] = [slice[0], slice[1], slice[2], slice[3]];
    if big_endian {
        u32::from_be_bytes(bytes)
    } else {
        u32::from_le_bytes(bytes)
    }
}

fn parse_header_fields(
    fields: &[u8],
    big_endian: bool,
) -> Result<(Option<u32>, Option<String>), String> {
    let mut reply_serial = None;
    let mut error_name = None;
    let mut pos = 0usize;
    while pos < fields.len() {
        pos = align_up(pos, 8);
        if pos >= fields.len() {
            break;
        }
        let code = fields[pos];
        pos += 1;
        if pos >= fields.len() {
            return Err("truncated D-Bus header variant signature".to_string());
        }
        let sig_len = fields[pos] as usize;
        pos += 1;
        if pos + sig_len >= fields.len() {
            return Err("truncated D-Bus header signature bytes".to_string());
        }
        let sig = &fields[pos..pos + sig_len];
        pos += sig_len + 1; // NUL terminator
        match sig {
            b"s" | b"o" => {
                pos = align_up(pos, 4);
                if pos + 4 > fields.len() {
                    return Err("truncated D-Bus string header length".to_string());
                }
                let len = read_u32(&fields[pos..pos + 4], big_endian) as usize;
                pos += 4;
                if pos + len >= fields.len() {
                    return Err("truncated D-Bus string header value".to_string());
                }
                let val = std::str::from_utf8(&fields[pos..pos + len])
                    .map_err(|_| "non-UTF-8 D-Bus header string".to_string())?;
                if code == HDR_ERROR_NAME {
                    error_name = Some(val.to_string());
                }
                pos += len + 1;
            }
            b"g" => {
                if pos >= fields.len() {
                    return Err("truncated D-Bus signature header".to_string());
                }
                let len = fields[pos] as usize;
                pos += 1;
                if pos + len >= fields.len() {
                    return Err("truncated D-Bus signature value".to_string());
                }
                pos += len + 1;
            }
            b"u" | b"h" => {
                pos = align_up(pos, 4);
                if pos + 4 > fields.len() {
                    return Err("truncated D-Bus u32 header".to_string());
                }
                let val = read_u32(&fields[pos..pos + 4], big_endian);
                if code == HDR_REPLY_SERIAL && sig == b"u" {
                    reply_serial = Some(val);
                }
                pos += 4;
            }
            b"y" => {
                if pos >= fields.len() {
                    return Err("truncated D-Bus byte header".to_string());
                }
                pos += 1;
            }
            _ => break,
        }
    }
    Ok((reply_serial, error_name))
}

fn parse_bool_variant(body: &[u8], big_endian: bool) -> Result<KeyringStatus, String> {
    if body.len() < 3 {
        return Err("truncated Properties.Get variant body".to_string());
    }
    let sig_len = body[0] as usize;
    if 1 + sig_len >= body.len() {
        return Err("truncated Properties.Get variant signature".to_string());
    }
    let sig = &body[1..1 + sig_len];
    if body[1 + sig_len] != 0 || sig != b"b" {
        return Err(format!(
            "unexpected Properties.Get variant signature {:?}",
            String::from_utf8_lossy(sig)
        ));
    }
    let val_off = align_up(1 + sig_len + 1, 4);
    if val_off + 4 > body.len() {
        return Err("truncated Properties.Get boolean value".to_string());
    }
    match read_u32(&body[val_off..val_off + 4], big_endian) {
        0 => Ok(KeyringStatus::Unlocked),
        1 => Ok(KeyringStatus::Locked),
        other => Err(format!("invalid D-Bus boolean value {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ParsedClientCall {
        serial: u32,
        path: String,
        interface: String,
        member: String,
        destination: String,
        body: Vec<u8>,
    }

    async fn server_read_sasl(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> String {
        let mut buf = Vec::new();
        reader.read_until(b'\n', &mut buf).await.unwrap();
        String::from_utf8(buf).unwrap()
    }

    async fn server_accept_sasl(
        reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        writer: &mut tokio::net::unix::OwnedWriteHalf,
    ) -> String {
        let auth = server_read_sasl(reader).await;
        let expected = sasl_external_command(unsafe { libc::geteuid() });
        assert_eq!(auth, expected);
        writer
            .write_all(b"OK 0123456789abcdef0123456789abcdef\r\n")
            .await
            .unwrap();
        let begin = server_read_sasl(reader).await;
        assert_eq!(begin, "BEGIN\r\n");
        auth
    }

    async fn server_read_call(
        reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    ) -> ParsedClientCall {
        let mut fixed = [0u8; 16];
        reader.read_exact(&mut fixed).await.unwrap();
        assert_eq!(fixed[0], b'l');
        assert_eq!(fixed[1], MSG_METHOD_CALL);
        assert_eq!(fixed[3], 1);
        let body_len = read_u32(&fixed[4..8], false) as usize;
        let serial = read_u32(&fixed[8..12], false);
        let fields_len = read_u32(&fixed[12..16], false) as usize;
        let header_total = align_up(16 + fields_len, 8);
        let pad_len = header_total - (16 + fields_len);
        let mut rest = vec![0u8; fields_len + pad_len + body_len];
        reader.read_exact(&mut rest).await.unwrap();

        let fields = &rest[..fields_len];
        let mut pos = 0;
        let (mut path, mut interface, mut member, mut destination) =
            (String::new(), String::new(), String::new(), String::new());
        while pos < fields.len() {
            pos = align_up(pos, 8);
            if pos >= fields.len() {
                break;
            }
            let code = fields[pos];
            pos += 1;
            let sig_len = fields[pos] as usize;
            pos += 1;
            let sig = &fields[pos..pos + sig_len];
            pos += sig_len + 1;
            match sig {
                b"s" | b"o" => {
                    pos = align_up(pos, 4);
                    let len = read_u32(&fields[pos..pos + 4], false) as usize;
                    pos += 4;
                    let s = std::str::from_utf8(&fields[pos..pos + len])
                        .unwrap()
                        .to_string();
                    pos += len + 1;
                    match code {
                        HDR_PATH => path = s,
                        HDR_INTERFACE => interface = s,
                        HDR_MEMBER => member = s,
                        HDR_DESTINATION => destination = s,
                        _ => {}
                    }
                }
                b"g" => {
                    let len = fields[pos] as usize;
                    pos += 1 + len + 1;
                }
                _ => panic!("unexpected client header sig {sig:?}"),
            }
        }
        ParsedClientCall {
            serial,
            path,
            interface,
            member,
            destination,
            body: rest[fields_len + pad_len..].to_vec(),
        }
    }

    fn decode_first_string(body: &[u8]) -> (String, usize) {
        let len = read_u32(&body[0..4], false) as usize;
        let s = std::str::from_utf8(&body[4..4 + len]).unwrap().to_string();
        (s, 4 + len + 1)
    }

    fn encode_method_return(reply_serial: u32, signature: Option<&str>, body: &[u8]) -> Vec<u8> {
        let mut buf = vec![b'l', MSG_METHOD_RETURN, 0, 1];
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(&99u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        // REPLY_SERIAL
        pad_to(&mut buf, 8);
        buf.push(HDR_REPLY_SERIAL);
        buf.push(1);
        buf.push(b'u');
        buf.push(0);
        pad_to(&mut buf, 4);
        buf.extend_from_slice(&reply_serial.to_le_bytes());
        if let Some(sig) = signature {
            push_header_field_sig(&mut buf, HDR_SIGNATURE, sig);
        }
        let fields_len = (buf.len() - 16) as u32;
        buf[12..16].copy_from_slice(&fields_len.to_le_bytes());
        pad_to(&mut buf, 8);
        buf.extend_from_slice(body);
        buf
    }

    fn encode_error_reply(reply_serial: u32, error_name: &str) -> Vec<u8> {
        let mut buf = vec![b'l', MSG_ERROR, 0, 1];
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        push_header_field_str(&mut buf, HDR_ERROR_NAME, b's', error_name);
        pad_to(&mut buf, 8);
        buf.push(HDR_REPLY_SERIAL);
        buf.push(1);
        buf.push(b'u');
        buf.push(0);
        pad_to(&mut buf, 4);
        buf.extend_from_slice(&reply_serial.to_le_bytes());
        let fields_len = (buf.len() - 16) as u32;
        buf[12..16].copy_from_slice(&fields_len.to_le_bytes());
        pad_to(&mut buf, 8);
        buf
    }

    fn encode_signal(path: &str, interface: &str, member: &str) -> Vec<u8> {
        let mut buf = vec![b'l', MSG_SIGNAL, 0, 1];
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&200u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        push_header_field_str(&mut buf, HDR_PATH, b'o', path);
        push_header_field_str(&mut buf, HDR_INTERFACE, b's', interface);
        push_header_field_str(&mut buf, HDR_MEMBER, b's', member);
        let fields_len = (buf.len() - 16) as u32;
        buf[12..16].copy_from_slice(&fields_len.to_le_bytes());
        pad_to(&mut buf, 8);
        buf
    }

    fn encode_bool_variant_body(locked: bool) -> Vec<u8> {
        let mut body = vec![1, b'b', 0, 0];
        body.extend_from_slice(&(u32::from(locked)).to_le_bytes());
        body
    }

    #[test]
    fn parses_dbus_session_bus_addresses() {
        assert_eq!(
            parse_dbus_unix_path("unix:path=/run/user/1000/bus"),
            Some(PathBuf::from("/run/user/1000/bus"))
        );
        assert_eq!(
            parse_dbus_unix_path("unix:path=/tmp/my%20bus,guid=deadbeef"),
            Some(PathBuf::from("/tmp/my bus"))
        );
        assert_eq!(
            parse_dbus_unix_path("tcp:host=127.0.0.1;unix:path=/run/user/1000/bus,guid=1"),
            Some(PathBuf::from("/run/user/1000/bus"))
        );
        assert_eq!(parse_dbus_unix_path("disabled:"), None);
        assert_eq!(parse_dbus_unix_path("unix:abstract=/tmp/dbus-abc"), None);
    }

    #[tokio::test]
    async fn dbus_wait_unlocked_subscribes_before_probe_and_wakes_on_canonical_collection_signal() {
        let (client, server) = UnixStream::pair().unwrap();
        let server_task = tokio::spawn(async move {
            let (read, mut write) = server.into_split();
            let mut reader = BufReader::new(read);

            // 1. SASL EXTERNAL
            server_accept_sasl(&mut reader, &mut write).await;

            // 2. Hello (interleave NameAcquired signal before METHOD_RETURN)
            let hello = server_read_call(&mut reader).await;
            assert_eq!(hello.member, "Hello");
            assert_eq!(hello.destination, "org.freedesktop.DBus");
            write
                .write_all(&encode_signal(
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "NameAcquired",
                ))
                .await
                .unwrap();
            let mut unique_body = Vec::new();
            push_dbus_string(&mut unique_body, ":1.42");
            write
                .write_all(&encode_method_return(hello.serial, Some("s"), &unique_body))
                .await
                .unwrap();

            // 3. All 4 AddMatch rules arrive BEFORE Properties.Get, with no path= on PropertiesChanged.
            let mut rules = Vec::new();
            for _ in 0..4 {
                let call = server_read_call(&mut reader).await;
                assert_eq!(call.member, "AddMatch");
                let (rule, _) = decode_first_string(&call.body);
                rules.push(rule);
                write
                    .write_all(&encode_method_return(call.serial, None, &[]))
                    .await
                    .unwrap();
            }
            assert!(
                rules.iter().any(|r| {
                    r.contains("member='PropertiesChanged'")
                        && r.contains("arg0='org.freedesktop.Secret.Collection'")
                        && !r.contains("path=")
                }),
                "PropertiesChanged match rule must omit path=: {rules:?}"
            );
            assert!(
                rules
                    .iter()
                    .any(|r| r.contains("member='CollectionChanged'"))
            );
            assert!(
                rules
                    .iter()
                    .any(|r| r.contains("member='CollectionCreated'"))
            );
            assert!(
                rules
                    .iter()
                    .any(|r| r.contains("member='NameOwnerChanged'"))
            );

            // 4. First Properties.Get -> Locked = true
            let get1 = server_read_call(&mut reader).await;
            assert_eq!(get1.destination, "org.freedesktop.secrets");
            assert_eq!(get1.path, "/org/freedesktop/secrets/aliases/default");
            assert_eq!(get1.interface, "org.freedesktop.DBus.Properties");
            assert_eq!(get1.member, "Get");
            let (iface, next) = decode_first_string(&get1.body);
            let next = align_up(next, 4);
            let (prop, _) = decode_first_string(&get1.body[next..]);
            assert_eq!(iface, "org.freedesktop.Secret.Collection");
            assert_eq!(prop, "Locked");
            write
                .write_all(&encode_method_return(
                    get1.serial,
                    Some("v"),
                    &encode_bool_variant_body(true),
                ))
                .await
                .unwrap();

            // 5. Emit PropertiesChanged on canonical /org/freedesktop/secrets/collection/login
            tokio::time::sleep(Duration::from_millis(20)).await;
            write
                .write_all(&encode_signal(
                    "/org/freedesktop/secrets/collection/login",
                    "org.freedesktop.DBus.Properties",
                    "PropertiesChanged",
                ))
                .await
                .unwrap();

            // 6. Client immediately re-queries Properties.Get -> Locked = false
            let get2 = server_read_call(&mut reader).await;
            assert_eq!(get2.member, "Get");
            write
                .write_all(&encode_method_return(
                    get2.serial,
                    Some("v"),
                    &encode_bool_variant_body(false),
                ))
                .await
                .unwrap();
        });

        // Large fallback timer proves wake came from the signal, not the poll.
        tokio::time::timeout(
            Duration::from_secs(2),
            wait_unlocked_on_stream(client, Duration::from_secs(60)),
        )
        .await
        .expect("must wake promptly on unlock signal")
        .unwrap();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn dbus_service_unknown_and_unknown_object_degrade_to_unavailable() {
        for err_name in [
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "org.freedesktop.DBus.Error.UnknownObject",
        ] {
            let (client, server) = UnixStream::pair().unwrap();
            let err_owned = err_name.to_string();
            let server_task = tokio::spawn(async move {
                let (read, mut write) = server.into_split();
                let mut reader = BufReader::new(read);
                server_accept_sasl(&mut reader, &mut write).await;
                let hello = server_read_call(&mut reader).await;
                write
                    .write_all(&encode_method_return(hello.serial, None, &[]))
                    .await
                    .unwrap();
                let get = server_read_call(&mut reader).await;
                write
                    .write_all(&encode_error_reply(get.serial, &err_owned))
                    .await
                    .unwrap();
            });
            let status = probe_on_stream(client).await.unwrap();
            assert_eq!(status, KeyringStatus::Unavailable, "{err_name}");
            server_task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn mock_keyring_watcher_tracks_probes_and_unblocks_waiters() {
        let mock = MockKeyringWatcher::locked();
        assert_eq!(mock.check_locked().await, KeyringStatus::Locked);
        assert_eq!(mock.probe_count(), 1);

        let waiter = {
            let mock = Arc::clone(&mock);
            tokio::spawn(async move {
                mock.wait_unlocked().await;
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        mock.unlock();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mock.check_locked().await, KeyringStatus::Unlocked);
        assert_eq!(mock.probe_count(), 2);

        let unavail = MockKeyringWatcher::unavailable();
        assert_eq!(unavail.check_locked().await, KeyringStatus::Unavailable);
        unavail.wait_unlocked().await;

        let open = MockKeyringWatcher::unlocked();
        assert_eq!(open.check_locked().await, KeyringStatus::Unlocked);
        open.wait_unlocked().await;
    }
}
