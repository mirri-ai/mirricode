//! Persistent server bearer token (`<mirri-home>/server.token`).
//!
//! Mirrors the TypeScript kap-server's `persistentToken` semantics so both
//! engines share one token file: the token is generated once on first boot
//! (256-bit, base64url, 43 chars), reused across restarts, and only changes
//! when the operator rotates the file. Writes go through the shared atomic
//! replace primitive ([`mirri_persistence::write_file_atomically`]: unique
//! temp file + fsync + rename) so a concurrent reader never observes a
//! half-written token; the file is created 0600 and reads refuse a file with
//! looser permissions, so the secret is never world/group-readable.
//!
//! `mirri serve` loads (or creates) the token at boot when `--auth-token`
//! is absent; an explicit `--auth-token` wins and leaves the file untouched.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// On-disk filename for the persistent token, relative to the Mirri home dir.
pub const SERVER_TOKEN_FILE: &str = "server.token";

/// Absolute path of the persistent token file for a given home dir.
pub fn server_token_path(home_dir: &Path) -> PathBuf {
    home_dir.join(SERVER_TOKEN_FILE)
}

/// Return the existing persistent token, generating and persisting one on
/// first boot. An empty file is treated as missing and regenerated.
pub fn load_or_create_server_token(home_dir: &Path) -> io::Result<String> {
    let path = server_token_path(home_dir);
    match read_token_file(&path) {
        Ok(Some(token)) => Ok(token),
        Ok(None) => {
            let token = generate_server_token();
            write_token_file(&path, &token)?;
            Ok(token)
        }
        Err(error) => Err(error),
    }
}

/// Fresh 256-bit token, base64url-encoded (43 chars, URL-safe, unpadded).
fn generate_server_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("system RNG unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Read the token file. `Ok(None)` when it is missing or empty; an error when
/// it exists but is group/world-readable (the secret would be leaked).
fn read_token_file(path: &Path) -> io::Result<Option<String>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(io::Error::other(format!(
                "server token file '{}' has overly permissive mode {:o}; \
                 expected 0600 (rotate or chmod it)",
                path.display(),
                mode & 0o777
            )));
        }
    }
    let token = contents.trim();
    if token.is_empty() {
        Ok(None)
    } else {
        Ok(Some(token.to_string()))
    }
}

/// Persist `token` through the shared atomic-write primitive
/// ([`mirri_persistence::write_file_atomically`]): a uniquely named sibling
/// temp file created 0600, fsync'd, then renamed over `path`, with a
/// best-effort parent-directory fsync. A failed write leaves the previous
/// token intact and removes its temp file.
fn write_token_file(path: &Path, token: &str) -> io::Result<()> {
    mirri_persistence::write_file_atomically(path, token.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn should_create_a_token_file_with_0600_when_no_token_exists() {
        let home = TempDir::new().expect("tempdir");
        let token = load_or_create_server_token(home.path()).expect("token created");

        assert_eq!(token.len(), 43, "base64url of 32 bytes is 43 chars");
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "token must be URL-safe base64: {token}"
        );
        let file = server_token_path(home.path());
        assert_eq!(
            fs::read_to_string(&file).expect("token file readable"),
            token,
            "token file must hold the exact token"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&file).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "token file must be owner-only");
        }
        let leftovers: Vec<_> = fs::read_dir(home.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files must be renamed away, not left behind: {leftovers:?}"
        );
    }

    #[test]
    fn should_reuse_the_persisted_token_across_calls_when_a_token_exists() {
        let home = TempDir::new().expect("tempdir");
        let first = load_or_create_server_token(home.path()).expect("first boot");
        let second = load_or_create_server_token(home.path()).expect("second boot");

        assert_eq!(first, second, "an existing token must not be rotated");
        assert_eq!(
            fs::read_to_string(server_token_path(home.path())).expect("file"),
            first
        );
    }

    #[test]
    fn should_regenerate_the_token_when_the_file_is_empty() {
        let home = TempDir::new().expect("tempdir");
        let path = server_token_path(home.path());
        fs::write(&path, "   \n").expect("empty file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .expect("0600 so the permission gate stays out of the way");
        }

        let token = load_or_create_server_token(home.path()).expect("regenerated");
        assert!(!token.is_empty(), "an empty file must trigger regeneration");
        assert_eq!(
            fs::read_to_string(&path).expect("file"),
            token,
            "the empty file must be overwritten with the new token"
        );
    }

    #[cfg(unix)]
    #[test]
    fn should_error_when_the_token_file_is_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let home = TempDir::new().expect("tempdir");
        let path = server_token_path(home.path());
        fs::write(&path, "leaky-token").expect("file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

        let error = load_or_create_server_token(home.path())
            .expect_err("a world-readable token must be refused");
        assert!(
            error.to_string().contains("overly permissive"),
            "error should explain the permission problem: {error}"
        );
    }
}
