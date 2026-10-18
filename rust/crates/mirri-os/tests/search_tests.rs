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
        path: dir.path().to_path_buf(),
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
        path: dir.path().to_path_buf(),
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
        path: dir.path().to_path_buf(),
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
async fn should_return_file_paths_only_when_searching_in_files_with_matches_mode() {
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
        path: dir.path().to_path_buf(),
        output_mode: "files_with_matches".to_string(),
        ..SearchParams::default()
    };

    // When: searching in files_with_matches mode
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
async fn should_return_counts_when_searching_in_count_matches_mode() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }

    // Given: a file with 3 occurrences of the pattern
    let dir = TempDir::new().unwrap();
    write_file(&dir, "test.txt", "match\nmatch\nmatch\n");

    let params = SearchParams {
        pattern: "match".to_string(),
        path: dir.path().to_path_buf(),
        output_mode: "count_matches".to_string(),
        ..SearchParams::default()
    };

    // When: searching in count_matches mode
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
        path: dir.path().to_path_buf(),
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
        path: dir.path().to_path_buf(),
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
