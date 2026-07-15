use helix_view::{
    editor::{FileExplorerConfig, FilePickerConfig},
    Editor,
};

use super::{menu, Menu, Popup, PromptEvent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileSourceOption {
    Hidden,
    Ignore,
    GitIgnore,
    GitGlobal,
    GitExclude,
    Parents,
    FollowSymlinks,
    DeduplicateLinks,
    FlattenDirectories,
}

impl FileSourceOption {
    const PICKER: [Self; 8] = [
        Self::Hidden,
        Self::Ignore,
        Self::GitIgnore,
        Self::GitGlobal,
        Self::GitExclude,
        Self::Parents,
        Self::FollowSymlinks,
        Self::DeduplicateLinks,
    ];

    const EXPLORER: [Self; 8] = [
        Self::Hidden,
        Self::Ignore,
        Self::GitIgnore,
        Self::GitGlobal,
        Self::GitExclude,
        Self::Parents,
        Self::FollowSymlinks,
        Self::FlattenDirectories,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Hidden => "Hidden files",
            Self::Ignore => "Ignore files",
            Self::GitIgnore => "Repository .gitignore",
            Self::GitGlobal => "Global Git ignore",
            Self::GitExclude => "Repository Git exclude",
            Self::Parents => "Parent ignore files",
            Self::FollowSymlinks => "Follow symlinks",
            Self::DeduplicateLinks => "Deduplicate symlink targets",
            Self::FlattenDirectories => "Flatten single-child directories",
        }
    }

    pub(crate) fn picker_items(config: &FilePickerConfig) -> Vec<FileSourceOptionItem> {
        Self::PICKER
            .into_iter()
            .map(|option| FileSourceOptionItem {
                option,
                enabled: option.picker_enabled(config),
            })
            .collect()
    }

    pub(crate) fn explorer_items(config: &FileExplorerConfig) -> Vec<FileSourceOptionItem> {
        Self::EXPLORER
            .into_iter()
            .map(|option| FileSourceOptionItem {
                option,
                enabled: option.explorer_enabled(config),
            })
            .collect()
    }

    fn picker_enabled(self, config: &FilePickerConfig) -> bool {
        match self {
            Self::Hidden => config.hidden,
            Self::Ignore => config.ignore,
            Self::GitIgnore => config.git_ignore,
            Self::GitGlobal => config.git_global,
            Self::GitExclude => config.git_exclude,
            Self::Parents => config.parents,
            Self::FollowSymlinks => config.follow_symlinks,
            Self::DeduplicateLinks => config.deduplicate_links,
            Self::FlattenDirectories => false,
        }
    }

    fn explorer_enabled(self, config: &FileExplorerConfig) -> bool {
        match self {
            Self::Hidden => config.hidden,
            Self::Ignore => config.ignore,
            Self::GitIgnore => config.git_ignore,
            Self::GitGlobal => config.git_global,
            Self::GitExclude => config.git_exclude,
            Self::Parents => config.parents,
            Self::FollowSymlinks => config.follow_symlinks,
            Self::FlattenDirectories => config.flatten_dirs,
            Self::DeduplicateLinks => false,
        }
    }

    pub(crate) fn toggle_picker(self, config: &mut FilePickerConfig) {
        let value = !self.picker_enabled(config);
        match self {
            Self::Hidden => config.hidden = value,
            Self::Ignore => config.ignore = value,
            Self::GitIgnore => config.git_ignore = value,
            Self::GitGlobal => config.git_global = value,
            Self::GitExclude => config.git_exclude = value,
            Self::Parents => config.parents = value,
            Self::FollowSymlinks => config.follow_symlinks = value,
            Self::DeduplicateLinks => config.deduplicate_links = value,
            Self::FlattenDirectories => {}
        }
    }

    pub(crate) fn toggle_explorer(self, config: &mut FileExplorerConfig) {
        let value = !self.explorer_enabled(config);
        match self {
            Self::Hidden => config.hidden = value,
            Self::Ignore => config.ignore = value,
            Self::GitIgnore => config.git_ignore = value,
            Self::GitGlobal => config.git_global = value,
            Self::GitExclude => config.git_exclude = value,
            Self::Parents => config.parents = value,
            Self::FollowSymlinks => config.follow_symlinks = value,
            Self::FlattenDirectories => config.flatten_dirs = value,
            Self::DeduplicateLinks => {}
        }
    }
}

pub(crate) struct FileSourceOptionItem {
    option: FileSourceOption,
    enabled: bool,
}

impl menu::Item for FileSourceOptionItem {
    type Data = ();

    fn format(&self, _data: &Self::Data) -> menu::Row<'_> {
        menu::Row::new([
            menu::Cell::from(if self.enabled { "●" } else { "○" }),
            menu::Cell::from(self.option.label()),
        ])
    }
}

pub(crate) fn popup(
    id: &'static str,
    items: Vec<FileSourceOptionItem>,
    toggle: impl Fn(&mut Editor, FileSourceOption) + Send + 'static,
) -> Popup<Menu<FileSourceOptionItem>> {
    let mut menu = Menu::new(items, (), move |editor, item, event| {
        if event == PromptEvent::Validate {
            if let Some(item) = item {
                toggle(editor, item.option);
            }
        }
    });
    menu.move_down();
    Popup::new(id, menu).with_scrollbar(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_options_toggle_every_enumeration_boolean() {
        let mut config = FilePickerConfig::default();
        for option in FileSourceOption::PICKER {
            let before = option.picker_enabled(&config);
            option.toggle_picker(&mut config);
            assert_ne!(option.picker_enabled(&config), before, "{option:?}");
        }
    }

    #[test]
    fn explorer_options_toggle_every_enumeration_boolean() {
        let mut config = FileExplorerConfig::default();
        for option in FileSourceOption::EXPLORER {
            let before = option.explorer_enabled(&config);
            option.toggle_explorer(&mut config);
            assert_ne!(option.explorer_enabled(&config), before, "{option:?}");
        }
    }
}
