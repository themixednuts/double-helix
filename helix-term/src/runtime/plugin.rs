use std::path::PathBuf;

/// The lines of `new_text` that `changes` touched, `start..end`, sorted and merged.
pub(crate) fn changed_lines(
    changes: &helix_core::ChangeSet,
    new_text: helix_core::RopeSlice,
) -> Vec<(usize, usize)> {
    let mut lines: Vec<(usize, usize)> = Vec::new();
    for (from, _to, inserted) in changes.changes_iter() {
        let start = changes.map_pos(from, helix_core::Assoc::Before);
        let end =
            (start + inserted.map_or(0, |text| text.chars().count())).min(new_text.len_chars());
        let range = (new_text.char_to_line(start), new_text.char_to_line(end) + 1);
        match lines.last_mut() {
            Some(last) if range.0 <= last.1 => last.1 = last.1.max(range.1),
            _ => lines.push(range),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::changed_lines;
    use helix_core::{Rope, Transaction};

    #[test]
    fn changed_lines_cover_each_edit_in_the_new_text() {
        let old = Rope::from("a\nb\nc\nd\n");
        let edit = Transaction::change(
            &old,
            [(2, 3, Some("x\ny".into())), (6, 7, Some("z".into()))].into_iter(),
        );
        let mut new = old.clone();
        edit.apply(&mut new);
        assert_eq!(new, "a\nx\ny\nc\nz\n");
        assert_eq!(
            changed_lines(edit.changes(), new.slice(..)),
            vec![(1, 3), (4, 5)]
        );
    }
}

/// Lightweight editor signal resolved into a typed plugin event on the UI thread.
#[derive(Debug, Clone)]
pub enum PluginNotification {
    BufferOpen {
        document_id: helix_view::DocumentId,
        resource: Option<String>,
    },
    BufferChanged {
        document_id: helix_view::DocumentId,
        version: i32,
        /// Lines the change touched in the new text, `start..end`, sorted and disjoint.
        changed_lines: Vec<(usize, usize)>,
    },
    BufferClosed {
        document_id: helix_view::DocumentId,
    },
    SelectionChange {
        document_id: helix_view::DocumentId,
        path: Option<PathBuf>,
    },
    ModeChange {
        old_mode: String,
        new_mode: String,
    },
    KeyPress {
        key: String,
    },
    LspDiagnostic {
        document_id: helix_view::DocumentId,
        diagnostic_count: usize,
    },
}
