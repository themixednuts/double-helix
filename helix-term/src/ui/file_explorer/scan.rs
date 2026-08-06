use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use helix_view::editor::FileExplorerConfig;

use super::path_ops::display_path;
use super::ExplorerPath;

#[derive(Clone, Debug)]
pub(super) struct ExplorerChild {
    pub(super) path: ExplorerPath,
    pub(super) is_dir: bool,
}

pub(super) struct DirectoryScanner<'a> {
    explorer: &'a FileExplorerConfig,
    excluded_types: ignore::types::Types,
    config_ignore: PathBuf,
    workspace_ignore: &'static str,
}

impl<'a> DirectoryScanner<'a> {
    pub(super) fn new(explorer: &'a FileExplorerConfig) -> Self {
        Self {
            explorer,
            excluded_types: crate::ui::file_scan::excluded_types(),
            config_ignore: helix_loader::config_dir().join("ignore"),
            workspace_ignore: helix_loader::workspace_ignore_file_name(),
        }
    }

    pub(super) fn children(&self, root: &Path) -> Result<Vec<ExplorerChild>, std::io::Error> {
        use ignore::WalkBuilder;

        let start = Instant::now();
        let scan = self.explorer.workspace_directory_options().scan;
        let mut walk_builder = WalkBuilder::new(root);
        crate::ui::file_scan::configure_walk(&mut walk_builder, scan);
        let mut children: Vec<ExplorerChild> = walk_builder
            .max_depth(Some(1))
            .add_custom_ignore_filename(self.config_ignore.clone())
            .add_custom_ignore_filename(self.workspace_ignore)
            .types(self.excluded_types.clone())
            .build()
            .filter_map(|entry| {
                let child = match entry {
                    Ok(entry) => {
                        let path = entry.path();
                        let is_dir = entry
                            .file_type()
                            .map_or_else(|| path.is_dir(), |file_type| file_type.is_dir());
                        let mut path = path.to_path_buf();
                        if is_dir && path != root && self.explorer.flatten_dirs {
                            while let Some(single_child_directory) =
                                crate::ui::file_scan::single_child_directory(&path)
                            {
                                path = single_child_directory;
                            }
                        }
                        ExplorerChild {
                            path: ExplorerPath::Local(path),
                            is_dir,
                        }
                    }
                    Err(err) => {
                        log::debug!(
                            "failed to read file explorer entry under {}: {err}",
                            helix_stdx::path::display_path(root)
                        );
                        return None;
                    }
                };
                (child.path.local_path() != Some(root)).then_some(child)
            })
            .collect();

        children.sort_by(|child1, child2| {
            (!child1.is_dir, &child1.path).cmp(&(!child2.is_dir, &child2.path))
        });
        log::info!(
            "[file_explorer] directory_scan root={} children={} dirs={} files={} elapsed_us={}",
            display_path(root),
            children.len(),
            children.iter().filter(|child| child.is_dir).count(),
            children.iter().filter(|child| !child.is_dir).count(),
            start.elapsed().as_micros()
        );
        Ok(children)
    }
}
