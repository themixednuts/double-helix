use helix_core::Rope;
use tokio::task::JoinHandle;

use crate::diff::{DiffHandle, Hunk};

impl DiffHandle {
    fn new_test(diff_base: &str, doc: &str) -> (DiffHandle, JoinHandle<()>) {
        let gate = helix_runtime::FrameGate::new();
        DiffHandle::new_with_handle(
            Rope::from_str(diff_base),
            Rope::from_str(doc),
            gate.handle(),
        )
    }
    async fn into_diff(self, handle: JoinHandle<()>) -> Vec<Hunk> {
        let diff = self.diff;
        // dropping the channel terminates the task
        drop(self.channel);
        handle.await.unwrap();
        let diff = diff.load_full();
        Vec::clone(&diff.hunks)
    }
}

#[tokio::test]
async fn append_line() {
    let (differ, handle) = DiffHandle::new_test("foo\n", "foo\nbar\n");
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[Hunk {
            before: 1..1,
            after: 1..2
        }]
    )
}

#[tokio::test]
async fn prepend_line() {
    let (differ, handle) = DiffHandle::new_test("foo\n", "bar\nfoo\n");
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[Hunk {
            before: 0..0,
            after: 0..1
        }]
    )
}

#[tokio::test]
async fn modify() {
    let (differ, handle) = DiffHandle::new_test("foo\nbar\n", "foo bar\nbar\n");
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[Hunk {
            before: 0..1,
            after: 0..1
        }]
    )
}

#[tokio::test]
async fn delete_line() {
    let (differ, handle) = DiffHandle::new_test("foo\nfoo bar\nbar\n", "foo\nbar\n");
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[Hunk {
            before: 1..2,
            after: 1..1
        }]
    )
}

#[tokio::test]
async fn delete_line_and_modify() {
    let (differ, handle) = DiffHandle::new_test("foo\nbar\ntest\nfoo", "foo\ntest\nfoo bar");
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[
            Hunk {
                before: 1..2,
                after: 1..1
            },
            Hunk {
                before: 3..4,
                after: 2..3
            },
        ]
    )
}

#[tokio::test]
async fn add_use() {
    let (differ, handle) = DiffHandle::new_test(
        "use ropey::Rope;\nuse tokio::task::JoinHandle;\n",
        "use ropey::Rope;\nuse ropey::RopeSlice;\nuse tokio::task::JoinHandle;\n",
    );
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[Hunk {
            before: 1..1,
            after: 1..2
        },]
    )
}

#[tokio::test]
async fn update_document() {
    let (differ, handle) = DiffHandle::new_test("foo\nbar\ntest\nfoo", "foo\nbar\ntest\nfoo");
    differ.update_document(Rope::from_str("foo\ntest\nfoo bar"));
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[
            Hunk {
                before: 1..2,
                after: 1..1
            },
            Hunk {
                before: 3..4,
                after: 2..3
            },
        ]
    )
}

#[tokio::test]
async fn update_base() {
    let (differ, handle) = DiffHandle::new_test("foo\ntest\nfoo bar", "foo\ntest\nfoo bar");
    differ.update_diff_base(Rope::from_str("foo\nbar\ntest\nfoo"));
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[
            Hunk {
                before: 1..2,
                after: 1..1
            },
            Hunk {
                before: 3..4,
                after: 2..3
            },
        ]
    )
}

/// Tests that `load()` (the path the gutter uses) sees updates after document changes.
/// This catches stale-cache bugs where the gutter reads an old diff snapshot.
#[tokio::test]
async fn load_reflects_update_after_edit() {
    // Start with identical base and doc — no hunks
    let (differ, handle) = DiffHandle::new_test("line1\nline2\nline3\n", "line1\nline2\nline3\n");

    // Wait for initial diff to be computed (worker processes initial state)
    wait_for_load(
        &differ,
        |hunks| hunks.is_empty(),
        "initial diff should be empty",
    )
    .await;

    // Now edit the document: insert a new line after line1
    differ.update_document(Rope::from_str("line1\nnew_line\nline2\nline3\n"));

    // The gutter calls load() — it should eventually see the insertion hunk
    wait_for_load(
        &differ,
        |hunks| {
            hunks
                == [Hunk {
                    before: 1..1,
                    after: 1..2,
                }]
        },
        "load() should reflect inserted line after update_document",
    )
    .await;

    // Clean up
    drop(differ);
    let _ = handle.await;
}

/// Tests that multiple rapid successive edits all get reflected in load().
/// Simulates the user typing quickly — each keystroke triggers update_document.
#[tokio::test]
async fn load_reflects_rapid_successive_edits() {
    let base = "fn main() {\n    println!(\"hello\");\n}\n";
    let (differ, handle) = DiffHandle::new_test(base, base);

    wait_for_load(
        &differ,
        |hunks| hunks.is_empty(),
        "initial diff should be empty",
    )
    .await;

    // Edit 1: modify the println line
    differ.update_document(Rope::from_str("fn main() {\n    println!(\"world\");\n}\n"));

    wait_for_load(
        &differ,
        |hunks| {
            hunks
                == [Hunk {
                    before: 1..2,
                    after: 1..2,
                }]
        },
        "should see modified line after edit 1",
    )
    .await;

    // Edit 2: add a new line
    differ.update_document(Rope::from_str(
        "fn main() {\n    println!(\"world\");\n    println!(\"extra\");\n}\n",
    ));

    wait_for_load(
        &differ,
        |hunks| {
            // lines 1-2 in base (just println) replaced by lines 1-3 in doc (two printlns)
            hunks
                == [Hunk {
                    before: 1..2,
                    after: 1..3,
                }]
        },
        "should see modified + added lines after edit 2",
    )
    .await;

    // Edit 3: revert to original — hunks should disappear
    differ.update_document(Rope::from_str(base));

    wait_for_load(
        &differ,
        |hunks| hunks.is_empty(),
        "should see no hunks after reverting to original",
    )
    .await;

    drop(differ);
    let _ = handle.await;
}

/// Tests that updating the diff base (e.g. after git commit) correctly updates load().
#[tokio::test]
async fn load_reflects_base_change() {
    let (differ, handle) = DiffHandle::new_test("aaa\nbbb\nccc\n", "aaa\nbbb_modified\nccc\n");

    // Should show line 1 as modified
    wait_for_load(
        &differ,
        |hunks| {
            hunks
                == [Hunk {
                    before: 1..2,
                    after: 1..2,
                }]
        },
        "initial diff should show modified line",
    )
    .await;

    // Now update the base to match the current doc (simulates git commit)
    differ.update_diff_base(Rope::from_str("aaa\nbbb_modified\nccc\n"));

    // After base update, doc matches base — no hunks
    wait_for_load(
        &differ,
        |hunks| hunks.is_empty(),
        "after base update matching doc, should have no hunks",
    )
    .await;

    drop(differ);
    let _ = handle.await;
}

/// Tests that a deletion followed by an insertion produces correct gutter state.
/// This is a common editing pattern (delete line, type new content).
#[tokio::test]
async fn load_reflects_delete_then_insert() {
    let base = "alpha\nbeta\ngamma\ndelta\n";
    let (differ, handle) = DiffHandle::new_test(base, base);

    wait_for_load(&differ, |hunks| hunks.is_empty(), "initial should be clean").await;

    // Delete "beta" line
    differ.update_document(Rope::from_str("alpha\ngamma\ndelta\n"));

    wait_for_load(
        &differ,
        |hunks| {
            hunks
                == [Hunk {
                    before: 1..2,
                    after: 1..1,
                }]
        },
        "should show deletion of beta",
    )
    .await;

    // Now insert "epsilon" where beta was
    differ.update_document(Rope::from_str("alpha\nepsilon\ngamma\ndelta\n"));

    wait_for_load(
        &differ,
        |hunks| {
            // beta replaced by epsilon = modification of line 1
            hunks
                == [Hunk {
                    before: 1..2,
                    after: 1..2,
                }]
        },
        "should show beta→epsilon as modification",
    )
    .await;

    drop(differ);
    let _ = handle.await;
}

/// Tests that load() is coherent when doc and base are both updated in quick succession.
#[tokio::test]
async fn load_coherent_after_simultaneous_base_and_doc_update() {
    let (differ, handle) = DiffHandle::new_test("old\n", "old\nnew_line\n");

    wait_for_load(
        &differ,
        |hunks| {
            hunks
                == [Hunk {
                    before: 1..1,
                    after: 1..2,
                }]
        },
        "initial should show added line",
    )
    .await;

    // Update both base and doc to completely new content
    differ.update_diff_base(Rope::from_str("completely\ndifferent\nbase\n"));
    differ.update_document(Rope::from_str("completely\ndifferent\nbase\nwith extra\n"));

    wait_for_load(
        &differ,
        |hunks| {
            hunks
                == [Hunk {
                    before: 3..3,
                    after: 3..4,
                }]
        },
        "should reflect both base and doc updates",
    )
    .await;

    drop(differ);
    let _ = handle.await;
}

/// Helper: polls `differ.load()` until `predicate` returns true on the hunks,
/// or panics after a timeout with `msg`.
async fn wait_for_load(differ: &DiffHandle, predicate: impl Fn(&[Hunk]) -> bool, msg: &str) {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    loop {
        let diff = differ.load();
        let hunks: Vec<Hunk> = (0..diff.len()).map(|i| diff.nth_hunk(i)).collect();
        if predicate(&hunks) {
            return;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("Timed out waiting for diff state: {msg}\n  actual hunks: {hunks:?}");
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}
