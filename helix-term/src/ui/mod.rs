pub mod animation;
pub mod assistant;
mod cmdline_popup;
mod completion;
pub(crate) mod completion_ingress;
pub(crate) mod design;
mod document;
pub(crate) mod editor;
mod file_explorer;
pub mod gradient_border;
pub(crate) mod info;
pub mod lsp;
pub(crate) mod markdown;
pub mod menu;
mod notification_popup;
pub mod overlay;
pub mod picker;
pub(crate) mod pkg;
pub(crate) mod plugin_float;
pub mod plugin_panel;
pub(crate) mod plugin_render;
pub mod popup;
pub mod prompt;
mod select;
mod spinner;
mod statusline;
mod text;
mod text_decorations;
pub(crate) mod text_layout;

use crate::runtime::{send_ui_command_with, LayerCommand, UiCommand};
pub use cmdline_popup::CmdlinePopup;
pub use completion::Completion;
pub use editor::EditorView;
pub(crate) use file_explorer::PANEL_WIDTH as FILE_EXPLORER_PANEL_WIDTH;
pub use file_explorer::{FileExplorerPanel, ID as FILE_EXPLORER_ID};
use helix_stdx::rope;
use helix_view::theme::Style;
pub use markdown::Markdown;
pub use menu::Menu;
pub use notification_popup::NotificationPopup;
pub use picker::{Column as PickerColumn, FileLocation, Picker, PickerRuntime};
pub use popup::Popup;
pub use prompt::{Prompt, PromptEvent};
pub use select::Select;
pub use spinner::{ProgressSpinners, Spinner};
pub use text::Text;

use helix_view::editor::{CmdlineStyle, FileExplorerConfig};
use helix_view::Editor;
use tui::text::{Span, Spans};

use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::{error::Error, path::PathBuf};

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

pub fn prompt(
    cx: &mut crate::commands::Context,
    prompt: std::borrow::Cow<'static, str>,
    history_register: Option<char>,
    completion_fn: impl FnMut(&Editor, &str) -> Vec<prompt::Completion> + Send + 'static,
    callback_fn: impl FnMut(&mut crate::compositor::Context, &str, PromptEvent) + Send + 'static,
) {
    let mut prompt = Prompt::new(prompt, history_register, completion_fn, callback_fn);
    // Calculate the initial completion
    prompt.recalculate_completion(cx.editor);
    cx.push_layer(Box::new(prompt));
}

pub fn prompt_with_input(
    cx: &mut crate::commands::Context,
    prompt: std::borrow::Cow<'static, str>,
    input: String,
    history_register: Option<char>,
    completion_fn: impl FnMut(&Editor, &str) -> Vec<prompt::Completion> + Send + 'static,
    callback_fn: impl FnMut(&mut crate::compositor::Context, &str, PromptEvent) + Send + 'static,
) {
    let prompt = Prompt::new(prompt, history_register, completion_fn, callback_fn)
        .with_line(input, cx.editor);
    cx.push_layer(Box::new(prompt));
}

pub fn regex_prompt(
    cx: &mut crate::commands::Context,
    prompt: std::borrow::Cow<'static, str>,
    history_register: Option<char>,
    completion_fn: impl FnMut(&Editor, &str) -> Vec<prompt::Completion> + Send + 'static,
    fun: impl Fn(&mut crate::compositor::Context, rope::Regex, PromptEvent) + Send + 'static,
) {
    raw_regex_prompt(
        cx,
        prompt,
        history_register,
        completion_fn,
        move |cx, regex, _, event| fun(cx, regex, event),
    );
}
pub fn raw_regex_prompt(
    cx: &mut crate::commands::Context,
    prompt: std::borrow::Cow<'static, str>,
    history_register: Option<char>,
    completion_fn: impl FnMut(&Editor, &str) -> Vec<prompt::Completion> + Send + 'static,
    fun: impl Fn(&mut crate::compositor::Context, rope::Regex, &str, PromptEvent) + Send + 'static,
) {
    let (view_id, doc) = focused!(cx.editor);
    let doc_id = doc.id();
    let snapshot = doc.selection(view_id).clone();
    let offset_snapshot = doc.view_offset(view_id);
    let config = cx.editor.config();
    let cmdline_style = config.cmdline.style;
    let smart_case = config.search.smart_case;
    let scrolloff = config.scrolloff;
    drop(config);

    match cmdline_style {
        CmdlineStyle::Popup => {
            let cmdline = CmdlinePopup::new(
                prompt,
                history_register,
                completion_fn,
                move |cx: &mut crate::compositor::Context, input: &str, event: PromptEvent| {
                    match event {
                        PromptEvent::Abort => {
                            let (view_id, doc) = focused!(cx.editor);
                            doc.set_selection(view_id, snapshot.clone());
                            doc.set_view_offset(view_id, offset_snapshot);
                        }
                        PromptEvent::Update | PromptEvent::Validate => {
                            if input.is_empty() {
                                return;
                            }

                            let case_insensitive = if smart_case {
                                !input.chars().any(char::is_uppercase)
                            } else {
                                false
                            };

                            match rope::RegexBuilder::new()
                                .syntax(
                                    rope::Config::new()
                                        .case_insensitive(case_insensitive)
                                        .multi_line(true),
                                )
                                .build(input)
                            {
                                Ok(regex) => {
                                    let (view_id, doc) = focused!(cx.editor);
                                    doc.set_selection(view_id, snapshot.clone());

                                    if event == PromptEvent::Validate {
                                        let view = view_mut!(cx.editor, view_id);
                                        view.history.jumps.push((doc_id, snapshot.clone()));
                                    }

                                    fun(cx, regex, input, event);

                                    let (view_id, doc) = focused!(cx.editor);
                                    let view = view_mut!(cx.editor, view_id);
                                    view.ensure_cursor_in_view(doc, scrolloff);
                                }
                                Err(err) => {
                                    let (view_id, doc) = focused!(cx.editor);
                                    doc.set_selection(view_id, snapshot.clone());
                                    doc.set_view_offset(view_id, offset_snapshot);

                                    if event == PromptEvent::Validate {
                                        let msg = err.to_string();
                                        let ingress = cx.ingress.clone();
                                        cx.editor
                                            .work()
                                            .spawn(async move {
                                                send_ui_command_with(
                                                    UiCommand::Layer(
                                                        LayerCommand::InvalidRegexPopup {
                                                            message: msg,
                                                        },
                                                    ),
                                                    ingress,
                                                )
                                                .await;
                                            })
                                            .detach();
                                    }
                                }
                            }
                        }
                    }
                },
                CmdlineStyle::Popup,
            )
            .with_language("regex", std::sync::Arc::clone(&cx.editor.syn_loader));

            cx.push_layer(Box::new(cmdline));
        }
        CmdlineStyle::Bottom => {
            let mut prompt = Prompt::new(
                prompt,
                history_register,
                completion_fn,
                move |cx: &mut crate::compositor::Context, input: &str, event: PromptEvent| {
                    match event {
                        PromptEvent::Abort => {
                            let (view_id, doc) = focused!(cx.editor);
                            doc.set_selection(view_id, snapshot.clone());
                            doc.set_view_offset(view_id, offset_snapshot);
                        }
                        PromptEvent::Update | PromptEvent::Validate => {
                            if input.is_empty() {
                                return;
                            }

                            let case_insensitive = if smart_case {
                                !input.chars().any(char::is_uppercase)
                            } else {
                                false
                            };

                            match rope::RegexBuilder::new()
                                .syntax(
                                    rope::Config::new()
                                        .case_insensitive(case_insensitive)
                                        .multi_line(true),
                                )
                                .build(input)
                            {
                                Ok(regex) => {
                                    let (view_id, doc) = focused!(cx.editor);

                                    // revert state to what it was before the last update
                                    doc.set_selection(view_id, snapshot.clone());

                                    if event == PromptEvent::Validate {
                                        // Equivalent to push_jump to store selection just before jump
                                        let view = view_mut!(cx.editor, view_id);
                                        view.history.jumps.push((doc_id, snapshot.clone()));
                                    }

                                    fun(cx, regex, input, event);

                                    let (view_id, doc) = focused!(cx.editor);
                                    let view = view_mut!(cx.editor, view_id);
                                    view.ensure_cursor_in_view(doc, scrolloff);
                                }
                                Err(err) => {
                                    let (view_id, doc) = focused!(cx.editor);
                                    doc.set_selection(view_id, snapshot.clone());
                                    doc.set_view_offset(view_id, offset_snapshot);

                                    if event == PromptEvent::Validate {
                                        let msg = err.to_string();
                                        let ingress = cx.ingress.clone();
                                        cx.editor
                                            .work()
                                            .spawn(async move {
                                                send_ui_command_with(
                                                    UiCommand::Layer(
                                                        LayerCommand::InvalidRegexPopup {
                                                            message: msg,
                                                        },
                                                    ),
                                                    ingress,
                                                )
                                                .await;
                                            })
                                            .detach();
                                    }
                                }
                            }
                        }
                    }
                },
            )
            .with_language("regex", std::sync::Arc::clone(&cx.editor.syn_loader));
            // Calculate initial completion
            prompt.recalculate_completion(cx.editor);
            // prompt
            cx.push_layer(Box::new(prompt));
        }
    }
}

#[derive(Debug)]
pub struct FilePickerData {
    root: PathBuf,
    file_picker_config: helix_view::editor::FilePickerConfig,
    current_file: Option<PathBuf>,
    directory_style: Style,
    search_epoch: AtomicU64,
    search_lock: Mutex<()>,
    trace: picker::PickerTrace,
}

#[derive(Debug)]
pub struct FilePickerItem {
    path: PathBuf,
    query: std::sync::Arc<str>,
    track_fff: bool,
}

pub type FilePicker = Picker<FilePickerItem, FilePickerData>;

pub fn file_picker(
    editor: &Editor,
    root: PathBuf,
    ingress: crate::runtime::RuntimeIngress,
) -> FilePicker {
    let open_start = std::time::Instant::now();
    let trace = picker::PickerTrace::new("file_picker", open_start);
    let config = editor.config();
    let file_picker_config = config.file_picker.clone();
    let current_file = editor
        .tree
        .try_get(editor.tree.focus)
        .and_then(|view| editor.document(view.doc))
        .and_then(|doc| doc.path().cloned());
    let data = FilePickerData {
        root: root.clone(),
        file_picker_config: file_picker_config.clone(),
        current_file: current_file.clone(),
        directory_style: editor.theme.get("ui.text.directory"),
        search_epoch: AtomicU64::new(0),
        search_lock: Mutex::new(()),
        trace,
    };
    trace.log(
        "open_start",
        format_args!(
            "root={} current_file={} hide_preview={} hidden={} ignore={} git_ignore={} max_depth={:?}",
            root.display(),
            current_file
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "none".to_string()),
            file_picker_config.hide_preview,
            file_picker_config.hidden,
            file_picker_config.ignore,
            file_picker_config.git_ignore,
            file_picker_config.max_depth,
        ),
    );

    let columns = [PickerColumn::new(
        "path",
        |item: &FilePickerItem, data: &FilePickerData| {
            let path = item.path.strip_prefix(&data.root).unwrap_or(&item.path);
            let mut spans = Vec::with_capacity(3);
            if let Some(dirs) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                spans.extend([
                    Span::styled(dirs.to_string_lossy(), data.directory_style),
                    Span::styled(std::path::MAIN_SEPARATOR_STR, data.directory_style),
                ]);
            }
            let filename = path
                .file_name()
                .expect("normalized paths can't end in `..`")
                .to_string_lossy();
            spans.push(Span::raw(filename));
            Spans::from(spans).into()
        },
    )];

    let open_root = root.clone();
    let open_file_picker_config = file_picker_config.clone();
    let open_item = move |cx: &mut crate::compositor::Context, item: &FilePickerItem, action| {
        let path = helix_stdx::path::canonicalize(&item.path);
        let old_id = cx.editor.document_id_by_path(&path);

        match cx.editor.open(&path, action) {
            Ok(doc_id) => {
                if item.track_fff {
                    crate::fff::record_file_open(
                        &open_root,
                        &open_file_picker_config,
                        &item.query,
                        &path,
                    );
                }
                if old_id != Some(doc_id) {
                    default_folding(cx.editor);
                }
            }
            Err(e) => {
                let err = if let Some(err) = e.source() {
                    format!("{}", err)
                } else {
                    format!("unable to open \"{}\"", path.display())
                };
                cx.editor.set_error(err);
            }
        }
    };

    let get_files = |query: &str,
                     _editor: &mut Editor,
                     data: Arc<FilePickerData>,
                     injector: &picker::Injector<FilePickerItem, FilePickerData>,
                     work: helix_runtime::Work| {
        let query = query.to_owned();
        let injector = injector.clone();
        let epoch = data.search_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        data.trace.log(
            "dynamic_query_callback_queued",
            format_args!("epoch={epoch} query={query:?}"),
        );
        work.spawn(async move {
            tokio::task::spawn_blocking(move || {
                let task_start = std::time::Instant::now();
                let _search = data
                    .search_lock
                    .lock()
                    .map_err(|_| anyhow::anyhow!("FFF file picker search lock was poisoned"))?;
                if data.search_epoch.load(Ordering::Acquire) != epoch {
                    data.trace.log(
                        "dynamic_query_stale_before_search",
                        format_args!(
                            "epoch={epoch} query={query:?} elapsed_us={}",
                            task_start.elapsed().as_micros(),
                        ),
                    );
                    return Ok(());
                }

                let search_start = std::time::Instant::now();
                data.trace.log(
                    "fff_search_start",
                    format_args!(
                        "epoch={epoch} query={query:?} root={}",
                        data.root.display(),
                    ),
                );
                let matches = crate::fff::search_files(
                    &data.root,
                    &query,
                    data.current_file.as_deref(),
                    &data.file_picker_config,
                )?;
                if data.search_epoch.load(Ordering::Acquire) != epoch {
                    data.trace.log(
                        "dynamic_query_stale_after_search",
                        format_args!(
                            "epoch={epoch} query={query:?} search_us={} elapsed_us={}",
                            search_start.elapsed().as_micros(),
                            task_start.elapsed().as_micros(),
                        ),
                    );
                    return Ok(());
                }

                let result_count = matches.len();
                let inject_start = std::time::Instant::now();
                let mut injected = 0usize;
                for item in matches {
                    if data.search_epoch.load(Ordering::Acquire) != epoch {
                        break;
                    }
                    if injector
                        .push(FilePickerItem {
                            path: item.path,
                            query: item.query,
                            track_fff: true,
                        })
                        .is_err()
                    {
                        break;
                    }
                    injected += 1;
                }
                data.trace.log(
                    "dynamic_query_inject_done",
                    format_args!(
                        "epoch={epoch} query={query:?} search_us={} inject_us={} elapsed_us={} results={result_count} injected={injected}",
                        search_start.elapsed().as_micros(),
                        inject_start.elapsed().as_micros(),
                        task_start.elapsed().as_micros(),
                    ),
                );

                Ok(())
            })
            .await
            .map_err(|err| anyhow::anyhow!("FFF file picker search task failed: {err}"))?
        })
    };
    let refresh_root = root.clone();
    let refresh_config = file_picker_config.clone();
    let refresh_ingress = ingress.clone();
    editor
        .runtime()
        .block()
        .spawn(move || {
            trace.log(
                "initial_refresh_wait_start",
                format_args!("root={}", refresh_root.display()),
            );
            let send_refresh = || {
                trace.log("initial_refresh_send", format_args!("query=\"\""));
                refresh_ingress.ui(crate::runtime::UiCommand::Picker(
                    crate::runtime::ui::command::PickerCommand::RunDynamicQuery {
                        query: "".into(),
                    },
                ));
            };
            match crate::fff::wait_for_initial_file_results(&refresh_root, &refresh_config) {
                Ok(true) => {
                    trace.log(
                        "initial_results_ready",
                        format_args!("root={}", refresh_root.display()),
                    );
                    send_refresh();
                }
                Ok(false) => trace.log(
                    "initial_results_timeout",
                    format_args!("root={}", refresh_root.display()),
                ),
                Err(err) => log::error!("FFF file picker first-results wait failed: {err:#}"),
            }
            match crate::fff::wait_for_initial_file_scan(&refresh_root, &refresh_config) {
                Ok(true) => {
                    trace.log(
                        "initial_scan_ready",
                        format_args!("root={}", refresh_root.display()),
                    );
                    send_refresh();
                }
                Ok(false) => trace.log(
                    "initial_scan_timeout",
                    format_args!("root={}", refresh_root.display()),
                ),
                Err(err) => log::error!("FFF file picker scan readiness failed: {err:#}"),
            }
        })
        .detach();

    let initial_search_start = std::time::Instant::now();
    let initial_options = match crate::fff::search_files_available(
        &root,
        "",
        current_file.as_deref(),
        &file_picker_config,
    ) {
        Ok(matches) => matches
            .into_iter()
            .map(|item| FilePickerItem {
                path: item.path,
                query: item.query,
                track_fff: true,
            })
            .collect::<Vec<_>>(),
        Err(err) => {
            trace.log(
                "initial_search_failed",
                format_args!(
                    "elapsed_us={} error={err:#}",
                    initial_search_start.elapsed().as_micros(),
                ),
            );
            Vec::new()
        }
    };
    trace.log(
        "initial_search_done",
        format_args!(
            "elapsed_us={} results={}",
            initial_search_start.elapsed().as_micros(),
            initial_options.len(),
        ),
    );

    let picker = Picker::new(
        columns,
        0,
        initial_options,
        data,
        PickerRuntime::new(editor),
        ingress.clone(),
        open_item,
    )
    .with_trace(trace)
    .with_preview(|_editor, item| Some((item.path.as_path().into(), None)))
    .show_preview(!file_picker_config.hide_preview)
    .with_item_data(
        |item: &FilePickerItem| helix_view::model::PickerItemData::FilePath {
            path: item.path.clone(),
            is_dir: false,
        },
    )
    .with_dynamic_query(get_files, Some(40))
    .with_initial_dynamic_query()
    .with_external_filtering();
    trace.log(
        "open_return",
        format_args!(
            "root={} elapsed_us={}",
            root.display(),
            open_start.elapsed().as_micros(),
        ),
    );
    picker
}

fn get_excluded_types() -> ignore::types::Types {
    use ignore::types::TypesBuilder;
    let mut type_builder = TypesBuilder::new();
    type_builder
        .add(
            "compressed",
            "*.{zip,gz,bz2,zst,lzo,sz,tgz,tbz2,lz,lz4,lzma,lzo,z,Z,xz,7z,rar,cab}",
        )
        .expect("Invalid type definition");
    type_builder.negate("all");
    type_builder
        .build()
        .expect("failed to build excluded_types")
}

pub fn directory_content(
    root: &Path,
    editor: &Editor,
) -> Result<Vec<(PathBuf, bool)>, std::io::Error> {
    let config = editor.config();
    directory_content_with_config(root, &config.file_explorer)
}

pub fn directory_content_with_config(
    root: &Path,
    config: &FileExplorerConfig,
) -> Result<Vec<(PathBuf, bool)>, std::io::Error> {
    use ignore::WalkBuilder;

    let mut walk_builder = WalkBuilder::new(root);

    let mut content: Vec<(PathBuf, bool)> = walk_builder
        .hidden(config.hidden)
        .parents(config.parents)
        .ignore(config.ignore)
        .follow_links(config.follow_symlinks)
        .git_ignore(config.git_ignore)
        .git_global(config.git_global)
        .git_exclude(config.git_exclude)
        .max_depth(Some(1))
        .add_custom_ignore_filename(helix_loader::config_dir().join("ignore"))
        .add_custom_ignore_filename(helix_loader::workspace_ignore_file_name())
        .types(get_excluded_types())
        .build()
        .filter_map(|entry| {
            entry
                .map(|entry| {
                    let path = entry.path();
                    let is_dir = path.is_dir();
                    let mut path = path.to_path_buf();
                    if is_dir && path != root && config.flatten_dirs {
                        while let Some(single_child_directory) = get_child_if_single_dir(&path) {
                            path = single_child_directory;
                        }
                    }
                    (path, is_dir)
                })
                .ok()
                .filter(|entry| entry.0 != root)
        })
        .collect();

    content.sort_by(|(path1, is_dir1), (path2, is_dir2)| (!is_dir1, path1).cmp(&(!is_dir2, path2)));
    if root.parent().is_some() {
        content.insert(0, (root.join(".."), true));
    }
    Ok(content)
}

fn get_child_if_single_dir(path: &Path) -> Option<PathBuf> {
    let mut entries = path.read_dir().ok()?;
    let entry = entries.next()?.ok()?;
    let entry_path = entry.path();
    if entries.next().is_none() && entry_path.is_dir() {
        Some(entry_path)
    } else {
        None
    }
}

pub fn default_folding(editor: &mut Editor) {
    use crate::commands::typed::{fold_textobjects, FOLD_SIGNATURE};
    use helix_core::command_line::Args;

    let textobjects = editor.config.load().fold_textobjects.join(" ");
    if textobjects.is_empty() {
        return;
    }

    let loader = editor.syn_loader.load();

    let str = format!("--document {textobjects}");
    let args = Args::parse(&str, FOLD_SIGNATURE, true, |token| Ok(token.content)).unwrap();

    let (view_id, doc) = focused!(editor);
    let view = view!(editor, view_id);
    _ = fold_textobjects(doc, view, &loader, args);
}
pub mod completers {
    use super::Utf8PathBuf;
    use crate::ui::prompt::Completion;
    use helix_core::command_line::{self, Tokenizer};
    use helix_core::fuzzy::fuzzy_match;
    use helix_core::syntax::config::LanguageServerFeature;
    use helix_view::document::SCRATCH_BUFFER_NAME;
    use helix_view::theme;
    use helix_view::{editor::Config, Editor};
    use once_cell::sync::Lazy;
    use std::borrow::Cow;
    use std::collections::BTreeSet;
    use tui::text::Span;

    pub type Completer = fn(&Editor, &str) -> Vec<Completion>;

    pub fn none(_editor: &Editor, _input: &str) -> Vec<Completion> {
        Vec::new()
    }

    pub fn buffer(editor: &Editor, input: &str) -> Vec<Completion> {
        let names = editor.documents.values().map(|doc| {
            doc.relative_path()
                .map(|p| p.display().to_string().into())
                .unwrap_or_else(|| Cow::from(SCRATCH_BUFFER_NAME))
        });

        fuzzy_match(input, names, true)
            .into_iter()
            .map(|(name, _)| ((0..), name.into()))
            .collect()
    }

    pub fn theme(_editor: &Editor, input: &str) -> Vec<Completion> {
        let mut names = theme::Loader::read_names(&helix_loader::config_dir().join("themes"));
        for rt_dir in helix_loader::runtime_dirs() {
            names.extend(theme::Loader::read_names(&rt_dir.join("themes")));
        }
        names.push("default".into());
        names.push("base16_default".into());
        names.sort();
        names.dedup();

        fuzzy_match(input, names, false)
            .into_iter()
            .map(|(name, _)| ((0..), name.into()))
            .collect()
    }

    /// Recursive function to get all keys from this value and add them to vec
    fn get_keys(value: &serde_json::Value, vec: &mut Vec<String>, scope: Option<&str>) {
        if let Some(map) = value.as_object() {
            for (key, value) in map.iter() {
                let key = match scope {
                    Some(scope) => format!("{}.{}", scope, key),
                    None => key.clone(),
                };
                get_keys(value, vec, Some(&key));
                if !value.is_object() {
                    vec.push(key);
                }
            }
        }
    }

    /// Completes names of language servers which are running for the current document.
    pub fn active_language_servers(editor: &Editor, input: &str) -> Vec<Completion> {
        let language_servers = focused_ref!(editor)
            .1
            .language_servers()
            .map(|ls| ls.name());

        fuzzy_match(input, language_servers, false)
            .into_iter()
            .map(|(name, _)| ((0..), Span::raw(name.to_string())))
            .collect()
    }

    /// Completes names of language servers which are configured for the language of the current
    /// document.
    pub fn configured_language_servers(editor: &Editor, input: &str) -> Vec<Completion> {
        let language_servers = focused_ref!(editor)
            .1
            .language_config()
            .into_iter()
            .flat_map(|config| &config.language_servers)
            .map(|ls| ls.name.as_str());

        fuzzy_match(input, language_servers, false)
            .into_iter()
            .map(|(name, _)| ((0..), Span::raw(name.to_string())))
            .collect()
    }

    pub fn setting(_editor: &Editor, input: &str) -> Vec<Completion> {
        static KEYS: Lazy<Vec<String>> = Lazy::new(|| {
            let mut keys = Vec::new();
            let json = serde_json::json!(Config::default());
            get_keys(&json, &mut keys, None);
            keys
        });

        fuzzy_match(input, &*KEYS, false)
            .into_iter()
            .map(|(name, _)| ((0..), Span::raw(name)))
            .collect()
    }

    pub fn filename(editor: &Editor, input: &str) -> Vec<Completion> {
        filename_with_git_ignore(editor, input, true)
    }

    pub fn filename_with_git_ignore(
        editor: &Editor,
        input: &str,
        git_ignore: bool,
    ) -> Vec<Completion> {
        filename_impl(editor, input, git_ignore, |entry| {
            if entry.path().is_dir() {
                FileMatch::AcceptIncomplete
            } else {
                FileMatch::Accept
            }
        })
    }

    pub fn language(editor: &Editor, input: &str) -> Vec<Completion> {
        let text: String = "text".into();

        let loader = editor.syn_loader.load();
        let language_ids = loader
            .language_configs()
            .map(|config| &config.language_id)
            .chain(std::iter::once(&text));

        fuzzy_match(input, language_ids, false)
            .into_iter()
            .map(|(name, _)| ((0..), name.to_owned().into()))
            .collect()
    }

    pub fn lsp_workspace_command(editor: &Editor, input: &str) -> Vec<Completion> {
        let commands = focused_ref!(editor)
            .1
            .language_servers_with_feature(LanguageServerFeature::WorkspaceCommand)
            .flat_map(|ls| {
                ls.capabilities()
                    .execute_command_provider
                    .iter()
                    .flat_map(|options| options.commands.iter())
            });

        fuzzy_match(input, commands, false)
            .into_iter()
            .map(|(name, _)| ((0..), name.to_owned().into()))
            .collect()
    }

    pub fn directory(editor: &Editor, input: &str) -> Vec<Completion> {
        directory_with_git_ignore(editor, input, true)
    }

    pub fn directory_with_git_ignore(
        editor: &Editor,
        input: &str,
        git_ignore: bool,
    ) -> Vec<Completion> {
        filename_impl(editor, input, git_ignore, |entry| {
            if entry.path().is_dir() {
                FileMatch::Accept
            } else {
                FileMatch::Reject
            }
        })
    }

    #[derive(Copy, Clone, PartialEq, Eq)]
    enum FileMatch {
        /// Entry should be ignored
        Reject,
        /// Entry is usable but can't be the end (for instance if the entry is a directory and we
        /// try to match a file)
        AcceptIncomplete,
        /// Entry is usable and can be the end of the match
        Accept,
    }

    // TODO: we could return an iter/lazy thing so it can fetch as many as it needs.
    fn filename_impl<F>(
        editor: &Editor,
        input: &str,
        git_ignore: bool,
        filter_fn: F,
    ) -> Vec<Completion>
    where
        F: Fn(&ignore::DirEntry) -> FileMatch,
    {
        // Rust's filename handling is really annoying.

        use ignore::WalkBuilder;
        use std::path::Path;

        let is_tilde = input == "~";
        let path = helix_stdx::path::expand_tilde(Path::new(input));
        #[cfg(windows)]
        if path.is_absolute() {
            return Vec::new();
        }

        let (dir, file_name) = if input.ends_with(std::path::MAIN_SEPARATOR) {
            (path, None)
        } else {
            let is_period = (input.ends_with((format!("{}.", std::path::MAIN_SEPARATOR)).as_str())
                && input.len() > 2)
                || input == ".";
            let file_name = if is_period {
                Some(String::from("."))
            } else {
                path.file_name()
                    .and_then(|file| file.to_str().map(|path| path.to_owned()))
            };

            let path = if is_period {
                path
            } else {
                match path.parent() {
                    Some(path) if !path.as_os_str().is_empty() => Cow::Borrowed(path),
                    // Path::new("h")'s parent is Some("")...
                    _ => Cow::Owned(helix_stdx::env::current_working_dir()),
                }
            };

            (path, file_name)
        };

        let end = input.len()..;

        let files = WalkBuilder::new(&dir)
            .hidden(false)
            .follow_links(false) // We're scanning over depth 1
            .git_ignore(git_ignore)
            .parents(false)
            .max_depth(Some(1))
            .build()
            .filter_map(|file| {
                file.ok().and_then(|entry| {
                    let fmatch = filter_fn(&entry);

                    if fmatch == FileMatch::Reject {
                        return None;
                    }

                    let path = entry.path();
                    let is_dir = path.is_dir();
                    let file_type = entry.file_type();
                    let is_symlink = file_type.is_some_and(|ft| ft.is_symlink());
                    let mut path = if is_tilde {
                        // if it's a single tilde an absolute path is displayed so that when `TAB` is pressed on
                        // one of the directories the tilde will be replaced with a valid path not with a relative
                        // home directory name.
                        // ~ -> <TAB> -> /home/user
                        // ~/ -> <TAB> -> ~/first_entry
                        path.to_path_buf()
                    } else {
                        path.strip_prefix(&dir).unwrap_or(path).to_path_buf()
                    };

                    if fmatch == FileMatch::AcceptIncomplete {
                        path.push("");
                    }

                    let path = path.into_os_string().into_string().ok()?;
                    Some(Utf8PathBuf {
                        path,
                        is_dir,
                        is_symlink,
                    })
                })
            }) // TODO: unwrap or skip
            .filter(|path| !path.path.is_empty());

        let directory_color = editor.theme.get("ui.text.directory");
        let symlink_color = editor.theme.get("ui.text.symlink");

        let style_from_file = |file: Utf8PathBuf| {
            if file.is_symlink {
                Span::styled(file.path, symlink_color)
            } else if file.is_dir {
                Span::styled(file.path, directory_color)
            } else {
                Span::raw(file.path)
            }
        };

        // if empty, return a list of dirs and files in current dir
        if let Some(file_name) = file_name {
            let range = (input.len().saturating_sub(file_name.len()))..;
            fuzzy_match(&file_name, files, true)
                .into_iter()
                .map(|(name, _)| (range.clone(), style_from_file(name)))
                .collect()

            // TODO: complete to longest common match
        } else {
            let mut files: Vec<_> = files
                .map(|file| (end.clone(), style_from_file(file)))
                .collect();
            files.sort_unstable_by(|(_, path1), (_, path2)| path1.content.cmp(&path2.content));
            files
        }
    }

    pub fn register(editor: &Editor, input: &str) -> Vec<Completion> {
        let iter = editor
            .registers
            .iter_preview()
            // Exclude special registers that shouldn't be written to
            .filter(|(ch, _)| !matches!(ch, '%' | '#' | '.'))
            .map(|(ch, _)| ch.to_string());

        fuzzy_match(input, iter, false)
            .into_iter()
            .map(|(name, _)| ((0..), name.into()))
            .collect()
    }

    pub fn program(_editor: &Editor, input: &str) -> Vec<Completion> {
        static PROGRAMS_IN_PATH: Lazy<BTreeSet<String>> = Lazy::new(|| {
            // Go through the entire PATH and read all files into a set.
            let Some(path) = std::env::var_os("PATH") else {
                return Default::default();
            };

            std::env::split_paths(&path)
                .filter_map(|path| std::fs::read_dir(path).ok())
                .flatten()
                .filter_map(|res| {
                    let entry = res.ok()?;
                    if entry.metadata().ok()?.is_file() {
                        entry.file_name().into_string().ok()
                    } else {
                        None
                    }
                })
                .collect()
        });

        fuzzy_match(input, PROGRAMS_IN_PATH.iter(), false)
            .into_iter()
            .map(|(name, _)| ((0..), name.clone().into()))
            .collect()
    }

    /// This expects input to be a raw string of arguments, because this is what Signature's raw_after does.
    pub fn repeating_filenames(editor: &Editor, input: &str) -> Vec<Completion> {
        let token = match Tokenizer::new(input, false).last() {
            Some(token) => token.unwrap(),
            None => return filename(editor, input),
        };

        let offset = token.content_start;

        let mut completions = filename(editor, &input[offset..]);
        for completion in completions.iter_mut() {
            completion.0.start += offset;
        }
        completions
    }

    pub fn shell(editor: &Editor, input: &str) -> Vec<Completion> {
        let (command, args, complete_command) = command_line::split(input);

        if complete_command {
            return program(editor, command);
        }

        let mut completions = repeating_filenames(editor, args);
        for completion in completions.iter_mut() {
            // + 1 for separator between `command` and `args`
            completion.0.start += command.len() + 1;
        }

        completions
    }

    pub fn foldable_textobjects(_editor: &Editor, input: &str) -> Vec<Completion> {
        let textobjects = [
            "class",
            "function",
            "comment",
            "test",
            "conditional",
            "loop",
        ];
        fuzzy_match(input, textobjects.iter(), false)
            .into_iter()
            .map(|(name, _)| ((0..), (*name).into()))
            .collect()
    }
}
