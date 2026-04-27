use std::path::PathBuf;

use crate::{Document, Editor};

impl Editor {
    pub fn buffer_label(&self, doc: &Document) -> String {
        let scratch = PathBuf::from(crate::document::SCRATCH_BUFFER_NAME);

        if doc.path().is_none() {
            let scratch_docs: Vec<_> = self
                .documents()
                .filter(|candidate| candidate.path().is_none())
                .map(|candidate| candidate.id)
                .collect();
            if scratch_docs.len() > 1 {
                if let Some(index) = scratch_docs.iter().position(|id| *id == doc.id) {
                    let ordinal = index + 1;
                    return crate::document::SCRATCH_BUFFER_NAME
                        .strip_suffix(']')
                        .map(|prefix| format!("{prefix} {ordinal}]"))
                        .unwrap_or_else(|| {
                            format!("{} {ordinal}", crate::document::SCRATCH_BUFFER_NAME)
                        });
                }
            }
        }

        let paths: Vec<String> = self
            .documents()
            .map(|doc| {
                doc.path()
                    .unwrap_or(&scratch)
                    .to_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();

        let components: Vec<Vec<String>> = paths
            .iter()
            .map(|path| {
                path.split(std::path::MAIN_SEPARATOR)
                    .map(String::from)
                    .collect()
            })
            .collect();

        let doc_path = doc
            .path()
            .unwrap_or(&scratch)
            .to_str()
            .unwrap_or_default()
            .to_string();
        let doc_index = paths.iter().position(|path| path == &doc_path).unwrap_or(0);
        let doc_components_len = components[doc_index].len();

        let mut suffix_len = 1;
        loop {
            let start = doc_components_len.saturating_sub(suffix_len);
            let current = &components[doc_index][start..];

            let conflicts = components
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != doc_index)
                .filter(|(_, parts)| {
                    let start = parts.len().saturating_sub(suffix_len);
                    &parts[start..] == current
                })
                .count();

            if conflicts == 0 {
                return current.join(std::path::MAIN_SEPARATOR_STR);
            }

            suffix_len += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{editor::test_support, Document};

    #[test]
    fn buffer_label_disambiguates_matching_suffixes() {
        let mut editor = test_support::collab_test_editor();

        let first = editor.new_document(Document::default(
            editor.config.clone(),
            editor.syn_loader.clone(),
        ));
        let second = editor.new_document(Document::default(
            editor.config.clone(),
            editor.syn_loader.clone(),
        ));

        editor
            .document_mut(first)
            .expect("first")
            .set_path(Some(std::path::Path::new("alpha/src/main.rs")));
        editor
            .document_mut(second)
            .expect("second")
            .set_path(Some(std::path::Path::new("beta/src/main.rs")));

        let first_label = editor.buffer_label(editor.document(first).expect("first doc"));
        let second_label = editor.buffer_label(editor.document(second).expect("second doc"));

        assert_eq!(
            first_label,
            std::path::PathBuf::from_iter(["alpha", "src", "main.rs"])
                .display()
                .to_string()
        );
        assert_eq!(
            second_label,
            std::path::PathBuf::from_iter(["beta", "src", "main.rs"])
                .display()
                .to_string()
        );
    }

    #[test]
    fn buffer_label_numbers_multiple_scratch_buffers() {
        let mut editor = test_support::collab_test_editor();

        let extra = editor.new_document(Document::default(
            editor.config.clone(),
            editor.syn_loader.clone(),
        ));

        let focused_doc = editor.tree.get(editor.tree.focus).doc;
        let first_label = editor.buffer_label(editor.document(focused_doc).expect("focused doc"));
        let second_label = editor.buffer_label(editor.document(extra).expect("extra doc"));

        assert!(first_label.contains('1'));
        assert!(second_label.contains('2'));
    }
}
