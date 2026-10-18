//! Integration tests for the indexed query store.
//!
//! Verifies the [`IQueryStore`] contract against the in-memory
//! implementation: put/get round-trips, deletion, prefixed listing,
//! atomic batches, checkpoint persistence, and value overwrite
//! semantics.

use mirri_persistence::{IQueryStore, InMemoryQueryStore, QueryCheckpoint, WriteOp};
use serde_json::json;

/// Helper: build an empty in-memory query store.
fn make_store() -> InMemoryQueryStore {
    InMemoryQueryStore::new()
}

#[test]
fn should_store_and_retrieve_value_when_put_and_get_called() {
    let store = make_store();
    let value = json!({"role": "user", "content": "hello"});

    store.put("messages", "msg-1", value.clone()).unwrap();

    let retrieved = store.get("messages", "msg-1").unwrap();
    assert_eq!(retrieved, Some(value));
}

#[test]
fn should_return_none_when_key_does_not_exist() {
    let store = make_store();

    let missing_key = store.get("messages", "no-such-key").unwrap();
    assert_eq!(missing_key, None);

    let missing_collection = store.get("no-such-collection", "msg-1").unwrap();
    assert_eq!(missing_collection, None);
}

#[test]
fn should_delete_value_when_delete_called() {
    let store = make_store();
    store
        .put("messages", "msg-1", json!({"content": "hello"}))
        .unwrap();

    store.delete("messages", "msg-1").unwrap();

    let retrieved = store.get("messages", "msg-1").unwrap();
    assert_eq!(retrieved, None);
}

#[test]
fn should_list_keys_filtered_by_prefix_when_prefix_provided() {
    let store = make_store();
    store.put("sessions", "session-1", json!({})).unwrap();
    store.put("sessions", "session-2", json!({})).unwrap();
    store.put("sessions", "summary-1", json!({})).unwrap();

    let all_keys = store.list("sessions", None).unwrap();
    assert_eq!(
        all_keys,
        vec!["session-1", "session-2", "summary-1"],
        "keys should be listed in lexicographic order"
    );

    let prefixed_keys = store.list("sessions", Some("session-")).unwrap();
    assert_eq!(prefixed_keys, vec!["session-1", "session-2"]);

    let no_match = store.list("sessions", Some("nope-")).unwrap();
    assert!(no_match.is_empty());

    let unknown_collection = store.list("no-such-collection", None).unwrap();
    assert!(unknown_collection.is_empty());
}

#[test]
fn should_execute_batch_atomically_when_batch_called() {
    let store = make_store();
    store
        .put("messages", "msg-keep", json!({"content": "keep me"}))
        .unwrap();

    let ops = vec![
        WriteOp::Put {
            collection: "messages".to_string(),
            key: "msg-a".to_string(),
            value: json!({"content": "batch a"}),
        },
        WriteOp::Put {
            collection: "messages".to_string(),
            key: "msg-b".to_string(),
            value: json!({"content": "batch b"}),
        },
        WriteOp::Delete {
            collection: "messages".to_string(),
            key: "msg-keep".to_string(),
        },
    ];
    store.batch(&ops).unwrap();

    // Every operation in the batch took effect.
    assert_eq!(
        store.get("messages", "msg-a").unwrap(),
        Some(json!({"content": "batch a"}))
    );
    assert_eq!(
        store.get("messages", "msg-b").unwrap(),
        Some(json!({"content": "batch b"}))
    );
    assert_eq!(store.get("messages", "msg-keep").unwrap(), None);

    // An empty batch is a harmless no-op.
    store.batch(&[]).unwrap();
    assert_eq!(
        store.get("messages", "msg-a").unwrap(),
        Some(json!({"content": "batch a"}))
    );
}

#[test]
fn should_persist_checkpoint_when_set_checkpoint_called() {
    let store = make_store();
    let checkpoint = QueryCheckpoint { seq: 42 };

    store
        .set_checkpoint("wire-log", checkpoint.clone())
        .unwrap();

    let retrieved = store.get_checkpoint("wire-log").unwrap();
    assert_eq!(retrieved, Some(checkpoint));
}

#[test]
fn should_return_none_when_checkpoint_not_set() {
    let store = make_store();

    let checkpoint = store.get_checkpoint("wire-log").unwrap();
    assert_eq!(checkpoint, None);
}

#[test]
fn should_overwrite_value_when_same_key_put_twice() {
    let store = make_store();
    store
        .put("messages", "msg-1", json!({"version": 1}))
        .unwrap();

    store
        .put("messages", "msg-1", json!({"version": 2}))
        .unwrap();

    let retrieved = store.get("messages", "msg-1").unwrap();
    assert_eq!(retrieved, Some(json!({"version": 2})), "latest put wins");
}
