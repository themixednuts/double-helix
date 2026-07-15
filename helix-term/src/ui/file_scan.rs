use std::path::{Path, PathBuf};

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
