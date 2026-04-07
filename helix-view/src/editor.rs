use crate::{
    assistant::BackendDriver,
    annotations::diagnostics::{DiagnosticFilter, InlineDiagnosticsConfig},
    bench::log_run_event,
    clipboard::ClipboardProvider,
    document::{
        DocumentOpenError, DocumentSavedEventFuture, Mode, SavePoint,
        SCRATCH_BUFFER_NAME,
    },
    events::{DocumentDidClose, DocumentDidOpen, DocumentFocusLost},
    graphics::{CursorKind, Rect},
    handlers::Handlers,
    info::Info,
    input::{KeyCode, KeyEvent, KeyModifiers},
    register::Registers,
    traits::HistoryViewport,
    theme::{self, Theme},
    tree::{self, Dimension, Resize, Tree},
    view::{ensure_cursor_in_view_center_in, AnyViewMut, AnyViewRef, ComponentViewState},
    Document, DocumentId, View, ViewId,
};
use anyhow::Context;
use helix_event::dispatch;
use helix_vcs::DiffProviderRegistry;

use futures_util::future;
use futures_util::stream::SelectAll;
use helix_lsp::{Call, LanguageServerId};
use helix_runtime::{Receiver as RuntimeReceiver, Runtime, Sender as RuntimeSender};

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{self, stdin},
    num::{NonZeroU8, NonZeroUsize},
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::time::{Duration, Instant};

use anyhow::{bail, Error};

pub use helix_core::diagnostic::Severity;
use helix_core::{
    auto_pairs::AutoPairs,
    diagnostic::DiagnosticProvider,
    syntax::{
        self,
        config::{AutoPairConfig, IndentationHeuristic, LanguageServerFeature, SoftWrap},
    },
    Change, LineEnding, Position, Range, Selection, Uri, NATIVE_LINE_ENDING,
};
use helix_dap::{self as dap};
use helix_lsp::lsp;
use helix_stdx::path::canonicalize;

use serde::{ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};

use arc_swap::{
    access::{DynAccess, DynGuard},
    ArcSwap,
};

pub const DEFAULT_AUTO_SAVE_DELAY: u64 = 3000;

fn deserialize_duration_millis<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let millis = u64::deserialize(deserializer)?;
    Ok(Duration::from_millis(millis))
}

fn serialize_duration_millis<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_u64(
        duration
            .as_millis()
            .try_into()
            .map_err(|_| serde::ser::Error::custom("duration value overflowed u64"))?,
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct GutterConfig {
    /// Gutter Layout
    pub layout: Vec<GutterType>,
    /// Options specific to the "line-numbers" gutter
    pub line_numbers: GutterLineNumbersConfig,
}

impl Default for GutterConfig {
    fn default() -> Self {
        Self {
            layout: vec![
                GutterType::Diagnostics,
                GutterType::Spacer,
                GutterType::LineNumbers,
                GutterType::Spacer,
                GutterType::Diff,
            ],
            line_numbers: GutterLineNumbersConfig::default(),
        }
    }
}

impl From<Vec<GutterType>> for GutterConfig {
    fn from(x: Vec<GutterType>) -> Self {
        GutterConfig {
            layout: x,
            ..Default::default()
        }
    }
}

fn deserialize_gutter_seq_or_struct<'de, D>(deserializer: D) -> Result<GutterConfig, D::Error>
where
    D: Deserializer<'de>,
{
    struct GutterVisitor;

    impl<'de> serde::de::Visitor<'de> for GutterVisitor {
        type Value = GutterConfig;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(
                formatter,
                "an array of gutter names or a detailed gutter configuration"
            )
        }

        fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            let mut gutters = Vec::new();
            while let Some(gutter) = seq.next_element::<String>()? {
                gutters.push(
                    gutter
                        .parse::<GutterType>()
                        .map_err(serde::de::Error::custom)?,
                )
            }

            Ok(gutters.into())
        }

        fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let deserializer = serde::de::value::MapAccessDeserializer::new(map);
            Deserialize::deserialize(deserializer)
        }
    }

    deserializer.deserialize_any(GutterVisitor)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct GutterLineNumbersConfig {
    /// Minimum number of characters to use for line number gutter. Defaults to 3.
    pub min_width: usize,
}

impl Default for GutterLineNumbersConfig {
    fn default() -> Self {
        Self { min_width: 3 }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InlineBlameShow {
    /// Do not show inline blame, and do not request it in the background
    ///
    /// When manually requesting the inline blame, it may take several seconds to appear.
    Never,
    /// Show the inline blame on the cursor line
    CursorLine,
    /// Show the inline blame on every other line
    AllLines,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct InlineBlameConfig {
    /// How to show the inline blame
    pub show: InlineBlameShow,
    /// Whether the inline blame should be fetched in the background
    pub auto_fetch: bool,
    /// How the inline blame should look like and the information it includes
    pub format: String,
}

impl Default for InlineBlameConfig {
    fn default() -> Self {
        Self {
            show: InlineBlameShow::Never,
            format: "{author}, {time-ago} • {title} • {commit}".to_owned(),
            auto_fetch: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct FilePickerConfig {
    /// IgnoreOptions
    /// Enables ignoring hidden files.
    /// Whether to hide hidden files in file picker and global search results. Defaults to true.
    pub hidden: bool,
    /// Enables following symlinks.
    /// Whether to follow symbolic links in file picker and file or directory completions. Defaults to true.
    pub follow_symlinks: bool,
    /// Hides symlinks that point into the current directory. Defaults to true.
    pub deduplicate_links: bool,
    /// Enables reading ignore files from parent directories. Defaults to true.
    pub parents: bool,
    /// Enables reading `.ignore` files.
    /// Whether to hide files listed in .ignore in file picker and global search results. Defaults to true.
    pub ignore: bool,
    /// Enables reading `.gitignore` files.
    /// Whether to hide files listed in .gitignore in file picker and global search results. Defaults to true.
    pub git_ignore: bool,
    /// Enables reading global .gitignore, whose path is specified in git's config: `core.excludefile` option.
    /// Whether to hide files listed in global .gitignore in file picker and global search results. Defaults to true.
    pub git_global: bool,
    /// Enables reading `.git/info/exclude` files.
    /// Whether to hide files listed in .git/info/exclude in file picker and global search results. Defaults to true.
    pub git_exclude: bool,
    /// WalkBuilder options
    /// Maximum Depth to recurse directories in file picker and global search. Defaults to `None`.
    pub max_depth: Option<usize>,
    /// Whether to hide the preview panel. Defaults to false.
    pub hide_preview: bool,
}

impl Default for FilePickerConfig {
    fn default() -> Self {
        Self {
            hidden: true,
            follow_symlinks: true,
            deduplicate_links: true,
            parents: true,
            ignore: true,
            git_ignore: true,
            git_global: true,
            git_exclude: true,
            max_depth: None,
            hide_preview: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct FileExplorerConfig {
    /// IgnoreOptions
    /// Enables ignoring hidden files.
    /// Whether to hide hidden files in file explorer and global search results. Defaults to false.
    pub hidden: bool,
    /// Enables following symlinks.
    /// Whether to follow symbolic links in file picker and file or directory completions. Defaults to false.
    pub follow_symlinks: bool,
    /// Enables reading ignore files from parent directories. Defaults to false.
    pub parents: bool,
    /// Enables reading `.ignore` files.
    /// Whether to hide files listed in .ignore in file picker and global search results. Defaults to false.
    pub ignore: bool,
    /// Enables reading `.gitignore` files.
    /// Whether to hide files listed in .gitignore in file picker and global search results. Defaults to false.
    pub git_ignore: bool,
    /// Enables reading global .gitignore, whose path is specified in git's config: `core.excludefile` option.
    /// Whether to hide files listed in global .gitignore in file picker and global search results. Defaults to false.
    pub git_global: bool,
    /// Enables reading `.git/info/exclude` files.
    /// Whether to hide files listed in .git/info/exclude in file picker and global search results. Defaults to false.
    pub git_exclude: bool,
    /// Whether to flatten single-child directories in file explorer. Defaults to true.
    pub flatten_dirs: bool,
}

impl Default for FileExplorerConfig {
    fn default() -> Self {
        Self {
            hidden: false,
            follow_symlinks: false,
            parents: false,
            ignore: false,
            git_ignore: false,
            git_global: false,
            git_exclude: false,
            flatten_dirs: true,
        }
    }
}

fn serialize_alphabet<S>(alphabet: &[char], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let alphabet: String = alphabet.iter().collect();
    serializer.serialize_str(&alphabet)
}

fn deserialize_alphabet<'de, D>(deserializer: D) -> Result<Vec<char>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let str = String::deserialize(deserializer)?;
    let chars: Vec<_> = str.chars().collect();
    let unique_chars: HashSet<_> = chars.iter().copied().collect();
    if unique_chars.len() != chars.len() {
        return Err(<D::Error as Error>::custom(
            "jump-label-alphabet must contain unique characters",
        ));
    }
    Ok(chars)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Config {
    /// Whether to enable the welcome screen
    pub welcome_screen: bool,
    /// Padding to keep between the edge of the screen and the cursor when scrolling. Defaults to 5.
    pub scrolloff: usize,
    /// Number of lines to scroll at once. Defaults to 3
    pub scroll_lines: isize,
    /// Mouse support. Defaults to true.
    pub mouse: bool,
    /// Shell to use for shell commands. Defaults to ["cmd", "/C"] on Windows and ["sh", "-c"] otherwise.
    pub shell: Vec<String>,
    /// Line number mode.
    pub line_number: LineNumber,
    /// Symbol to use for the picker. Defaults to ">".
    pub picker_symbol: String,
    /// Highlight the lines cursors are currently on. Defaults to false.
    pub cursorline: bool,
    /// Highlight the columns cursors are currently on. Defaults to false.
    pub cursorcolumn: bool,
    #[serde(deserialize_with = "deserialize_gutter_seq_or_struct")]
    pub gutters: GutterConfig,
    /// Middle click paste support. Defaults to true.
    pub middle_click_paste: bool,
    /// Automatic insertion of pairs to parentheses, brackets,
    /// etc. Optionally, this can be a list of 2-tuples to specify a
    /// global list of characters to pair. Defaults to true.
    pub auto_pairs: AutoPairConfig,
    /// Automatic auto-completion, automatically pop up without user trigger. Defaults to true.
    pub auto_completion: bool,
    /// Enable filepath completion.
    /// Show files and directories if an existing path at the cursor was recognized,
    /// either absolute or relative to the current opened document or current working directory (if the buffer is not yet saved).
    /// Defaults to true.
    pub path_completion: bool,
    /// Configures completion of words from open buffers.
    /// Defaults to enabled with a trigger length of 7.
    pub word_completion: WordCompletion,
    /// Automatic formatting on save. Defaults to true.
    pub auto_format: bool,
    /// Default register used for yank/paste. Defaults to '"'
    pub default_yank_register: char,
    /// Automatic save on focus lost and/or after delay.
    /// Time delay in milliseconds since last edit after which auto save timer triggers.
    /// Time delay defaults to false with 3000ms delay. Focus lost defaults to false.
    #[serde(deserialize_with = "deserialize_auto_save")]
    pub auto_save: AutoSave,
    /// Automatically reload buffers when files change on disk (via OS file watcher).
    /// Only reloads unmodified buffers. Defaults to true.
    #[serde(default = "default_true")]
    pub auto_reload: bool,
    /// Set a global text_width
    pub text_width: usize,
    /// Time in milliseconds since last keypress before idle timers trigger.
    /// Used for various UI timeouts. Defaults to 250ms.
    #[serde(
        serialize_with = "serialize_duration_millis",
        deserialize_with = "deserialize_duration_millis"
    )]
    pub idle_timeout: Duration,
    /// Time in milliseconds after typing a word character before auto completions
    /// are shown, set to 5 for instant. Defaults to 250ms.
    #[serde(
        serialize_with = "serialize_duration_millis",
        deserialize_with = "deserialize_duration_millis"
    )]
    pub completion_timeout: Duration,
    /// Whether to insert the completion suggestion on hover. Defaults to true.
    pub preview_completion_insert: bool,
    pub completion_trigger_len: u8,
    /// Whether to instruct the LSP to replace the entire word when applying a completion
    /// or to only insert new text
    pub completion_replace: bool,
    /// `true` if helix should automatically add a line comment token if you're currently in a comment
    /// and press `enter`.
    pub continue_comments: bool,
    /// Whether to display infoboxes. Defaults to true.
    pub auto_info: bool,
    pub file_picker: FilePickerConfig,
    /// Configuration of the bufferline
    pub bufferline: BufferLineConfig,
    /// Configuration of the file explorer
    pub file_explorer: FileExplorerConfig,
    /// Configuration of the statusline elements
    pub statusline: StatusLineConfig,
    /// Shape for cursor in each mode
    pub cursor_shape: CursorShapeConfig,
    /// Set to `true` to override automatic detection of terminal truecolor support in the event of a false negative. Defaults to `false`.
    pub true_color: bool,
    /// Set to `true` to override automatic detection of terminal undercurl support in the event of a false negative. Defaults to `false`.
    pub undercurl: bool,
    /// Search configuration.
    #[serde(default)]
    pub search: SearchConfig,
    pub lsp: LspConfig,
    pub terminal: Option<TerminalConfig>,
    /// Column numbers at which to draw the rulers. Defaults to `[]`, meaning no rulers.
    pub rulers: Vec<u16>,
    /// Character used to render rulers in the foreground. Defaults to "" (background-style rulers).
    /// Set to empty string "" to use background-style rulers instead of a glyph.
    pub ruler_char: String,
    #[serde(default)]
    pub whitespace: WhitespaceConfig,
    /// Vertical indent width guides.
    pub indent_guides: IndentGuidesConfig,
    /// Whether to color modes with different colors. Defaults to `false`.
    pub color_modes: bool,
    pub soft_wrap: SoftWrap,
    /// Workspace specific lsp ceiling dirs
    pub workspace_lsp_roots: Vec<PathBuf>,
    /// Which line ending to choose for new documents. Defaults to `native`. i.e. `crlf` on Windows, otherwise `lf`.
    pub default_line_ending: LineEndingConfig,
    /// Whether to automatically insert a trailing line-ending on write if missing. Defaults to `true`.
    pub insert_final_newline: bool,
    /// Whether to use atomic operations to write documents to disk.
    /// This prevents data loss if the editor is interrupted while writing the file, but may
    /// confuse some file watching/hot reloading programs. Defaults to `true`.
    pub atomic_save: bool,
    /// Whether to automatically remove all trailing line-endings after the final one on write.
    /// Defaults to `false`.
    pub trim_final_newlines: bool,
    /// Whether to automatically remove all whitespace characters preceding line-endings on write.
    /// Defaults to `false`.
    pub trim_trailing_whitespace: bool,
    /// Enables smart tab
    pub smart_tab: Option<SmartTabConfig>,
    /// Draw border around popups.
    pub popup_border: PopupBorderConfig,
    /// Draw rounded border corners
    pub rounded_corners: bool,
    /// Which indent heuristic to use when a new line is inserted
    #[serde(default)]
    pub indent_heuristic: IndentationHeuristic,
    /// labels characters used in jumpmode
    #[serde(
        serialize_with = "serialize_alphabet",
        deserialize_with = "deserialize_alphabet"
    )]
    pub jump_label_alphabet: Vec<char>,
    /// Display diagnostic below the line they occur.
    pub inline_diagnostics: InlineDiagnosticsConfig,
    pub end_of_line_diagnostics: DiagnosticFilter,
    // Set to override the default clipboard provider
    pub clipboard_provider: ClipboardProvider,
    /// Whether to read settings from [EditorConfig](https://editorconfig.org) files. Defaults to
    /// `true`.
    pub editor_config: bool,
    /// Maximum width for panel resizing. Set to 0 for dynamic limit based on terminal width. Defaults to 50.
    pub max_panel_width: usize,
    /// Maximum height for panel resizing. Set to 0 for dynamic limit based on terminal height. Defaults to 50.
    pub max_panel_height: usize,
    /// Maximum panel width as percentage of terminal width (0.0-1.0). Used when max_panel_width is 0. Defaults to 0.8.
    pub max_panel_width_percent: f32,
    /// Maximum panel height as percentage of terminal height (0.0-1.0). Used when max_panel_height is 0. Defaults to 0.8.
    pub max_panel_height_percent: f32,
    /// Inline blame allows showing the latest commit that affected the line the cursor is on as virtual text
    #[serde(default)]
    pub inline_blame: InlineBlameConfig,
    /// Whether to render rainbow colors for matching brackets. Defaults to `false`.
    pub rainbow_brackets: bool,
    /// Whether to enable Kitty Keyboard Protocol
    pub kitty_keyboard_protocol: KittyKeyboardProtocolConfig,
    /// Command line configuration
    #[serde(default)]
    pub cmdline: CmdlineConfig,
    /// Picker gradient border configuration
    #[serde(default)]
    pub gradient_borders: GradientBorderConfig,
    /// Notification system configuration
    #[serde(default)]
    pub notifications: NotificationConfig,
    /// Completion Highlight configuration
    #[serde(default)]
    pub completion_highlight: CompletionHighlight,
    pub buffer_picker: BufferPickerConfig,
    /// Defines which text objects will be folded when a document is opened.
    #[serde(default)]
    pub fold_textobjects: Vec<String>,
    /// Preconfigured assistant agents.
    #[serde(default = "default_agents")]
    pub agents: Vec<AgentConfig>,
    /// Assistant panel configuration (keybindings for cycle agent, thinking, model).
    #[serde(default)]
    pub acp: AcpConfig,
    /// Which editing engine to use. Defaults to `"helix"`.
    /// Set to `"vim"` for Vim-style operator-pending composition.
    #[serde(default)]
    pub editing_engine: EditingEngineConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct AcpConfig {
    /// Cycle through thinking options (if available). Default: C-t
    pub cycle_thinking: Option<String>,
    /// Cycle through model options. Default: C-m
    pub cycle_model: Option<String>,
    /// Cycle through mode options (e.g. Default, Accept Edits, Plan Mode). Default: S-tab
    pub cycle_mode: Option<String>,
    /// Bubble border corner style: "rounded" (default) or "squared".
    pub bubble_corners: Option<String>,
}

impl AcpConfig {
    pub fn cycle_thinking(&self) -> KeyEvent {
        self.cycle_thinking
            .as_deref()
            .unwrap_or("C-t")
            .parse()
            .unwrap_or(KeyEvent {
                code: KeyCode::Char('t'),
                modifiers: KeyModifiers::CONTROL,
            })
    }
    pub fn cycle_model(&self) -> KeyEvent {
        self.cycle_model
            .as_deref()
            .unwrap_or("C-m")
            .parse()
            .unwrap_or(KeyEvent {
                code: KeyCode::Char('m'),
                modifiers: KeyModifiers::CONTROL,
            })
    }
    pub fn cycle_mode(&self) -> KeyEvent {
        self.cycle_mode
            .as_deref()
            .unwrap_or("S-tab")
            .parse()
            .unwrap_or(KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::SHIFT,
            })
    }
    /// Whether bubble borders use rounded or squared corners.
    pub fn bubble_corners_rounded(&self) -> bool {
        match self.bubble_corners.as_deref() {
            Some("squared") => false,
            _ => true, // default: rounded
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AgentConfig {
    /// Display name for the agent.
    pub name: String,
    /// Command to spawn the agent process.
    pub command: String,
    /// Arguments to pass to the agent command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional theme to apply when this agent is connected (e.g. "claude", "gemini").
    #[serde(default)]
    pub theme: Option<String>,
}

fn default_agents() -> Vec<AgentConfig> {
    let claude_cmd = if cfg!(windows) {
        (
            "npm.cmd".into(),
            vec![
                "exec".into(),
                "--yes".into(),
                "@zed-industries/claude-agent-acp@0.20.2".into(),
            ],
        )
    } else {
        ("claude-agent-acp".into(), vec![])
    };
    vec![
        AgentConfig {
            name: "Claude Agent".into(),
            command: claude_cmd.0,
            args: claude_cmd.1,
            theme: None,
        },
        AgentConfig {
            name: "Cursor".into(),
            command: "cursor".into(),
            args: vec!["agent".into(), "acp".into()],
            theme: None,
        },
        AgentConfig {
            name: "Gemini CLI".into(),
            command: "gemini".into(),
            args: vec!["--experimental-acp".into()],
            theme: None,
        },
        AgentConfig {
            name: "Goose".into(),
            command: "goose".into(),
            args: vec!["acp".into()],
            theme: None,
        },
    ]
}

#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub struct BufferPickerConfig {
    pub start_position: PickerStartPosition,
}

#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum PickerStartPosition {
    #[default]
    Current,
    Previous,
}

impl PickerStartPosition {
    #[must_use]
    pub fn is_previous(self) -> bool {
        matches!(self, Self::Previous)
    }

    #[must_use]
    pub fn is_current(self) -> bool {
        matches!(self, Self::Current)
    }
}

#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum KittyKeyboardProtocolConfig {
    #[default]
    Auto,
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct CmdlineConfig {
    /// Command line display style. Options: "bottom", "popup". Defaults to "bottom".
    pub style: CmdlineStyle,
    /// Enable command icons in cmdline. Defaults to true.
    pub show_icons: bool,
    /// Minimum width for popup cmdline. Defaults to 40.
    pub min_popup_width: u16,
    /// Maximum width for popup cmdline. Defaults to 80.
    pub max_popup_width: u16,
    /// Use full height when cmdline style is popup (removes the bottom space). Defaults to false.
    pub use_full_height: bool,
    /// Customizable icons for different command types.
    pub icons: CmdlineIcons,
}

impl Default for CmdlineConfig {
    fn default() -> Self {
        Self {
            style: CmdlineStyle::Bottom,
            show_icons: true,
            min_popup_width: 40,
            max_popup_width: 80,
            use_full_height: false,
            icons: CmdlineIcons::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct CmdlineIcons {
    /// Icon for search commands (/,?). Defaults to "🔍".
    pub search: String,
    /// Icon for command mode (:). Defaults to "⚙".
    pub command: String,
    /// Icon for shell commands (!). Defaults to "⚡".
    pub shell: String,
    /// Icon for general prompts. Defaults to "💬".
    pub general: String,
}

impl Default for CmdlineIcons {
    fn default() -> Self {
        Self {
            search: "🔍".to_string(),
            command: "🛠️".to_string(),
            shell: "⚡".to_string(),
            general: "💬".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CmdlineStyle {
    /// Traditional bottom command line
    #[default]
    Bottom,
    /// Centered popup window (noice.nvim style)
    Popup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct GradientBorderConfig {
    /// Enable gradient borders for pickers. Defaults to false.
    pub enable: bool,
    /// Border thickness (1-5). Defaults to 1.
    pub thickness: u8,
    /// Gradient direction. Options: "horizontal", "vertical", "diagonal". Defaults to "horizontal".
    pub direction: GradientDirection,
    /// Start color (in hex format like "#FF0000"). Defaults to "#8A2BE2".
    pub start_color: String,
    /// End color (in hex format like "#FF0000"). Defaults to "#00BFFF".
    pub end_color: String,
    /// Middle color for 3-color gradients (optional). Defaults to "".
    pub middle_color: String,
    /// Animation speed (0-10, 0 = disabled). Defaults to 0.
    pub animation_speed: u8,
}

impl Default for GradientBorderConfig {
    fn default() -> Self {
        Self {
            enable: false,
            thickness: 1,
            direction: GradientDirection::Horizontal,
            start_color: "#8A2BE2".to_string(), // BlueViolet
            end_color: "#00BFFF".to_string(),   // DeepSkyBlue
            middle_color: "".to_string(),
            animation_speed: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GradientDirection {
    /// Left to right gradient
    #[default]
    Horizontal,
    /// Top to bottom gradient
    Vertical,
    /// Diagonal gradient (top-left to bottom-right)
    Diagonal,
    /// Radial gradient from center
    Radial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct NotificationConfig {
    /// Enable notification system. Defaults to true.
    pub enable: bool,
    /// Notification display style. Options: "popup", "statusline". Defaults to "popup".
    pub style: NotificationStyle,
    /// Maximum number of notifications to keep in history. Defaults to 100.
    pub max_history: usize,
    /// Default timeout for notifications in milliseconds. 0 = no timeout. Defaults to 5000.
    #[serde(
        serialize_with = "serialize_duration_millis",
        deserialize_with = "deserialize_duration_millis"
    )]
    pub default_timeout: Duration,
    /// Position for popup notifications. Defaults to "top-right".
    pub position: NotificationPosition,
    /// Maximum width for notification popups. Defaults to 60.
    pub max_width: u16,
    /// Maximum height for notification popups. Defaults to 10.
    pub max_height: u16,
    /// Show notification icons. Defaults to true.
    pub show_icons: bool,
    /// Notification icons for different severity levels.
    pub icons: NotificationIcons,
    /// Padding inside the notification content area
    pub padding: u16,
    /// Show notification emojis. Defaults to true.
    pub show_emojis: bool,
    /// Notification emojis for different severity levels.
    pub emojis: NotificationEmojis,
    /// Enable notification history command. Defaults to true.
    pub enable_history: bool,
    /// Border configuration for notifications.
    pub border: NotificationBorderConfig,
    /// Drop shadow configuration for notifications
    pub shadow: NotificationShadowConfig,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enable: true,
            style: NotificationStyle::Popup,
            max_history: 100,
            default_timeout: Duration::from_millis(5000),
            position: NotificationPosition::TopRight,
            max_width: 60,
            max_height: 10,
            show_icons: true,
            icons: NotificationIcons::default(),
            padding: 1,
            show_emojis: true,
            emojis: NotificationEmojis::default(),
            enable_history: true,
            border: NotificationBorderConfig::default(),
            shadow: NotificationShadowConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationStyle {
    /// Show notifications as popup windows (noice.nvim style)
    #[default]
    Popup,
    /// Show notifications in statusline (traditional style)
    Statusline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationPosition {
    /// Top-left corner
    TopLeft,
    /// Top-center
    TopCenter,
    /// Top-right corner
    #[default]
    TopRight,
    /// Bottom-left corner
    BottomLeft,
    /// Bottom-center
    BottomCenter,
    /// Bottom-right corner
    BottomRight,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct NotificationIcons {
    /// Icon for info notifications. Defaults to "ℹ".
    pub info: String,
    /// Icon for warning notifications. Defaults to "⚠".
    pub warning: String,
    /// Icon for error notifications. Defaults to "✗".
    pub error: String,
    /// Icon for success notifications. Defaults to "✓".
    pub success: String,
}

impl Default for NotificationIcons {
    fn default() -> Self {
        Self {
            info: "ℹ".to_string(),
            warning: "⚠".to_string(),
            error: "✗".to_string(),
            success: "✓".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct NotificationEmojis {
    /// Emoji for info notifications. Defaults to "💡".
    pub info: String,
    /// Emoji for warning notifications. Defaults to "⚠️".
    pub warning: String,
    /// Emoji for error notifications. Defaults to "❌".
    pub error: String,
    /// Emoji for success notifications. Defaults to "✅".
    pub success: String,
}

impl Default for NotificationEmojis {
    fn default() -> Self {
        Self {
            info: "💡".to_string(),
            warning: "⚠️".to_string(),
            error: "❌".to_string(),
            success: "✅".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct NotificationBorderConfig {
    /// Enable borders for notifications. Defaults to true.
    pub enable: bool,
    /// Border width (1-5). Defaults to 1.
    pub width: u8,
    /// Border radius for rounded corners (0-10). Defaults to 2.
    pub radius: u8,
    /// Use gradient borders. Defaults to false.
    pub gradient: bool,
    /// Gradient start color (in hex format like "#FF0000"). Defaults to "#8A2BE2".
    pub gradient_start: String,
    /// Gradient end color (in hex format like "#FF0000"). Defaults to "#00BFFF".
    pub gradient_end: String,
    /// Border style. Options: "solid", "dashed", "dotted". Defaults to "solid".
    pub style: NotificationBorderStyle,
}

impl Default for NotificationBorderConfig {
    fn default() -> Self {
        Self {
            enable: true,
            width: 1,
            radius: 2,
            gradient: false,
            gradient_start: "#8A2BE2".to_string(), // BlueViolet
            gradient_end: "#00BFFF".to_string(),   // DeepSkyBlue
            style: NotificationBorderStyle::Solid,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationBorderStyle {
    /// Solid border
    #[default]
    Solid,
    /// Dashed border
    Dashed,
    /// Dotted border
    Dotted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct NotificationShadowConfig {
    /// Enable drop shadow
    pub enable: bool,
    /// Shadow offset on x and y axes
    pub offset_x: u16,
    pub offset_y: u16,
}

impl Default for NotificationShadowConfig {
    fn default() -> Self {
        Self {
            enable: true,
            offset_x: 1,
            offset_y: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Eq, PartialOrd, Ord)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct SmartTabConfig {
    pub enable: bool,
    pub supersede_menu: bool,
}

impl Default for SmartTabConfig {
    fn default() -> Self {
        SmartTabConfig {
            enable: true,
            supersede_menu: false,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct TerminalConfig {
    pub command: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

#[cfg(windows)]
pub fn get_terminal_provider() -> Option<TerminalConfig> {
    use helix_stdx::env::binary_exists;

    if binary_exists("wt") {
        return Some(TerminalConfig {
            command: "wt".to_string(),
            args: vec![
                "new-tab".to_string(),
                "--title".to_string(),
                "DEBUG".to_string(),
                "cmd".to_string(),
                "/C".to_string(),
            ],
        });
    }

    Some(TerminalConfig {
        command: "conhost".to_string(),
        args: vec!["cmd".to_string(), "/C".to_string()],
    })
}

#[cfg(not(any(windows, target_arch = "wasm32")))]
pub fn get_terminal_provider() -> Option<TerminalConfig> {
    use helix_stdx::env::{binary_exists, env_var_is_set};

    if env_var_is_set("TMUX") && binary_exists("tmux") {
        return Some(TerminalConfig {
            command: "tmux".to_string(),
            args: vec!["split-window".to_string()],
        });
    }

    if env_var_is_set("WEZTERM_UNIX_SOCKET") && binary_exists("wezterm") {
        return Some(TerminalConfig {
            command: "wezterm".to_string(),
            args: vec!["cli".to_string(), "split-pane".to_string()],
        });
    }

    None
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct LspConfig {
    /// Enables LSP
    pub enable: bool,
    /// Display LSP messagess from $/progress below statusline
    pub display_progress_messages: bool,
    /// Display LSP messages from window/showMessage below statusline
    pub display_messages: bool,
    /// Enable automatic pop up of signature help (parameter hints)
    pub auto_signature_help: bool,
    /// Display docs under signature help popup
    pub display_signature_help_docs: bool,
    /// Position of signature help popup relative to cursor: "above" or "below"
    pub signature_help_position: SignatureHelpPosition,
    /// Display inlay hints
    pub display_inlay_hints: bool,
    /// Maximum displayed length of inlay hints (excluding the added trailing `…`).
    /// If it's `None`, there's no limit
    pub inlay_hints_length_limit: Option<NonZeroU8>,
    /// Display document color swatches
    pub display_color_swatches: bool,
    /// Color swatches string. Defaults to `"■"`.
    pub color_swatches_string: String,
    /// Whether to enable snippet support
    pub snippets: bool,
    /// Whether to include declaration in the goto reference query
    pub goto_reference_include_declaration: bool,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enable: true,
            display_progress_messages: false,
            display_messages: true,
            auto_signature_help: true,
            display_signature_help_docs: true,
            signature_help_position: SignatureHelpPosition::Above,
            display_inlay_hints: false,
            inlay_hints_length_limit: None,
            snippets: true,
            goto_reference_include_declaration: true,
            display_color_swatches: true,
            color_swatches_string: "■".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureHelpPosition {
    Above,
    Below,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct SearchConfig {
    /// Smart case: Case insensitive searching unless pattern contains upper case characters. Defaults to true.
    pub smart_case: bool,
    /// Whether the search should wrap after depleting the matches. Default to true.
    pub wrap_around: bool,
}

/// bufferline render modes
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BufferLineRenderMode {
    /// Don't render bufferline
    #[default]
    Never,
    /// Always render
    Always,
    /// Only if multiple buffers are open
    Multiple,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct BufferLineConfig {
    pub render_mode: BufferLineRenderMode,
    pub separator: String,
}

impl Default for BufferLineConfig {
    fn default() -> Self {
        Self {
            render_mode: BufferLineRenderMode::default(),
            separator: String::from("│"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct StatusLineConfig {
    pub left: Vec<StatusLineElement>,
    pub center: Vec<StatusLineElement>,
    pub right: Vec<StatusLineElement>,
    pub separator: String,
    pub mode: ModeConfig,
    pub diagnostics: Vec<Severity>,
    pub workspace_diagnostics: Vec<Severity>,
}

impl Default for StatusLineConfig {
    fn default() -> Self {
        use StatusLineElement as E;

        Self {
            left: vec![
                E::Mode,
                E::Spinner,
                E::FileName,
                E::ReadOnlyIndicator,
                E::FileModificationIndicator,
            ],
            center: vec![],
            right: vec![
                E::Diagnostics,
                E::Selections,
                E::Register,
                E::Position,
                E::FileEncoding,
            ],
            separator: String::from("│"),
            mode: ModeConfig::default(),
            diagnostics: vec![Severity::Warning, Severity::Error],
            workspace_diagnostics: vec![Severity::Warning, Severity::Error],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct ModeConfig {
    pub normal: String,
    pub insert: String,
    pub select: String,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            normal: String::from("NOR"),
            insert: String::from("INS"),
            select: String::from("SEL"),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusLineElement {
    /// The editor mode (Normal, Insert, Visual/Selection)
    Mode,

    /// The LSP activity spinner
    Spinner,

    /// The file basename (the leaf of the open file's path)
    FileBaseName,

    /// The relative file path
    FileName,

    /// The file absolute path
    FileAbsolutePath,

    // The file modification indicator
    FileModificationIndicator,

    /// An indicator that shows `"[readonly]"` when a file cannot be written
    ReadOnlyIndicator,

    /// The file encoding
    FileEncoding,

    /// The file line endings (CRLF or LF)
    FileLineEnding,

    /// The file indentation style
    FileIndentStyle,

    /// The file type (language ID or "text")
    FileType,

    /// A summary of the number of errors and warnings
    Diagnostics,

    /// A summary of the number of errors and warnings on file and workspace
    WorkspaceDiagnostics,

    /// The number of selections (cursors)
    Selections,

    /// The number of characters currently in primary selection
    PrimarySelectionLength,

    /// The cursor position
    Position,

    /// The separator string
    Separator,

    /// The cursor position as a percent of the total file
    PositionPercentage,

    /// The total line numbers of the current file
    TotalLineNumbers,

    /// A single space
    Spacer,

    /// Current version control information
    VersionControl,

    /// Indicator for selected register
    Register,

    /// The base of current working directory
    CurrentWorkingDirectory,

    /// The current function name (from tree-sitter)
    FunctionName,
}

// Cursor shape is read and used on every rendered frame and so needs
// to be fast. Therefore we avoid a hashmap and use an enum indexed array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShapeConfig([CursorKind; 3]);

impl CursorShapeConfig {
    pub fn from_mode(&self, mode: Mode) -> CursorKind {
        self.get(mode as usize).copied().unwrap_or_default()
    }
}

impl<'de> Deserialize<'de> for CursorShapeConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let m = HashMap::<Mode, CursorKind>::deserialize(deserializer)?;
        let into_cursor = |mode: Mode| m.get(&mode).copied().unwrap_or_default();
        Ok(CursorShapeConfig([
            into_cursor(Mode::Normal),
            into_cursor(Mode::Select),
            into_cursor(Mode::Insert),
        ]))
    }
}

impl Serialize for CursorShapeConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.len()))?;
        let modes = [Mode::Normal, Mode::Select, Mode::Insert];
        for mode in modes {
            map.serialize_entry(&mode, &self.from_mode(mode))?;
        }
        map.end()
    }
}

impl std::ops::Deref for CursorShapeConfig {
    type Target = [CursorKind; 3];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Default for CursorShapeConfig {
    fn default() -> Self {
        Self([CursorKind::Block; 3])
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineNumber {
    /// Show absolute line number
    Absolute,

    /// If focused and in normal/select mode, show relative line number to the primary cursor.
    /// If unfocused or in insert mode, show absolute line number.
    Relative,
}

/// Which editing engine to use.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditingEngineConfig {
    /// Helix select→act paradigm (default).
    #[default]
    Helix,
    /// Vim verb→object paradigm with operator-pending mode.
    Vim,
}

impl std::str::FromStr for LineNumber {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "absolute" | "abs" => Ok(Self::Absolute),
            "relative" | "rel" => Ok(Self::Relative),
            _ => anyhow::bail!("Line number can only be `absolute` or `relative`."),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GutterType {
    /// Show diagnostics and other features like breakpoints
    Diagnostics,
    /// Show line numbers
    LineNumbers,
    /// Show one blank space
    Spacer,
    /// Highlight local changes
    Diff,
}

impl std::str::FromStr for GutterType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "diagnostics" => Ok(Self::Diagnostics),
            "spacer" => Ok(Self::Spacer),
            "line-numbers" => Ok(Self::LineNumbers),
            "diff" => Ok(Self::Diff),
            _ => anyhow::bail!(
                "Gutter type can only be `diagnostics`, `spacer`, `line-numbers` or `diff`."
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WhitespaceConfig {
    pub render: WhitespaceRender,
    pub characters: WhitespaceCharacters,
}

impl Default for WhitespaceConfig {
    fn default() -> Self {
        Self {
            render: WhitespaceRender::Basic(WhitespaceRenderValue::None),
            characters: WhitespaceCharacters::default(),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, rename_all = "kebab-case")]
pub enum WhitespaceRender {
    Basic(WhitespaceRenderValue),
    Specific {
        default: Option<WhitespaceRenderValue>,
        space: Option<WhitespaceRenderValue>,
        nbsp: Option<WhitespaceRenderValue>,
        nnbsp: Option<WhitespaceRenderValue>,
        tab: Option<WhitespaceRenderValue>,
        newline: Option<WhitespaceRenderValue>,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WhitespaceRenderValue {
    None,
    // TODO
    // Selection,
    All,
}

impl WhitespaceRender {
    pub fn space(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(val) => val,
            Self::Specific { default, space, .. } => {
                space.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }
    pub fn nbsp(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(val) => val,
            Self::Specific { default, nbsp, .. } => {
                nbsp.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }
    pub fn nnbsp(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(val) => val,
            Self::Specific { default, nnbsp, .. } => {
                nnbsp.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }
    pub fn tab(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(val) => val,
            Self::Specific { default, tab, .. } => {
                tab.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }
    pub fn newline(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(val) => val,
            Self::Specific {
                default, newline, ..
            } => newline.or(default).unwrap_or(WhitespaceRenderValue::None),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct AutoSave {
    /// Auto save after a delay in milliseconds. Defaults to disabled.
    #[serde(default)]
    pub after_delay: AutoSaveAfterDelay,
    /// Auto save on focus lost. Defaults to false.
    #[serde(default)]
    pub focus_lost: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutoSaveAfterDelay {
    #[serde(default)]
    /// Enable auto save after delay. Defaults to false.
    pub enable: bool,
    #[serde(default = "default_auto_save_delay")]
    /// Time delay in milliseconds. Defaults to [DEFAULT_AUTO_SAVE_DELAY].
    pub timeout: u64,
}

impl Default for AutoSaveAfterDelay {
    fn default() -> Self {
        Self {
            enable: false,
            timeout: DEFAULT_AUTO_SAVE_DELAY,
        }
    }
}

fn default_auto_save_delay() -> u64 {
    DEFAULT_AUTO_SAVE_DELAY
}

const fn default_true() -> bool {
    true
}

fn deserialize_auto_save<'de, D>(deserializer: D) -> Result<AutoSave, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize, Serialize)]
    #[serde(untagged, deny_unknown_fields, rename_all = "kebab-case")]
    enum AutoSaveToml {
        EnableFocusLost(bool),
        AutoSave(AutoSave),
    }

    match AutoSaveToml::deserialize(deserializer)? {
        AutoSaveToml::EnableFocusLost(focus_lost) => Ok(AutoSave {
            focus_lost,
            ..Default::default()
        }),
        AutoSaveToml::AutoSave(auto_save) => Ok(auto_save),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WhitespaceCharacters {
    pub space: char,
    pub nbsp: char,
    pub nnbsp: char,
    pub tab: char,
    pub tabpad: char,
    pub newline: char,
}

impl Default for WhitespaceCharacters {
    fn default() -> Self {
        Self {
            space: '·',   // U+00B7
            nbsp: '⍽',    // U+237D
            nnbsp: '␣',   // U+2423
            tab: '→',     // U+2192
            newline: '⏎', // U+23CE
            tabpad: ' ',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct IndentGuidesConfig {
    pub render: bool,
    pub character: char,
    pub skip_levels: u8,
}

impl Default for IndentGuidesConfig {
    fn default() -> Self {
        Self {
            skip_levels: 0,
            render: false,
            character: '│',
        }
    }
}

/// Line ending configuration.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineEndingConfig {
    /// The platform's native line ending.
    ///
    /// `crlf` on Windows, otherwise `lf`.
    #[default]
    Native,
    /// Line feed.
    LF,
    /// Carriage return followed by line feed.
    Crlf,
    /// Form feed.
    #[cfg(feature = "unicode-lines")]
    FF,
    /// Carriage return.
    #[cfg(feature = "unicode-lines")]
    CR,
    /// Next line.
    #[cfg(feature = "unicode-lines")]
    Nel,
}

impl From<LineEndingConfig> for LineEnding {
    fn from(line_ending: LineEndingConfig) -> Self {
        match line_ending {
            LineEndingConfig::Native => NATIVE_LINE_ENDING,
            LineEndingConfig::LF => LineEnding::LF,
            LineEndingConfig::Crlf => LineEnding::Crlf,
            #[cfg(feature = "unicode-lines")]
            LineEndingConfig::FF => LineEnding::FF,
            #[cfg(feature = "unicode-lines")]
            LineEndingConfig::CR => LineEnding::CR,
            #[cfg(feature = "unicode-lines")]
            LineEndingConfig::Nel => LineEnding::Nel,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PopupBorderConfig {
    None,
    All,
    Popup,
    Menu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct WordCompletion {
    pub enable: bool,
    pub trigger_length: NonZeroU8,
}

impl Default for WordCompletion {
    fn default() -> Self {
        Self {
            enable: true,
            trigger_length: NonZeroU8::new(7).unwrap(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompletionHighlightType {
    Default,
    ThemeColors,
    Vibrant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct CompletionHighlight {
    /// What kind of highlight type: "default", "theme-colors", "vibrant"
    pub highlight_type: CompletionHighlightType,
}

impl Default for CompletionHighlight {
    fn default() -> Self {
        Self {
            highlight_type: CompletionHighlightType::Default,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            welcome_screen: true,
            scrolloff: 5,
            scroll_lines: 3,
            mouse: true,
            shell: if cfg!(windows) {
                vec!["cmd".to_owned(), "/C".to_owned()]
            } else {
                vec!["sh".to_owned(), "-c".to_owned()]
            },
            line_number: LineNumber::Absolute,
            picker_symbol: ">".to_string(),
            cursorline: false,
            cursorcolumn: false,
            gutters: GutterConfig::default(),
            middle_click_paste: true,
            auto_pairs: AutoPairConfig::default(),
            auto_completion: true,
            path_completion: true,
            word_completion: WordCompletion::default(),
            auto_format: true,
            default_yank_register: '"',
            auto_save: AutoSave::default(),
            auto_reload: true,
            idle_timeout: Duration::from_millis(250),
            completion_timeout: Duration::from_millis(250),
            preview_completion_insert: true,
            completion_trigger_len: 2,
            auto_info: true,
            file_picker: FilePickerConfig::default(),
            bufferline: BufferLineConfig::default(),
            file_explorer: FileExplorerConfig::default(),
            statusline: StatusLineConfig::default(),
            cursor_shape: CursorShapeConfig::default(),
            true_color: false,
            undercurl: false,
            search: SearchConfig::default(),
            lsp: LspConfig::default(),
            terminal: get_terminal_provider(),
            rulers: Vec::new(),
            ruler_char: "".to_string(),
            whitespace: WhitespaceConfig::default(),
            indent_guides: IndentGuidesConfig::default(),
            color_modes: false,
            soft_wrap: SoftWrap {
                enable: Some(false),
                ..SoftWrap::default()
            },
            text_width: 80,
            completion_replace: false,
            continue_comments: true,
            workspace_lsp_roots: Vec::new(),
            default_line_ending: LineEndingConfig::default(),
            insert_final_newline: true,
            atomic_save: true,
            trim_final_newlines: false,
            trim_trailing_whitespace: false,
            smart_tab: Some(SmartTabConfig::default()),
            popup_border: PopupBorderConfig::None,
            rounded_corners: false,
            indent_heuristic: IndentationHeuristic::default(),
            jump_label_alphabet: ('a'..='z').collect(),
            inline_diagnostics: InlineDiagnosticsConfig::default(),
            end_of_line_diagnostics: DiagnosticFilter::Enable(Severity::Hint),
            clipboard_provider: ClipboardProvider::default(),
            inline_blame: InlineBlameConfig::default(),
            editor_config: true,
            max_panel_width: 50,
            max_panel_height: 50,
            max_panel_width_percent: 0.8,
            max_panel_height_percent: 0.8,
            rainbow_brackets: false,
            kitty_keyboard_protocol: Default::default(),
            cmdline: CmdlineConfig::default(),
            gradient_borders: GradientBorderConfig::default(),
            notifications: NotificationConfig::default(),
            completion_highlight: CompletionHighlight::default(),
            buffer_picker: BufferPickerConfig::default(),
            fold_textobjects: Vec::new(),
            agents: vec![
                AgentConfig {
                    name: "Claude Agent".into(),
                    command: if cfg!(windows) {
                        "npm.cmd".into()
                    } else {
                        "claude-agent-acp".into()
                    },
                    args: if cfg!(windows) {
                        vec![
                            "exec".into(),
                            "--yes".into(),
                            "@zed-industries/claude-agent-acp@0.20.2".into(),
                        ]
                    } else {
                        vec![]
                    },
                    theme: None,
                },
                AgentConfig {
                    name: "Gemini CLI".into(),
                    command: "gemini".into(),
                    args: vec!["--experimental-acp".into()],
                    theme: None,
                },
                AgentConfig {
                    name: "Cursor".into(),
                    command: "cursor".into(),
                    args: vec!["agent".into(), "acp".into()],
                    theme: None,
                },
                AgentConfig {
                    name: "Goose".into(),
                    command: "goose".into(),
                    args: vec!["acp".into()],
                    theme: None,
                },
                AgentConfig {
                    name: "Codex (bridge)".into(),
                    command: "codex-acp".into(),
                    args: vec![],
                    theme: None,
                },
            ],
            acp: AcpConfig::default(),
            editing_engine: EditingEngineConfig::default(),
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            wrap_around: true,
            smart_case: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Breakpoint {
    pub id: Option<usize>,
    pub verified: bool,
    pub message: Option<String>,

    pub line: usize,
    pub column: Option<usize>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

type Diagnostics = BTreeMap<Uri, Vec<(lsp::Diagnostic, DiagnosticProvider)>>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct WorkspaceDiagnosticCounts {
    pub hints: u32,
    pub info: u32,
    pub warnings: u32,
    pub errors: u32,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: usize,
    pub message: Cow<'static, str>,
    pub severity: Severity,
    pub timestamp: Instant,
    pub timeout: Option<Duration>,
    pub dismissed: bool,
    /// Optional corner radius override for this notification
    pub corner_radius: Option<u8>,
}

impl Notification {
    pub fn new(message: impl Into<Cow<'static, str>>, severity: Severity) -> Self {
        Self {
            id: 0, // Will be set by NotificationManager
            message: message.into(),
            severity,
            timestamp: Instant::now(),
            timeout: None,
            dismissed: false,
            corner_radius: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn is_expired(&self) -> bool {
        if let Some(timeout) = self.timeout {
            let elapsed = self.timestamp.elapsed();
            let expired = elapsed >= timeout;
            if expired {
                log::warn!(
                    "Notification {} expired: elapsed={:?}, timeout={:?}",
                    self.id,
                    elapsed,
                    timeout
                );
            }
            expired
        } else {
            false
        }
    }

    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }
}

#[derive(Debug, Default)]
pub struct NotificationManager {
    notifications: Vec<Notification>,
    next_id: usize,
    max_history: usize,
}

impl NotificationManager {
    pub fn new(max_history: usize) -> Self {
        Self {
            notifications: Vec::new(),
            next_id: 1,
            max_history,
        }
    }

    pub fn add(&mut self, mut notification: Notification) -> usize {
        notification.id = self.next_id;
        self.next_id += 1;

        let id = notification.id; // Store the ID before moving
        self.notifications.push(notification);

        // Clean up old notifications if we exceed max_history
        if self.notifications.len() > self.max_history {
            let excess = self.notifications.len() - self.max_history;
            self.notifications.drain(0..excess);
        }

        id
    }

    pub fn dismiss(&mut self, id: usize) {
        if let Some(notification) = self.notifications.iter_mut().find(|n| n.id == id) {
            notification.dismiss();
        }
    }

    pub fn dismiss_all(&mut self) {
        for notification in &mut self.notifications {
            notification.dismiss();
        }
    }

    pub fn get_active(&self) -> Vec<&Notification> {
        self.notifications
            .iter()
            .filter(|n| !n.dismissed && !n.is_expired())
            .collect()
    }

    pub fn get_all(&self) -> &[Notification] {
        &self.notifications
    }

    pub fn cleanup_expired(&mut self) {
        self.notifications
            .retain(|n| !n.is_expired() && !n.dismissed);
    }

    pub fn clear_history(&mut self) {
        self.notifications.clear();
    }
}

/// Identifies the active editing context for a component's editing region.
/// Set by the compositor on `Editor` before key dispatch so that `focused!()`
/// routes to the component's viewport and document instead of the tree's.
#[derive(Clone, Copy, Debug)]
pub struct EditTarget {
    pub view_id: ViewId,
    pub doc_id: DocumentId,
}

pub struct Editor {
    /// Current editing mode.
    pub mode: Mode,
    pub tree: Tree,
    pub next_document_id: DocumentId,
    pub documents: BTreeMap<DocumentId, Document>,
    /// Documents owned by UI components (not shown in bufferline/pickers).
    pub component_docs: BTreeMap<DocumentId, Document>,
    /// Counter for allocating unique `ViewId`s for component-owned viewports.
    next_virtual_view_idx: u32,
    /// Per-viewport state for component-owned viewports that are not in the tree.
    pub component_views: BTreeMap<ViewId, ComponentViewState>,

    pub saves: HashMap<DocumentId, RuntimeSender<DocumentSavedEventFuture>>,
    save_tx: RuntimeSender<DocumentSavedEventFuture>,
    pub save_queue: RuntimeReceiver<DocumentSavedEventFuture>,
    pub write_count: usize,

    /// Snapshot of the currently focused editing surface's transient modal input state.
    /// Engines own the authoritative state; this is published for UI consumers
    /// that only have access to `Editor`.
    pub focused_modal_input: crate::engine::ModalInputState,
    pub registers: Registers,
    pub macro_recording: Option<(char, Vec<KeyEvent>)>,
    pub macro_replaying: Vec<char>,
    pub language_servers: helix_lsp::Registry,
    pub diagnostics: Diagnostics,
    pub workspace_diagnostic_counts: WorkspaceDiagnosticCounts,
    pub diff_providers: DiffProviderRegistry,

    pub debug_adapters: dap::registry::Registry,
    pub assistant_terminals: std::sync::Arc<helix_acp::TerminalManager>,
    /// Theme for the assistant panel only (when an agent has a theme configured).
    pub assistant_panel_theme: Option<Theme>,
    /// Shared engine factory for component-owned edit regions.
    pub engine_factory: Option<std::sync::Arc<dyn crate::engine::EditingEngineFactory>>,
    /// Shared modal keymap source for component-owned edit regions.
    pub modal_keymaps: Option<
        std::sync::Arc<
            arc_swap::ArcSwap<std::collections::HashMap<Mode, crate::keymap::ModalKeyTrie>>,
        >,
    >,
    pub breakpoints: HashMap<PathBuf, Vec<Breakpoint>>,

    /// Async runtime (UI / work / block / clock) threaded in from the application or test harness.
    runtime: helix_runtime::Runtime,

    pub syn_loader: Arc<ArcSwap<syntax::Loader>>,
    pub theme_loader: Arc<theme::Loader>,
    /// last_theme is used for theme previews. We store the current theme here,
    /// and if previewing is cancelled, we can return to it.
    pub last_theme: Option<Theme>,
    /// The currently applied editor theme. While previewing a theme, the previewed theme
    /// is set here.
    pub theme: Theme,

    /// The primary Selection prior to starting a goto_line_number preview. This is
    /// restored when the preview is aborted, or added to the jumplist when it is
    /// confirmed.
    pub last_selection: Option<Selection>,

    pub status_msg: Option<(Cow<'static, str>, Severity)>,
    pub notifications: NotificationManager,
    pub autoinfo: Option<Info>,

    pub config: Arc<dyn DynAccess<Config> + Send + Sync>,
    pub auto_pairs: Option<AutoPairs>,

    last_motion: Option<Motion>,
    pub last_completion: Option<CompleteAction>,
    last_cwd: Option<PathBuf>,

    pub exit_code: i32,

    pub config_events: (RuntimeSender<ConfigEvent>, RuntimeReceiver<ConfigEvent>),
    pub frame_gate: helix_runtime::FrameGate,
    pub needs_redraw: bool,
    /// Generation counter incremented on config changes. Used for render cache invalidation.
    pub config_gen: u64,
    /// Cached position of the cursor calculated during rendering.
    /// The content of `cursor_cache` is returned by `Editor::cursor` if
    /// set to `Some(_)`. The value will be cleared after it's used.
    /// If `cursor_cache` is `None` then the `Editor::cursor` function will
    /// calculate the cursor position.
    ///
    /// `Some(None)` represents a cursor position outside of the visible area.
    /// This will just cause `Editor::cursor` to return `None`.
    ///
    /// This cache is only a performance optimization to
    /// avoid calculating the cursor position multiple
    /// times during rendering and should not be set by other functions.
    pub handlers: Handlers,

    pub file_watcher: Option<crate::file_watcher::FileWatcher>,

    pub mouse_down_range: Option<Range>,
    pub cursor_cache: CursorCache,

    /// Shared UI state (layers, panels, focus). Commands mutate this; frontends render it.
    pub model: crate::model::Model,

    /// Collaboration-aware surface registry.
    pub surface_registry: crate::collab::Registry,

    /// Collaboration substrate (Phase 5+).
    pub collab: crate::collab::Store,
    /// Assistant durable state (Phase 6+).
    pub assistant: crate::assistant::Store,
    pub assistant_history: Option<crate::assistant::history::Backend>,
    pub assistant_context: crate::assistant::context::Registry,
    pub assistant_saves: BTreeMap<crate::assistant::thread::Id, helix_runtime::Debounce>,
    pub assistant_layout_save: helix_runtime::Debounce,
    pub assistant_backends:
        BTreeMap<crate::assistant::backend::Id, crate::assistant::BackendHandle>,
    pub assistant_updates_tx: RuntimeSender<crate::assistant::backend::Update>,
    pub assistant_updates_rx: RuntimeReceiver<crate::assistant::backend::Update>,
    assistant_follow_snapshot: Option<AssistantFollowSnapshot>,
    suppress_assistant_follow_pause: bool,

    /// Active benchmark state, set by `:bench` command.
    pub bench: Option<BenchState>,
}

pub type Motion = Box<dyn Fn(&mut Editor) + Send + Sync>;

#[derive(Debug)]
pub enum EditorEvent {
    CursorMoved,
    Scrolled,
    Edited,
    BufferSwitched,
    Redraw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AssistantFollowSnapshot {
    doc: DocumentId,
    version: i32,
    cursor: usize,
    scroll: usize,
}

pub struct AssistantUpdateOutcome {
    pub effects: Vec<crate::assistant::effect::Effect>,
    pub permission_request: Option<(
        crate::assistant::thread::Id,
        crate::assistant::permission::Request,
    )>,
}

#[derive(Debug, Clone)]
pub enum ConfigEvent {
    Refresh,
    Update(Box<Config>),
}

enum ThemeAction {
    Set,
    Preview,
}

#[derive(Debug, Clone)]
pub enum CompleteAction {
    Triggered,
    /// A savepoint of the currently selected completion. The savepoint
    /// MUST be restored before sending any event to the LSP
    Selected {
        savepoint: Arc<SavePoint>,
    },
    Applied {
        trigger_offset: usize,
        changes: Vec<Change>,
        placeholder: bool,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Action {
    Load,
    Replace,
    HorizontalSplit,
    VerticalSplit,
}

impl Action {
    /// Whether to align the view to the cursor after executing this action
    pub fn align_view(&self, view: &View, new_doc: DocumentId) -> bool {
        !matches!((self, view.doc == new_doc), (Action::Load, false))
    }
}

/// Error thrown on failed document closed
pub enum CloseError {
    /// Document doesn't exist
    DoesNotExist,
    /// Buffer is modified
    BufferModified(String),
    /// Document failed to save
    SaveError(anyhow::Error),
}

pub use crate::bench::{BenchSnapshot, BenchState};

fn render_assistant_change_summary(summary: &crate::assistant::change::Summary, title: &str) -> String {
    let mut body = format!("# {title}\n\n");
    for file in &summary.files {
        use std::fmt::Write as _;

        let _ = writeln!(body, "## {}\n", file.path.display());
        if file.hunks.is_empty() {
            body.push_str("- changed\n\n");
            continue;
        }
        for hunk in &file.hunks {
            let _ = writeln!(body, "- {}", hunk.summary);
        }
        body.push('\n');
    }
    body
}

impl Editor {
    pub fn buffer_label(&self, doc: &Document) -> String {
        let scratch = PathBuf::from(SCRATCH_BUFFER_NAME);

        if doc.path().is_none() {
            let scratch_docs: Vec<_> = self
                .documents()
                .filter(|candidate| candidate.path().is_none())
                .map(|candidate| candidate.id)
                .collect();
            if scratch_docs.len() > 1 {
                if let Some(index) = scratch_docs.iter().position(|id| *id == doc.id) {
                    let ordinal = index + 1;
                    return SCRATCH_BUFFER_NAME
                        .strip_suffix(']')
                        .map(|prefix| format!("{prefix} {ordinal}]"))
                        .unwrap_or_else(|| format!("{SCRATCH_BUFFER_NAME} {ordinal}"));
                }
            }
        }

        let paths: Vec<String> = self
            .documents()
            .map(|doc| {
                doc.path()
                    .unwrap_or(&scratch)
                    .to_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();

        let components: Vec<Vec<String>> = paths
            .iter()
            .map(|path| {
                path.split(std::path::MAIN_SEPARATOR)
                    .map(String::from)
                    .collect()
            })
            .collect();

        let doc_path = doc
            .path()
            .unwrap_or(&scratch)
            .to_str()
            .unwrap_or_default()
            .to_string();
        let doc_index = paths.iter().position(|path| path == &doc_path).unwrap_or(0);
        let doc_components_len = components[doc_index].len();

        let mut suffix_len = 1;
        loop {
            let start = doc_components_len.saturating_sub(suffix_len);
            let current = &components[doc_index][start..];

            let conflicts = components
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != doc_index)
                .filter(|(_, parts)| {
                    let start = parts.len().saturating_sub(suffix_len);
                    &parts[start..] == current
                })
                .count();

            if conflicts == 0 {
                return current.join(std::path::MAIN_SEPARATOR_STR);
            }

            suffix_len += 1;
        }
    }

    pub fn new(
        mut area: Rect,
        theme_loader: Arc<theme::Loader>,
        syn_loader: Arc<ArcSwap<syntax::Loader>>,
        config: Arc<dyn DynAccess<Config> + Send + Sync>,
        runtime: Runtime,
        handlers: Handlers,
    ) -> Self {
        let language_servers = helix_lsp::Registry::new(syn_loader.clone());
        let conf = config.load();
        let auto_pairs = (&conf.auto_pairs).into();
        let (assistant_updates_tx, assistant_updates_rx) = helix_runtime::channel(128);

        // HAXX: offset the render area height by 1 to account for prompt/commandline
        area.height -= 1;

        let (save_tx, save_queue) = helix_runtime::channel(64);

        Self {
            mode: Mode::Normal,
            tree: Tree::new(area),
            next_document_id: DocumentId::default(),
            documents: BTreeMap::new(),
            component_docs: BTreeMap::new(),
            next_virtual_view_idx: 0,
            component_views: BTreeMap::new(),
            saves: HashMap::new(),
            save_tx,
            save_queue,
            write_count: 0,
            focused_modal_input: crate::engine::ModalInputState::default(),
            macro_recording: None,
            macro_replaying: Vec::new(),
            theme: theme_loader.default(),
            language_servers,
            diagnostics: Diagnostics::new(),
            workspace_diagnostic_counts: WorkspaceDiagnosticCounts::default(),
            diff_providers: DiffProviderRegistry::default(),
            debug_adapters: dap::registry::Registry::new(),
            assistant_terminals: std::sync::Arc::new(helix_acp::TerminalManager::new()),
            assistant_panel_theme: None,
            engine_factory: None,
            modal_keymaps: None,
            breakpoints: HashMap::new(),
            runtime,
            syn_loader,
            theme_loader,
            last_theme: None,
            last_selection: None,
            registers: Registers::new(Box::new(arc_swap::access::Map::new(
                Arc::clone(&config),
                |config: &Config| &config.clipboard_provider,
            ))),
            status_msg: None,
            notifications: NotificationManager::new(conf.notifications.max_history),
            autoinfo: None,
            last_motion: None,
            last_completion: None,
            last_cwd: None,
            config,
            auto_pairs,
            exit_code: 0,
            config_events: helix_runtime::channel(64),
            frame_gate: helix_runtime::FrameGate::new(64),
            needs_redraw: false,
            config_gen: 0,
            handlers,
            file_watcher: None,
            mouse_down_range: None,
            cursor_cache: CursorCache::default(),
            model: crate::model::Model::default(),
            surface_registry: crate::collab::Registry::new(),
            collab: crate::collab::Store::default(),
            assistant: crate::assistant::Store::default(),
            assistant_history: None,
            assistant_context: crate::assistant::context::Registry::default(),
            assistant_saves: BTreeMap::new(),
            assistant_layout_save: helix_runtime::Debounce::new(std::time::Duration::from_millis(
                300,
            )),
            assistant_backends: BTreeMap::new(),
            assistant_updates_tx,
            assistant_updates_rx,
            assistant_follow_snapshot: None,
            suppress_assistant_follow_pause: false,
            bench: None,
        }
    }

    pub fn popup_border(&self) -> bool {
        self.config().popup_border == PopupBorderConfig::All
            || self.config().popup_border == PopupBorderConfig::Popup
    }

    pub fn take_save_queue(&mut self) -> RuntimeReceiver<DocumentSavedEventFuture> {
        std::mem::replace(&mut self.save_queue, helix_runtime::channel(1).1)
    }

    pub fn take_config_rx(&mut self) -> RuntimeReceiver<ConfigEvent> {
        std::mem::replace(&mut self.config_events.1, helix_runtime::channel(1).1)
    }

    pub fn take_redraw_rx(&mut self) -> RuntimeReceiver<()> {
        self.frame_gate.take_receiver()
    }

    pub fn take_assistant_updates_rx(
        &mut self,
    ) -> RuntimeReceiver<crate::assistant::backend::Update> {
        std::mem::replace(&mut self.assistant_updates_rx, helix_runtime::channel(1).1)
    }

    pub fn take_lsp_incoming(
        &mut self,
    ) -> SelectAll<helix_runtime::Receiver<(LanguageServerId, Call)>> {
        std::mem::replace(&mut self.language_servers.incoming, SelectAll::new())
    }

    pub fn take_debugger_incoming(
        &mut self,
    ) -> SelectAll<helix_runtime::Receiver<(dap::registry::DebugAdapterId, dap::Payload)>> {
        std::mem::replace(&mut self.debug_adapters.incoming, SelectAll::new())
    }

    fn bind_view_redraw(&self, view: &mut View) {
        view.diagnostics_handler.bind_runtime(self.runtime.clone());
        view.diagnostics_handler.bind_redraw(self.frame_gate.handle());
    }

    fn track_tree_surface(&mut self, view_id: ViewId) -> Option<crate::collab::SurfaceId> {
        if !self.tree.contains(view_id) {
            self.surface_registry.remove_view(view_id);
            return None;
        }

        let view = self.tree.get(view_id);
        Some(self.surface_registry.track(
            crate::collab::surface::kind::EDITOR,
            crate::collab::surface::Role::Editor,
            view.id,
            view.doc,
        ))
    }

    fn track_component_surface(
        &mut self,
        view_id: ViewId,
        doc_id: DocumentId,
    ) -> crate::collab::SurfaceId {
        self.surface_registry.track(
            crate::collab::surface::kind::ASSISTANT_THREAD,
            crate::collab::surface::Role::Auxiliary,
            view_id,
            doc_id,
        )
    }

    pub fn request_redraw(&self) {
        self.frame_gate.request_redraw();
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn menu_border(&self) -> bool {
        self.config().popup_border == PopupBorderConfig::All
            || self.config().popup_border == PopupBorderConfig::Menu
    }

    pub fn workspace_diagnostic_counts(&self) -> WorkspaceDiagnosticCounts {
        self.workspace_diagnostic_counts
    }

    pub fn active_thread(
        &self,
    ) -> Option<(
        crate::assistant::thread::Id,
        &crate::assistant::thread::Thread,
    )> {
        let thread = self.assistant.active()?;
        Some((thread, self.assistant.thread(thread)?))
    }

    pub fn assistant_thread(
        &self,
        thread: crate::assistant::thread::Id,
    ) -> Option<&crate::assistant::thread::Thread> {
        self.assistant.thread(thread)
    }

    pub fn assistant_threads(
        &self,
    ) -> Box<dyn Iterator<Item = &crate::assistant::thread::Thread> + '_> {
        self.assistant.threads()
    }

    pub fn assistant_view(&self, focused: bool) -> crate::assistant::view::View {
        self.assistant.view(focused)
    }

    pub fn active_assistant_view_thread(&self) -> Option<crate::assistant::view::Thread> {
        self.assistant_view(false).active
    }

    pub fn assistant_view_tabs(&self) -> Vec<crate::assistant::view::ThreadTab> {
        self.assistant_view(false).tabs
    }

    pub fn assistant_view_entry(
        &self,
        entry: crate::assistant::thread::EntryId,
    ) -> Option<crate::assistant::view::Entry> {
        self.active_assistant_view_thread()?
            .entries
            .into_iter()
            .find(|current| current.id == entry)
    }

    pub fn assistant_entry_details(
        &self,
        entry: crate::assistant::thread::EntryId,
    ) -> Option<String> {
        let model = self.assistant_model(false);
        let entry = model.entries.into_iter().find(|current| current.id == entry)?;
        Some(entry.details_markdown(&model.agent_name))
    }

    pub fn selected_assistant_thread_entry(
        &self,
    ) -> Option<(
        crate::assistant::thread::Id,
        crate::assistant::thread::Thread,
        crate::assistant::thread::Entry,
    )> {
        let (thread, state) = self.active_thread().map(|(thread, state)| (thread, state.clone()))?;
        let entry = state
            .selected_entry()
            .and_then(|id| state.entries().iter().find(|entry| entry.id == id))
            .cloned()?;
        Some((thread, state, entry))
    }

    pub fn assistant_thread_change_summary(
        &self,
        thread: &crate::assistant::thread::Thread,
    ) -> Option<crate::assistant::change::Summary> {
        let files = thread
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                crate::assistant::thread::EntryKind::ChangeSummary(summary) => {
                    Some(summary.files.clone())
                }
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        (!files.is_empty()).then_some(crate::assistant::change::Summary { files })
    }

    pub fn assistant_turn_change_summary(
        &self,
        thread: &crate::assistant::thread::Thread,
        entry: &crate::assistant::thread::Entry,
    ) -> Option<crate::assistant::change::Summary> {
        if let crate::assistant::thread::EntryKind::ChangeSummary(summary) = &entry.kind {
            return Some(summary.clone());
        }

        let turn = entry.turn?;
        let files = thread
            .entries()
            .iter()
            .filter(|candidate| candidate.turn == Some(turn))
            .filter_map(|candidate| match &candidate.kind {
                crate::assistant::thread::EntryKind::ChangeSummary(summary) => {
                    Some(summary.files.clone())
                }
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        (!files.is_empty()).then_some(crate::assistant::change::Summary { files })
    }

    pub fn selected_assistant_turn_change_summary(
        &self,
    ) -> Option<crate::assistant::change::Summary> {
        let (_thread_id, thread, entry) = self.selected_assistant_thread_entry()?;
        self.assistant_turn_change_summary(&thread, &entry)
    }

    pub fn active_assistant_thread_change_summary(
        &self,
    ) -> Option<crate::assistant::change::Summary> {
        let (_thread_id, thread) = self.active_thread()?;
        self.assistant_thread_change_summary(thread)
    }

    pub fn selected_assistant_entry(&self) -> Option<crate::assistant::thread::EntryId> {
        self.active_assistant_view_thread()?.selected
    }

    pub fn selected_assistant_entry_locations(
        &self,
    ) -> Option<Vec<crate::collab::Location>> {
        let entry = self.selected_assistant_entry()?;
        Some(self.assistant_view_entry(entry)?.locations)
    }

    pub fn assistant_entry_id_at(&self, index: usize) -> Option<crate::assistant::thread::EntryId> {
        self.active_assistant_view_thread()?
            .entries
            .get(index)
            .map(|entry| entry.id)
    }

    pub fn assistant_entry_is_folded(
        &self,
        entry: crate::assistant::thread::EntryId,
    ) -> bool {
        self.active_assistant_view_thread()
            .is_some_and(|thread| thread.folded.contains(&entry))
    }

    pub fn assistant_history_entries(
        &self,
        scope: &crate::assistant::thread::Scope,
    ) -> Option<Vec<crate::assistant::history::Stub>> {
        self.assistant.history(scope).map(|page| page.entries.clone())
    }

    pub fn assistant_layout_history_entries(&self) -> Vec<crate::assistant::history::Stub> {
        self.assistant_history_entries(&crate::assistant::layout::current_scope())
            .unwrap_or_default()
    }

    pub fn assistant_model(&self, focused: bool) -> crate::model::AssistantModel {
        self.assistant.model(
            focused,
            self.assistant_layout_history_entries(),
            self.active_thread()
                .and_then(|(_, thread)| thread.title().map(ToOwned::to_owned))
                .unwrap_or_else(|| {
                    if self.assistant.threads().next().is_none() {
                        "No agent".to_string()
                    } else {
                        "Agent".to_string()
                    }
                }),
        )
    }

    pub fn assistant_history_backend(
        &self,
    ) -> Option<crate::assistant::history::Backend> {
        self.assistant_history.clone()
    }

    pub fn set_assistant_history_backend(
        &mut self,
        history: crate::assistant::history::Backend,
    ) {
        self.assistant_history = Some(history);
    }

    pub fn set_assistant_context_registry(
        &mut self,
        registry: crate::assistant::context::Registry,
    ) {
        self.assistant_context = registry;
    }

    pub fn assistant_history_records(&self) -> Vec<crate::assistant::history::Record> {
        self.assistant_threads()
            .map(crate::assistant::history::Record::from_thread)
            .collect()
    }

    pub fn persist_assistant_layout(&self) {
        let scope = crate::assistant::layout::current_scope();
        let (open, active) = self.assistant_layout_threads(&scope);
        self.runtime
            .work()
            .spawn(async move {
                let _ = crate::assistant::layout::save_layout(&scope, open, active).await;
            })
            .detach();
    }

    pub async fn flush_assistant_persistence(&self) -> Vec<anyhow::Error> {
        let Some(history) = self.assistant_history_backend() else {
            return Vec::new();
        };

        let mut errors = Vec::new();
        let records = self.assistant_history_records();
        let scope = crate::assistant::layout::current_scope();
        let (open, active) = self.assistant_layout_threads(&scope);

        for record in records {
            if let Err(err) = history.save(record).await {
                errors.push(err);
            }
        }
        if let Err(err) = crate::assistant::layout::save_layout(&scope, open, active).await {
            errors.push(err);
        }
        errors
    }

    pub fn apply_assistant_effects(&mut self, effects: Vec<crate::assistant::effect::Effect>) {
        for effect in effects {
            match effect {
                crate::assistant::effect::Effect::EnsureParticipant { thread } => {
                    self.ensure_assistant_participant(thread);
                }
                crate::assistant::effect::Effect::LeaveParticipant { thread } => {
                    let _ = self.leave_participant(crate::assistant::thread::participant(thread));
                }
                crate::assistant::effect::Effect::PublishLocation { thread, location } => {
                    self.ensure_assistant_participant(thread);
                    let participant = crate::assistant::thread::participant(thread);
                    let _ = self.publish_location(participant, location);
                }
                crate::assistant::effect::Effect::RevealLocation { location } => {
                    self.suppress_assistant_follow_pause = true;
                    let _ = self.reveal_location(&location, Action::Replace);
                }
                crate::assistant::effect::Effect::SendBackendCommand { backend, command } => {
                    let Some(handle) = self.ensure_assistant_backend(&backend) else {
                        self.set_error(format!("Assistant backend missing: {backend}"));
                        continue;
                    };
                    self.runtime
                        .work()
                        .spawn(async move {
                            let _ = handle.send(command).await;
                        })
                        .detach();
                }
                crate::assistant::effect::Effect::OpenEntryDoc {
                    thread,
                    entry,
                    action,
                } => {
                    if let Some(effects) = self.open_assistant_entry_scratch(thread, entry, action) {
                        self.apply_assistant_effects(effects);
                    }
                }
                crate::assistant::effect::Effect::SetStatus { message } => {
                    self.set_status(message);
                }
                crate::assistant::effect::Effect::Save { thread } => {
                    self.save_assistant_thread(thread);
                }
                crate::assistant::effect::Effect::SaveNow { record } => {
                    self.save_assistant_record_now(record);
                }
                crate::assistant::effect::Effect::Delete { thread } => {
                    self.delete_assistant_thread(thread);
                }
                crate::assistant::effect::Effect::SyncModel => {
                    let scope = crate::assistant::layout::current_scope();
                    let (open, active) = self.assistant_layout_threads(&scope);
                    self.debounce_assistant_layout(async move {
                        let _ = crate::assistant::layout::save_layout(&scope, open, active).await;
                    });
                    self.request_redraw();
                }
            }
        }
    }

    pub fn assistant_record(
        &self,
        thread: crate::assistant::thread::Id,
    ) -> Option<crate::assistant::history::Record> {
        self.assistant.record(thread)
    }

    pub fn assistant_layout_threads(
        &self,
        scope: &crate::assistant::thread::Scope,
    ) -> (Vec<crate::assistant::thread::Id>, Option<crate::assistant::thread::Id>) {
        let open = self
            .assistant_threads()
            .filter(|thread| thread.scope() == scope)
            .map(|thread| thread.id)
            .collect();
        let active = self.active_thread().map(|(thread, _)| thread).filter(|thread| {
            self.assistant_thread(*thread)
                .is_some_and(|state| state.scope() == scope)
        });
        (open, active)
    }

    pub fn assistant_update_tx(
        &self,
    ) -> RuntimeSender<crate::assistant::backend::Update> {
        self.assistant_updates_tx.clone()
    }

    pub fn assistant_backend(
        &self,
        backend: &crate::assistant::backend::Id,
    ) -> Option<crate::assistant::BackendHandle> {
        self.assistant_backends.get(backend).cloned()
    }

    pub fn assistant_terminals(&self) -> std::sync::Arc<helix_acp::TerminalManager> {
        self.assistant_terminals.clone()
    }

    pub fn assistant_runtime(&self) -> Runtime {
        self.runtime.clone()
    }

    pub fn assistant_agent(
        &self,
        backend: &crate::assistant::backend::Id,
    ) -> Option<crate::editor::AgentConfig> {
        self.config()
            .agents
            .iter()
            .find(|agent| agent.command == backend.as_ref())
            .cloned()
    }

    pub fn cache_assistant_backend(&mut self, handle: crate::assistant::BackendHandle) {
        self.assistant_backends.insert(handle.id.clone(), handle);
    }

    pub fn spawn_assistant_backend(
        &mut self,
        command: String,
        args: Vec<String>,
    ) -> anyhow::Result<crate::assistant::BackendHandle> {
        let runtime = self.assistant_runtime();
        let cwd = std::env::current_dir().unwrap_or_default();
        let config = helix_acp::client::AgentConfig {
            command: command.clone(),
            args,
            env: Vec::new(),
            cwd: cwd.clone(),
            timeout_secs: 120,
        };
        let client_info = helix_acp::types::Implementation {
            name: "helix".to_string(),
            title: Some("Helix Editor".to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };

        let driver = crate::assistant::acp::Driver::new(config, client_info);
        let handle = driver.spawn(
            &runtime,
            crate::assistant::host::local_set(self),
            self.assistant_update_tx(),
            crate::assistant::backend::Connect {
                scope: crate::assistant::thread::Scope::new(cwd),
                context_servers: Vec::new(),
            },
        )?;
        self.cache_assistant_backend(handle.clone());
        Ok(handle)
    }

    pub fn ensure_assistant_backend(
        &mut self,
        backend: &crate::assistant::backend::Id,
    ) -> Option<crate::assistant::BackendHandle> {
        if let Some(handle) = self.assistant_backend(backend) {
            return Some(handle);
        }

        let agent = self.assistant_agent(backend)?;
        self.spawn_assistant_backend(agent.command, agent.args).ok()
    }

    pub fn connect_assistant_backend(
        &mut self,
        command: String,
        args: Vec<String>,
    ) -> anyhow::Result<(
        crate::assistant::backend::Id,
        Vec<crate::assistant::effect::Effect>,
    )> {
        let cwd = std::env::current_dir().unwrap_or_default();
        let handle = self.spawn_assistant_backend(command, args)?;
        let effects = self.new_assistant_thread(
            handle.id.clone(),
            crate::assistant::thread::Scope::new(cwd),
        );
        Ok((handle.id, effects))
    }

    pub fn close_active_assistant_thread(
        &mut self,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (thread, _) = self.active_thread().context("No active assistant thread")?;
        Ok(self.close_assistant_thread(thread))
    }

    pub fn new_assistant_thread_from_active_backend(
        &mut self,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (_, thread) = self.active_thread_owned().context("No active assistant thread")?;
        let crate::assistant::thread::Origin::Backend { backend, .. } = thread.origin() else {
            anyhow::bail!("Active assistant thread is not backend-backed")
        };
        Ok(self.new_assistant_thread(backend.clone(), thread.clone_scope()))
    }

    pub fn cancel_active_assistant_thread(
        &mut self,
    ) -> Option<Vec<crate::assistant::effect::Effect>> {
        let (thread, _) = self.active_thread()?;
        Some(self.cancel_assistant_thread(thread))
    }

    pub fn submit_active_assistant_prompt(
        &mut self,
        text: String,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .active_backend_thread()
            .context("Active assistant thread is not bound to a backend")?;
        Ok(self.submit_assistant_prompt(thread, text))
    }

    pub fn set_active_assistant_draft_if_changed(
        &mut self,
        text: String,
    ) -> Option<Vec<crate::assistant::effect::Effect>> {
        let (thread, state) = self.active_thread_owned()?;
        if state.draft() == text {
            return None;
        }
        Some(self.set_assistant_draft(thread, text))
    }

    pub fn attach_active_assistant_context(
        &mut self,
        item: crate::assistant::context::Kind,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (thread, _) = self.active_thread().context("No active assistant thread")?;
        Ok(self.attach_assistant_context(thread, item))
    }

    pub fn detach_active_assistant_context(
        &mut self,
        item: crate::assistant::context::Id,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (thread, _) = self.active_thread().context("No active assistant thread")?;
        Ok(self.detach_assistant_context(thread, item))
    }

    pub fn cycle_active_assistant_config(
        &mut self,
        key: &str,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (thread, state) = self.active_thread_owned().context("No active assistant thread")?;
        let Some((option, value)) = state.config().cycle(key) else {
            anyhow::bail!("No {key} options from assistant backend")
        };
        Ok(self.set_assistant_config(thread, option, value))
    }

    pub fn cycle_active_assistant_mode(
        &mut self,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (thread, state) = self.active_thread_owned().context("No active assistant thread")?;
        let Some(mode) = state.mode() else {
            anyhow::bail!("No mode options from assistant backend")
        };
        let next = match mode.selected() {
            crate::assistant::mode::Selected::Current(current) => {
                let ids: Vec<_> = mode.items().map(|item| item.id.clone()).collect();
                let idx = ids.iter().position(|id| id == current).unwrap_or(0);
                ids[(idx + 1) % ids.len()].clone()
            }
            crate::assistant::mode::Selected::Pending { next, .. } => next.clone(),
        };
        Ok(self.set_assistant_mode(thread, next))
    }

    pub fn toggle_active_assistant_follow(
        &mut self,
    ) -> anyhow::Result<(&'static str, Vec<crate::assistant::effect::Effect>)> {
        let (thread, _) = self.active_thread().context("No active assistant thread")?;
        Ok(self.toggle_assistant_follow(thread))
    }

    pub fn pause_active_assistant_follow(
        &mut self,
        reason: crate::collab::FollowPause,
    ) -> Option<Vec<crate::assistant::effect::Effect>> {
        let (thread, state) = self.active_thread_owned()?;
        if !matches!(state.follow(), crate::collab::FollowState::On { .. }) {
            return None;
        }
        Some(self.pause_assistant_follow(thread, reason))
    }

    pub fn assistant_act(
        &mut self,
        action: crate::assistant::Action,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant.act(action)
    }

    pub fn apply_assistant_thread_event(
        &mut self,
        thread: crate::assistant::thread::Id,
        event: crate::assistant::thread::Event,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant
            .apply(crate::assistant::event::Event::Thread { thread, event })
    }

    pub fn apply_assistant_backend_event(
        &mut self,
        backend: crate::assistant::backend::Id,
        event: crate::assistant::backend::Event,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant
            .apply(crate::assistant::event::Event::Backend { backend, event })
    }

    pub fn replace_assistant_history(
        &mut self,
        scope: crate::assistant::thread::Scope,
        entries: Vec<crate::assistant::history::Stub>,
        next: Option<crate::assistant::history::Cursor>,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant.replace_history(scope, entries, next)
    }

    pub fn apply_assistant_location_update(
        &mut self,
        thread: crate::assistant::thread::Id,
        location: crate::collab::Location,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.apply_assistant_thread_event(thread, crate::assistant::thread::Event::Follow(location))
    }

    pub fn apply_assistant_terminal_event(
        &mut self,
        thread: crate::assistant::thread::Id,
        event: crate::assistant::terminal::Event,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.apply_assistant_thread_event(thread, crate::assistant::thread::Event::Terminal(event))
    }

    pub fn apply_assistant_update(
        &mut self,
        update: crate::assistant::backend::Update,
    ) -> AssistantUpdateOutcome {
        use crate::assistant::backend;

        match update {
            backend::Update::Thread { thread, event } => AssistantUpdateOutcome {
                effects: self.apply_assistant_thread_event(thread, event),
                permission_request: None,
            },
            backend::Update::Permission { thread, request } => AssistantUpdateOutcome {
                effects: Vec::new(),
                permission_request: Some((thread, request)),
            },
            backend::Update::Backend { backend, event } => AssistantUpdateOutcome {
                effects: self.apply_assistant_backend_event(backend, event),
                permission_request: None,
            },
            backend::Update::History {
                scope,
                entries,
                next,
            } => AssistantUpdateOutcome {
                effects: self.replace_assistant_history(scope, entries, next),
                permission_request: None,
            },
            backend::Update::Location { thread, location } => AssistantUpdateOutcome {
                effects: self.apply_assistant_location_update(thread, location),
                permission_request: None,
            },
            backend::Update::Terminal { thread, event } => AssistantUpdateOutcome {
                effects: self.apply_assistant_terminal_event(thread, event),
                permission_request: None,
            },
            backend::Update::Error { at, error } => {
                match at {
                    backend::Target::Backend(_) => self.set_error(error.to_string()),
                    backend::Target::Thread(_) => self.set_status(error.to_string()),
                }
                AssistantUpdateOutcome {
                    effects: Vec::new(),
                    permission_request: None,
                }
            }
        }
    }

    pub fn untrack_assistant_doc(
        &mut self,
        doc: DocumentId,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::UntrackDoc { doc })
    }

    pub fn activate_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Activate { thread })
    }

    pub fn focus_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
        focus: crate::assistant::thread::Focus,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Focus { thread, focus })
    }

    pub fn select_assistant_entry(
        &mut self,
        thread: crate::assistant::thread::Id,
        entry: Option<crate::assistant::thread::EntryId>,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::SelectEntry { thread, entry })
    }

    pub fn cycle_active_assistant_thread(
        &mut self,
        delta: isize,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let tabs = self.assistant_view_tabs();
        if tabs.len() < 2 {
            anyhow::bail!("Need at least two assistant threads")
        }

        let active = self.active_thread().context("No active assistant thread")?.0;
        let index = tabs
            .iter()
            .position(|tab| tab.id == active)
            .context("Active assistant thread missing from tabs")?;
        let next = (index as isize + delta).rem_euclid(tabs.len() as isize) as usize;
        Ok(self.activate_assistant_thread(tabs[next].id))
    }

    pub fn set_active_assistant_focus(
        &mut self,
        focus: crate::assistant::thread::Focus,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (thread, _) = self.active_thread().context("No active assistant thread")?;
        Ok(self.focus_assistant_thread(thread, focus))
    }

    pub fn select_active_assistant_entry(
        &mut self,
        entry: Option<crate::assistant::thread::EntryId>,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (thread, _) = self.active_thread().context("No active assistant thread")?;
        Ok(self.select_assistant_entry(thread, entry))
    }

    pub fn load_assistant_thread(
        &mut self,
        record: crate::assistant::history::Record,
        activate: bool,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::LoadThread { record, activate })
    }

    pub fn close_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Close { thread })
    }

    pub fn new_assistant_thread(
        &mut self,
        backend: crate::assistant::backend::Id,
        scope: crate::assistant::thread::Scope,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::NewThread { backend, scope })
    }

    pub fn cancel_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Cancel { thread })
    }

    pub fn attach_assistant_context(
        &mut self,
        thread: crate::assistant::thread::Id,
        item: crate::assistant::context::Kind,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::AttachContext { thread, item })
    }

    pub fn detach_assistant_context(
        &mut self,
        thread: crate::assistant::thread::Id,
        item: crate::assistant::context::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::DetachContext { thread, item })
    }

    pub fn set_assistant_config(
        &mut self,
        thread: crate::assistant::thread::Id,
        option: crate::assistant::config::Id,
        value: crate::assistant::config::ValueId,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::SetConfig {
            thread,
            option,
            value,
        })
    }

    pub fn set_assistant_mode(
        &mut self,
        thread: crate::assistant::thread::Id,
        mode: crate::assistant::mode::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::SetMode { thread, mode })
    }

    pub fn set_assistant_draft(
        &mut self,
        thread: crate::assistant::thread::Id,
        text: String,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::SetDraft { thread, text })
    }

    pub fn resolve_assistant_permission(
        &mut self,
        thread: crate::assistant::thread::Id,
        request: crate::assistant::permission::RequestId,
        decision: crate::assistant::permission::Decision,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::ResolvePermission {
            thread,
            request,
            decision,
        })
    }

    pub fn submit_assistant_prompt(
        &mut self,
        thread: crate::assistant::thread::Id,
        text: String,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Submit { thread, text })
    }

    pub fn open_assistant_entry_doc(
        &mut self,
        thread: crate::assistant::thread::Id,
        entry: crate::assistant::thread::EntryId,
        action: Action,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::OpenEntryDoc {
            thread,
            entry,
            action,
        })
    }

    pub fn open_assistant_entry_scratch(
        &mut self,
        thread: crate::assistant::thread::Id,
        entry: crate::assistant::thread::EntryId,
        action: Action,
    ) -> Option<Vec<crate::assistant::effect::Effect>> {
        let Some(details) = self.assistant_entry_details(entry) else {
            return None;
        };
        let opened = self
            .active_assistant_view_thread()
            .and_then(|thread| thread.opened_docs.get(&entry).copied());

        if let Some(doc_id) = opened {
            if self.switch_document_if_exists(doc_id, action) {
                return Some(Vec::new());
            }
            let effects = self.untrack_assistant_entry_doc(thread, entry);
            let doc_id = self.open_markdown_scratch(action, details);
            let mut next = self.track_assistant_entry_doc(thread, entry, doc_id);
            let mut all = effects;
            all.append(&mut next);
            return Some(all);
        }

        let doc_id = self.open_markdown_scratch(action, details);
        Some(self.track_assistant_entry_doc(thread, entry, doc_id))
    }

    pub fn open_selected_assistant_entry_scratch(
        &mut self,
        action: Action,
    ) -> Option<Vec<crate::assistant::effect::Effect>> {
        let Some(entry) = self.selected_assistant_entry() else {
            return None;
        };
        let Some((thread, _)) = self.active_thread() else {
            return None;
        };
        self.open_assistant_entry_scratch(thread, entry, action)
    }

    pub fn untrack_assistant_entry_doc(
        &mut self,
        thread: crate::assistant::thread::Id,
        entry: crate::assistant::thread::EntryId,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::UntrackEntryDoc { thread, entry })
    }

    pub fn track_assistant_entry_doc(
        &mut self,
        thread: crate::assistant::thread::Id,
        entry: crate::assistant::thread::EntryId,
        doc: DocumentId,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::TrackEntryDoc { thread, entry, doc })
    }

    pub fn pause_assistant_follow(
        &mut self,
        thread: crate::assistant::thread::Id,
        reason: crate::collab::FollowPause,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::PauseFollow { thread, reason })
    }

    pub fn follow_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Follow { thread })
    }

    pub fn unfollow_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Unfollow { thread })
    }

    pub fn toggle_assistant_follow(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> (&'static str, Vec<crate::assistant::effect::Effect>) {
        let follow = self
            .assistant_thread(thread)
            .map(|thread| thread.follow().clone())
            .unwrap_or(crate::collab::FollowState::Off);

        let status = match follow {
            crate::collab::FollowState::Off => "Assistant follow enabled",
            crate::collab::FollowState::On { .. } => "Assistant follow disabled",
            crate::collab::FollowState::Paused { .. } => "Assistant follow resumed",
        };

        let effects = match follow {
            crate::collab::FollowState::Off | crate::collab::FollowState::Paused { .. } => {
                self.follow_assistant_thread(thread)
            }
            crate::collab::FollowState::On { .. } => self.unfollow_assistant_thread(thread),
        };
        (status, effects)
    }

    pub fn open_selected_assistant_turn_changes(&mut self) -> bool {
        let Some(summary) = self.selected_assistant_turn_change_summary() else {
            return false;
        };
        self.open_markdown_scratch(
            Action::Replace,
            render_assistant_change_summary(&summary, "Turn Changes"),
        );
        true
    }

    pub fn open_active_assistant_thread_changes(&mut self) -> bool {
        let Some(summary) = self.active_assistant_thread_change_summary() else {
            return false;
        };
        self.open_markdown_scratch(
            Action::Replace,
            render_assistant_change_summary(&summary, "Thread Changes"),
        );
        true
    }

    pub fn save_assistant_thread(&mut self, thread: crate::assistant::thread::Id) {
        let (Some(history), Some(record)) = (
            self.assistant_history.clone(),
            self.assistant_record(thread),
        ) else {
            return;
        };

        self.assistant_saves
            .entry(thread)
            .or_insert_with(|| helix_runtime::Debounce::new(std::time::Duration::from_millis(300)))
            .restart(&self.runtime.work(), &self.runtime.clock(), async move {
                let _ = history.save(record).await;
            });
    }

    pub fn save_assistant_record_now(&mut self, record: crate::assistant::history::Record) {
        if let Some(debounce) = self.assistant_saves.get_mut(&record.id) {
            debounce.cancel();
        }
        if let Some(history) = self.assistant_history.clone() {
            self.runtime
                .work()
                .spawn(async move {
                    let _ = history.save(record).await;
                })
                .detach();
        }
    }

    pub fn delete_assistant_thread(&mut self, thread: crate::assistant::thread::Id) {
        if let Some(debounce) = self.assistant_saves.get_mut(&thread) {
            debounce.cancel();
        }
        if let Some(history) = self.assistant_history.clone() {
            self.runtime
                .work()
                .spawn(async move {
                    let _ = history.delete(thread).await;
                })
                .detach();
        }
    }

    pub fn debounce_assistant_layout<F>(&mut self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.assistant_layout_save
            .restart(&self.runtime.work(), &self.runtime.clock(), future);
    }

    pub fn active_thread_mut(
        &mut self,
    ) -> Option<(
        crate::assistant::thread::Id,
        &mut crate::assistant::thread::Thread,
    )> {
        let thread = self.assistant.active()?;
        Some((thread, self.assistant.thread_mut(thread)?))
    }

    pub fn active_backend_thread(&self) -> Option<crate::assistant::thread::Id> {
        let (thread, state) = self.active_thread()?;
        matches!(state.origin(), crate::assistant::thread::Origin::Backend { .. }).then_some(thread)
    }

    pub fn active_thread_owned(
        &self,
    ) -> Option<(
        crate::assistant::thread::Id,
        crate::assistant::thread::Thread,
    )> {
        self.active_thread().map(|(thread, state)| (thread, state.clone()))
    }

    pub fn active_thread_snapshot(&self) -> Option<crate::assistant::thread::Snapshot> {
        let (_, thread) = self.active_thread()?;
        Some(thread.snapshot())
    }

    pub fn active_thread_context(&self) -> Option<Vec<crate::assistant::context::Item>> {
        let (_, thread) = self.active_thread()?;
        Some(thread.context_items().to_vec())
    }

    pub fn active_thread_scope_or_layout(&self) -> crate::assistant::thread::Scope {
        self.active_thread()
            .map(|(_, thread)| thread.clone_scope())
            .unwrap_or_else(crate::assistant::layout::current_scope)
    }

    pub fn publish_location(
        &mut self,
        participant: crate::collab::ParticipantId,
        location: crate::collab::Location,
    ) -> Result<Vec<crate::collab::Effect>, crate::collab::MissingParticipant> {
        let location = self.resolve_location_surface(location);
        self.collab.publish_location(participant, location)
    }

    pub fn apply_collab_effects(&mut self, effects: Vec<crate::collab::Effect>) {
        let mut sync_presence = false;
        let mut reveals = Vec::new();

        for effect in effects {
            match effect {
                crate::collab::Effect::Open { .. } | crate::collab::Effect::ClearPresence { .. } => {
                    sync_presence = true;
                }
                crate::collab::Effect::Reveal { location, .. } => {
                    sync_presence = true;
                    reveals.push(location);
                }
                crate::collab::Effect::ShowPresence { surface, presence } => {
                    self.render_presence(surface, &presence);
                }
            }
        }

        if sync_presence {
            self.sync_collab_presence();
        }

        for location in reveals {
            let _ = self.reveal_location(&location, Action::Replace);
        }
    }

    fn current_assistant_follow_snapshot(&self) -> Option<AssistantFollowSnapshot> {
        let view = self.tree.get(self.tree.focus);
        let doc = self.document(view.doc)?;
        Some(AssistantFollowSnapshot {
            doc: view.doc,
            version: doc.version(),
            cursor: doc.selection(view.id).primary().cursor(doc.text().slice(..)),
            scroll: doc.view_offset(view.id).vertical_offset,
        })
    }

    pub fn pause_assistant_follow_if_local_change(&mut self) {
        let current = self.current_assistant_follow_snapshot();
        let Some(snapshot) = current.clone() else {
            self.assistant_follow_snapshot = current;
            self.suppress_assistant_follow_pause = false;
            return;
        };

        let Some(previous) = self.assistant_follow_snapshot.replace(snapshot.clone()) else {
            self.suppress_assistant_follow_pause = false;
            return;
        };

        if previous == snapshot {
            self.suppress_assistant_follow_pause = false;
            return;
        }

        if self.suppress_assistant_follow_pause {
            self.suppress_assistant_follow_pause = false;
            return;
        }

        let event = if previous.doc != snapshot.doc {
            EditorEvent::BufferSwitched
        } else if previous.version != snapshot.version {
            EditorEvent::Edited
        } else if previous.cursor != snapshot.cursor {
            EditorEvent::CursorMoved
        } else if previous.scroll != snapshot.scroll {
            EditorEvent::Scrolled
        } else {
            return;
        };

        let Some(reason) = self.pause_current_surface(&event) else {
            return;
        };
        if let Some(effects) = self.pause_active_assistant_follow(reason) {
            self.apply_assistant_effects(effects);
        }
    }

    pub fn participant(
        &self,
        participant: crate::collab::ParticipantId,
    ) -> Option<&crate::collab::Participant> {
        self.collab.participant(participant)
    }

    pub fn join_participant(
        &mut self,
        participant: crate::collab::Participant,
    ) -> Vec<crate::collab::Effect> {
        self.collab.join(participant)
    }

    pub fn leave_participant(
        &mut self,
        participant: crate::collab::ParticipantId,
    ) -> Vec<crate::collab::Effect> {
        self.collab.leave(participant)
    }

    pub fn ensure_assistant_participant(&mut self, thread: crate::assistant::thread::Id) {
        let participant = crate::assistant::thread::participant(thread);
        if self.participant(participant).is_some() {
            return;
        }

        let name = self
            .assistant_thread(thread)
            .and_then(|thread| thread.title().map(ToOwned::to_owned))
            .unwrap_or_else(|| format!("assistant-{}", thread.value().get()));
        let effects = self.join_participant(crate::collab::Participant {
            id: participant,
            kind: crate::collab::participant::Kind::Agent,
            name,
            access: crate::collab::participant::Access::Read,
        });
        self.apply_collab_effects(effects);
    }

    fn resolve_location_surface(&self, mut location: crate::collab::Location) -> crate::collab::Location {
        if location.surface.is_none() {
            location.surface = self.surface_for_location(&location);
        }
        location
    }

    fn surface_for_location(&self, location: &crate::collab::Location) -> Option<crate::collab::SurfaceId> {
        if let Some(surface) = location.surface.filter(|id| self.surface_registry.get(*id).is_some()) {
            return Some(surface);
        }

        let doc_id = self.document_id_by_path(&location.path)?;
        self.surface_registry
            .surfaces()
            .filter(|surface| surface.doc == doc_id)
            .min_by_key(|surface| match surface.role {
                crate::collab::surface::Role::Editor => 0,
                crate::collab::surface::Role::Auxiliary => 1,
            })
            .map(|surface| surface.id)
    }

    fn snapshot_presence(
        &self,
        participant: crate::collab::ParticipantId,
        location: &crate::collab::Location,
    ) -> Option<crate::collab::Presence> {
        let surface = self.surface_for_location(location)?;
        let viewport = self
            .with_surface(surface, |surface_ref| match surface_ref {
                crate::collab::surface::Ref::Tree { view, doc } => {
                    let offset = doc.view_offset(view.id);
                    crate::collab::ViewportAnchor::new(
                        location.range.map(|range| range.head).unwrap_or(offset.anchor),
                        offset.vertical_offset,
                        offset.horizontal_offset,
                    )
                }
                crate::collab::surface::Ref::Component { view, doc } => {
                    let offset = doc.view_offset(view.id);
                    crate::collab::ViewportAnchor::new(
                        location.range.map(|range| range.head).unwrap_or(offset.anchor),
                        offset.vertical_offset,
                        offset.horizontal_offset,
                    )
                }
            })
            .ok();

        let cursor = location
            .range
            .map(|range| crate::collab::RangeAnchor::new(range.head, range.head));
        let selection = location.range.filter(|range| range.anchor != range.head);

        Some(crate::collab::Presence {
            participant,
            surface,
            cursor,
            selection,
            viewport,
        })
    }

    fn derived_presence_for_surface(
        &self,
        surface: crate::collab::SurfaceId,
    ) -> Vec<crate::collab::Presence> {
        self.collab
            .locations()
            .filter_map(|(participant, location)| self.snapshot_presence(participant, location))
            .filter(|presence| presence.surface == surface)
            .collect()
    }

    fn render_presence(
        &mut self,
        surface: crate::collab::SurfaceId,
        presence: &[crate::collab::Presence],
    ) {
        let annotations = crate::collab::surface::presence_annotations(self, presence);
        let _ = self.with_surface_mut(surface, |surface_ref| match surface_ref {
            crate::collab::surface::Mut::Tree { view, doc } => {
                doc.set_presence_annotations(view.id, annotations.clone());
            }
            crate::collab::surface::Mut::Component { view, doc } => {
                doc.set_presence_annotations(view.id, annotations.clone());
            }
        });
    }

    fn clear_surface_presence(&mut self, surface: crate::collab::SurfaceId) {
        let _ = self.collab.clear_presence(surface);
        self.render_presence(surface, &[]);
    }

    fn sync_collab_presence(&mut self) {
        let surfaces: Vec<_> = self.surface_registry.surfaces().map(|surface| surface.id).collect();
        let snapshots: Vec<_> = surfaces
            .iter()
            .copied()
            .map(|surface| (surface, self.derived_presence_for_surface(surface)))
            .collect();

        for (surface, presence) in snapshots {
            if presence.is_empty() {
                self.clear_surface_presence(surface);
            } else {
                let _ = self.collab.show_presence(surface, presence.clone());
                self.render_presence(surface, &presence);
            }
        }
    }

    pub fn reveal_location(
        &mut self,
        location: &crate::collab::Location,
        action: crate::editor::Action,
    ) -> Result<(), DocumentOpenError> {
        let doc_id = self.open(&location.path, action)?;
        let target_view = location
            .surface
            .and_then(|id| self.surface_registry.get(id))
            .map(|surface| surface.view)
            .filter(|view_id| self.tree.contains(*view_id))
            .or_else(|| {
                self.tree
                    .views()
                    .find(|(view, _)| view.doc == doc_id)
                    .map(|(view, _)| view.id)
            })
            .unwrap_or(self.tree.focus);
        self.reveal_location_in_view(target_view, doc_id, location);
        Ok(())
    }

    fn reveal_location_in_view(
        &mut self,
        view_id: ViewId,
        doc_id: DocumentId,
        location: &crate::collab::Location,
    ) {
        if self.tree.contains(view_id) && self.tree.focus != view_id {
            self.focus(view_id);
        }

        let scrolloff = self.config().scrolloff;
        self.with_view_doc_mut(view_id, doc_id, |view, doc| {
            if view.doc_id() != doc_id {
                return;
            }

            doc.ensure_view_init(view_id);
            view.sync_changes(doc);

            if let Some(range) = location.range {
                doc.set_selection(view_id, Selection::single(range.anchor, range.head));
            }

            ensure_cursor_in_view_center_in(view, doc, scrolloff);
        });
    }

    pub fn apply_presence(
        &mut self,
        surface: crate::collab::SurfaceId,
        presence: Vec<crate::collab::Presence>,
    ) -> Vec<crate::collab::Effect> {
        let effects = self.collab.show_presence(surface, presence.clone());
        self.render_presence(surface, &presence);
        effects
    }

    #[cfg(test)]
    fn collab_test_editor() -> Self {
        let theme_loader = Arc::new(theme::Loader::new(&[]));
        let syn_loader = Arc::new(ArcSwap::from_pointee(syntax::Loader::default()));
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        let tokio = Box::leak(Box::new(tokio::runtime::Runtime::new().expect("runtime")));
        let _guard = tokio.enter();
        let runtime = helix_runtime::Runtime::new(tokio.handle().clone());
        let handlers = crate::handlers::Handlers::dummy();
        let mut editor = Editor::new(
            Rect::new(0, 0, 120, 40),
            theme_loader,
            syn_loader,
            config,
            runtime,
            handlers,
        );
        let doc_id = editor.new_document(Document::default(editor.config.clone(), editor.syn_loader.clone()));
        let mut view = View::new(doc_id, editor.config().gutters.clone());
        editor.bind_view_redraw(&mut view);
        let view_id = editor.tree.insert(view);
        let _ = editor.track_tree_surface(view_id);
        let doc = doc_mut!(editor, &doc_id);
        doc.ensure_view_init(view_id);
        doc.mark_as_focused();
        editor
    }

    #[cfg(test)]
    fn collab_test_location(
        editor: &Editor,
        participant: crate::collab::ParticipantId,
        range: std::ops::Range<usize>,
    ) -> crate::collab::Location {
        let view = editor.tree.get(editor.tree.focus);
        let doc = editor.document(view.doc).expect("doc");
        let path = doc.path().map(|path| path.to_path_buf()).unwrap_or_else(|| {
            PathBuf::from(format!("participant-{}.rs", participant.value().get()))
        });

        let mut location = crate::collab::Location::new(path, crate::collab::location::Source::Tool)
            .with_range(crate::collab::RangeAnchor::new(range.start, range.end));
        if let Some(surface) = editor.surface_registry.get_by_view(view.id) {
            location = location.on_surface(surface);
        }
        location
    }

    #[cfg(test)]
    fn collab_test_path(&self) -> PathBuf {
        let view = self.tree.get(self.tree.focus);
        let doc = self.document(view.doc).expect("doc");
        doc.path()
            .map(|path| path.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("collab-test.rs"))
    }

    pub fn refresh_workspace_diagnostic_counts(&mut self) {
        let mut counts = WorkspaceDiagnosticCounts::default();
        for (diagnostic, _) in self.diagnostics.values().flatten() {
            match diagnostic.severity {
                Some(lsp::DiagnosticSeverity::WARNING) => counts.warnings += 1,
                Some(lsp::DiagnosticSeverity::ERROR) => counts.errors += 1,
                Some(lsp::DiagnosticSeverity::HINT) => counts.hints += 1,
                Some(lsp::DiagnosticSeverity::INFORMATION) => counts.info += 1,
                _ => counts.hints += 1,
            }
        }
        self.workspace_diagnostic_counts = counts;
    }

    pub fn apply_motion<F: Fn(&mut Self) + Send + Sync + 'static>(&mut self, motion: F) {
        motion(self);
        self.last_motion = Some(Box::new(motion));
    }

    pub fn repeat_last_motion(&mut self, count: usize) {
        if let Some(motion) = self.last_motion.take() {
            for _ in 0..count {
                motion(self);
            }
            self.last_motion = Some(motion);
        }
    }
    /// Current editing mode for the [`Editor`].
    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn config(&self) -> DynGuard<Config> {
        self.config.load()
    }

    /// Call if the config has changed to let the editor update all
    /// relevant members.
    pub fn refresh_config(&mut self, old_config: &Config) {
        let config = self.config();
        self.auto_pairs = (&config.auto_pairs).into();
        self._refresh();
        helix_event::dispatch(crate::events::ConfigDidChange {
            editor: self,
            old: old_config,
            new: &config,
        })
    }

    pub fn clear_status(&mut self) {
        self.status_msg = None;
        self.notifications.dismiss_all();
    }

    #[inline]
    pub fn set_status<T: Into<Cow<'static, str>>>(&mut self, status: T) {
        let status = status.into();
        log::debug!("editor status: {}", status);

        let config = self.config();
        if config.notifications.enable && config.notifications.style == NotificationStyle::Popup {
            // Only create notification, don't set status_msg for popup style
            self.notify_info(status);
        } else {
            // Traditional behavior: set status_msg and optionally create notification
            self.status_msg = Some((status.clone(), Severity::Info));
            if config.notifications.enable {
                self.notify_info(status);
            }
        }
    }

    #[inline]
    pub fn set_error<T: Into<Cow<'static, str>>>(&mut self, error: T) {
        let error = error.into();
        log::debug!("editor error: {}", error);

        let config = self.config();
        if config.notifications.enable && config.notifications.style == NotificationStyle::Popup {
            // Only create notification, don't set status_msg for popup style
            self.notify_error(error);
        } else {
            // Traditional behavior: set status_msg and optionally create notification
            self.status_msg = Some((error.clone(), Severity::Error));
            if config.notifications.enable {
                self.notify_error(error);
            }
        }
    }

    #[inline]
    pub fn set_result<T: Into<Cow<'static, str>>>(&mut self, result: Result<T, T>) {
        match result {
            Ok(ok) => self.set_status(ok),
            Err(err) => self.set_error(err),
        }
    }

    #[inline]
    pub fn set_warning<T: Into<Cow<'static, str>>>(&mut self, warning: T) {
        let warning = warning.into();
        log::warn!("editor warning: {}", warning);

        let config = self.config();
        if config.notifications.enable && config.notifications.style == NotificationStyle::Popup {
            // Only create notification, don't set status_msg for popup style
            self.notify_warning(warning);
        } else {
            // Traditional behavior: set status_msg and optionally create notification
            self.status_msg = Some((warning.clone(), Severity::Warning));
            if config.notifications.enable {
                self.notify_warning(warning);
            }
        }
    }

    #[inline]
    pub fn get_status(&self) -> Option<(&Cow<'static, str>, &Severity)> {
        self.status_msg
            .as_ref()
            .map(|(status, sev)| (status, sev))
            .or_else(|| {
                self.notifications
                    .get_active()
                    .last()
                    .map(|n| (&n.message, &n.severity))
            })
    }

    /// Returns true if the current status is an error
    #[inline]
    pub fn is_err(&self) -> bool {
        self.get_status()
            .map(|(_, sev)| *sev == Severity::Error)
            .unwrap_or(false)
    }

    // Notification methods
    pub fn notify<T: Into<Cow<'static, str>>>(&mut self, message: T) -> usize {
        self.notify_with_severity(message, Severity::Info)
    }

    pub fn notify_info<T: Into<Cow<'static, str>>>(&mut self, message: T) -> usize {
        self.notify_with_severity(message, Severity::Info)
    }

    pub fn notify_warning<T: Into<Cow<'static, str>>>(&mut self, message: T) -> usize {
        self.notify_with_severity(message, Severity::Warning)
    }

    pub fn notify_error<T: Into<Cow<'static, str>>>(&mut self, message: T) -> usize {
        self.notify_with_severity(message, Severity::Error)
    }

    pub fn notify_with_severity<T: Into<Cow<'static, str>>>(
        &mut self,
        message: T,
        severity: Severity,
    ) -> usize {
        let config = self.config();
        if !config.notifications.enable {
            // Fall back to traditional status messages if notifications are disabled
            match severity {
                Severity::Error => self.set_error(message),
                Severity::Warning => self.set_warning(message),
                _ => self.set_status(message),
            }
            return 0;
        }

        let mut notification = Notification::new(message, severity);

        // Set default timeout if configured
        if config.notifications.default_timeout > Duration::ZERO {
            let timeout = config.notifications.default_timeout;
            notification = notification.with_timeout(timeout);
            // Schedule a redraw at the timeout moment so the UI can expire/fade it immediately
            let runtime = self.runtime.clone();
            let redraw = self.frame_gate.handle();
            let redraw = async move {
                tokio::time::sleep(timeout).await;
                redraw.request_redraw();
            };
            runtime.work().spawn(redraw).detach();
        }

        let id = self.notifications.add(notification);

        // Also set status message for compatibility if using statusline style
        if config.notifications.style == NotificationStyle::Statusline {
            match severity {
                Severity::Error => {
                    let msg = self
                        .notifications
                        .notifications
                        .last()
                        .unwrap()
                        .message
                        .clone();
                    self.status_msg = Some((msg, Severity::Error));
                }
                Severity::Warning => {
                    let msg = self
                        .notifications
                        .notifications
                        .last()
                        .unwrap()
                        .message
                        .clone();
                    self.status_msg = Some((msg, Severity::Warning));
                }
                _ => {
                    let msg = self
                        .notifications
                        .notifications
                        .last()
                        .unwrap()
                        .message
                        .clone();
                    self.status_msg = Some((msg, Severity::Info));
                }
            }
        }

        id
    }

    pub fn dismiss_notification(&mut self, id: usize) {
        self.notifications.dismiss(id);
    }

    pub fn dismiss_all_notifications(&mut self) {
        self.notifications.dismiss_all();
    }

    pub fn get_active_notifications(&self) -> Vec<&Notification> {
        self.notifications.get_active()
    }

    pub fn get_notification_history(&self) -> &[Notification] {
        self.notifications.get_all()
    }

    pub fn clear_notification_history(&mut self) {
        self.notifications.clear_history();
    }

    pub fn cleanup_notifications(&mut self) {
        self.notifications.cleanup_expired();
    }

    pub fn unset_theme_preview(&mut self) {
        if let Some(last_theme) = self.last_theme.take() {
            self.set_theme(last_theme);
        }
        // None likely occurs when the user types ":theme" and then exits before previewing
    }

    pub fn set_theme_preview(&mut self, theme: Theme) {
        self.set_theme_impl(theme, ThemeAction::Preview);
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.set_theme_impl(theme, ThemeAction::Set);
    }

    /// Set the assistant panel theme based on the connected agent.
    /// Only affects the assistant panel, not the rest of Helix.
    pub fn apply_assistant_agent_theme(&mut self, agent_index: Option<usize>) {
        let theme_name = agent_index.and_then(|i| {
            self.config
                .load()
                .agents
                .get(i)
                .and_then(|a| a.theme.as_ref())
                .cloned()
        });

        self.assistant_panel_theme = theme_name.and_then(|name| self.theme_loader.load(&name).ok());
    }

    /// Theme to use when rendering the assistant panel.
    pub fn assistant_theme(&self) -> &Theme {
        self.assistant_panel_theme.as_ref().unwrap_or(&self.theme)
    }

    fn set_theme_impl(&mut self, theme: Theme, preview: ThemeAction) {
        // `ui.selection` is the only scope required to be able to render a theme.
        if theme.find_highlight_exact("ui.selection").is_none() {
            self.set_error("Invalid theme: `ui.selection` required");
            return;
        }

        let scopes = theme.scopes();
        (*self.syn_loader).load().set_scopes(scopes.to_vec());

        match preview {
            ThemeAction::Preview => {
                let last_theme = std::mem::replace(&mut self.theme, theme);
                // only insert on first preview: this will be the last theme the user has saved
                self.last_theme.get_or_insert(last_theme);
            }
            ThemeAction::Set => {
                self.last_theme = None;
                self.theme = theme;
            }
        }

        self._refresh();
    }

    #[inline]
    pub fn language_server_by_id(
        &self,
        language_server_id: LanguageServerId,
    ) -> Option<&helix_lsp::Client> {
        self.language_servers
            .get_by_id(language_server_id)
            .map(|client| &**client)
    }

    /// Refreshes the language server for a given document
    pub fn refresh_language_servers(&mut self, doc_id: DocumentId) {
        self.launch_language_servers(doc_id)
    }

    /// moves/renames a path, invoking any event handlers (currently only lsp)
    /// and calling `set_doc_path` if the file is open in the editor
    pub fn move_path(&mut self, old_path: &Path, new_path: &Path) -> io::Result<()> {
        let new_path = canonicalize(new_path);
        // sanity check
        if old_path == new_path {
            return Ok(());
        }
        let is_dir = old_path.is_dir();
        let language_servers: Vec<_> = self
            .language_servers
            .iter_clients()
            .filter(|client| client.is_initialized())
            .cloned()
            .collect();
        for language_server in language_servers {
            let Some(request) = language_server.will_rename(old_path, &new_path, is_dir) else {
                continue;
            };
            let edit = match helix_lsp::block_on(request) {
                Ok(edit) => edit.unwrap_or_default(),
                Err(err) => {
                    log::error!("invalid willRename response: {err:?}");
                    continue;
                }
            };
            if let Err(err) = self.apply_workspace_edit(language_server.offset_encoding(), &edit) {
                log::error!("failed to apply workspace edit: {err:?}")
            }
        }

        if old_path.exists() {
            fs::rename(old_path, &new_path)?;
        }

        if let Some(doc) = self.document_by_path(old_path) {
            self.set_doc_path(doc.id(), &new_path);
        }
        let is_dir = new_path.is_dir();
        for ls in self.language_servers.iter_clients() {
            // A new language server might have been started in `set_doc_path` and won't
            // be initialized yet. Skip the `did_rename` notification for this server.
            if !ls.is_initialized() {
                continue;
            }
            ls.did_rename(old_path, &new_path, is_dir);
        }
        self.language_servers
            .file_event_handler
            .file_changed(old_path.to_owned());
        self.language_servers
            .file_event_handler
            .file_changed(new_path);
        Ok(())
    }

    pub fn set_doc_path(&mut self, doc_id: DocumentId, path: &Path) {
        let doc = doc_mut!(self, &doc_id);
        let old_path = doc.path();

        if let Some(old_path) = old_path {
            // sanity check, should not occur but some callers (like an LSP) may
            // create bogus calls
            if old_path == path {
                return;
            }
            // if we are open in LSPs send did_close notification
            for language_server in doc.language_servers() {
                language_server.text_document_did_close(doc.identifier());
            }
        }
        // we need to clear the list of language servers here so that
        // refresh_doc_language/refresh_language_servers doesn't resend
        // text_document_did_close. Since we called `text_document_did_close`
        // we have fully unregistered this document from its LS
        doc.clear_language_servers();
        doc.set_path(Some(path));
        doc.detect_editor_config();
        self.refresh_doc_language(doc_id)
    }

    pub fn refresh_doc_language(&mut self, doc_id: DocumentId) {
        let loader = self.syn_loader.load();
        let doc = doc_mut!(self, &doc_id);
        doc.detect_language(&loader);
        doc.detect_editor_config();
        doc.detect_indent_and_line_ending();
        self.refresh_language_servers(doc_id);
        let doc = doc_mut!(self, &doc_id);
        let diagnostics = Editor::doc_diagnostics(&self.language_servers, &self.diagnostics, doc);
        doc.replace_diagnostics(diagnostics, &[], None);
        doc.reset_all_inlay_hints();
    }

    /// Launch a language server for a given document
    fn launch_language_servers(&mut self, doc_id: DocumentId) {
        if !self.config().lsp.enable {
            return;
        }
        // if doc doesn't have a URL it's a scratch buffer, ignore it
        let Some(doc) = self.documents.get_mut(&doc_id) else {
            return;
        };
        let Some(doc_url) = doc.url() else {
            return;
        };
        let (lang, path) = (doc.language_configuration().cloned(), doc.path().cloned());
        let config = doc.config.load();
        let root_dirs = &config.workspace_lsp_roots;

        // store only successfully started language servers
        let language_servers = lang.as_ref().map_or_else(HashMap::default, |language| {
            self.language_servers
                .get(language, path.as_ref(), root_dirs, config.lsp.snippets)
                .filter_map(|(lang, client)| match client {
                    Ok(client) => Some((lang, client)),
                    Err(err) => {
                        if let helix_lsp::Error::ExecutableNotFound(err) = err {
                            // Silence by default since some language servers might just not be installed
                            log::debug!(
                                "Language server not found for `{}` {} {}", language.scope, lang, err,
                            );
                        } else {
                            log::error!(
                                "Failed to initialize the language servers for `{}` - `{}` {{ {} }}",
                                language.scope,
                                lang,
                                err
                            );
                        }
                        None
                    }
                })
                .collect::<HashMap<_, _>>()
        });

        if language_servers.is_empty() && !doc.has_language_servers() {
            return;
        }

        let language_id = doc.language_id().map(ToOwned::to_owned).unwrap_or_default();

        // only spawn new language servers if the servers aren't the same
        let doc_language_servers_not_in_registry = doc.all_language_servers().filter(|doc_ls| {
            language_servers
                .get(doc_ls.name())
                .is_none_or(|language_server| language_server.id() != doc_ls.id())
        });

        for language_server in doc_language_servers_not_in_registry {
            language_server.text_document_did_close(doc.identifier());
        }

        let language_servers_not_in_doc =
            language_servers.iter().filter(|(name, language_server)| {
                doc.language_server_by_name(name)
                    .is_none_or(|doc_ls| language_server.id() != doc_ls.id())
            });

        for (_, language_server) in language_servers_not_in_doc {
            // TODO: this now races with on_init code if the init happens too quickly
            language_server.text_document_did_open(
                doc_url.clone(),
                doc.version(),
                doc.text(),
                language_id.clone(),
            );
        }

        doc.set_language_servers(language_servers);
    }

    fn _refresh(&mut self) {
        let config = self.config();

        // Reset the inlay hints annotations *before* updating the views, that way we ensure they
        // will disappear during the `.sync_change(doc)` call below.
        //
        // We can't simply check this config when rendering because inlay hints are only parts of
        // the possible annotations, and others could still be active, so we need to selectively
        // drop the inlay hints.
        if !config.lsp.display_inlay_hints {
            for doc in self.documents_mut() {
                doc.reset_all_inlay_hints();
            }
        }

        for (view, _) in self.tree.views_mut() {
            let doc = doc_mut!(self, &view.doc);
            view.sync_changes(doc);
            view.gutters = config.gutters.clone();
            view.ensure_cursor_in_view(doc, config.scrolloff)
        }
    }

    fn replace_document_in_view(&mut self, current_view: ViewId, doc_id: DocumentId) {
        let scrolloff = self.config().scrolloff;
        let view = self.tree.get_mut(current_view);

        view.doc = doc_id;
        let doc = doc_mut!(self, &doc_id);

        doc.ensure_view_init(view.id);
        view.sync_changes(doc);
        doc.mark_as_focused();

        view.ensure_cursor_in_view(doc, scrolloff)
    }

    pub fn switch(&mut self, id: DocumentId, action: Action) {
        use crate::tree::Layout;

        if !self.documents.contains_key(&id) {
            log::error!("cannot switch to document that does not exist (anymore)");
            return;
        }

        if !matches!(action, Action::Load) {
            self.enter_normal_mode();
        }

        let focust_lost = match action {
            Action::Replace => {
                let (view_id, doc) = focused_ref!(self);
                // If the current view is an empty scratch buffer and is not displayed in any other views, delete it.
                // Boolean value is determined before the call to `view_mut` because the operation requires a borrow
                // of `self.tree`, which is mutably borrowed when `view_mut` is called.
                let remove_empty_scratch = !doc.is_modified()
                    // If the buffer has no path and is not modified, it is an empty scratch buffer.
                    && doc.path().is_none()
                    && !doc.is_persistent_scratch()
                    // If the buffer we are changing to is not this buffer
                    && id != doc.id
                    // Ensure the buffer is not displayed in any other splits.
                    && !self
                        .tree
                        .traverse()
                        .any(|(_, v)| v.doc == doc.id && v.id != view_id);

                if doc.path().is_none() || doc.is_persistent_scratch() {
                    log::warn!(
                        "[acp_scratch] switch action={:?} from_doc={:?} to_doc={:?} modified={} persistent={} remove_empty_scratch={} view_id={:?}",
                        action,
                        doc.id,
                        id,
                        doc.is_modified(),
                        doc.is_persistent_scratch(),
                        remove_empty_scratch,
                        view_id
                    );
                }

                let (view_id, doc) = focused!(self);

                // Append any outstanding changes to history in the old document.
                let view = self.tree.get_mut(view_id);
                doc.append_changes_to_history(view);

                if remove_empty_scratch {
                    log::warn!(
                        "[acp_scratch] removing empty scratch doc={:?} while switching to {:?}",
                        doc.id,
                        id
                    );
                    // Copy `doc.id` into a variable before calling `self.documents.remove`, which requires a mutable
                    // borrow, invalidating direct access to `doc.id`.
                    let id = doc.id;
                    self.documents.remove(&id);

                    // Remove the scratch buffer from any jumplists
                    for (view, _) in self.tree.views_mut() {
                        view.remove_document(&id);
                    }
                } else {
                    let view = self.tree.get_mut(view_id);
                    let jump = (view.doc, doc.selection(view_id).clone());
                    view.history.jumps.push(jump);
                    // Set last accessed doc if it is a different document
                    if doc.id != id {
                        view.add_to_history(view.doc);
                        // Set last modified doc if modified and last modified doc is different
                        if doc.take_modified_since_accessed()
                            && view.last_modified_docs[0] != Some(view.doc)
                        {
                            view.last_modified_docs = [Some(view.doc), view.last_modified_docs[0]];
                        }
                    }
                }

                self.replace_document_in_view(view_id, id);

                dispatch(DocumentFocusLost {
                    editor: self,
                    doc: id,
                });
                return;
            }
            Action::Load => {
                let view_id = view!(self).id;
                let doc = doc_mut!(self, &id);
                doc.ensure_view_init(view_id);
                doc.mark_as_focused();
                return;
            }
            Action::HorizontalSplit | Action::VerticalSplit => {
                let focus_lost = self.tree.try_get(self.tree.focus).map(|view| view.doc);
                // copy the current view, unless there is no view yet
                let view = self
                    .tree
                    .try_get(self.tree.focus)
                    .filter(|v| id == v.doc) // Different Document
                    .cloned()
                    .unwrap_or_else(|| View::new(id, self.config().gutters.clone()));
                let mut view = view;
                self.bind_view_redraw(&mut view);
                let view_id = self.tree.split(
                    view,
                    match action {
                        Action::HorizontalSplit => Layout::Horizontal,
                        Action::VerticalSplit => Layout::Vertical,
                        _ => unreachable!(),
                    },
                );
                // initialize selection for view
                let doc = doc_mut!(self, &id);
                doc.ensure_view_init(view_id);
                doc.mark_as_focused();
                focus_lost
            }
        };

        self._refresh();
        if let Some(focus_lost) = focust_lost {
            dispatch(DocumentFocusLost {
                editor: self,
                doc: focus_lost,
            });
        }
    }

    /// Generate an id for a new document and register it.
    fn new_document(&mut self, mut doc: Document) -> DocumentId {
        let id = self.next_document_id;
        // Safety: adding 1 from 1 is fine, practically impossible to reach usize max
        // Safety: adding 1 to a NonZeroUsize that started at 1 won't reach 0.
        self.next_document_id = DocumentId::new(unsafe {
            NonZeroUsize::new_unchecked(self.next_document_id.value().get() + 1)
        });
        doc.bind_redraw(self.frame_gate.handle());
        doc.id = id;
        self.documents.insert(id, doc);

        let save_sender = self.save_tx.clone();
        self.saves.insert(id, save_sender);

        id
    }

    /// Create a component-owned document (not shown in bufferline/pickers).
    /// The document lives in `component_docs` and is accessed explicitly by ID.
    /// The caller is responsible for cleanup.
    pub fn new_component_doc(&mut self, mut doc: Document) -> DocumentId {
        let id = self.next_document_id;
        self.next_document_id = DocumentId::new(unsafe {
            std::num::NonZeroUsize::new_unchecked(self.next_document_id.value().get() + 1)
        });
        doc.bind_redraw(self.frame_gate.handle());
        doc.id = id;
        self.component_docs.insert(id, doc);
        id
    }

    /// Allocate a unique `ViewId` for a component-owned viewport.
    /// Uses a high version to avoid collisions with tree-allocated ViewIds.
    pub fn allocate_view_id(&mut self) -> ViewId {
        let idx = self.next_virtual_view_idx;
        self.next_virtual_view_idx += 1;
        // Tree-allocated keys use low versions (starting at 1).
        // We use version = u32::MAX so the key spaces never overlap.
        let raw = ((u32::MAX as u64) << 32) | (idx as u64);
        ViewId::from(slotmap::KeyData::from_ffi(raw))
    }

    pub fn component_view(&self, id: ViewId) -> Option<&ComponentViewState> {
        self.component_views.get(&id)
    }

    pub fn component_view_mut(&mut self, id: ViewId) -> Option<&mut ComponentViewState> {
        self.component_views.get_mut(&id)
    }

    pub fn ensure_component_view(
        &mut self,
        id: ViewId,
        doc: DocumentId,
    ) -> &mut ComponentViewState {
        self.track_component_surface(id, doc);
        self.component_views
            .entry(id)
            .and_modify(|view| view.doc = doc)
            .or_insert_with(|| ComponentViewState::new(id, doc))
    }

    pub fn with_view_doc_mut<R>(
        &mut self,
        view_id: ViewId,
        doc_id: DocumentId,
        f: impl FnOnce(&mut AnyViewMut<'_>, &mut Document) -> R,
    ) -> R {
        let Self {
            tree,
            documents,
            component_docs,
            component_views,
            ..
        } = self;

        let is_tree = tree.contains(view_id);
        let doc = documents
            .get_mut(&doc_id)
            .or_else(|| component_docs.get_mut(&doc_id))
            .expect("document not found in documents or component_docs");
        let mut view = if is_tree {
            AnyViewMut::Tree(tree.get_mut(view_id))
        } else {
            AnyViewMut::Component(
                component_views
                    .get_mut(&view_id)
                    .expect("component view not found"),
            )
        };
        f(&mut view, doc)
    }

    pub fn with_view<R>(&self, view_id: ViewId, f: impl FnOnce(AnyViewRef<'_>) -> R) -> R {
        f(AnyViewRef::from_editor(self, view_id))
    }

    pub fn with_view_doc<R>(
        &self,
        view_id: ViewId,
        doc_id: DocumentId,
        f: impl FnOnce(AnyViewRef<'_>, &Document) -> R,
    ) -> R {
        let doc = self
            .document(doc_id)
            .expect("document not found in documents or component_docs");
        self.with_view(view_id, |view| f(view, doc))
    }

    pub fn with_view_mut<R>(
        &mut self,
        view_id: ViewId,
        f: impl FnOnce(&mut AnyViewMut<'_>) -> R,
    ) -> R {
        let mut view = AnyViewMut::from_editor(self, view_id);
        f(&mut view)
    }

    pub fn with_surface<R>(
        &self,
        id: crate::collab::SurfaceId,
        f: impl FnOnce(crate::collab::surface::Ref<'_>) -> R,
    ) -> Result<R, crate::collab::surface::Missing> {
        let surface = self.surface_registry.require(id)?;
        let value = self.with_view_doc(surface.view, surface.doc, |view, doc| {
            f(view.as_surface_ref(doc))
        });
        Ok(value)
    }

    pub fn capture_current_surface(
        &self,
        capture: crate::collab::surface::Capture,
    ) -> Option<crate::assistant::context::Kind> {
        let view = self.tree.get(self.tree.focus);
        let doc = self.document(view.doc)?;
        crate::collab::surface::Context::capture(
            &crate::collab::surface::Ref::Tree { view, doc },
            self,
            capture,
        )
    }

    pub fn pause_current_surface(&self, event: &EditorEvent) -> Option<crate::collab::FollowPause> {
        let view = self.tree.get(self.tree.focus);
        let doc = self.document(view.doc)?;
        crate::collab::surface::PauseFollow::pause(
            &crate::collab::surface::Ref::Tree { view, doc },
            event,
        )
    }

    pub fn with_surface_mut<R>(
        &mut self,
        id: crate::collab::SurfaceId,
        f: impl FnOnce(crate::collab::surface::Mut<'_>) -> R,
    ) -> Result<R, crate::collab::surface::Missing> {
        let surface = self.surface_registry.require(id)?;
        let doc_id = surface.doc;
        let view_id = surface.view;
        let value = self.with_view_doc_mut(view_id, doc_id, |view, doc| f(view.as_surface_mut(doc)));
        Ok(value)
    }

    pub fn new_file_from_document(&mut self, action: Action, doc: Document) -> DocumentId {
        let id = self.new_document(doc);
        self.switch(id, action);
        id
    }

    pub fn open_markdown_scratch(&mut self, action: Action, text: String) -> DocumentId {
        let mut doc = Document::from(text.into(), None, self.config.clone(), self.syn_loader.clone())
            .with_persistent_scratch();
        let _ = doc.set_language_by_language_id("markdown", &self.syn_loader.load());
        self.new_file_from_document(action, doc)
    }

    pub fn switch_document_if_exists(&mut self, id: DocumentId, action: Action) -> bool {
        if self.document(id).is_none() {
            return false;
        }
        self.switch(id, action);
        true
    }

    pub fn new_file(&mut self, action: Action) -> DocumentId {
        self.new_file_from_document(
            action,
            Document::default(self.config.clone(), self.syn_loader.clone()),
        )
    }

    /// Use when Helix is opened with no arguments passed
    pub fn new_file_welcome(&mut self) -> DocumentId {
        self.new_file_from_document(
            Action::VerticalSplit,
            Document::default(self.config.clone(), self.syn_loader.clone()).with_welcome(),
        )
    }

    pub fn new_file_from_stdin(&mut self, action: Action) -> Result<DocumentId, Error> {
        let (stdin, encoding, has_bom) = crate::document::read_to_string(&mut stdin(), None)?;
        let doc = Document::from(
            helix_core::Rope::default(),
            Some((encoding, has_bom)),
            self.config.clone(),
            self.syn_loader.clone(),
        );
        let doc_id = self.new_file_from_document(action, doc);
        let doc = doc_mut!(self, &doc_id);
        let view = view_mut!(self);
        doc.ensure_view_init(view.id);
        let transaction =
            helix_core::Transaction::insert(doc.text(), doc.selection(view.id), stdin.into())
                .with_selection(Selection::point(0));
        doc.apply(&transaction, view.id);
        doc.append_changes_to_history(view);
        Ok(doc_id)
    }

    pub fn document_id_by_path(&self, path: &Path) -> Option<DocumentId> {
        self.document_by_path(path).map(|doc| doc.id)
    }

    // ??? possible use for integration tests
    pub fn open(&mut self, path: &Path, action: Action) -> Result<DocumentId, DocumentOpenError> {
        let path = helix_stdx::path::canonicalize(path);
        let id = self.document_id_by_path(&path);

        let id = if let Some(id) = id {
            id
        } else {
            let mut doc = Document::open(
                &path,
                None,
                true,
                self.config.clone(),
                self.syn_loader.clone(),
            )?;

            let diagnostics =
                Editor::doc_diagnostics(&self.language_servers, &self.diagnostics, &doc);
            doc.replace_diagnostics(diagnostics, &[], None);

            if let Some(diff_base) = self.diff_providers.get_diff_base(&path) {
                doc.set_diff_base(diff_base);
            }
            doc.set_version_control_head(self.diff_providers.get_current_head_name(&path));

            let id = self.new_document(doc);

            self.launch_language_servers(id);

            helix_event::dispatch(DocumentDidOpen {
                editor: self,
                doc: id,
                path: &path,
            });

            id
        };

        self.switch(id, action);

        Ok(id)
    }

    pub fn close(&mut self, id: ViewId) {
        // Remove selections for the closed view on all documents.
        for doc in self.documents_mut() {
            doc.remove_view(id);
        }
        self.tree.remove(id);
        self._refresh();
    }

    pub fn close_document(&mut self, doc_id: DocumentId, force: bool) -> Result<(), CloseError> {
        let doc = match self.documents.get(&doc_id) {
            Some(doc) => doc,
            None => return Err(CloseError::DoesNotExist),
        };
        if !force && doc.is_modified() {
            return Err(CloseError::BufferModified(doc.display_name().into_owned()));
        }

        // This will also disallow any follow-up writes
        self.saves.remove(&doc_id);

        enum Action {
            Close(ViewId),
            ReplaceDoc(ViewId, DocumentId),
        }

        let actions: Vec<Action> = self
            .tree
            .views_mut()
            .filter_map(|(view, _focus)| {
                view.remove_document(&doc_id);

                if view.doc == doc_id {
                    // something was previously open in the view, switch to previous doc
                    if let Some(prev_doc) = view.docs_access_history.pop() {
                        Some(Action::ReplaceDoc(view.id, prev_doc))
                    } else {
                        // only the document that is being closed was in the view, close it
                        Some(Action::Close(view.id))
                    }
                } else {
                    None
                }
            })
            .collect();

        for action in actions {
            match action {
                Action::Close(view_id) => {
                    self.close(view_id);
                }
                Action::ReplaceDoc(view_id, doc_id) => {
                    self.replace_document_in_view(view_id, doc_id);
                }
            }
        }

        let doc = self.documents.remove(&doc_id).unwrap();

        // If the document we removed was visible in all views, we will have no more views. We don't
        // want to close the editor just for a simple buffer close, so we need to create a new view
        // containing either an existing document, or a brand new document.
        if self.tree.views().next().is_none() {
            let doc_id = self
                .documents
                .iter()
                .map(|(&doc_id, _)| doc_id)
                .next()
                .unwrap_or_else(|| {
                    self.new_document(Document::default(
                        self.config.clone(),
                        self.syn_loader.clone(),
                    ))
                });
            let mut view = View::new(doc_id, self.config().gutters.clone());
            self.bind_view_redraw(&mut view);
            let view_id = self.tree.insert(view);
            let _ = self.track_tree_surface(view_id);
            let doc = doc_mut!(self, &doc_id);
            doc.ensure_view_init(view_id);
            doc.mark_as_focused();
        }

        self._refresh();

        helix_event::dispatch(DocumentDidClose { editor: self, doc });

        Ok(())
    }

    pub fn save<P: Into<PathBuf>>(
        &mut self,
        doc_id: DocumentId,
        path: Option<P>,
        force: bool,
    ) -> anyhow::Result<()> {
        // convert a channel of futures to pipe into main queue one by one
        // via stream.then() ? then push into main future

        let path = path.map(|path| path.into());
        let doc = doc_mut!(self, &doc_id);
        let doc_save_future = doc.save(path, force)?;

        // When a file is written to, notify the file event handler.
        // Note: This can be removed once proper file watching is implemented.
        let handler = self.language_servers.file_event_handler.clone();
        let future = async move {
            let res = doc_save_future.await;
            if let Ok(event) = &res {
                handler.file_changed(event.path.clone());
            }
            res
        };

        helix_runtime::send_blocking(
            self.saves
                .get(&doc_id)
                .ok_or_else(|| anyhow::format_err!("saves are closed for this document!"))?,
            Box::pin(future),
        );

        self.write_count += 1;

        Ok(())
    }

    pub fn resize(&mut self, area: Rect) {
        if self.tree.resize(area) {
            self._refresh();
        };
    }

    pub fn focus(&mut self, view_id: ViewId) {
        if self.tree.focus == view_id {
            return;
        }

        // Reset mode to normal and ensure any pending changes are committed in the old document.
        self.enter_normal_mode();
        let (cur_view_id, doc) = focused!(self);
        let view = self.tree.get_mut(cur_view_id);
        doc.append_changes_to_history(view);
        self.ensure_cursor_in_view(view_id);
        // Update jumplist selections with new document changes.
        for (view, _focused) in self.tree.views_mut() {
            let doc = doc_mut!(self, &view.doc);
            view.sync_changes(doc);
        }

        let prev_id = std::mem::replace(&mut self.tree.focus, view_id);
        focused!(self).1.mark_as_focused();

        let focus_lost = self.tree.get(prev_id).doc;
        dispatch(DocumentFocusLost {
            editor: self,
            doc: focus_lost,
        });
    }

    pub fn focus_next(&mut self) {
        self.focus(self.tree.next());
    }

    pub fn focus_prev(&mut self) {
        self.focus(self.tree.prev());
    }

    pub fn focus_direction(&mut self, direction: tree::Direction) {
        let current_view = self.tree.focus;
        if let Some(id) = self.tree.find_split_in_direction(current_view, direction) {
            self.focus(id)
        }
    }

    pub fn swap_split_in_direction(&mut self, direction: tree::Direction) {
        self.tree.swap_split_in_direction(direction);
    }

    pub fn transpose_view(&mut self) {
        self.tree.transpose();
    }

    pub fn resize_buffer(&mut self, resize_type: Resize, dimension: Dimension) {
        self.tree
            .resize_buffer(resize_type, dimension, &self.config());
    }

    pub fn toggle_focus_window(&mut self) {
        self.tree.toggle_focus_window();
    }

    pub fn should_close(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn ensure_cursor_in_view(&mut self, id: ViewId) {
        let config = self.config();
        let view = self.tree.get(id);
        let doc = doc_mut!(self, &view.doc);
        view.ensure_cursor_in_view(doc, config.scrolloff)
    }

    #[inline]
    pub fn document(&self, id: DocumentId) -> Option<&Document> {
        self.documents
            .get(&id)
            .or_else(|| self.component_docs.get(&id))
    }

    pub fn focused_document_id(&self) -> DocumentId {
        self.tree.get(self.tree.focus).doc
    }

    pub fn focused_document(&self) -> Option<&Document> {
        self.document(self.focused_document_id())
    }

    #[inline]
    pub fn document_mut(&mut self, id: DocumentId) -> Option<&mut Document> {
        self.documents
            .get_mut(&id)
            .or_else(|| self.component_docs.get_mut(&id))
    }

    #[inline]
    pub fn documents(&self) -> impl Iterator<Item = &Document> {
        self.documents.values()
    }

    #[inline]
    pub fn documents_mut(&mut self) -> impl Iterator<Item = &mut Document> {
        self.documents.values_mut()
    }

    pub fn has_stale_syntax(&self) -> bool {
        self.documents
            .values()
            .any(|doc| doc.syntax_snapshot().is_stale())
            || self
                .component_docs
                .values()
                .any(|doc| doc.syntax_snapshot().is_stale())
    }

    pub fn refresh_one_stale_syntax(&mut self) -> bool {
        let focused_doc_id = self.tree.get(self.tree.focus).doc;
        let loader = self.syn_loader.load();

        if let Some(doc) = self.documents.get_mut(&focused_doc_id) {
            if doc.syntax_snapshot().is_stale() {
                let refreshed = doc.refresh_stale_syntax(&loader);
                log_run_event("syntax_refresh_attempt", || {
                    format!(
                        "target=focused doc_id={} refreshed={}",
                        focused_doc_id, refreshed
                    )
                });
                if refreshed {
                    self.needs_redraw = true;
                }
                return refreshed;
            }
        }

        for (doc_id, doc) in &mut self.documents {
            if *doc_id == focused_doc_id || !doc.syntax_snapshot().is_stale() {
                continue;
            }
            let refreshed = doc.refresh_stale_syntax(&loader);
            log_run_event("syntax_refresh_attempt", || {
                format!(
                    "target=background doc_id={} refreshed={}",
                    doc_id, refreshed
                )
            });
            if refreshed {
                self.needs_redraw = true;
            }
            return refreshed;
        }

        for doc in self.component_docs.values_mut() {
            if !doc.syntax_snapshot().is_stale() {
                continue;
            }
            let refreshed = doc.refresh_stale_syntax(&loader);
            log_run_event("syntax_refresh_attempt", || {
                format!("target=component refreshed={}", refreshed)
            });
            if refreshed {
                self.needs_redraw = true;
            }
            return refreshed;
        }

        false
    }

    pub fn document_by_path<P: AsRef<Path>>(&self, path: P) -> Option<&Document> {
        self.documents()
            .find(|doc| doc.path().map(|p| p == path.as_ref()).unwrap_or(false))
    }

    pub fn document_by_path_mut<P: AsRef<Path>>(&mut self, path: P) -> Option<&mut Document> {
        self.documents_mut()
            .find(|doc| doc.path().map(|p| p == path.as_ref()).unwrap_or(false))
    }

    /// Returns all supported diagnostics for the document
    pub fn doc_diagnostics<'a>(
        language_servers: &'a helix_lsp::Registry,
        diagnostics: &'a Diagnostics,
        document: &Document,
    ) -> impl Iterator<Item = helix_core::Diagnostic> + 'a {
        Editor::doc_diagnostics_with_filter(language_servers, diagnostics, document, |_, _| true)
    }

    /// Returns all supported diagnostics for the document
    /// filtered by `filter` which is invocated with the raw `lsp::Diagnostic` and the language server id it came from
    pub fn doc_diagnostics_with_filter<'a>(
        language_servers: &'a helix_lsp::Registry,
        diagnostics: &'a Diagnostics,
        document: &Document,
        filter: impl Fn(&lsp::Diagnostic, &DiagnosticProvider) -> bool + 'a,
    ) -> impl Iterator<Item = helix_core::Diagnostic> + 'a {
        let text = document.text().clone();
        let language_config = document.language_configuration().cloned();
        document
            .uri()
            .and_then(|uri| diagnostics.get(&uri))
            .map(|diags| {
                diags.iter().filter_map(move |(diagnostic, provider)| {
                    let server_id = provider.language_server_id()?;
                    let ls = language_servers.get_by_id(server_id)?;
                    language_config
                        .as_ref()
                        .and_then(|c| {
                            c.language_servers.iter().find(|features| {
                                features.name == ls.name()
                                    && features.has_feature(LanguageServerFeature::Diagnostics)
                            })
                        })
                        .and_then(|_| {
                            if filter(diagnostic, provider) {
                                Document::lsp_diagnostic_to_diagnostic(
                                    &text,
                                    language_config.as_deref(),
                                    diagnostic,
                                    provider.clone(),
                                    ls.offset_encoding(),
                                )
                            } else {
                                None
                            }
                        })
                })
            })
            .into_iter()
            .flatten()
    }

    /// Gets the primary cursor position in screen coordinates,
    /// or `None` if the primary cursor is not visible on screen.
    pub fn cursor(&self) -> (Option<Position>, CursorKind) {
        let config = self.config();
        let (view_id, doc) = focused_ref!(self);
        let view = self.tree.get(view_id);
        if let Some(mut pos) = self.cursor_cache.get(view, doc) {
            let inner = view.inner_area(doc);
            pos.col += inner.x as usize;
            pos.row += inner.y as usize;
            let cursorkind = config.cursor_shape.from_mode(self.mode);
            (Some(pos), cursorkind)
        } else {
            (None, CursorKind::default())
        }
    }

    /// Closes language servers with timeout. The default timeout is 10000 ms, use
    /// `timeout` parameter to override this.
    pub async fn close_language_servers(
        &self,
        timeout: Option<u64>,
    ) -> Result<(), tokio::time::error::Elapsed> {
        // Remove all language servers from the file event handler.
        // Note: this is non-blocking.
        for client in self.language_servers.iter_clients() {
            self.language_servers
                .file_event_handler
                .remove_client(client.id());
        }

        tokio::time::timeout(
            Duration::from_millis(timeout.unwrap_or(3000)),
            future::join_all(
                self.language_servers
                    .iter_clients()
                    .map(|client| client.force_shutdown()),
            ),
        )
        .await
        .map(|_| ())
    }

    pub async fn flush_writes(&mut self) -> anyhow::Result<()> {
        while self.write_count > 0 {
            if let Some(save_event) = self.save_queue.recv().await {
                self.write_count -= 1;

                let save_event = match save_event.await {
                    Ok(event) => event,
                    Err(err) => {
                        self.set_error(err.to_string());
                        bail!(err);
                    }
                };

                let doc = doc_mut!(self, &save_event.doc_id);
                doc.set_last_saved_revision(save_event.revision, save_event.save_time);
            }
        }

        Ok(())
    }

    /// Switches the editor into normal mode.
    pub fn enter_normal_mode(&mut self) {
        use helix_core::graphemes;

        if self.mode == Mode::Normal {
            return;
        }

        self.mode = Mode::Normal;
        let (view_id, doc) = focused!(self);
        let view = self.tree.get_mut(view_id);

        try_restore_indent(doc, view);

        // if leaving append mode, move cursor back by 1
        if doc.restore_cursor() {
            let text = doc.text().slice(..);
            let selection = doc.selection(view_id).clone().transform(|range| {
                let mut head = range.to();
                if range.head > range.anchor {
                    head = graphemes::prev_grapheme_boundary(text, head);
                }

                Range::new(range.from(), head)
            });

            doc.set_selection(view_id, selection);
            doc.clear_restore_cursor();
        }
    }

    pub fn current_stack_frame(&self) -> Option<&dap::StackFrame> {
        self.debug_adapters.current_stack_frame()
    }

    /// Returns the id of a view that this doc contains a selection for,
    /// making sure it is synced with the current changes
    /// if possible or there are no selections returns current_view
    /// otherwise uses an arbitrary view
    pub fn get_synced_view_id(&mut self, id: DocumentId) -> ViewId {
        let current_view = view_mut!(self);
        let doc = self.documents.get_mut(&id).unwrap();
        if doc.selections().contains_key(&current_view.id) {
            // only need to sync current view if this is not the current doc
            if current_view.doc != id {
                current_view.sync_changes(doc);
            }
            current_view.id
        } else if let Some(view_id) = doc.selections().keys().next() {
            let view_id = *view_id;
            let view = self.tree.get_mut(view_id);
            view.sync_changes(doc);
            view_id
        } else {
            doc.ensure_view_init(current_view.id);
            current_view.id
        }
    }

    pub fn set_cwd(&mut self, path: &Path) -> std::io::Result<()> {
        self.last_cwd = helix_stdx::env::set_current_working_dir(path)?;
        self.clear_doc_relative_paths();
        Ok(())
    }

    pub fn get_last_cwd(&mut self) -> Option<&Path> {
        self.last_cwd.as_deref()
    }

    pub fn jump_forward(&mut self, view_id: ViewId, count: usize) {
        let jump = self.with_view_mut(view_id, |view| view.jumps_mut().forward(count).cloned());
        if let Some((doc_id, selection)) = jump {
            self.jump_to(view_id, doc_id, selection);
        }
    }

    pub fn jump_backward(&mut self, view_id: ViewId, count: usize) {
        let current_doc_id = self.with_view_mut(view_id, |view| view.doc_id());
        let jump = self.with_view_doc_mut(view_id, current_doc_id, |view, doc| {
            view.jumps_mut().backward(view_id, doc, count).cloned()
        });
        if let Some((doc_id, selection)) = jump {
            self.jump_to(view_id, doc_id, selection);
        }
    }

    fn jump_to(&mut self, view_id: ViewId, dest_doc_id: DocumentId, mut selection: Selection) {
        if self.with_view(view_id, |view| view.is_tree()) {
            let view = view_mut!(self, view_id);
            let old_doc_id = view.doc;
            if old_doc_id != dest_doc_id {
                if let Some(transaction) = self.with_view_doc_mut(view_id, dest_doc_id, |view, doc| {
                    view.changes_to_sync(doc)
                }) {
                    let new_doc = doc_mut!(self, &dest_doc_id);
                    let text = new_doc.text().slice(..);
                    selection = selection.map(transaction.changes()).ensure_invariants(text);
                }
                self.replace_document_in_view(view_id, dest_doc_id);
                dispatch(DocumentFocusLost {
                    editor: self,
                    doc: old_doc_id,
                });
            }
            let (cur_view_id, doc) = focused!(self);
            doc.set_selection(cur_view_id, selection);
            let view = self.tree.get(cur_view_id);
            view.ensure_cursor_in_view_center(doc, self.config.load().scrolloff);
            return;
        }

        let scrolloff = self.config.load().scrolloff;
        self.with_view_doc_mut(view_id, dest_doc_id, |view, doc| {
            if view.doc_id() != dest_doc_id {
                return;
            }
            doc.set_selection(view_id, selection);
            ensure_cursor_in_view_center_in(view, doc, scrolloff);
        });
    }
}

impl crate::traits::Modal for Editor {
    fn mode(&self) -> Mode {
        Editor::mode(self)
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }
}

fn try_restore_indent(doc: &mut Document, view: &mut View) {
    use helix_core::{
        chars::char_is_whitespace,
        line_ending::{line_end_char_index, str_is_line_ending},
        unicode::segmentation::UnicodeSegmentation,
        Operation, Transaction,
    };

    fn inserted_a_new_blank_line(changes: &[Operation], pos: usize, line_end_pos: usize) -> bool {
        if let [Operation::Retain(move_pos), Operation::Insert(ref inserted_str), Operation::Retain(_)] =
            changes
        {
            let mut graphemes = inserted_str.graphemes(true);
            move_pos + inserted_str.len() == pos
                && graphemes.next().is_some_and(str_is_line_ending)
                && graphemes.all(|g| g.chars().all(char_is_whitespace))
                && pos == line_end_pos // ensure no characters exists after current position
        } else {
            false
        }
    }

    let doc_changes = doc.changes().changes();
    let text = doc.text().slice(..);
    let range = doc.selection(view.id).primary();
    let pos = range.cursor(text);
    let line_end_pos = line_end_char_index(&text, range.cursor_line(text));

    if inserted_a_new_blank_line(doc_changes, pos, line_end_pos) {
        // Removes tailing whitespaces for the primary selection only, preserving existing behavior
        let line_start_pos = text.line_to_char(range.cursor_line(text));
        let transaction =
            Transaction::change(doc.text(), [(line_start_pos, pos, None)].into_iter());
        doc.apply(&transaction, view.id);
    }
}

#[cfg(test)]
mod tests {
    use super::Editor;
    use crate::collab::{participant, Participant, ParticipantId};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn participant(id: u64, name: &str) -> Participant {
        Participant {
            id: ParticipantId::new(std::num::NonZeroU64::new(id).unwrap()),
            kind: participant::Kind::Agent,
            name: name.to_string(),
            access: participant::Access::Read,
        }
    }

    fn temp_file(name: &str, contents: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("helix-collab-{name}-{stamp}.rs"));
        fs::write(&path, contents).expect("write temp file");
        helix_stdx::path::canonicalize(path)
    }

    #[test]
    fn collab_effects_publish_presence_for_open_locations() {
        let mut editor = Editor::collab_test_editor();
        let alice = participant(1, "alice");
        let bob = participant(2, "bob");

        let join_alice = editor.join_participant(alice.clone());
        editor.apply_collab_effects(join_alice);
        let join_bob = editor.join_participant(bob.clone());
        editor.apply_collab_effects(join_bob);

        let alice_location = Editor::collab_test_location(&editor, alice.id, 2..5);
        let alice_effects = editor.publish_location(alice.id, alice_location).expect("location");
        editor.apply_collab_effects(alice_effects);

        let bob_location = Editor::collab_test_location(&editor, bob.id, 8..8);
        let bob_effects = editor.publish_location(bob.id, bob_location).expect("location");
        editor.apply_collab_effects(bob_effects);

        let surface = editor.surface_registry.get_by_view(editor.tree.focus).expect("surface");
        let presence = editor.collab.presence(surface).expect("presence");
        assert_eq!(presence.len(), 2);
        assert!(presence.iter().any(|item| item.participant == alice.id && item.selection.is_some()));
        assert!(presence.iter().any(|item| item.participant == bob.id && item.cursor.is_some()));
    }

    #[test]
    fn surface_resolution_prefers_editor_role_over_auxiliary() {
        let mut editor = Editor::collab_test_editor();
        let alice = participant(1, "alice");

        let join_effects = editor.join_participant(alice.clone());
        editor.apply_collab_effects(join_effects);

        let editor_view = editor.tree.focus;
        let editor_surface = editor.surface_registry.get_by_view(editor_view).expect("editor surface");
        let doc_id = editor.tree.get(editor_view).doc;
        let path = editor.collab_test_path();
        let doc = doc_mut!(editor, &doc_id);
        doc.set_path(Some(&path));

        let component_view_id = editor.allocate_view_id();
        editor.ensure_component_view(component_view_id, doc_id);
        let auxiliary_surface = editor
            .surface_registry
            .get_by_view(component_view_id)
            .expect("auxiliary surface");
        assert_ne!(editor_surface, auxiliary_surface);

        let mut location = Editor::collab_test_location(&editor, alice.id, 4..9);
        location.surface = None;

        let effects = editor.publish_location(alice.id, location).expect("location");
        editor.apply_collab_effects(effects);

        let presence = editor.collab.presence(editor_surface).expect("presence");
        assert_eq!(presence.len(), 1);
        assert_eq!(presence[0].surface, editor_surface);
        assert!(editor.collab.presence(auxiliary_surface).is_none_or(|items| items.is_empty()));
    }

    #[test]
    fn leaving_participant_clears_derived_presence() {
        let mut editor = Editor::collab_test_editor();
        let alice = participant(1, "alice");

        let join_effects = editor.join_participant(alice.clone());
        editor.apply_collab_effects(join_effects);

        let location = Editor::collab_test_location(&editor, alice.id, 3..7);
        let location_effects = editor.publish_location(alice.id, location).expect("location");
        editor.apply_collab_effects(location_effects);

        let surface = editor.surface_registry.get_by_view(editor.tree.focus).expect("surface");
        assert!(editor.collab.presence(surface).is_some());

        let leave_effects = editor.leave_participant(alice.id);
        editor.apply_collab_effects(leave_effects);

        let presence = editor.collab.presence(surface).unwrap_or(&[]);
        assert!(presence.is_empty());

        let view = editor.tree.get(editor.tree.focus);
        let doc = editor.document(view.doc).expect("doc");
        let annotations = doc.presence_annotations(view.id).cloned().unwrap_or_default();
        assert!(annotations.is_empty());
    }

    #[test]
    fn collab_open_keeps_current_focus_while_loading_target_document() {
        let mut editor = Editor::collab_test_editor();
        let active_doc = editor.tree.get(editor.tree.focus).doc;
        let alice = participant(1, "alice");

        let join_effects = editor.join_participant(alice.clone());
        editor.apply_collab_effects(join_effects);
        let new_path = temp_file("open-target", "fn open_target() {}\n");

        let location = crate::collab::Location::new(new_path.clone(), crate::collab::location::Source::Tool)
            .with_range(crate::collab::RangeAnchor::new(0, 0));
        editor.apply_collab_effects(vec![crate::collab::Effect::Open {
            participant: alice.id,
            location,
        }]);

        assert_eq!(editor.tree.get(editor.tree.focus).doc, active_doc);
        let opened_doc = editor.open(&new_path, super::Action::Load).expect("open target");
        assert!(editor.document(opened_doc).is_some());
        assert_eq!(editor.tree.get(editor.tree.focus).doc, active_doc);
        let _ = fs::remove_file(new_path);
    }

    #[test]
    fn collab_reveal_switches_focus_to_target_document() {
        let mut editor = Editor::collab_test_editor();
        let active_doc = editor.tree.get(editor.tree.focus).doc;
        let alice = participant(1, "alice");

        let join_effects = editor.join_participant(alice.clone());
        editor.apply_collab_effects(join_effects);
        let new_path = temp_file("reveal-target", "fn reveal_target() {}\n");

        let location = crate::collab::Location::new(new_path.clone(), crate::collab::location::Source::Tool)
            .with_range(crate::collab::RangeAnchor::new(0, 0));
        editor.apply_collab_effects(vec![crate::collab::Effect::Reveal {
            participant: alice.id,
            location,
        }]);

        let new_doc_id = editor.document_id_by_path(&new_path).expect("target doc");
        assert_eq!(editor.tree.get(editor.tree.focus).doc, new_doc_id);
        assert_ne!(editor.tree.get(editor.tree.focus).doc, active_doc);
        let _ = fs::remove_file(new_path);
    }
}

/// Lock-free cursor position cache. Packs `Option<Option<Position>>` into
/// a single `AtomicU64` so the cache is `Sync` without any locking.
///
/// Encoding: row (upper 32 bits) | col (lower 32 bits).
/// Sentinel `u64::MAX` = not yet computed, `u64::MAX - 1` = offscreen.
pub struct CursorCache(std::sync::atomic::AtomicU64);

const CURSOR_UNSET: u64 = u64::MAX;
const CURSOR_OFFSCREEN: u64 = u64::MAX - 1;

impl Default for CursorCache {
    fn default() -> Self {
        Self(std::sync::atomic::AtomicU64::new(CURSOR_UNSET))
    }
}

impl CursorCache {
    pub fn get(&self, view: &View, doc: &Document) -> Option<Position> {
        let v = self.0.load(std::sync::atomic::Ordering::Relaxed);
        if v != CURSOR_UNSET {
            return Self::decode(v);
        }

        let text = doc.text().slice(..);
        let cursor = doc.selection(view.id).primary().cursor(text);
        let pos = view.screen_coords_at_pos(doc, text, cursor);
        self.set(pos);
        pos
    }

    pub fn set(&self, cursor_pos: Option<Position>) {
        self.0.store(
            Self::encode(cursor_pos),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn reset(&self) {
        self.0
            .store(CURSOR_UNSET, std::sync::atomic::Ordering::Relaxed);
    }

    fn encode(pos: Option<Position>) -> u64 {
        match pos {
            None => CURSOR_OFFSCREEN,
            Some(p) => ((p.row as u64) << 32) | (p.col as u64 & 0xFFFF_FFFF),
        }
    }

    fn decode(v: u64) -> Option<Position> {
        if v == CURSOR_OFFSCREEN {
            return None;
        }
        Some(Position {
            row: (v >> 32) as usize,
            col: (v & 0xFFFF_FFFF) as usize,
        })
    }
}
