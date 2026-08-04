use std::path::{Path, PathBuf};

use assert_fs::{prelude::*, TempDir};

#[derive(Debug)]
pub(crate) struct TempFs {
    temp: TempDir,
    root: PathBuf,
}

impl TempFs {
    pub(crate) fn new() -> Self {
        let temp = TempDir::new().expect("create temp test filesystem");
        let root = helix_stdx::path::normalize(temp.path());
        Self { temp, root }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        helix_stdx::path::normalize(self.temp.child(relative))
    }

    pub(crate) fn dir(&self, relative: impl AsRef<Path>) -> &Self {
        let path = self.temp.child(relative);
        path.create_dir_all()
            .unwrap_or_else(|err| panic!("create test directory {}: {err}", path.display()));
        self
    }

    pub(crate) fn file(&self, relative: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> &Self {
        let path = self.temp.child(relative);
        path.write_binary(contents.as_ref())
            .unwrap_or_else(|err| panic!("write test file {}: {err}", path.display()));
        self
    }

    pub(crate) fn assert_exists(&self, relative: impl AsRef<Path>) {
        self.temp.child(relative).assert(predicates::path::exists());
    }

    pub(crate) fn assert_missing(&self, relative: impl AsRef<Path>) {
        self.temp
            .child(relative)
            .assert(predicates::path::missing());
    }
}
