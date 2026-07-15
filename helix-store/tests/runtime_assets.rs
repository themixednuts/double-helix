use std::collections::BTreeMap;

use helix_store::{
    ActivePackage, Error, PackageActivation, PkgReceipt, RegistryHead, RuntimeAsset,
    RuntimeAssetSpec, Store, StorePaths,
};

#[test]
fn activation_rejects_collisions_without_advancing_generation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(paths(temp.path())).expect("open store");
    let first_path = temp.path().join("first");
    let second_path = temp.path().join("second");

    assert_eq!(
        store
            .runtime_assets()
            .activate(activation("lsp", "first", "1", first_path, "shared"))
            .unwrap(),
        1
    );
    let error = store
        .runtime_assets()
        .activate(activation("lsp", "second", "1", second_path, "shared"))
        .unwrap_err();
    assert!(matches!(error, Error::RuntimeAssetCollision { .. }));

    let snapshot = store.runtime_assets().snapshot().unwrap();
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.assets.len(), 1);
    assert_eq!(snapshot.assets[0].package.name, "first");
}

#[test]
fn rollback_and_remove_restore_exact_asset_snapshots() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(paths(temp.path())).expect("open store");
    let package_v1 = ActivePackage::new("lsp", "demo", "1");
    let command_v1 = RuntimeAssetSpec::command("demo", temp.path().join("demo-v1"))
        .with_prefix_args(["--stdio".to_owned()])
        .with_default_args(["--log=warn".to_owned()])
        .with_env([
            ("DEMO_HOME".to_owned(), "v1".to_owned()),
            ("RUST_LOG".to_owned(), "warn".to_owned()),
        ]);
    let file_v1 = RuntimeAssetSpec::file(
        "queries/demo/highlights.scm",
        temp.path().join("highlights-v1.scm"),
    );
    let expected_v1 = vec![
        RuntimeAsset::from_spec(package_v1.clone(), command_v1.clone()),
        RuntimeAsset::from_spec(package_v1.clone(), file_v1.clone()),
    ];
    store
        .runtime_assets()
        .activate(PackageActivation::new(
            package_v1,
            vec![command_v1, file_v1],
        ))
        .unwrap();

    let package_v2 = ActivePackage::new("lsp", "demo", "2");
    let command_v2 = RuntimeAssetSpec::command("demo", temp.path().join("demo-v2"))
        .with_prefix_args(["run".to_owned()])
        .with_default_args(["--log=debug".to_owned()])
        .with_env([("DEMO_HOME".to_owned(), "v2".to_owned())]);
    store
        .runtime_assets()
        .activate(PackageActivation::new(package_v2, vec![command_v2]))
        .unwrap();

    assert_eq!(
        store.runtime_assets().rollback("lsp", "demo").unwrap(),
        Some(3)
    );
    let restored = store.runtime_assets().snapshot().unwrap();
    assert_eq!(restored.generation, 3);
    assert_eq!(restored.assets, expected_v1);

    assert_eq!(store.runtime_assets().remove("lsp", "demo").unwrap(), 4);
    assert!(store.runtime_assets().snapshot().unwrap().assets.is_empty());
    assert_eq!(
        store.runtime_assets().rollback("lsp", "demo").unwrap(),
        Some(5)
    );
    assert_eq!(
        store.runtime_assets().snapshot().unwrap().assets,
        expected_v1
    );

    let history = store.runtime_assets().history("lsp", "demo").unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[1].rolled_back_generation, Some(3));
    assert_eq!(history[2].rolled_back_generation, Some(5));
}

#[test]
fn compatibility_import_and_registry_heads_are_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(paths(temp.path())).expect("open store");
    let imported = activation(
        "formatter",
        "pretty",
        "1",
        temp.path().join("pretty-v1"),
        "pretty",
    );
    let ignored = activation(
        "formatter",
        "pretty",
        "2",
        temp.path().join("pretty-v2"),
        "pretty",
    );

    assert!(store
        .runtime_assets()
        .import_once("runtime-assets-test-v1", &[imported])
        .unwrap());
    assert!(!store
        .runtime_assets()
        .import_once("runtime-assets-test-v1", &[ignored])
        .unwrap());
    assert!(store
        .runtime_assets()
        .import_marker_exists("runtime-assets-test-v1")
        .unwrap());
    let snapshot = store.runtime_assets().snapshot().unwrap();
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.assets[0].package.version, "1");

    let head = RegistryHead {
        registry: "builtin".to_owned(),
        source: "embedded".to_owned(),
        revision: "abc123".to_owned(),
        updated_at: 42,
    };
    store.registry_heads().upsert(head.clone()).unwrap();
    assert_eq!(
        store.registry_heads().get("builtin").unwrap(),
        Some(head.clone())
    );
    assert_eq!(store.registry_heads().all().unwrap(), vec![head]);
    store.registry_heads().delete("builtin").unwrap();
    assert!(store.registry_heads().all().unwrap().is_empty());
}

#[test]
fn aggregate_rollback_returns_exact_before_after_state_and_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(paths(temp.path())).expect("open store");
    let receipt_v1 = receipt("lsp", "demo", "1");
    let receipt_v2 = receipt("lsp", "demo", "2");

    let first = store
        .package_state()
        .activate(
            receipt_v1.clone(),
            activation("lsp", "demo", "1", temp.path().join("demo-v1"), "demo"),
        )
        .unwrap();
    assert!(first.before.receipt.is_none());
    assert!(first.before.assets.is_empty());
    assert_eq!(first.after.receipt, Some(receipt_v1.clone()));
    assert_eq!(first.snapshot.generation, 1);
    assert_eq!(first.snapshot.assets, first.after.assets);

    let second = store
        .package_state()
        .activate(
            receipt_v2.clone(),
            activation("lsp", "demo", "2", temp.path().join("demo-v2"), "demo"),
        )
        .unwrap();
    assert_eq!(second.before.receipt, Some(receipt_v1.clone()));
    assert_eq!(second.after.receipt, Some(receipt_v2.clone()));
    assert_eq!(second.snapshot.generation, 2);

    let rolled_back = store
        .package_state()
        .rollback(receipt_v1.clone())
        .unwrap()
        .expect("rollback target");
    assert_eq!(rolled_back.before.receipt, Some(receipt_v2));
    assert_eq!(rolled_back.before.assets[0].package.version, "2");
    assert_eq!(rolled_back.after.receipt, Some(receipt_v1));
    assert_eq!(rolled_back.after.assets[0].package.version, "1");
    assert_eq!(rolled_back.snapshot.generation, 3);
    assert_eq!(rolled_back.snapshot.assets, rolled_back.after.assets);

    let history = store.runtime_assets().history("lsp", "demo").unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].rolled_back_generation, Some(3));
}

#[test]
fn aggregate_activation_commit_failure_rolls_back_every_package_row() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = paths(temp.path());
    let state_path = paths.state.clone();
    let mut store = Store::open(paths).expect("open store");
    install_deferred_commit_failure(
        &state_path,
        r#"
CREATE TRIGGER fail_package_receipt_insert
AFTER INSERT ON pkg_receipts
BEGIN
    INSERT INTO package_commit_failure_child(parent_id) VALUES (999);
END;
"#,
    );

    let result = store.package_state().activate(
        receipt("lsp", "demo", "1"),
        activation("lsp", "demo", "1", temp.path().join("demo-v1"), "demo"),
    );
    assert!(matches!(result, Err(Error::Sqlite(_))));

    let state = store.package_state().get("lsp", "demo").unwrap();
    assert!(state.receipt.is_none());
    assert!(state.assets.is_empty());
    let snapshot = store.runtime_assets().snapshot().unwrap();
    assert_eq!(snapshot.generation, 0);
    assert!(snapshot.assets.is_empty());
    assert!(store
        .runtime_assets()
        .history("lsp", "demo")
        .unwrap()
        .is_empty());
    assert_eq!(deferred_failure_rows(&state_path), 0);
}

#[test]
fn aggregate_deactivation_commit_failure_preserves_entire_before_image() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = paths(temp.path());
    let state_path = paths.state.clone();
    let mut store = Store::open(paths).expect("open store");
    let receipt = receipt("lsp", "demo", "1");
    store
        .package_state()
        .activate(
            receipt.clone(),
            activation("lsp", "demo", "1", temp.path().join("demo-v1"), "demo"),
        )
        .unwrap();
    install_deferred_commit_failure(
        &state_path,
        r#"
CREATE TRIGGER fail_package_receipt_delete
AFTER DELETE ON pkg_receipts
BEGIN
    INSERT INTO package_commit_failure_child(parent_id) VALUES (999);
END;
"#,
    );

    let result = store.package_state().deactivate("lsp", "demo");
    assert!(matches!(result, Err(Error::Sqlite(_))));

    let state = store.package_state().get("lsp", "demo").unwrap();
    assert_eq!(state.receipt, Some(receipt));
    assert_eq!(state.assets.len(), 1);
    let snapshot = store.runtime_assets().snapshot().unwrap();
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.assets, state.assets);
    let history = store.runtime_assets().history("lsp", "demo").unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].rolled_back_generation, None);
    assert_eq!(deferred_failure_rows(&state_path), 0);
}

#[test]
fn aggregate_rollback_commit_failure_preserves_history_and_current_version() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = paths(temp.path());
    let state_path = paths.state.clone();
    let mut store = Store::open(paths).expect("open store");
    let receipt_v1 = receipt("lsp", "demo", "1");
    let receipt_v2 = receipt("lsp", "demo", "2");
    store
        .package_state()
        .activate(
            receipt_v1.clone(),
            activation("lsp", "demo", "1", temp.path().join("demo-v1"), "demo"),
        )
        .unwrap();
    store
        .package_state()
        .activate(
            receipt_v2.clone(),
            activation("lsp", "demo", "2", temp.path().join("demo-v2"), "demo"),
        )
        .unwrap();
    install_deferred_commit_failure(
        &state_path,
        r#"
CREATE TRIGGER fail_package_receipt_update
AFTER UPDATE ON pkg_receipts
BEGIN
    INSERT INTO package_commit_failure_child(parent_id) VALUES (999);
END;
"#,
    );

    let result = store.package_state().rollback(receipt_v1);
    assert!(matches!(result, Err(Error::Sqlite(_))));

    let state = store.package_state().get("lsp", "demo").unwrap();
    assert_eq!(state.receipt, Some(receipt_v2));
    assert_eq!(state.assets[0].package.version, "2");
    let snapshot = store.runtime_assets().snapshot().unwrap();
    assert_eq!(snapshot.generation, 2);
    assert_eq!(snapshot.assets, state.assets);
    let history = store.runtime_assets().history("lsp", "demo").unwrap();
    assert_eq!(history.len(), 2);
    assert!(history
        .iter()
        .all(|event| event.rolled_back_generation.is_none()));
    assert_eq!(deferred_failure_rows(&state_path), 0);
}

fn activation(
    kind: &str,
    name: &str,
    version: &str,
    path: std::path::PathBuf,
    key: &str,
) -> PackageActivation {
    PackageActivation::new(
        ActivePackage::new(kind, name, version),
        vec![RuntimeAssetSpec {
            kind: helix_store::RuntimeAssetKind::Command,
            key: key.to_owned(),
            path,
            prefix_args: Vec::new(),
            default_args: Vec::new(),
            env: BTreeMap::new(),
        }],
    )
}

fn receipt(kind: &str, name: &str, version: &str) -> PkgReceipt {
    PkgReceipt {
        kind: kind.to_owned(),
        name: name.to_owned(),
        version: version.to_owned(),
        source: "archive".to_owned(),
        hash: format!("hash-{version}"),
        bin: name.to_owned(),
        shim: String::new(),
        files_json: "{}".to_owned(),
        installed_at: "now".to_owned(),
        native_manager: None,
        native_id: None,
        receipt_json: format!(r#"{{"name":"{name}","version":"{version}"}}"#),
    }
}

fn install_deferred_commit_failure(state_path: &std::path::Path, trigger_sql: &str) {
    let connection = rusqlite::Connection::open(state_path).expect("open raw state database");
    connection
        .execute_batch(
            r#"
PRAGMA foreign_keys = ON;
CREATE TABLE package_commit_failure_parent(id INTEGER PRIMARY KEY);
CREATE TABLE package_commit_failure_child(
    parent_id INTEGER NOT NULL,
    FOREIGN KEY(parent_id) REFERENCES package_commit_failure_parent(id)
        DEFERRABLE INITIALLY DEFERRED
);
"#,
        )
        .expect("create deferred failure tables");
    connection
        .execute_batch(trigger_sql)
        .expect("create deferred failure trigger");
}

fn deferred_failure_rows(state_path: &std::path::Path) -> i64 {
    let connection = rusqlite::Connection::open(state_path).expect("open raw state database");
    connection
        .query_row(
            "SELECT COUNT(*) FROM package_commit_failure_child",
            [],
            |row| row.get(0),
        )
        .expect("count deferred failure rows")
}

fn paths(root: &std::path::Path) -> StorePaths {
    StorePaths::new(root.join("state.sqlite3"), root.join("cache.sqlite3"))
}
