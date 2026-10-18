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
