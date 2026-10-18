use mirri_persistence::AtomicDocument;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct TestState {
    version: u32,
    name: String,
}

#[tokio::test]
async fn should_return_same_data_when_writing_and_reading_back() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("state.json");

    let state = TestState {
        version: 1,
        name: "test".to_string(),
    };
    AtomicDocument::write(&path, &state).await.unwrap();

    let read: TestState = AtomicDocument::read(&path).await.unwrap().unwrap();
    assert_eq!(read, state);
}

#[tokio::test]
async fn should_return_none_when_reading_nonexistent_file() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("nonexistent.json");
    let result: Option<TestState> = AtomicDocument::read(&path).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn should_replace_existing_content_when_writing_to_existing_file() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("state.json");

    let v1 = TestState {
        version: 1,
        name: "v1".to_string(),
    };
    AtomicDocument::write(&path, &v1).await.unwrap();

    let v2 = TestState {
        version: 2,
        name: "v2".to_string(),
    };
    AtomicDocument::write(&path, &v2).await.unwrap();

    let read: TestState = AtomicDocument::read(&path).await.unwrap().unwrap();
    assert_eq!(read, v2);
}

#[tokio::test]
async fn should_not_corrupt_existing_file_when_temp_write_is_abandoned_before_rename() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("state.json");

    // Write initial state.
    let initial = TestState {
        version: 1,
        name: "initial".to_string(),
    };
    AtomicDocument::write(&path, &initial).await.unwrap();

    // Simulate crash: write a temp file but don't rename.
    let tmp_path = path.with_extension("json.tmp");
    tokio::fs::write(&tmp_path, b"partial").await.unwrap();

    // Original should still be intact.
    let read: TestState = AtomicDocument::read(&path).await.unwrap().unwrap();
    assert_eq!(read, initial);
}

#[tokio::test]
async fn should_create_parent_directories_when_writing_to_nested_path() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("nested/dir/state.json");

    let state = TestState {
        version: 1,
        name: "nested".to_string(),
    };
    AtomicDocument::write(&path, &state).await.unwrap();

    let read: TestState = AtomicDocument::read(&path).await.unwrap().unwrap();
    assert_eq!(read, state);
}

#[tokio::test]
async fn should_remove_temp_file_when_write_completes_successfully() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("state.json");
    let tmp_path = path.with_extension("json.tmp");

    let state = TestState {
        version: 1,
        name: "test".to_string(),
    };
    AtomicDocument::write(&path, &state).await.unwrap();

    // Temp file should not exist after successful write.
    assert!(
        !tmp_path.exists(),
        "temp file should be cleaned up after rename"
    );
    // Main file should exist.
    assert!(path.exists(), "main file should exist");
}

#[tokio::test]
async fn should_return_none_when_reading_empty_file() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("empty.json");
    tokio::fs::write(&path, b"").await.unwrap();

    let result: Option<TestState> = AtomicDocument::read(&path).await.unwrap();
    assert!(result.is_none());
}
