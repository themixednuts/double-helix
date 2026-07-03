use std::sync::{Arc, Barrier};
use std::thread;

use helix_store::{AssistantThread, FrecencyEntry, PkgReceipt, Store, StorePaths};

#[test]
fn migrations_wal_and_drizzle_round_trips() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = paths(temp.path());
    let mut store = Store::open(paths).expect("open store");

    assert_eq!(
        store.state_journal_mode().unwrap().to_ascii_lowercase(),
        "wal"
    );
    assert_eq!(
        store.cache_journal_mode().unwrap().to_ascii_lowercase(),
        "wal"
    );

    let thread = AssistantThread {
        id: "thread-1".to_owned(),
        scope: "workspace-a".to_owned(),
        title: Some("First".to_owned()),
        created_at: 10,
        updated_at: 20,
        rating: Some("up".to_owned()),
        has_feedback: true,
        record_json: r#"{"id":"thread-1"}"#.to_owned(),
    };
    store.threads().upsert(thread.clone()).unwrap();
    assert_eq!(
        store.threads().get("thread-1").unwrap(),
        Some(thread.clone())
    );

    let updated_thread = AssistantThread {
        title: None,
        updated_at: 30,
        rating: None,
        has_feedback: false,
        record_json: r#"{"id":"thread-1","updated":true}"#.to_owned(),
        ..thread
    };
    store.threads().upsert(updated_thread.clone()).unwrap();
    assert_eq!(
        store.threads().list_by_scope("workspace-a").unwrap(),
        vec![updated_thread.clone()]
    );
    store.threads().delete("thread-1").unwrap();
    assert_eq!(store.threads().get("thread-1").unwrap(), None);

    let frecency = FrecencyEntry {
        workspace: "workspace-a".to_owned(),
        path_hash: "hash-1".to_owned(),
        first_accessed_at: 100,
        last_accessed_at: 100,
        access_count: 1,
        timestamps_json: "[100]".to_owned(),
    };
    store.frecency().upsert(frecency.clone()).unwrap();
    assert_eq!(
        store.frecency().get("workspace-a", "hash-1").unwrap(),
        Some(frecency)
    );
    store.frecency().bump("workspace-a", "hash-1", 200).unwrap();
    let updated_frecency = store
        .frecency()
        .get("workspace-a", "hash-1")
        .unwrap()
        .unwrap();
    assert_eq!(updated_frecency.access_count, 2);
    assert_eq!(updated_frecency.last_accessed_at, 200);
    assert_eq!(updated_frecency.timestamps_json, "[100,200]");
    store.frecency().delete("workspace-a", "hash-1").unwrap();
    assert_eq!(store.frecency().get("workspace-a", "hash-1").unwrap(), None);

    let receipt = PkgReceipt {
        kind: "lsp".to_owned(),
        name: "demo".to_owned(),
        version: "1.0.0".to_owned(),
        source: "archive".to_owned(),
        hash: "abc".to_owned(),
        bin: "demo".to_owned(),
        shim: "demo.cmd".to_owned(),
        files_json: r#"{"demo.exe":"abc"}"#.to_owned(),
        installed_at: "2026-07-03T00:00:00Z".to_owned(),
        native_manager: None,
        native_id: None,
        receipt_json: r#"{"name":"demo"}"#.to_owned(),
    };
    store.receipts().upsert(receipt.clone()).unwrap();
    assert_eq!(store.receipts().all().unwrap(), vec![receipt.clone()]);
    assert_eq!(
        store.receipts().get("lsp", "demo").unwrap(),
        Some(receipt.clone())
    );

    let updated_receipt = PkgReceipt {
        version: "1.0.1".to_owned(),
        hash: "def".to_owned(),
        files_json: r#"{"demo.exe":"def"}"#.to_owned(),
        receipt_json: r#"{"name":"demo","version":"1.0.1"}"#.to_owned(),
        ..receipt
    };
    store.receipts().upsert(updated_receipt.clone()).unwrap();
    assert_eq!(
        store.receipts().all().unwrap(),
        vec![updated_receipt.clone()]
    );
    assert!(store
        .receipts()
        .import_once("pkg-test-marker", &[updated_receipt])
        .unwrap());
    assert!(store
        .receipts()
        .import_marker_exists("pkg-test-marker")
        .unwrap());
    store.receipts().delete("lsp", "demo").unwrap();
    assert!(store.receipts().all().unwrap().is_empty());
}

#[test]
fn concurrent_writer_connections_do_not_corrupt_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = paths(temp.path());
    Store::open(paths.clone()).expect("initial open");

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for writer in 0..2 {
        let paths = paths.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let mut store = Store::open(paths).expect("open writer store");
            barrier.wait();
            for index in 0..50 {
                store
                    .threads()
                    .upsert(AssistantThread {
                        id: format!("thread-{writer}-{index}"),
                        scope: "shared".to_owned(),
                        title: Some(format!("Writer {writer}")),
                        created_at: index,
                        updated_at: index,
                        rating: None,
                        has_feedback: false,
                        record_json: format!(r#"{{"writer":{writer},"index":{index}}}"#),
                    })
                    .expect("thread upsert");
            }
        }));
    }

    for handle in handles {
        handle.join().expect("writer thread");
    }

    let mut store = Store::open(paths).expect("reopen store");
    let rows = store.threads().list_by_scope("shared").unwrap();
    assert_eq!(rows.len(), 100);
}

fn paths(root: &std::path::Path) -> StorePaths {
    StorePaths::new(root.join("state.sqlite3"), root.join("cache.sqlite3"))
}
