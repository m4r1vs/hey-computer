//! File and image attachment loading, validation, and ACP block encoding.
//!
//! Clients send absolute file paths over the Unix socket (`protocol::Attachment`)
//! so `MAX_REQUEST_LINE` stays bounded. The daemon reads and validates each file
//! inside `spawn_blocking` before replying `Accepted`, eliminating temp-file
//! races and ensuring bad paths fail fast without superseding a running turn.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::normalize_lexical;
use crate::protocol::{Attachment, AttachmentKind};

/// Maximum attachments accepted on a single request.
pub const MAX_ATTACHMENTS: usize = 8;
/// Maximum raw bytes for a single UTF-8 text file (`--file`).
pub const MAX_FILE_BYTES: usize = 512 * 1024;
/// Maximum raw bytes for a single raster image (`--image`, ~13.3 MiB base64).
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// Maximum combined raw bytes across all attachments (~21.3 MiB base64, safely
/// below `acp::MAX_AGENT_LINE`).
pub const MAX_TOTAL_RAW_BYTES: usize = 16 * 1024 * 1024;

/// Metadata persisted in the history store (never raw contents or base64).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentMeta {
    pub kind: AttachmentKind,
    pub name: String,
    pub mime_type: String,
    pub bytes: usize,
}

/// A validated attachment ready to encode into ACP `session/prompt` blocks.
#[derive(Debug, Clone)]
pub enum LoadedAttachment {
    Image {
        name: String,
        mime_type: &'static str,
        bytes: usize,
        base64: String,
    },
    File {
        name: String,
        uri: String,
        mime_type: &'static str,
        bytes: usize,
        text: String,
    },
}

impl LoadedAttachment {
    pub fn meta(&self) -> AttachmentMeta {
        match self {
            Self::Image {
                name,
                mime_type,
                bytes,
                ..
            } => AttachmentMeta {
                kind: AttachmentKind::Image,
                name: name.clone(),
                mime_type: (*mime_type).to_string(),
                bytes: *bytes,
            },
            Self::File {
                name,
                mime_type,
                bytes,
                ..
            } => AttachmentMeta {
                kind: AttachmentKind::File,
                name: name.clone(),
                mime_type: (*mime_type).to_string(),
                bytes: *bytes,
            },
        }
    }

    pub fn to_acp_block(&self) -> Value {
        match self {
            Self::Image {
                mime_type, base64, ..
            } => json!({
                "type": "image",
                "mimeType": mime_type,
                "data": base64,
            }),
            Self::File {
                uri,
                mime_type,
                text,
                ..
            } => json!({
                "type": "resource",
                "resource": {
                    "uri": uri,
                    "mimeType": mime_type,
                    "text": text,
                },
            }),
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image { .. })
    }
}

/// Resolve a client-supplied path against the caller's working directory and
/// ensure it can be represented as UTF-8 JSON.
pub fn resolve_client_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("attachment path is empty");
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("reading the current working directory")?
            .join(path)
    };
    let normalized = normalize_lexical(&abs);
    if normalized.to_str().is_none() {
        bail!("attachment path is not valid UTF-8");
    }
    Ok(normalized)
}

/// Load and validate every attachment in `specs`.
pub fn load_all(specs: &[Attachment]) -> Result<Vec<LoadedAttachment>> {
    if specs.len() > MAX_ATTACHMENTS {
        bail!(
            "too many attachments: {} given, at most {MAX_ATTACHMENTS} allowed",
            specs.len()
        );
    }
    let mut loaded = Vec::with_capacity(specs.len());
    let mut total_bytes: usize = 0;

    for spec in specs {
        let item = load_one(spec, &mut total_bytes)?;
        loaded.push(item);
    }
    Ok(loaded)
}

fn load_one(spec: &Attachment, total_bytes: &mut usize) -> Result<LoadedAttachment> {
    if spec.path.as_os_str().is_empty() || !spec.path.is_absolute() {
        bail!("attachment path must be absolute: {}", spec.path.display());
    }
    // O_NONBLOCK avoids hanging in open(2) on a FIFO or device before fstat.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(&spec.path)
        .with_context(|| format!("opening attachment {}", spec.path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("checking attachment {}", spec.path.display()))?;
    if !meta.is_file() {
        bail!("attachment {} is not a regular file", spec.path.display());
    }

    let limit = match spec.kind {
        AttachmentKind::File => MAX_FILE_BYTES,
        AttachmentKind::Image => MAX_IMAGE_BYTES,
    };
    let mut bytes = Vec::new();
    file.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading attachment {}", spec.path.display()))?;
    if bytes.is_empty() {
        bail!("attachment {} is empty", spec.path.display());
    }
    if bytes.len() > limit {
        bail!(
            "attachment {} exceeds the {} KiB limit",
            spec.path.display(),
            limit / 1024
        );
    }
    *total_bytes = total_bytes.saturating_add(bytes.len());
    if *total_bytes > MAX_TOTAL_RAW_BYTES {
        bail!(
            "attachments exceed the {} MiB combined size limit",
            MAX_TOTAL_RAW_BYTES / (1024 * 1024)
        );
    }

    let name = spec
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment")
        .to_string();

    match spec.kind {
        AttachmentKind::Image => {
            let Some(mime_type) = detect_image_mime(&bytes) else {
                bail!(
                    "attachment {} is not a supported image format (supported: PNG, JPEG, WebP, GIF)",
                    spec.path.display()
                );
            };
            let len = bytes.len();
            let base64 = base64_encode(&bytes);
            Ok(LoadedAttachment::Image {
                name,
                mime_type,
                bytes: len,
                base64,
            })
        }
        AttachmentKind::File => {
            if bytes.contains(&0) {
                bail!(
                    "attachment {} appears to be binary, not UTF-8 text (use --image for images)",
                    spec.path.display()
                );
            }
            let len = bytes.len();
            let text = String::from_utf8(bytes).map_err(|_| {
                anyhow::anyhow!(
                    "attachment {} is not valid UTF-8 text (use --image for images)",
                    spec.path.display()
                )
            })?;
            let mime_type = text_mime_for_path(&spec.path);
            let uri = file_uri(&spec.path);
            Ok(LoadedAttachment::File {
                name,
                uri,
                mime_type,
                bytes: len,
                text,
            })
        }
    }
}

/// Sniff raster image format strictly from magic bytes (ignoring extension).
pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

fn text_mime_for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("md" | "markdown") => "text/markdown",
        Some("json") => "application/json",
        Some("toml") => "application/toml",
        Some("yaml" | "yml") => "text/yaml",
        Some("xml") => "application/xml",
        Some("csv") => "text/csv",
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("js" | "mjs") => "text/javascript",
        Some("ts" | "tsx") => "text/typescript",
        Some("rs") => "text/x-rust",
        Some("py") => "text/x-python",
        Some("sh" | "bash" | "zsh") => "text/x-shellscript",
        Some("nix") => "text/plain",
        _ => "text/plain",
    }
}

/// RFC 3986 percent-encoded `file://` URI for an absolute Unix path.
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Standard RFC 4648 base64 encoding with `=` padding and no line breaks.
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let (chunks, remainder) = input.as_chunks::<3>();
    for chunk in chunks {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        out.push(TABLE[(n & 0x3F) as usize] as char);
    }
    match remainder {
        [a] => {
            let n = (*a as u32) << 16;
            out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
            out.push('=');
            out.push('=');
        }
        [a, b] => {
            let n = ((*a as u32) << 16) | ((*b as u32) << 8);
            out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// Synthesize a concise default instruction when the user attaches files/images
/// without typing prompt text.
pub fn default_prompt(loaded: &[LoadedAttachment]) -> &'static str {
    let images = loaded.iter().filter(|a| a.is_image()).count();
    let files = loaded.len() - images;
    match (images, files) {
        (1, 0) => "Describe or explain the attached image.",
        (_, 0) => "Describe or explain the attached images.",
        (0, 1) => "Review or explain the attached file.",
        (0, _) => "Review or explain the attached files.",
        _ => "Inspect and explain the attached files and images.",
    }
}

/// Progress notification body preview when prompt text is empty.
pub fn progress_label(text: &str, loaded: &[LoadedAttachment]) -> String {
    if !text.trim().is_empty() {
        return text.to_string();
    }
    let images = loaded.iter().filter(|a| a.is_image()).count();
    let files = loaded.len() - images;
    match (images, files) {
        (1, 0) => "Inspecting 1 image…".to_string(),
        (n, 0) => format!("Inspecting {n} images…"),
        (0, 1) => "Inspecting 1 file…".to_string(),
        (0, n) => format!("Inspecting {n} files…"),
        (i, f) => format!("Inspecting {i} image(s) and {f} file(s)…"),
    }
}

/// Build the ACP `prompt` array: text first, then text resources, then images.
pub fn build_prompt_blocks(text: &str, loaded: &[LoadedAttachment]) -> Vec<Value> {
    let mut blocks = Vec::with_capacity(1 + loaded.len());
    blocks.push(json!({"type": "text", "text": text}));
    for item in loaded.iter().filter(|a| !a.is_image()) {
        blocks.push(item.to_acp_block());
    }
    for item in loaded.iter().filter(|a| a.is_image()) {
        blocks.push(item.to_acp_block());
    }
    blocks
}

/// Safe `info`-level summary (kinds, MIME types, and byte counts — never names or contents).
pub fn summary_for_log(loaded: &[LoadedAttachment]) -> String {
    loaded
        .iter()
        .map(|item| match item {
            LoadedAttachment::Image {
                mime_type, bytes, ..
            } => format!("image ({mime_type}, {bytes} B)"),
            LoadedAttachment::File {
                mime_type, bytes, ..
            } => format!("file ({mime_type}, {bytes} B)"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hc-attach-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (raw, expected) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
            (b"\x89PNG\r\n\x1a\n", "iVBORw0KGgo="),
        ] {
            assert_eq!(base64_encode(raw), expected);
        }
    }

    #[test]
    fn detects_supported_raster_image_magic_bytes() {
        assert_eq!(
            detect_image_mime(b"\x89PNG\r\n\x1a\nxxxx"),
            Some("image/png")
        );
        assert_eq!(
            detect_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(detect_image_mime(b"GIF89a\x01\x00"), Some("image/gif"));
        assert_eq!(
            detect_image_mime(b"RIFF\x10\x00\x00\x00WEBPxxxx"),
            Some("image/webp")
        );
        assert_eq!(detect_image_mime(b"<svg></svg>"), None);
        assert_eq!(detect_image_mime(b"plain text"), None);
    }

    #[test]
    fn file_uri_percent_encodes_spaces_and_special_chars() {
        assert_eq!(
            file_uri(Path::new("/tmp/my note #1.md")),
            "file:///tmp/my%20note%20%231.md"
        );
    }

    #[test]
    fn loads_valid_text_and_image_and_rejects_bad_inputs() {
        let dir = scratch("validate");
        let txt = dir.join("sample.md");
        std::fs::write(&txt, "# hello\n").unwrap();
        let png = dir.join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00").unwrap();
        let empty = dir.join("empty.txt");
        std::fs::write(&empty, b"").unwrap();
        let binary = dir.join("bin.dat");
        std::fs::write(&binary, b"\x00\x01\x02\x03").unwrap();
        let svg = dir.join("bad.svg");
        std::fs::write(&svg, b"<svg/>").unwrap();

        let loaded = load_all(&[
            Attachment {
                kind: AttachmentKind::File,
                path: txt.clone(),
            },
            Attachment {
                kind: AttachmentKind::Image,
                path: png.clone(),
            },
        ])
        .unwrap();
        assert_eq!(loaded.len(), 2);
        let blocks = build_prompt_blocks("explain", &loaded);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "resource");
        assert_eq!(blocks[1]["resource"]["mimeType"], "text/markdown");
        assert_eq!(blocks[1]["resource"]["text"], "# hello\n");
        assert_eq!(blocks[2]["type"], "image");
        assert_eq!(blocks[2]["mimeType"], "image/png");

        // Relative path, directory, empty, binary text file, and SVG image are all rejected.
        for bad in [
            Attachment {
                kind: AttachmentKind::File,
                path: PathBuf::from("relative.txt"),
            },
            Attachment {
                kind: AttachmentKind::File,
                path: dir.clone(),
            },
            Attachment {
                kind: AttachmentKind::File,
                path: empty,
            },
            Attachment {
                kind: AttachmentKind::File,
                path: binary,
            },
            Attachment {
                kind: AttachmentKind::Image,
                path: svg,
            },
        ] {
            assert!(load_all(&[bad]).is_err());
        }

        // Too many attachments rejected.
        let many: Vec<Attachment> = (0..=MAX_ATTACHMENTS)
            .map(|_| Attachment {
                kind: AttachmentKind::File,
                path: txt.clone(),
            })
            .collect();
        assert!(load_all(&many).is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
