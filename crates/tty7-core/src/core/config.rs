use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

pub const SUPPORTED_GUI_LANGUAGES: &[&str] = &["en", "zh-CN", "ja-JP", "ru-RU"];

#[derive(Default, Clone, Eq, PartialEq, Hash)]
pub struct FontFeatures(pub Arc<Vec<(String, u32)>>);

impl FontFeatures {
    pub fn tag_value_list(&self) -> &[(String, u32)] {
        self.0.as_slice()
    }
}

impl std::fmt::Debug for FontFeatures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("FontFeatures");
        for (tag, value) in self.tag_value_list() {
            debug.field(tag, value);
        }
        debug.finish()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum FeatureValue {
    Bool(bool),
    Number(serde_json::Number),
}

fn is_valid_feature_tag(tag: &str) -> bool {
    tag.len() == 4 && tag.chars().all(|c| c.is_ascii_alphanumeric())
}

impl<'de> serde::Deserialize<'de> for FontFeatures {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};

        struct FontFeaturesVisitor;

        impl<'de> Visitor<'de> for FontFeaturesVisitor {
            type Value = FontFeatures;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map of font features")
            }

            fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut feature_list = Vec::new();
                while let Some((key, value)) =
                    access.next_entry::<String, Option<FeatureValue>>()?
                {
                    if !is_valid_feature_tag(&key) {
                        log::error!("Incorrect font feature tag: {key}");
                        continue;
                    }
                    let Some(value) = value else { continue };
                    match value {
                        FeatureValue::Bool(enable) => {
                            feature_list.push((key, u32::from(enable)));
                        }
                        FeatureValue::Number(value) => match value.as_u64() {
                            Some(value) => feature_list.push((key, value as u32)),
                            None => {
                                log::error!(
                                    "Incorrect font feature value {value} for feature tag {key}"
                                );
                                continue;
                            }
                        },
                    }
                }
                Ok(FontFeatures(Arc::new(feature_list)))
            }
        }

        deserializer.deserialize_map(FontFeaturesVisitor)
    }
}

impl serde::Serialize for FontFeatures {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        for (tag, value) in self.tag_value_list() {
            map.serialize_entry(tag, value)?;
        }
        map.end()
    }
}

/// One action's line in `keybindings`.
///
/// The two shapes mean different things, so a save writes back whichever one
/// was read.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(untagged)]
pub enum KeybindingOverride {
    /// `"NextTab": "cmd-shift-]"` — a chord *beside* the ones the action
    /// already has, the way VS Code, Zed and kitty read a line like it (#868).
    /// Empty unbinds the action, which is what `""` has always meant here.
    Add(String),
    /// `"NextTab": ["cmd-shift-]"]` — exactly these chords, replacing the
    /// default and the preset's. `[]` unbinds. This is what the Settings page
    /// writes, because recording a shortcut there sets it.
    Exact(Vec<String>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub font_family: String,
    pub font_fallbacks: Vec<String>,
    pub font_family_bold: Option<String>,
    pub font_family_italic: Option<String>,
    pub font_features: Option<FontFeatures>,
    /// macOS only: let CoreGraphics font smoothing thicken glyph strokes — by
    /// more the lighter the text, which is why light-on-dark looks bolder.
    /// On by default, because it is what every macOS app draws with.
    ///
    /// Off tells gpui's rasterizer not to dilate, so tty7's glyphs are drawn
    /// at the face's own weight. `AppleFontSmoothing` is left alone, so menus
    /// and other natively drawn text keep the system's look (#1119). Applied
    /// at launch, so a change takes hold at the next one. Ignored elsewhere,
    /// where there is no such dilation to turn off.
    #[serde(default = "default_true")]
    pub font_thicken: bool,
    pub font_size: f32,
    pub line_height: f32,
    /// The face the chrome is drawn in; `None` is the system UI font.
    ///
    /// Only the chrome — the terminal grid keeps `font_family`. The
    /// struct-level `serde(default)` fills it in for a config written before
    /// the setting existed, and like every other optional key it is written
    /// out as `null` rather than dropped, so the file still lists what can be
    /// set by hand.
    pub ui_font_family: Option<String>,
    pub theme_preset: String,
    /// When set, the live theme is `theme_preset_dark` or `theme_preset_light`
    /// according to the OS appearance. On for a new config: a missing key used
    /// to mean off, and a file that says `false` still does.
    #[serde(default = "default_true")]
    pub theme_follow_system: bool,
    pub theme_preset_light: String,
    pub theme_preset_dark: String,
    #[serde(default = "default_true")]
    pub theme_legible_palette: bool,
    pub window_opacity: Option<f32>,
    pub window_blur: Option<bool>,
    #[serde(default, deserialize_with = "de_lenient")]
    pub window_backdrop: WindowBackdrop,
    /// Lenient one entry at a time, for the same reason the nested keys below
    /// are: this is hand-edited, and it used to be all-or-nothing. A single
    /// value serde could not read — `"ActivateTab1": null`, a number, an object
    /// — failed the whole `Config`, which quarantines `config.json` and starts
    /// the app on built-in defaults; every rebinding in the file then read as
    /// its shipped default, and the next settings write persisted those
    /// defaults over what the user wrote (#901). A line that cannot be read is
    /// skipped with a warning, and the rest of the map still binds.
    #[serde(default, deserialize_with = "de_keybindings")]
    pub keybindings: HashMap<String, KeybindingOverride>,
    #[serde(default = "default_preset")]
    pub keybinding_preset: String,
    #[serde(default = "default_prefix")]
    pub prefix: String,
    #[serde(default, deserialize_with = "de_shell")]
    pub shell: Option<ShellConfig>,

    pub link_url: bool,
    /// What a clicked file link opens in. `None` means the key predates this
    /// setting; [`Config::sanitize`] resolves it, and every reader should go
    /// through [`Config::file_open_mode`] rather than read it raw.
    #[serde(default, deserialize_with = "de_lenient")]
    pub link_file_open: Option<LinkFileOpen>,
    pub link_file_command: Option<String>,
    pub cursor_blink: bool,
    pub scrollback_limit: usize,
    #[serde(default, deserialize_with = "de_lenient")]
    pub notify_on_command_finish: NotifyMode,
    /// GUI-only locale selection. See `SUPPORTED_GUI_LANGUAGES`.
    /// CLI output stays English so agent/script integrations are stable.
    #[serde(default = "default_gui_language")]
    pub gui_language: String,
    #[serde(default = "default_notify_threshold_secs")]
    pub notify_threshold_secs: u64,
    /// When closing a tab or a pane asks first — see [`ConfirmClose`].
    #[serde(default, deserialize_with = "de_lenient")]
    pub confirm_close: ConfirmClose,
    #[serde(default, deserialize_with = "de_lenient")]
    pub bell: BellMode,

    #[serde(default, deserialize_with = "de_lenient")]
    pub cursor_style: CursorStyle,
    /// The caret's shape while the shell waits at a prompt, whether tty7's
    /// inline editor or the shell's own line editor draws it. `follow` keeps
    /// `cursor_style` everywhere; any other value leaves `cursor_style` to the
    /// programs the shell runs, which is how kitty and ghostty put a bar at
    /// the prompt and a block inside a TUI that never sets a shape itself.
    #[serde(default, deserialize_with = "de_lenient")]
    pub prompt_cursor_style: PromptCursorStyle,

    pub macos_option_as_alt: bool,
    pub mouse_hide_while_typing: bool,
    pub focus_follows_mouse: bool,
    pub mouse_scroll_multiplier: f32,
    /// Spread a wheel detent over several frames instead of jumping the whole
    /// distance at once. Trackpad gestures are left alone — they are already a
    /// continuous stream, and animating one would just add lag.
    #[serde(default = "default_true")]
    pub smooth_scroll: bool,
    #[serde(default = "default_true")]
    pub mouse_reporting: bool,
    /// Which modifier turns the wheel into a font zoom over a terminal.
    ///
    /// Off by default. It used to be the platform modifier, but on macOS that
    /// is ⌘, a key people are holding half the time for something else
    /// entirely, so the font jumped size while they scrolled (#668). ⌘+ / ⌘−
    /// still zoom; this only opts the wheel in.
    #[serde(default, deserialize_with = "de_lenient")]
    pub mouse_zoom_modifier: MouseZoomModifier,
    pub clipboard_trim_trailing_spaces: bool,
    /// In a Claude Code pane, copy joins the rows Claude wrapped itself, so a
    /// paragraph pastes as one line. ⌘⌥C and Copy Raw copy the rows as is.
    #[serde(default)]
    pub copy_join_wrapped: bool,
    pub copy_on_select: bool,
    #[serde(default = "default_true")]
    pub smart_select: bool,
    #[serde(default = "default_word_separators")]
    pub word_separators: String,
    #[serde(default, deserialize_with = "de_lenient")]
    pub startup_mode: StartupMode,

    #[serde(default)]
    pub working_directory: WorkingDirectory,
    #[serde(default)]
    pub env: HashMap<String, String>,

    #[serde(default)]
    pub agent_commands: HashMap<String, String>,
    /// The command line a quick launch types for an agent, keyed by its slug:
    /// `{"claude": "claude --dangerously-skip-permissions"}`. An agent with no
    /// entry is launched as its bare binary. A wrapper named here is detected
    /// as that agent without an `agent_commands` entry of its own.
    #[serde(default)]
    pub agent_launch: HashMap<String, String>,
    /// Give each pane its own shell history instead of one file every pane
    /// appends to and reads back.
    ///
    /// Seeded from the shell's real history file, so a new pane is not blank,
    /// and merged back into it when the pane closes, so nothing typed is lost.
    /// Off by default because shared history is what a terminal has always
    /// done, and someone who has not asked for the change would experience it
    /// as their history mysteriously forgetting the other window.
    #[serde(default)]
    pub per_pane_history: bool,

    /// This instance is the stand-in for a file that could not be read or
    /// parsed: `load` kept a copy aside and handed back defaults. Never
    /// serialized — it describes how the file *load* went, not a setting —
    /// and it makes [`Config::save`] refuse to run, because writing these
    /// defaults back over the user's hand-edited file is how one typo becomes
    /// permanent data loss (#537). Cleared only by a load that parses.
    #[serde(skip)]
    pub quarantined: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct WorkingDirectory {
    #[serde(deserialize_with = "de_lenient")]
    pub strategy: WdStrategy,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WdStrategy {
    #[default]
    Inherit,
    Home,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartupMode {
    #[default]
    Normal,
    Maximized,
    Fullscreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CursorStyle {
    #[default]
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptCursorStyle {
    #[default]
    Follow,
    Block,
    Bar,
    Underline,
}

impl PromptCursorStyle {
    /// The shape this names, or `None` when the prompt follows `cursor_style`.
    pub fn shape(self) -> Option<CursorStyle> {
        match self {
            Self::Follow => None,
            Self::Block => Some(CursorStyle::Block),
            Self::Bar => Some(CursorStyle::Bar),
            Self::Underline => Some(CursorStyle::Underline),
        }
    }
}

/// Native window backdrop material for the Windows GUI. Other platforms retain
/// the value for config synchronization but do not use it for rendering.
/// `Auto` keeps the legacy behavior where theme blur decides between blurred
/// and plain translucent, `Blur` explicitly requests classic WCA acrylic, the
/// material variants fall back to acrylic or plain translucency on older builds
/// inside `src/ui/theme.rs`, and `Off` never requests a material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowBackdrop {
    #[default]
    Auto,
    Blur,
    Mica,
    MicaAlt,
    Acrylic,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotifyMode {
    Never,
    #[default]
    Unfocused,
    Always,
}

impl NotifyMode {
    /// Whether a notification goes out, given whether the reader is already
    /// watching what it is about.
    pub fn allows(self, watched: bool) -> bool {
        match self {
            NotifyMode::Never => false,
            NotifyMode::Unfocused => !watched,
            NotifyMode::Always => true,
        }
    }
}

/// The modifier that makes the mouse wheel resize the font.
///
/// `Platform` is ⌘ on macOS and Ctrl elsewhere, stored rather than the
/// resolved key so one config file can be shared between machines that
/// disagree about which key that is. `None`, the default, leaves the wheel to
/// scrolling.
///
/// Shift is deliberately not offered: shift+wheel is the escape hatch that
/// scrolls the scrollback out from under a mouse-reporting program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MouseZoomModifier {
    Platform,
    Ctrl,
    Alt,
    /// The wheel never zooms; every scroll goes to the buffer.
    #[default]
    None,
}

/// When closing a tab or a pane asks first (#1021).
///
/// `WhenBusy` is the question tty7 has always asked, and the default: only
/// when something would be cut off — a program still running in the
/// foreground, an agent mid-turn. `Always` also asks about an idle shell, for
/// people who would rather confirm every close than ever lose one; `Never`
/// stops asking about busy panes.
///
/// The warning before dropping a live SSH connection is not governed by this.
/// It is an opt-in of its own, per host or for all of them, so it is still
/// honoured under `Never`: turning this down is not turning that off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfirmClose {
    Never,
    #[default]
    WhenBusy,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BellMode {
    None,
    #[default]
    Visual,
    Audible,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ShellConfig {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

pub fn default_font_fallbacks() -> Vec<String> {
    let names: &[&str] = if cfg!(target_os = "macos") {
        &[
            "Menlo",
            "Hasklug Nerd Font Mono",
            "Maple Mono NF CN",
            "PingFang SC",
            "Apple Color Emoji",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            "Maple Mono NF CN",
            "Cascadia Mono",
            "Microsoft YaHei",
            "Segoe UI Emoji",
        ]
    } else {
        &[
            "Maple Mono NF CN",
            "DejaVu Sans Mono",
            "Noto Sans CJK SC",
            "Noto Color Emoji",
        ]
    };
    names.iter().map(|n| n.to_string()).collect()
}

pub fn platform_last_resort_fallbacks() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["PingFang SC", "Apple Color Emoji"]
    } else if cfg!(target_os = "windows") {
        &["Microsoft YaHei", "Segoe UI Emoji"]
    } else {
        &["Noto Sans CJK SC", "Noto Color Emoji"]
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_family: "Hack".to_string(),
            font_fallbacks: default_font_fallbacks(),
            font_family_bold: None,
            font_family_italic: None,
            font_features: None,
            font_thicken: true,
            font_size: 15.0,
            line_height: 1.4,
            ui_font_family: None,
            theme_preset: "light".to_string(),
            theme_follow_system: true,
            theme_preset_light: "light".to_string(),
            theme_preset_dark: "dark".to_string(),
            theme_legible_palette: true,
            window_opacity: None,
            window_blur: None,
            window_backdrop: WindowBackdrop::default(),
            keybindings: HashMap::new(),
            keybinding_preset: default_preset(),
            prefix: default_prefix(),
            shell: None,
            link_url: true,
            link_file_open: Some(LinkFileOpen::System),
            link_file_command: None,
            cursor_blink: true,
            scrollback_limit: 10_000,
            notify_on_command_finish: NotifyMode::Unfocused,
            gui_language: default_gui_language(),
            notify_threshold_secs: default_notify_threshold_secs(),
            confirm_close: ConfirmClose::WhenBusy,
            bell: BellMode::Visual,
            cursor_style: CursorStyle::Block,
            prompt_cursor_style: PromptCursorStyle::Follow,
            macos_option_as_alt: false,
            mouse_hide_while_typing: true,
            focus_follows_mouse: false,
            mouse_scroll_multiplier: 1.0,
            smooth_scroll: true,
            mouse_reporting: true,
            mouse_zoom_modifier: MouseZoomModifier::default(),
            clipboard_trim_trailing_spaces: false,
            copy_join_wrapped: false,
            copy_on_select: false,
            smart_select: true,
            word_separators: default_word_separators(),
            startup_mode: StartupMode::Normal,
            working_directory: WorkingDirectory::default(),
            env: HashMap::new(),
            agent_commands: HashMap::new(),
            agent_launch: HashMap::new(),
            per_pane_history: false,
            quarantined: false,
        }
    }
}

/// How the file behind a [`Config::load_with_outcome`] went — the answer a
/// hot-reload watcher needs before it swaps a running app onto the result,
/// because "no file yet" and "a broken file" must not do the same thing
/// (#537).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    /// The file parsed (after the usual field-level leniency).
    Parsed,
    /// There is no file (or no config dir yet): the defaults simply are the
    /// config, and saving them is fine.
    Absent,
    /// The file existed but did not parse. A copy was kept beside it, and the
    /// returned config is the defaults with writes suppressed — saving over
    /// the broken file would make one typo permanent.
    Quarantined,
    /// The file is there but could not be read at all. Writes are suppressed
    /// the same way, but nothing was parked beside it: there was nothing
    /// readable to copy. Distinct from [`LoadOutcome::Quarantined`] because
    /// telling the user to look in `config.json.corrupt` for contents that
    /// were never written there sends them after a file that is not there.
    Unreadable,
}

impl LoadOutcome {
    /// Whether the file is standing between the user and their settings: the
    /// values handed back are defaults with writes suppressed, not anything
    /// the user wrote.
    pub fn failed(self) -> bool {
        matches!(self, Self::Quarantined | Self::Unreadable)
    }
}

impl Config {
    pub fn load() -> Self {
        Self::load_with_outcome().0
    }

    /// [`Config::load`] with the verdict the file earned. Most callers want
    /// the values either way and use `load`; the watcher that swaps a running
    /// app onto the result needs the outcome to keep a broken file from
    /// evicting the settings the app is running on.
    pub fn load_with_outcome() -> (Self, LoadOutcome) {
        match Self::path() {
            Some(path) => Self::load_from(&path),
            None => (Config::default(), LoadOutcome::Absent),
        }
    }

    pub(crate) fn load_from(path: &Path) -> (Self, LoadOutcome) {
        let (cfg, outcome, _) = Self::load_config_file(path);
        (cfg, outcome)
    }

    /// `config.json` alone, with the text it parsed from.
    fn load_config_file(path: &Path) -> (Self, LoadOutcome, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (Config::default(), LoadOutcome::Absent, None);
            }
            Err(e) => {
                // Unreadable is not unparseable, but the rule is the same:
                // what cannot be read must not be overwritten. There may be
                // nothing readable to keep a copy of, so nothing is parked —
                // the file itself is still where the user left it.
                log::warn!(
                    "failed to read config at {}: {e}; using defaults, writes suppressed",
                    path.display()
                );
                let cfg = Config {
                    quarantined: true,
                    ..Config::default()
                };
                return (cfg, LoadOutcome::Unreadable, None);
            }
        };
        match serde_json::from_str::<Config>(strip_bom(&text)) {
            Ok(mut cfg) => {
                cfg.sanitize();
                (cfg, LoadOutcome::Parsed, Some(text))
            }
            Err(e) => {
                // The next `save` overwrites this file wholesale, so handing
                // back defaults with no trace quietly discards whatever the
                // file held the moment anything — a dragged sidebar divider —
                // writes. Park a copy first, the way `WindowViews::load`
                // does, and mark the stand-in so `save` refuses to run for it.
                log::warn!(
                    "failed to parse config at {}: {e}; keeping it aside and using defaults",
                    path.display()
                );
                quarantine(path);
                let cfg = Config {
                    quarantined: true,
                    ..Config::default()
                };
                (cfg, LoadOutcome::Quarantined, None)
            }
        }
    }

    /// What a clicked file link should open in, with the pre-setting default
    /// filled in for a `Config` that never went through [`Self::sanitize`].
    pub fn file_open_mode(&self) -> LinkFileOpen {
        self.link_file_open.unwrap_or_default()
    }

    fn sanitize(&mut self) {
        if !self.font_size.is_finite() || self.font_size <= 0.0 {
            self.font_size = Config::default().font_size;
        }
        self.font_size = self.font_size.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX);
        if !self.line_height.is_finite() || self.line_height <= 0.0 {
            self.line_height = Config::default().line_height;
        }
        self.line_height = self.line_height.clamp(LINE_HEIGHT_MIN, LINE_HEIGHT_MAX);
        if let Some(family) = &self.ui_font_family
            && family.trim().is_empty()
        {
            self.ui_font_family = None;
        }
        self.scrollback_limit = self.scrollback_limit.clamp(100, MAX_SCROLLBACK);
        if !self.mouse_scroll_multiplier.is_finite() || self.mouse_scroll_multiplier <= 0.0 {
            self.mouse_scroll_multiplier = Config::default().mouse_scroll_multiplier;
        }
        self.mouse_scroll_multiplier = self.mouse_scroll_multiplier.clamp(0.1, 10.0);
        self.notify_threshold_secs = self.notify_threshold_secs.clamp(1, 3600);
        self.window_opacity = self
            .window_opacity
            .filter(|o| o.is_finite())
            .map(|o| o.clamp(0.2, 1.0));
        if let Some(command) = &self.link_file_command
            && command.trim().is_empty()
        {
            self.link_file_command = None;
        }
        // Absent means a config written before this key. A command the user
        // already named keeps that opener; otherwise the file opens with
        // whatever the OS has it associated with.
        self.link_file_open
            .get_or_insert(match self.link_file_command {
                Some(_) => LinkFileOpen::Command,
                None => LinkFileOpen::System,
            });
        if !SUPPORTED_GUI_LANGUAGES.contains(&self.gui_language.as_str()) {
            self.gui_language = default_gui_language();
        }
    }

    pub fn save(&self) {
        if let Err(error) = self.try_save() {
            log::warn!("failed to save config: {error}");
        }
    }

    /// Persist without hiding a failure from an interactive settings editor.
    pub fn try_save(&self) -> std::io::Result<()> {
        let path = Self::path()
            .ok_or_else(|| std::io::Error::other("configuration directory is unavailable"))?;
        self.try_save_to(&path)
    }

    /// [`Config::try_save`] for the `config.json` at `path`.
    pub(crate) fn try_save_to(&self, path: &Path) -> std::io::Result<()> {
        if self.quarantined {
            return Err(std::io::Error::other(
                "the existing configuration could not be read; repair it before saving",
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.write_config(path)
    }

    fn write_config(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        write_atomic(path, text.as_bytes())
    }

    fn path() -> Option<PathBuf> {
        config_path("config.json")
    }

    /// `config.json`, written from `self` when the file is not there yet.
    ///
    /// [`Config::try_save_to`] creates the parent directory, so a caller that
    /// only has to hand the path to an opener never touches the filesystem
    /// itself. An existing file is left as it is.
    pub fn ensure_config_file_exists(&self) -> Option<PathBuf> {
        let path = Self::path()?;
        if !path.exists() {
            self.save();
        }
        Some(path)
    }
}

static CONFIG_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

pub fn set_config_dir(dir: PathBuf) {
    let _ = CONFIG_DIR_OVERRIDE.set(dir);
}

fn config_dir() -> Option<PathBuf> {
    resolve_config_dir(
        CONFIG_DIR_OVERRIDE.get().cloned(),
        env_config_dir(),
        portable_config_dir(),
        default_config_dir,
    )
}

/// Where the config directory comes from, first match wins: `--config-dir`,
/// then `$NTTY7_CONFIG_DIR`, then a portable install's own data folder, then
/// the platform default. Explicit beats implicit, so a portable copy can still
/// be pointed somewhere else for one launch or one session.
///
/// `$NTTY7_CONFIG_DIR` is the only environment override. A parent `tty7`
/// exports `TTY7_CONFIG_DIR` for its own shells; this process leaves that
/// variable unread so each app keeps its own directory.
///
/// `default` is a closure because the answer rarely gets that far and the
/// default reads the environment again.
fn resolve_config_dir(
    flag: Option<PathBuf>,
    env: Option<PathBuf>,
    portable: Option<PathBuf>,
    default: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    flag.or(env).or(portable).or_else(default)
}

fn env_config_dir() -> Option<PathBuf> {
    env_config_dir_from(std::env::var_os("NTTY7_CONFIG_DIR").map(PathBuf::from))
}

fn env_config_dir_from(dir: Option<PathBuf>) -> Option<PathBuf> {
    dir.filter(|dir| !dir.as_os_str().is_empty())
}

/// The marker the Windows portable ZIP ships beside its executables. The
/// updater reads it as "this is a portable install" (see `update.rs` and
/// `tty7-updater`); here it is half of the opt-in to keeping data beside the
/// executables too.
pub const PORTABLE_MARKER: &str = ".tty7-portable";

/// The folder, beside the executables, a portable install keeps its whole
/// config directory in — `config.json`, sessions, scrollback, history, the
/// daemon's socket and lock.
///
/// Never one of the updater's managed roots: those are moved aside and
/// replaced on every update, and this is the one thing in the install
/// directory that must survive one.
pub const PORTABLE_DATA_DIR: &str = "data";

/// `<exe dir>\data`, when the running executable is a portable install that
/// opted in to keeping its data there. Windows only; `None` everywhere else.
///
/// Resolved once per process: the executable does not move, and a folder
/// created while tty7 runs must not move the config directory under a live
/// daemon.
fn portable_config_dir() -> Option<PathBuf> {
    // `cfg!` rather than `#[cfg]`, so every platform's build type-checks it.
    if !cfg!(windows) {
        return None;
    }
    static PORTABLE: OnceLock<Option<PathBuf>> = OnceLock::new();
    PORTABLE
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?;
            portable_data_dir(exe.parent()?)
        })
        .clone()
}

/// The portable data folder for executables in `exe_dir`, if both the marker
/// and the folder are there.
///
/// The folder has to exist rather than being created on demand, because the
/// marker alone says nothing new: every portable ZIP has shipped it since the
/// in-app updater arrived, as the updater's cue. Keying on the marker by
/// itself would move every existing portable user onto an empty directory at
/// their next update — their settings and sessions still sitting in
/// `%APPDATA%\tty7`, looking lost. Creating `data` is the explicit opt-in, as
/// the `.portable` file is for Windows Terminal. The ZIP cannot ship the folder
/// either: the updater rejects package entries outside its managed roots, and
/// anything inside them is replaced on update.
pub fn portable_data_dir(exe_dir: &Path) -> Option<PathBuf> {
    let data = exe_dir.join(PORTABLE_DATA_DIR);
    (exe_dir.join(PORTABLE_MARKER).is_file() && data.is_dir()).then_some(data)
}

#[cfg(not(windows))]
pub fn default_config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".config/ntty7"))
}

#[cfg(windows)]
pub fn default_config_dir() -> Option<PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(appdata).join("ntty7"));
    }
    let profile = std::env::var_os("USERPROFILE").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(profile).join(".config").join("ntty7"))
}

pub fn config_path(file: &str) -> Option<PathBuf> {
    Some(config_dir()?.join(file))
}

pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{FEFF}').unwrap_or(text)
}

/// Sets a corrupt state file aside (copied, the original left in place) so the
/// caller can fall back to defaults without silently destroying what was there.
pub(crate) fn quarantine(path: &std::path::Path) {
    // A broken file is read again and again — `Config::load` alone runs on
    // every pane spawn and every palette command — so this is reached over
    // and over for the same contents. A sibling already holding those bytes
    // *is* the copy this call would make; without the check, opening a
    // couple of tabs on a broken config.json fills the config directory with
    // eight identical `.corrupt` files and then overwrites the first one.
    if let Ok(bytes) = std::fs::read(path)
        && already_kept(path, &bytes)
    {
        return;
    }
    let aside = quarantine_path(path);
    match std::fs::copy(path, &aside) {
        Ok(_) => log::warn!("the previous contents were kept at {}", aside.display()),
        Err(e) => log::warn!("could not keep a copy at {}: {e}", aside.display()),
    }
}

/// Whether an earlier quarantine of `path` already holds exactly `bytes`.
fn already_kept(path: &std::path::Path, bytes: &[u8]) -> bool {
    quarantine_candidates(path).any(|kept| std::fs::read(&kept).is_ok_and(|held| held == bytes))
}

/// Every name a quarantined copy of `path` may go under, oldest first.
fn quarantine_candidates(path: &std::path::Path) -> impl Iterator<Item = PathBuf> + use<'_> {
    const MAX_QUARANTINED: u32 = 8;

    std::iter::once(path.with_extension("json.corrupt"))
        .chain((1..MAX_QUARANTINED).map(|n| path.with_extension(format!("json.corrupt.{n}"))))
}

fn quarantine_path(path: &std::path::Path) -> PathBuf {
    let mut candidates = quarantine_candidates(path);
    let base = candidates.next().expect("the base name is always offered");
    if !base.exists() {
        return base;
    }
    candidates
        .find(|candidate| !candidate.exists())
        .unwrap_or(base)
}

pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    // Renaming over a symlink replaces the link itself, so a config.json
    // symlinked into a dotfiles repo would silently turn into a plain file and
    // stop syncing. Write next to (and rename over) the file it points at.
    let resolved = resolve_symlinks(path);
    let path = resolved.as_path();
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("out"),
        std::process::id()
    ));
    {
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create(true).truncate(true);
        let mut f = open.open(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        let _ = f.sync_all();
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Follows `path` through any chain of symlinks to the file it finally names,
/// which need not exist yet. Gives up after a bounded number of hops, so a
/// symlink loop falls back to writing over the last link reached.
fn resolve_symlinks(path: &std::path::Path) -> PathBuf {
    const MAX_HOPS: usize = 40;

    let mut resolved = path.to_path_buf();
    for _ in 0..MAX_HOPS {
        let Ok(target) = std::fs::read_link(&resolved) else {
            break;
        };
        resolved = match resolved.parent() {
            Some(parent) => parent.join(target),
            None => target,
        };
    }
    resolved
}

pub fn config_dir_path() -> Option<PathBuf> {
    config_dir()
}

pub fn shell_command() -> Option<(String, Vec<String>)> {
    Config::load().shell.map(|s| (s.program, s.args))
}

pub fn working_directory_base() -> Option<PathBuf> {
    let wd = Config::load().working_directory;
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    match wd.strategy {
        WdStrategy::Inherit => None,
        WdStrategy::Home => home(),
        WdStrategy::Custom => {
            let p = wd.path.trim();
            if p.is_empty() {
                home()
            } else {
                Some(PathBuf::from(p))
            }
        }
    }
}

pub fn extra_env() -> HashMap<String, String> {
    Config::load().env
}

/// Every wrapper name the daemon should read as an agent: `agent_commands`,
/// plus the programs `agent_launch` entries run (see
/// [`crate::core::cli_agent::launch_aliases`]). An explicit `agent_commands`
/// entry wins over an implied one.
///
/// Read again whenever `config.json` changes rather than once per process: the
/// daemon outlives any number of edits, and a launch override added while it
/// runs has to be recognised by the very next launch that uses it.
pub fn agent_detection_aliases() -> std::sync::Arc<HashMap<String, String>> {
    type Stamp = Option<(std::time::SystemTime, u64)>;
    static CACHE: std::sync::Mutex<Option<(Stamp, std::sync::Arc<HashMap<String, String>>)>> =
        std::sync::Mutex::new(None);
    let stamp: Stamp = Config::path()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| Some((m.modified().ok()?, m.len())));
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((seen, aliases)) = cache.as_ref()
        && *seen == stamp
    {
        return aliases.clone();
    }
    let cfg = Config::load();
    let aliases = std::sync::Arc::new(detection_aliases(&cfg.agent_commands, &cfg.agent_launch));
    *cache = Some((stamp, aliases.clone()));
    aliases
}

/// [`agent_detection_aliases`] over the two maps it reads, for a caller that
/// already holds them.
pub fn detection_aliases(
    commands: &HashMap<String, String>,
    launch: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut aliases = crate::core::cli_agent::launch_aliases(launch);
    for (name, slug) in commands {
        aliases.insert(name.to_ascii_lowercase(), slug.clone());
    }
    aliases
}

fn default_preset() -> String {
    "default".to_string()
}

fn default_true() -> bool {
    true
}

fn default_gui_language() -> String {
    "en".to_string()
}

fn default_word_separators() -> String {
    ",│`|:\"' ()[]{}<>\t".to_string()
}

fn default_notify_threshold_secs() -> u64 {
    10
}

fn default_prefix() -> String {
    "ctrl-b".to_string()
}

/// What opens when a file link in the grid is clicked.
///
/// `"internal"` was the built-in editor. That opener is gone, so the name no
/// longer deserializes: [`de_lenient`] reads it as [`LinkFileOpen::System`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkFileOpen {
    /// Whatever the OS has the file associated with.
    #[default]
    System,
    /// [`Config::link_file_command`], for people who want their own editor.
    Command,
}

/// The terminal's font-size and line-height bounds, shared by `sanitize` and
/// the GUI's steppers. The GUI used to keep its own, narrower pair (6–48,
/// 1.0–2.0), so a value inside the config range but outside the GUI's got
/// pushed the *wrong way* by a single step — `font_size: 50` shrank to 48 on
/// "+" — and written back to the file, permanently (#550). One range, defined
/// where the value is validated, is what the steppers and the file agree on.
pub const FONT_SIZE_MIN: f32 = 4.0;
pub const FONT_SIZE_MAX: f32 = 256.0;
pub const LINE_HEIGHT_MIN: f32 = 0.5;
pub const LINE_HEIGHT_MAX: f32 = 4.0;

pub const MAX_SCROLLBACK: usize = 100_000;

/// `keybindings`, read one line at a time.
///
/// [`de_lenient`] is all-or-nothing per field, which for a map means one bad
/// line throwing away every good one. Here each entry stands on its own: the
/// ones that name a shortcut or a list of shortcuts bind, the ones that do not
/// are logged and dropped. A `keybindings` that is not an object at all falls
/// back to an empty map rather than failing the file.
fn de_keybindings<'de, D>(deserializer: D) -> Result<HashMap<String, KeybindingOverride>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let serde_json::Value::Object(entries) = value else {
        log::warn!("ignoring `keybindings` {value}: expected an object of action name to shortcut");
        return Ok(HashMap::new());
    };
    let mut bindings = HashMap::with_capacity(entries.len());
    for (action, raw) in entries {
        match KeybindingOverride::deserialize(&raw) {
            Ok(binding) => {
                bindings.insert(action, binding);
            }
            Err(e) => log::warn!(
                "ignoring keybinding for {action:?}: {raw} is not a shortcut, \
                 a list of shortcuts, or \"\" to unbind ({e})"
            ),
        }
    }
    Ok(bindings)
}

pub(crate) fn de_lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(&value).unwrap_or_else(|e| {
        log::warn!("ignoring invalid config value {value}: {e}; using default");
        T::default()
    }))
}

/// `shell`, read the way Settings writes it: no program means the login
/// shell. A hand-edited `"shell": { "args": ["-l"] }` — or any other shape
/// that does not name a program — used to fail the whole file, and every other
/// setting went with it into quarantine.
fn de_shell<'de, D>(deserializer: D) -> Result<Option<ShellConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    match ShellConfig::deserialize(&value) {
        Ok(shell) if !shell.program.trim().is_empty() => Ok(Some(shell)),
        Ok(_) => Ok(None),
        Err(e) => {
            log::warn!("ignoring invalid shell {value}: {e}; using the login shell");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_mode_holds_back_only_what_is_watched_under_unfocused() {
        for watched in [false, true] {
            assert!(!NotifyMode::Never.allows(watched));
            assert!(NotifyMode::Always.allows(watched));
            assert_eq!(NotifyMode::Unfocused.allows(watched), !watched);
        }
    }

    #[test]
    fn a_shell_without_a_program_is_the_login_shell_not_a_broken_file() {
        for shell in [
            r#"{"args": ["-l"]}"#,
            r#"{"program": "", "args": []}"#,
            r#"{"program": 5}"#,
            "null",
        ] {
            let text = format!(r#"{{"font_size": 17.0, "shell": {shell}}}"#);
            let cfg: Config = serde_json::from_str(&text).expect("the file still parses");
            assert_eq!(cfg.shell, None, "{shell}");
            assert_eq!(cfg.font_size, 17.0, "the rest of the file is kept");
        }
        let cfg: Config =
            serde_json::from_str(r#"{"shell": {"program": "fish", "args": ["-l"]}}"#).unwrap();
        assert_eq!(
            cfg.shell,
            Some(ShellConfig {
                program: "fish".into(),
                args: vec!["-l".into()]
            })
        );
    }
    /// A config from before the setting keeps asking exactly what it asked
    /// before, and a value this build does not know falls back to that too
    /// rather than failing the file.
    #[test]
    fn confirm_close_defaults_to_asking_when_busy() {
        assert_eq!(Config::default().confirm_close, ConfirmClose::WhenBusy);
        let cfg: Config = serde_json::from_str(r#"{"restore_session": false}"#).unwrap();
        assert_eq!(cfg.confirm_close, ConfirmClose::WhenBusy);
        let cfg: Config = serde_json::from_str(r#"{"confirm_close": "sometimes"}"#).unwrap();
        assert_eq!(cfg.confirm_close, ConfirmClose::WhenBusy);
    }

    #[test]
    fn confirm_close_round_trips_under_its_written_names() {
        for (mode, name) in [
            (ConfirmClose::Never, "never"),
            (ConfirmClose::WhenBusy, "when-busy"),
            (ConfirmClose::Always, "always"),
        ] {
            let cfg = Config {
                confirm_close: mode,
                ..Config::default()
            };
            let json = serde_json::to_value(&cfg).unwrap();
            assert_eq!(json["confirm_close"], name);
            let back: Config = serde_json::from_value(json).unwrap();
            assert_eq!(back.confirm_close, mode);
        }
    }
    /// The upgrade path, which is the whole reason `link_file_open` is an
    /// `Option`: whichever way a config written before this key existed is
    /// read, it has to keep doing what it did yesterday.
    #[test]
    fn link_file_open_takes_its_default_from_the_config_it_upgrades() {
        let mut had_a_command: Config =
            serde_json::from_str(r#"{"link_file_command":"code --goto {path}"}"#).unwrap();
        had_a_command.sanitize();
        assert_eq!(
            had_a_command.file_open_mode(),
            LinkFileOpen::Command,
            "someone who pointed this at their own editor asked for it and keeps it"
        );
        assert_eq!(
            had_a_command.link_file_command.as_deref(),
            Some("code --goto {path}")
        );

        let mut had_none: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        had_none.sanitize();
        assert_eq!(
            had_none.file_open_mode(),
            LinkFileOpen::System,
            "a file link opens with the program the OS has it associated with"
        );

        // A command that only ever held whitespace is no command at all —
        // `sanitize` drops it, and the mode must not be decided on the corpse.
        let mut blank: Config = serde_json::from_str(r#"{"link_file_command":"   "}"#).unwrap();
        blank.sanitize();
        assert_eq!(blank.link_file_command, None);
        assert_eq!(blank.file_open_mode(), LinkFileOpen::System);

        let mut chosen: Config =
            serde_json::from_str(r#"{"link_file_open":"system","link_file_command":"code"}"#)
                .unwrap();
        chosen.sanitize();
        assert_eq!(
            chosen.file_open_mode(),
            LinkFileOpen::System,
            "an explicit choice outranks whatever the command box holds"
        );

        let json = serde_json::to_string(&chosen).unwrap();
        assert!(json.contains("\"link_file_open\":\"system\""), "persisted");
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.file_open_mode(), LinkFileOpen::System);

        // Unknown values, including the retired built-in editor, fall back
        // rather than refusing the whole file.
        let garbage: Config = serde_json::from_str(r#"{"link_file_open":"emacs"}"#).unwrap();
        assert_eq!(garbage.file_open_mode(), LinkFileOpen::System);
        let retired: Config = serde_json::from_str(r#"{"link_file_open":"internal"}"#).unwrap();
        assert_eq!(retired.file_open_mode(), LinkFileOpen::System);
    }
    #[test]
    fn theme_follow_system_defaults_and_round_trips() {
        assert!(Config::default().theme_follow_system);

        // A file written before the key existed follows the system. One that
        // says false still does not.
        let cfg: Config = serde_json::from_str(r#"{"theme_preset":"dracula"}"#).unwrap();
        assert!(cfg.theme_follow_system);
        assert_eq!(cfg.theme_preset_light, "light");
        assert_eq!(cfg.theme_preset_dark, "dark");
        assert_eq!(cfg.theme_preset, "dracula");
        // The palette rescue defaults on, and survives a round trip either way.
        assert!(cfg.theme_legible_palette);

        let off: Config = serde_json::from_str(r#"{"theme_follow_system":false}"#).unwrap();
        assert!(!off.theme_follow_system);
        let json = serde_json::to_string(&off).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert!(!back.theme_follow_system);

        let cfg = Config {
            theme_preset_light: "one_light".to_string(),
            theme_preset_dark: "dracula".to_string(),
            ..Config::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert!(back.theme_follow_system);
        assert_eq!(back.theme_preset_light, "one_light");
        assert_eq!(back.theme_preset_dark, "dracula");

        let off: Config = serde_json::from_str(r#"{"theme_legible_palette":false}"#).unwrap();
        assert!(!off.theme_legible_palette);
        let json = serde_json::to_string(&off).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert!(!back.theme_legible_palette);
    }

    #[test]
    fn font_features_are_optional_and_parse_as_gpui_features() {
        let cfg: Config =
            serde_json::from_str(r#"{"font_features":{"calt":true,"liga":1}}"#).unwrap();
        let features = cfg.font_features.expect("font features should parse");
        assert!(
            features
                .tag_value_list()
                .iter()
                .any(|(tag, value)| tag == "liga" && *value == 1)
        );

        let default_cfg = Config::default();
        assert!(default_cfg.font_features.is_none());
    }

    #[test]
    fn font_thicken_defaults_on_and_reads_an_explicit_off() {
        // On is what tty7 drew before the key existed, so a config that never
        // names it must keep rendering exactly as it did.
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.font_thicken);
        assert!(Config::default().font_thicken);

        let cfg: Config = serde_json::from_str(r#"{"font_thicken":false}"#).unwrap();
        assert!(!cfg.font_thicken);
    }

    #[test]
    fn font_features_round_trip_to_integer_valued_json() {
        let features: FontFeatures =
            serde_json::from_str(r#"{"calt":true,"liga":1,"ss01":0,"zero":false}"#).unwrap();
        assert_eq!(
            features.tag_value_list(),
            &[
                ("calt".to_string(), 1),
                ("liga".to_string(), 1),
                ("ss01".to_string(), 0),
                ("zero".to_string(), 0),
            ]
        );

        let json = serde_json::to_string(&features).unwrap();
        assert_eq!(json, r#"{"calt":1,"liga":1,"ss01":0,"zero":0}"#);
        let back: FontFeatures = serde_json::from_str(&json).unwrap();
        assert_eq!(back, features);
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    #[test]
    fn font_features_skip_bad_tags_and_values_instead_of_failing() {
        let features: FontFeatures = serde_json::from_str(
            r#"{"toolong":1,"ss":1,"calt":true,"liga":null,"kern":-1,"dlig":1.5}"#,
        )
        .unwrap();
        assert_eq!(features.tag_value_list(), &[("calt".to_string(), 1)]);
    }

    #[test]
    fn font_features_survive_a_config_round_trip() {
        let cfg: Config =
            serde_json::from_str(r#"{"font_features":{"calt":true,"liga":1}}"#).unwrap();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.font_features, cfg.font_features);
        assert_eq!(
            back.font_features.as_ref().map(|f| f.tag_value_list()),
            Some(&[("calt".to_string(), 1), ("liga".to_string(), 1)][..])
        );
    }

    #[test]
    fn stale_override_keys_are_ignored() {
        let cfg: Config = serde_json::from_str(
            r##"{"font_size": 20.0, "colors": {"border": "#fff"}, "ansi_colors": {"color1": "#f00"}, "sidebar_width": 400, "mobile_access": true, "editor_lsp": false, "update_channel": "nightly", "show_tray_icon": true}"##,
        )
        .expect("retired workbench keys must be ignored");
        assert_eq!(cfg.font_size, 20.0);
        assert_eq!(cfg.theme_preset, "light");
    }

    #[test]
    fn sanitize_clamps_degenerate_font_metrics() {
        let sanitized = |font_size: f32, line_height: f32| {
            let mut cfg = Config {
                font_size,
                line_height,
                ..Config::default()
            };
            cfg.sanitize();
            (cfg.font_size, cfg.line_height)
        };

        let (fs, lh) = sanitized(0.0, 0.0);
        assert!(fs >= 4.0, "font_size clamped above zero");
        assert!(lh >= 0.5, "line_height clamped above zero");

        let (fs, lh) = sanitized(f32::NAN, f32::INFINITY);
        assert!(fs.is_finite() && fs > 0.0);
        assert!(lh.is_finite() && lh > 0.0);

        assert_eq!(sanitized(15.0, 1.4), (15.0, 1.4));
    }

    #[test]
    fn sanitize_clamps_to_the_same_bounds_the_gui_steps_within() {
        // The GUI used to clamp to its own, narrower pair, so a config-legal
        // value landed outside the stepper's range and one click pushed it the
        // wrong way. The bounds are one shared set now: `Tty7App::set_font_size`
        // and `set_line_height` clamp to these very constants, which is what
        // makes the steppers and the Ctrl+=/Ctrl+- keys agree with the file
        // (#550).
        //
        // The numbers themselves are the supported range, so pin them: narrowing
        // either side is a behaviour change, not a refactor.
        assert_eq!((FONT_SIZE_MIN, FONT_SIZE_MAX), (4.0, 256.0));
        assert_eq!((LINE_HEIGHT_MIN, LINE_HEIGHT_MAX), (0.5, 4.0));

        // A value at either edge survives sanitize unchanged, so the stepper
        // has somewhere to stop rather than a value that keeps being rewritten.
        let mut cfg = Config {
            font_size: FONT_SIZE_MIN,
            line_height: LINE_HEIGHT_MAX,
            ..Config::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.font_size, FONT_SIZE_MIN);
        assert_eq!(cfg.line_height, LINE_HEIGHT_MAX);

        // And a value past an edge lands *on* that edge — the direction the
        // step was going — rather than anywhere else.
        let mut cfg = Config {
            font_size: 1_000.0,
            line_height: 0.1,
            ..Config::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.font_size, FONT_SIZE_MAX, "over the top clamps down");
        assert_eq!(
            cfg.line_height, LINE_HEIGHT_MIN,
            "under the floor clamps up"
        );

        // The values the issue was reported with: both are legal, so sanitize
        // leaves them alone, and the steppers now step from where they are.
        let mut cfg = Config {
            font_size: 50.0,
            line_height: 3.0,
            ..Config::default()
        };
        cfg.sanitize();
        assert_eq!((cfg.font_size, cfg.line_height), (50.0, 3.0));
        // Both are inside the range the steppers clamp to, which is the whole
        // point: `50 + 1` and `3.0 - 0.05` are legal, so neither click can be
        // turned around by a clamp.
        const { assert!(50.0 + 1.0 <= FONT_SIZE_MAX && 3.0 - 0.05 >= LINE_HEIGHT_MIN) };
    }

    #[test]
    fn ui_font_family_defaults_to_none_and_sanitizes_blank_to_none() {
        let cfg: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert_eq!(cfg.ui_font_family, None);

        let mut cfg: Config = serde_json::from_str(r#"{"ui_font_family": "  "}"#).unwrap();
        cfg.sanitize();
        assert_eq!(cfg.ui_font_family, None);

        let mut cfg: Config = serde_json::from_str(r#"{"ui_font_family": "Inter"}"#).unwrap();
        cfg.sanitize();
        assert_eq!(cfg.ui_font_family, Some("Inter".to_string()));
    }

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("tty7-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_writes_through_a_symlink_instead_of_replacing_it() {
        let dir = TestDir::new("symlink");
        let real = dir.path().join("dotfiles-config.json");
        std::fs::write(&real, b"old").unwrap();
        let link = dir.path().join("config.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "config.json must stay a symlink"
        );
        assert_eq!(std::fs::read(&real).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_follows_relative_and_dangling_symlinks() {
        let dir = TestDir::new("symlink-relative");
        std::fs::create_dir(dir.path().join("dotfiles")).unwrap();
        let link = dir.path().join("config.json");
        // Relative to the link's own directory, and not created yet.
        std::os::unix::fs::symlink("dotfiles/config.json", &link).unwrap();
        write_atomic(&link, b"fresh").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(dir.path().join("dotfiles/config.json")).unwrap(),
            b"fresh"
        );
        let leftover: Vec<_> = std::fs::read_dir(dir.path().join("dotfiles"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp file should be renamed away");
    }

    #[test]
    fn write_atomic_replaces_contents_and_leaves_no_temp() {
        let dir = TestDir::new("atomic");
        let target = dir.path().join("data.json");
        write_atomic(&target, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");
        write_atomic(&target, b"second-longer-and-then-short").unwrap();
        write_atomic(&target, b"3rd").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "3rd");
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp file should be renamed away");
    }
    #[test]
    fn behavior_enums_fall_back_leniently_on_bad_values() {
        let cfg: Config = serde_json::from_str(
            r#"{"font_size": 20.0, "startup_mode": "middle", "notify_on_command_finish": "sometimes", "bell": "loud"}"#,
        )
        .expect("a bad enum value must not fail the whole parse");
        assert_eq!(cfg.font_size, 20.0);
        assert_eq!(cfg.startup_mode, StartupMode::Normal);
        assert_eq!(cfg.notify_on_command_finish, NotifyMode::Unfocused);
        assert_eq!(cfg.bell, BellMode::Visual);

        let cfg: Config = serde_json::from_str(
            r#"{"startup_mode": "fullscreen", "notify_on_command_finish": "always", "bell": "audible"}"#,
        )
        .unwrap();
        assert_eq!(cfg.startup_mode, StartupMode::Fullscreen);
        assert_eq!(cfg.notify_on_command_finish, NotifyMode::Always);
        assert_eq!(cfg.bell, BellMode::Audible);
    }

    #[test]
    fn working_directory_defaults_to_inherit_and_parses_kebab() {
        let cfg = Config::default();
        assert_eq!(cfg.working_directory.strategy, WdStrategy::Inherit);
        assert!(cfg.working_directory.path.is_empty());

        let cfg: Config = serde_json::from_str(
            r#"{"working_directory": {"strategy": "custom", "path": "/tmp/x"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.working_directory.strategy, WdStrategy::Custom);
        assert_eq!(cfg.working_directory.path, "/tmp/x");

        let cfg: Config =
            serde_json::from_str(r#"{"working_directory": {"strategy": "elsewhere"}}"#).unwrap();
        assert_eq!(cfg.working_directory.strategy, WdStrategy::Inherit);
    }

    #[test]
    fn sanitize_clamps_scroll_multiplier_into_band() {
        let clamp = |m: f32| {
            let mut cfg = Config {
                mouse_scroll_multiplier: m,
                ..Config::default()
            };
            cfg.sanitize();
            cfg.mouse_scroll_multiplier
        };
        assert_eq!(clamp(1.0), 1.0);
        assert_eq!(clamp(0.0), 1.0);
        assert_eq!(clamp(-3.0), 1.0);
        assert_eq!(clamp(100.0), 10.0);
        assert_eq!(clamp(0.01), 0.1);
    }
    #[test]
    fn sanitize_clamps_window_opacity_override() {
        let clamp = |o: Option<f32>| {
            let mut cfg = Config {
                window_opacity: o,
                ..Config::default()
            };
            cfg.sanitize();
            cfg.window_opacity
        };
        assert_eq!(clamp(None), None);
        assert_eq!(clamp(Some(0.8)), Some(0.8));
        assert_eq!(clamp(Some(0.0)), Some(0.2));
        assert_eq!(clamp(Some(2.0)), Some(1.0));
        assert_eq!(clamp(Some(f32::NAN)), None);
    }

    #[test]
    fn window_backdrop_defaults_and_round_trips_leniently() {
        let cfg = Config::default();
        assert_eq!(cfg.window_backdrop, WindowBackdrop::Auto);

        let text = serde_json::to_string(&Config {
            window_backdrop: WindowBackdrop::MicaAlt,
            ..Config::default()
        })
        .unwrap();
        assert!(text.contains("\"window_backdrop\":\"mica-alt\""));

        let restored: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(restored.window_backdrop, WindowBackdrop::MicaAlt);

        let blur: Config = serde_json::from_str(r#"{"window_backdrop":"blur"}"#).unwrap();
        assert_eq!(blur.window_backdrop, WindowBackdrop::Blur);

        // Unknown values fall back to Auto instead of rejecting the whole config.
        let lenient: Config = serde_json::from_str(r#"{"window_backdrop":"nope"}"#).unwrap();
        assert_eq!(lenient.window_backdrop, WindowBackdrop::Auto);
    }

    #[test]
    fn sanitize_clamps_scrollback_into_band() {
        let clamp = |n: usize| {
            let mut cfg = Config {
                scrollback_limit: n,
                ..Config::default()
            };
            cfg.sanitize();
            cfg.scrollback_limit
        };
        assert_eq!(clamp(0), 100);
        assert_eq!(clamp(10_000), 10_000);
        assert_eq!(clamp(usize::MAX), MAX_SCROLLBACK);
    }
    #[test]
    fn new_terminal_prefs_default_and_parse_leniently() {
        let cfg = Config::default();
        assert!(cfg.mouse_reporting);
        assert_eq!(cfg.notify_threshold_secs, 10);
        assert_eq!(cfg.bell, BellMode::Visual);
        assert_eq!(cfg.startup_mode, StartupMode::Normal);

        let cfg: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert!(cfg.mouse_reporting);
        assert_eq!(cfg.font_size, 15.0);
        assert_eq!(cfg.notify_threshold_secs, 10);
        assert_eq!(cfg.bell, BellMode::Visual);

        // Keys from the removed prompt editor stay readable so an old
        // config.json does not fail to load.
        let cfg: Config = serde_json::from_str(
            r#"{"prompt_editor":false,"tab_completion":false,"history_search":false,"font_size":19.0}"#,
        )
        .unwrap();
        assert_eq!(cfg.font_size, 19.0);
        assert!(cfg.mouse_reporting);

        let cfg: Config =
            serde_json::from_str(r#"{"mouse_reporting": false, "bell": "audible"}"#).unwrap();
        assert!(!cfg.mouse_reporting);
        assert_eq!(cfg.bell, BellMode::Audible);

        let cfg: Config = serde_json::from_str(r#"{"bell": "both"}"#).unwrap();
        assert_eq!(cfg.bell, BellMode::Both);

        let cfg: Config = serde_json::from_str(r#"{"bell": "loud"}"#).unwrap();
        assert_eq!(cfg.bell, BellMode::Visual);

        assert_eq!(serde_json::to_string(&BellMode::Both).unwrap(), "\"both\"");
    }

    /// #668: a config written on a Mac travels to a Linux box, where the
    /// platform modifier is a different key — so the *choice* is stored, not
    /// the key it resolves to. Unset, or unknown, the wheel does not zoom.
    #[test]
    fn the_zoom_modifier_round_trips_and_falls_back() {
        let cfg: Config = serde_json::from_str(r#"{"mouse_zoom_modifier": "none"}"#).unwrap();
        assert_eq!(cfg.mouse_zoom_modifier, MouseZoomModifier::None);

        let cfg: Config = serde_json::from_str(r#"{"mouse_zoom_modifier": "alt"}"#).unwrap();
        assert_eq!(cfg.mouse_zoom_modifier, MouseZoomModifier::Alt);

        let cfg: Config = serde_json::from_str(r#"{"mouse_zoom_modifier": "platform"}"#).unwrap();
        assert_eq!(cfg.mouse_zoom_modifier, MouseZoomModifier::Platform);

        let cfg: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert_eq!(cfg.mouse_zoom_modifier, MouseZoomModifier::None);

        let cfg: Config = serde_json::from_str(r#"{"mouse_zoom_modifier": "meta"}"#).unwrap();
        assert_eq!(cfg.mouse_zoom_modifier, MouseZoomModifier::None);

        assert_eq!(
            serde_json::to_string(&MouseZoomModifier::None).unwrap(),
            "\"none\""
        );
    }

    #[test]
    fn sanitize_clamps_notify_threshold_into_band() {
        let clamp = |n: u64| {
            let mut cfg = Config {
                notify_threshold_secs: n,
                ..Config::default()
            };
            cfg.sanitize();
            cfg.notify_threshold_secs
        };
        assert_eq!(clamp(0), 1);
        assert_eq!(clamp(10), 10);
        assert_eq!(clamp(100_000), 3600);
    }
    #[test]
    fn gui_language_defaults_to_english_and_rejects_unsupported_values() {
        let cfg = Config::default();
        assert_eq!(cfg.gui_language, "en");

        let cfg: Config = serde_json::from_str(r#"{"gui_language": "zh-CN"}"#).unwrap();
        assert_eq!(cfg.gui_language, "zh-CN");

        let cfg: Config = serde_json::from_str(r#"{"gui_language": "ja-JP"}"#).unwrap();
        assert_eq!(cfg.gui_language, "ja-JP");

        let cfg: Config = serde_json::from_str(r#"{"gui_language": "ru-RU"}"#).unwrap();
        assert_eq!(cfg.gui_language, "ru-RU");

        let mut cfg: Config = serde_json::from_str(r#"{"gui_language": "ko"}"#).unwrap();
        cfg.sanitize();
        assert_eq!(cfg.gui_language, "en");
    }

    #[test]
    fn keybinding_preset_and_prefix_default_and_round_trip() {
        let cfg = Config::default();
        assert_eq!(cfg.keybinding_preset, "default");
        assert_eq!(cfg.prefix, "ctrl-b");

        let cfg: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert_eq!(cfg.keybinding_preset, "default");
        assert_eq!(cfg.prefix, "ctrl-b");

        let cfg: Config =
            serde_json::from_str(r#"{"keybinding_preset": "tmux", "prefix": "ctrl-a"}"#).unwrap();
        assert_eq!(cfg.keybinding_preset, "tmux");
        assert_eq!(cfg.prefix, "ctrl-a");
    }

    #[test]
    fn default_font_fallbacks_are_platform_appropriate() {
        let defaults = default_font_fallbacks();
        assert!(!defaults.is_empty());

        for name in platform_last_resort_fallbacks() {
            assert!(
                defaults.iter().any(|f| f == name),
                "default chain {defaults:?} omits stock face {name}"
            );
        }

        let maple = defaults.iter().position(|f| f == "Maple Mono NF CN");
        let maple = maple.expect("the exact-fit CJK face must stay in the chain");
        for name in platform_last_resort_fallbacks() {
            let stock = defaults.iter().position(|f| f == name).unwrap();
            assert!(maple < stock, "{name} must not preempt Maple Mono NF CN");
        }

        if !cfg!(target_os = "macos") {
            for name in ["Menlo", "Apple Color Emoji"] {
                assert!(
                    !defaults.iter().any(|f| f == name),
                    "{name} ships only with macOS"
                );
            }
        }
    }

    #[test]
    fn config_deserialize_fills_missing_fields_from_defaults() {
        let cfg: Config = serde_json::from_str(r#"{"font_size": 20.0}"#).unwrap();
        assert_eq!(cfg.font_size, 20.0);
        assert_eq!(cfg.line_height, 1.4);
        assert_eq!(cfg.font_family, "Hack");
        assert_eq!(cfg.theme_preset, "light");
        assert!(cfg.keybindings.is_empty());
    }

    #[test]
    fn keybindings_take_a_chord_or_a_list_and_write_back_what_they_read() {
        // A string is the shape every config written before #868 has, from the
        // Settings page and by hand; a list is the one that replaces an
        // action's chords outright. Both have to load, and a save must not turn
        // one into the other — the two mean different things.
        let written = serde_json::json!({
            "NextTab": "cmd-shift-]",
            "AlternatePaste": "",
            "PrevTab": ["cmd-shift-[", "ctrl-shift-tab"],
            "SplitRight": [],
        });
        let cfg: Config =
            serde_json::from_value(serde_json::json!({ "keybindings": written.clone() }))
                .expect("both shapes load");
        assert_eq!(cfg.keybindings.len(), 4);
        assert_eq!(serde_json::to_value(&cfg.keybindings).unwrap(), written);
    }

    #[test]
    fn a_keybinding_line_that_cannot_be_read_does_not_take_the_config_with_it() {
        // #901: one unreadable line used to fail the whole `Config`. The loader
        // then quarantines config.json and starts on built-in defaults, so
        // every rebinding in the file — the point of the file — came back as
        // the shipped default, and the next settings write made that permanent.
        let cfg: Config = serde_json::from_str(
            r#"{"font_size": 20.0,
                "keybindings": {"ActivateTab1": null, "ActivateTab2": 2,
                                "ActivateTab3": {"key": "alt-shift-3"},
                                "ActivateTab4": [], "NextTab": "ctrl-alt-]"}}"#,
        )
        .expect("a bad keybinding line must not fail the file");
        assert_eq!(cfg.font_size, 20.0, "the rest of the file still loads");
        assert_eq!(
            serde_json::to_value(&cfg.keybindings).unwrap(),
            serde_json::json!({"ActivateTab4": [], "NextTab": "ctrl-alt-]"}),
            "the readable lines survive and the unreadable ones are dropped"
        );

        // And a `keybindings` that is not a map at all is no reason to hand
        // the user back default fonts, themes and everything else.
        let odd: Config = serde_json::from_str(r#"{"font_size": 20.0, "keybindings": []}"#)
            .expect("a keybindings of the wrong shape must not fail the file");
        assert_eq!(odd.font_size, 20.0);
        assert!(odd.keybindings.is_empty());
    }

    fn pin_config_dir() {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        set_config_dir(dir);
    }

    static CONFIG_FILE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_config_file() -> std::sync::MutexGuard<'static, ()> {
        CONFIG_FILE.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_missing_config_file_is_written_before_it_is_opened() {
        let _guard = lock_config_file();
        pin_config_dir();
        let path = config_path("config.json").expect("the pinned dir has a config path");
        std::fs::remove_file(&path).ok();
        assert!(!path.exists());

        let cfg = Config {
            font_size: 19.0,
            ..Config::default()
        };
        let written = cfg
            .ensure_config_file_exists()
            .expect("the pinned dir is available");
        assert_eq!(written, path);
        let back = Config::load();
        assert_eq!(back.font_size, 19.0);
        assert!(back.theme_follow_system);

        // A file that is already there is the user's. Opening it must not
        // overwrite what they wrote.
        std::fs::write(&path, r#"{"font_size": 21.0}"#).unwrap();
        let again = Config::default()
            .ensure_config_file_exists()
            .expect("still available");
        assert_eq!(again, path);
        let kept: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(kept.font_size, 21.0);
    }

    #[test]
    fn save_load_and_shell_command_round_trip_through_disk() {
        let _guard = lock_config_file();
        pin_config_dir();
        let mut cfg = Config {
            font_size: 18.0,
            ..Config::default()
        };
        cfg.shell = Some(ShellConfig {
            program: "fish".to_string(),
            args: vec!["-l".to_string()],
        });
        cfg.save();

        let loaded = Config::load();
        assert_eq!(loaded.font_size, 18.0);
        assert_eq!(
            loaded.shell.as_ref().map(|s| s.program.as_str()),
            Some("fish")
        );
        let (program, args) = shell_command().expect("shell override present");
        assert_eq!(program, "fish");
        assert_eq!(args, vec!["-l".to_string()]);
    }

    #[test]
    fn a_utf8_bom_does_not_silently_reset_the_config() {
        let _guard = lock_config_file();
        pin_config_dir();
        let path = Config::path().expect("pinned config dir");
        let text = "\u{FEFF}{\"font_size\": 21.0, \"restore_session\": false}";
        write_atomic(&path, text.as_bytes()).unwrap();

        let loaded = Config::load();
        assert_eq!(loaded.font_size, 21.0);
        assert!(
            loaded.mouse_reporting,
            "a retired key beside the marker must not disturb the rest"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_corrupt_config_is_kept_aside_and_never_overwritten() {
        let _guard = lock_config_file();
        pin_config_dir();
        let path = Config::path().expect("pinned config dir");
        let aside = path.with_extension("json.corrupt");
        clear_quarantines(&path);
        std::fs::write(&path, "{ not json").unwrap();

        let (loaded, outcome) = Config::load_with_outcome();
        assert_eq!(outcome, LoadOutcome::Quarantined);
        assert!(loaded.quarantined, "the stand-in must say what it is");
        assert_eq!(
            std::fs::read_to_string(&aside).as_deref().ok(),
            Some("{ not json"),
            "the next save overwrites config.json wholesale, so the old \
             contents must already be parked beside it"
        );

        // The save every sidebar drag issues must not turn one typo into
        // permanent loss: a quarantined config refuses to write, and the file
        // on disk stays exactly the user's own.
        loaded.save();
        assert_eq!(
            std::fs::read_to_string(&path).as_deref().ok(),
            Some("{ not json"),
            "a quarantined config must never overwrite the file it stood in for"
        );

        // Fixing the file is what re-arms writes.
        std::fs::write(&path, r#"{"font_size": 19.0}"#).unwrap();
        let (fixed, outcome) = Config::load_with_outcome();
        assert_eq!(outcome, LoadOutcome::Parsed);
        assert!(!fixed.quarantined);
        fixed.save();
        assert!(std::fs::read_to_string(&path).unwrap().contains("19.0"));

        std::fs::remove_file(&path).ok();
        clear_quarantines(&path);
    }

    #[test]
    fn a_config_that_stays_broken_is_parked_once_not_once_per_read() {
        let _guard = lock_config_file();
        pin_config_dir();
        let path = Config::path().expect("pinned config dir");
        clear_quarantines(&path);
        std::fs::write(&path, "{ not json").unwrap();

        // `Config::load` runs on every pane spawn and every palette command,
        // so a file left broken is read dozens of times a session. Each read
        // used to leave another copy, filling the config directory and then
        // overwriting the oldest one.
        for _ in 0..12 {
            let _ = Config::load();
        }
        let parked: Vec<_> = quarantine_candidates(&path)
            .filter(|candidate| candidate.exists())
            .collect();
        assert_eq!(
            parked.len(),
            1,
            "one broken file, one copy — found {parked:?}"
        );

        // A *different* broken version is still worth keeping.
        std::fs::write(&path, "{ also not json").unwrap();
        let _ = Config::load();
        let parked: Vec<_> = quarantine_candidates(&path)
            .filter(|candidate| candidate.exists())
            .collect();
        assert_eq!(parked.len(), 2, "found {parked:?}");
        assert_eq!(
            std::fs::read_to_string(&parked[0]).unwrap(),
            "{ not json",
            "the first rescue copy is still the first one"
        );

        std::fs::remove_file(&path).ok();
        clear_quarantines(&path);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_config_suppresses_writes_without_parking_a_copy() {
        use std::os::unix::fs::PermissionsExt as _;

        let _guard = lock_config_file();
        pin_config_dir();
        let path = Config::path().expect("pinned config dir");
        clear_quarantines(&path);
        std::fs::write(&path, r#"{"font_size": 21.0}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_to_string(&path).is_ok() {
            std::fs::remove_file(&path).ok();
            return;
        }

        let (loaded, outcome) = Config::load_with_outcome();
        // Not `Quarantined`: nothing was parked, so the notification must not
        // point at a `config.json.corrupt` that was never written.
        assert_eq!(outcome, LoadOutcome::Unreadable);
        assert!(outcome.failed());
        assert!(
            loaded.quarantined,
            "what cannot be read must not be written"
        );
        assert!(
            quarantine_candidates(&path).all(|candidate| !candidate.exists()),
            "there was nothing readable to copy"
        );

        loaded.save();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"font_size": 21.0}"#,
            "the file the app could not read is the file the user still has"
        );
        std::fs::remove_file(&path).ok();
    }

    fn clear_quarantines(path: &std::path::Path) {
        for candidate in quarantine_candidates(path) {
            std::fs::remove_file(candidate).ok();
        }
    }

    #[test]
    fn a_missing_config_file_is_absent_not_quarantined() {
        let _guard = lock_config_file();
        pin_config_dir();
        let path = Config::path().expect("pinned config dir");
        std::fs::remove_file(&path).ok();

        let (loaded, outcome) = Config::load_with_outcome();
        assert_eq!(outcome, LoadOutcome::Absent);
        // A first run saves its defaults without anyone calling that loss.
        assert!(!loaded.quarantined);
    }

    #[test]
    fn the_quarantined_flag_never_reaches_disk() {
        let cfg = Config {
            quarantined: true,
            ..Config::default()
        };
        let text = serde_json::to_string(&cfg).unwrap();
        assert!(!text.contains("quarantined"));
        // And a hand-written `"quarantined": true` in the file does not
        // suppress saves either — the flag belongs to the loader, not the file.
        let parsed: Config =
            serde_json::from_str(r#"{"quarantined": true, "font_size": 20.0}"#).unwrap();
        assert!(!parsed.quarantined);
        assert_eq!(parsed.font_size, 20.0);
    }

    #[test]
    fn strip_bom_only_removes_a_leading_marker() {
        assert_eq!(strip_bom("{}"), "{}");
        assert_eq!(strip_bom("\u{FEFF}{}"), "{}");
        assert_eq!(strip_bom("\u{FEFF}\u{FEFF}{}"), "\u{FEFF}{}");
        let inner = "{\"tab_title\":\"\u{FEFF}\"}";
        assert_eq!(strip_bom(inner), inner);
        assert_eq!(strip_bom(""), "");
    }

    #[test]
    fn a_retired_custom_shells_key_does_not_fail_the_rest_of_the_config() {
        let cfg: Config = serde_json::from_str(
            r#"{"font_size": 15.0, "custom_shells":[{"label":"Ubuntu","program":"wsl.exe"}]}"#,
        )
        .expect("an unknown key must not fail the whole config parse");
        assert_eq!(cfg.font_size, 15.0);
    }

    #[test]
    fn config_path_resolves_under_the_pinned_dir() {
        pin_config_dir();
        let p = config_path("config.json").expect("config path resolves");
        assert!(p.ends_with("config.json"));
        assert_eq!(p.parent(), config_dir_path().as_deref());
    }

    #[test]
    fn config_dir_resolution_prefers_explicit_over_portable_over_default() {
        let dir = |name: &str| Some(PathBuf::from(name));
        let default = || dir("default");

        assert_eq!(
            resolve_config_dir(dir("flag"), dir("env"), dir("portable"), default),
            dir("flag"),
            "--config-dir wins over everything"
        );
        assert_eq!(
            resolve_config_dir(None, dir("env"), dir("portable"), default),
            dir("env"),
            "$NTTY7_CONFIG_DIR still redirects a portable copy"
        );
        assert_eq!(
            resolve_config_dir(None, None, dir("portable"), default),
            dir("portable"),
            "a portable install keeps its data beside the executables"
        );
        assert_eq!(
            resolve_config_dir(None, None, None, default),
            dir("default")
        );
        assert_eq!(resolve_config_dir(None, None, None, || None), None);
    }

    #[test]
    fn config_env_reads_only_ntty7_config_dir() {
        let ntty7 = PathBuf::from("/tmp/ntty7-cfg");
        assert_eq!(env_config_dir_from(Some(ntty7.clone())), Some(ntty7));
        assert_eq!(env_config_dir_from(Some(PathBuf::new())), None);
        assert_eq!(env_config_dir_from(None), None);
    }

    #[test]
    fn portable_data_needs_both_the_marker_and_the_folder() {
        let exe_dir = tempfile::tempdir().unwrap();
        let data = exe_dir.path().join(PORTABLE_DATA_DIR);

        assert_eq!(portable_data_dir(exe_dir.path()), None, "a plain directory");

        // Every portable ZIP ships the marker for the updater, so the marker
        // alone must not relocate an existing portable user's data.
        std::fs::write(exe_dir.path().join(PORTABLE_MARKER), "portable-v1").unwrap();
        assert_eq!(portable_data_dir(exe_dir.path()), None, "marker only");

        std::fs::create_dir(&data).unwrap();
        assert_eq!(portable_data_dir(exe_dir.path()), Some(data.clone()));

        // A `data` folder without the marker — an installer layout, or any
        // directory someone ran a build from — is not a portable install.
        std::fs::remove_file(exe_dir.path().join(PORTABLE_MARKER)).unwrap();
        assert_eq!(portable_data_dir(exe_dir.path()), None, "folder only");
    }

    #[test]
    fn a_file_named_data_is_not_a_portable_data_folder() {
        let exe_dir = tempfile::tempdir().unwrap();
        std::fs::write(exe_dir.path().join(PORTABLE_MARKER), "portable-v1").unwrap();
        std::fs::write(exe_dir.path().join(PORTABLE_DATA_DIR), "").unwrap();
        assert_eq!(portable_data_dir(exe_dir.path()), None);
    }

    #[test]
    fn launch_overrides_teach_the_daemon_their_wrappers_as_the_file_changes() {
        let _guard = lock_config_file();
        pin_config_dir();
        let mut cfg = Config::default();
        cfg.agent_launch
            .insert("claude".to_string(), "cc --fast".to_string());
        cfg.save();
        let aliases = agent_detection_aliases();
        assert_eq!(aliases.get("cc").map(String::as_str), Some("claude"));

        // An edit made while the daemon runs is picked up on the next read,
        // and an explicit `agent_commands` entry outranks an implied one.
        cfg.agent_launch
            .insert("codex".to_string(), "/opt/bin/cx".to_string());
        cfg.agent_commands
            .insert("CC".to_string(), "gemini".to_string());
        cfg.save();
        let aliases = agent_detection_aliases();
        assert_eq!(aliases.get("cc").map(String::as_str), Some("gemini"));
        assert_eq!(aliases.get("cx").map(String::as_str), Some("codex"));

        let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.agent_launch, cfg.agent_launch);
        Config::default().save();
    }

    #[test]
    fn prompt_cursor_style_defaults_to_following_cursor_style() {
        assert_eq!(
            Config::default().prompt_cursor_style,
            PromptCursorStyle::Follow
        );
        assert_eq!(PromptCursorStyle::Follow.shape(), None);

        let cfg: Config =
            serde_json::from_str(r#"{"cursor_style": "block", "prompt_cursor_style": "bar"}"#)
                .unwrap();
        assert_eq!(cfg.cursor_style, CursorStyle::Block);
        assert_eq!(cfg.prompt_cursor_style.shape(), Some(CursorStyle::Bar));

        // A value this build does not know falls back rather than failing the
        // whole config.
        let cfg: Config = serde_json::from_str(r#"{"prompt_cursor_style": "beam"}"#).unwrap();
        assert_eq!(cfg.prompt_cursor_style, PromptCursorStyle::Follow);
    }
}
