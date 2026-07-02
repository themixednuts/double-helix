//! UI storybook for terminal components.
//!
//! Stories are deterministic Ratatui renders. They give us a place to standardize
//! Helix's terminal UI while keeping Lua plugins on the typed render-op ABI.

use std::{path::PathBuf, sync::OnceLock, time::Duration};

use anyhow::{bail, Result};
use helix_plugin::types::{SurfaceRenderOp, SurfaceRenderOps};
use helix_view::{document::Mode, graphics::Rect, info::Info, theme as helix_theme, Editor};
use tui::ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect as RatatuiRect},
};
use tui::text::{Span as HSpan, Spans};

use crate::ui::design;
#[cfg(test)]
use crate::ui::design::HelixUiDesign;

mod harness;
mod model;
mod shell;
mod theme;
mod timing;
mod util;
use harness::Stage;
use model::{LoadedTheme, StoryArg, StoryCanvas, StoryContext, StoryKind, StoryRenderer};
pub use model::{Story, UiStyleGuide};
use shell::render_storybook;
use theme::{
    available_theme_names, load_named_storybook_theme, load_storybook_theme, parse_theme_mode,
    theme_loader, ThemeChoice,
};
use timing::{cycling_scroll_offset, pulse, storybook_tick, tick_elapsed};
use util::{
    buffer_to_string, centered_rect, fill_rect, hline, inset, render_panel,
    render_panel_with_corners, set_clipped, set_right_clipped, split_horizontal, split_vertical,
};

const DEFAULT_WIDTH: u16 = 108;
const DEFAULT_HEIGHT: u16 = 34;
const STORYBOOK_TICK_MS: u64 = 120;

static STORIES: &[Story] = &[
    Story {
        id: "foundations/tokens",
        title: "Semantic Tokens",
        category: "Foundations",
        component: "Foundations/Semantic Tokens",
        variant: "Default",
        summary: "Opinionated semantic color and emphasis roles for core UI and plugins.",
        kind: StoryKind::Docs,
        args: &[],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Styled(render_tokens_story),
    },
    Story {
        id: "foundations/layout",
        title: "Layout Frames",
        category: "Foundations",
        component: "Foundations/Layout Frames",
        variant: "Docked Shell",
        summary: "Docked shell, sidebars, overlays, and dense terminal layout conventions.",
        kind: StoryKind::Docs,
        args: &[],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Styled(render_layout_story),
    },
    Story {
        id: "foundations/widget-style",
        title: "Widget Style Bundle",
        category: "Foundations",
        component: "Foundations/Widget Style Bundle",
        variant: "Default",
        summary:
            "Resolved widget state styles for text, cursor, placeholder, selection, and frame.",
        kind: StoryKind::Docs,
        args: &[],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Styled(render_widget_style_story),
    },
    Story {
        id: "widgets/chrome",
        title: "Chrome",
        category: "Widgets",
        component: "Widgets/Chrome",
        variant: "Overview",
        summary: "Headers, counted headers, borders, horizontal dividers, and vertical dividers.",
        kind: StoryKind::Component,
        args: &[
            StoryArg::new("rounded", "true", "Rounded popup frame variant"),
            StoryArg::new("square", "true", "Square frame variant for grids"),
            StoryArg::new("counts", "12/164", "Counted header state"),
        ],
        canvas: StoryCanvas::Padded { x: 1, y: 1 },
        render: StoryRenderer::Styled(render_chrome_story),
    },
    Story {
        id: "widgets/text-input",
        title: "Text Input",
        category: "Widgets",
        component: "Widgets/Text Input",
        variant: "Overview",
        summary: "Focused input, long input anchoring, cursor state, and truncation behavior.",
        kind: StoryKind::Component,
        args: &[
            StoryArg::new("states", "4", "Focused, long path, search, empty"),
            StoryArg::new("cursor", "varies", "Cursor position per fixture row"),
        ],
        canvas: StoryCanvas::Padded { x: 1, y: 1 },
        render: StoryRenderer::Styled(render_text_input_story),
    },
    Story {
        id: "widgets/text-input/focused",
        title: "Focused",
        category: "Widgets",
        component: "Widgets/Text Input",
        variant: "Focused",
        summary: "Single focused input state with command text and cursor.",
        kind: StoryKind::Component,
        args: &[
            StoryArg::new("text", ":theme double-helix-dark --preview", "Input value"),
            StoryArg::new("cursor", "18", "Cursor byte offset"),
        ],
        canvas: StoryCanvas::Centered {
            width: 72,
            height: 1,
        },
        render: StoryRenderer::Styled(render_text_input_focused_story),
    },
    Story {
        id: "widgets/text-input/long-path",
        title: "Long Path",
        category: "Widgets",
        component: "Widgets/Text Input",
        variant: "Long Path",
        summary: "Anchored input state for paths wider than the available terminal width.",
        kind: StoryKind::Component,
        args: &[
            StoryArg::new("text", "long path", "Path exceeds visible width"),
            StoryArg::new("cursor", "58", "Cursor near the end"),
        ],
        canvas: StoryCanvas::Centered {
            width: 62,
            height: 1,
        },
        render: StoryRenderer::Styled(render_text_input_long_path_story),
    },
    Story {
        id: "widgets/text-input/empty",
        title: "Empty",
        category: "Widgets",
        component: "Widgets/Text Input",
        variant: "Empty",
        summary: "Empty focused input state with no caller-supplied placeholder.",
        kind: StoryKind::Component,
        args: &[
            StoryArg::new("text", "", "No input value"),
            StoryArg::new("cursor", "0", "Cursor at start"),
        ],
        canvas: StoryCanvas::Centered {
            width: 48,
            height: 1,
        },
        render: StoryRenderer::Styled(render_text_input_empty_story),
    },
    Story {
        id: "widgets/item-list",
        title: "Item List",
        category: "Widgets",
        component: "Widgets/Item List",
        variant: "Selected With Details",
        summary: "Scrollable selectable rows with caller-owned row rendering and scrollbar state.",
        kind: StoryKind::Component,
        args: &[
            StoryArg::new("items", "11", "Visible list fixture size"),
            StoryArg::new("selected", "2", "Selected row index"),
            StoryArg::new("scroll", "0", "Top of list"),
        ],
        canvas: StoryCanvas::Padded { x: 1, y: 1 },
        render: StoryRenderer::Styled(render_item_list_story),
    },
    Story {
        id: "widgets/scrolling",
        title: "Scrolling",
        category: "Widgets",
        component: "Widgets/Scroll Region",
        variant: "Overflow",
        summary: "Proportional scrollbars and styled text regions for docs, logs, and previews.",
        kind: StoryKind::Component,
        args: &[
            StoryArg::new("content_rows", "28", "Total content rows"),
            StoryArg::new("viewport_rows", "12", "Visible row count"),
            StoryArg::new("scroll", "animated", "Wall-clock scrolled state"),
        ],
        canvas: StoryCanvas::Padded { x: 1, y: 1 },
        render: StoryRenderer::Styled(render_scrolling_story),
    },
    Story {
        id: "widgets/message",
        title: "Message Bubble",
        category: "Widgets",
        component: "Widgets/Message Bubble",
        variant: "Directional",
        summary: "Left and right directional bubbles with border accent progress.",
        kind: StoryKind::Component,
        args: &[
            StoryArg::new("align", "left/right", "Speaker direction"),
            StoryArg::new("accent", "animated", "Progress accent around border"),
        ],
        canvas: StoryCanvas::Padded { x: 1, y: 1 },
        render: StoryRenderer::Styled(render_message_widget_story),
    },
    Story {
        id: "widgets/message-list",
        title: "Message List",
        category: "Widgets",
        component: "Widgets/Message List",
        variant: "Conversation",
        summary: "Conversation layout with selected details, accessories, and selected bars.",
        kind: StoryKind::Component,
        args: &[
            StoryArg::new("messages", "5", "Transcript fixture length"),
            StoryArg::new("selected", "3", "Selected message index"),
            StoryArg::new("details", "visible", "Selected details region"),
        ],
        canvas: StoryCanvas::Padded { x: 1, y: 1 },
        render: StoryRenderer::Styled(render_message_list_story),
    },
    Story {
        id: "widgets/anchored-text",
        title: "Anchored Text",
        category: "Widgets",
        component: "Widgets/Anchored Text",
        variant: "Truncated",
        summary: "Ratatui-native anchored text drawing with start and end truncation markers.",
        kind: StoryKind::Component,
        args: &[
            StoryArg::new("anchor", "start/end", "Truncation anchor"),
            StoryArg::new("width", "varies", "Constrained drawing width"),
        ],
        canvas: StoryCanvas::Padded { x: 1, y: 1 },
        render: StoryRenderer::Styled(render_anchored_text_story),
    },
    Story {
        id: "widgets/spinner-shadow",
        title: "Spinner And Shadow",
        category: "Widgets",
        component: "Widgets/Spinner And Shadow",
        variant: "Animated",
        summary: "Progress spinner frames and terminal box shadows for modal depth.",
        kind: StoryKind::Component,
        args: &[
            StoryArg::new("tick", "wall-clock", "Animation frame source"),
            StoryArg::new("shadow", "2x1 blur", "Modal depth decoration"),
        ],
        canvas: StoryCanvas::Padded { x: 1, y: 1 },
        render: StoryRenderer::Styled(render_spinner_shadow_story),
    },
    Story {
        id: "ui/editor-shell",
        title: "Editor Shell",
        category: "UI Patterns",
        component: "Editor/Shell",
        variant: "Normal Mode",
        summary: "Helix document viewport with bufferline, gutter, cursorline, and statusline.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("mode", "normal", "Statusline mode state"),
            StoryArg::new("diagnostic", "warning", "Inline diagnostic row"),
            StoryArg::new("buffer", "modified", "Active bufferline state"),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_editor_shell_story),
    },
    Story {
        id: "ui/key-help",
        title: "Key Help Menu",
        category: "UI Patterns",
        component: "Editor/Key Help Menu",
        variant: "Space",
        summary: "Pending-key autoinfo popup rendered from the real normal-mode Space keymap.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("mode", "normal", "Keymap mode"),
            StoryArg::new("pending", "space", "Pending key sequence"),
            StoryArg::new("source", "runtime keymap", "Uses real default keymap data"),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_key_help_story),
    },
    Story {
        id: "ui/autoinfo-scroll",
        title: "Scrollable Autoinfo",
        category: "UI Patterns",
        component: "Editor/Key Help Menu",
        variant: "Scrolled",
        summary: "Runtime autoinfo renderer with clamped body scrolling and scrollbar state.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("pending", "space", "Pending key sequence"),
            StoryArg::new("scroll", "bottom", "Scrolled body state"),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_autoinfo_scroll_story),
    },
    Story {
        id: "ui/picker",
        title: "Picker",
        category: "UI Patterns",
        component: "Editor/Picker",
        variant: "Default",
        summary: "Runtime picker layout with query row, result list, and selected row state.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("query", "ui", "Prompt query"),
            StoryArg::new("selected", "1", "Selected result row"),
            StoryArg::new(
                "preview",
                "hidden",
                "Preview disabled for this list-only state",
            ),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_picker_story),
    },
    Story {
        id: "ui/picker/empty",
        title: "Empty",
        category: "UI Patterns",
        component: "Editor/Picker",
        variant: "Empty",
        summary: "Picker empty-results state with prompt and count still visible.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("query", "zz-no-match", "Prompt query"),
            StoryArg::new("matched", "0", "No matching rows"),
            StoryArg::new("preview", "hidden", "No selected item preview"),
        ],
        canvas: StoryCanvas::Centered {
            width: 72,
            height: 12,
        },
        render: StoryRenderer::Runtime(render_picker_empty_story),
    },
    Story {
        id: "ui/picker/long-paths",
        title: "Long Paths",
        category: "UI Patterns",
        component: "Editor/Picker",
        variant: "Long Paths",
        summary: "Picker result rows with start truncation for deeply nested paths.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("query", "helix", "Prompt query"),
            StoryArg::new("truncate_start", "true", "Rows preserve file suffix"),
            StoryArg::new("selected", "2", "Selected result row"),
        ],
        canvas: StoryCanvas::Centered {
            width: 58,
            height: 14,
        },
        render: StoryRenderer::Runtime(render_picker_long_paths_story),
    },
    Story {
        id: "ui/prompt-cmdline",
        title: "Prompt And Cmdline",
        category: "UI Patterns",
        component: "Editor/Prompt And Cmdline",
        variant: "With Completion",
        summary: "Command prompt, history, validation, and completion positioning.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("prompt", ":write-all", "Command line input"),
            StoryArg::new("history", "4", "History row fixture count"),
            StoryArg::new("completion", "3", "Completion row fixture count"),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_prompt_cmdline_story),
    },
    Story {
        id: "ui/completion-menu",
        title: "Completion Menu",
        category: "UI Patterns",
        component: "Editor/Completion Menu",
        variant: "Documentation",
        summary: "Completion rows, detail panes, documentation preview, and selection semantics.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("items", "6", "Completion candidates"),
            StoryArg::new("selected", "2", "Selected candidate"),
            StoryArg::new("docs", "shown", "Documentation pane visible"),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_completion_menu_story),
    },
    Story {
        id: "ui/popups-overlays",
        title: "Popups And Overlays",
        category: "UI Patterns",
        component: "Editor/Popups And Overlays",
        variant: "Permission Dialog",
        summary: "Popup, overlay, select, menu, notification, and permission dialog shape.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("modal", "permission", "Centered modal state"),
            StoryArg::new("shadow", "enabled", "Depth decorator"),
            StoryArg::new("notice", "bottom", "Notification placement"),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_popups_overlays_story),
    },
    Story {
        id: "ui/file-explorer",
        title: "File Explorer",
        category: "UI Patterns",
        component: "Editor/File Explorer",
        variant: "Default",
        summary:
            "Runtime file explorer panel with docked width, icons, counted header, and selection.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("root", "helix-term", "Real workspace package root"),
            StoryArg::new("width", "runtime", "Uses file explorer docked panel width"),
            StoryArg::new(
                "renderer",
                "FileExplorerPanel",
                "Runtime component renderer",
            ),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_file_explorer_story),
    },
    Story {
        id: "ui/file-explorer/empty",
        title: "Empty",
        category: "UI Patterns",
        component: "Editor/File Explorer",
        variant: "Empty",
        summary: "Runtime file explorer on an empty directory; the root row remains selectable.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("items", "1", "Root row only"),
            StoryArg::new("selection", "0", "Root row selected"),
        ],
        canvas: StoryCanvas::Centered {
            width: 36,
            height: 14,
        },
        render: StoryRenderer::Runtime(render_file_explorer_empty_story),
    },
    Story {
        id: "ui/file-explorer/scrolled",
        title: "Scrolled",
        category: "UI Patterns",
        component: "Editor/File Explorer",
        variant: "Scrolled",
        summary: "File explorer tree panel with non-zero scroll offset.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("root", "workspace", "Real repository root"),
            StoryArg::new("scroll", "4", "Requested top visible tree row"),
            StoryArg::new(
                "selected",
                "7",
                "Selected row remains visible when available",
            ),
        ],
        canvas: StoryCanvas::Centered {
            width: 36,
            height: 12,
        },
        render: StoryRenderer::Runtime(render_file_explorer_scrolled_story),
    },
    Story {
        id: "ui/assistant-panel",
        title: "Assistant Panel",
        category: "UI Patterns",
        component: "Assistant/Panel",
        variant: "Tool Request",
        summary: "Assistant transcript, tool status, permission row, and composer input.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("messages", "conversation", "Transcript fixture"),
            StoryArg::new("tool", "pending", "Tool status row"),
            StoryArg::new("composer", "focused", "Input state"),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_assistant_panel_story),
    },
    Story {
        id: "ui/lsp",
        title: "LSP Surfaces",
        category: "UI Patterns",
        component: "Editor/LSP Surfaces",
        variant: "Overview",
        summary: "Hover markdown, signature help, code actions, symbols, and diagnostics.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("hover", "markdown", "Hover popup content"),
            StoryArg::new("actions", "4", "Code action count"),
            StoryArg::new("symbols", "4", "Symbol list count"),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_lsp_story),
    },
    Story {
        id: "ui/status-notifications",
        title: "Status And Notifications",
        category: "UI Patterns",
        component: "Editor/Status And Notifications",
        variant: "Progress",
        summary: "Statusline, mode blocks, progress state, marquee text, and notifications.",
        kind: StoryKind::Pattern,
        args: &[
            StoryArg::new("mode", "normal", "Statusline mode"),
            StoryArg::new("progress", "active", "Spinner/progress state"),
            StoryArg::new("notification", "success", "Notification severity"),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_status_notifications_story),
    },
    Story {
        id: "components/inputs",
        title: "Inputs And Chrome",
        category: "Components",
        component: "Compositions/Inputs And Chrome",
        variant: "Kitchen Sink",
        summary: "Combined input, picker, scrollbar, and chrome composition.",
        kind: StoryKind::Composite,
        args: &[
            StoryArg::new(
                "includes",
                "input+picker+scrollbar",
                "Composite coverage fixture",
            ),
            StoryArg::new("picker_selected", "1", "Selected picker row"),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Runtime(render_inputs_story),
    },
    Story {
        id: "components/messages",
        title: "Messages",
        category: "Components",
        component: "Compositions/Messages",
        variant: "Compact",
        summary: "Compact message composition used for dense assistant and status content.",
        kind: StoryKind::Composite,
        args: &[
            StoryArg::new("messages", "2", "Compact fixture count"),
            StoryArg::new("selected", "none", "No selected details state"),
        ],
        canvas: StoryCanvas::FullBleed,
        render: StoryRenderer::Styled(render_messages_story),
    },
    Story {
        id: "plugins/render-ops",
        title: "Plugin Render Ops",
        category: "Plugins",
        component: "Plugins/Render Ops",
        variant: "Typed Surface Ops",
        summary: "Lua-facing typed render operations applied directly to a Ratatui buffer.",
        kind: StoryKind::Contract,
        args: &[
            StoryArg::new("ops", "6", "Typed render operation count"),
            StoryArg::new("surface", "ratatui", "Native terminal render target"),
        ],
        canvas: StoryCanvas::Padded { x: 1, y: 1 },
        render: StoryRenderer::Styled(render_plugin_ops_story),
    },
];

pub fn stories() -> &'static [Story] {
    STORIES
}

pub fn run_cli(args: impl IntoIterator<Item = String>) -> Result<i32> {
    let mut command = Command::Interactive {
        story_id: STORIES[0].id.to_string(),
    };
    let mut theme_choice = ThemeChoice::Configured;
    let mut theme_mode = None;
    let mut tick = 0;

    let mut args = args.into_iter().peekable();
    if args.peek().is_none() {
        run_interactive(STORIES[0].id, theme_choice, theme_mode)?;
        return Ok(0);
    }

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                return Ok(0);
            }
            "--list" => command = Command::List,
            "--themes" => command = Command::Themes,
            "--theme" => {
                let theme = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--theme requires a value"))?;
                theme_choice = ThemeChoice::parse(&theme);
            }
            "--theme-mode" => {
                let mode = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--theme-mode requires a value"))?;
                theme_mode = Some(parse_theme_mode(&mode)?);
            }
            "--tick" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--tick requires a value"))?;
                tick = value
                    .parse::<u64>()
                    .map_err(|_| anyhow::anyhow!("--tick must be an integer"))?;
            }
            "--interactive" => {
                let story_id = args
                    .next_if(|arg| !arg.starts_with('-'))
                    .unwrap_or_else(|| STORIES[0].id.to_string());
                command = Command::Interactive { story_id };
            }
            "--dump" => {
                let story_id = args.next().unwrap_or_else(|| STORIES[0].id.to_string());
                let (width, height) = command.size();
                command = Command::Dump {
                    story_id,
                    width,
                    height,
                };
            }
            "--width" => {
                let width = parse_dimension("--width", args.next())?;
                command.set_width(width);
            }
            "--height" => {
                let height = parse_dimension("--height", args.next())?;
                command.set_height(height);
            }
            other => bail!("unexpected argument: {other}"),
        }
    }

    match command {
        Command::List => print_story_list(),
        Command::Themes => print_theme_list()?,
        Command::Interactive { story_id } => run_interactive(&story_id, theme_choice, theme_mode)?,
        Command::Dump {
            story_id,
            width,
            height,
        } => {
            let theme = load_storybook_theme(&theme_choice, theme_mode)?.with_tick(tick);
            if story_id == "all" {
                for story in STORIES {
                    println!("--- {} ---", story.id);
                    println!(
                        "{}",
                        dump_story_with_theme(story.id, width, height, &theme)?
                    );
                }
            } else {
                println!(
                    "{}",
                    dump_story_with_theme(&story_id, width, height, &theme)?
                );
            }
        }
    }

    Ok(0)
}

#[cfg(windows)]
fn run_interactive(
    story_id: &str,
    theme_choice: ThemeChoice,
    theme_mode: Option<helix_theme::Mode>,
) -> Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use tui::{backend::CrosstermBackend, ratatui::TerminalSession};

    let initial_index = story_index(story_id)?;
    let themes = available_theme_names();
    let mut active_theme = load_storybook_theme(&theme_choice, theme_mode)?;
    let mut theme_index = themes
        .iter()
        .position(|theme| theme == &active_theme.name)
        .unwrap_or(0);
    let editor_config = helix_view::editor::Config::default();
    let backend = CrosstermBackend::new(std::io::stdout(), (&editor_config).into());
    let mut session = TerminalSession::new(backend)?.claim()?;

    let mut selected = initial_index;
    let animation_started = std::time::Instant::now();
    let result: Result<()> = (|| loop {
        let tick = storybook_tick(animation_started.elapsed());
        session.draw(|frame| {
            let area = frame.area();
            let frame_theme = active_theme.with_tick(tick);
            render_storybook(
                frame.buffer_mut(),
                area.width,
                area.height,
                STORIES[selected].id,
                &frame_theme,
            );
        })?;

        if event::poll(Duration::from_millis(STORYBOOK_TICK_MS))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => break Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(STORIES.len().saturating_sub(1));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => selected = STORIES.len().saturating_sub(1),
                    KeyCode::Tab | KeyCode::Char(']') => {
                        if !themes.is_empty() {
                            theme_index = (theme_index + 1) % themes.len();
                            active_theme = load_named_storybook_theme(&themes[theme_index])?;
                        }
                    }
                    KeyCode::BackTab | KeyCode::Char('[') => {
                        if !themes.is_empty() {
                            theme_index =
                                (theme_index + themes.len().saturating_sub(1)) % themes.len();
                            active_theme = load_named_storybook_theme(&themes[theme_index])?;
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    })();

    let restore_result = session.restore();
    result?;
    restore_result?;
    Ok(())
}

#[cfg(not(windows))]
fn run_interactive(
    _story_id: &str,
    _theme_choice: ThemeChoice,
    _theme_mode: Option<helix_theme::Mode>,
) -> Result<()> {
    bail!("interactive storybook currently requires the crossterm terminal backend")
}

pub fn dump_story(story_id: &str, width: u16, height: u16) -> Result<String> {
    // Dumps are used by tests and CI snapshots, so they must be deterministic
    // and independent of the user's config. We load the named theme but pair
    // it with the default editor config so mode labels, bufferline visibility,
    // statusline elements, etc. don't drift with personal customizations.
    let theme = theme_loader().load("default")?;
    let editor_config = helix_view::editor::Config::default();
    let loaded = LoadedTheme::new(theme, editor_config);
    dump_story_with_theme(story_id, width, height, &loaded)
}

fn dump_story_with_theme(
    story_id: &str,
    width: u16,
    height: u16,
    theme: &LoadedTheme,
) -> Result<String> {
    let story = STORIES[story_index(story_id)?];
    let mut surface = Buffer::empty(RatatuiRect::new(0, 0, width, height));
    render_storybook(&mut surface, width, height, story.id, theme);
    Ok(buffer_to_string(&surface, width, height))
}

fn story_index(story_id: &str) -> Result<usize> {
    STORIES
        .iter()
        .position(|story| story.id == story_id)
        .ok_or_else(|| anyhow::anyhow!("unknown story: {story_id}"))
}

fn render_tokens_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    let rows = [
        ("surface", styles.surface, "base page background"),
        ("panel", styles.panel, "contained tool or popup surface"),
        ("border", styles.border, "low-contrast structural frame"),
        ("text", styles.text, "primary readable text"),
        ("muted", styles.muted, "secondary labels and metadata"),
        (
            "accent",
            styles.accent,
            "active affordance and current focus",
        ),
        ("selected", styles.selected, "selected row or option"),
        ("success", styles.success, "completed or healthy state"),
        ("warning", styles.warning, "recoverable attention state"),
        ("error", styles.error, "failure or destructive state"),
        ("info", styles.info, "informational status"),
    ];

    for (idx, (name, style, note)) in rows.into_iter().enumerate() {
        let y = area.y.saturating_add(idx as u16);
        if y >= area.bottom() {
            break;
        }
        surface.set_stringn(area.x, y, format!("{name:<10}"), 10, styles.rat(style));
        surface.set_stringn(
            area.x.saturating_add(12),
            y,
            note,
            area.width.saturating_sub(12) as usize,
            styles.rat(styles.text),
        );
    }

    let plugin_y = area.y.saturating_add(rows.len() as u16 + 2);
    if plugin_y < area.bottom() {
        surface.set_stringn(
            area.x,
            plugin_y,
            "Lua plugins should ask for semantic scopes, not raw colors.",
            area.width as usize,
            styles.rat(styles.accent),
        );
        surface.set_stringn(
            area.x,
            plugin_y.saturating_add(1),
            "Recommended scopes: ui.text, ui.text.focus, ui.selection, ui.window, warning, error, hint, info",
            area.width as usize,
            styles.rat(styles.muted),
        );
    }
}

fn render_layout_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    let root = tui::ratatui::to_ratatui_rect(area);
    let [main, status] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(2)])
        .areas(root);
    let [explorer, editor, assistant] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(24),
            Constraint::Min(30),
            Constraint::Length(30),
        ])
        .areas(main);

    let explorer = tui::ratatui::to_helix_rect(explorer);
    let editor = render_panel(
        surface,
        tui::ratatui::to_helix_rect(editor),
        "editor viewport",
        styles,
    );
    let assistant = render_panel(
        surface,
        tui::ratatui::to_helix_rect(assistant),
        "right panel",
        styles,
    );

    let tree_items = story_file_tree_items();
    let explorer_inner = crate::widgets::Panel::edge(
        crate::widgets::PanelStyle::new(styles.surface, styles.border, styles.statusline),
        crate::widgets::PanelEdge::Right,
    )
    .render(surface, explorer);
    crate::widgets::header_with_counts(
        surface,
        explorer_inner,
        " Files",
        8,
        tree_items.len(),
        styles.statusline,
    );
    crate::widgets::tree_list(
        surface,
        explorer_inner.clip_top(1).clip_left(1),
        &tree_items,
        tree_list_styles(styles),
        Some("No files"),
    );

    for row in 0..editor.height.min(8) {
        let y = editor.y + row;
        let gutter = format!("{:>4}", 41 + row);
        surface.set_stringn(editor.x, y, gutter, 4, styles.rat(styles.muted));
        let line = match row {
            0 => "pub fn render(frame: &mut Frame<'_>) {",
            1 => "    let model = view.snapshot();",
            2 => "    widgets::editor(model).render(area, frame);",
            3 => "}",
            5 => "// overlays anchor to content, not absolute guesses",
            _ => "",
        };
        surface.set_stringn(
            editor.x.saturating_add(6),
            y,
            line,
            editor.width.saturating_sub(6) as usize,
            styles.rat(if row == 5 { styles.muted } else { styles.text }),
        );
    }

    for (row, text) in [
        "message list",
        "tool calls",
        "permission row",
        "composer input",
    ]
    .into_iter()
    .enumerate()
    {
        set_clipped(surface, assistant, row as u16, text, styles.text);
    }

    fill_rect(
        surface,
        tui::ratatui::to_helix_rect(status),
        styles.selected,
    );
    let status = tui::ratatui::to_helix_rect(status);
    set_clipped(
        surface,
        status,
        0,
        "NORMAL  main.rs  utf-8  rust-analyzer ready  42:9",
        styles.text,
    );
}

fn render_widget_style_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    let bundle = crate::widgets::WidgetStyle {
        text: styles.text,
        cursor: styles.selected,
        placeholder: styles.muted,
        selection: styles.selected,
        border: styles.border,
    };
    let rows = [
        ("text", bundle.text, "primary widget text"),
        ("cursor", bundle.cursor, "focused cursor cell"),
        ("placeholder", bundle.placeholder, "empty input hint"),
        ("selection", bundle.selection, "selected row or range"),
        ("border", bundle.border, "focused frame"),
    ];

    for (idx, (name, style, note)) in rows.into_iter().enumerate() {
        let y = area.y.saturating_add(idx as u16 * 2);
        if y >= area.bottom() {
            break;
        }
        surface.set_stringn(area.x, y, format!("{name:<12}"), 12, styles.rat(style));
        surface.set_stringn(
            area.x.saturating_add(14),
            y,
            note,
            area.width.saturating_sub(14) as usize,
            styles.rat(styles.text),
        );
        if y + 1 < area.bottom() {
            crate::widgets::hdivider(
                surface,
                Rect::new(area.x, y + 1, area.width.min(72), 1),
                styles.border,
            );
        }
    }
}

fn render_chrome_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    let [top, middle, bottom] = split_vertical(area, [3, 9, area.height.saturating_sub(12)]);

    crate::widgets::header(surface, top, "Header: focused tool surface", styles.accent);
    crate::widgets::header_with_counts(
        surface,
        Rect::new(
            top.x,
            top.y.saturating_add(1),
            top.width.min(60),
            top.height.saturating_sub(1),
        ),
        "Counted header",
        12,
        164,
        styles.info,
    );

    let [left, right] = split_horizontal(middle, 42);
    let left_inner = render_panel(surface, left, "rounded border", styles);
    set_clipped(
        surface,
        left_inner,
        0,
        "quiet structure around dense rows",
        styles.text,
    );
    set_clipped(
        surface,
        left_inner,
        2,
        "dividers separate regions",
        styles.muted,
    );
    if left_inner.height > 3 {
        crate::widgets::hdivider(
            surface,
            Rect::new(left_inner.x, left_inner.y + 3, left_inner.width, 1),
            styles.border,
        );
    }
    let right_inner = render_panel_with_corners(surface, right, "square grid", styles, false);
    if right_inner.width > 2 {
        crate::widgets::vdivider(
            surface,
            Rect::new(
                right_inner.x.saturating_add(right_inner.width / 2),
                right_inner.y,
                1,
                right_inner.height,
            ),
            styles.border,
        );
    }
    set_clipped(surface, right_inner, 0, "left pane", styles.text);
    set_clipped(
        surface,
        Rect::new(
            right_inner.x.saturating_add(right_inner.width / 2 + 2),
            right_inner.y,
            right_inner.width / 2,
            right_inner.height,
        ),
        0,
        "right pane",
        styles.text,
    );

    if bottom.height > 0 {
        set_clipped(
            surface,
            bottom,
            0,
            "Widget chrome stays functional: headers, counts, frame edges, and separators are reusable primitives.",
            styles.text,
        );
    }
}

fn render_text_input_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    let rows = [
        (
            "focused command",
            ":theme double-helix-dark --preview",
            18usize,
        ),
        (
            "long path",
            "open C:/Users/jonfo/source/double-helix/runtime/plugin-panel/storybook.rs",
            58usize,
        ),
        ("search", "/render\\(_surface, area, style\\)", 9usize),
        ("empty hint", "", 0usize),
    ];

    for (index, (label, text, cursor)) in rows.into_iter().enumerate() {
        let y = area.y.saturating_add(index as u16 * 4);
        if y >= area.bottom() {
            break;
        }
        surface.set_stringn(area.x, y, label, 18, styles.rat(styles.muted));
        let input = Rect::new(
            area.x.saturating_add(20),
            y,
            area.width.saturating_sub(22).min(62),
            1,
        );
        fill_rect(surface, input, styles.panel);
        if text.is_empty() {
            surface.set_stringn(
                input.x,
                input.y,
                "type command...",
                input.width as usize,
                styles.rat(styles.muted),
            );
        } else {
            let state = crate::widgets::text_input(
                surface,
                input,
                text,
                cursor,
                styles.text,
                styles.selected,
            );
            let state_row = Rect::new(area.x.saturating_add(20), y + 1, input.width, 1);
            let summary = format!(
                "cursor_x={} anchor={} start={} end={}",
                state.cursor_x, state.anchor, state.truncated_start, state.truncated_end
            );
            set_clipped(surface, state_row, 0, &summary, styles.muted);
        }
    }
}

fn render_text_input_focused_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.panel);
    crate::widgets::text_input(
        surface,
        area,
        ":theme double-helix-dark --preview",
        18,
        styles.text,
        styles.selected,
    );
}

fn render_text_input_long_path_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.panel);
    crate::widgets::text_input(
        surface,
        area,
        "open C:/Users/jonfo/source/double-helix/runtime/plugin-panel/storybook.rs",
        58,
        styles.text,
        styles.selected,
    );
}

fn render_text_input_empty_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.panel);
    crate::widgets::text_input(surface, area, "", 0, styles.text, styles.selected);
}

fn render_item_list_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    let root = tui::ratatui::to_ratatui_rect(area);
    let [list, details] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(38), Constraint::Min(30)])
        .areas(root);
    let list = render_panel(
        surface,
        tui::ratatui::to_helix_rect(list),
        "results",
        styles,
    );
    let details = render_panel(
        surface,
        tui::ratatui::to_helix_rect(details),
        "state",
        styles,
    );

    let items = [
        "helix-term/src/ui/editor.rs",
        "helix-term/src/ui/picker.rs",
        "helix-term/src/ui/prompt.rs",
        "helix-term/src/ui/menu.rs",
        "helix-term/src/ui/popup.rs",
        "helix-term/src/ui/assistant.rs",
        "helix-term/src/ui/file_explorer.rs",
        "helix-term/src/widgets/item_list.rs",
        "helix-term/src/widgets/message.rs",
        "helix-term/src/widgets/surface.rs",
        "helix-plugin/src/lua/api/facade.rs",
    ];
    let list_styles = crate::widgets::ListStyles {
        normal: styles.menu,
        selected: styles.menu_selected,
        scrollbar_thumb: styles.menu_scroll,
        scrollbar_track: styles.popup_border,
    };
    let state = crate::widgets::item_list(
        surface,
        list,
        items.len(),
        Some(5),
        2,
        &list_styles,
        |idx, row, surface, selected| {
            let marker = if selected { "> " } else { "  " };
            let style = if selected { styles.text } else { styles.muted };
            surface.set_stringn(
                row.x,
                row.y,
                format!("{marker}{}", items[idx]),
                row.width as usize,
                tui::ratatui::to_ratatui_style(style),
            );
        },
    );

    for (row, text) in [
        format!("scroll: {}", state.scroll),
        format!("visible: {}..{}", state.visible_start, state.visible_end),
        "row renderer is caller-owned".to_string(),
        "widget owns selection bg and scrollbar".to_string(),
    ]
    .into_iter()
    .enumerate()
    {
        set_clipped(surface, details, row as u16, &text, styles.text);
    }
}

fn render_scrolling_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    let root = tui::ratatui::to_ratatui_rect(area);
    let [region, standalone] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Min(18)])
        .areas(root);
    let region = render_panel(
        surface,
        tui::ratatui::to_helix_rect(region),
        "scroll_region",
        styles,
    );
    let standalone = render_panel(
        surface,
        tui::ratatui::to_helix_rect(standalone),
        "scrollbar",
        styles,
    );

    let lines: Vec<Spans<'static>> = (0..32)
        .map(|line| {
            Spans::from(vec![
                HSpan::styled(format!("{line:02} "), styles.muted),
                HSpan::styled(
                    "rendered styled text line for docs, preview panes, or logs",
                    if line == 12 {
                        styles.selected
                    } else {
                        styles.text
                    },
                ),
            ])
        })
        .collect();
    let scroll_styles = crate::widgets::ScrollStyles {
        thumb: styles.accent,
        track: styles.border,
    };
    let state = crate::widgets::scroll_region(surface, region, &lines, 10, true, &scroll_styles);

    set_clipped(
        surface,
        standalone,
        0,
        &format!("max_scroll: {}", state.max_scroll),
        styles.text,
    );
    set_clipped(
        surface,
        standalone,
        1,
        &format!("visible_lines: {}", state.visible_lines),
        styles.text,
    );
    if standalone.height > 4 {
        let total = 120;
        let visible = 18;
        let scroll_offset = cycling_scroll_offset(total, visible, styles.tick, 3);
        crate::widgets::Scrollbar::new(total, scroll_offset, visible)
            .symbol("▌")
            .thumb_style(styles.accent)
            .track("·", styles.border)
            .render(
                Rect::new(
                    standalone.x.saturating_add(standalone.width / 2),
                    standalone.y.saturating_add(3),
                    1,
                    standalone.height.saturating_sub(3),
                ),
                surface,
            );
    }
}

fn render_message_widget_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    let message_style = crate::widgets::MessageStyle {
        border: styles.border,
        corners: crate::widgets::MessageCorners::Rounded,
        accent: Some(styles.accent),
        accent_progress: pulse(styles.tick, 18),
    };
    let square_style = crate::widgets::MessageStyle {
        corners: crate::widgets::MessageCorners::Squared,
        accent_progress: pulse(styles.tick + 6, 18),
        ..message_style
    };

    let left = vec![
        hline("Tool result is ready.", styles.text),
        hline("The row is compact and left anchored.", styles.muted),
    ];
    crate::widgets::message(
        surface,
        Rect::new(area.x, area.y, area.width, 5.min(area.height)),
        &left,
        area.width.saturating_sub(12).min(54),
        crate::widgets::MessageAlign::Left,
        message_style,
        0,
    );

    let right_y = area.y.saturating_add(6);
    if right_y < area.bottom() {
        let right = vec![
            hline("Keep the UI opinionated.", styles.text),
            hline("Plugins should inherit the same states.", styles.muted),
        ];
        crate::widgets::message(
            surface,
            Rect::new(area.x, right_y, area.width, 5.min(area.bottom() - right_y)),
            &right,
            area.width.saturating_sub(16).min(48),
            crate::widgets::MessageAlign::Right,
            square_style,
            0,
        );
    }
}

fn render_message_list_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    let message_style = crate::widgets::MessageStyle {
        border: styles.border,
        corners: crate::widgets::MessageCorners::Rounded,
        accent: Some(styles.accent),
        accent_progress: pulse(styles.tick, 20),
    };
    let items = vec![
        crate::widgets::Message::plain(vec![Spans::from(vec![
            HSpan::styled("system ", styles.muted),
            HSpan::styled(
                format!("storybook rendered {} surfaces", STORIES.len()),
                styles.success,
            ),
        ])])
        .with_accessory(
            vec![hline("deterministic dump", styles.muted)],
            crate::widgets::MessageAccessoryAlign::Right,
        ),
        crate::widgets::Message::bubble(
            Some(("assistant".to_string(), styles.success)),
            vec![
                hline(
                    "The catalog should expose component density and state.",
                    styles.text,
                ),
                hline(
                    "Selected messages can reveal details without a second widget.",
                    styles.muted,
                ),
            ],
            area.width.saturating_sub(10).min(58),
            crate::widgets::MessageAlign::Left,
            message_style,
        )
        .with_selected_details(vec![hline(
            "details: applied render ops, state model, and layout measurements",
            styles.info,
        )])
        .with_selected_bar("▎", styles.accent),
        crate::widgets::Message::bubble(
            Some(("you".to_string(), styles.info)),
            vec![hline(
                "Make sure it covers the full UI surface.",
                styles.text,
            )],
            area.width.saturating_sub(16).min(46),
            crate::widgets::MessageAlign::Right,
            message_style,
        )
        .with_selected_accessory(
            vec![hline("reply draft", styles.warning)],
            crate::widgets::MessageAccessoryAlign::Right,
        ),
    ];

    let state = crate::widgets::message_list(surface, area, &items, 0, Some(1));
    if area.height > 2 {
        let footer = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        fill_rect(surface, footer, styles.panel);
        set_clipped(
            surface,
            footer,
            0,
            &format!(
                "items={} visible={}..{} total_height={}",
                state.len(),
                state.visible_start,
                state.visible_end,
                state.total_height
            ),
            styles.muted,
        );
    }
}

fn render_anchored_text_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    let samples = [
        (
            "end",
            "src/ui/editor.rs:render_view paints directly into the Ratatui buffer",
            false,
            true,
        ),
        (
            "start",
            "C:/Users/jonfo/source/double-helix/helix-term/src/storybook.rs",
            true,
            false,
        ),
        (
            "both",
            "plugin://assistant/panel/status/permission-request/very-long-command-name",
            true,
            true,
        ),
    ];

    for (idx, (label, text, start, end)) in samples.into_iter().enumerate() {
        let y = area.y.saturating_add(idx as u16 * 3);
        if y >= area.bottom() {
            break;
        }
        surface.set_stringn(area.x, y, label, 8, styles.rat(styles.muted));
        let text_area = Rect::new(
            area.x.saturating_add(10),
            y,
            area.width.saturating_sub(12).min(64),
            1,
        );
        fill_rect(surface, text_area, styles.panel);
        let mut style_for_offset = |offset: usize| {
            styles.rat(if offset == 0 || offset >= text.len() {
                styles.accent
            } else {
                styles.text
            })
        };
        crate::widgets::draw_string_anchored(
            surface,
            crate::widgets::AnchoredText::new(
                text_area.x,
                text_area.y,
                text,
                text_area.width as usize,
            )
            .truncate_start(start)
            .truncate_end(end),
            &mut style_for_offset,
        );
    }
}

fn render_spinner_shadow_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    let card = Rect::new(
        area.x.saturating_add(4),
        area.y.saturating_add(3),
        area.width.saturating_sub(10).min(56),
        area.height.saturating_sub(6).min(10),
    );
    crate::widgets::BoxShadow::new()
        .offset(2, 1)
        .blur(2)
        .spread(1)
        .opacity(0.55)
        .render(surface, card);
    fill_rect(surface, card, styles.panel);
    crate::widgets::border(surface, card, styles.border, true);

    let inner = inset(card, 2, 1);
    let inset_card = Rect::new(
        card.x.saturating_add(card.width.saturating_sub(18)),
        card.y.saturating_add(2),
        14.min(card.width),
        5.min(card.height),
    );
    let text_inner = Rect::new(
        inner.x,
        inner.y,
        inset_card.x.saturating_sub(inner.x).saturating_sub(1),
        inner.height,
    );
    let spinner = crate::widgets::Spinner::new(
        &["◐", "◓", "◑", "◒"],
        Duration::from_millis(STORYBOOK_TICK_MS),
    );
    set_clipped(
        surface,
        text_inner,
        1,
        &format!(
            "{} indexing workspace symbols",
            spinner.frame_for_elapsed(tick_elapsed(styles.tick))
        ),
        styles.accent,
    );
    set_clipped(
        surface,
        text_inner,
        3,
        "redraw owned by component",
        styles.muted,
    );

    crate::widgets::BoxShadow::new()
        .inset(true)
        .blur(2)
        .opacity(0.45)
        .render(surface, inset_card);
    crate::widgets::border(surface, inset_card, styles.border, false);
    set_clipped(surface, inset(inset_card, 1, 1), 1, "inset", styles.text);
}

fn render_editor_shell_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    Stage::new(area, context)
        .with_modified_document("storybook.rs", EDITOR_SHELL_FIXTURE)
        .render_editor_view(surface);
}

const EDITOR_SHELL_FIXTURE: &str = "use crate::compositor::{Component, RenderContext};\n\
                                    use crate::ui::popup::Popup;\n\
                                    \n\
                                    impl Component for EditorView {\n\
                                    \x20   fn render(&mut self, area: Rect, surface: &mut Buffer, cx: &RenderContext) {\n\
                                    \x20       self.draw_bufferline_model(&model, area.with_height(1), surface);\n\
                                    \x20       self.render_document(surface, inner, doc, view_offset, ...);\n\
                                    \x20       self.prepare_statusline(cx, doc, view, is_focused);\n\
                                    \x20   }\n\
                                    }\n\
                                    \n\
                                    // hint: press <space>? for the command palette\n";

fn render_key_help_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    let mut stage =
        Stage::new(area, context).with_modified_document("storybook.rs", EDITOR_SHELL_FIXTURE);
    let mut editor_view = harness::build_editor_view();
    stage.draw(surface, &mut editor_view);
    let mut info = space_key_infobox().clone();
    stage.draw(surface, &mut info);
}

fn render_autoinfo_scroll_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    let mut stage =
        Stage::new(area, context).with_modified_document("storybook.rs", EDITOR_SHELL_FIXTURE);
    let mut editor_view = harness::build_editor_view();
    stage.draw(surface, &mut editor_view);
    let mut info = space_key_infobox().clone();
    let popup_area = design::info_popup_area(area, &info);
    let visible_body_height = design::info_popup_body_height(popup_area);
    info.scroll_to(info.max_scroll(visible_body_height), visible_body_height);
    stage.draw(surface, &mut info);
}

fn space_key_infobox() -> &'static Info {
    static SPACE_INFO: OnceLock<Info> = OnceLock::new();
    SPACE_INFO.get_or_init(|| {
        let keymaps = crate::keymap::default::default();
        let space = "space"
            .parse()
            .expect("default key event syntax for space must parse");
        keymaps
            .get(&Mode::Normal)
            .and_then(|trie| trie.search(&[space]))
            .and_then(crate::keymap::KeyTrie::node)
            .expect("default normal-mode Space keymap must exist")
            .infobox()
    })
}

#[derive(Debug, Clone)]
struct StoryPickerItem {
    path: &'static str,
    kind: &'static str,
}

fn story_picker_path<'a>(item: &'a StoryPickerItem, _data: &'a ()) -> crate::ui::menu::Cell<'a> {
    item.path.into()
}

fn story_picker_kind<'a>(item: &'a StoryPickerItem, _data: &'a ()) -> crate::ui::menu::Cell<'a> {
    item.kind.into()
}

fn story_picker_rows() -> Vec<StoryPickerItem> {
    vec![
        StoryPickerItem {
            path: "helix-term/src/ui/editor.rs",
            kind: "modified",
        },
        StoryPickerItem {
            path: "helix-term/src/ui/picker.rs",
            kind: "file",
        },
        StoryPickerItem {
            path: "helix-term/src/ui/prompt.rs",
            kind: "file",
        },
        StoryPickerItem {
            path: "helix-term/src/ui/plugin_panel.rs",
            kind: "file",
        },
        StoryPickerItem {
            path: "helix-term/src/widgets/text_input.rs",
            kind: "widget",
        },
        StoryPickerItem {
            path: "helix-term/src/widgets/message_list.rs",
            kind: "widget",
        },
    ]
}

fn story_picker_long_path_rows() -> Vec<StoryPickerItem> {
    vec![
        StoryPickerItem {
            path: "C:/Users/jonfo/source/double-helix/runtime/plugin-panel/storybook.rs",
            kind: "file",
        },
        StoryPickerItem {
            path: "C:/Users/jonfo/source/double-helix/helix-term/src/ui/file_explorer.rs",
            kind: "file",
        },
        StoryPickerItem {
            path: "C:/Users/jonfo/source/double-helix/helix-term/src/ui/picker.rs",
            kind: "file",
        },
        StoryPickerItem {
            path: "C:/Users/jonfo/source/double-helix/helix-term/src/widgets/picker_table.rs",
            kind: "file",
        },
    ]
}

fn story_picker(
    editor: &Editor,
    ingress: crate::runtime::RuntimeIngress,
    rows: Vec<StoryPickerItem>,
    query: &str,
    cursor: u32,
    truncate_start: bool,
    show_kind: bool,
) -> crate::ui::Picker<StoryPickerItem, ()> {
    let picker = if show_kind {
        crate::ui::Picker::new(
            [
                crate::ui::PickerColumn::new("path", story_picker_path),
                crate::ui::PickerColumn::new("kind", story_picker_kind),
            ],
            0,
            rows,
            (),
            crate::ui::PickerRuntime::new(editor),
            ingress,
            |_cx, _item, _action| {},
        )
    } else {
        crate::ui::Picker::new(
            [crate::ui::PickerColumn::new("path", story_picker_path)],
            0,
            rows,
            (),
            crate::ui::PickerRuntime::new(editor),
            ingress,
            |_cx, _item, _action| {},
        )
    };

    picker
        .show_preview(false)
        .truncate_start(truncate_start)
        .with_query(query, editor)
        .with_cursor(cursor)
}

fn render_story_picker_component(
    surface: &mut Buffer,
    area: Rect,
    context: StoryContext<'_>,
    rows: Vec<StoryPickerItem>,
    query: &str,
    cursor: u32,
    truncate_start: bool,
    show_kind: bool,
) {
    let mut stage = Stage::new(area, context);
    let mut picker = story_picker(
        stage.editor(),
        stage.ingress(),
        rows,
        query,
        cursor,
        truncate_start,
        show_kind,
    );
    // Block on the fuzzy matcher so the snapshot is final before render.
    // Without this, render's own 10ms tick can race the rayon worker pool
    // under parallel test load and emit a partial result.
    picker.drain_matcher();
    stage.draw(surface, &mut picker);
}

fn render_picker_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    render_story_picker_component(
        surface,
        area,
        context,
        story_picker_rows(),
        "ui",
        1,
        false,
        true,
    );
}

fn render_picker_empty_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    render_story_picker_component(
        surface,
        area,
        context,
        Vec::new(),
        "zz-no-match",
        0,
        false,
        true,
    );
}

fn render_picker_long_paths_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    render_story_picker_component(
        surface,
        area,
        context,
        story_picker_long_path_rows(),
        "helix",
        2,
        true,
        false,
    );
}
fn render_prompt_cmdline_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    let mut stage =
        Stage::new(area, context).with_modified_document("storybook.rs", EDITOR_SHELL_FIXTURE);

    let mut editor_view = harness::build_editor_view();
    stage.draw(surface, &mut editor_view);

    let mut prompt = build_cmdline_prompt(stage.editor(), "wr");
    let height = 6.min(area.height);
    let prompt_area = Rect::new(
        area.x,
        area.bottom().saturating_sub(height),
        area.width,
        height,
    );
    stage.draw_in(prompt_area, surface, &mut prompt);
}

fn build_cmdline_prompt(editor: &Editor, initial: &str) -> crate::ui::Prompt {
    use crate::ui::prompt::{Completion, PromptEvent};
    let completer = |_editor: &Editor, input: &str| -> Vec<Completion> {
        const COMMANDS: &[&str] = &[
            "write",
            "write-all",
            "write-quit",
            "write-quit-all",
            "write-buffer-close",
        ];
        COMMANDS
            .iter()
            .filter(|name| name.starts_with(input))
            .map(|name| (0.., (*name).into()))
            .collect()
    };
    let callback = |_cx: &mut crate::compositor::Context, _input: &str, _event: PromptEvent| {};
    // The "Cmdline" label is rewritten to `:` in CmdlineStyle::Bottom (which
    // is the default), giving us the real cmdline appearance for free.
    let mut prompt = crate::ui::Prompt::new("Cmdline".into(), None, completer, callback)
        .with_line(initial.into(), editor);
    prompt.recalculate_completion(editor);
    prompt
}

fn render_completion_menu_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    use crate::handlers::completion::{CompletionItem, ResolveRuntime};
    use helix_core::completion::CompletionProvider;
    use helix_core::Transaction;

    let mut stage =
        Stage::new(area, context).with_modified_document("storybook.rs", EDITOR_SHELL_FIXTURE);

    let mut editor_view = harness::build_editor_view();
    stage.draw(surface, &mut editor_view);

    let doc_text = stage
        .editor()
        .documents()
        .next()
        .map(|doc| doc.text().clone())
        .unwrap_or_default();
    let make_item = |label: &'static str, kind: &'static str, docs: &'static str| {
        CompletionItem::Other(helix_core::CompletionItem {
            transaction: Transaction::new(&doc_text),
            label: label.into(),
            kind: kind.into(),
            documentation: Some(docs.to_string()),
            provider: CompletionProvider::Word,
        })
    };
    let items = vec![
        make_item(
            "render_storybook",
            "fn",
            "Render the storybook chrome and the active story panel.",
        ),
        make_item(
            "render_story_panel",
            "fn",
            "Render the body of the currently selected story.",
        ),
        make_item(
            "render_plugin_ops_story",
            "fn",
            "Story: apply typed Lua render operations directly to a Ratatui buffer.",
        ),
        make_item(
            "RenderOutput",
            "struct",
            "Owned render target used by `Component::prepare_render`.",
        ),
        make_item(
            "RenderMetrics",
            "struct",
            "Timing counters captured during one frame.",
        ),
        make_item(
            "Renderer",
            "trait",
            "Anything that can render into a `RenderOutput`.",
        ),
    ];

    let resolve_runtime = ResolveRuntime::new(stage.editor().runtime());
    let ingress = stage.ingress();
    let mut completion =
        crate::ui::Completion::new(stage.editor(), items, 0, resolve_runtime, ingress);
    // Select `render_plugin_ops_story` so the docs preview pane renders.
    for _ in 0..3 {
        completion.move_down();
    }
    stage.draw(surface, &mut completion);
}

fn render_popups_overlays_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    use helix_core::Position;

    let mut stage = Stage::new(area, context)
        .with_modified_document("storybook.rs", EDITOR_SHELL_FIXTURE)
        .with_status("formatter completed with 2 changed files");

    let mut editor_view = harness::build_editor_view();
    stage.draw(surface, &mut editor_view);

    // Real Popup<Text> anchored near the centre of the canvas.
    let permission_text = crate::ui::Text::new(
        "Plugin wants to run:\n  cargo test -p helix-term storybook\n\n\
         > Allow once\n  Deny\n  Always allow for workspace"
            .to_string(),
    );
    let anchor = Position::new(
        (area.y + area.height / 2).saturating_sub(4) as usize,
        (area.x + area.width / 2).saturating_sub(20) as usize,
    );
    let mut popup = crate::ui::Popup::new("storybook-permission", permission_text)
        .position(Some(anchor))
        .auto_close(true);
    stage.draw(surface, &mut popup);
}

fn render_file_explorer_component(
    surface: &mut Buffer,
    area: Rect,
    context: StoryContext<'_>,
    root: PathBuf,
    cursor: Option<usize>,
    scroll: usize,
) {
    let mut stage = Stage::new(area, context);
    let mut panel = crate::ui::FileExplorerPanel::new_with_cursor(root, stage.editor(), cursor)
        .expect("storybook file explorer root must be readable");
    if scroll > 0 {
        helix_view::traits::Scrollable::scroll_to(&mut panel, scroll);
    }
    stage.draw(surface, &mut panel);
}

fn render_file_explorer_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    fill_rect(surface, area, context.styles.surface);
    let width = area.width.min(crate::ui::FILE_EXPLORER_PANEL_WIDTH);
    let panel_area = Rect::new(area.x, area.y, width, area.height);
    render_file_explorer_component(
        surface,
        panel_area,
        context,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        Some(5),
        0,
    );
}

fn render_file_explorer_empty_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let root = std::env::temp_dir().join(format!(
        "dhx-ui-storybook-empty-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&root).expect("storybook empty file explorer root");
    render_file_explorer_component(surface, area, context, root.clone(), None, 0);
    let _ = std::fs::remove_dir(&root);
}

fn render_file_explorer_scrolled_story(
    surface: &mut Buffer,
    area: Rect,
    context: StoryContext<'_>,
) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("helix-term has workspace parent")
        .to_path_buf();
    render_file_explorer_component(surface, area, context, root, Some(7), 4);
}

fn story_file_tree_items() -> Vec<crate::widgets::TreeListItem<'static>> {
    const CONTINUE_ROOT: &[bool] = &[false];
    const CONTINUE_SRC: &[bool] = &[false, false];

    vec![
        crate::widgets::TreeListItem::new("helix-term").directory(true),
        crate::widgets::TreeListItem::new("src")
            .directory(true)
            .depth(1)
            .last(false),
        crate::widgets::TreeListItem::new("ui")
            .directory(true)
            .depth(2)
            .last(false)
            .ancestors(CONTINUE_ROOT),
        crate::widgets::TreeListItem::new("editor.rs")
            .depth(3)
            .last(false)
            .ancestors(CONTINUE_SRC),
        crate::widgets::TreeListItem::new("picker.rs")
            .depth(3)
            .last(true)
            .ancestors(CONTINUE_SRC),
        crate::widgets::TreeListItem::new("widgets")
            .directory(true)
            .depth(2)
            .last(false)
            .ancestors(CONTINUE_ROOT),
        crate::widgets::TreeListItem::new("message_list.rs")
            .depth(3)
            .last(true)
            .ancestors(CONTINUE_SRC),
        crate::widgets::TreeListItem::new("storybook.rs")
            .depth(2)
            .last(true)
            .ancestors(CONTINUE_ROOT),
        crate::widgets::TreeListItem::new("Cargo.toml")
            .depth(1)
            .last(true),
    ]
}

fn tree_list_styles(styles: UiStyleGuide) -> crate::widgets::TreeListStyles {
    crate::widgets::TreeListStyles {
        background: styles.surface,
        text: styles.text,
        inactive: styles.muted,
        directory: styles.directory,
        guide: styles.border,
        selection: styles.selected,
    }
}

fn render_assistant_panel_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    use helix_view::assistant::{
        event as assistant_event,
        thread::{self, Content, EntryKind, NewEntry, Scope},
    };

    let mut stage =
        Stage::new(area, context).with_modified_document("storybook.rs", EDITOR_SHELL_FIXTURE);

    let thread_id = stage
        .editor_mut()
        .create_local_assistant_thread(Scope::new(std::path::PathBuf::from(".")));

    let push = |editor: &mut Editor, thread: thread::Id, kind: EntryKind| {
        let effects = editor.assistant.apply(assistant_event::Event::Thread {
            thread,
            event: thread::Event::Content(Content::Append(NewEntry {
                turn: None,
                kind,
                locations: Vec::new(),
            })),
        });
        editor.apply_assistant_effects(effects);
    };

    push(
        stage.editor_mut(),
        thread_id,
        EntryKind::UserPrompt {
            text: "Standardize the whole terminal UI surface.".into(),
        },
    );
    push(
        stage.editor_mut(),
        thread_id,
        EntryKind::AssistantText {
            text: "The storybook now tracks widgets and app patterns. \
                   Plugin render ops keep the same semantic look."
                .into(),
        },
    );
    push(
        stage.editor_mut(),
        thread_id,
        EntryKind::Status {
            text: "awaiting permission: install local binary".into(),
        },
    );

    let mut panel = crate::ui::assistant::AssistantPanel::new();
    stage.draw(surface, &mut panel);
}

fn render_lsp_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    use helix_lsp::lsp;

    let mut stage =
        Stage::new(area, context).with_modified_document("storybook.rs", EDITOR_SHELL_FIXTURE);

    let mut editor_view = harness::build_editor_view();
    stage.draw(surface, &mut editor_view);

    // Real LSP hover popup with markdown contents and a multi-server header.
    let hover_md = "```rust\n\
        fn render(&mut self, area: Rect, frame: &mut Frame)\n\
        ```\n\n\
        Renders the component into the Ratatui frame.\n\n\
        Theme scopes are resolved before drawing. Components must own clipping\n\
        for their own area and avoid drawing outside it.\n";
    let hover = lsp::Hover {
        contents: lsp::HoverContents::Markup(lsp::MarkupContent {
            kind: lsp::MarkupKind::Markdown,
            value: hover_md.to_string(),
        }),
        range: None,
    };
    let syn_loader = stage.editor().syn_loader.clone();
    let mut hover_popup =
        crate::ui::lsp::hover::Hover::new(vec![("rust-analyzer".to_string(), hover)], syn_loader);
    stage.draw(surface, &mut hover_popup);
}

fn render_status_notifications_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    let mut stage = Stage::new(area, context)
        .with_modified_document("storybook.rs", EDITOR_SHELL_FIXTURE)
        .with_status("Saved helix-term/src/storybook.rs  ·  cargo fmt queued");

    let mut editor_view = harness::build_editor_view();
    stage.draw(surface, &mut editor_view);
}

fn render_inputs_story(surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
    let styles = context.styles;
    let [top, middle, bottom] = split_vertical(area, [3, 6, area.height.saturating_sub(9)]);

    crate::widgets::header(surface, top, "Command prompt", styles.accent);
    let input_area = Rect::new(
        top.x.saturating_add(2),
        top.y.saturating_add(1),
        top.width.saturating_sub(4),
        1,
    );
    crate::widgets::text_input(
        surface,
        input_area,
        ":theme double-helix-dark --preview",
        18,
        styles.text,
        styles.selected,
    );

    render_story_picker_component(
        surface,
        middle,
        context,
        vec![
            StoryPickerItem {
                path: "ui/editor.rs",
                kind: "file",
            },
            StoryPickerItem {
                path: "ui/plugin_panel.rs",
                kind: "file",
            },
            StoryPickerItem {
                path: "widgets/text_input.rs",
                kind: "widget",
            },
        ],
        "ui",
        1,
        false,
        false,
    );

    let scrollbar_area = Rect::new(
        bottom.right().saturating_sub(1),
        bottom.y,
        1,
        bottom.height.min(8),
    );
    crate::widgets::Scrollbar::new(100, 24, 16)
        .thumb_style(styles.accent)
        .track(" ", styles.border)
        .render(scrollbar_area, surface);
    crate::widgets::vdivider(
        surface,
        Rect::new(bottom.x, bottom.y, 1, bottom.height.min(8)),
        styles.border,
    );
    surface.set_stringn(
        bottom.x.saturating_add(2),
        bottom.y,
        "Chrome should be quiet: clear focus, dense rows, no decorative bulk.",
        bottom.width.saturating_sub(4) as usize,
        styles.rat(styles.text),
    );
}

fn render_messages_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    let message_style = crate::widgets::MessageStyle {
        border: styles.border,
        corners: crate::widgets::MessageCorners::Rounded,
        accent: Some(styles.accent),
        accent_progress: pulse(styles.tick, 20),
    };
    let items = vec![
        crate::widgets::Message::bubble(
            Some(("user".to_string(), styles.info)),
            vec![hline(
                "Can we standardize the assistant panel and plugin popups?",
                styles.text,
            )],
            area.width.saturating_sub(10).min(48),
            crate::widgets::MessageAlign::Right,
            message_style,
        ),
        crate::widgets::Message::bubble(
            Some(("assistant".to_string(), styles.success)),
            vec![
                hline("Yes. Shared tokens define the state language.", styles.text),
                hline(
                    "The message list owns selection, details, and accessories.",
                    styles.muted,
                ),
            ],
            area.width.saturating_sub(8).min(58),
            crate::widgets::MessageAlign::Left,
            message_style,
        )
        .with_selected_details(vec![hline(
            "details: same widget path as the assistant transcript",
            styles.info,
        )])
        .with_selected_bar("▎", styles.accent),
        crate::widgets::Message::plain(vec![Spans::from(vec![
            HSpan::styled("system ", styles.muted),
            HSpan::styled(
                "Plugin render callbacks emit typed ops and stay backend-free.",
                styles.text,
            ),
        ])])
        .with_accessory(
            vec![hline("contract story covers ops", styles.success)],
            crate::widgets::MessageAccessoryAlign::Right,
        ),
    ];

    crate::widgets::message_list(surface, area, &items, 0, Some(1));
}

fn render_plugin_ops_story(surface: &mut Buffer, area: Rect, styles: UiStyleGuide) {
    let mut ops = SurfaceRenderOps::default();
    ops.push(SurfaceRenderOp::Clear {
        area,
        style: styles.panel,
    });
    ops.push(SurfaceRenderOp::Header {
        area: Rect::new(area.x, area.y, area.width, 1),
        title: "Plugin panel from typed render ops".to_string(),
        style: styles.accent,
    });
    ops.push(SurfaceRenderOp::SetString {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(2),
        text: "Lua emits data; term applies directly to Ratatui.".to_string(),
        style: styles.text,
    });
    ops.push(SurfaceRenderOp::SetStringN {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(4),
        text: "Typed operations are applied directly to the active terminal buffer.".to_string(),
        max_width: area.width.saturating_sub(4) as usize,
        style: styles.success,
    });
    ops.push(SurfaceRenderOp::Scrollbar {
        area: Rect::new(
            area.right().saturating_sub(1),
            area.y.saturating_add(1),
            1,
            area.height.saturating_sub(2),
        ),
        total: 40,
        offset: cycling_scroll_offset(40, 12, styles.tick, 1),
        visible: 12,
        thumb_style: styles.accent,
        track_symbol: Some(" ".to_string()),
        track_style: styles.border,
    });

    crate::ui::plugin_render::apply_plugin_render_ops(surface, ops);
}

fn print_usage() {
    println!(
        "\
dhx-ui-storybook

USAGE:
    dhx-ui-storybook
    dhx-ui-storybook --interactive [story-id] [--theme <name|config>] [--theme-mode <dark|light>]
    dhx-ui-storybook --list
    dhx-ui-storybook --themes
    dhx-ui-storybook --dump <story-id|all> [--theme <name|config>] [--theme-mode <dark|light>] [--tick N] [--width N] [--height N]

Interactive keys: Up/Down or k/j select stories, Tab/Shift+Tab or [/ ] switch themes, q exits.

Default story: {}
Dump defaults: --width {} --height {}
",
        STORIES[0].id, DEFAULT_WIDTH, DEFAULT_HEIGHT
    );
}

fn print_story_list() {
    for story in STORIES {
        println!(
            "{:<34} {:<34} {:<22} {}",
            story.id, story.component, story.variant, story.summary
        );
    }
}

fn print_theme_list() -> Result<()> {
    let configured = load_storybook_theme(&ThemeChoice::Configured, None)?.name;
    for name in available_theme_names() {
        let marker = if name == configured { "*" } else { " " };
        println!("{marker} {name}");
    }
    Ok(())
}

fn parse_dimension(name: &str, value: Option<String>) -> Result<u16> {
    let value = value.ok_or_else(|| anyhow::anyhow!("{name} requires a value"))?;
    let parsed = value
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("{name} must be an integer"))?;
    if parsed == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(parsed)
}

enum Command {
    List,
    Themes,
    Interactive {
        story_id: String,
    },
    Dump {
        story_id: String,
        width: u16,
        height: u16,
    },
}

impl Command {
    fn size(&self) -> (u16, u16) {
        match self {
            Self::List | Self::Themes | Self::Interactive { .. } => (DEFAULT_WIDTH, DEFAULT_HEIGHT),
            Self::Dump { width, height, .. } => (*width, *height),
        }
    }

    fn set_width(&mut self, width: u16) {
        if let Self::Dump { width: current, .. } = self {
            *current = width;
        }
    }

    fn set_height(&mut self, height: u16) {
        if let Self::Dump {
            height: current, ..
        } = self
        {
            *current = height;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn story_ids_are_unique() {
        let mut ids = HashSet::new();
        for story in STORIES {
            assert!(ids.insert(story.id), "duplicate story id: {}", story.id);
        }
    }

    #[test]
    fn all_stories_render_visible_content() {
        for story in STORIES {
            let output = dump_story(story.id, 96, 28).unwrap();
            assert!(
                output.contains(story.component) || output.contains(story.variant),
                "story component/variant missing from render: {}",
                story.id
            );
        }
    }

    #[test]
    fn storybook_shell_is_clean_catalog_not_editor_chrome() {
        let output = dump_story("foundations/tokens", 120, 36).unwrap();
        assert!(output.contains("◆ Storybook"));
        assert!(output.contains("STORIES"));
        assert!(output.contains("theme · default"));
        assert!(!output.contains("Double Helix UI"));
        assert!(!output.contains("helix-term/src/storybook.rs"));
        assert!(!output.contains(" NOR "));
    }

    #[test]
    fn editor_shell_story_keeps_helix_component_chrome() {
        let output = dump_story("ui/editor-shell", 120, 36).unwrap();
        assert!(output.contains("◆ Storybook"));
        assert!(output.contains("Editor/Shell"));
        assert!(output.contains("Normal Mode"));
        assert!(output.contains("storybook.rs [+]"));
        assert!(output.contains(" NOR "));
    }

    #[test]
    fn chrome_story_respects_square_panel_corners() {
        let output = dump_story("widgets/chrome", 120, 36).unwrap();
        assert!(output.contains("╭─rounded border"));
        assert!(output.contains("┌─square grid"));
        assert!(!output.contains("╭─square grid"));
    }

    #[test]
    fn component_catalog_uses_current_component_grouping() {
        let output = dump_story("components/inputs", 120, 36).unwrap();
        assert!(output.contains("Compositions/Inputs And Chrome"));
        assert!(output.contains("Kitchen Sink"));
        assert!(!output.contains("Legacy"));
    }

    #[test]
    fn storybook_shell_uses_component_variants_and_args_panel() {
        let output = dump_story("ui/picker", 120, 36).unwrap();
        // Crumb: `${component} · ${variant}` with a middle-dot separator.
        assert!(output.contains("Editor/Picker · Default"));
        // Args panel uses the muted "PROPS" label and bullet-separated meta.
        assert!(output.contains("PROPS"));
        assert!(output.contains("Default · pattern · fullscreen"));
        assert!(output.contains("query"));
        assert!(output.contains("ui"));
    }

    #[test]
    fn component_stories_have_named_variants() {
        for component in [
            "Widgets/Text Input",
            "Editor/Picker",
            "Editor/File Explorer",
        ] {
            let variants = STORIES
                .iter()
                .filter(|story| story.component == component)
                .map(|story| story.variant)
                .collect::<HashSet<_>>();
            assert!(
                variants.len() >= 3,
                "{component} should document multiple states, got {variants:?}"
            );
            assert!(
                variants.contains("Default") || variants.contains("Overview"),
                "{component} should have a baseline story"
            );
        }
    }

    #[test]
    fn component_and_pattern_stories_declare_args() {
        for story in STORIES {
            if matches!(
                story.kind,
                StoryKind::Component
                    | StoryKind::Pattern
                    | StoryKind::Composite
                    | StoryKind::Contract
            ) {
                assert!(
                    !story.args.is_empty(),
                    "{} should document args for controls-style inspection",
                    story.id
                );
            }
        }
    }

    #[test]
    fn runtime_backed_stories_use_runtime_renderer() {
        for id in [
            "ui/key-help",
            "ui/autoinfo-scroll",
            "ui/picker",
            "ui/picker/empty",
            "ui/picker/long-paths",
            "ui/file-explorer",
            "ui/file-explorer/empty",
            "ui/file-explorer/scrolled",
            "components/inputs",
        ] {
            let story = STORIES
                .iter()
                .find(|story| story.id == id)
                .expect("runtime-backed story exists");
            assert!(
                matches!(story.render, StoryRenderer::Runtime(_)),
                "{id} should render through the runtime component path"
            );
        }
    }

    #[test]
    fn inputs_story_uses_runtime_picker_surface() {
        let output = dump_story("components/inputs", 120, 36).unwrap();
        assert!(output.contains("ui"));
        // Picker count now uses a middle-dot separator: `matched · total`.
        assert!(output.contains("2 · 3"));
        assert!(output.contains("ui/plugin_panel.rs"));
        assert!(!output.contains("╭─Picker"));
    }

    #[test]
    fn picker_story_uses_runtime_picker_component_not_fake_panels() {
        let output = dump_story("ui/picker", 120, 36).unwrap();
        assert!(output.contains("ui"));
        assert!(output.contains("4 · 6"));
        // The accent chevron is the picker's signature search affordance.
        assert!(output.contains("›"));
        assert!(output.contains("helix-term/src/ui/picker.rs"));
        assert!(output.contains("helix-term/src/ui/prompt.rs"));
        assert!(!output.contains("╭─matches"));
        assert!(!output.contains("╭─preview"));
        assert!(!output.contains("pub struct Picker<T, D>"));
    }

    #[test]
    fn file_explorer_story_uses_runtime_panel_not_generic_fixture() {
        let output = dump_story("ui/file-explorer", 120, 36).unwrap();
        // Uppercase section label matches the chrome's section-header language.
        assert!(output.contains(" FILES"));
        assert!(output.contains("helix-term"));
        assert!(output.contains("Cargo.toml"));
        assert!(!output.contains("╭─explorer"));
        assert!(!output.contains("Storybook coverage"));
    }

    #[test]
    fn spinner_shadow_story_keeps_text_visible() {
        let output = dump_story("widgets/spinner-shadow", 120, 36).unwrap();
        assert!(output.contains("redraw owned by component"));
    }

    #[test]
    fn storybook_tick_is_wall_clock_based() {
        assert_eq!(storybook_tick(Duration::from_millis(0)), 0);
        assert_eq!(
            storybook_tick(Duration::from_millis(STORYBOOK_TICK_MS - 1)),
            0
        );
        assert_eq!(storybook_tick(Duration::from_millis(STORYBOOK_TICK_MS)), 1);
        assert_eq!(
            storybook_tick(Duration::from_millis(STORYBOOK_TICK_MS * 2 + 1)),
            2
        );
    }

    #[test]
    fn catalog_covers_reusable_widgets_and_ui_patterns() {
        let required = [
            "foundations/tokens",
            "foundations/layout",
            "foundations/widget-style",
            "widgets/chrome",
            "widgets/text-input",
            "widgets/text-input/focused",
            "widgets/text-input/long-path",
            "widgets/text-input/empty",
            "widgets/item-list",
            "widgets/scrolling",
            "widgets/message",
            "widgets/message-list",
            "widgets/anchored-text",
            "widgets/spinner-shadow",
            "ui/editor-shell",
            "ui/key-help",
            "ui/autoinfo-scroll",
            "ui/picker",
            "ui/picker/empty",
            "ui/picker/long-paths",
            "ui/prompt-cmdline",
            "ui/completion-menu",
            "ui/popups-overlays",
            "ui/file-explorer",
            "ui/file-explorer/empty",
            "ui/file-explorer/scrolled",
            "ui/assistant-panel",
            "ui/lsp",
            "ui/status-notifications",
            "components/inputs",
            "components/messages",
            "plugins/render-ops",
        ];

        for id in required {
            assert!(
                STORIES.iter().any(|story| story.id == id),
                "missing required story: {id}"
            );
        }
    }

    #[test]
    fn key_help_story_uses_default_space_infobox() {
        let output = dump_story("ui/key-help", 120, 52).unwrap();
        assert!(output.contains("Space"));
        assert!(output.contains("Open file picker"));
        assert!(output.contains("Open command palette"));
    }

    #[test]
    fn storybook_helix_roles_match_runtime_design_contract() {
        let theme = theme_loader().load("default").unwrap();
        let storybook = UiStyleGuide::from_theme(&theme);
        let design = HelixUiDesign::from_theme(&theme);

        assert_eq!(storybook.bufferline, design.bufferline.background);
        assert_eq!(storybook.bufferline_active, design.bufferline.active);
        assert_eq!(storybook.bufferline_inactive, design.bufferline.inactive);
        assert_eq!(storybook.statusline, design.statusline.base);
        assert_eq!(storybook.statusline_normal, design.statusline.normal);
        assert_eq!(storybook.statusline_insert, design.statusline.insert);
        assert_eq!(storybook.statusline_separator, design.statusline.separator);
        assert_eq!(storybook.popup, design.popup.background);
        assert_eq!(storybook.popup_border, design.popup.border);
        assert_eq!(storybook.menu, design.menu.background);
        assert_eq!(storybook.menu_selected, design.menu.selected);
        assert_eq!(storybook.gutter, design.gutter.base);
        assert_eq!(storybook.linenr, design.gutter.line_number);
        assert_eq!(
            storybook.linenr_selected,
            design.gutter.selected_line_number
        );
        assert_eq!(storybook.cursorline, design.cursorline.primary);
        assert_eq!(storybook.popup_info, design.info_popup.popup);
        assert_eq!(storybook.text_info, design.info_popup.text);
        assert_eq!(storybook.menu_scroll, design.info_popup.scrollbar);
        assert_eq!(storybook.menu_scroll, design.menu.scroll);
    }

    #[test]
    fn animated_scroll_offsets_include_the_terminal_offset() {
        assert_eq!(cycling_scroll_offset(40, 12, 28, 1), 28);
        assert_eq!(cycling_scroll_offset(120, 18, 34, 3), 102);
    }

    #[test]
    fn autoinfo_scroll_story_uses_runtime_renderer_bottom_state() {
        let output = dump_story("ui/autoinfo-scroll", 120, 36).unwrap();
        assert!(output.contains("Space"));
        assert!(output.contains("Open command palette"));
        assert!(output.contains("Show blame for the current line"));
    }

    #[test]
    fn unknown_story_is_rejected() {
        let err = dump_story("missing", 80, 24).unwrap_err();
        assert!(err.to_string().contains("unknown story"));
    }
}
