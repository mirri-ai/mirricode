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
        limit: 0,
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
        limit: 0,
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
        limit: 0,
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
        limit: 0,
    };
    let result = glob.glob(&params).await.unwrap();

    // Then no files are returned.
    assert!(result.matches.is_empty());
    assert!(!result.truncated);
}

#[tokio::test]
async fn should_collect_the_full_set_when_matches_exceed_the_hard_cap_region() {
    // Given a temp dir with 101 .txt files and no explicit limit.
    let tmp = TempDir::new().unwrap();
    for i in 0..101 {
        write_file(tmp.path(), &format!("file_{i}.txt"), "content");
    }

    // When globbing for *.txt with limit 0 (unlimited — under the hard caps).
    let glob = LocalGlob::new();
    let params = GlobParams {
        pattern: "*.txt".to_string(),
        path: tmp.path().to_path_buf(),
        include_ignored: false,
        limit: 0,
    };
    let result = glob.glob(&params).await.unwrap();

    // Then all 101 matches are returned untruncated with the true total.
    assert!(!result.truncated);
    assert_eq!(result.matches.len(), 101);
    assert_eq!(result.total_matched, 101);
}

#[tokio::test]
async fn should_return_the_collected_set_untruncated_when_matches_exceed_the_limit() {
    // Given a temp dir with 5 .txt files.
    let tmp = TempDir::new().unwrap();
    for i in 0..5 {
        write_file(tmp.path(), &format!("f{i}.txt"), "x");
    }

    // When globbing with limit 3 (non-zero) — the search layer does not
    // truncate the result set; limit only stops the walker once enough
    // matches have been collected (T3 ruling: non-zero limit means the
    // walker stop point IS the reported total, "enough collected" semantics).
    let r = <LocalGlob as GlobMatch>::glob(&LocalGlob::new(), &GlobParams {
        pattern: "*.txt".to_string(),
        path: tmp.path().to_path_buf(),
        include_ignored: false,
        limit: 3,
    })
    .await
    .unwrap();

    // Then every match the walker collected before stopping is returned —
    // no truncation to a result-set cap, and no truncated flag.
    assert_eq!(r.matches.len(), 3, "搜索层不截断结果集——limit 只管 walker 停点");
    assert_eq!(r.total_matched, 3, "非零 limit 的停点即 total（够数了语义）");
    assert!(!r.truncated);
}

#[tokio::test]
async fn should_return_the_full_set_when_limit_is_large_enough() {
    // Given a temp dir with 5 .txt files.
    let tmp = TempDir::new().unwrap();
    for i in 0..5 {
        write_file(tmp.path(), &format!("f{i}.txt"), "x");
    }

    // When globbing with limit 10 (larger than the match count).
    let r = <LocalGlob as GlobMatch>::glob(&LocalGlob::new(), &GlobParams {
        pattern: "*.txt".to_string(),
        path: tmp.path().to_path_buf(),
        include_ignored: false,
        limit: 10,
    })
    .await
    .unwrap();

    // Then the whole match set is returned with the true total.
    assert_eq!(r.matches.len(), 5);
    assert_eq!(r.total_matched, 5);
    assert!(!r.truncated);
}

#[tokio::test]
async fn should_exclude_sensitive_files_from_total_when_counting() {
    // Given a temp dir with one regular file and one sensitive file.
    let tmp = TempDir::new().unwrap();
    write_file(tmp.path(), "ok.txt", "x");
    write_file(tmp.path(), ".env", "SECRET");

    // When globbing for * with limit 0.
    let r = <LocalGlob as GlobMatch>::glob(&LocalGlob::new(), &GlobParams {
        pattern: "*".to_string(),
        path: tmp.path().to_path_buf(),
        include_ignored: false,
        limit: 0,
    })
    .await
    .unwrap();

    // Then the sensitive file neither matches nor counts toward the total
    // (prevents existence disclosure via the count header).
    assert_eq!(r.total_matched, 1, "敏感文件不计入 total（防存在性泄漏）");
    assert!(!r.matches.iter().any(|m| m.contains(".env")));
}

#[tokio::test]
async fn should_break_mtime_ties_by_path_order_when_sorting() {
    // Given three files whose mtimes are pinned to the same instant.
    let tmp = TempDir::new().unwrap();
    for name in ["b.txt", "a.txt", "c.txt"] {
        let p = tmp.path().join(name);
        write_file(tmp.path(), name, "x");
        set_mtime_now(&p);
    }

    // When globbing for *.txt with limit 0.
    let r = <LocalGlob as GlobMatch>::glob(&LocalGlob::new(), &GlobParams {
        pattern: "*.txt".to_string(),
        path: tmp.path().to_path_buf(),
        include_ignored: false,
        limit: 0,
    })
    .await
    .unwrap();

    // Then equal mtimes are broken by relative-path lexicographic order.
    assert_eq!(r.matches, vec!["a.txt", "b.txt", "c.txt"]);
}

/// Pin a file's modification time to `UNIX_EPOCH + 1h` (the same value for
/// every caller), so files sorted by mtime tie and the secondary sort key
/// (relative path lexicographic order) decides the result.
fn set_mtime_now(path: &Path) {
    use std::fs::FileTimes;
    let file = fs::File::open(path).unwrap();
    let times = FileTimes::new()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(3_600));
    file.set_times(times).expect("set mtime");
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
        limit: 0,
    };
    let result = glob.glob(&params).await.unwrap();

    // Then file_b.txt (newer) appears before file_a.txt (older).
    assert_eq!(result.matches.len(), 2);
    assert_eq!(result.matches[0], "file_b.txt");
    assert_eq!(result.matches[1], "file_a.txt");
}
