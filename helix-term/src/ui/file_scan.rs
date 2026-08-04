use std::path::{Path, PathBuf};

pub(crate) fn configure_walk(
    builder: &mut ignore::WalkBuilder,
    options: helix_workspace::ScanOptions,
) {
    builder
        .hidden(options.hidden)
        .parents(options.parents)
        .ignore(options.ignore)
        .git_ignore(options.git_ignore)
        .git_global(options.git_global)
        .git_exclude(options.git_exclude)
        .follow_links(options.follow_symlinks)
        .max_depth(options.max_depth.map(|depth| depth as usize));
}

pub(crate) fn filter_picker_entry(
    entry: &ignore::DirEntry,
    root: &Path,
    deduplicate_symlinks: bool,
) -> bool {
    if matches!(
        entry.file_name().to_str(),
        Some(".git" | ".pijul" | ".jj" | ".hg" | ".svn")
    ) {
        return false;
    }

    if deduplicate_symlinks && entry.path_is_symlink() {
        return entry
            .path()
            .canonicalize()
            .ok()
            .is_some_and(|path| !path.starts_with(root));
    }

    true
}

pub(crate) fn excluded_types() -> ignore::types::Types {
    use ignore::types::TypesBuilder;

    let mut types = TypesBuilder::new();
    types
        .add(
            "compressed",
            "*.{zip,gz,bz2,zst,lzo,sz,tgz,tbz2,lz,lz4,lzma,lzo,z,Z,xz,7z,rar,cab}",
        )
        .expect("invalid compressed file type definition");
    types.negate("all");
    types.build().expect("failed to build excluded file types")
}

pub(crate) fn single_child_directory(path: &Path) -> Option<PathBuf> {
    let mut entries = path.read_dir().ok()?;
    let entry = entries.next()?.ok()?;
    let path = entry.path();
    (entries.next().is_none() && path.is_dir()).then_some(path)
}
