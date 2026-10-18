//! The home-level workspace trust store (design
//! `docs/specs/rust-core-design/08-multi-workspace.md` §5 D6, batch W7).
//!
//! `WorkspaceTrustStore` persists which registered workspaces the user has
//! trusted, at `<home>/workspace-trust.json` — a file **deliberately
//! separate** from `workspaces.json` so a workspace's trust survives
//! unregister/re-register (D6: "信任状态落 home 级存储（不随注销删除）").
//!
//! Semantics (D6 "默认未信任"):
//!
//! - an id absent from the store (and every degraded read: missing file,
//!   corrupt JSON, a wrong-shaped document) answers **untrusted** — the
//!   default is never an implicit grant of trust;
//! - [`WorkspaceTrustStore::set_trusted`] is the write face the W8
//!   `workspace.setTrust` RPC will consume;
//! - the store is a pure home-keyed document: it never consults (or writes)
//!   the workspace registry, the session index, or any bucket state.
//!
//! Reads and writes are direct local filesystem access — this lives in
//! `mirri-embedded` (outside the mirri-core zero-IO guard), like the
//! bootstrap's other home-level file IO.

use std::path::{Path, PathBuf};

use mirri_persistence::fs_utils::write_file_atomically;

/// The store's file name under the Mirri home.
const TRUST_FILENAME: &str = "workspace-trust.json";

/// The on-disk document shape: `{"version":1,"trustedWorkspaces":[..]}`.
#[derive(serde::Serialize, serde::Deserialize)]
struct TrustDocument {
    version: u8,
    #[serde(default)]
    trusted_workspaces: Vec<String>,
}

/// The home-level workspace trust store (see the module doc). Cheap to
/// hold; every read re-reads the file so processes sharing a home observe
/// each other's trust grants without a restart. Cloning shares nothing (it
/// is a path handle), so the composition root hands the same store to the
/// registry's builder and the W8 `workspace.setTrust` RPC face.
#[derive(Clone)]
pub struct WorkspaceTrustStore {
    /// The store file (`<home>/workspace-trust.json`).
    path: PathBuf,
}

impl WorkspaceTrustStore {
    /// Open the store rooted at `<home>/workspace-trust.json`. `None` when
    /// no home is given (an engine without an injected home has no place to
    /// persist trust — its non-default workspaces stay untrusted).
    ///
    /// Never fails and never creates the file: a missing or unreadable
    /// document reads as "nothing is trusted".
    pub fn open(home: Option<&Path>) -> Option<Self> {
        let home = home?;
        Some(Self {
            path: home.join(TRUST_FILENAME),
        })
    }

    /// Whether the workspace is trusted. Unknown ids — and every degraded
    /// read (missing file, corrupt JSON, wrong shape) — answer `false`
    /// (D6: default untrusted; the gate must fail closed).
    pub fn is_trusted(&self, workspace_id: &str) -> bool {
        self.read_document()
            .trusted_workspaces
            .iter()
            .any(|id| id == workspace_id)
    }

    /// Set a workspace's trust state, persisting the whole document
    /// atomically (temp file + rename). Trusting an already-trusted id (or
    /// untrusting an absent one) is an idempotent no-op write.
    ///
    /// Note: an unlocked read-modify-write (two engines sharing a home can
    /// race and drop each other's grants — the D8 lock+merge policy
    /// `workspaces.json` carries is not applied to the trust file); the
    /// single-writer W8 RPC face makes the window negligible, and a lost
    /// grant degrades to untrusted (the safe direction).
    pub fn set_trusted(&self, workspace_id: &str, trusted: bool) -> anyhow::Result<()> {
        let mut document = self.read_document();
        let present = document
            .trusted_workspaces
            .iter()
            .any(|id| id == workspace_id);
        match (trusted, present) {
            (true, false) => document.trusted_workspaces.push(workspace_id.to_string()),
            (false, true) => {
                document.trusted_workspaces.retain(|id| id != workspace_id);
            }
            _ => return Ok(()), // already in the requested state
        }
        document.trusted_workspaces.sort();
        document.trusted_workspaces.dedup();
        write_file_atomically(
            &self.path,
            serde_json::to_string_pretty(&document)
                .map_err(|error| anyhow::anyhow!("serialize the trust store: {error}"))?
                .as_bytes(),
        )
        .map_err(|error| anyhow::anyhow!("write the trust store: {error}"))
    }

    /// Read the document tolerantly: missing file, IO failure, corrupt
    /// JSON, or a wrong-shaped value all degrade to the empty document
    /// (everything untrusted) with a warning — a broken trust file must
    /// never block startup, and must never *grant* trust either.
    fn read_document(&self) -> TrustDocument {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return TrustDocument {
                    version: 1,
                    trusted_workspaces: Vec::new(),
                };
            }
            Err(error) => {
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "reading workspace-trust.json failed; treating every workspace as untrusted"
                );
                return TrustDocument {
                    version: 1,
                    trusted_workspaces: Vec::new(),
                };
            }
        };
        match serde_json::from_slice::<TrustDocument>(&bytes) {
            Ok(document) => document,
            Err(error) => {
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "workspace-trust.json is not a valid trust document; treating every \
                     workspace as untrusted"
                );
                TrustDocument {
                    version: 1,
                    trusted_workspaces: Vec::new(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given a fresh home with no trust file, when a workspace is queried,
    // then it answers untrusted (D6: the default is never an implicit
    // trust grant).
    #[test]
    fn should_answer_untrusted_when_the_store_is_fresh() {
        let home = tempfile::tempdir().expect("home tempdir");
        let store = WorkspaceTrustStore::open(Some(home.path())).expect("store opens");

        assert!(
            !store.is_trusted("wd_fresh_aaaaaaaaaaaa"),
            "a fresh workspace must be untrusted"
        );
    }

    // Given a trusted workspace, when it is unregistered (its registry row
    // removed) and re-registered, then the trust store still answers
    // trusted — the store lives in its own file, deliberately outside
    // workspaces.json (D6: 不随注销删除).
    #[test]
    fn should_keep_trust_when_the_workspace_is_unregistered_and_re_registered() {
        let home = tempfile::tempdir().expect("home tempdir");
        let id = "wd_keep_bbbbbbbbbbbb";

        // "Register" (a workspaces.json row), trust, then "unregister"
        // (remove every registry trace — workspaces.json included).
        std::fs::write(
            home.path().join("workspaces.json"),
            format!("{{\"version\":1,\"workspaces\":{{\"{id}\":{{}}}}}}"),
        )
        .expect("write workspaces.json");
        {
            let store = WorkspaceTrustStore::open(Some(home.path())).expect("store opens");
            store.set_trusted(id, true).expect("trust persists");
        }
        std::fs::remove_file(home.path().join("workspaces.json"))
            .expect("the unregister removes workspaces.json");

        // "Re-register": a NEW store handle on the same home (a fresh
        // process of the next registration) still sees the trust.
        let store = WorkspaceTrustStore::open(Some(home.path())).expect("store reopens");
        assert!(
            store.is_trusted(id),
            "trust must survive unregister/re-register (separate storage)"
        );

        // And an explicit untrust flips it back off.
        store.set_trusted(id, false).expect("untrust persists");
        assert!(!store.is_trusted(id), "untrusting must take effect");
    }

    // Given a corrupt trust file, when a workspace is queried, then every
    // id answers untrusted (tolerant read — and never a trust grant).
    #[test]
    fn should_default_every_id_to_untrusted_when_the_file_is_corrupt() {
        let home = tempfile::tempdir().expect("home tempdir");
        std::fs::write(home.path().join("workspace-trust.json"), "{ not json")
            .expect("write corrupt trust file");
        let store = WorkspaceTrustStore::open(Some(home.path())).expect("store opens");

        assert!(
            !store.is_trusted("wd_corrupt_cccccccccccc"),
            "a corrupt document must read as untrusted"
        );

        // The write path recovers: it replaces the corrupt document with a
        // well-formed one carrying the new grant.
        store
            .set_trusted("wd_corrupt_cccccccccccc", true)
            .expect("writing over a corrupt file recovers");
        assert!(store.is_trusted("wd_corrupt_cccccccccccc"));
    }

    // Given no home, when the store is opened, then there is nothing to
    // consult (the caller's workspaces stay untrusted).
    #[test]
    fn should_open_to_none_when_no_home_is_given() {
        assert!(WorkspaceTrustStore::open(None).is_none());
    }
}
