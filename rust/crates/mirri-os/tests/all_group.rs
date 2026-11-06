// layer: T1
// Domain group binary consolidated from per-file test binaries
// per 16-test-execution-strategy.md §4.4 (pure file moves).
mod fs_tests {
//! Integration tests for `mirri-os` filesystem operations.

use std::path::PathBuf;

use mirri_os::{FileSystem, LocalFs};
use tempfile::TempDir;

fn make_lines(count: usize) -> String {
    (0..count)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[tokio::test]
async fn should_return_same_content_when_write_text_then_read_text() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("hello.txt");

    let content = "hello world\nsecond line\n";
    fs.write_text(&path, content).await.unwrap();

    let read_back = fs.read_text(&path).await.unwrap();
    assert_eq!(read_back, content);
}

#[tokio::test]
async fn should_auto_create_parent_dirs_when_writing_to_nested_path() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("a/b/c/deep.txt");

    fs.write_text(&path, "nested").await.unwrap();

    let read_back = fs.read_text(&path).await.unwrap();
    assert_eq!(read_back, "nested");
}

#[tokio::test]
async fn should_report_correct_line_count_when_scanning_text_file() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("lines.txt");

    // 20 lines, each terminated by \n
    let content = make_lines(20);
    fs.write_text(&path, &content).await.unwrap();

    let scan = fs.scan_text_file(&path).await.unwrap();
    assert_eq!(scan.total_lines, 20);
    assert!(scan.ends_with_newline);
    assert!(!scan.has_nul);
    assert_eq!(scan.line_ending_flags, "lf");
}

#[tokio::test]
async fn should_report_has_nul_true_when_file_contains_nul_bytes() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("binary.txt");

    // Content with a NUL byte in the middle
    let content = b"before\0after\n";
    fs.write_bytes(&path, content).await.unwrap();

    let scan = fs.scan_text_file(&path).await.unwrap();
    assert!(scan.has_nul);
}

#[tokio::test]
async fn should_report_crlf_when_file_uses_crlf_line_endings() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("crlf.txt");

    let content = "line one\r\nline two\r\nline three\r\n";
    fs.write_text(&path, content).await.unwrap();

    let scan = fs.scan_text_file(&path).await.unwrap();
    assert_eq!(scan.line_ending_flags, "crlf");
    assert_eq!(scan.total_lines, 3);
    assert!(scan.ends_with_newline);
}

#[tokio::test]
async fn should_return_correct_lines_when_reading_line_range() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("range.txt");

    // 30 lines: "line 0" .. "line 29"
    let content = make_lines(30);
    fs.write_text(&path, &content).await.unwrap();

    // start_line=10 (0-based), max_lines=5 → lines 10..14
    let lines = fs.read_line_range(&path, 10, 5).await.unwrap();
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[0], "line 10");
    assert_eq!(lines[4], "line 14");
}

#[tokio::test]
async fn should_return_last_n_lines_when_reading_tail_lines() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("tail.txt");

    // 30 lines: "line 0" .. "line 29"
    let content = make_lines(30);
    fs.write_text(&path, &content).await.unwrap();

    let lines = fs.read_tail_lines(&path, 3).await.unwrap();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "line 27");
    assert_eq!(lines[1], "line 28");
    assert_eq!(lines[2], "line 29");
}

#[tokio::test]
async fn should_list_all_entries_when_iterating_directory() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let dir = tmp.path().join("mydir");
    fs.mkdir(&dir, true).await.unwrap();

    fs.write_text(&dir.join("a.txt"), "a").await.unwrap();
    fs.write_text(&dir.join("b.txt"), "b").await.unwrap();
    fs.mkdir(&dir.join("sub"), false).await.unwrap();

    let mut entries = fs.iterdir(&dir).await.unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].name, "a.txt");
    assert!(entries[0].is_file);
    assert!(!entries[0].is_dir);

    assert_eq!(entries[1].name, "b.txt");
    assert!(entries[1].is_file);

    assert_eq!(entries[2].name, "sub");
    assert!(entries[2].is_dir);
    assert!(!entries[2].is_file);
}

#[tokio::test]
async fn should_report_correct_metadata_when_stating_a_file() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("stat.txt");

    let content = "12345";
    fs.write_text(&path, content).await.unwrap();

    let stat = fs.stat(&path).await.unwrap();
    assert_eq!(stat.size, content.len() as u64);
    assert!(stat.is_file);
    assert!(!stat.is_dir);
    assert!(!stat.is_symlink);
    assert!(stat.mtime > 0);
}

#[tokio::test]
async fn should_move_file_when_renaming() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let from = tmp.path().join("from.txt");
    let to = tmp.path().join("to.txt");

    fs.write_text(&from, "move me").await.unwrap();
    fs.rename(&from, &to).await.unwrap();

    // Source should no longer exist
    let src_stat = fs.stat(&from).await;
    assert!(src_stat.is_err());

    // Destination should have the content
    let content = fs.read_text(&to).await.unwrap();
    assert_eq!(content, "move me");
}

#[tokio::test]
async fn should_copy_file_when_copying() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let from = tmp.path().join("original.txt");
    let to = tmp.path().join("copy.txt");

    fs.write_text(&from, "copy me").await.unwrap();
    fs.copy(&from, &to).await.unwrap();

    // Both source and destination should have the content
    let src_content = fs.read_text(&from).await.unwrap();
    let dst_content = fs.read_text(&to).await.unwrap();
    assert_eq!(src_content, "copy me");
    assert_eq!(dst_content, "copy me");
}

#[tokio::test]
async fn should_remove_file_when_remove_file() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("deleteme.txt");

    fs.write_text(&path, "gone").await.unwrap();
    assert!(fs.stat(&path).await.is_ok());

    fs.remove_file(&path).await.unwrap();

    assert!(fs.stat(&path).await.is_err());
}

#[tokio::test]
async fn should_remove_directory_recursively_when_recursive_true() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let dir: PathBuf = tmp.path().join("tree");
    fs.mkdir(&dir, true).await.unwrap();
    fs.write_text(&dir.join("nested/deep.txt"), "deep")
        .await
        .unwrap();

    fs.remove_dir(&dir, true).await.unwrap();

    assert!(fs.stat(&dir).await.is_err());
}

#[tokio::test]
async fn should_return_the_requested_byte_window_when_reading_a_range() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("ranged.bin");

    fs.write_bytes(&path, b"0123456789abcdef").await.unwrap();

    let window = fs.read_bytes_range(&path, 4, 6).await.unwrap();
    assert_eq!(window, b"456789");
}

#[tokio::test]
async fn should_return_a_short_read_when_the_range_reaches_end_of_file() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("short.bin");

    fs.write_bytes(&path, b"0123456789").await.unwrap();

    let window = fs.read_bytes_range(&path, 8, 100).await.unwrap();
    assert_eq!(window, b"89");
}

#[tokio::test]
async fn should_return_empty_when_the_range_offset_is_beyond_end_of_file() {
    let tmp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let path = tmp.path().join("past.bin");

    fs.write_bytes(&path, b"0123456789").await.unwrap();

    let window = fs.read_bytes_range(&path, 100, 10).await.unwrap();
    assert!(window.is_empty());
}

}

mod glob_tests {
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
    let r = <LocalGlob as GlobMatch>::glob(
        &LocalGlob::new(),
        &GlobParams {
            pattern: "*.txt".to_string(),
            path: tmp.path().to_path_buf(),
            include_ignored: false,
            limit: 3,
        },
    )
    .await
    .unwrap();

    // Then every match the walker collected before stopping is returned —
    // no truncation to a result-set cap, and no truncated flag.
    assert_eq!(
        r.matches.len(),
        3,
        "搜索层不截断结果集——limit 只管 walker 停点"
    );
    assert_eq!(
        r.total_matched, 3,
        "非零 limit 的停点即 total（够数了语义）"
    );
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
    let r = <LocalGlob as GlobMatch>::glob(
        &LocalGlob::new(),
        &GlobParams {
            pattern: "*.txt".to_string(),
            path: tmp.path().to_path_buf(),
            include_ignored: false,
            limit: 10,
        },
    )
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
    let r = <LocalGlob as GlobMatch>::glob(
        &LocalGlob::new(),
        &GlobParams {
            pattern: "*".to_string(),
            path: tmp.path().to_path_buf(),
            include_ignored: false,
            limit: 0,
        },
    )
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
    let r = <LocalGlob as GlobMatch>::glob(
        &LocalGlob::new(),
        &GlobParams {
            pattern: "*.txt".to_string(),
            path: tmp.path().to_path_buf(),
            include_ignored: false,
            limit: 0,
        },
    )
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
    let times = FileTimes::new().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(3_600));
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

}

mod integration_tests {
//! Cross-module integration tests for mirri-os.
//!
//! These tests verify that multiple modules cooperate correctly:
//! search results feeding into filesystem reads, gitignore consistency
//! between glob and search, sensitive-file filtering in search, process
//! output read back through the filesystem, path-access enforcement, and
//! line-range scanning of large files.

use std::path::Path;

use mirri_os::fs::{FileSystem, LocalFs};
use mirri_os::glob::{GlobMatch, GlobParams, LocalGlob};
use mirri_os::path_access::PathAccess;
use mirri_os::process::{ExecOptions, LocalProcess, OsProcess};
use mirri_os::search::{ContentSearch, RipgrepSearch, SearchParams};
use tempfile::TempDir;

/// Return `true` if the `rg` (ripgrep) binary is available on `PATH`.
fn ripgrep_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Initialize an empty git repository in `dir`.
fn init_git_repo(dir: &Path) {
    std::process::Command::new("git")
        .arg("init")
        .current_dir(dir)
        .output()
        .expect("failed to run `git init`");
}

#[tokio::test]
async fn should_read_file_content_when_found_by_search() {
    if !ripgrep_available() {
        return;
    }

    // Given: a temp directory containing src/main.rs with "fn main() {}".
    let temp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let main_rs = temp.path().join("src").join("main.rs");
    fs.write_text(&main_rs, "fn main() {}").await.unwrap();

    // When: searching for "fn main" via RipgrepSearch.
    let search = RipgrepSearch::new();
    let params = SearchParams {
        pattern: "fn main".to_string(),
        paths: vec![temp.path().to_path_buf()],
        ..Default::default()
    };
    let result = search.search(&params).await.unwrap();

    // Then: the search produces at least one match.
    assert!(
        !result.matches.is_empty(),
        "expected at least one search match"
    );

    // And: the matched file can be read via LocalFs and contains "fn main".
    let matched_path = Path::new(&result.matches[0].path);
    let content = fs.read_text(matched_path).await.unwrap();
    assert!(
        content.contains("fn main"),
        "file content should contain 'fn main', got: {content}"
    );
}

#[tokio::test]
async fn should_respect_gitignore_when_globbing_and_searching() {
    // Given: a git repo with a .gitignore that excludes *.log,
    // and two files (app.log and app.rs) both containing "pattern".
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());

    let fs = LocalFs::new();
    fs.write_text(&temp.path().join(".gitignore"), "*.log")
        .await
        .unwrap();
    fs.write_text(&temp.path().join("app.log"), "pattern")
        .await
        .unwrap();
    fs.write_text(&temp.path().join("app.rs"), "pattern")
        .await
        .unwrap();

    // When: globbing "*" with include_ignored = false.
    let glob = LocalGlob::new();
    let glob_params = GlobParams {
        pattern: "*".to_string(),
        path: temp.path().to_path_buf(),
        include_ignored: false,
        limit: 0,
    };
    let glob_result = glob.glob(&glob_params).await.unwrap();

    // Then: app.log is excluded by .gitignore, but app.rs is present.
    let glob_has_log = glob_result.matches.iter().any(|p| p.ends_with("app.log"));
    let glob_has_rs = glob_result.matches.iter().any(|p| p.ends_with("app.rs"));
    assert!(
        !glob_has_log,
        "app.log should not appear in glob results (gitignore)"
    );
    assert!(glob_has_rs, "app.rs should appear in glob results");

    // And when: searching for "pattern" via RipgrepSearch (if available).
    if !ripgrep_available() {
        return;
    }
    let search = RipgrepSearch::new();
    let search_params = SearchParams {
        pattern: "pattern".to_string(),
        paths: vec![temp.path().to_path_buf()],
        ..Default::default()
    };
    let search_result = search.search(&search_params).await.unwrap();

    // Then: app.log is excluded from search results as well.
    let search_has_log = search_result
        .matches
        .iter()
        .any(|m| m.path.ends_with("app.log"));
    let search_has_rs = search_result
        .matches
        .iter()
        .any(|m| m.path.ends_with("app.rs"));
    assert!(
        !search_has_log,
        "app.log should not appear in search results (gitignore)"
    );
    assert!(search_has_rs, "app.rs should appear in search results");
}

#[tokio::test]
async fn should_exclude_sensitive_files_when_searching() {
    if !ripgrep_available() {
        return;
    }

    // Given: a temp directory with .env and config.txt, both containing "SECRET_KEY".
    let temp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    fs.write_text(&temp.path().join(".env"), "SECRET_KEY=abc")
        .await
        .unwrap();
    fs.write_text(&temp.path().join("config.txt"), "SECRET_KEY=xyz")
        .await
        .unwrap();

    // When: searching for "SECRET_KEY" with include_ignored = true
    // so that ripgrep searches hidden files like .env.
    let search = RipgrepSearch::new();
    let params = SearchParams {
        pattern: "SECRET_KEY".to_string(),
        paths: vec![temp.path().to_path_buf()],
        include_ignored: true,
        ..Default::default()
    };
    let result = search.search(&params).await.unwrap();

    // Then: only config.txt appears — .env is filtered out by the
    // sensitive-file filter in the search module.
    let has_env = result.matches.iter().any(|m| m.path.ends_with(".env"));
    let has_config = result
        .matches
        .iter()
        .any(|m| m.path.ends_with("config.txt"));
    assert!(
        !has_env,
        ".env should be excluded by the sensitive-file filter"
    );
    assert!(has_config, "config.txt should be present in search results");
}

#[tokio::test]
async fn should_read_file_when_created_by_process_execution() {
    // Given: a temp directory.
    let temp = TempDir::new().unwrap();
    let fs = LocalFs::new();

    // When: executing a shell command that writes to output.txt
    // in the temp directory.
    let process = LocalProcess::new();
    let opts = ExecOptions {
        cwd: Some(temp.path().to_path_buf()),
        ..Default::default()
    };
    let mut child = process
        .exec("/bin/sh", &["-c", "echo hello > output.txt"], &opts)
        .await
        .unwrap();

    // Close stdin so the child sees EOF (it does not read stdin, but
    // this avoids holding the pipe open unnecessarily).
    drop(child.take_stdin());

    let exit_code = child.wait().await.unwrap();
    assert_eq!(exit_code, 0, "shell command should exit successfully");

    // Then: the file can be read via LocalFs and contains "hello\n".
    let output_path = temp.path().join("output.txt");
    let content = fs.read_text(&output_path).await.unwrap();
    assert_eq!(content, "hello\n");
}

#[tokio::test]
async fn should_verify_path_access_when_reading_file() {
    // Given: a workspace temp dir with safe.txt inside, and outside.txt
    // in a separate temp dir outside the workspace.
    let workspace = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();

    let fs = LocalFs::new();
    let safe_file = workspace.path().join("safe.txt");
    let outside_file = outside.path().join("outside.txt");
    fs.write_text(&safe_file, "safe").await.unwrap();
    fs.write_text(&outside_file, "outside").await.unwrap();

    // When: checking path access with PathAccess rooted at the workspace.
    let path_access = PathAccess::new(workspace.path().to_path_buf());

    // Then: the safe file is accessible.
    assert!(
        path_access.check_access(&safe_file).is_ok(),
        "safe.txt should be within the workspace"
    );

    // And: the outside file is rejected.
    assert!(
        path_access.check_access(&outside_file).is_err(),
        "outside.txt should be outside the workspace"
    );
}

#[tokio::test]
async fn should_scan_and_read_line_range_when_file_is_large() {
    // Given: a 100-line file in a temp directory.
    let temp = TempDir::new().unwrap();
    let fs = LocalFs::new();
    let file_path = temp.path().join("large.txt");

    let mut content = String::new();
    for i in 0..100u32 {
        content.push_str(&format!("line {i}\n"));
    }
    fs.write_text(&file_path, &content).await.unwrap();

    // When: scanning the file for line metadata.
    let scan = fs.scan_text_file(&file_path).await.unwrap();

    // Then: the scan reports 100 lines.
    assert_eq!(scan.total_lines, 100);

    // And when: reading a range of 5 lines starting at line 10 (0-based).
    let range = fs.read_line_range(&file_path, 10, 5).await.unwrap();

    // Then: 5 lines are returned, starting at "line 10".
    assert_eq!(range.len(), 5);
    assert_eq!(range[0], "line 10");
    assert_eq!(range[4], "line 14");

    // And when: reading the last 3 lines.
    let tail = fs.read_tail_lines(&file_path, 3).await.unwrap();

    // Then: 3 lines are returned, corresponding to the last 3 lines.
    assert_eq!(tail.len(), 3);
    assert_eq!(tail[0], "line 97");
    assert_eq!(tail[1], "line 98");
    assert_eq!(tail[2], "line 99");
}

}

mod path_access_tests {
//! Integration tests for `mirri-os` path access control.

use std::path::PathBuf;

use mirri_os::PathAccess;

/// Build a `PathAccess` rooted at a synthetic workspace root `/workspace`.
fn workspace_access() -> PathAccess {
    PathAccess::new(PathBuf::from("/workspace"))
}

#[test]
fn should_resolve_dotdot_when_normalizing_path_with_dotdot() {
    // Given a workspace root and a path with a `..` component.
    let access = workspace_access();

    // When normalizing.
    let normalized = access.normalize(&PathBuf::from("foo/../bar"));

    // Then the `..` cancels out `foo`, leaving `bar` under the workspace root.
    assert_eq!(normalized, PathBuf::from("/workspace/bar"));
}

#[test]
fn should_collapse_multiple_slashes_when_normalizing() {
    // Given a path with repeated slashes.
    let access = workspace_access();

    // When normalizing.
    let normalized = access.normalize(&PathBuf::from("foo//bar"));

    // Then the repeated slashes collapse into a single separator.
    assert_eq!(normalized, PathBuf::from("/workspace/foo/bar"));
}

#[test]
fn should_resolve_relative_path_when_path_is_relative_to_workspace() {
    // Given a relative path.
    let access = workspace_access();

    // When normalizing.
    let normalized = access.normalize(&PathBuf::from("src/main.rs"));

    // Then it is resolved against the workspace root.
    assert_eq!(normalized, PathBuf::from("/workspace/src/main.rs"));
}

#[test]
fn should_return_true_when_path_is_inside_workspace() {
    // Given a path inside the workspace.
    let access = workspace_access();

    // When checking containment.
    let inside = access.is_within_workspace(&PathBuf::from("/workspace/src/main.rs"));

    // Then it is considered within the workspace.
    assert!(inside);
}

#[test]
fn should_return_false_when_path_is_outside_workspace() {
    // Given a path outside the workspace.
    let access = workspace_access();

    // When checking containment.
    let outside = access.is_within_workspace(&PathBuf::from("/etc/passwd"));

    // Then it is not considered within the workspace.
    assert!(!outside);
}

#[test]
fn should_return_true_when_path_is_workspace_root_itself() {
    // Given the workspace root path itself.
    let access = workspace_access();

    // When checking containment.
    let root_contained = access.is_within_workspace(&PathBuf::from("/workspace"));

    // Then it is considered within the workspace.
    assert!(root_contained);
}

#[test]
fn should_detect_as_sensitive_when_file_is_env_file() {
    // Given a `.env` file inside the workspace.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/.env"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_detect_as_sensitive_when_file_is_env_production() {
    // Given a `.env.production` file inside the workspace.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/.env.production"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_not_detect_as_sensitive_when_file_is_env_example() {
    // Given a `.env.example` file inside the workspace.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/.env.example"));

    // Then it is not flagged as sensitive.
    assert!(!sensitive);
}

#[test]
fn should_detect_as_sensitive_when_file_is_id_rsa_in_ssh_dir() {
    // Given an SSH private key inside a `.ssh` directory.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/.ssh/id_rsa"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_detect_as_sensitive_when_file_is_pem() {
    // Given a `.pem` certificate file.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/cert.pem"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_detect_as_sensitive_when_file_is_key_file() {
    // Given a `.key` private key file.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/private.key"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_detect_as_sensitive_when_file_is_credentials() {
    // Given a `credentials` file.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/credentials"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_detect_as_sensitive_when_path_is_git_directory() {
    // Given a path that traverses a `.git` directory.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/.git/config"));

    // Then it is flagged as sensitive because of the `.git` component.
    assert!(sensitive);
}

#[test]
fn should_pass_access_check_when_file_is_normal_in_workspace() {
    // Given a normal source file inside the workspace.
    let access = workspace_access();

    // When checking access.
    let result = access.check_access(&PathBuf::from("/workspace/src/main.rs"));

    // Then access is granted.
    assert!(result.is_ok());
}

#[test]
fn should_fail_access_check_when_file_is_outside_workspace() {
    // Given a file outside the workspace.
    let access = workspace_access();

    // When checking access.
    let result = access.check_access(&PathBuf::from("/etc/passwd"));

    // Then access is denied.
    assert!(result.is_err());
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("outside the workspace"),
        "unexpected error message: {message}"
    );
}

#[test]
fn should_fail_access_check_when_file_is_sensitive_in_workspace() {
    // Given a sensitive `.env` file inside the workspace.
    let access = workspace_access();

    // When checking access.
    let result = access.check_access(&PathBuf::from("/workspace/.env"));

    // Then access is denied.
    assert!(result.is_err());
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("sensitive file"),
        "unexpected error message: {message}"
    );
}

}

mod process_tests {
//! Integration tests for `mirri-os` process execution.

use std::collections::HashMap;

use mirri_os::{ExecOptions, LocalProcess, OsProcess};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn should_return_exit_code_0_when_running_echo_command() {
    // Given: a LocalProcess and the system `echo` command
    let process = LocalProcess::new();
    let opts = ExecOptions::default();

    // When: executing `echo hello`
    let mut child = process.exec("/bin/echo", &["hello"], &opts).await.unwrap();

    // Then: the process should exit with code 0
    let code = child.wait().await.unwrap();
    assert_eq!(code, 0);
}

#[tokio::test]
async fn should_capture_stdout_when_command_produces_output() {
    // Given: a LocalProcess ready to run echo
    let process = LocalProcess::new();
    let opts = ExecOptions::default();

    // When: executing `echo hello world`
    let mut child = process
        .exec("/bin/echo", &["hello", "world"], &opts)
        .await
        .unwrap();

    let mut output = String::new();
    if let Some(stdout) = child.stdout() {
        use tokio::io::AsyncReadExt;
        stdout.read_to_string(&mut output).await.unwrap();
    }
    let _code = child.wait().await.unwrap();

    // Then: stdout should contain the echoed text
    assert_eq!(output.trim(), "hello world");
}

#[tokio::test]
async fn should_see_env_var_when_exec_with_custom_env() {
    // Given: a LocalProcess and a custom environment variable
    let process = LocalProcess::new();
    let mut env = HashMap::new();
    env.insert("MIRRI_TEST_VAR".to_string(), "secret-value".to_string());
    let opts = ExecOptions::default();

    // When: running `env` with the custom env set
    let mut child = process
        .exec_with_env("/usr/bin/env", &[], env, &opts)
        .await
        .unwrap();

    let mut output = String::new();
    if let Some(stdout) = child.stdout() {
        use tokio::io::AsyncReadExt;
        stdout.read_to_string(&mut output).await.unwrap();
    }
    let _code = child.wait().await.unwrap();

    // Then: the child environment should contain our variable
    assert!(
        output.contains("MIRRI_TEST_VAR=secret-value"),
        "expected MIRRI_TEST_VAR in env output, got: {output}"
    );
}

#[tokio::test]
async fn should_run_in_correct_directory_when_cwd_is_set() {
    // Given: a temp directory as cwd
    let tmp = TempDir::new().unwrap();
    let process = LocalProcess::new();
    let opts = ExecOptions {
        cwd: Some(tmp.path().to_path_buf()),
        ..Default::default()
    };

    // When: running `pwd` with that cwd
    let mut child = process.exec("/bin/pwd", &[], &opts).await.unwrap();

    let mut output = String::new();
    if let Some(stdout) = child.stdout() {
        use tokio::io::AsyncReadExt;
        stdout.read_to_string(&mut output).await.unwrap();
    }
    let _code = child.wait().await.unwrap();

    // Then: pwd should report the temp directory (resolve symlinks for macOS /tmp)
    let canonical_tmp = std::fs::canonicalize(tmp.path()).unwrap();
    let canonical_pwd = std::fs::canonicalize(output.trim())
        .unwrap_or_else(|_| std::path::PathBuf::from(output.trim()));
    assert_eq!(canonical_pwd, canonical_tmp);
}

#[tokio::test]
async fn should_terminate_when_kill_is_called() {
    // Given: a long-running `sleep 30` process
    let process = LocalProcess::new();
    let opts = ExecOptions::default();
    let mut child = process.exec("/bin/sleep", &["30"], &opts).await.unwrap();
    // When: killing the process
    child.kill().await.unwrap();

    // Then: the process should no longer be alive. We verify by checking that
    // wait returns quickly (kill already reaped it or wait reaps it now).
    let code = child.wait().await.unwrap();
    // A signal-killed process has no exit code, so we expect -1.
    assert_eq!(code, -1);

    // Confirm the PID is no longer a live process (kill(pid, 0) should
    // fail). Unix-only: the liveness re-check rides the nix signal probe.
    #[cfg(unix)]
    {
        let pid = child.pid();
        let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
        let result = nix::sys::signal::kill(nix_pid, None);
        assert!(result.is_err(), "process {pid} should be dead after kill");
    }
}

#[tokio::test]
async fn should_echo_input_when_piping_to_stdin() {
    // Given: a `cat` process with piped stdin
    let process = LocalProcess::new();
    let opts = ExecOptions::default();
    let mut child = process.exec("/bin/cat", &[], &opts).await.unwrap();

    // When: writing to stdin then closing it
    if let Some(stdin) = child.stdin() {
        stdin.write_all(b"piped-content\n").await.unwrap();
    }
    // Drop stdin so cat sees EOF.
    child.take_stdin();

    let mut output = String::new();
    if let Some(stdout) = child.stdout() {
        use tokio::io::AsyncReadExt;
        stdout.read_to_string(&mut output).await.unwrap();
    }
    let _code = child.wait().await.unwrap();

    // Then: cat should echo back what we wrote
    assert_eq!(output, "piped-content\n");
}

#[tokio::test]
async fn should_capture_stderr_when_command_writes_to_stderr() {
    // Given: a LocalProcess ready to run a command that writes to stderr.
    // Using `sh -c 'echo err >&2'` to write to stderr.
    let process = LocalProcess::new();
    let opts = ExecOptions::default();

    // When: executing a command that writes to stderr
    let mut child = process
        .exec("/bin/sh", &["-c", "echo err-msg >&2"], &opts)
        .await
        .unwrap();

    let mut stderr_output = String::new();
    if let Some(stderr) = child.stderr() {
        use tokio::io::AsyncReadExt;
        stderr.read_to_string(&mut stderr_output).await.unwrap();
    }
    let _code = child.wait().await.unwrap();

    // Then: stderr should contain the message
    assert_eq!(stderr_output.trim(), "err-msg");
}

}

mod search_tests {
//! Integration tests for `mirri-os` content search (ripgrep).

use std::fs;
use std::io::Write;

use mirri_os::{ContentSearch, RipgrepSearch, SearchParams};
use tempfile::TempDir;

/// Return `true` if the `rg` binary is available on PATH.
fn rg_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_ok()
}

/// Helper: create a temp dir and write a file with the given name and contents.
fn write_file(dir: &TempDir, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut f = fs::File::create(&path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
    path
}

#[tokio::test]
async fn should_return_matches_when_searching_directory_with_matching_content() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }

    // Given: a temp dir with a file containing "hello world"
    let dir = TempDir::new().unwrap();
    write_file(&dir, "test.txt", "hello world\n");

    let params = SearchParams {
        pattern: "hello".to_string(),
        paths: vec![dir.path().to_path_buf()],
        output_mode: "content".to_string(),
        ..SearchParams::default()
    };

    // When: searching for "hello"
    let result = RipgrepSearch::new().search(&params).await.unwrap();

    // Then: exactly one match is returned with the correct path and line
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.total_matches, 1);
    assert!(!result.truncated);
    let m = &result.matches[0];
    assert!(m.path.ends_with("test.txt"));
    assert!(m.line.contains("hello"));
    assert_eq!(m.line_number, 1);
}

#[tokio::test]
async fn should_match_different_cases_when_case_insensitive_is_true() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }

    // Given: a file containing "Hello World" (capitalised)
    let dir = TempDir::new().unwrap();
    write_file(&dir, "test.txt", "Hello World\n");

    let params = SearchParams {
        pattern: "hello".to_string(),
        paths: vec![dir.path().to_path_buf()],
        case_insensitive: true,
        output_mode: "content".to_string(),
        ..SearchParams::default()
    };

    // When: searching case-insensitively
    let result = RipgrepSearch::new().search(&params).await.unwrap();

    // Then: the match is found despite case differences
    assert_eq!(result.matches.len(), 1);
    assert!(result.matches[0].line.contains("Hello"));
}

#[tokio::test]
async fn should_only_search_matching_files_when_glob_filter_is_set() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }

    // Given: two files — a.ts and b.rs — both containing "pattern"
    let dir = TempDir::new().unwrap();
    write_file(&dir, "a.ts", "pattern\n");
    write_file(&dir, "b.rs", "pattern\n");

    let params = SearchParams {
        pattern: "pattern".to_string(),
        paths: vec![dir.path().to_path_buf()],
        glob: Some("*.ts".to_string()),
        output_mode: "content".to_string(),
        ..SearchParams::default()
    };

    // When: searching with glob "*.ts"
    let result = RipgrepSearch::new().search(&params).await.unwrap();

    // Then: only a.ts is returned
    assert_eq!(result.matches.len(), 1);
    assert!(result.matches[0].path.ends_with("a.ts"));
}

#[tokio::test]
async fn should_return_file_paths_only_when_searching_in_files_mode() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }

    // Given: multiple files containing the search term
    let dir = TempDir::new().unwrap();
    write_file(&dir, "a.txt", "search_term\n");
    write_file(&dir, "b.txt", "search_term\n");

    let params = SearchParams {
        pattern: "search_term".to_string(),
        paths: vec![dir.path().to_path_buf()],
        output_mode: "files".to_string(),
        ..SearchParams::default()
    };

    // When: searching in files mode
    let result = RipgrepSearch::new().search(&params).await.unwrap();

    // Then: matches have paths but line_number is 0
    assert_eq!(result.matches.len(), 2);
    for m in &result.matches {
        assert_eq!(m.line_number, 0);
        assert!(m.line.is_empty());
        assert!(m.path.ends_with("a.txt") || m.path.ends_with("b.txt"));
    }
}

#[tokio::test]
async fn should_return_counts_when_searching_in_count_mode() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }

    // Given: a file with 3 occurrences of the pattern
    let dir = TempDir::new().unwrap();
    write_file(&dir, "test.txt", "match\nmatch\nmatch\n");

    let params = SearchParams {
        pattern: "match".to_string(),
        paths: vec![dir.path().to_path_buf()],
        output_mode: "count".to_string(),
        ..SearchParams::default()
    };

    // When: searching in count mode
    let result = RipgrepSearch::new().search(&params).await.unwrap();

    // Then: line_number carries the count (3)
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].line_number, 3);
}

#[tokio::test]
async fn should_set_truncated_when_results_exceed_head_limit() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }

    // Given: 5 files each containing one match
    let dir = TempDir::new().unwrap();
    for i in 0..5 {
        write_file(&dir, &format!("file{i}.txt"), "target\n");
    }

    let params = SearchParams {
        pattern: "target".to_string(),
        paths: vec![dir.path().to_path_buf()],
        output_mode: "content".to_string(),
        head_limit: 2,
        ..SearchParams::default()
    };

    // When: searching with head_limit=2
    let result = RipgrepSearch::new().search(&params).await.unwrap();

    // Then: only 2 matches are returned, total is 5, and truncated is true
    assert_eq!(result.matches.len(), 2);
    assert_eq!(result.total_matches, 5);
    assert!(result.truncated);
}

#[tokio::test]
async fn should_filter_out_sensitive_files_when_searching() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }

    // Given: a .env file and a normal file both containing the search term
    let dir = TempDir::new().unwrap();
    write_file(&dir, ".env", "search_term\n");
    write_file(&dir, "normal.txt", "search_term\n");

    let params = SearchParams {
        pattern: "search_term".to_string(),
        paths: vec![dir.path().to_path_buf()],
        output_mode: "content".to_string(),
        // Include hidden files so rg actually searches .env — the filtering
        // happens at the result layer via PathAccess.
        include_ignored: true,
        ..SearchParams::default()
    };

    // When: searching for "search_term"
    let result = RipgrepSearch::new().search(&params).await.unwrap();

    // Then: only normal.txt appears in the results; .env is filtered out
    let paths: Vec<&str> = result.matches.iter().map(|m| m.path.as_str()).collect();
    assert!(
        !paths.iter().any(|p| p.ends_with(".env")),
        "sensitive .env should be filtered out, got {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p.ends_with("normal.txt")),
        "normal.txt should be present, got {paths:?}"
    );
}

#[tokio::test]
async fn should_find_matches_when_a_separator_glob_targets_the_search_root_itself() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }

    // Given: a fixture with matches inside a subdirectory, searched with
    // a `/`-bearing glob whose common-ancestor pinning makes the search
    // root its own ancestor (the rg search path must degrade to "." — the
    // cwd equals the ancestor — not an empty-string argv element)
    let dir = TempDir::new().unwrap();
    write_file(&dir, "sub/nested/a.txt", "needle\n");
    write_file(&dir, "sub/nested/b.txt", "needle\n");
    write_file(&dir, "top.txt", "needle\n");

    let params = SearchParams {
        pattern: "needle".to_string(),
        paths: vec![dir.path().to_path_buf()],
        glob: Some("sub/**/*.txt".to_string()),
        output_mode: "content".to_string(),
        ..SearchParams::default()
    };

    // When: searching with glob "sub/**/*.txt" and the root as its own
    // common ancestor
    let result = RipgrepSearch::new().search(&params).await.unwrap();

    // Then: the subdirectory matches are found (rg received a valid search
    // path, not an empty-string argv that makes it exit 2) and top.txt is
    // filtered out by the glob
    let paths: Vec<&str> = result
        .matches
        .iter()
        .filter(|m| m.is_match)
        .map(|m| m.path.as_str())
        .collect();
    assert_eq!(paths.len(), 2, "matches were: {paths:?}");
    assert!(
        paths.iter().all(|p| !p.ends_with("top.txt")),
        "top.txt must be filtered out by the glob: {paths:?}"
    );
}

#[tokio::test]
async fn should_search_all_paths_and_group_in_path_array_order() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }

    // Given: three files — b.txt and a.rs both match, c.rs does not
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "needle\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "needle\n").unwrap();
    std::fs::write(tmp.path().join("c.rs"), "nope\n").unwrap();
    let params = SearchParams {
        pattern: "needle".to_string(),
        // And the paths array intentionally lists b.txt before a.rs
        paths: vec![tmp.path().join("b.txt"), tmp.path().join("a.rs")],
        output_mode: "content".to_string(),
        ..Default::default()
    };

    // When the search runs three times (rg's multi-path output order is
    // nondeterministic — the loop guards against flaky regressions)
    for _ in 0..3 {
        let result = RipgrepSearch::new().search(&params).await.unwrap();

        // Then matches are grouped in paths-array order: b.txt first, a.rs
        // second
        assert_eq!(result.matches.len(), 2);
        assert!(
            result.matches[0].path.ends_with("b.txt"),
            "group order must follow the paths array: {:?}",
            result.matches
        );
        assert!(
            result.matches[1].path.ends_with("a.rs"),
            "{:?}",
            result.matches
        );
    }
}

}

mod windows_probe_semantics {
//! Windows real-machine `probe_process` semantics verification (verification
//! checklist group C supplement, `docs/specs/todos/2026-10-31-windows-verification-and-changeset-review.md`,
//! issue #191).
//!
//! The whole crate compiles to nothing on non-Windows hosts (`#![cfg(windows)]`),
//! so `cargo test --workspace` on macOS/Linux neither compiles nor runs any of
//! this. On a real Windows machine the suite runs and *is* the checklist
//! verification: it exercises the real Win32 probe implementation
//! (`OpenProcess` + `GetExitCodeProcess` + `GetProcessTimes`) on real
//! processes, covering the pid-liveness check the ACP desktop discovery
//! (checklist C1) depends on.

#![cfg(windows)]

use std::process::Stdio;
use std::time::{Duration, Instant};

use mirri_os::{new_command, probe_process};

/// Given a running console child, when probed by pid, then it reports alive
/// with readable start ticks — `GetProcessTimes` succeeded and the
/// CreationTime identity is available for fingerprint comparison.
#[test]
fn should_report_a_live_process_alive_with_start_ticks() {
    let mut child = new_command("cmd")
        .args(["/C", "ping", "-n", "10", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cmd /C ping");
    let pid = child.id() as i32;

    let probe = probe_process(pid).expect("probe must not error on a live child");
    assert!(probe.alive, "a running child must probe alive");
    assert!(
        probe.start_ticks.is_some(),
        "GetProcessTimes must supply the creation time for a live own-child process"
    );

    // A same-process probe must return a stable identity: the start ticks of
    // one process cannot change between probes.
    let again = probe_process(pid).expect("second probe");
    assert!(again.alive);
    assert_eq!(
        again.start_ticks, probe.start_ticks,
        "start ticks must be stable across probes of the same process"
    );

    // Cleanup.
    let _ = child.kill();
    let _ = child.wait();
}

/// Given a child that has already exited, when its pid is probed, then it
/// reports not alive — `OpenProcess` with the no-such-pid failure (or a
/// settled exit code), matching the desktop discovery's fail-closed read.
#[test]
fn should_report_a_dead_pid_as_not_alive() {
    let mut child = new_command("cmd")
        .args(["/C", "exit", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cmd /C exit 0");
    let pid = child.id() as i32;
    let _ = child.wait();

    // After a wait the process object is settled; on Windows the exit is
    // immediate, but poll briefly anyway so a busy test machine cannot make
    // this a race between exit and kernel bookkeeping.
    let start = Instant::now();
    let mut probe = probe_process(pid).expect("probe must not error on a dead pid");
    while probe.alive && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
        probe = probe_process(pid).expect("probe must not error on a dead pid");
    }
    assert!(
        !probe.alive,
        "an exited child's pid must not probe alive (STILL_ACTIVE ambiguity direction: dead)"
    );
}

}

mod windows_pty_cleanup {
//! Windows real-machine PTY process-tree cleanup verification (verification
//! checklist group B, `docs/specs/todos/2026-10-31-windows-verification-and-changeset-review.md`,
//! issue #189).
//!
//! The whole crate compiles to nothing on non-Windows hosts (`#![cfg(windows)]`),
//! so `cargo test --workspace` on macOS/Linux neither compiles nor runs any of
//! this. On a real Windows machine the suite runs and *is* the checklist
//! verification.
//!
//! Checklist mapping:
//! - B3 (terminal_close kills the `ping`/`node` grandchildren too, via the
//!   Job Object tree kill): this test drives the exact mechanism — a
//!   grandchild spawned inside the PTY shell must die with `pty.kill()`.

#![cfg(windows)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use mirri_os::{PtyConfig, PtyTerminal, probe_process};

/// Poll `predicate` until it returns `Some`, or `deadline` elapses.
fn wait_for<T>(mut predicate: impl FnMut() -> Option<T>, deadline: Duration) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Some(value) = predicate() {
            return Some(value);
        }
        if start.elapsed() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Given a running terminal whose shell spawned a grandchild, when the
/// terminal is killed, then the grandchild dies too — the whole process tree
/// is cleaned up, not just the direct PTY child.
///
/// The grandchild is a `powershell` that writes its own pid to a temp file
/// and then sleeps, so the test can observe a process that is *not* the
/// direct PTY child. This covers the production mechanism end to end: the
/// Job Object tree kill should make the assertion pass within milliseconds;
/// the toolhelp32 fallback (should the job path be unavailable) also kills
/// the grandchild, just through the snapshot walk — both paths must satisfy
/// this test, with the generous 10 s deadline absorbing the slowest one.
#[test]
fn should_kill_the_whole_grandchild_tree_when_the_terminal_closes() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let pid_file: PathBuf = temp_dir.path().join("grandchild.pid");

    // A ConPTY shell: `cmd.exe` reading commands from the PTY master.
    let config = PtyConfig {
        program: "cmd.exe".to_string(),
        args: Vec::new(),
        cwd: std::env::temp_dir(),
        env: Vec::new(),
        cols: 80,
        rows: 24,
    };
    let pty = PtyTerminal::spawn(&config).expect("spawn cmd.exe PTY");

    // Give the shell a moment to be ready, then have it start the
    // grandchild: powershell emits its own pid to the file and then sleeps
    // for 120 s, both inside one powershell process (`;` keeps the sleep in
    // the same process rather than spawning an extra shell).
    let pid_file_arg = pid_file
        .to_str()
        .expect("temp path is valid UTF-8")
        .replace('\\', "\\\\");
    let command = format!(
        "powershell -NoProfile -Command \"$PID | Out-File -Encoding ascii '{pid_file_arg}'; Start-Sleep -Seconds 120\"\r\n"
    );
    pty.write(command.as_bytes())
        .expect("write the grandchild command to the PTY");

    // The grandchild's pid materializes once powershell runs. This may take
    // several seconds (powershell startup) — poll the file.
    let grandchild_pid: i32 = wait_for(
        || {
            std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|content| content.trim().parse::<i32>().ok())
        },
        Duration::from_secs(15),
    )
    .expect("the grandchild must write its pid within 15 s");

    // Precondition: the grandchild is genuinely alive — the assertion below
    // must not pass merely because the process never started.
    let probe = probe_process(grandchild_pid).expect("probe the grandchild");
    assert!(probe.alive, "the grandchild must be alive before the kill");
    // Sanity: it is not the direct PTY child's pid — we probed a real
    // grandchild, not the shell itself. (The direct child's pid is not on
    // the public API, so the guard is that the pid came from the file the
    // powershell grandchild wrote, which is by construction not the shell.)

    // When: the terminal closes.
    pty.kill().expect("kill the PTY (terminal_close)");

    // Then: the whole tree, grandchild included, is gone. The Job Object
    // path kills it within milliseconds; the fallback within a snapshot
    // walk. A 10 s deadline absorbs either path plus scheduler noise.
    let died = wait_for(
        || {
            probe_process(grandchild_pid)
                .ok()
                .filter(|probe| !probe.alive)
                .map(|_| ())
        },
        Duration::from_secs(10),
    );
    assert!(
        died.is_some(),
        "the grandchild (pid {grandchild_pid}) must be dead within 10 s of terminal close"
    );
}

}

mod windows_spawn_hygiene {
//! Windows real-machine spawn-hygiene verification (verification checklist
//! group A, `docs/specs/todos/2026-10-31-windows-verification-and-changeset-review.md`).
//!
//! The whole crate compiles to nothing on non-Windows hosts (`#![cfg(windows)]`),
//! so `cargo test --workspace` on macOS/Linux neither compiles nor runs any of
//! this. On a real Windows machine the suite runs and *is* the checklist
//! verification.
//!
//! Checklist mapping:
//! - A5/A6 (MCP child processes, git child processes flash no console): the
//!   production spawns go through `mirri_os::new_command`
//!   (`CREATE_NO_WINDOW`); this test verifies that behavior at the source.

#![cfg(windows)]

use std::cell::RefCell;
use std::process::Stdio;
use std::time::Duration;

use mirri_os::{new_command, probe_process};
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
};

thread_local! {
    /// `(owning pid, hwnd)` of every visible top-level window seen by the
    /// most recent enumeration pass. EnumWindows has no user payload for
    /// plain-FFI callers, so the callback reports through this thread-local.
    static VISIBLE_WINDOWS: RefCell<Vec<(u32, isize)>> = const { RefCell::new(Vec::new()) };
}

/// `EnumWindows` callback: record every visible top-level window together
/// with its owning pid.
unsafe extern "system" fn collect_visible_window_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    // SAFETY (both calls): plain Win32 queries on a hwnd handed to us by
    // EnumWindows; the pid out-pointer is a valid local.
    if unsafe { IsWindowVisible(hwnd) } != 0 {
        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        VISIBLE_WINDOWS.with(|windows| windows.borrow_mut().push((pid, hwnd as isize)));
    }
    // Keep enumerating.
    1
}

/// Enumerate all visible top-level windows on the current interactive
/// desktop and return the handles owned by `pid`.
fn visible_windows_belonging_to(pid: u32) -> Vec<isize> {
    VISIBLE_WINDOWS.with(|windows| windows.borrow_mut().clear());
    // SAFETY: the callback only touches the thread-local vec owned by this
    // thread; no borrows are held across the call.
    unsafe { EnumWindows(Some(collect_visible_window_proc), 0) };
    VISIBLE_WINDOWS.with(|windows| {
        windows
            .borrow()
            .iter()
            .filter(|(owner, _)| *owner == pid)
            .map(|(_, hwnd)| *hwnd)
            .collect()
    })
}

/// Given a child spawned through the production `new_command` wrapper, when
/// the child is running, then it owns no visible top-level window on the
/// interactive desktop — a `CREATE_NO_WINDOW` child never flashes a console.
///
/// Trade-off — no control group: a raw `std::process::Command` control
/// spawn would NOT reliably create a window either. `cargo test` (and any
/// console-attached harness) already owns a console, and console children
/// inherit it instead of allocating their own, so a control child would
/// vacuously pass. Only the positive assertion is made here; it carries the
/// verification weight exactly in the production shape that matters — a
/// GUI-host process spawning through `new_command` must leave no visible
/// window behind, which is what this asserts for a console-subsystem child.
#[test]
fn should_spawn_children_without_a_visible_console_window() {
    // A console-subsystem child that stays alive long enough to observe.
    let mut child = new_command("cmd")
        .args(["/C", "ping", "-n", "10", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cmd /C ping through new_command");
    let pid = child.id();

    // Give the child a moment to (potentially) create a window before
    // asserting none exists. A console window, if one were created, appears
    // within a few scheduler quanta of process start.
    std::thread::sleep(Duration::from_millis(750));

    // Sanity: the child is genuinely running, so the absence of a window
    // cannot be explained by the child having already exited.
    let probe = probe_process(pid as i32).expect("probe the child");
    assert!(probe.alive, "the child must still be alive when enumerated");

    let windows = visible_windows_belonging_to(pid);
    assert!(
        windows.is_empty(),
        "a new_command child must own no visible top-level window, found: {windows:?}"
    );

    // Cleanup: never leak the child.
    let _ = child.kill();
    let _ = child.wait();
}

}

