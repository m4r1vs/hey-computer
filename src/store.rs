//! Persistent state stored under `$XDG_STATE_HOME/hey-computer/`.
//!
//! Currently stores the most recent successful answer (`last.json`, mode 0600)
//! for `hey-computer last [--copy | --open]`. Written atomically so a reader
//! never sees a partial file, and never touched by `environment::prune_history`.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::attachments::AttachmentMeta;

pub const LAST_FILE: &str = "last.json";
pub const SCHEMA_VERSION: u32 = 1;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn default_version() -> u32 {
    SCHEMA_VERSION
}

/// The most recent successful answer persisted by the daemon.
///
/// Intentionally omits `deny_unknown_fields` so future Phase 3 fields remain
/// backwards-compatible when read by older binaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastAnswer {
    #[serde(default = "default_version")]
    pub version: u32,
    pub id: u64,
    pub timestamp: u64,
    /// The raw request text that produced this answer.
    pub request: String,
    /// The raw, trimmed answer text (not HTML-disarmed).
    pub text: String,
    /// Commands refused by the deny list during this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied: Vec<String>,
    /// Metadata for files or images attached to the request (never raw bytes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentMeta>,
}

/// Current Unix epoch time in seconds.
pub fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Atomically persist `answer` to `<state_dir>/last.json` with mode `0600`.
pub fn save_last(state_dir: &Path, answer: &LastAnswer) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", state_dir.display()))?;

    let payload = serde_json::to_vec(answer).context("encoding the last answer")?;
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = state_dir.join(format!("last.{}.{n}.tmp", std::process::id()));
    let write_result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {}", tmp.display()))?;
        file.write_all(&payload)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.flush()
            .with_context(|| format!("flushing {}", tmp.display()))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    let target = state_dir.join(LAST_FILE);
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", target.display()));
    }
    Ok(())
}

/// Load `<state_dir>/last.json`, or `Ok(None)` when no answer has been saved yet.
pub fn load_last(state_dir: &Path) -> Result<Option<LastAnswer>> {
    let path = state_dir.join(LAST_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let answer: LastAnswer =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hc-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_and_load_round_trip_with_private_permissions() {
        let dir = scratch("roundtrip");
        assert_eq!(load_last(&dir).unwrap(), None);

        let record = LastAnswer {
            version: SCHEMA_VERSION,
            id: 7,
            timestamp: 1_700_000_000,
            request: "check <script>".to_string(),
            text: "Result: <script>alert(1)</script>".to_string(),
            denied: vec!["sudo id".to_string()],
            attachments: vec![],
        };
        save_last(&dir, &record).unwrap();

        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let file_mode = std::fs::metadata(dir.join(LAST_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![LAST_FILE]);

        assert_eq!(load_last(&dir).unwrap(), Some(record));

        let updated = LastAnswer {
            version: SCHEMA_VERSION,
            id: 8,
            timestamp: 1_700_000_010,
            request: "second".to_string(),
            text: "Updated.".to_string(),
            denied: vec![],
            attachments: vec![],
        };
        save_last(&dir, &updated).unwrap();
        assert_eq!(load_last(&dir).unwrap(), Some(updated));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
