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
