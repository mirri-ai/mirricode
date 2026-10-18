//! Integration tests for `mirri-os` glob matching.

use std::fs;
use std::path::Path;
use std::time::Duration;

use mirri_os::{GlobMatch, GlobParams, LocalGlob};
use tempfile::TempDir;

/// Write a file with the given relative path under `root`, creating parent
/// directories as needed.
fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
}

/// Initialize an empty git repository at `root` so that the `ignore` crate
/// recognizes `.gitignore` files. Without a `.git` directory the walker does
/// not apply git-ignore rules.
fn init_git_repo(root: &Path) {
    fs::create_dir_all(root.join(".git")).unwrap();
}

#[tokio::test]
async fn should_return_all_rust_files_when_globbing_double_star_rs() {
    // Given a temp dir with .rs files in nested directories.
    let tmp = TempDir::new().unwrap();
    write_file(tmp.path(), "src/main.rs", "fn main() {}");
    write_file(tmp.path(), "src/lib.rs", "pub mod foo;");
    write_file(tmp.path(), "tests/test.rs", "#[test] fn t() {}");

    // When globbing for **/*.rs.
    let glob = LocalGlob::new();
    let params = GlobParams {
        pattern: "**/*.rs".to_string(),
        path: tmp.path().to_path_buf(),
        include_ignored: false,
    };
    let result = glob.glob(&params).await.unwrap();

    // Then all three .rs files are returned.
    assert_eq!(result.matches.len(), 3);
    assert!(result.matches.contains(&"src/main.rs".to_string()));
    assert!(result.matches.contains(&"src/lib.rs".to_string()));
    assert!(result.matches.contains(&"tests/test.rs".to_string()));
    assert!(!result.truncated);
}

#[tokio::test]
async fn should_respect_gitignore_when_include_ignored_is_false() {
    // Given a temp dir with a .gitignore that excludes *.log.
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    write_file(tmp.path(), ".gitignore", "*.log\n");
    write_file(tmp.path(), "app.log", "log content");
    write_file(tmp.path(), "app.rs", "fn main() {}");

    // When globbing for * with include_ignored=false.
    let glob = LocalGlob::new();
    let params = GlobParams {
        pattern: "*".to_string(),
        path: tmp.path().to_path_buf(),
        include_ignored: false,
    };
    let result = glob.glob(&params).await.unwrap();

    // Then app.log is excluded but app.rs is included.
    assert!(result.matches.iter().any(|p| p == "app.rs"));
    assert!(!result.matches.iter().any(|p| p == "app.log"));
}

#[tokio::test]
async fn should_include_ignored_files_when_include_ignored_is_true() {
    // Given a temp dir with a .gitignore that excludes *.log.
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    write_file(tmp.path(), ".gitignore", "*.log\n");
    write_file(tmp.path(), "app.log", "log content");
    write_file(tmp.path(), "app.rs", "fn main() {}");

    // When globbing for * with include_ignored=true.
    let glob = LocalGlob::new();
    let params = GlobParams {
        pattern: "*".to_string(),
        path: tmp.path().to_path_buf(),
        include_ignored: true,
    };
    let result = glob.glob(&params).await.unwrap();

    // Then both app.log and app.rs are included.
    assert!(result.matches.iter().any(|p| p == "app.rs"));
    assert!(result.matches.iter().any(|p| p == "app.log"));
}

#[tokio::test]
async fn should_return_empty_list_when_pattern_matches_nothing() {
    // Given a temp dir with only .rs files.
    let tmp = TempDir::new().unwrap();
    write_file(tmp.path(), "main.rs", "fn main() {}");
    write_file(tmp.path(), "lib.rs", "pub mod foo;");

    // When globbing for *.py.
    let glob = LocalGlob::new();
    let params = GlobParams {
        pattern: "*.py".to_string(),
        path: tmp.path().to_path_buf(),
        include_ignored: false,
    };
    let result = glob.glob(&params).await.unwrap();

    // Then no files are returned.
    assert!(result.matches.is_empty());
    assert!(!result.truncated);
}

#[tokio::test]
async fn should_set_truncated_when_matches_exceed_100() {
    // Given a temp dir with 101 .txt files.
    let tmp = TempDir::new().unwrap();
    for i in 0..101 {
        write_file(tmp.path(), &format!("file_{i}.txt"), "content");
    }

    // When globbing for *.txt.
    let glob = LocalGlob::new();
    let params = GlobParams {
        pattern: "*.txt".to_string(),
        path: tmp.path().to_path_buf(),
        include_ignored: false,
    };
    let result = glob.glob(&params).await.unwrap();

    // Then the result is truncated to exactly 100 matches.
    assert!(result.truncated);
    assert_eq!(result.matches.len(), 100);
}

#[tokio::test]
async fn should_sort_results_newest_first_when_globbing_files() {
    // Given a temp dir where file_a.txt is created before file_b.txt.
    let tmp = TempDir::new().unwrap();
    write_file(tmp.path(), "file_a.txt", "a");
    // Sleep to ensure a detectable mtime difference.
    std::thread::sleep(Duration::from_millis(50));
    write_file(tmp.path(), "file_b.txt", "b");

    // When globbing for *.txt.
    let glob = LocalGlob::new();
    let params = GlobParams {
        pattern: "*.txt".to_string(),
        path: tmp.path().to_path_buf(),
        include_ignored: false,
    };
    let result = glob.glob(&params).await.unwrap();

    // Then file_b.txt (newer) appears before file_a.txt (older).
    assert_eq!(result.matches.len(), 2);
    assert_eq!(result.matches[0], "file_b.txt");
    assert_eq!(result.matches[1], "file_a.txt");
}
