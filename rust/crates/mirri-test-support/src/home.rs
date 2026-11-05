//! One-shot fake HOME directories for tests.
//!
//! Per the isolation contract (16-test-execution-strategy.md §4.2), every
//! test's filesystem footprint must live inside a throwaway tempdir; HOME
//! semantics come from a fake home instead of the real `~`.

use std::path::PathBuf;

/// Create a one-shot fake HOME directory.
///
/// Returns the home path plus the owning [`tempfile::TempDir`]; the directory
/// is deleted when the `TempDir` is dropped, so callers must keep it alive
/// for as long as the path is in use.
pub fn temp_home() -> (PathBuf, tempfile::TempDir) {
    let tempdir = tempfile::tempdir().expect("tempdir creation should succeed");
    let home = tempdir.path().to_path_buf();
    (home, tempdir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn should_create_writable_home_under_tempdir() {
        let (home, _tempdir) = temp_home();
        assert!(home.is_dir(), "fake home should exist as a directory");
        let probe = home.join("write-probe.txt");
        fs::write(&probe, "probe").expect("fake home should be writable");
        assert_eq!(fs::read_to_string(&probe).expect("probe readable"), "probe");
    }

    #[test]
    fn returned_tempdir_outlives_usage() {
        let (home, tempdir) = temp_home();
        // While the returned TempDir is still owned, the path must exist.
        assert!(home.is_dir());
        drop(tempdir);
        // After the TempDir drops, the path is gone — proving ownership was
        // with the returned value all along.
        assert!(!home.exists());
    }
}
