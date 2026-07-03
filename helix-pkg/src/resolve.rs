use std::{env, path::PathBuf};

use crate::{spec::PkgKind, store::Store, Result};

pub fn binary(store: &Store, _kind: PkgKind, name: &str) -> Option<PathBuf> {
    let bin = store.bin_dir();
    let candidates = if cfg!(windows) {
        vec![
            bin.join(format!("{name}.exe")),
            bin.join(format!("{name}.cmd")),
            bin.join(format!("{name}.bat")),
            bin.join(name),
        ]
    } else {
        vec![bin.join(name)]
    };
    candidates
        .into_iter()
        .find(|path| path.exists())
        .or_else(|| which(name))
}

fn which(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths).find_map(|dir| {
        let path = dir.join(name);
        if path.exists() {
            return Some(path);
        }
        if cfg!(windows) {
            for ext in ["exe", "cmd", "bat"] {
                let path = dir.join(format!("{name}.{ext}"));
                if path.exists() {
                    return Some(path);
                }
            }
        }
        None
    })
}

pub fn system_binary(name: &str) -> Result<PathBuf> {
    which(name).ok_or_else(|| crate::Error::SystemMissing(name.to_owned()))
}
