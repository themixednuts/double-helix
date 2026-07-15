use std::{borrow::Cow, path::Path, sync::Arc};

use helix_core::{
    command_line::{self, Tokenizer},
    fuzzy::fuzzy_match,
    syntax::config::LanguageServerFeature,
};
use helix_view::{document::SCRATCH_BUFFER_NAME, Editor, Theme};
use once_cell::sync::Lazy;
use tui::text::Span;

use super::prompt::{
    completion::{self, CompletionRequest, FileIndexKey},
    Completion, CompletionProvider,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Completer {
    None,
    Buffer,
    Theme,
    ActiveLanguageServers,
    ConfiguredLanguageServers,
    Setting,
    Filename { git_ignore: bool },
    Language,
    LspWorkspaceCommand,
    Directory { git_ignore: bool },
    Register,
    Program,
    RepeatingFilenames,
    Shell,
    FoldableTextobjects,
}

#[allow(non_upper_case_globals)]
pub const none: Completer = Completer::None;
#[allow(non_upper_case_globals)]
pub const buffer: Completer = Completer::Buffer;
#[allow(non_upper_case_globals)]
pub const theme: Completer = Completer::Theme;
#[allow(non_upper_case_globals)]
pub const active_language_servers: Completer = Completer::ActiveLanguageServers;
#[allow(non_upper_case_globals)]
pub const configured_language_servers: Completer = Completer::ConfiguredLanguageServers;
#[allow(non_upper_case_globals)]
pub const setting: Completer = Completer::Setting;
#[allow(non_upper_case_globals)]
pub const filename: Completer = Completer::Filename { git_ignore: true };
#[allow(non_upper_case_globals)]
pub const language: Completer = Completer::Language;
#[allow(non_upper_case_globals)]
pub const lsp_workspace_command: Completer = Completer::LspWorkspaceCommand;
#[allow(non_upper_case_globals)]
pub const directory: Completer = Completer::Directory { git_ignore: true };
#[allow(non_upper_case_globals)]
pub const register: Completer = Completer::Register;
#[allow(non_upper_case_globals)]
pub const program: Completer = Completer::Program;
#[allow(non_upper_case_globals)]
pub const repeating_filenames: Completer = Completer::RepeatingFilenames;
#[allow(non_upper_case_globals)]
pub const shell: Completer = Completer::Shell;
#[allow(non_upper_case_globals)]
pub const foldable_textobjects: Completer = Completer::FoldableTextobjects;

pub const fn filename_with_git_ignore(git_ignore: bool) -> Completer {
    Completer::Filename { git_ignore }
}

pub const fn directory_with_git_ignore(git_ignore: bool) -> Completer {
    Completer::Directory { git_ignore }
}

#[derive(Clone)]
pub(crate) enum CompleterSnapshot {
    Empty,
    Names(Arc<[String]>),
    Theme(Arc<Theme>),
    Syntax(Arc<helix_core::syntax::Loader>),
}

impl Completer {
    pub(crate) fn capture_snapshot(self, editor: &Editor) -> CompleterSnapshot {
        match self {
            Self::Buffer => CompleterSnapshot::Names(
                editor
                    .documents
                    .values()
                    .map(|doc| {
                        doc.relative_path()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| SCRATCH_BUFFER_NAME.to_owned())
                    })
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Self::ActiveLanguageServers => CompleterSnapshot::Names(
                focused_ref!(editor)
                    .1
                    .language_servers()
                    .map(|server| server.name().to_owned())
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Self::ConfiguredLanguageServers => CompleterSnapshot::Names(
                focused_ref!(editor)
                    .1
                    .language_config()
                    .into_iter()
                    .flat_map(|config| &config.language_servers)
                    .map(|server| server.name.clone())
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Self::Language => CompleterSnapshot::Syntax(editor.syn_loader.load_full()),
            Self::LspWorkspaceCommand => CompleterSnapshot::Names(
                focused_ref!(editor)
                    .1
                    .language_servers_with_feature(LanguageServerFeature::WorkspaceCommand)
                    .flat_map(|server| {
                        server
                            .capabilities()
                            .execute_command_provider
                            .iter()
                            .flat_map(|options| options.commands.iter())
                    })
                    .cloned()
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Self::Filename { .. }
            | Self::Directory { .. }
            | Self::RepeatingFilenames
            | Self::Shell => CompleterSnapshot::Theme(editor.theme.clone()),
            Self::Register => CompleterSnapshot::Names(
                editor
                    .registers
                    .iter_preview()
                    .filter(|(name, _)| !matches!(name, '%' | '#' | '.'))
                    .map(|(name, _)| name.to_string())
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Self::None
            | Self::Theme
            | Self::Setting
            | Self::Program
            | Self::FoldableTextobjects => CompleterSnapshot::Empty,
        }
    }

    pub(crate) fn complete(self, snapshot: &CompleterSnapshot, input: &str) -> Vec<Completion> {
        match self {
            Self::None => Vec::new(),
            Self::Buffer
            | Self::ActiveLanguageServers
            | Self::ConfiguredLanguageServers
            | Self::Register => complete_names(snapshot, input, false),
            Self::Theme => completion::theme_names()
                .map(|names| fuzzy_names(input, names.iter(), false))
                .unwrap_or_default(),
            Self::Setting => {
                static KEYS: Lazy<Arc<[String]>> = Lazy::new(|| {
                    let mut keys = Vec::new();
                    get_keys(
                        &serde_json::json!(helix_view::editor::Config::default()),
                        &mut keys,
                        None,
                    );
                    keys.into()
                });
                fuzzy_names(input, KEYS.iter(), false)
            }
            Self::Filename { git_ignore } => {
                filename_impl(snapshot, input, git_ignore, FileTarget::File)
            }
            Self::Directory { git_ignore } => {
                filename_impl(snapshot, input, git_ignore, FileTarget::Directory)
            }
            Self::Language => {
                let CompleterSnapshot::Syntax(loader) = snapshot else {
                    return Vec::new();
                };
                let text = "text".to_owned();
                fuzzy_names(
                    input,
                    loader
                        .language_configs()
                        .map(|config| &config.language_id)
                        .chain(std::iter::once(&text)),
                    false,
                )
            }
            Self::LspWorkspaceCommand => complete_names(snapshot, input, false),
            Self::Program => {
                let Some(programs) = completion::program_names() else {
                    return Vec::new();
                };
                let mut managed = helix_loader::runtime_assets_if_initialized()
                    .map(|assets| assets.command_keys())
                    .unwrap_or_default();
                managed.retain(|command| programs.binary_search(command).is_err());
                fuzzy_names(input, programs.iter().chain(managed.iter()), false)
            }
            Self::RepeatingFilenames => complete_repeating_filenames(snapshot, input),
            Self::Shell => complete_shell(snapshot, input),
            Self::FoldableTextobjects => fuzzy_names(
                input,
                [
                    "class",
                    "function",
                    "comment",
                    "test",
                    "conditional",
                    "loop",
                ],
                false,
            ),
        }
    }
}

impl CompletionProvider for Completer {
    fn capture(&mut self, editor: &Editor, input: Arc<str>) -> CompletionRequest {
        let completer = *self;
        let snapshot = completer.capture_snapshot(editor);
        CompletionRequest::new(move || completer.complete(&snapshot, &input))
    }
}

#[derive(Clone, Copy)]
pub enum CandidateMatch {
    Prefix,
    Fuzzy,
}

pub struct CandidateCompleter {
    candidates: Arc<[String]>,
    matching: CandidateMatch,
}

impl CandidateCompleter {
    pub fn prefix(candidates: impl Into<Arc<[String]>>) -> Self {
        Self {
            candidates: candidates.into(),
            matching: CandidateMatch::Prefix,
        }
    }

    pub fn fuzzy(candidates: impl Into<Arc<[String]>>) -> Self {
        Self {
            candidates: candidates.into(),
            matching: CandidateMatch::Fuzzy,
        }
    }
}

impl CompletionProvider for CandidateCompleter {
    fn capture(&mut self, _editor: &Editor, input: Arc<str>) -> CompletionRequest {
        let candidates = self.candidates.clone();
        let matching = self.matching;
        CompletionRequest::new(move || match matching {
            CandidateMatch::Prefix => candidates
                .iter()
                .filter(|candidate| candidate.starts_with(input.as_ref()))
                .map(|candidate| (0.., candidate.clone().into()))
                .collect(),
            CandidateMatch::Fuzzy => fuzzy_names(&input, candidates.iter(), false),
        })
    }
}

fn complete_names(snapshot: &CompleterSnapshot, input: &str, use_paths: bool) -> Vec<Completion> {
    let CompleterSnapshot::Names(names) = snapshot else {
        return Vec::new();
    };
    fuzzy_names(input, names.iter(), use_paths)
}

fn fuzzy_names<I, T>(input: &str, names: I, use_paths: bool) -> Vec<Completion>
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    fuzzy_match(input, names, use_paths)
        .into_iter()
        .map(|(name, _)| ((0..), name.as_ref().to_owned().into()))
        .collect()
}

fn get_keys(value: &serde_json::Value, keys: &mut Vec<String>, scope: Option<&str>) {
    if let Some(map) = value.as_object() {
        for (key, value) in map {
            let key = scope.map_or_else(|| key.clone(), |scope| format!("{scope}.{key}"));
            get_keys(value, keys, Some(&key));
            if !value.is_object() {
                keys.push(key);
            }
        }
    }
}

#[derive(Clone)]
struct Utf8PathBuf {
    path: String,
    is_dir: bool,
    is_symlink: bool,
}

impl AsRef<str> for Utf8PathBuf {
    fn as_ref(&self) -> &str {
        &self.path
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileMatch {
    Reject,
    AcceptIncomplete,
    Accept,
}

#[derive(Clone, Copy)]
enum FileTarget {
    File,
    Directory,
}

fn filename_impl(
    snapshot: &CompleterSnapshot,
    input: &str,
    git_ignore: bool,
    target: FileTarget,
) -> Vec<Completion> {
    let CompleterSnapshot::Theme(theme_snapshot) = snapshot else {
        return Vec::new();
    };
    let is_tilde = input == "~";
    let path = helix_stdx::path::expand_tilde(Path::new(input));
    #[cfg(windows)]
    if path.is_absolute() {
        return Vec::new();
    }

    let (base_directory, file_name) = if input.ends_with(std::path::MAIN_SEPARATOR) {
        (path.into_owned(), None)
    } else {
        let is_period = (input.ends_with(format!("{}.", std::path::MAIN_SEPARATOR).as_str())
            && input.len() > 2)
            || input == ".";
        let file_name = if is_period {
            Some(".".to_owned())
        } else {
            path.file_name()
                .and_then(|file| file.to_str().map(str::to_owned))
        };
        let base_directory = if is_period {
            path
        } else {
            match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => Cow::Borrowed(parent),
                _ => Cow::Borrowed(Path::new(".")),
            }
        };
        (base_directory.into_owned(), file_name)
    };

    let Some(entries) = completion::file_entries(FileIndexKey {
        directory: base_directory.clone(),
        git_ignore,
    }) else {
        return Vec::new();
    };
    let end = input.len()..;
    let files = entries.iter().filter_map(|entry| {
        let matched = match target {
            FileTarget::File if entry.is_dir => FileMatch::AcceptIncomplete,
            FileTarget::File => FileMatch::Accept,
            FileTarget::Directory if entry.is_dir => FileMatch::Accept,
            FileTarget::Directory => FileMatch::Reject,
        };
        if matched == FileMatch::Reject {
            return None;
        }
        let mut path = if is_tilde {
            entry.path.clone()
        } else {
            entry
                .path
                .strip_prefix(&base_directory)
                .unwrap_or(&entry.path)
                .to_path_buf()
        };
        if matched == FileMatch::AcceptIncomplete {
            path.push("");
        }
        let path = path.into_os_string().into_string().ok()?;
        (!path.is_empty()).then_some(Utf8PathBuf {
            path,
            is_dir: entry.is_dir,
            is_symlink: entry.is_symlink,
        })
    });

    let directory_style = theme_snapshot.get("ui.text.directory");
    let symlink_style = theme_snapshot.get("ui.text.symlink");
    let style = |file: Utf8PathBuf| {
        if file.is_symlink {
            Span::styled(file.path, symlink_style)
        } else if file.is_dir {
            Span::styled(file.path, directory_style)
        } else {
            Span::raw(file.path)
        }
    };

    if let Some(file_name) = file_name {
        let range = input.len().saturating_sub(file_name.len())..;
        fuzzy_match(&file_name, files, true)
            .into_iter()
            .map(|(file, _)| (range.clone(), style(file)))
            .collect()
    } else {
        let mut files = files
            .map(|file| (end.clone(), style(file)))
            .collect::<Vec<_>>();
        files.sort_unstable_by(|(_, left), (_, right)| left.content.cmp(&right.content));
        files
    }
}

fn complete_repeating_filenames(snapshot: &CompleterSnapshot, input: &str) -> Vec<Completion> {
    let token = match Tokenizer::new(input, false).last() {
        Some(Ok(token)) => token,
        Some(Err(_)) => return Vec::new(),
        None => return filename_impl(snapshot, input, true, FileTarget::File),
    };
    let offset = token.content_start;
    let mut completions = filename_impl(snapshot, &input[offset..], true, FileTarget::File);
    for completion in &mut completions {
        completion.0.start += offset;
    }
    completions
}

fn complete_shell(snapshot: &CompleterSnapshot, input: &str) -> Vec<Completion> {
    let (command, args, complete_command) = command_line::split(input);
    if complete_command {
        return Completer::Program.complete(&CompleterSnapshot::Empty, command);
    }
    let mut completions = complete_repeating_filenames(snapshot, args);
    for completion in &mut completions {
        completion.0.start += command.len() + 1;
    }
    completions
}
