use anyhow::{anyhow, Error};
use helix_core::encoding::Encoding;
use once_cell::sync::OnceCell;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use url::Url;

#[derive(Debug)]
pub struct FileBoundState {
    path: Option<PathBuf>,
    relative_path: OnceCell<Option<PathBuf>>,
    encoding: &'static Encoding,
    has_bom: bool,
    last_saved_time: SystemTime,
    last_saved_revision: usize,
    readonly: bool,
}

impl FileBoundState {
    pub fn new(encoding: &'static Encoding, has_bom: bool) -> Self {
        Self {
            path: None,
            relative_path: OnceCell::new(),
            encoding,
            has_bom,
            last_saved_time: SystemTime::now(),
            last_saved_revision: 0,
            readonly: false,
        }
    }

    pub fn clear_relative_path(&mut self) {
        self.relative_path.take();
    }

    pub fn set_encoding(&mut self, label: &str) -> Result<(), Error> {
        let encoding =
            Encoding::for_label(label.as_bytes()).ok_or_else(|| anyhow!("unknown encoding"))?;
        self.encoding = encoding;
        Ok(())
    }

    pub fn encoding(&self) -> &'static Encoding {
        self.encoding
    }

    pub fn encoding_with_bom_info(&self) -> (&'static Encoding, bool) {
        (self.encoding, self.has_bom)
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    pub fn set_path(&mut self, path: Option<&Path>) {
        self.path = path.map(helix_stdx::path::canonicalize);
        self.clear_relative_path();
        self.detect_readonly();
        self.pickup_last_saved_time();
    }

    pub fn url(&self) -> Option<Url> {
        Url::from_file_path(self.path()?).ok()
    }

    pub fn uri(&self) -> Option<helix_core::Uri> {
        Some(self.path()?.clone().into())
    }

    pub fn relative_path(&self) -> Option<&Path> {
        self.relative_path
            .get_or_init(|| {
                self.path
                    .as_ref()
                    .map(|path| helix_stdx::path::get_relative_path(path).to_path_buf())
            })
            .as_deref()
    }

    pub fn display_name<'a>(&'a self, scratch_name: &'a str) -> Cow<'a, str> {
        self.relative_path()
            .map_or_else(|| scratch_name.into(), |path| path.to_string_lossy())
    }

    pub fn pickup_last_saved_time(&mut self) {
        self.last_saved_time = match self.path() {
            Some(path) => match path.metadata() {
                Ok(metadata) => match metadata.modified() {
                    Ok(mtime) => mtime,
                    Err(err) => {
                        log::debug!(
                            "Could not fetch file system's mtime, falling back to current system time: {}",
                            err
                        );
                        SystemTime::now()
                    }
                },
                Err(err) => {
                    log::debug!(
                        "Could not fetch file system's mtime, falling back to current system time: {}",
                        err
                    );
                    SystemTime::now()
                }
            },
            None => SystemTime::now(),
        };
    }

    pub fn last_saved_time(&self) -> SystemTime {
        self.last_saved_time
    }

    pub fn detect_readonly(&mut self) {
        self.readonly = match self.path() {
            None => false,
            Some(path) => helix_stdx::faccess::readonly(path),
        };
    }

    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn is_modified(&self, current_revision: usize, has_pending_changes: bool) -> bool {
        current_revision != self.last_saved_revision || has_pending_changes
    }

    pub fn reset_modified(&mut self, current_revision: usize) {
        self.last_saved_revision = current_revision;
    }

    pub fn set_last_saved_revision(&mut self, rev: usize, save_time: SystemTime) {
        self.last_saved_revision = rev;
        self.last_saved_time = save_time;
    }

    pub fn last_saved_revision(&self) -> usize {
        self.last_saved_revision
    }
}
