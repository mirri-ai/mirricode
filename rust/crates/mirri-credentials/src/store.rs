//! Credential store — the durable plaintext JSON store for provider
//! credentials (`<mirri-home>/credentials/credentials.json`).
//!
//! Semantics (see docs/specs/provider-credential-architecture.md):
//! - one credential per provider, `set` overwrites in place (SDR-004);
//! - all reads go straight to disk — no in-process content cache
//!   (ADR-006; the store only owns the path);
//! - writes take the cross-process `RegistryLock`
//!   (`credentials.json.lock`) and re-read the snapshot inside the lock
//!   before modifying (ADR-007);
//! - `encoding.cipher` is a per-entry declaration, currently always
//!   `"plaintext"` (ADR-008); unknown ciphers are skipped on read, not
//!   guessed.

use std::path::{Path, PathBuf};

use anyhow::Context;
use mirri_os::{LocalSyncFs, SyncFs};
use mirri_session_index::RegistryLock;

pub const CREDENTIALS_DIR: &str = "credentials";
pub const CREDENTIALS_FILE: &str = "credentials.json";
const STORE_VERSION: u32 = 1;
const PLAINTEXT: &str = "plaintext";

/// The value a credential holds — a closed tagged union on `"type"`
/// (D-M4). `oauth` is a reserved serde slot only; its semantics land
/// with P-2.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialValue {
    Key {
        key: String,
    },
    External {
        method_id: String,
        #[serde(default)]
        metadata: std::collections::HashMap<String, serde_json::Value>,
    },
    Oauth {
        access: String,
        refresh: String,
        expires: u64,
        method_id: String,
    },
}

/// Per-entry encryption-form declaration (ADR-008). The only legal value
/// today is `cipher: "plaintext"` (zero behavior, an explicit field).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Encoding {
    pub cipher: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CredentialRecord {
    pub id: String,
    pub provider_id: String,
    pub value: CredentialValue,
    pub encoding: Encoding,
    pub time_created: u64,
    pub time_updated: u64,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    entries: Vec<CredentialRecord>,
}

/// The store is only a path holder (ADR-006: every read goes to disk), so a
/// clone shares nothing and observes the same file — the async dispatch
/// paths clone one out to move it across `spawn_blocking` boundaries.
#[derive(Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    pub fn open(home: &Path) -> anyhow::Result<Self> {
        let dir = home.join(CREDENTIALS_DIR);
        LocalSyncFs.mkdir_private(&dir).context("create credentials dir")?;
        Ok(Self { path: dir.join(CREDENTIALS_FILE) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_all(&self) -> Vec<CredentialRecord> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return vec![] };
        let parsed: Result<StoreFile, _> = serde_json::from_str(&text);
        match parsed {
            Ok(f) if f.version <= STORE_VERSION => f
                .entries
                .into_iter()
                // ADR-008: unknown cipher → drop the entry, never guess the
                // decryption form. Mixed states are legal, so this is a
                // per-entry filter, not a whole-store corruption.
                .filter(|r| r.encoding.cipher == PLAINTEXT)
                .collect(),
            _ => {
                let corrupt = self.path.with_extension("json.corrupt");
                if std::fs::rename(&self.path, &corrupt).is_ok() {
                    tracing::warn!(path = %self.path.display(), "credentials store corrupt; renamed to .corrupt and starting empty");
                }
                vec![]
            }
        }
    }

    pub fn get(&self, provider_id: &str) -> Option<CredentialRecord> {
        self.read_all().into_iter().find(|r| r.provider_id == provider_id)
    }

    pub fn reveal(&self, provider_id: &str) -> Option<String> {
        match self.get(provider_id)?.value {
            CredentialValue::Key { key } => Some(key),
            CredentialValue::External { method_id, .. } => Some(format!("external:{method_id}")),
            CredentialValue::Oauth { .. } => None, // reserved slot, not implemented (D-M4)
        }
    }

    pub fn set(
        &self,
        provider_id: &str,
        value: CredentialValue,
    ) -> anyhow::Result<CredentialRecord> {
        let lock = RegistryLock::new(lock_path_of(&self.path));
        let _guard = lock.acquire().context("acquire credential store lock")?;
        let mut entries = self.read_all(); // re-read inside the lock (ADR-006/007)
        let now = unix_now_ms();
        let existing = entries.iter().position(|r| r.provider_id == provider_id);
        let record = match existing {
            // Overwrite in place: keep id/time_created, refresh time_updated (A.3).
            Some(i) => {
                let kept = entries.remove(i);
                CredentialRecord {
                    id: kept.id,
                    time_created: kept.time_created,
                    time_updated: now,
                    provider_id: provider_id.into(),
                    value,
                    encoding: Encoding { cipher: PLAINTEXT.into() },
                }
            }
            None => CredentialRecord {
                id: format!("cred_{:08}", next_seq(&entries)),
                provider_id: provider_id.into(),
                value,
                encoding: Encoding { cipher: PLAINTEXT.into() },
                time_created: now,
                time_updated: now,
            },
        };
        entries.push(record.clone());
        let file = StoreFile { version: STORE_VERSION, entries };
        // Compact form: the on-disk contract asserts no spaces in
        // `"cipher":"plaintext"` / `"version":1` (see store_tests).
        let bytes = serde_json::to_vec(&file)?;
        mirri_persistence::write_file_atomically(&self.path, &bytes)?;
        Ok(record)
    }

    pub fn remove(&self, provider_id: &str) -> anyhow::Result<bool> {
        let lock = RegistryLock::new(lock_path_of(&self.path));
        let _guard = lock.acquire().context("acquire credential store lock")?;
        let mut entries = self.read_all();
        let before = entries.len();
        entries.retain(|r| r.provider_id != provider_id);
        if entries.len() == before {
            return Ok(false);
        }
        let bytes = serde_json::to_vec(&StoreFile { version: STORE_VERSION, entries })?;
        mirri_persistence::write_file_atomically(&self.path, &bytes)?;
        Ok(true)
    }
}

fn lock_path_of(store: &Path) -> PathBuf {
    store.with_file_name(format!("{CREDENTIALS_FILE}.lock"))
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn next_seq(entries: &[CredentialRecord]) -> u64 {
    entries
        .iter()
        .filter_map(|r| r.id.strip_prefix("cred_"))
        .filter_map(|s| s.parse::<u64>().ok())
        .max()
        .map_or(1, |m| m + 1)
}
