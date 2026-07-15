pub mod animation;
pub mod assistant;
mod cmdline_popup;
mod completion;
pub(crate) mod completion_ingress;
mod confirmation;
pub(crate) mod design;
mod document;
pub(crate) mod editor;
mod file_explorer;
pub(crate) mod file_options;
mod file_scan;
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

use crate::{
    alt,
    runtime::{send_ui_command_with, LayerCommand, UiCommand},
};
pub use cmdline_popup::CmdlinePopup;
pub use completion::Completion;
pub use confirmation::Confirmation;
pub use editor::EditorView;
pub use file_explorer::VcsSnapshot;
#[cfg(any(test, feature = "storybook"))]
pub(crate) use file_explorer::PANEL_WIDTH as FILE_EXPLORER_PANEL_WIDTH;
pub use file_explorer::{FileExplorerPanel, ID as FILE_EXPLORER_ID};
pub(crate) use file_explorer::{FileExplorerTreeWork, PreparedFileExplorerTree};
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

use helix_view::editor::CmdlineStyle;
use helix_view::Editor;
use tui::text::{Span, Spans};

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, RwLock,
};

pub fn prompt(
    cx: &mut crate::commands::Context,
    prompt: std::borrow::Cow<'static, str>,
    history_register: Option<char>,
    completion_provider: impl prompt::CompletionProvider + 'static,
    callback_fn: impl FnMut(&mut crate::compositor::Context, &str, PromptEvent) + Send + 'static,
) {
    let mut prompt = Prompt::new(prompt, history_register, completion_provider, callback_fn);
    // Calculate the initial completion
    prompt.recalculate_completion(cx.editor);
    prompt.dispatch_completion_work(cx.editor.runtime(), cx.ingress.clone());
    cx.push_layer(Box::new(prompt));
}

pub fn prompt_with_input(
    cx: &mut crate::commands::Context,
    prompt: std::borrow::Cow<'static, str>,
    input: String,
    history_register: Option<char>,
    completion_provider: impl prompt::CompletionProvider + 'static,
    callback_fn: impl FnMut(&mut crate::compositor::Context, &str, PromptEvent) + Send + 'static,
) {
    let mut prompt = Prompt::new(prompt, history_register, completion_provider, callback_fn)
        .with_line(input, cx.editor);
    prompt.dispatch_completion_work(cx.editor.runtime(), cx.ingress.clone());
    cx.push_layer(Box::new(prompt));
}

pub fn regex_prompt(
    cx: &mut crate::commands::Context,
    prompt: std::borrow::Cow<'static, str>,
    history_register: Option<char>,
    completion_provider: impl prompt::CompletionProvider + 'static,
    fun: impl Fn(&mut crate::compositor::Context, rope::Regex, PromptEvent) + Send + 'static,
) {
    raw_regex_prompt(
        cx,
        prompt,
        history_register,
        completion_provider,
        move |cx, regex, _, event| fun(cx, regex, event),
    );
}
pub fn raw_regex_prompt(
    cx: &mut crate::commands::Context,
    prompt: std::borrow::Cow<'static, str>,
    history_register: Option<char>,
    completion_provider: impl prompt::CompletionProvider + 'static,
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
            let mut cmdline = CmdlinePopup::new(
                prompt,
                history_register,
                completion_provider,
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

            cmdline.prepare_completion(cx.editor, cx.ingress.clone());

            cx.push_layer(Box::new(cmdline));
        }
        CmdlineStyle::Bottom => {
            let mut prompt = Prompt::new(
                prompt,
                history_register,
                completion_provider,
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
            prompt.dispatch_completion_work(cx.editor.runtime(), cx.ingress.clone());
            // prompt
            cx.push_layer(Box::new(prompt));
        }
    }
}

#[derive(Debug)]
pub struct FilePickerData {
    root: PathBuf,
    file_picker_config: Arc<RwLock<helix_view::editor::FilePickerConfig>>,
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
    let file_picker_config_state = Arc::new(RwLock::new(file_picker_config.clone()));
    let current_file = editor
        .tree
        .try_get(editor.tree.focus)
        .and_then(|view| editor.document(view.doc))
        .and_then(|doc| doc.path().cloned());
    let data = FilePickerData {
        root: root.clone(),
        file_picker_config: file_picker_config_state.clone(),
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
    let open_file_picker_config = file_picker_config_state.clone();
    let open_item = move |cx: &mut crate::compositor::Context, item: &FilePickerItem, action| {
        let path = item.path.clone();
        let target = cx.editor.focused_view_id();
        let fff_record = item.track_fff.then(|| crate::runtime::FffOpenRecord {
            root: open_root.clone(),
            config: open_file_picker_config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            query: item.query.to_string(),
        });
        crate::runtime::ui::document::queue_document_open(
            cx.editor,
            &cx.ingress,
            &cx.foreground,
            crate::runtime::DocumentOpenRequest {
                path,
                action,
                lane: crate::runtime::DocumentOpenLane::Navigation,
                target: crate::runtime::DocumentOpenTarget::View(target),
                selection: crate::runtime::DocumentOpenSelection::None,
                alignment: crate::runtime::DocumentOpenAlignment::None,
                default_folding_if_new: true,
                fff_record,
                external_if_binary: None,
                post_action: crate::runtime::DocumentOpenPostAction::None,
                completion: crate::runtime::DocumentOpenCompletionTarget::Editor,
            },
        );
    };

    let get_files = |query: &str,
                     _editor: &mut Editor,
                     data: Arc<FilePickerData>,
                     injector: &picker::Injector<FilePickerItem, FilePickerData>,
                     work: helix_runtime::Work,
                     block: helix_runtime::Block| {
        let query = query.to_owned();
        let injector = injector.clone();
        let epoch = data.search_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        data.trace.log(
            "dynamic_query_callback_queued",
            format_args!("epoch={epoch} query={query:?}"),
        );
        let search = block.spawn(move || {
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
                let file_picker_config = data
                    .file_picker_config
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let matches = crate::fff::search_files(
                    &data.root,
                    &query,
                    data.current_file.as_deref(),
                    &file_picker_config,
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
            });
        work.spawn(async move {
            search
                .await
                .map_err(|err| anyhow::anyhow!("FFF file picker search task failed: {err}"))?
        })
    };
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

    let mut key_handlers = picker::PickerKeyHandlers::new();
    key_handlers.insert_layer(
        alt!('o'),
        Box::new(|cx, data: Arc<FilePickerData>, picker| {
            let items = {
                let config = data
                    .file_picker_config
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                file_options::FileSourceOption::picker_items(&config)
            };
            let ingress = cx.ingress.clone();
            let data_for_toggle = data.clone();
            Box::new(file_options::popup(
                "file-picker-source-options",
                items,
                move |editor, option| {
                    {
                        let mut config = data_for_toggle
                            .file_picker_config
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        option.toggle_picker(&mut config);
                    }
                    data_for_toggle.search_epoch.fetch_add(1, Ordering::AcqRel);
                    if let Err(error) = ingress.ui(crate::runtime::UiCommand::Picker(
                        crate::runtime::ui::command::PickerCommand::RefreshDynamicQuery { picker },
                    )) {
                        editor.set_error(error.to_string());
                    }
                },
            ))
        }),
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
    .with_key_handlers(key_handlers)
    .with_custom_hints([crate::widgets::Hint::new("A-o", "options").priority(90)])
    .with_preview(|_editor, item| Some((item.path.as_path().into(), None)))
    .show_preview(!file_picker_config.hide_preview)
    .with_item_data(
        |item: &FilePickerItem| helix_view::model::PickerItemData::FilePath {
            path: item.path.clone(),
            is_dir: false,
        },
    )
    .with_dynamic_query(get_files, picker::DynamicQuerySchedule::Immediate)
    .with_initial_dynamic_query()
    .with_external_filtering();
    let picker_id = picker.instance_id();
    let refresh_root = root.clone();
    let refresh_config = file_picker_config.clone();
    let refresh_ingress = ingress.clone();
    let blocking = editor.runtime().block().spawn(move || {
        let mut refreshes = 0u8;
        trace.log(
            "initial_refresh_wait_start",
            format_args!("root={}", refresh_root.display()),
        );
        match crate::fff::wait_for_initial_file_results(&refresh_root, &refresh_config) {
            Ok(true) => {
                trace.log(
                    "initial_results_ready",
                    format_args!("root={}", refresh_root.display()),
                );
                refreshes += 1;
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
                refreshes += 1;
            }
            Ok(false) => trace.log(
                "initial_scan_timeout",
                format_args!("root={}", refresh_root.display()),
            ),
            Err(err) => log::error!("FFF file picker scan readiness failed: {err:#}"),
        }
        refreshes
    });
    editor
        .work()
        .spawn(async move {
            let Ok(refreshes) = blocking.await else {
                return;
            };
            for _ in 0..refreshes {
                trace.log("initial_refresh_send", format_args!("query=\"\""));
                if refresh_ingress
                    .send_ui(crate::runtime::UiCommand::Picker(
                        crate::runtime::ui::command::PickerCommand::RefreshDynamicQuery {
                            picker: picker_id,
                        },
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
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
pub mod completers;
