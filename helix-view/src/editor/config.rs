use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU8,
    path::PathBuf,
    time::Duration,
};

use crate::{
    annotations::diagnostics::{DiagnosticFilter, InlineDiagnosticsConfig},
    clipboard::ClipboardProvider,
    document::Mode,
    graphics::CursorKind,
    input::{KeyCode, KeyEvent, KeyModifiers},
};
use helix_core::{
    syntax::config::{AutoPairConfig, IndentationHeuristic, SoftWrap},
    LineEnding, NATIVE_LINE_ENDING,
};
use serde::{ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};

use super::Severity;

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
    pub layout: Vec<GutterType>,
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
    fn from(layout: Vec<GutterType>) -> Self {
        Self {
            layout,
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
    Never,
    CursorLine,
    AllLines,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct InlineBlameConfig {
    pub show: InlineBlameShow,
    pub auto_fetch: bool,
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
    pub hidden: bool,
    pub follow_symlinks: bool,
    pub deduplicate_links: bool,
    pub parents: bool,
    pub ignore: bool,
    pub git_ignore: bool,
    pub git_global: bool,
    pub git_exclude: bool,
    pub max_depth: Option<usize>,
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
    pub hidden: bool,
    pub follow_symlinks: bool,
    pub parents: bool,
    pub ignore: bool,
    pub git_ignore: bool,
    pub git_global: bool,
    pub git_exclude: bool,
    pub flatten_dirs: bool,
    pub icons: bool,
    pub vcs: bool,
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
            icons: true,
            vcs: true,
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

    let value = String::deserialize(deserializer)?;
    let chars: Vec<_> = value.chars().collect();
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
    pub welcome_screen: bool,
    pub scrolloff: usize,
    pub scroll_lines: isize,
    pub mouse: bool,
    pub shell: Vec<String>,
    pub line_number: LineNumber,
    pub picker_symbol: String,
    pub cursorline: bool,
    pub cursorcolumn: bool,
    #[serde(deserialize_with = "deserialize_gutter_seq_or_struct")]
    pub gutters: GutterConfig,
    pub middle_click_paste: bool,
    pub auto_pairs: AutoPairConfig,
    pub auto_completion: bool,
    pub path_completion: bool,
    pub word_completion: WordCompletion,
    pub auto_format: bool,
    pub default_yank_register: char,
    #[serde(deserialize_with = "deserialize_auto_save")]
    pub auto_save: AutoSave,
    #[serde(default = "default_true")]
    pub auto_reload: bool,
    pub text_width: usize,
    #[serde(
        serialize_with = "serialize_duration_millis",
        deserialize_with = "deserialize_duration_millis"
    )]
    pub idle_timeout: Duration,
    #[serde(
        serialize_with = "serialize_duration_millis",
        deserialize_with = "deserialize_duration_millis"
    )]
    pub completion_timeout: Duration,
    pub preview_completion_insert: bool,
    pub completion_trigger_len: u8,
    pub completion_replace: bool,
    pub continue_comments: bool,
    pub auto_info: bool,
    pub file_picker: FilePickerConfig,
    pub bufferline: BufferLineConfig,
    pub file_explorer: FileExplorerConfig,
    pub vcs: VcsConfig,
    pub statusline: StatusLineConfig,
    pub cursor_shape: CursorShapeConfig,
    pub true_color: bool,
    pub undercurl: bool,
    #[serde(default)]
    pub search: SearchConfig,
    pub lsp: LspConfig,
    pub terminal: Option<TerminalConfig>,
    pub rulers: Vec<u16>,
    pub ruler_char: String,
    #[serde(default)]
    pub whitespace: WhitespaceConfig,
    pub indent_guides: IndentGuidesConfig,
    pub color_modes: bool,
    pub soft_wrap: SoftWrap,
    pub workspace_lsp_roots: Vec<PathBuf>,
    pub default_line_ending: LineEndingConfig,
    pub insert_final_newline: bool,
    pub atomic_save: bool,
    pub trim_final_newlines: bool,
    pub trim_trailing_whitespace: bool,
    pub smart_tab: Option<SmartTabConfig>,
    pub popup_border: PopupBorderConfig,
    pub rounded_corners: bool,
    #[serde(default)]
    pub indent_heuristic: IndentationHeuristic,
    #[serde(
        serialize_with = "serialize_alphabet",
        deserialize_with = "deserialize_alphabet"
    )]
    pub jump_label_alphabet: Vec<char>,
    pub inline_diagnostics: InlineDiagnosticsConfig,
    pub end_of_line_diagnostics: DiagnosticFilter,
    pub clipboard_provider: ClipboardProvider,
    pub editor_config: bool,
    pub max_panel_width: usize,
    pub max_panel_height: usize,
    pub max_panel_width_percent: f32,
    pub max_panel_height_percent: f32,
    #[serde(default)]
    pub inline_blame: InlineBlameConfig,
    pub rainbow_brackets: bool,
    pub kitty_keyboard_protocol: KittyKeyboardProtocolConfig,
    #[serde(default)]
    pub cmdline: CmdlineConfig,
    #[serde(default)]
    pub gradient_borders: GradientBorderConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub completion_highlight: CompletionHighlight,
    pub buffer_picker: BufferPickerConfig,
    #[serde(default)]
    pub fold_textobjects: Vec<String>,
    #[serde(default = "default_agents")]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub acp: AcpConfig,
    #[serde(default)]
    pub editing_engine: EditingEngineConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct VcsConfig {
    pub provider: VcsProvider,
}

impl Default for VcsConfig {
    fn default() -> Self {
        Self {
            provider: VcsProvider::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VcsProvider {
    #[default]
    Auto,
    Git,
    Jj,
    None,
}

impl From<VcsProvider> for helix_vcs::VcsProvider {
    fn from(provider: VcsProvider) -> Self {
        match provider {
            VcsProvider::Auto => Self::Auto,
            VcsProvider::Git => Self::Git,
            VcsProvider::Jj => Self::Jj,
            VcsProvider::None => Self::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct AcpConfig {
    pub cycle_thinking: Option<String>,
    pub cycle_model: Option<String>,
    pub cycle_mode: Option<String>,
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

    pub fn bubble_corners_rounded(&self) -> bool {
        !matches!(self.bubble_corners.as_deref(), Some("squared"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AgentConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub theme: Option<String>,
}

pub(super) fn default_agents() -> Vec<AgentConfig> {
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
    pub style: CmdlineStyle,
    pub show_icons: bool,
    pub min_popup_width: u16,
    pub max_popup_width: u16,
    pub use_full_height: bool,
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
    pub search: String,
    pub command: String,
    pub shell: String,
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
    #[default]
    Bottom,
    Popup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct GradientBorderConfig {
    pub enable: bool,
    pub thickness: u8,
    pub direction: GradientDirection,
    pub start_color: String,
    pub end_color: String,
    pub middle_color: String,
    pub animation_speed: u8,
}

impl Default for GradientBorderConfig {
    fn default() -> Self {
        Self {
            enable: false,
            thickness: 1,
            direction: GradientDirection::Horizontal,
            start_color: "#8A2BE2".to_string(),
            end_color: "#00BFFF".to_string(),
            middle_color: "".to_string(),
            animation_speed: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GradientDirection {
    #[default]
    Horizontal,
    Vertical,
    Diagonal,
    Radial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct NotificationConfig {
    pub enable: bool,
    pub style: NotificationStyle,
    pub max_history: usize,
    #[serde(
        serialize_with = "serialize_duration_millis",
        deserialize_with = "deserialize_duration_millis"
    )]
    pub default_timeout: Duration,
    pub position: NotificationPosition,
    pub max_width: u16,
    pub max_height: u16,
    pub show_icons: bool,
    pub icons: NotificationIcons,
    pub padding: u16,
    pub show_emojis: bool,
    pub emojis: NotificationEmojis,
    pub enable_history: bool,
    pub border: NotificationBorderConfig,
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
    #[default]
    Popup,
    Statusline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationPosition {
    TopLeft,
    TopCenter,
    #[default]
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct NotificationIcons {
    pub info: String,
    pub warning: String,
    pub error: String,
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
    pub info: String,
    pub warning: String,
    pub error: String,
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
    pub enable: bool,
    pub width: u8,
    pub radius: u8,
    pub gradient: bool,
    pub gradient_start: String,
    pub gradient_end: String,
    pub style: NotificationBorderStyle,
}

impl Default for NotificationBorderConfig {
    fn default() -> Self {
        Self {
            enable: true,
            width: 1,
            radius: 2,
            gradient: false,
            gradient_start: "#8A2BE2".to_string(),
            gradient_end: "#00BFFF".to_string(),
            style: NotificationBorderStyle::Solid,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationBorderStyle {
    #[default]
    Solid,
    Dashed,
    Dotted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct NotificationShadowConfig {
    pub enable: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct SmartTabConfig {
    pub enable: bool,
    pub supersede_menu: bool,
}

impl Default for SmartTabConfig {
    fn default() -> Self {
        Self {
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
    pub enable: bool,
    pub display_progress_messages: bool,
    pub display_messages: bool,
    pub auto_signature_help: bool,
    pub display_signature_help_docs: bool,
    pub signature_help_position: SignatureHelpPosition,
    pub display_inlay_hints: bool,
    pub inlay_hints_length_limit: Option<NonZeroU8>,
    pub display_color_swatches: bool,
    pub color_swatches_string: String,
    pub snippets: bool,
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
    pub smart_case: bool,
    pub wrap_around: bool,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            wrap_around: true,
            smart_case: true,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BufferLineRenderMode {
    #[default]
    Never,
    Always,
    Multiple,
}

fn bufferline_render_mode_from_str<E>(value: &str) -> Result<BufferLineRenderMode, E>
where
    E: serde::de::Error,
{
    match value {
        "never" => Ok(BufferLineRenderMode::Never),
        "always" => Ok(BufferLineRenderMode::Always),
        "multiple" => Ok(BufferLineRenderMode::Multiple),
        other => Err(E::unknown_variant(other, &["never", "always", "multiple"])),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
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

impl<'de> Deserialize<'de> for BufferLineConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BufferLineVisitor;

        impl<'de> serde::de::Visitor<'de> for BufferLineVisitor {
            type Value = BufferLineConfig;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(
                    formatter,
                    "a bufferline render mode string or a detailed bufferline configuration"
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(BufferLineConfig {
                    render_mode: bufferline_render_mode_from_str(value)?,
                    ..Default::default()
                })
            }

            fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                #[derive(Deserialize)]
                #[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
                struct BufferLineConfigFields {
                    render_mode: BufferLineRenderMode,
                    separator: String,
                }

                impl Default for BufferLineConfigFields {
                    fn default() -> Self {
                        let config = BufferLineConfig::default();
                        Self {
                            render_mode: config.render_mode,
                            separator: config.separator,
                        }
                    }
                }

                let fields = BufferLineConfigFields::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;

                Ok(BufferLineConfig {
                    render_mode: fields.render_mode,
                    separator: fields.separator,
                })
            }
        }

        deserializer.deserialize_any(BufferLineVisitor)
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
        use StatusLineElement as Element;

        Self {
            left: vec![
                Element::Mode,
                Element::Spinner,
                Element::FileName,
                Element::ReadOnlyIndicator,
                Element::FileModificationIndicator,
            ],
            center: vec![],
            right: vec![
                Element::Diagnostics,
                Element::Selections,
                Element::Register,
                Element::Position,
                Element::FileEncoding,
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
    Mode,
    Spinner,
    FileBaseName,
    FileName,
    FileAbsolutePath,
    FileModificationIndicator,
    ReadOnlyIndicator,
    FileEncoding,
    FileLineEnding,
    FileIndentStyle,
    FileType,
    Diagnostics,
    WorkspaceDiagnostics,
    Selections,
    PrimarySelectionLength,
    Position,
    Separator,
    PositionPercentage,
    TotalLineNumbers,
    Spacer,
    VersionControl,
    Register,
    CurrentWorkingDirectory,
    FunctionName,
}

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
        let modes = HashMap::<Mode, CursorKind>::deserialize(deserializer)?;
        let into_cursor = |mode: Mode| modes.get(&mode).copied().unwrap_or_default();
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
        S: Serializer,
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
    Absolute,
    Relative,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditingEngineConfig {
    #[default]
    Helix,
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
    Diagnostics,
    LineNumbers,
    Spacer,
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
    All,
}

impl WhitespaceRender {
    pub fn space(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(value) => value,
            Self::Specific { default, space, .. } => {
                space.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }

    pub fn nbsp(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(value) => value,
            Self::Specific { default, nbsp, .. } => {
                nbsp.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }

    pub fn nnbsp(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(value) => value,
            Self::Specific { default, nnbsp, .. } => {
                nnbsp.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }

    pub fn tab(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(value) => value,
            Self::Specific { default, tab, .. } => {
                tab.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }

    pub fn newline(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(value) => value,
            Self::Specific {
                default, newline, ..
            } => newline.or(default).unwrap_or(WhitespaceRenderValue::None),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct AutoSave {
    #[serde(default)]
    pub after_delay: AutoSaveAfterDelay,
    #[serde(default)]
    pub focus_lost: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutoSaveAfterDelay {
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_auto_save_delay")]
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
    D: Deserializer<'de>,
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
            space: '·',
            nbsp: '⍽',
            nnbsp: '␣',
            tab: '→',
            newline: '⏎',
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

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineEndingConfig {
    #[default]
    Native,
    LF,
    Crlf,
    #[cfg(feature = "unicode-lines")]
    FF,
    #[cfg(feature = "unicode-lines")]
    CR,
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
            completion_replace: false,
            continue_comments: true,
            auto_info: true,
            file_picker: FilePickerConfig::default(),
            bufferline: BufferLineConfig::default(),
            file_explorer: FileExplorerConfig::default(),
            vcs: VcsConfig::default(),
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
            workspace_lsp_roots: Vec::new(),
            text_width: 80,
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
            editor_config: true,
            max_panel_width: 50,
            max_panel_height: 50,
            max_panel_width_percent: 0.8,
            max_panel_height_percent: 0.8,
            inline_blame: InlineBlameConfig::default(),
            rainbow_brackets: false,
            kitty_keyboard_protocol: KittyKeyboardProtocolConfig::default(),
            cmdline: CmdlineConfig::default(),
            gradient_borders: GradientBorderConfig::default(),
            notifications: NotificationConfig::default(),
            completion_highlight: CompletionHighlight::default(),
            buffer_picker: BufferPickerConfig::default(),
            fold_textobjects: Vec::new(),
            agents: default_agents(),
            acp: AcpConfig::default(),
            editing_engine: EditingEngineConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BufferLineRenderMode, Config, VcsProvider};

    #[test]
    fn bufferline_accepts_render_mode_string() {
        let config: Config = toml::from_str(r#"bufferline = "multiple""#).unwrap();

        assert_eq!(
            config.bufferline.render_mode,
            BufferLineRenderMode::Multiple
        );
        assert_eq!(config.bufferline.separator, "│");
    }

    #[test]
    fn bufferline_accepts_detailed_config_table() {
        let config: Config = toml::from_str(
            r#"
            [bufferline]
            render-mode = "always"
            separator = ">"
            "#,
        )
        .unwrap();

        assert_eq!(config.bufferline.render_mode, BufferLineRenderMode::Always);
        assert_eq!(config.bufferline.separator, ">");
    }

    #[test]
    fn vcs_provider_accepts_explicit_selection() {
        let config: Config = toml::from_str(
            r#"
            [vcs]
            provider = "jj"
            "#,
        )
        .unwrap();

        assert_eq!(config.vcs.provider, VcsProvider::Jj);
    }
}
