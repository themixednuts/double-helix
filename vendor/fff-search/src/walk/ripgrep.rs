use crate::ignore::non_git_repo_overrides;
use crate::scan_options::{FilePickerScanOptions, is_supported_entry};
use crate::types::FileItem;
use crate::walk::WalkOutput;
use crate::watch::is_git_file;
use ignore::WalkBuilder;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tracing::instrument(skip_all, name = "ripgrep walker", level = "info")]
pub(crate) fn walk_collect_files(
    base_path: &Path,
    is_git_repo: bool,
    scan_options: &FilePickerScanOptions,
    symlink_scope_root: &Path,
    threads: usize,
    synced_files_count: &Arc<AtomicUsize>,
) -> crate::Result<WalkOutput> {
    // FFF_SCAN_OPTIONS_BLOCKER: Helix scan options replace upstream's
    // hard-coded `.hidden(!is_git_repo)` / all-ignore-sources-on policy.
    let mut walk_builder = WalkBuilder::new(base_path);
    walk_builder
        .hidden(scan_options.hidden)
        .parents(scan_options.parents)
        .git_ignore(scan_options.git_ignore)
        .git_exclude(scan_options.git_exclude)
        .git_global(scan_options.git_global)
        .ignore(scan_options.ignore)
        .follow_links(scan_options.follow_links)
        .max_depth(scan_options.max_depth)
        .threads(threads);

    for ignore_file in scan_options.custom_ignore_files.iter() {
        walk_builder.add_custom_ignore_filename(ignore_file);
    }

    if !is_git_repo
        && scan_options.ignore
        && let Some(overrides) = non_git_repo_overrides(base_path)
    {
        walk_builder.overrides(overrides);
    }

    // Symlink scope/dedup is measured against the indexed root, which differs
    // from `base_path` when the watcher walks a newly created subdirectory.
    let filter_root = symlink_scope_root.to_path_buf();
    let deduplicate_links = scan_options.deduplicate_links;
    let symlink_target_scope = scan_options.symlink_target_scope;
    walk_builder.filter_entry(move |entry| {
        is_supported_entry(
            entry.path(),
            &filter_root,
            deduplicate_links,
            symlink_target_scope,
            entry.path_is_symlink(),
        )
    });

    let walker = walk_builder.build_parallel();

    // Single lock for both collections: every entry is either a file or a
    // dir, so this keeps one mutex acquisition per entry.
    let collected =
        parking_lot::Mutex::new((Vec::<(FileItem, String)>::new(), Vec::<String>::new()));
    walker.run(|| {
        let collected = &collected;
        let counter = Arc::clone(synced_files_count);
        let base_path = base_path.to_path_buf();

        Box::new(move |result| {
            let Ok(entry) = result else {
                return ignore::WalkState::Continue;
            };

            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                let path = entry.path();

                // Ignore walkers sometimes surface files inside `.git/`
                // when the base is itself a git repo — skip them.
                if is_git_file(path) {
                    return ignore::WalkState::Continue;
                }

                let metadata = entry.metadata().ok();
                let (file_item, rel_path) =
                    FileItem::new_from_walk(path, &base_path, None, metadata.as_ref());

                collected.lock().0.push((file_item, rel_path));
                counter.fetch_add(1, Ordering::Relaxed);
            } else if entry.depth() > 0 && entry.file_type().is_some_and(|ft| ft.is_dir()) {
                let path = entry.path();
                if !is_git_file(path)
                    && let Ok(rel) = path.strip_prefix(&base_path)
                {
                    let mut rel = crate::path_utils::to_canonical_slashes(&rel.to_string_lossy())
                        .into_owned();
                    rel.push('/');
                    collected.lock().1.push(rel);
                }
            }
            ignore::WalkState::Continue
        })
    });

    let (pairs, dirs) = collected.into_inner();
    Ok(WalkOutput {
        pairs,
        dirs,
        ignore_rules: None,
    })
}
