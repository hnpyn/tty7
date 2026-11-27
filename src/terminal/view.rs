use alacritty_terminal::event::Event as AlacEvent;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use gpui::{
    App, ClipboardEntry, ClipboardItem, Context, ExternalPaths, FocusHandle, Focusable, Font,
    KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, Pixels, ScrollDelta, ScrollWheelEvent,
    Task, WeakEntity, Window, actions, div, prelude::*, px,
};
use gpui_component::kbd::Kbd;
use gpui_component::menu::ContextMenuExt;
use gpui_component::scroll::Scrollbar;
use gpui_component::{ActiveTheme as _, WindowExt as _, h_flex};

use super::TermSize;
use super::element::{GridSnapshot, RenderCell, TerminalElement};
use super::remote::RemoteTerminal;
use super::scrollbar::{GridScroll, TerminalScrollHandle};
use super::search::{LinkTarget, SearchState};
use crate::core::actions::{
    CloseWindow, CopyLinkPathUnderPointer, DecreaseFontSize, IncreaseFontSize, NewWindow,
    OpenLinkUnderPointer, OpenLinkWithDefaultApp, RevealLinkUnderPointer, SendBackTab, SendTab,
};
use crate::core::config::{BellMode, Config, LinkFileOpen, MouseZoomModifier};
use crate::core::shell_quote::quote_for_shell;
use crate::daemon::protocol::{RemoteContext, ShellSpec};
use crate::ui::i18n::{L10nKey, t, t_fmt};

mod copy_unwrap;

pub(super) const GRID_PAD_X: f32 = 8.;
pub(super) const GRID_PAD_Y: f32 = 4.;

actions!(
    terminal,
    [
        CopyText,
        CopyRaw,
        CutText,
        PasteText,
        AlternatePaste,
        SelectAll,
        UndoEdit,
        RedoEdit,
        FindInTerminal,
        FindNext,
        FindPrevious,
        ClearScrollback,
        InsertNewline,
        ToggleComposer,
    ]
);

pub struct ChildExited;

impl gpui::EventEmitter<ChildExited> for TerminalView {}

/// What a pane is called when nothing running in it has said otherwise.
pub(crate) const DEFAULT_TITLE: &str = "ntty7";

/// What a pane is *saying* about itself, if anything — the reading behind
/// [`TerminalView::stated_title`], split out so it can be pinned without a
/// live pane.
///
/// Anything but the placeholder counts. That is wider than "arrived over OSC
/// 0/2" on purpose: an SSH pane answers to the host it dialled and a workspace
/// pane to its workspace's name, and those are names tty7 gave the pane
/// deliberately (#438) rather than the absence of one. The literal string
/// `ntty7` is the only title that says nothing, because it is the app's own
/// name standing in for a pane that has never introduced itself.
#[cfg(test)]
pub(crate) fn stated_title(title: &str) -> Option<&str> {
    match title.trim() {
        "" => None,
        t if t == DEFAULT_TITLE => None,
        t => Some(t),
    }
}

pub struct ShellParts {
    terminal: RemoteTerminal,
    pub(crate) pane_id: u64,
    shell_spec: Option<ShellSpec>,
    /// Set when a configured shell failed and the login shell was used instead.
    pub(crate) shell_notice: Option<String>,
}

#[derive(Clone, Copy)]
struct DragScroll {
    overshoot: f32,
    col: usize,
    side: Side,
}

/// In-flight wheel animation. `remaining` is what is left to scroll, in lines,
/// relative to wherever the view happens to be — deliberately not an absolute
/// target, so output arriving mid-animation shifts the grid under us without
/// dragging the animation somewhere else.
#[derive(Clone, Copy)]
struct ScrollAnim {
    remaining: f32,
    last: std::time::Instant,
}

/// Fraction of the remaining distance consumed per [`SCROLL_ANIM_FRAME`].
const SCROLL_ANIM_SMOOTH: f32 = 0.4;
/// The frame `SCROLL_ANIM_SMOOTH` is calibrated against: one nominal 60 Hz
/// frame. Decay is scaled by real elapsed time, so the same feel holds at any
/// refresh rate.
const SCROLL_ANIM_FRAME: std::time::Duration = std::time::Duration::from_millis(16);
/// Below this much left to travel, land instead of asymptoting toward it. A
/// twentieth of a line is around a pixel — the tail of an exponential decay is
/// invisible long before it ends, and every frame of it costs a full repaint.
const SCROLL_ANIM_MIN: f32 = 0.05;
/// A jump smaller than this reads as continuous already; spreading it would
/// only put lag between the hand and the grid. Inching a wheel one detent at a
/// time lands here, and so does every event a trackpad sends.
const SCROLL_ANIM_MIN_JUMP: f32 = 1.0;
/// How long a trackpad gesture stays "live" after its last event. Long enough
/// to bridge the gaps in a momentum tail, short enough that reaching for the
/// wheel right after a swipe is not mistaken for more of the swipe.
const SCROLL_GESTURE_IDLE: std::time::Duration = std::time::Duration::from_millis(150);
/// How far a trackpad has to travel, in lines, to earn one font-size step while
/// the platform modifier is held. A wheel detent is one step on its own, so
/// this only ever applies to the continuous stream a gesture produces.
const ZOOM_SCROLL_LINES: f32 = 3.0;

fn cwd_is_on_host(pane_runs_remotely: bool, host_is_local: bool) -> bool {
    match pane_runs_remotely {
        false => host_is_local,
        true => !host_is_local,
    }
}

/// Which path dialect a pane's output is written in.
///
/// A pane running on this machine spells paths the way this OS does, and that
/// is the end of it: `/etc` printed by a `cmd.exe` pane sitting on `C:` means
/// `C:\etc`, exactly as `cd /etc` would there. Reading it as a rooted POSIX
/// path would underline a file this machine has not got, and a link that
/// cannot be opened is worse than no link.
///
/// A pane whose paths live somewhere else is asked instead — by the only thing
/// that host ever says about its own spelling, the directory it reports. A
/// `/`-rooted cwd is a POSIX host's. A pane that has not said where it is
/// falls to POSIX: there is no local drive to measure it from either way, and
/// every host tty7 installs a server on over SSH or WSL spells paths that way.
fn link_path_style(
    paths_are_local: bool,
    host_cwd: Option<&std::path::Path>,
) -> super::search::PathStyle {
    use super::search::PathStyle;
    match paths_are_local {
        true => PathStyle::NATIVE,
        false => host_cwd.map_or(PathStyle::Posix, PathStyle::of_dir),
    }
}

pub struct TerminalView {
    pub terminal: RemoteTerminal,
    host_id: crate::ui::host_ops::HostId,
    pub pane_id: u64,
    shell_spec: Option<ShellSpec>,
    pub focus_handle: FocusHandle,
    pub font: Font,
    pub font_bold: Option<Font>,
    pub font_italic: Option<Font>,
    font_features: Option<gpui::FontFeatures>,
    pub font_size: Pixels,
    pub line_height_mul: f32,
    pub cell_width: Pixels,
    pub(super) line_height: Pixels,
    /// What the grid's height leaves under its last whole row: rows are
    /// drawn from the top, so this is empty space above the bottom padding.
    pub(super) grid_slack: Pixels,
    /// The grid the last frame painted, and the snapshot that went with it.
    /// A frame that cannot have the terminal lock repaints this rather than
    /// waiting on the pane's reader — see [`TerminalElement::build_grid`].
    /// Owned per pane rather than kept in one shared scratch buffer, because
    /// what makes it reusable is that it is still the *previous frame of this
    /// pane* when the next one starts.
    pub(super) grid_buf: Vec<RenderCell>,
    pub(super) grid_snap: Option<GridSnapshot>,
    /// Terminal mode and selection as of the last frame that got the lock.
    /// What the *frame* declares — the keymap context it publishes, whether it
    /// draws a selection — is read from here, so drawing never queues behind
    /// the pane's reader for two bits it can be one frame late about.
    /// Everything with a decision to make (a keystroke asking whether a
    /// full-screen program owns the screen) still asks the terminal itself.
    frame_alt_screen: bool,
    frame_has_selection: bool,
    selecting: bool,
    drag_scroll: Option<DragScroll>,
    drag_scroll_epoch: u64,
    scroll_anim: Option<ScrollAnim>,
    scroll_anim_epoch: u64,
    gesture_until: Option<std::time::Instant>,
    /// Whether the trackpad gesture in flight is zooming, latched at its first
    /// event. `None` between gestures, and never set for a wheel, which has no
    /// gesture to belong to and decides notch by notch.
    gesture_zoom: Option<bool>,
    pub title: String,
    /// A title the pane has been told about but has not adopted yet — see
    /// `set_title_when_settled`. `None` means the tab is showing the newest
    /// title there is.
    pending_title: Option<String>,
    /// What the pane is called before anything running in it says otherwise —
    /// and what it goes back to when the program resets the title or the
    /// session ends.
    pub(super) default_title: String,
    pub marked_text: String,
    last_mouse_cell: Option<(usize, usize)>,
    /// The button whose press this pane reported to the application, until
    /// its release. The drag and release handlers listen on the whole window,
    /// so this is what keeps a drag over the title bar or the pane next door
    /// from reaching a program that never saw it begin (#1182).
    mouse_grab: Option<MouseButton>,
    /// Where the grid sat in the window on the last frame, so an event that
    /// arrives with nothing but a window position — the wheel — can be told
    /// which cell it is over.
    pub(super) cell_geom: Option<super::element::CellGeom>,
    /// The IME anchor this pane last asked the platform to place the
    /// candidate window at, while it had focus.
    ime_anchor: Option<gpui::Bounds<Pixels>>,
    last_hover_cell: Option<(usize, usize)>,
    link_modifier_down: bool,
    /// The repository the pane's directory sits in, once the host has been
    /// asked, and the directory that answer belongs to. A second root for
    /// relative paths: build output routinely names files from the workspace
    /// root while the shell sits in a member crate.
    link_repo_root: Option<(std::path::PathBuf, Option<std::path::PathBuf>)>,
    link_repo_root_pending: bool,
    /// The verdict of `should_show_context_menu` for the most recent right
    /// mouse-down, latched so the menu builder — which gpui-component runs on a
    /// deferred callback, one turn after the click — can still see the
    /// modifiers the user actually held.
    pub(super) context_menu_allowed: bool,
    /// The file link the most recent right mouse-down landed on, latched for
    /// the same reason [`context_menu_allowed`](Self::context_menu_allowed)
    /// is: by the time the menu is built the pointer is only a memory.
    menu_link: Option<super::search::LinkTarget>,
    scroll_debt: f32,
    /// Lines travelled under the zoom modifier that have not yet added up to a
    /// font-size step. Kept apart from [`scroll_debt`](Self::scroll_debt) so
    /// letting go of the modifier mid-gesture cannot hand the leftovers of one
    /// to the other.
    zoom_debt: f32,
    pub(super) scroll_frac: f32,
    /// The scrollback bar's end of the grid: where it thinks the viewport is,
    /// and where it has asked for it to go. See [`super::scrollbar`].
    pub(super) scroll_handle: TerminalScrollHandle,
    pub search: Option<SearchState>,
    pub cursor_visible: bool,
    pub(super) search_focused: bool,
    /// The message box docked under an agent, once it has been asked for —
    /// see [`super::composer`].
    pub(super) composer: Option<super::composer::Composer>,
    pub(super) composer_focused: bool,
    pub(super) search_case_sensitive: bool,
    pub(super) search_regex: bool,
    pub(super) search_regex_error: bool,
    pub(super) search_last_query: String,
    /// Bumped by every wakeup that reaches an open search bar, so the pending
    /// rescan can tell "the pane went quiet" from "more output landed".
    pub(super) search_scan_epoch: u64,
    /// Whether a rescan is already waiting out the debounce. One task at a
    /// time, however fast the pane is printing.
    pub(super) search_scan_armed: bool,
    pub bell_flash: bool,
    /// Bumped by every bell, so only the timer armed by the latest one clears
    /// the flash: a burst of bells holds one steady flash instead of strobing.
    bell_epoch: u64,
    pub report_mouse: bool,
    last_at_prompt: bool,
    running_since: Option<std::time::Instant>,
    running_title: String,
    running_agent: Option<crate::core::cli_agent::CLIAgent>,
    last_agent_status: Option<crate::core::cli_agent::AgentStatus>,
    agent_turn_started: Option<std::time::Instant>,
    /// The "finished" notification for a turn that just reached `Done`,
    /// waiting out [`AGENT_DONE_SETTLE`]. Dropped — and so never sent — when
    /// the status moves on first.
    pending_finish_notice: Option<Task<()>>,
    agent_was_rich: bool,
    git_status_cwd: Option<std::path::PathBuf>,
    pub(super) hovered_link: Option<HoveredLink>,
    _focus_subs: Vec<gpui::Subscription>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct HoveredLink {
    /// The cells to underline, one run per row. A link in a table cell
    /// shares its rows with other cells, so its extent is not always one
    /// stretch of the grid from a start to an end.
    pub runs: Vec<(Point, Point)>,
    /// Whether the modifier that would open this link is down.
    ///
    /// A link underlines as soon as the pointer reaches it, so the user can
    /// see there is something there without holding anything. Only once the
    /// modifier is down does it look and behave like something clickable:
    /// promising a hand cursor over a link a plain click will not follow is
    /// the kind of small lie that teaches people to stop trusting the
    /// underline.
    pub armed: bool,
}

/// What sits under a cell, once the grid has been read.
enum GridLink {
    /// An OSC 8 hyperlink the emitter declared, with the extent it declared,
    /// one run per row.
    Hyperlink(String, Vec<(Point, Point)>),
    /// The logical line the cell belongs to, the grid point of every character
    /// in it, and which of those the cell is.
    Text(String, Vec<Point>, usize),
}

/// The outcome of asking what a cell links to.
enum LinkAt {
    /// A link, and the cells it covers as one run per row.
    Found(LinkTarget, Vec<(Point, Point)>),
    /// A token that parses as a path, and the local filesystem said it is not there.
    Unresolved {
        candidate: super::search::FileCandidate,
    },
    None,
}

enum CmdKey {
    Consumed,
    Bubble,
    FallThrough,
}

/// How long a title the program set has to stand before the tab adopts it.
///
/// Long enough that a command which is over almost as soon as it started never
/// reaches the label, short enough that one still running is named while the
/// wait for it is still what the reader is doing.
const TITLE_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// How long a turn has to stay finished before it is announced. Long enough
/// for a queued message or a `Stop` hook to start the next turn, short enough
/// that the notification still reads as the moment the agent stopped.
const AGENT_DONE_SETTLE: std::time::Duration = std::time::Duration::from_millis(1500);

/// What a pane does with a title the program just set — see
/// `TerminalView::set_title_when_settled`.
#[derive(Debug, PartialEq, Eq)]
enum TitleSettle {
    /// The tab already reads this, so whatever was waiting to replace it never
    /// has to happen. This is the case a short command lands in, and the one an
    /// agent's spinner lands in: a frame that only changes the status mark in
    /// front of the title reads the same on the tab, so it asks for no repaint.
    Revert,
    /// Hold it: a wait is already running and adopts the newest title when it
    /// elapses. Restarting the wait instead would let a program that rewrites
    /// its title faster than the wait — a download reporting progress — put
    /// off the tab's next update forever.
    Queue,
    /// Hold it and start the wait.
    QueueAndWait,
}

fn settle_title(showing: &str, waiting: bool, incoming: &str) -> TitleSettle {
    if tty7_core::core::osc::same_title_ignoring_status_mark(incoming, showing) {
        return TitleSettle::Revert;
    }
    match waiting {
        true => TitleSettle::Queue,
        false => TitleSettle::QueueAndWait,
    }
}

/// Join whatever context a pane has into a notification title, most specific
/// part first: an agent name if one is running, otherwise the machine the pane
/// lives on, then the workspace it belongs to. Kept to two segments — Windows
/// toast titles are a single line and ellipsize anything longer.
fn compose_notification_title(
    lead: Option<String>,
    host: Option<String>,
    workspace: Option<String>,
) -> String {
    match (lead.or(host), workspace) {
        (Some(lead), Some(workspace)) => format!("{lead} · {workspace}"),
        (Some(only), None) | (None, Some(only)) => only,
        (None, None) => DEFAULT_TITLE.to_string(),
    }
}

/// The longest command line to put in a confirmation. The shell sends up to
/// 512 bytes; a dialog asking whether to end your work should still read as a
/// sentence.
const BUSY_COMMAND_MAX: usize = 60;

/// Undo the escaping the shell integration applies to an OSC 133;C payload so
/// it cannot break OSC framing: `%` and the four control bytes. Anything else
/// is left exactly as the user typed it.
fn unescape_mark_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let (decoded, width) = match tail.get(..3) {
            Some("%25") => ("%", 3),
            Some("%1B") | Some("%07") | Some("%0D") => ("", 3),
            Some("%0A") => (" ", 3),
            _ => ("%", 1),
        };
        out.push_str(decoded);
        rest = &tail[width..];
    }
    out.push_str(rest);
    out
}

fn clamp_command(cmd: &str) -> String {
    let cmd = cmd.trim();
    match cmd.chars().count() > BUSY_COMMAND_MAX {
        false => cmd.to_string(),
        true => format!(
            "{}…",
            cmd.chars()
                .take(BUSY_COMMAND_MAX)
                .collect::<String>()
                .trim_end()
        ),
    }
}

impl TerminalView {
    fn notify_command_finished(
        &self,
        label: &str,
        elapsed: std::time::Duration,
        cx: &mut Context<Self>,
    ) {
        let secs = elapsed.as_secs().to_string();
        let command = label.trim();
        let body = if command.is_empty() {
            t_fmt(L10nKey::NotifyCommandFinished, &[("secs", &secs)])
        } else {
            t_fmt(
                L10nKey::NotifyCommandFinishedWithCommand,
                &[("command", command), ("secs", &secs)],
            )
        };
        self.notify_pane(None, &body, cx);
    }

    fn notify_agent_finished(
        &self,
        agent: crate::core::cli_agent::CLIAgent,
        elapsed: std::time::Duration,
        cx: &mut Context<Self>,
    ) {
        let secs = elapsed.as_secs().to_string();
        let body = t_fmt(L10nKey::NotifyAgentFinished, &[("secs", &secs)]);
        self.notify_pane(Some(agent.display_name()), &body, cx);
    }

    /// Notify about this pane, with a title describing where it lives.
    fn notify_pane(&self, lead: Option<&str>, body: &str, cx: &mut Context<Self>) {
        let title = self.notification_title(lead, cx);
        super::notify::notify_desktop_for_pane(Some(&title), body);
    }

    /// Desktop notifications the program wrote (OSC 9, 99, 777), such as
    /// Claude Code's with its Notifications setting on `ghostty`, `kitty` or
    /// `iterm2`. The agent's Waiting mark on the tab is the daemon's doing.
    ///
    /// `Unfocused` holds a note back only while the reader is looking at this
    /// very pane. A pane whose agent reports through tty7's hooks already gets
    /// its Waiting and Done notices from `poll_agent_status` whenever
    /// `hooks_notify`, so the program's own copy would be a duplicate there.
    ///
    /// At most a few per pane every few seconds reach the desktop (pane output
    /// is untrusted), with one note saying the rest were not shown.
    fn show_program_notes(&self, hooks_notify: bool, window: &Window, cx: &mut Context<Self>) {
        let notes = self.terminal.take_osc_notes();
        let watched = window.is_window_active() && self.focus_handle.is_focused(window);
        let hooked = self.terminal.agent_session().is_some_and(|s| s.rich);
        let show = cx
            .global::<Config>()
            .notify_on_command_finish
            .allows(watched)
            && !(hooked && hooks_notify);
        if !notes.is_empty() {
            log::debug!(
                "{} program notification(s) {}",
                notes.len(),
                if show { "shown" } else { "held back" }
            );
        }
        // Asked on every poll, so the rest are said once a flood stops too,
        // and while notes are held back, so a stale count never surfaces
        // minutes later.
        let (notes, dropped) = self.terminal.pace_osc_notes(
            if show { notes } else { Vec::new() },
            std::time::Instant::now(),
        );
        if !show {
            return;
        }
        let agent = self.terminal.foreground_agent().map(|a| a.display_name());
        if dropped {
            self.notify_pane(agent, t(L10nKey::ProgramNotesDropped), cx);
        }
        for (title, body) in notes {
            match title {
                Some(title) => {
                    super::notify::notify_desktop_for_pane(Some(&title), &body);
                }
                None => self.notify_pane(agent, &body, cx),
            }
        }
    }

    fn notification_title(&self, lead: Option<&str>, _cx: &App) -> String {
        compose_notification_title(lead.map(str::to_string), None, None)
    }
}

/// Ring the platform's alert sound, reporting whether one was actually made —
/// `Audible` falls back to a flash when it wasn't, so a silent `false` is the
/// difference between "you heard the bell" and "you saw it instead".
///
/// Linux has no equivalent worth the dependency: the desktop sound APIs
/// (libcanberra, PipeWire) are a runtime link away and X11's `XBell` does
/// nothing under Wayland, so the flash fallback stays the answer there.
fn ring_system_bell() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2_app_kit::NSBeep();
        true
    }
    #[cfg(target_os = "windows")]
    {
        // MB_OK is the "Default Beep" scheme entry, so this follows whatever
        // the user picked in Sound Settings — including "None", which is a
        // deliberate silence and still reports success. Deliberately not
        // `Beep()`, which synthesizes a fixed tone straight at the speaker and
        // ignores the scheme. Returns immediately; the sound plays async.
        use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
        use windows_sys::Win32::UI::WindowsAndMessaging::MB_OK;
        unsafe { MessageBeep(MB_OK) != 0 }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    tty7_core::core::paste::paste_bytes(text.as_bytes(), bracketed)
}

/// Whether a single line can be sent as raw keystrokes rather than a bracketed
/// paste. Control characters are out: under a bracketed paste the receiver
/// inserts every byte literally, but delivered raw each one hits a binding
/// (Tab completes, `^C` abandons the line). A line past 512 bytes is pasted
/// too, so a long command does not pay a per-keystroke redraw.
pub(super) fn types_cleanly(line: &str) -> bool {
    line.len() <= 512 && !line.chars().any(char::is_control)
}

fn trim_trailing_spaces(text: &str) -> String {
    text.split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The local files the clipboard carries — a Finder or Explorer Copy on a file
/// puts its path there, not its bytes.
fn clipboard_paths(item: &ClipboardItem) -> Vec<std::path::PathBuf> {
    item.entries()
        .iter()
        .filter_map(|e| match e {
            ClipboardEntry::ExternalPaths(paths) => Some(paths.paths()),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect()
}

/// Paths as one line of shell words, trailing space included so whatever is
/// typed next starts a word of its own.
pub(super) fn pasted_paths_text(paths: &[String], shell: Option<&str>) -> String {
    let words: Vec<String> = paths.iter().map(|p| quote_for_shell(p, shell)).collect();
    format!("{} ", words.join(" "))
}

/// Whether the clipboard carries files rather than (only) text.
pub(super) fn clipboard_has_paths(item: &ClipboardItem) -> bool {
    !clipboard_paths(item).is_empty()
}

fn write_clipboard_image(img: &gpui::Image) -> Option<std::path::PathBuf> {
    use gpui::ImageFormat;
    let dir = std::env::temp_dir().join("tty7-clipboard");
    std::fs::create_dir_all(&dir).ok()?;
    let (ext, transcoded) = match img.format {
        ImageFormat::Png => ("png", None),
        ImageFormat::Jpeg => ("jpg", None),
        ImageFormat::Gif => ("gif", None),
        ImageFormat::Webp => ("webp", None),
        other => ("png", Some(transcode_to_png(&img.bytes, other)?)),
    };
    let data: &[u8] = transcoded.as_deref().unwrap_or(&img.bytes);
    let path = dir.join(format!("paste-{:016x}.{ext}", img.id));
    std::fs::write(&path, data).ok()?;
    Some(path)
}

/// Whether a pane stages the clipboard image to a file instead of forwarding
/// SYN and letting the agent read the clipboard itself.
///
/// SYN only works because the agent shares a clipboard with the pane. That
/// holds for a local macOS pane — where it is the better path, carrying the
/// image at full fidelity — and nowhere else: off macOS Claude Code silently
/// drops raw screenshots (anthropics/claude-code#26679), and an agent in a
/// remote pane reads the clipboard of the host it runs on, which never holds
/// this machine's screenshot no matter what the local OS is.
fn stages_clipboard_image(is_remote: bool) -> bool {
    cfg!(not(target_os = "macos")) || is_remote
}

/// The Windows spelling of a WSL pane's POSIX cwd. The distro's `\\wsl$`
/// share is how a local `read_dir` can list a directory this process cannot
/// reach natively.
///
/// Everything stays on the share, `/mnt/<drive>` included. Mapping the
/// automount back to the drive letter would list faster, but an absolute
/// word completes against its *cwd's* path prefix (`resolve_dir` keeps the
/// prefix when a rooted word lands on it) — so a drive-spelled cwd would send
/// `ls /etc<Tab>` to `C:\etc` instead of the distro's `/etc`. One prefix,
/// one meaning. A distro name with a path separator cannot name a share.
fn wsl_share_path(distro: &str, posix: &str) -> Option<std::path::PathBuf> {
    if distro.is_empty() || distro.contains(['\\', '/']) {
        return None;
    }
    let rest = posix.strip_prefix('/')?;
    Some(std::path::PathBuf::from(format!(
        r"\\wsl$\{distro}\{}",
        rest.replace('/', "\\")
    )))
}

/// The distro whose `\\wsl$` share holds a pane's filesystem, or `None` when
/// the pane is not a WSL one and Tab belongs to the shell.
///
/// A pane that runs `wsl.exe` is tagged by its remote context. Only this
/// machine's panes may take the share; a remote host's same-named distro
/// would list the wrong files.
fn wsl_share_distro(
    remote: Option<&crate::daemon::protocol::RemoteContext>,
    host_is_local: bool,
) -> Option<String> {
    let remote = remote?;
    (remote.kind == crate::daemon::protocol::RemoteKind::Wsl && host_is_local)
        .then(|| remote.target.clone())
}

/// The directory the Files panel lists for a pane, spelled so the pane's
/// host can read it — `None` when no spelling can.
///
/// A cwd the host resolves natively is used as-is. Past that, only a pane
/// whose files sit in a WSL distro on this machine has another way in: its
/// POSIX cwd goes through the distro's `\\wsl$` share (#896 — handed to a
/// local `read_dir` raw, `/home/me` is `C:\home\me`, and the panel showed
/// "Could not be read"). A cwd that is already a Windows path came from the
/// Windows side of the pane and is readable as it stands. Anything else — a
/// shell ssh'd onward to a machine tty7 has no link to — has no spelling
/// here, and rooting the tree at it could only list nothing or the wrong
/// machine's files.
fn files_cwd(
    host_cwd: Option<std::path::PathBuf>,
    wsl_distro: Option<&str>,
    cwd: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    if host_cwd.is_some() {
        return host_cwd;
    }
    let distro = wsl_distro?;
    let cwd = cwd?;
    let spelled = cwd.to_string_lossy();
    match spelled.starts_with('/') {
        true => wsl_share_path(distro, &spelled),
        false => Some(cwd),
    }
}

fn transcode_to_png(bytes: &[u8], format: gpui::ImageFormat) -> Option<Vec<u8>> {
    use gpui::ImageFormat as G;
    let src = match format {
        G::Png => image::ImageFormat::Png,
        G::Jpeg => image::ImageFormat::Jpeg,
        G::Webp => image::ImageFormat::WebP,
        G::Gif => image::ImageFormat::Gif,
        G::Bmp => image::ImageFormat::Bmp,
        G::Tiff => image::ImageFormat::Tiff,
        G::Ico => image::ImageFormat::Ico,
        G::Pnm => image::ImageFormat::Pnm,
        G::Svg => return None,
    };
    let decoded = image::load_from_memory_with_format(bytes, src).ok()?;
    let mut out = Vec::new();
    decoded
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

fn fallback_chain(family: &str, configured: &[String]) -> Vec<String> {
    let mut chain = configured.to_vec();
    let mut pin = |name: &str| {
        if family != name && !chain.iter().any(|f| f == name) {
            chain.push(name.to_string());
        }
    };
    for name in crate::core::config::platform_last_resort_fallbacks() {
        pin(name);
    }
    pin("Hack");
    chain
}

/// Put `chain` on the regular face and on the bold and italic ones.
///
/// Bold and italic never carry a chain of their own — `alt_font` copies theirs
/// off the regular face when they are built — so a rebuild that skipped them
/// would leave two of the three faces resolving against the old chain.
fn apply_fallback_chain(
    chain: Vec<String>,
    font: &mut Font,
    bold: &mut Option<Font>,
    italic: &mut Option<Font>,
) {
    let fallbacks = Some(gpui::FontFallbacks::from_fonts(chain));
    font.fallbacks = fallbacks.clone();
    if let Some(font) = bold {
        font.fallbacks = fallbacks.clone();
    }
    if let Some(font) = italic {
        font.fallbacks = fallbacks;
    }
}

impl TerminalView {
    pub(crate) fn kill_pane(pane_id: u64) {
        RemoteTerminal::kill_pane(pane_id);
    }

    pub fn spawn_shell_terminal_in(
        working_directory: Option<std::path::PathBuf>,
        shell: Option<ShellSpec>,
    ) -> anyhow::Result<ShellParts> {
        // `shell: None` still reads `config.json`. A broken `shell.program`
        // has to fall back to the login shell, or a Dock launch dies before
        // the window exists and the user cannot open Settings to fix it.
        let customized = shell.is_none() && crate::core::config::shell_command().is_some();
        match Self::open_shell_terminal(working_directory.clone(), shell.clone(), true) {
            Ok(parts) => Ok(parts),
            Err(error) if customized => {
                let program = crate::core::config::shell_command()
                    .map(|(program, _)| program)
                    .unwrap_or_default();
                log::warn!(
                    "configured shell {program} failed to start ({error:#}); using the login shell"
                );
                let mut parts = Self::open_shell_terminal(working_directory, None, false)?;
                parts.shell_notice = Some(crate::ui::i18n::t_fmt(
                    crate::ui::i18n::L10nKey::ShellFellBack,
                    &[("shell", program.as_str())],
                ));
                Ok(parts)
            }
            Err(error) => Err(error),
        }
    }

    fn open_shell_terminal(
        working_directory: Option<std::path::PathBuf>,
        shell: Option<ShellSpec>,
        use_config_shell: bool,
    ) -> anyhow::Result<ShellParts> {
        let (terminal, pane_id) = RemoteTerminal::spawn_on(
            TermSize::new(80, 24),
            8,
            17,
            working_directory,
            shell.clone(),
            None,
            use_config_shell,
        )?;
        Ok(ShellParts {
            terminal,
            pane_id,
            shell_spec: shell,
            shell_notice: None,
        })
    }

    pub fn from_shell_parts(
        parts: ShellParts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::with_terminal(parts.terminal, parts.pane_id, window, cx);
        view.shell_spec = parts.shell_spec;
        view
    }

    fn with_terminal(
        terminal: RemoteTerminal,
        pane_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = cx.global::<Config>();
        let font_family = config.font_family.clone();
        let fallbacks = fallback_chain(&font_family, &config.font_fallbacks);
        let font_size = px(config.font_size);
        let line_height_mul = config.line_height;
        let font_features = config
            .font_features
            .as_ref()
            .map(crate::core::config::gpui_font_features);
        let report_mouse = config.mouse_reporting;
        let mut font = gpui::font(font_family);
        font.fallbacks = Some(gpui::FontFallbacks::from_fonts(fallbacks.clone()));
        if let Some(features) = &font_features {
            font.features = features.clone();
        }
        let alt_font = |family: &Option<String>| {
            family.as_ref().map(|f| {
                let mut af = gpui::font(f.clone());
                af.fallbacks = Some(gpui::FontFallbacks::from_fonts(fallbacks.clone()));
                if let Some(features) = &font_features {
                    af.features = features.clone();
                }
                af
            })
        };
        let font_bold = alt_font(&config.font_family_bold);
        let font_italic = alt_font(&config.font_family_italic);

        let focus_handle = cx.focus_handle();

        let events = terminal.events.clone();
        cx.spawn(async move |this, cx| {
            let mut batch = Vec::new();
            while let Ok(ev) = events.recv().await {
                batch.push(ev);
                while let Ok(ev) = events.try_recv() {
                    batch.push(ev);
                }
                // Output reaches the screen through `handle_event`'s
                // `notify()`, which gpui scopes to windows currently
                // rendering this view. A pane in a background tab must stay
                // out of the frame loop entirely: refreshing the window
                // directly from here pinned the visible tab at full frame
                // rate whenever any hidden pane was producing output.
                let res = this.update(cx, |view, cx| {
                    let mut woke = false;
                    for ev in batch.drain(..) {
                        if matches!(ev, AlacEvent::Wakeup) && std::mem::replace(&mut woke, true) {
                            continue;
                        }
                        view.handle_event(ev, cx);
                    }
                });
                if res.is_err() {
                    break;
                }
            }
        })
        .detach();

        let focus_subs = vec![
            cx.on_focus_in(&focus_handle, window, |view, _window, cx| {
                view.cursor_visible = true;
                view.report_focus_change(true);
                cx.notify();
            }),
            cx.on_blur(&focus_handle, window, |view, _window, cx| {
                view.report_focus_change(false);
                cx.notify();
            }),
        ];

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(530))
                    .await;
                if this
                    .update_in(cx, |view, window, cx| {
                        // A pane can be focused once while it is being built, before
                        // its leaf is attached to the window. Moving focus away in
                        // that interval does not deliver the leaf a blur callback,
                        // so no flag a callback maintains can be trusted to say
                        // which pane the reader is on. The window's handle is
                        // authoritative here, just as it is in the paint path.
                        if view.focus_handle.is_focused(window) {
                            if cx.global::<Config>().cursor_blink {
                                view.cursor_visible = !view.cursor_visible;
                                cx.notify();
                            } else if !view.cursor_visible {
                                view.cursor_visible = true;
                                cx.notify();
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(300))
                    .await;
                if this
                    .update_in(cx, |view, window, cx| view.poll_foreground(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        super::color_scheme::watch(cx);

        cx.on_release_in(window, move |view, window, cx| {
            view.terminal.detach_link();
            for image in view.terminal.images().take_for_release() {
                cx.drop_image(image, Some(window));
            }
        })
        .detach();

        window.focus(&focus_handle, cx);

        Self {
            terminal,
            host_id: crate::ui::host_ops::HostId::LOCAL,
            pane_id,
            shell_spec: None,
            focus_handle,
            font,
            font_bold,
            font_italic,
            font_features,
            font_size,
            line_height_mul,
            cell_width: px(8.),
            line_height: px(17.),
            grid_slack: px(0.),
            grid_buf: Vec::new(),
            grid_snap: None,
            frame_alt_screen: false,
            frame_has_selection: false,
            selecting: false,
            drag_scroll: None,
            drag_scroll_epoch: 0,
            scroll_anim: None,
            scroll_anim_epoch: 0,
            gesture_until: None,
            gesture_zoom: None,
            title: DEFAULT_TITLE.to_string(),
            pending_title: None,
            default_title: DEFAULT_TITLE.to_string(),
            marked_text: String::new(),
            last_mouse_cell: None,
            mouse_grab: None,
            cell_geom: None,
            ime_anchor: None,
            report_mouse,
            last_hover_cell: None,
            link_modifier_down: false,
            link_repo_root: None,
            link_repo_root_pending: false,
            context_menu_allowed: true,
            menu_link: None,
            scroll_debt: 0.,
            zoom_debt: 0.,
            scroll_frac: 0.,
            scroll_handle: TerminalScrollHandle::default(),
            search: None,
            cursor_visible: true,
            search_focused: false,
            composer: None,
            composer_focused: false,
            search_case_sensitive: false,
            search_regex: false,
            search_regex_error: false,
            search_last_query: String::new(),
            search_scan_epoch: 0,
            search_scan_armed: false,
            bell_flash: false,
            bell_epoch: 0,
            last_at_prompt: false,
            running_since: None,
            running_title: String::new(),
            running_agent: None,
            last_agent_status: None,
            agent_turn_started: None,
            pending_finish_notice: None,
            agent_was_rich: false,
            git_status_cwd: None,
            hovered_link: None,
            _focus_subs: focus_subs,
        }
    }

    pub fn set_grid_size(
        &mut self,
        cols: usize,
        rows: usize,
        cell_width: Pixels,
        line_height: Pixels,
        scale: f32,
        _cx: &mut Context<Self>,
    ) {
        // A column change reflows every wrapped line when the daemon echoes
        // `Size`. That echo wakes the view, and the debounced search rescan
        // (`Wakeup` → `note_output_under_search`) moves highlights onto the
        // new geometry (#586). Scanning here would still see the pre-reflow
        // grid. Rows alone reflow nothing.
        if (cols, rows) != (self.terminal.size().cols, self.terminal.size().rows) {
            self.last_hover_cell = None;
            self.hovered_link = None;
        }
        self.cell_width = cell_width;
        self.line_height = line_height;
        // Report the cell size to the child in *device* pixels (logical × display
        // scale), so `ws_xpixel`/`ws_ypixel` describe the real framebuffer. A
        // pixel-aware program like terminal-browser renders its frame at that
        // native resolution; painted back into logical-pixel bounds, gpui blits
        // it ~1:1 on the framebuffer instead of upscaling a half-resolution
        // bitmap (which looked soft and magnified on Retina). This is what
        // kitty/ghostty report. `self.cell_width` stays logical — glyph layout
        // and mouse mapping work in logical pixels.
        let scale = if scale.is_finite() && scale > 0. {
            scale
        } else {
            1.
        };
        self.terminal.resize(
            TermSize::new(cols, rows),
            (cell_width.as_f32() * scale).round().max(1.) as u16,
            (line_height.as_f32() * scale).round().max(1.) as u16,
        );
    }

    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        self.terminal.foreground_cwd()
    }

    /// The title this pane is showing, or `None` while it is still answering
    /// to the app's own name — see [`stated_title`]. The label ladder reads
    /// this where the machine tree reads
    /// [`PaneRecord::osc_title`](tty7_core::core::machine::PaneRecord::osc_title),
    /// which is what lets the tab strip and the switcher name a tab the same
    /// way.
    #[cfg(test)]
    pub(crate) fn stated_title(&self) -> Option<&str> {
        stated_title(&self.title)
    }

    /// `name` with the suffix that says how this pane ended.
    fn ended_title(&self, name: &str) -> String {
        let key = L10nKey::PaneTitleProcessExited;
        t_fmt(key, &[("title", name)])
    }

    pub fn remote_context(&self) -> Option<RemoteContext> {
        self.terminal.remote_context()
    }

    pub fn local_cwd(&self) -> Option<std::path::PathBuf> {
        self.paths_are_local().then(|| self.cwd())?
    }

    fn paths_are_local(&self) -> bool {
        self.remote_context().is_none() && self.host_id.is_local()
    }

    pub fn host(&self) -> Option<std::sync::Arc<tty7_core::host::local::LocalHost>> {
        self.host_id
            .is_local()
            .then(tty7_core::host::local::LocalHost::shared)
    }

    pub fn host_id(&self) -> crate::ui::host_ops::HostId {
        self.host_id
    }

    pub fn host_cwd(&self) -> Option<std::path::PathBuf> {
        self.cwd_is_on_host().then(|| self.cwd())?
    }

    fn cwd_is_on_host(&self) -> bool {
        cwd_is_on_host(!self.paths_are_local(), self.host_id.is_local())
    }

    pub fn agent(&self) -> Option<crate::core::cli_agent::CLIAgent> {
        self.terminal.foreground_agent()
    }

    pub fn agent_session(&self) -> Option<crate::core::cli_agent::AgentSessionState> {
        self.terminal.agent_session()
    }

    /// What this pane is in the middle of, when it can say so. `None` means
    /// either nothing is running or the shell never told us — and a terminal
    /// that guessed would raise this question on every single close.
    pub fn busy(&self) -> Option<PaneBusy> {
        use crate::core::cli_agent::AgentStatus;
        // `Done` is the opposite of busy: the turn is over, and the badge
        // saying so is exactly what sends a reader to close the tab. Only a
        // turn still in flight — running, or stopped on a question — is work
        // that closing would cut short.
        if let Some(agent) = self.agent()
            && self
                .agent_session()
                .is_some_and(|s| matches!(s.status, AgentStatus::Working | AgentStatus::Waiting))
        {
            return Some(PaneBusy::Agent(agent.display_name()));
        }
        // Without shell integration `at_prompt` is permanently false, so
        // `running_since` is permanently Some. Only trust it when the shell is
        // actually reporting.
        if !self.terminal.shell_active() {
            return None;
        }
        self.running_since?;
        // Prefer what the shell said it was running. `running_title` is only
        // the window title as it stood when the command began, and a prompt
        // that titles by directory — a very common setup — made this ask
        // "tty7 is still running. Closing ends it." about a folder. The
        // OSC 133;C mark carries the submitted line itself.
        let named = Some(clamp_command(&unescape_mark_text(
            &self.terminal.running_command(),
        )))
        .filter(|t| !t.is_empty());
        Some(PaneBusy::Command(match named {
            Some(cmd) => cmd,
            None => match self.running_title.trim().is_empty() {
                true => self.title.clone(),
                false => self.running_title.clone(),
            },
        }))
    }

    /// The directory this pane's *work* is happening in — what every panel
    /// that answers "where am I?" should show.
    ///
    /// [`Self::cwd`] is the kernel's idea: the cwd of the foreground process.
    /// That is right for a shell, and wrong for a coding agent, because an
    /// agent moving into a git worktree does not `chdir` — the `claude`
    /// process stays where it was launched while the session works somewhere
    /// else entirely. The hook stream carries the agent's own cwd for exactly
    /// this reason (`AgentSessionState::cwd`), and the git-status poll already
    /// folds the two together into `git_status_cwd`; this reads that result
    /// back out under a name that doesn't imply it's only about git.
    ///
    /// `git_status_cwd` is only ever set for a pane whose paths belong to its
    /// host, so the fallback here is what decides that: this one takes any
    /// cwd, [`Self::effective_host_cwd`] takes only one the host can resolve.
    #[cfg(test)]
    pub fn effective_cwd(&self) -> Option<std::path::PathBuf> {
        self.git_status_cwd.clone().or_else(|| self.cwd())
    }

    /// [`Self::effective_cwd`], restricted to paths this machine can act on.
    /// Callers hand the result to [`tty7_core::host::local::LocalHost`].
    pub fn effective_host_cwd(&self) -> Option<std::path::PathBuf> {
        self.git_status_cwd.clone().or_else(|| self.host_cwd())
    }

    /// The directory the Files panel roots its tree at — see [`files_cwd`].
    ///
    /// Follows a coding agent the way [`Self::effective_cwd`] does. A WSL
    /// pane gets no `git_status_cwd` (its paths are not its host's), so the
    /// agent's own cwd is read here directly before the shell's.
    pub fn files_cwd(&self) -> Option<std::path::PathBuf> {
        let distro = wsl_share_distro(
            self.terminal.remote_context().as_ref(),
            self.host_id.is_local(),
        );
        let cwd = self
            .terminal
            .agent_session()
            .and_then(|s| s.cwd)
            .or_else(|| self.cwd());
        files_cwd(self.effective_host_cwd(), distro.as_deref(), cwd)
    }

    /// The shell binary this pane is running, for the path-quoting rules.
    ///
    /// `None` before the pane has resolved one, which [`quote_for_shell`]
    /// answers from the platform — PowerShell on Windows, POSIX elsewhere.
    /// The one pane that guess is wrong for is a cmd.exe pane that has not
    /// reported in yet.
    pub(super) fn shell_program(&self) -> Option<String> {
        self.shell_spec.as_ref().map(|s| s.program.clone())
    }

    /// Takes a title the program set, and gives it to the tab only once it has
    /// stood for `TITLE_SETTLE`.
    ///
    /// Nearly every prompt framework titles the tab with the command it is
    /// about to run and puts the old title back at the next prompt — that is
    /// where a tab's process name comes from. For a command that finishes in a
    /// blink both edges land within a few frames of each other, so the label
    /// flashed the command and snapped back, which reads as a rendering glitch
    /// rather than as information. Holding the change means a command has to
    /// actually still be running to name the tab.
    ///
    /// A title that reverts before it lands never happens at all: the revert
    /// matches what the tab already shows and just drops the pending one. A
    /// program that keeps rewriting its title — a download writing progress
    /// there — still updates, because the wait already in flight adopts
    /// whatever the newest title is when it elapses rather than starting over.
    fn set_title_when_settled(&mut self, title: String, cx: &mut Context<Self>) {
        match settle_title(&self.title, self.pending_title.is_some(), &title) {
            TitleSettle::Revert => self.pending_title = None,
            TitleSettle::Queue => self.pending_title = Some(title),
            TitleSettle::QueueAndWait => {
                self.pending_title = Some(title);
                cx.spawn(async move |view, cx| {
                    cx.background_executor().timer(TITLE_SETTLE).await;
                    view.update(cx, |view, cx| {
                        if let Some(title) = view.pending_title.take() {
                            view.title = title;
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    fn handle_event(&mut self, ev: AlacEvent, cx: &mut Context<Self>) {
        self.terminal.poll_exited();
        match ev {
            AlacEvent::Wakeup => {
                // The grid moved under whatever the search bar last measured.
                self.note_output_under_search(cx);
                cx.notify();
            }
            AlacEvent::Title(title) => self.set_title_when_settled(title, cx),
            AlacEvent::ResetTitle => self.set_title_when_settled(self.default_title.clone(), cx),
            AlacEvent::PtyWrite(text) => self.terminal.write(text.into_bytes()),
            AlacEvent::ChildExit(_) | AlacEvent::Exit => {
                self.terminal.exited = true;
                // Nothing is left to settle, and a title still waiting its turn
                // would land on top of the state below a moment from now.
                self.pending_title = None;
                // The pane keeps answering to its own name (an SSH pane's
                // host, #438) — only the state suffix is localized (#602).
                self.title = self.ended_title(&self.default_title);
                if self.terminal.child_exited() {
                    cx.emit(ChildExited);
                }
                cx.notify();
            }
            AlacEvent::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            AlacEvent::ClipboardLoad(_, fmt) => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    self.terminal.write(fmt(&text).into_bytes());
                }
            }
            AlacEvent::ColorRequest(idx, fmt) => {
                let theme = cx.theme();
                let rgb = match idx {
                    256 => super::palette::hsla_to_rgb(theme.foreground),
                    257 => super::palette::hsla_to_rgb(theme.background),
                    258 => super::palette::hsla_to_rgb(theme.caret),
                    i => self.terminal.palette[i.min(255)],
                };
                self.terminal.write(fmt(rgb).into_bytes());
            }
            AlacEvent::Bell => match cx.global::<Config>().bell {
                BellMode::None => {}
                BellMode::Visual => self.flash_bell(cx),
                BellMode::Audible => {
                    if !ring_system_bell() {
                        self.flash_bell(cx);
                    }
                }
                BellMode::Both => {
                    ring_system_bell();
                    self.flash_bell(cx);
                }
            },
            AlacEvent::TextAreaSizeRequest(fmt) => {
                let size = self.terminal.size();
                let reply = fmt(alacritty_terminal::event::WindowSize {
                    num_lines: size.rows as u16,
                    num_cols: size.cols as u16,
                    cell_width: self.cell_width.as_f32().round() as u16,
                    cell_height: self.line_height.as_f32().round() as u16,
                });
                self.terminal.write(reply.into_bytes());
            }
            _ => {}
        }
    }

    fn report_focus_change(&self, focused: bool) {
        let mode = *self.terminal.term.lock().mode();
        if let Some(bytes) = focus_report_bytes(mode, focused) {
            self.terminal.write(bytes);
        }
    }
    fn on_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal.exited {
            return;
        }
        let reshaped = if cfg!(target_os = "macos") {
            super::input::reshape_option_keystroke(
                &ev.keystroke,
                cx.global::<Config>().macos_option_as_alt,
            )
        } else {
            None
        };
        let ks = reshaped.as_ref().unwrap_or(&ev.keystroke);
        let m = &ks.modifiers;

        if self.composer_focused {
            if ks.key == "escape" {
                self.composer_escape(window, cx);
                cx.stop_propagation();
            } else if ks.key == "c" && m.control && !m.platform && !m.alt && !m.shift {
                self.composer_interrupt(window, cx);
                cx.stop_propagation();
            }
            return;
        }

        if self.search.is_some() && self.search_focused {
            if ks.key == "escape" {
                self.close_search(window, cx);
                cx.stop_propagation();
            }
            return;
        }

        if m.platform && !m.control && !m.alt {
            match self.handle_cmd_shortcut(ks, window, cx) {
                CmdKey::Consumed => {
                    cx.stop_propagation();
                    return;
                }
                CmdKey::Bubble => return,
                CmdKey::FallThrough => {}
            }
        }

        // Ctrl+V is not here: off macOS it is the `AlternatePaste` binding,
        // which the keymap withholds on the alternate screen so a full-screen
        // program gets its SYN, and which the user can retire outright. Copy
        // and cut stay, because both answer a selection this view owns and
        // fall through to the PTY when there is none.
        //
        // Ctrl+Shift+C/X are the keymap's alone: rebinding Copy has to retire
        // Ctrl+Shift+C, which it cannot if this path answers it too.
        if cfg!(not(target_os = "macos"))
            && m.control
            && !m.platform
            && !m.alt
            && !m.shift
            && matches!(ks.key.as_str(), "c" | "x")
        {
            match self.handle_cmd_shortcut(ks, window, cx) {
                CmdKey::Consumed => {
                    cx.stop_propagation();
                    return;
                }
                CmdKey::Bubble | CmdKey::FallThrough => {}
            }
        }

        #[cfg(target_os = "macos")]
        if !window.has_pending_keystrokes() && super::input::defer_to_ime(ks, self.key_flags()) {
            return;
        }

        let kitty = self.key_flags();
        if let Some(bytes) = super::input::keystroke_to_bytes(ks, kitty) {
            self.terminal.write(bytes);
            self.cursor_visible = true;
            self.jump_to_prompt();
            cx.notify();
            cx.stop_propagation();
        }
    }
    fn handle_cmd_shortcut(
        &mut self,
        ks: &gpui::Keystroke,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CmdKey {
        let m = &ks.modifiers;
        match ks.key.as_str() {
            "c" => {
                if self.copy_contextual(m.control, false, cx) {
                    CmdKey::Consumed
                } else {
                    CmdKey::FallThrough
                }
            }
            "x" => {
                if self.cut_contextual(cx) {
                    CmdKey::Consumed
                } else {
                    CmdKey::FallThrough
                }
            }
            // Cmd+V only: macOS leaves `PasteText` unbound and pastes from
            // here, and Cmd carries no control code to lose. The Ctrl+V half
            // of this key lives in the keymap as `AlternatePaste`.
            "v" => {
                self.paste_from_clipboard(cx);
                CmdKey::Consumed
            }
            "a" => {
                self.select_all_contextual(cx);
                CmdKey::Consumed
            }
            "z" => {
                self.undo_edit(m.shift, cx);
                CmdKey::Consumed
            }
            "left" => {
                if cfg!(target_os = "macos") {
                    self.send_to_pty(&[0x01], cx);
                }
                CmdKey::Consumed
            }
            "right" => {
                if cfg!(target_os = "macos") {
                    self.send_to_pty(&[0x05], cx);
                }
                CmdKey::Consumed
            }
            "backspace" => {
                if cfg!(target_os = "macos") {
                    self.send_to_pty(&[0x15], cx);
                }
                CmdKey::Consumed
            }
            "delete" => {
                if cfg!(target_os = "macos") {
                    self.send_to_pty(&[0x0b], cx);
                }
                CmdKey::Consumed
            }
            _ => CmdKey::Bubble,
        }
    }

    fn has_selection(&self) -> bool {
        self.terminal.term.lock().selection.is_some()
    }
    /// Whether *this frame* draws a selection. The grid half comes from
    /// [`Self::sync_frame_facts`] rather than the terminal, which is what keeps
    /// the draw off the lock; a selection that appears while the reader holds
    /// it is drawn one frame later.
    fn any_selection(&self) -> bool {
        self.frame_has_selection
    }

    /// The keymap context this pane declares each frame.
    ///
    /// `alt_screen` is how a binding steps aside for a full-screen program:
    /// `AlternatePaste` carries `Terminal && !alt_screen`, so Ctrl+V pastes at
    /// a prompt, reaches vim as SYN, and can still be handed the whole screen
    /// by rebinding `PasteText` onto it (#677).
    pub(super) fn key_context(&self) -> gpui::KeyContext {
        let mut context = gpui::KeyContext::new_with_defaults();
        context.add("Terminal");
        // The frame's own answer, not the terminal's. gpui matches keystrokes
        // against the context the last painted frame published, so this was
        // already a frame-old reading of the mode even when it locked; the
        // chord that must not be a frame late (`AlternatePaste`) asks the
        // terminal again in `alternate_paste`, which is what that comment
        // below is about.
        if self.frame_alt_screen {
            context.add("alt_screen");
        }
        context
    }

    /// `AlternatePaste`, with the grid asked again before it pastes.
    ///
    /// The `!alt_screen` half of the binding's context comes from the frame
    /// that was last *painted*, and gpui matches keystrokes against that frame
    /// — so a program that took the alternate screen after the last paint is
    /// still "at a prompt" as far as the keymap is concerned. One frame is
    /// enough: the keystroke that launches a full-screen program and the
    /// Ctrl+V after it can land either side of a paint. Pasting there is not a
    /// mistake the user can take back — vim in normal mode runs the clipboard
    /// as commands — so the last word belongs to the terminal mode, not to the
    /// frame. Propagating hands the chord on to `on_key_down`, which encodes it
    /// as the SYN the program is waiting for.
    fn alternate_paste(&mut self, cx: &mut Context<Self>) {
        if self.on_alt_screen() {
            cx.propagate();
            return;
        }
        self.paste_from_clipboard(cx);
    }

    pub(super) fn key_flags(&self) -> super::input::KeyFlags {
        super::input::KeyFlags::from_mode_with_local_conpty(
            self.terminal.term.lock().mode(),
            self.terminal.is_local_conpty(),
        )
    }

    fn tab_bytes(&self, shift: bool) -> Vec<u8> {
        super::input::tab_bytes(shift, self.key_flags())
    }

    fn jump_to_prompt(&mut self) {
        self.cancel_scroll_anim();
        let mut term = self.terminal.term.lock();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        drop(term);
        self.scroll_frac = 0.;
    }

    pub(super) fn send_to_pty(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if self.terminal.exited {
            return;
        }
        self.terminal.write(bytes.to_vec());
        self.cursor_visible = true;
        self.jump_to_prompt();
        cx.notify();
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let mut term = self.terminal.term.lock();
        let grid = term.grid();
        let start = Point::new(grid.topmost_line(), Column(0));
        let end = Point::new(grid.bottommost_line(), grid.last_column());
        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
        sel.update(end, Side::Right);
        term.selection = Some(sel);
        drop(term);
        cx.notify();
    }
    pub fn select_all_contextual(&mut self, cx: &mut Context<Self>) {
        self.select_all(cx);
    }
    pub fn paste(&mut self, text: String, cx: &mut Context<Self>) {
        // The agent's input is under the box, so a paste meant for it goes
        // into the box — out of sight under it, it would be sent unseen.
        if self.composer_takes_typing(&text, cx) {
            return;
        }
        // What is pasted lands on the prompt, so the prompt is what has to be
        // on screen. This also clears the selection.
        self.jump_to_prompt();
        let bracketed = self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        self.terminal.write(paste_bytes(&text, bracketed));
        cx.notify();
    }

    fn flash_bell(&mut self, cx: &mut Context<Self>) {
        self.bell_epoch += 1;
        let epoch = self.bell_epoch;
        self.bell_flash = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            let _ = this.update(cx, |view, cx| {
                // A bell rung since this one owns the flash now. Holding
                // Backspace on an empty bash prompt rings at key-repeat rate,
                // and clearing here would blank it every few frames (#874).
                if view.bell_epoch != epoch {
                    return;
                }
                view.bell_flash = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Record where this frame anchors the IME, and say whether the platform
    /// has to be told. macOS asks for the rectangle itself whenever it needs
    /// one; Linux only takes it when it is pushed, and gpui pushes it on focus
    /// and around a preedit, never because the caret moved. A TUI moves the
    /// caret on its own — every redraw, every keystroke it echoes — so without
    /// a push the candidate window stayed wherever the last composition or
    /// the focus-in left it, or at the window's corner (#1131).
    pub(super) fn ime_anchor_moved(&mut self, anchor: Option<gpui::Bounds<Pixels>>) -> bool {
        if self.ime_anchor == anchor {
            return false;
        }
        self.ime_anchor = anchor;
        anchor.is_some()
    }

    pub fn mouse_mode(&self) -> bool {
        self.report_mouse
            && self
                .terminal
                .term
                .lock()
                .mode()
                .intersects(TermMode::MOUSE_MODE)
    }

    fn write_mouse(&self, base: u8, mods: &Modifiers, col: usize, row: usize, pressed: bool) {
        let sgr = self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::SGR_MOUSE);
        if let Some(msg) = encode_mouse(sgr, base, mods, col, row, pressed) {
            self.terminal.write(msg);
        }
    }

    pub fn mouse_press(&mut self, button: MouseButton, col: usize, row: usize, mods: &Modifiers) {
        let base = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        self.last_mouse_cell = Some((col, row));
        self.mouse_grab = Some(button);
        self.write_mouse(base, mods, col, row, true);
    }

    /// Only the release of a press this pane reported: a button let go
    /// anywhere else in the window is not this program's to hear.
    pub fn mouse_release(&mut self, button: MouseButton, col: usize, row: usize, mods: &Modifiers) {
        if self.mouse_grab != Some(button) {
            return;
        }
        self.mouse_grab = None;
        let base = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        self.write_mouse(base, mods, col, row, false);
    }

    /// Only a drag that began with a press this pane reported. Once it has,
    /// the drag follows the pointer out of the grid, pinned to its edge.
    pub fn mouse_drag(&mut self, button: MouseButton, col: usize, row: usize, mods: &Modifiers) {
        if self.mouse_grab != Some(button) || self.last_mouse_cell == Some((col, row)) {
            return;
        }
        let wants = self.report_mouse
            && self
                .terminal
                .term
                .lock()
                .mode()
                .intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION);
        if !wants {
            return;
        }
        self.last_mouse_cell = Some((col, row));
        let base = match button {
            MouseButton::Left => 32,
            MouseButton::Middle => 33,
            MouseButton::Right => 34,
            _ => return,
        };
        self.write_mouse(base, mods, col, row, true);
    }

    /// No button is down, so no press is still waiting for its release —
    /// even one whose release went to another window and never came back.
    pub(super) fn drop_mouse_grab(&mut self) {
        self.mouse_grab = None;
    }

    pub fn mouse_motion(&mut self, col: usize, row: usize, mods: &Modifiers) {
        if self.last_mouse_cell == Some((col, row)) {
            return;
        }
        if !self.report_mouse
            || !self
                .terminal
                .term
                .lock()
                .mode()
                .contains(TermMode::MOUSE_MOTION)
        {
            return;
        }
        self.last_mouse_cell = Some((col, row));
        self.write_mouse(35, mods, col, row, true);
    }

    /// `at` is the cell under the pointer when the wheel turned. A wheel
    /// report has to say where it happened — tmux scrolls the pane under the
    /// pointer, or its window list — and the last click is no stand-in for
    /// that: a program asking for button events only (1002) is never told the
    /// pointer moved away from it (#1131).
    pub fn scroll(
        &mut self,
        lines: i32,
        at: Option<(usize, usize)>,
        mods: &Modifiers,
        cx: &mut Context<Self>,
    ) {
        if lines == 0 {
            return;
        }
        self.cancel_scroll_anim();
        let mut mode = *self.terminal.term.lock().mode();
        if !self.report_mouse {
            mode.remove(TermMode::MOUSE_MODE);
        }
        match wheel_route(mode, mods.shift, lines > 0) {
            WheelRoute::Report { base } => {
                let (col, row) = at.or(self.last_mouse_cell).unwrap_or((0, 0));
                for _ in 0..lines.unsigned_abs() {
                    self.write_mouse(base, mods, col, row, true);
                }
            }
            WheelRoute::Arrows { seq } => {
                let mut out = Vec::with_capacity(seq.len() * lines.unsigned_abs() as usize);
                for _ in 0..lines.unsigned_abs() {
                    out.extend_from_slice(seq);
                }
                self.terminal.write(out);
            }
            WheelRoute::Scrollback => {
                self.scroll_frac = 0.;
                self.terminal
                    .term
                    .lock()
                    .scroll_display(Scroll::Delta(lines));
                cx.notify();
            }
        }
    }

    /// `raw` copies the rows as the grid holds them. Otherwise, in a Claude
    /// pane with `copy_join_wrapped` on, the rows Claude wrapped are joined;
    /// other programs' output is never joined, since `ls` columns, separators
    /// and an editor's own wraps all look like Claude's.
    pub fn copy_selection(&mut self, raw: bool, cx: &mut Context<Self>) {
        let cfg = cx.global::<Config>();
        let join = !raw
            && cfg.copy_join_wrapped
            && self.terminal.foreground_agent() == Some(crate::core::cli_agent::CLIAgent::Claude);
        let text = {
            let term = self.terminal.term.lock();
            if join {
                copy_unwrap::joined(&term)
            } else {
                term.selection_to_string()
            }
        };
        if let Some(mut text) = text {
            if cfg.clipboard_trim_trailing_spaces {
                text = trim_trailing_spaces(&text);
            }
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
        }
    }
    pub fn copy_contextual(
        &mut self,
        clear_on_copy: bool,
        raw: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.has_selection() {
            self.copy_selection(raw, cx);
            if clear_on_copy {
                self.terminal.term.lock().selection = None;
                cx.notify();
            }
            return true;
        }
        false
    }
    pub fn undo_edit(&mut self, _redo: bool, _cx: &mut Context<Self>) {}
    pub fn cut_contextual(&mut self, _cx: &mut Context<Self>) -> bool {
        false
    }

    pub fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let paths = clipboard_paths(&item);
        if !paths.is_empty() {
            self.paste_local_paths(paths, cx);
            return;
        }
        if let Some(text) = item.text() {
            self.paste(text, cx);
            return;
        }
        if let Some(img) = item.entries().iter().find_map(|e| match e {
            ClipboardEntry::Image(img) => Some(img),
            _ => None,
        }) {
            self.paste_clipboard_image(img, cx);
        }
    }

    pub(super) fn drop_files(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        self.paste_local_paths(paths.paths().to_vec(), cx);
    }

    /// Paste paths already spelled the way the pane's host reads them: into
    /// the composer as attachments when it has the keyboard, and as shell
    /// words onto the line otherwise. Every route that turns files into
    /// something to paste — a drop, a copied file, a screenshot, an upload to
    /// a remote pane — ends here.
    fn paste_paths(&mut self, spelled: Vec<String>, cx: &mut Context<Self>) {
        let Some(spelled) = self.composer_takes_paths(spelled, cx) else {
            return;
        };
        let text = pasted_paths_text(&spelled, self.shell_program().as_deref());
        self.paste(text, cx);
    }

    /// Paste files that live on this machine, spelled the way this machine
    /// names them, so the pane's program can open them.
    pub(super) fn paste_local_paths(
        &mut self,
        paths: Vec<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let spelled: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        self.paste_paths(spelled, cx);
    }

    fn paste_clipboard_image(&mut self, img: &gpui::Image, cx: &mut Context<Self>) {
        // A screenshot pasted into the composer is one of its attachments,
        // shown as a chip like a copied file. Forwarded as SYN it would land
        // in the agent's own input, hidden under the box.
        if self.composer_takes_files()
            && let Some(path) = write_clipboard_image(img)
        {
            self.paste_local_paths(vec![path], cx);
            return;
        }
        if self.paste_clipboard_image_as_path(img, cx) {
            return;
        }
        self.terminal.write(vec![0x16]);
        self.terminal.term.lock().selection = None;
        cx.notify();
    }

    /// Stage the clipboard image and paste a path for it, answering whether
    /// this took the paste over. `false` leaves the caller forwarding SYN —
    /// see [`stages_clipboard_image`] for when that is the better path.
    fn paste_clipboard_image_as_path(&mut self, img: &gpui::Image, cx: &mut Context<Self>) -> bool {
        let is_remote = false;
        if !stages_clipboard_image(is_remote) {
            return false;
        }
        let Some(path) = write_clipboard_image(img) else {
            return false;
        };
        // A macOS pane only gets here when it is remote — a local one returned
        // above — so pasting a path is right on every platform, and staying
        // silent would be the very no-op this route exists to avoid.
        self.paste_local_paths(vec![path], cx);
        true
    }

    /// One toast per failed open, so a broken `link_file_command` surfaces as a
    /// config problem instead of a "dead link" (#542). Spawn is all that is
    /// reported — a spawned opener that exits non-zero is nobody's to see.
    fn warn_file_open_failed(
        &self,
        path: &std::path::Path,
        reason: &std::io::Error,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::warn!("failed to open file link {}: {reason}", path.display());
        let path = path.display().to_string();
        let reason = reason.to_string();
        window.push_notification(
            crate::ui::host_ops::failure(
                crate::ui::i18n::t_fmt(
                    crate::ui::i18n::L10nKey::LinkFileOpenFailed,
                    &[("path", path.as_str()), ("error", reason.as_str())],
                ),
                &reason,
            ),
            cx,
        );
    }

    pub fn clear_scrollback(&mut self, cx: &mut Context<Self>) {
        use alacritty_terminal::vte::ansi::{ClearMode, Handler as _};

        self.cancel_scroll_anim();
        // Go through `clear_screen` rather than `grid_mut().clear_history()`:
        // it drops a selection anchored in the rows we are about to discard and
        // clamps the vi cursor back into the grid, which purging the history
        // behind the term's back would leave pointing at rows that no longer
        // exist.
        self.terminal.term.lock().clear_screen(ClearMode::Saved);
        // Image placements are anchored in absolute scrollback rows, so the
        // rows we just discarded moved every anchor. Drop them; the daemon does
        // not replay out-of-band image frames, so a browser redraws on its next
        // transmit.
        self.terminal.images().clear();
        self.scroll_frac = 0.;
        self.terminal.write(vec![0x0c_u8]);
        cx.notify();
    }

    pub fn set_font_family(&mut self, family: String, cx: &mut Context<Self>) {
        let mut font = gpui::font(family);
        if let Some(features) = &self.font_features {
            font.features = features.clone();
        }
        self.font = font;
        // Rebuild rather than carry the chain over: `fallback_chain` skips
        // pinning a last-resort face that the family already is, so the chain
        // that went with the old family can be missing a pin the new one needs.
        self.reread_fallback_chain(cx);
        cx.notify();
    }

    /// Rebuild the fallback chain from the config and put it on all three faces.
    ///
    /// The chain is built once in `with_terminal` and then only ever cloned
    /// around, so a `font_fallbacks` edit reaches new panes and no one else.
    pub fn reread_fallback_chain(&mut self, cx: &mut Context<Self>) {
        let chain = fallback_chain(&self.font.family, &cx.global::<Config>().font_fallbacks);
        apply_fallback_chain(
            chain,
            &mut self.font,
            &mut self.font_bold,
            &mut self.font_italic,
        );
    }

    pub fn set_font_family_bold(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        self.font_bold = self.alt_font(family);
        cx.notify();
    }

    pub fn set_font_family_italic(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        self.font_italic = self.alt_font(family);
        cx.notify();
    }

    pub fn set_font_features(
        &mut self,
        features: Option<gpui::FontFeatures>,
        cx: &mut Context<Self>,
    ) {
        self.font_features = features.clone();
        let apply = |font: &mut Font| {
            font.features = features.clone().unwrap_or_default();
        };
        apply(&mut self.font);
        if let Some(font) = &mut self.font_bold {
            apply(font);
        }
        if let Some(font) = &mut self.font_italic {
            apply(font);
        }
        cx.notify();
    }

    fn alt_font(&self, family: Option<String>) -> Option<Font> {
        family.map(|f| {
            let mut af = gpui::font(f);
            af.fallbacks = self.font.fallbacks.clone();
            if let Some(features) = &self.font_features {
                af.features = features.clone();
            }
            af
        })
    }

    fn poll_foreground(&mut self, window: &Window, cx: &mut Context<Self>) {
        let notify_allowed = cx
            .global::<Config>()
            .notify_on_command_finish
            .allows(window.is_window_active());
        // Ahead of the exit check: what a program said just before its shell
        // exited is still shown.
        self.show_program_notes(notify_allowed, window, cx);
        if self.terminal.exited {
            return;
        }
        let at_prompt = self.terminal.at_prompt();

        if at_prompt != self.last_at_prompt {
            self.last_at_prompt = at_prompt;
            cx.notify();
        }

        let running = !at_prompt;
        if running && self.running_agent.is_none() {
            self.running_agent = self.terminal.foreground_agent();
        }
        match (self.running_since, running) {
            (None, true) => {
                self.running_since = Some(std::time::Instant::now());
                self.running_title = self.title.clone();
                self.running_agent = self.terminal.foreground_agent();
            }
            (Some(start), false) => {
                let elapsed = start.elapsed();
                let title = std::mem::take(&mut self.running_title);
                let agent = self.running_agent.take();
                self.running_since = None;
                if notify_allowed {
                    match agent {
                        Some(_) if self.agent_was_rich => {}
                        Some(agent) => self.notify_agent_finished(agent, elapsed, cx),
                        None => {
                            let threshold = std::time::Duration::from_secs(
                                cx.global::<Config>().notify_threshold_secs,
                            );
                            if elapsed >= threshold {
                                self.notify_command_finished(&title, elapsed, cx);
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        self.poll_agent_status(notify_allowed, cx);

        let session = self.terminal.agent_session();
        let cwd_now = self
            .cwd_is_on_host()
            .then(|| {
                session
                    .as_ref()
                    .and_then(|s| s.cwd.clone())
                    .or_else(|| self.cwd())
            })
            .flatten();
        if cwd_now.as_ref() != self.git_status_cwd.as_ref() {
            self.git_status_cwd = cwd_now;
            cx.notify();
        }
    }

    fn poll_agent_status(&mut self, notify_allowed: bool, cx: &mut Context<Self>) {
        use crate::core::cli_agent::AgentStatus;

        let session = self.terminal.agent_session();
        if session.as_ref().is_some_and(|s| s.rich) {
            self.agent_was_rich = true;
        }
        if self.terminal.foreground_agent().is_none() && session.is_none() {
            self.agent_was_rich = false;
        }

        let status = session.as_ref().map(|s| s.status);
        if status == self.last_agent_status {
            return;
        }
        let prev = std::mem::replace(&mut self.last_agent_status, status);
        self.pending_finish_notice = None;
        let inferred = session.as_ref().is_some_and(|s| s.inferred);
        let rich = session.as_ref().is_some_and(|s| s.rich);
        let agent_name = self
            .terminal
            .foreground_agent()
            .map(|a| a.display_name())
            .unwrap_or("Agent");
        match status {
            Some(AgentStatus::Working) => {
                self.agent_turn_started = Some(std::time::Instant::now());
            }
            Some(AgentStatus::Waiting) if rich && notify_allowed => {
                let body = session
                    .as_ref()
                    .and_then(|s| s.message.clone())
                    .unwrap_or_else(|| t(L10nKey::NotifyAgentWaiting).to_string());
                self.notify_pane(Some(agent_name), &body, cx);
            }
            Some(AgentStatus::Done)
                if rich
                    && notify_allowed
                    && !inferred
                    && matches!(
                        prev,
                        Some(AgentStatus::Working) | Some(AgentStatus::Waiting)
                    ) =>
            {
                let body = match self.agent_turn_started.take() {
                    Some(start) => {
                        let secs = start.elapsed().as_secs().to_string();
                        t_fmt(L10nKey::NotifyAgentFinished, &[("secs", &secs)])
                    }
                    None => t(L10nKey::NotifyTurnFinished).to_string(),
                };
                // Held back briefly: a turn that ends only for the next one to
                // start at once — a message queued while the agent worked, a
                // `Stop` hook sending it back to work — is not finished, and
                // announcing it would be a notification about nothing.
                let agent_name = agent_name.to_string();
                self.pending_finish_notice = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(AGENT_DONE_SETTLE).await;
                    this.update(cx, |view, cx| {
                        view.pending_finish_notice = None;
                        view.notify_pane(Some(&agent_name), &body, cx);
                    })
                    .ok();
                }));
            }
            _ => {}
        }
        cx.notify();
    }

    pub(crate) fn on_alt_screen(&self) -> bool {
        self.terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::ALT_SCREEN)
    }
    /// Shift+Enter at the shell. Kitty (and a focused search field) encode
    /// the chord themselves; everyone else gets the legacy newline, which on
    /// ConPTY is the Ctrl+J pair a native console reader still understands.
    fn insert_newline_action(&mut self, cx: &mut Context<Self>) {
        if self.composer_focused
            || (self.search.is_some() && self.search_focused)
            || self.key_flags().kitty_active()
        {
            cx.propagate();
            return;
        }
        self.send_to_pty(self.key_flags().legacy_newline_bytes(), cx);
    }
    fn tab_pressed(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.composer_focused || self.search_focused {
            cx.propagate();
            return;
        }
        let bytes = self.tab_bytes(!forward);
        self.send_to_pty(&bytes, cx);
    }

    pub fn input_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.commit_text(text, cx);
    }
    pub fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.terminal.exited || text.is_empty() {
            return;
        }
        if self.composer_takes_typing(text, cx) {
            return;
        }
        // Typing goes to the prompt, so the view has to be looking at it.
        self.jump_to_prompt();
        self.terminal.write(text.as_bytes().to_vec());
        self.cursor_visible = true;
        cx.notify();
    }

    pub fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.marked_text = text;
        cx.notify();
    }

    pub fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if !self.marked_text.is_empty() {
            self.marked_text.clear();
            cx.notify();
        }
    }

    pub fn on_select_start(
        &mut self,
        col: usize,
        row: usize,
        left: bool,
        clicks: usize,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        let smart = cx.global::<Config>().smart_select;
        let mut term = self.terminal.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let point = Point::new(Line(row as i32 - display_offset), Column(col));
        let side = if left { Side::Left } else { Side::Right };
        if shift && clicks == 1 && term.selection.is_some() {
            if let Some(sel) = term.selection.as_mut() {
                sel.update(point, side);
            }
            drop(term);
            self.selecting = true;
            cx.notify();
            return;
        }
        let ty = match clicks {
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        let mut selection = Selection::new(ty, point, side);
        if clicks == 2
            && smart
            && let Some(r) = super::smart_select::grid_smart_range(&term, point)
        {
            let ty = if r.exact {
                SelectionType::Simple
            } else {
                SelectionType::Semantic
            };
            selection = Selection::new(ty, r.start, Side::Left);
            selection.update(r.end, Side::Right);
        }
        term.selection = Some(selection);
        drop(term);
        self.selecting = true;
        cx.notify();
    }

    pub fn on_select_update(&mut self, col: usize, row: usize, left: bool, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let mut term = self.terminal.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let point = Point::new(Line(row as i32 - display_offset), Column(col));
        let side = if left { Side::Left } else { Side::Right };
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, side);
        }
        drop(term);
        cx.notify();
    }

    pub fn select_autoscroll(
        &mut self,
        overshoot: f32,
        col: usize,
        left: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting || overshoot == 0. {
            self.drag_scroll = None;
            return;
        }
        let side = if left { Side::Left } else { Side::Right };
        let was_idle = self.drag_scroll.is_none();
        self.drag_scroll = Some(DragScroll {
            overshoot,
            col,
            side,
        });
        if !was_idle {
            return;
        }
        self.drag_scroll_epoch += 1;
        let epoch = self.drag_scroll_epoch;
        self.drag_scroll_tick(epoch, cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                if !matches!(
                    this.update(cx, |view, cx| view.drag_scroll_tick(epoch, cx)),
                    Ok(true)
                ) {
                    break;
                }
            }
        })
        .detach();
    }

    fn drag_scroll_tick(&mut self, epoch: u64, cx: &mut Context<Self>) -> bool {
        if epoch != self.drag_scroll_epoch {
            return false;
        }
        if !self.selecting {
            self.drag_scroll = None;
        }
        let Some(ds) = self.drag_scroll else {
            return false;
        };
        self.cancel_scroll_anim();
        let mut term = self.terminal.term.lock();
        let before = term.grid().display_offset();
        term.scroll_display(Scroll::Delta(drag_scroll_step(ds.overshoot)));
        let offset = term.grid().display_offset();
        let row = if ds.overshoot > 0. {
            0
        } else {
            term.screen_lines().saturating_sub(1)
        };
        let point = Point::new(Line(row as i32 - offset as i32), Column(ds.col));
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, ds.side);
        }
        drop(term);
        if offset != before {
            self.scroll_frac = 0.;
            cx.notify();
        }
        true
    }
    pub fn on_select_end(&mut self, cx: &mut Context<Self>) {
        let copy = select_end_copy(cx.global::<Config>().copy_on_select, self.selecting);
        self.selecting = false;
        self.drag_scroll = None;
        if copy == SelectEndCopy::Grid {
            self.copy_selection(false, cx);
        }
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let gesturing = self.track_scroll_gesture(ev.touch_phase);
        // One modifier turns the wheel into a zoom, the way it does in a
        // browser. Which one is the user's to say, because the default is the
        // platform modifier and on macOS that is a key half the world is
        // already holding for something else (#668).
        let wants_zoom = zoom_wheel(cx.global::<Config>().mouse_zoom_modifier, &ev.modifiers);
        // A trackpad gesture answers "scroll or zoom?" once, on its first
        // event, and keeps that answer until the stream dies — momentum tail
        // included. The tail is why: those events are the system's, not the
        // hand's, yet each one is stamped with whatever modifiers happen to be
        // down as it is delivered. Reaching for ⌘ during a flick's coast —
        // ⌘-Tab, ⌘-C, anything — would otherwise turn hundreds of coasting
        // lines into zoom steps and leave the font at its minimum, from a
        // gesture that was never a zoom (#912).
        // The answer is latched per gesture, not per stream: fingers going
        // back down ask it again. Carrying it over would be the same bug
        // wearing the other coat — one ⌘-zoom, and every later flick zooms
        // with nothing held at all, because `Started` keeps the gesture live
        // and would find the old answer still sitting there.
        if !gesturing || matches!(ev.touch_phase, gpui::TouchPhase::Started) {
            self.gesture_zoom = None;
            // Leftover travel belongs to the gesture that earned it; a new one
            // must not start already part-way to a step.
            self.zoom_debt = 0.;
        }
        let zoom = if gesturing {
            *self.gesture_zoom.get_or_insert(wants_zoom)
        } else {
            wants_zoom
        };
        if zoom {
            self.zoom_scroll(ev, gesturing, window, cx);
            return;
        }
        let mult = cx.global::<Config>().mouse_scroll_multiplier;
        let raw = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => p.y.as_f32() / self.line_height.as_f32(),
        };
        let delta = raw * mult;

        let quantized = !ev.modifiers.shift && {
            let mode = *self.terminal.term.lock().mode();
            mode.intersects(TermMode::MOUSE_MODE)
                || mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
        };
        if quantized {
            // Whole lines are what the application gets told about, so this path
            // cannot be spread over frames.
            self.cancel_scroll_anim();
            let total = self.scroll_debt + delta;
            let lines = total.trunc() as i32;
            self.scroll_debt = total - lines as f32;
            if lines != 0 {
                let at = self
                    .cell_geom
                    .map(|geom| geom.pos_to_cell(ev.position))
                    .map(|(col, row, _)| (col, row));
                self.scroll(lines, at, &ev.modifiers, cx);
            }
            return;
        }

        if self.should_animate_scroll(delta, gesturing, cx) {
            self.queue_scroll_anim(delta, window, cx);
        } else {
            self.cancel_scroll_anim();
            self.smooth_scroll(delta, cx);
        }
    }

    /// Resize the terminal font by whole steps under the platform modifier.
    ///
    /// The event never reaches the buffer, and never reaches the program
    /// running in it either: zooming is chrome, and showing a pane to someone
    /// standing behind you has to work the same whether or not what is running
    /// asked for the wheel. Steps go out as the same actions the keyboard and
    /// the View menu send, so the min/max clamp and the saved setting live in
    /// one place — the window handles `IncreaseFontSize` and `DecreaseFontSize`.
    fn zoom_scroll(
        &mut self,
        ev: &ScrollWheelEvent,
        gesturing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Whatever the scrollback still had in flight is dropped: it was
        // travelling in lines of a font that is about to change size.
        self.cancel_scroll_anim();
        let lines = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => p.y.as_f32() / self.line_height.as_f32(),
        };
        let (steps, debt) = zoom_scroll_steps(lines, self.zoom_debt, gesturing);
        self.zoom_debt = debt;
        for _ in 0..steps.unsigned_abs() {
            if steps > 0 {
                window.dispatch_action(Box::new(IncreaseFontSize), cx);
            } else {
                window.dispatch_action(Box::new(DecreaseFontSize), cx);
            }
        }
    }

    /// Track whether the pointing device is mid-gesture, which is what tells a
    /// trackpad apart from a wheel.
    ///
    /// Not the delta *type*: macOS reports a wheel mouse as pixels too — one
    /// notch arrives as a single ~100px event — so `Pixels` says nothing about
    /// the device. Phase does: only devices that can gesture ever report
    /// `Started`/`Ended`, and a wheel is `Moved` forever, on every platform.
    ///
    /// The gesture is held open on a timer rather than closed on `Ended`,
    /// because lifting the fingers is not the end of the stream — the momentum
    /// tail keeps delivering `Moved` events, larger than the gesture itself,
    /// and animating those would put a second layer of smoothing on scrolling
    /// the system is already smoothing.
    fn track_scroll_gesture(&mut self, phase: gpui::TouchPhase) -> bool {
        let now = std::time::Instant::now();
        let live = matches!(phase, gpui::TouchPhase::Started)
            || self.gesture_until.is_some_and(|until| now < until);
        self.gesture_until = live.then(|| now + SCROLL_GESTURE_IDLE);
        live
    }

    fn should_animate_scroll(&self, delta: f32, gesturing: bool, cx: &App) -> bool {
        if gesturing || !cx.global::<Config>().smooth_scroll {
            return false;
        }
        // Anything already in flight keeps accumulating, or a slow notch
        // arriving mid-animation would fight the frames still to come.
        self.scroll_anim.is_some() || delta.abs() >= SCROLL_ANIM_MIN_JUMP
    }

    /// Add `delta` to the in-flight animation, starting the frame loop if it is
    /// idle. Successive notches accumulate rather than restart, so spinning the
    /// wheel fast still lands exactly where the notches asked for.
    fn queue_scroll_anim(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(anim) = self.scroll_anim.as_mut() {
            anim.remaining += delta;
            return;
        }
        self.scroll_anim = Some(ScrollAnim {
            remaining: delta,
            last: std::time::Instant::now(),
        });
        self.scroll_anim_epoch += 1;
        let epoch = self.scroll_anim_epoch;
        schedule_scroll_anim_frame(cx.weak_entity(), epoch, window);
        // Ask for a frame right away: the callback chain's first step then
        // fires on the next display refresh even on a backend that only draws
        // when the window is dirty, and the callback keeps it dirty from then
        // on. Without this, a wheel notch queued on an idle, unfocused pane
        // could sit dormant instead of spreading over frames.
        cx.notify();
    }

    /// Drop whatever is left to travel. Every other way the display offset moves
    /// — jumping to a prompt, dragging a selection past the edge, the keyboard
    /// and mouse-reporting paths — has to come through here first, or the
    /// animation would keep walking away from wherever it put us.
    fn cancel_scroll_anim(&mut self) {
        if self.scroll_anim.take().is_some() {
            self.scroll_anim_epoch += 1;
        }
    }

    /// Advance the in-flight animation by the ground it covered since the
    /// previous frame. Runs at frame time — right before the next frame is
    /// drawn — so every presented frame shows exactly one decay step and no
    /// step is ever skipped or doubled against the monitor's cadence. Returns
    /// whether the animation is still going.
    fn scroll_anim_frame(&mut self, epoch: u64, cx: &mut Context<Self>) -> bool {
        if epoch != self.scroll_anim_epoch {
            return false;
        }
        let Some(anim) = self.scroll_anim.as_mut() else {
            return false;
        };
        let now = std::time::Instant::now();
        let dt = now.duration_since(anim.last);
        anim.last = now;
        let (step, last) = scroll_anim_step(anim.remaining, dt);
        anim.remaining -= step;
        if last {
            self.scroll_anim = None;
        }
        // Hitting the top or the bottom of the scrollback consumes nothing; the
        // remaining distance has nowhere to go, so stop instead of decaying it
        // against the clamp for another 100ms.
        let moved = self.smooth_scroll(step, cx);
        if !moved {
            self.cancel_scroll_anim();
            return false;
        }
        !last
    }

    /// Apply `delta` lines right now. Returns whether anything actually moved.
    fn smooth_scroll(&mut self, delta: f32, cx: &mut Context<Self>) -> bool {
        let mut term = self.terminal.term.lock();
        let offset = term.grid().display_offset();
        let max = term.grid().history_size();
        let (jump, frac) = smooth_scroll_step(offset, self.scroll_frac, delta, max);
        if jump != 0 {
            term.scroll_display(Scroll::Delta(jump));
        }
        drop(term);
        if jump != 0 || frac != self.scroll_frac {
            self.scroll_frac = frac;
            cx.notify();
            return true;
        }
        false
    }

    /// Settle up with the scrollback bar for this frame: move the viewport
    /// where a drag asked for, then tell the bar where the grid ended up.
    ///
    /// Both halves belong here rather than in the handle, so the bar — which
    /// runs from a mouse handler, with no pane to call into — never reaches
    /// into the terminal behind the pane's back.
    fn sync_scrollbar(&mut self) {
        if let Some(target) = self.scroll_handle.take_pending() {
            // Whatever the wheel had in flight was heading somewhere else.
            self.cancel_scroll_anim();
            let mut term = self.terminal.term.lock();
            let delta = target as i32 - term.grid().display_offset() as i32;
            if delta != 0 {
                term.scroll_display(Scroll::Delta(delta));
            }
            drop(term);
            // A sub-line remainder left over from a smooth wheel scroll would
            // paint the grid shifted off the row the thumb just picked.
            self.scroll_frac = 0.;
        }
        // Not worth a wait: the scrollbar is a picture of where the grid is,
        // and a frame that cannot have the lock keeps the picture it drew last
        // time rather than parking the whole window to refresh a thumb.
        let Some(term) = self.terminal.term.try_lock_unfair() else {
            return;
        };
        let grid = GridScroll {
            history: term.grid().history_size(),
            display_offset: term.grid().display_offset(),
            screen_lines: term.screen_lines(),
            line_height: self.line_height.as_f32(),
        };
        drop(term);
        self.scroll_handle.sync(grid);
    }

    /// Re-read the two things the frame itself declares — the terminal mode its
    /// keymap context is built from, and whether there is a selection to draw.
    ///
    /// One `try_lock` at the top of the frame, not one per reader: every
    /// caller inside `render` would otherwise take the lock separately, and
    /// each of those is another chance to sit behind the pane's reader with the
    /// whole window's frame in hand. Failing to get it leaves the previous
    /// frame's answers in place, which is the same bargain the grid makes in
    /// [`TerminalElement::build_grid`].
    fn sync_frame_facts(&mut self) {
        let Some(term) = self.terminal.term.try_lock_unfair() else {
            return;
        };
        self.frame_alt_screen = term.mode().contains(TermMode::ALT_SCREEN);
        self.frame_has_selection = term.selection.is_some();
    }

    /// The scrollback bar, laid down the right edge of the grid.
    ///
    /// The track is inset to the rows themselves — [`GRID_PAD_Y`] is padding
    /// the grid never scrolls through, and counting it would leave the thumb
    /// short of the ends by that much.
    fn render_scrollbar(&self) -> impl IntoElement + use<> {
        div()
            .absolute()
            .top(px(GRID_PAD_Y))
            .left_0()
            .right_0()
            .h(self.line_height * self.terminal.size().rows as f32)
            // No `scrollbar_show` override: the bar takes `cx.theme()`'s, which
            // `apply_theme` pins to `Scrolling` for every list in the app. A
            // pane disagreeing with every other list about when a scrollbar is worth
            // showing would be the odd one out.
            .child(Scrollbar::vertical(&self.scroll_handle).id("terminal-scrollbar"))
    }

    fn grid_line(
        term: &alacritty_terminal::Term<crate::terminal::remote::EventProxy>,
        row: usize,
    ) -> Option<Line> {
        let line = Line(row as i32 - term.grid().display_offset() as i32);
        (line >= term.topmost_line() && line <= term.bottommost_line()).then_some(line)
    }

    pub fn open_link_at(
        &mut self,
        col: usize,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !cx.global::<Config>().link_url {
            return false;
        }
        match self.resolve_link_at(col, row, true, cx) {
            LinkAt::Found(LinkTarget::Url(url), ..) => self.open_url(&url, window, cx),
            LinkAt::Found(
                LinkTarget::File {
                    path,
                    line,
                    column,
                    is_dir,
                },
                ..,
            ) => self.open_file_link(path, line, column, is_dir, window, cx),
            LinkAt::Unresolved { candidate } => {
                return self.report_unresolved_link(&candidate, window, cx);
            }
            LinkAt::None => return false,
        }
        true
    }

    /// Latches the file link under a right mouse-down for the context menu.
    ///
    /// Resolving here rather than in the menu builder is what lets the menu
    /// name a real file: the builder runs a turn later, with no event and no
    /// pointer, and asking the grid then would be asking about wherever the
    /// mouse has since gone.
    pub fn record_menu_link(&mut self, col: usize, row: usize, cx: &mut Context<Self>) {
        // The same switch that decides whether a path underlines and whether a
        // click follows one. Without this the menu would go on offering to
        // open files in a pane where link detection is turned off.
        if !cx.global::<Config>().link_url {
            self.menu_link = None;
            return;
        }
        self.menu_link = match self.resolve_link_at(col, row, true, cx) {
            LinkAt::Found(target @ LinkTarget::File { .. }, ..) => Some(target),
            _ => None,
        };
    }

    /// Drops a latched link, for a right click the application is taking.
    pub fn forget_menu_link(&mut self) {
        self.menu_link = None;
    }

    /// The path the context menu is about, if it is about one.
    fn menu_link_path(&self) -> Option<&std::path::Path> {
        match self.menu_link.as_ref()? {
            LinkTarget::File { path, .. } => Some(path),
            LinkTarget::Url(_) => None,
        }
    }

    fn open_menu_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(LinkTarget::File {
            path,
            line,
            column,
            is_dir,
        }) = self.menu_link.clone()
        else {
            return;
        };
        self.open_file_link(path, line, column, is_dir, window, cx);
    }

    /// "Open with Default App": the file link under the menu, handed to the
    /// OS association whatever `link_file_open` says "Open" does.
    fn open_menu_link_with_default_app(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The menu only offers this for a local pane; a key binding could
        // still fire it on a remote one, where the path means nothing here.
        if !self.host_id.is_local() {
            return;
        }
        let Some(path) = self.menu_link_path().map(std::path::Path::to_path_buf) else {
            return;
        };
        if let Err(e) = open_file_path(&path) {
            self.warn_file_open_failed(&path, &e, window, cx);
        }
    }

    fn reveal_menu_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.menu_link_path().map(std::path::Path::to_path_buf) else {
            return;
        };
        if let Err(e) = reveal_file_path(&path) {
            self.warn_file_open_failed(&path, &e, window, cx);
        }
    }

    fn copy_menu_link_path(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.menu_link_path() else {
            return;
        };
        let text = path.to_string_lossy().into_owned();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// Hands a resolved file link to whatever the user wants opening files.
    ///
    /// A local path opens with the program the OS has it associated with,
    /// unless `link_file_command` names one. A path on another machine has
    /// nothing here that can show it.
    fn open_file_link(
        &mut self,
        path: std::path::PathBuf,
        line: Option<u32>,
        column: Option<u32>,
        _is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cfg = cx.global::<Config>();
        let mode = cfg.file_open_mode();
        let command = cfg.link_file_command.clone();
        // A path that was just resolved on another machine means nothing to a
        // local opener: `open` and a user's `code --goto` would both be handed
        // a path this filesystem never had.
        if !self.host_id.is_local() {
            let shown = path.display().to_string();
            window.push_notification(t_fmt(L10nKey::LinkFileMissing, &[("path", &shown)]), cx);
            return;
        }
        // Both external arms have to answer for a failed spawn (#542): a
        // misspelled `link_file_command` or a missing opener used to hit only
        // the log, and the link read as dead — while the path was only ever
        // underlined because it verifiably exists.
        let outcome = match (mode, command) {
            (LinkFileOpen::Command, Some(template)) => {
                run_file_command(&template, &path, line, column)
            }
            _ => open_file_path(&path),
        };
        if let Err(e) = outcome {
            self.warn_file_open_failed(&path, &e, window, cx);
        }
    }

    /// Says why a path-shaped token did not open anything.
    ///
    /// Silence here is what makes the whole feature feel broken: a relative
    /// path measured from somewhere other than this pane's directory looks
    /// exactly like one that works, right up until the click does nothing.
    /// Returns whether the click was spoken for.
    fn report_unresolved_link(
        &mut self,
        candidate: &super::search::FileCandidate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let roots = self.link_roots(cx);
        // A word that is not written like a path was never a link, and saying
        // so on every modifier-click over ordinary output would be noise.
        if !candidate.looks_like_a_path(roots.style) {
            return false;
        }
        // An absolute or `~`-rooted path was never measured from anywhere, so
        // naming a directory it was "looked for under" would send the user to
        // somewhere nothing was ever asked about.
        let rooted = candidate.is_rooted(roots.style);
        let root = match rooted {
            true => None,
            false => roots.dirs.into_iter().next(),
        };
        let message = match root {
            Some(root) => t_fmt(
                L10nKey::LinkFileNotUnder,
                &[
                    ("path", &candidate.path),
                    ("dir", &root.display().to_string()),
                ],
            ),
            None if rooted => t_fmt(L10nKey::LinkFileMissing, &[("path", &candidate.path)]),
            None => t_fmt(L10nKey::LinkFileNoDirectory, &[("path", &candidate.path)]),
        };
        window.push_notification(message, cx);
        true
    }

    fn open_url(&self, url: &str, _window: &mut Window, cx: &mut Context<Self>) {
        cx.open_url(url);
    }

    pub fn hover_link_at(
        &mut self,
        col: usize,
        row: usize,
        armed: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        // A mouse crossing a pane lands on the same cell many times over.
        // Nothing about the answer depends on where inside the cell the
        // pointer is, so the work is worth doing once.
        if self.last_hover_cell == Some((col, row)) && self.link_modifier_down == armed {
            return self.hovered_link.is_some();
        }
        self.last_hover_cell = Some((col, row));
        self.link_modifier_down = armed;
        if !cx.global::<Config>().link_url {
            self.clear_hovered_link(cx);
            return false;
        }
        // A full-screen application drew what is on the grid and is watching
        // the mouse itself, so pointing things out inside it is tty7 drawing
        // on somebody else's window. Holding the modifier says the user wants
        // tty7's reading of the screen anyway, and then it is theirs to have.
        if !armed && self.on_alt_screen() {
            if self.hovered_link.take().is_some() {
                cx.notify();
            }
            return false;
        }
        // Most of a screen is blanks, and reading one cell costs a fraction
        // of lifting a whole soft-wrapped logical line out of the grid.
        if self.cell_is_blank(col, row) {
            if self.hovered_link.take().is_some() {
                cx.notify();
            }
            return false;
        }
        let next = self.link_span_at(col, row, armed, cx);
        if next != self.hovered_link {
            self.hovered_link = next;
            cx.notify();
        }
        self.hovered_link.is_some()
    }

    /// Whether the cell under the pointer holds anything a link could be made
    /// of.
    fn cell_is_blank(&self, col: usize, row: usize) -> bool {
        use alacritty_terminal::term::cell::Flags;

        let term = self.terminal.term.lock();
        let Some(line) = Self::grid_line(&term, row) else {
            return true;
        };
        if col >= term.columns() {
            return true;
        }
        let cell = &term.grid()[line][Column(col)];
        // The second column of a wide glyph is written as a space, and the
        // logical line hands a click there back to the character that owns it.
        // Reading it as empty would drop the underline on every other column
        // of a path spelled in CJK or emoji.
        if cell.flags.intersects(
            Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER | Flags::WIDE_CHAR,
        ) {
            return false;
        }
        cell.c.is_whitespace()
    }

    #[cfg(test)]
    pub fn refresh_link_hover(&mut self, armed: bool, cx: &mut Context<Self>) -> bool {
        let Some((col, row)) = self.last_hover_cell else {
            self.link_modifier_down = armed;
            return false;
        };
        self.hover_link_at(col, row, armed, cx)
    }

    /// Runs the hover again from scratch, for when the answer may have
    /// changed under a mouse that never moved: a probe landing, a repository
    /// root arriving.
    fn recompute_link_hover(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((col, row)) = self.last_hover_cell.take() else {
            return false;
        };
        self.hover_link_at(col, row, self.link_modifier_down, cx)
    }

    pub fn link_modifier_down(&self) -> bool {
        self.link_modifier_down
    }

    pub fn clear_hovered_link(&mut self, cx: &mut Context<Self>) {
        self.last_hover_cell = None;
        if self.hovered_link.take().is_some() {
            cx.notify();
        }
    }

    fn link_span_at(
        &mut self,
        col: usize,
        row: usize,
        armed: bool,
        cx: &mut Context<Self>,
    ) -> Option<HoveredLink> {
        match self.resolve_link_at(col, row, true, cx) {
            LinkAt::Found(_, runs) => Some(HoveredLink { runs, armed }),
            LinkAt::Unresolved { .. } | LinkAt::None => None,
        }
    }

    fn resolve_link_at(
        &mut self,
        col: usize,
        row: usize,
        include_files: bool,
        cx: &mut Context<Self>,
    ) -> LinkAt {
        let Some(line) = self.link_line_at(col, row) else {
            return LinkAt::None;
        };
        let (text, points, click_idx) = match line {
            GridLink::Hyperlink(uri, runs) => {
                return LinkAt::Found(LinkTarget::Url(uri), runs);
            }
            GridLink::Text(text, points, click_idx) => (text, points, click_idx),
        };

        // A pane whose paths belong to neither its host nor this machine —
        // an `ssh` typed into a local shell — has nothing that can answer for
        // them. It used to fall through to the local filesystem, so an
        // absolute path that happened to exist on both ends opened *this*
        // machine's copy without a word.
        let files = include_files && self.cwd_is_on_host();
        let roots = match files {
            true => self.link_roots(cx),
            false => super::search::LinkRoots::default(),
        };
        let mut probe = |path: &std::path::Path, require_file: bool| {
            super::search::local_probe(path, require_file)
        };
        let link = super::search::link_at(&text, click_idx, &roots, files, &mut probe);
        match link {
            Some(link) => {
                let term = self.terminal.term.lock();
                let runs = super::smart_select::row_runs(&term, &points[link.start..=link.end]);
                LinkAt::Found(link.target, runs)
            }
            // Nothing answered. Hand back what the token *said* so a click can
            // say so out loud instead of looking broken.
            None => match files
                .then(|| super::search::unresolved_candidate(&text, click_idx, roots.style))
                .flatten()
            {
                Some(candidate) => LinkAt::Unresolved { candidate },
                None => LinkAt::None,
            },
        }
    }

    /// The logical line under a cell, or the OSC 8 hyperlink that cell
    /// declares — which wins outright, because the emitter said what it meant.
    fn link_line_at(&self, col: usize, row: usize) -> Option<GridLink> {
        let term = self.terminal.term.lock();
        let line = Self::grid_line(&term, row)?;
        if col >= term.columns() {
            return None;
        }
        let click = Point::new(line, Column(col));

        if let Some(hl) = term.grid()[line][Column(col)].hyperlink() {
            let uri = hl.uri().to_string();
            if let Some((start, end)) = super::smart_select::hyperlink_run(&term, click) {
                let runs = super::smart_select::grid_runs(start, end, term.columns());
                return Some(GridLink::Hyperlink(uri, runs));
            }
        }

        let (text, points, click_idx) = super::smart_select::logical_line_at(&term, click, true)?;
        Some(GridLink::Text(text, points, click_idx))
    }

    /// The directories a relative path in this pane is measured from, best
    /// first: where the work is happening, then the repository around it.
    ///
    /// Cargo, tsc and friends name files from the workspace root while the
    /// shell sits in a member directory, so one root resolves only half of
    /// what a build prints. The pane's own directory stays first, so a name
    /// that exists in both is the near one.
    fn link_roots(&mut self, cx: &mut Context<Self>) -> super::search::LinkRoots {
        let local_home = self.host_id.is_local();
        let style = self.link_path_style();
        let Some(cwd) = self.effective_host_cwd() else {
            return super::search::LinkRoots {
                dirs: Vec::new(),
                local_home,
                style,
            };
        };
        self.request_link_repo_root(&cwd, cx);
        let mut dirs = vec![cwd.clone()];
        if let Some((asked_for, Some(root))) = &self.link_repo_root
            && *asked_for == cwd
            && *root != cwd
        {
            dirs.push(root.clone());
        }
        super::search::LinkRoots {
            dirs,
            local_home,
            style,
        }
    }

    /// Which path dialect this pane's output is written in — see
    /// [`link_path_style`].
    fn link_path_style(&self) -> super::search::PathStyle {
        link_path_style(self.paths_are_local(), self.effective_host_cwd().as_deref())
    }

    fn request_link_repo_root(&mut self, cwd: &std::path::Path, cx: &mut Context<Self>) {
        if self.link_repo_root_pending
            || self
                .link_repo_root
                .as_ref()
                .is_some_and(|(asked_for, _)| asked_for == cwd)
        {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        self.link_repo_root_pending = true;
        let cwd = cwd.to_path_buf();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            {
                let cwd = cwd.clone();
                move |h| h.repo_root(&cwd).ok().flatten()
            },
            move |view, root, cx| {
                view.link_repo_root_pending = false;
                view.link_repo_root = Some((cwd, root));
                view.recompute_link_hover(cx);
            },
        );
    }
}

/// Why closing a pane would end work that is still going on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneBusy {
    Command(String),
    Agent(&'static str),
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_frame_facts();
        self.sync_scrollbar();
        self.sync_composer(window, cx);
        let entity = cx.entity();
        let search_bar = self
            .search
            .as_ref()
            .map(|s| self.render_search_bar(s, window, cx));

        let (composer_docked, composer_over) = match self.render_composer(window, cx) {
            Some((el, true)) => (Some(el), None),
            Some((el, false)) => (None, Some(el)),
            None => (None, None),
        };
        let menu_focus = self.focus_handle.clone();
        let has_selection = self.any_selection();
        let menu_view = cx.entity();

        div()
            .id("terminal-surface")
            // The surface, not the grid inside it, is what carries the role:
            // a11y focus is only ever reported for a `div` that tracks a focus
            // handle *and* has a node of its own, so a terminal with no role
            // here is a window whose focused element is the window.
            .role(gpui::Role::MultilineTextInput)
            .track_focus(&self.focus_handle)
            .key_context(self.key_context())
            .size_full()
            .relative()
            // A column so the composer docks under the grid and takes its rows
            // from it: the grid's laid-out height is what sizes the pty.
            .flex()
            .flex_col()
            .overflow_hidden()
            .px(px(GRID_PAD_X))
            .py(px(GRID_PAD_Y))
            .text_color(cx.theme().foreground)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                    if window.default_prevented() {
                        return;
                    }
                    window.focus(&this.focus_handle, cx);
                }),
            )
            // Latch the context-menu verdict for this click. gpui-component's
            // `ContextMenu` element owns the right mouse-down that opens the
            // popup and hands the builder no event to inspect, so the modifiers
            // have to be recorded here. That element wraps this one, so its
            // listener actually fires *before* this one — harmless, because it
            // only builds the menu from a `window.defer` callback that runs
            // once the whole mouse dispatch has unwound.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, _window, _cx| {
                    this.context_menu_allowed =
                        should_show_context_menu(this.mouse_mode(), ev.modifiers.shift);
                }),
            )
            .drag_over::<ExternalPaths>(|s, _, _, cx| s.bg(cx.theme().drag_border.opacity(0.12)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.drop_files(paths, cx);
            }))
            .on_action(cx.listener(|this, _: &CopyText, _w, cx| {
                this.copy_contextual(false, false, cx);
            }))
            .on_action(cx.listener(|this, _: &CopyRaw, _w, cx| {
                // Nothing selected: let the chord reach the key handler.
                if !this.copy_contextual(false, true, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &CutText, _w, cx| {
                // Nothing to cut: the chord belongs to the shell.
                if !this.cut_contextual(cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &PasteText, _w, cx| this.paste_from_clipboard(cx)))
            .on_action(cx.listener(|this, _: &AlternatePaste, _w, cx| this.alternate_paste(cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _w, cx| this.select_all_contextual(cx)))
            .on_action(cx.listener(|this, _: &UndoEdit, _w, cx| this.undo_edit(false, cx)))
            .on_action(cx.listener(|this, _: &RedoEdit, _w, cx| this.undo_edit(true, cx)))
            .on_action(
                cx.listener(|this, _: &FindInTerminal, window, cx| this.open_search(window, cx)),
            )
            // Off macOS these two live on F3 and Shift+F3, which is also where
            // PSReadLine keeps CharacterSearch and readline users put their
            // own widgets. With no find bar open there is no next match to
            // step to, so the keystroke is given back the way `EditorSave`
            // gives back Ctrl+S — otherwise the action swallows the key and
            // the shell never sees it (#834).
            .on_action(cx.listener(|this, _: &FindNext, _w, cx| {
                if this.search.is_none() {
                    cx.propagate();
                    return;
                }
                this.step_match(Direction::Right, cx);
            }))
            .on_action(cx.listener(|this, _: &FindPrevious, _w, cx| {
                if this.search.is_none() {
                    cx.propagate();
                    return;
                }
                this.step_match(Direction::Left, cx);
            }))
            .on_action(cx.listener(|this, _: &ClearScrollback, _w, cx| this.clear_scrollback(cx)))
            .on_action(cx.listener(|this, _: &OpenLinkUnderPointer, window, cx| {
                this.open_menu_link(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenLinkWithDefaultApp, window, cx| {
                this.open_menu_link_with_default_app(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RevealLinkUnderPointer, window, cx| {
                this.reveal_menu_link(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopyLinkPathUnderPointer, _w, cx| {
                this.copy_menu_link_path(cx);
            }))
            .on_action(cx.listener(|this, _: &InsertNewline, _w, cx| {
                this.insert_newline_action(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleComposer, window, cx| {
                this.toggle_composer(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SendTab, _w, cx| {
                this.tab_pressed(true, cx);
            }))
            .on_action(cx.listener(|this, _: &SendBackTab, _w, cx| {
                this.tab_pressed(false, cx);
            }))
            .child(TerminalElement::new(entity))
            .children(composer_docked)
            .child(self.render_scrollbar())
            .children(search_bar)
            .children(composer_over)
            .context_menu(move |menu, _window, cx| {
                // Suppressing the popup means handing back an item-less menu:
                // gpui-component's `ContextMenu` element skips rendering the
                // anchored overlay entirely when the built menu `is_empty()`,
                // and its right mouse-down listener is registered after ours
                // (it wraps this element), so we cannot out-order it or veto it
                // any earlier. See `should_show_context_menu` for the rule.
                if !menu_view.read(cx).context_menu_allowed {
                    return menu;
                }
                let view = menu_view.read(cx);
                // A path is the most specific thing the pointer can be on, so
                // what it can do goes above what the pane can do.
                let menu = match view.menu_link_path() {
                    Some(path) => {
                        let local = view.host_id.is_local();
                        let reveal = match cfg!(target_os = "macos") {
                            true => L10nKey::AppMenuRevealInFinder,
                            false => L10nKey::AppMenuRevealInFolder,
                        };
                        let label = link_menu_label(path);
                        let default_app =
                            offers_default_app_open(local, cx.global::<Config>().file_open_mode());
                        menu.min_w(px(220.))
                            .action_context(menu_focus.clone())
                            .label(label)
                            .menu(t(L10nKey::AppMenuOpenLink), Box::new(OpenLinkUnderPointer))
                            .when(default_app, |menu| {
                                menu.menu(
                                    t(L10nKey::AppMenuOpenLinkWithDefaultApp),
                                    Box::new(OpenLinkWithDefaultApp),
                                )
                            })
                            // A file on another machine has no folder here to
                            // show it in, and naming this one's would show
                            // whatever it happens to keep at that path.
                            .menu_element_with_disabled(
                                Box::new(RevealLinkUnderPointer),
                                !local,
                                menu_row_with_hint(t(reveal), None),
                            )
                            .menu(
                                t(L10nKey::AppMenuCopyLinkPath),
                                Box::new(CopyLinkPathUnderPointer),
                            )
                            .separator()
                    }
                    None => menu,
                };
                menu.min_w(px(240.))
                    .action_context(menu_focus.clone())
                    .menu_element_with_disabled(
                        Box::new(CopyText),
                        !has_selection,
                        menu_row_with_hint(t(L10nKey::AppMenuCopy), mac_only("secondary-c")),
                    )
                    .menu_element(
                        Box::new(PasteText),
                        menu_row_with_hint(t(L10nKey::AppMenuPaste), mac_only("secondary-v")),
                    )
                    .menu_element(
                        Box::new(SelectAll),
                        menu_row_with_hint(t(L10nKey::AppMenuSelectAll), mac_only("secondary-a")),
                    )
                    .separator()
                    .menu(t(L10nKey::AppMenuFind), Box::new(FindInTerminal))
                    // Inside the terminal, what gets cleared goes without
                    // saying; the menu bar keeps the full "Clear Scrollback".
                    .menu(t(L10nKey::TerminalContextClear), Box::new(ClearScrollback))
                    .separator()
                    .menu(t(L10nKey::CmdNewWindow), Box::new(NewWindow))
                    .menu(t(L10nKey::CmdCloseWindow), Box::new(CloseWindow))
            })
    }
}

/// The header the link section of the context menu wears: the file's own
/// name, so a menu opened over a long path says which one it is about without
/// making the menu as wide as the path.
fn link_menu_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn menu_row_with_hint(
    label: &'static str,
    key: Option<&'static str>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyElement {
    move |_window, _cx| {
        let hint = key.map(|k| {
            Kbd::new(gpui::Keystroke::parse(k).expect("valid static keystroke"))
                .p_0()
                .flex_nowrap()
                .border_0()
                .bg(gpui::transparent_white())
        });
        h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .justify_between()
            .child(label)
            .children(hint)
            .into_any_element()
    }
}

#[cfg(target_os = "macos")]
fn mac_only(key: &'static str) -> Option<&'static str> {
    Some(key)
}
#[cfg(not(target_os = "macos"))]
fn mac_only(_key: &'static str) -> Option<&'static str> {
    None
}

#[derive(Debug, PartialEq)]
enum WheelRoute {
    Report { base: u8 },
    Arrows { seq: &'static [u8] },
    Scrollback,
}

fn wheel_route(mode: TermMode, shift: bool, up: bool) -> WheelRoute {
    if !shift && mode.intersects(TermMode::MOUSE_MODE) {
        return WheelRoute::Report {
            base: if up { 64 } else { 65 },
        };
    }
    if !shift && mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        let seq: &'static [u8] = match (up, mode.contains(TermMode::APP_CURSOR)) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        return WheelRoute::Arrows { seq };
    }
    WheelRoute::Scrollback
}

#[derive(Debug, PartialEq)]
enum SelectEndCopy {
    None,
    Grid,
}

/// Whether a right-click belongs to tty7's own context menu rather than to the
/// terminal application.
///
/// A TUI that has turned mouse reporting on (vim with `set mouse=a`, lazygit,
/// tmux, …) draws its own right-button menus, so popping the host menu on top
/// of the application's is a double delivery of one click — #251. While
/// reporting is active the unmodified click is the application's alone; Shift
/// is the escape hatch that reaches tty7, matching how Shift already overrides
/// mouse reporting for selection (`register_mouse_handlers`) and for the wheel
/// (`wheel_route`).
///
/// This is the exact complement of the "forward the press to the app" branch in
/// `TerminalElement::register_mouse_handlers`, which is why both call it: one
/// click must never feed both consumers.
pub(super) fn should_show_context_menu(mouse_mode: bool, shift: bool) -> bool {
    !mouse_mode || shift
}
fn select_end_copy(enabled: bool, grid: bool) -> SelectEndCopy {
    if enabled && grid {
        SelectEndCopy::Grid
    } else {
        SelectEndCopy::None
    }
}

/// Whether a file link's menu offers "Open with Default App" beside "Open".
///
/// Only for a file on this machine: the OS opener is handed a local path, and
/// a remote pane's path names nothing here — or worse, this machine's copy.
/// And not when "Open" already goes to the OS association, where the two rows
/// would do the same thing.
fn offers_default_app_open(host_is_local: bool, open_mode: LinkFileOpen) -> bool {
    host_is_local && open_mode != LinkFileOpen::System
}

/// Hands a path to whatever the OS has it associated with. Also the fallback
/// for a directory the file tree cannot reach.
///
/// The path is spelled this OS's way first: Explorer takes `c:/a.png` as a
/// request to show Documents.
pub(crate) fn open_file_path(path: &std::path::Path) -> std::io::Result<()> {
    let path = &super::search::spelled_for(super::search::PathStyle::NATIVE, path);
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    tty7_core::core::proc::spawn_reaped(std::process::Command::new(opener).arg(path))?;
    Ok(())
}

/// Shows a path where it lives, rather than opening it.
///
/// `open -R` and `explorer /select,` both select the file inside its folder;
/// no desktop-neutral Linux equivalent exists, so there the folder is opened
/// and the file is left for the eye to find.
pub(crate) fn reveal_file_path(path: &std::path::Path) -> std::io::Result<()> {
    let path = &super::search::spelled_for(super::search::PathStyle::NATIVE, path);
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg("-R").arg(path);
        c
    };
    // Explorer wants `/select,` bare and the path quoted behind it. `arg`
    // quotes the whole thing the moment the path holds a space, and Explorer
    // answers a quoted switch by opening Documents and reporting success —
    // so the command line is written out by hand.
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new("explorer");
        c.raw_arg(format!("/select,\"{}\"", path.display()));
        c
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path.parent().unwrap_or(path));
        c
    };
    tty7_core::core::proc::spawn_reaped(&mut command)?;
    Ok(())
}

fn run_file_command(
    template: &str,
    path: &std::path::Path,
    line: Option<u32>,
    column: Option<u32>,
) -> std::io::Result<()> {
    let argv = expand_file_command_template(template, path, line, column);
    let Some((program, args)) = argv.split_first() else {
        // Sanitize maps a blank template to None, so the only way here is a
        // template whose tokens all expand to nothing (a lone `{line}` on a
        // link with no line number). Still a config error worth reporting.
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "link_file_command expanded to nothing",
        ));
    };
    tty7_core::core::proc::spawn_reaped(std::process::Command::new(program).args(args))?;
    Ok(())
}

fn expand_file_command_template(
    template: &str,
    path: &std::path::Path,
    line: Option<u32>,
    column: Option<u32>,
) -> Vec<String> {
    let path = path.to_string_lossy();
    template
        .split_whitespace()
        .filter_map(|token| expand_file_command_token(token, &path, line, column))
        .collect()
}

fn expand_file_command_token(
    token: &str,
    path: &str,
    line: Option<u32>,
    column: Option<u32>,
) -> Option<String> {
    let mut out = String::with_capacity(token.len());
    let mut rest = token;
    while let Some(open) = rest.find('{') {
        let Some(close_rel) = rest[open..].find('}') else {
            break;
        };
        let close = open + close_rel;
        out.push_str(&rest[..open]);
        let value = match &rest[open + 1..close] {
            "path" => Some(path.to_string()),
            "line" => line.map(|l| l.to_string()),
            "column" => column.map(|c| c.to_string()),
            other => Some(format!("{{{other}}}")),
        };
        out.push_str(&value?);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Some(out)
}

fn encode_mouse(
    sgr: bool,
    base: u8,
    mods: &Modifiers,
    col: usize,
    row: usize,
    pressed: bool,
) -> Option<Vec<u8>> {
    let mut mod_bits = 0u8;
    if mods.shift {
        mod_bits += 4;
    }
    if mods.alt {
        mod_bits += 8;
    }
    if mods.control {
        mod_bits += 16;
    }

    if sgr {
        let c = if pressed { 'M' } else { 'm' };
        let msg = format!("\x1b[<{};{};{}{}", base + mod_bits, col + 1, row + 1, c);
        Some(msg.into_bytes())
    } else {
        if col >= 223 || row >= 223 {
            return None;
        }
        let code = if pressed {
            base + mod_bits
        } else {
            3 + mod_bits
        };
        Some(vec![
            0x1b,
            b'[',
            b'M',
            32 + code,
            (32 + 1 + col) as u8,
            (32 + 1 + row) as u8,
        ])
    }
}

fn focus_report_bytes(mode: TermMode, focused: bool) -> Option<&'static [u8]> {
    if !mode.contains(TermMode::FOCUS_IN_OUT) {
        return None;
    }
    Some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

/// Whether this wheel event is a zoom rather than a scroll.
///
/// Exactly one modifier, and it has to be the configured one: anything
/// alongside it is somebody else's gesture — shift in particular is the escape
/// hatch that scrolls the scrollback out from under a mouse-reporting program.
///
/// Off macOS the platform modifier *is* Ctrl, so `Platform` and `Ctrl` describe
/// the same key there; the setting still round-trips, so a config file shared
/// with a Mac keeps meaning what it meant.
fn zoom_wheel(modifier: MouseZoomModifier, mods: &Modifiers) -> bool {
    if mods.number_of_modifiers() != 1 {
        return false;
    }
    match modifier {
        MouseZoomModifier::Platform => mods.secondary(),
        MouseZoomModifier::Ctrl => mods.control,
        MouseZoomModifier::Alt => mods.alt,
        MouseZoomModifier::None => false,
    }
}

/// How many font-size steps a zoom event is worth, and what is left over for
/// the next one.
///
/// A wheel detent is a discrete click of intent, so it is one step whatever the
/// platform says it covers — macOS calls a single notch five lines, and five
/// points of font per notch would be unusable. A trackpad has no detents and
/// spends a flick over dozens of events, so those accumulate and only pay out
/// once the fingers have travelled [`ZOOM_SCROLL_LINES`].
fn zoom_scroll_steps(lines: f32, debt: f32, gesturing: bool) -> (i32, f32) {
    if !gesturing {
        let step = if lines > 0. {
            1
        } else if lines < 0. {
            -1
        } else {
            0
        };
        return (step, 0.);
    }
    let total = debt + lines;
    let steps = (total / ZOOM_SCROLL_LINES).trunc();
    (steps as i32, total - steps * ZOOM_SCROLL_LINES)
}

fn smooth_scroll_step(offset: usize, frac: f32, delta: f32, max: usize) -> (i32, f32) {
    let pos = (offset as f32 + frac + delta).clamp(0., max as f32);
    let new_offset = pos.floor();
    (new_offset as i32 - offset as i32, pos - new_offset)
}

/// Advance the in-flight scroll animation once per presented frame.
///
/// Registered from [`TerminalView::queue_scroll_anim`]: gpui runs the callback
/// immediately before the next frame is drawn, and it re-registers itself as
/// long as the animation lives. Stepping at frame time instead of on a
/// free-running timer keeps the decay steps locked to the monitor's vblank
/// cadence — a 16 ms timer drifting against a 16.67 ms vblank makes some
/// presented frames show twice the movement of their neighbours and others
/// none, which reads as stutter. Frame-aligned stepping shows every frame
/// exactly the ground it covered. The weak handle and the epoch guard make the
/// chain die quietly when the pane is closed or the animation is cancelled.
fn schedule_scroll_anim_frame(view: WeakEntity<TerminalView>, epoch: u64, window: &mut Window) {
    window.on_next_frame(move |window, cx| {
        let Some(view) = view.upgrade() else {
            return;
        };
        if view.update(cx, |view, cx| view.scroll_anim_frame(epoch, cx)) {
            schedule_scroll_anim_frame(view.downgrade(), epoch, window);
        }
    });
}

/// How much of `remaining` to consume this frame, and whether this is the last
/// one. Decay is scaled by the real elapsed time so a dropped frame covers the
/// ground it missed instead of stretching the animation out.
fn scroll_anim_step(remaining: f32, dt: std::time::Duration) -> (f32, bool) {
    if remaining.abs() <= SCROLL_ANIM_MIN {
        return (remaining, true);
    }
    let frames = (dt.as_secs_f32() / SCROLL_ANIM_FRAME.as_secs_f32()).clamp(0.1, 8.);
    let consumed = 1. - (1. - SCROLL_ANIM_SMOOTH).powf(frames);
    let step = remaining * consumed;
    if (remaining - step).abs() <= SCROLL_ANIM_MIN {
        (remaining, true)
    } else {
        (step, false)
    }
}

fn drag_scroll_step(overshoot: f32) -> i32 {
    let lines = overshoot.abs().ceil().clamp(1., 8.) as i32;
    if overshoot < 0. { -lines } else { lines }
}

#[cfg(test)]
mod tests {

    /// What the label ladder asks a pane: are you showing a name of your own,
    /// or still standing under the app's? (#740)
    #[test]
    fn a_pane_states_a_title_whenever_it_is_not_the_placeholder() {
        use super::stated_title;

        // Nothing has spoken — this is the pane a directory stands in for.
        assert_eq!(stated_title("ntty7"), None);
        assert_eq!(stated_title("  ntty7  "), None);
        assert_eq!(stated_title("   "), None);

        // A title from the program running in it.
        assert_eq!(stated_title("vim — main.rs"), Some("vim — main.rs"));
        assert_eq!(stated_title(" user@host:~/repo "), Some("user@host:~/repo"));
        // A default tty7 chose for the pane itself is a name, not the absence
        // of one: an SSH pane answers to its host (#438) and a workspace pane
        // to its workspace, and neither gives way to a directory.
        assert_eq!(stated_title("prod-web"), Some("prod-web"));
        // So does the state a finished pane is left showing.
        assert_eq!(
            stated_title("ntty7 — process exited"),
            Some("ntty7 — process exited")
        );
    }

    #[test]
    fn a_busy_command_name_undoes_the_shell_escaping() {
        use super::unescape_mark_text;
        // The integration escapes % and the four bytes that would break OSC
        // framing; everything else is the line as typed.
        assert_eq!(unescape_mark_text("printf '100%25'"), "printf '100%'");
        assert_eq!(unescape_mark_text("a%1Bb%07c%0Dd"), "abcd");
        assert_eq!(unescape_mark_text("one%0Atwo"), "one two");
        // A bare % the shell somehow left alone must survive rather than eat
        // the next two characters.
        assert_eq!(unescape_mark_text("50% off"), "50% off");
        assert_eq!(unescape_mark_text("cargo build"), "cargo build");
    }

    #[test]
    fn a_busy_command_name_stays_short_enough_to_read() {
        use super::{BUSY_COMMAND_MAX, clamp_command};
        assert_eq!(clamp_command("  sleep 300  "), "sleep 300");
        let long = "cargo ".repeat(40);
        let out = clamp_command(&long);
        assert!(out.ends_with('…'), "{out:?}");
        assert!(out.chars().count() <= BUSY_COMMAND_MAX + 1, "{out:?}");
        // No dangling space before the ellipsis.
        assert!(!out.contains(" …"), "{out:?}");
    }
    use super::{SCROLL_ANIM_FRAME, scroll_anim_step};
    use super::{
        SelectEndCopy, WheelRoute, clipboard_paths, compose_notification_title, cwd_is_on_host,
        link_path_style,
    };
    use super::{
        TitleSettle, files_cwd, settle_title, stages_clipboard_image, wsl_share_distro,
        wsl_share_path,
    };
    use super::{
        drag_scroll_step, encode_mouse, expand_file_command_template, fallback_chain,
        focus_report_bytes, paste_bytes, select_end_copy, should_show_context_menu,
        smooth_scroll_step, trim_trailing_spaces, wheel_route,
    };
    use alacritty_terminal::term::TermMode;
    use gpui::{ClipboardEntry, ClipboardItem, ExternalPaths, Modifiers};
    use std::path::{Path, PathBuf};

    use crate::daemon::protocol::RemoteKind;

    #[test]
    fn a_title_the_tab_already_shows_drops_the_one_that_was_waiting() {
        // The revert is what cancels a flash: the pane is told the title it is
        // already displaying, and the queued command name goes away with it.
        assert_eq!(
            settle_title("~/dev", true, "~/dev"),
            TitleSettle::Revert,
            "a revert cancels the pending title rather than queueing behind it"
        );
        assert_eq!(
            settle_title("~/dev", false, "~/dev"),
            TitleSettle::Revert,
            "and asks for no wait when there was nothing pending either"
        );
        // A second title while one is already waiting rides the wait in
        // flight; restarting it is what would let a title that keeps changing
        // put the tab's next update off forever.
        assert_eq!(settle_title("~/dev", true, "wget 40%"), TitleSettle::Queue);
        assert_eq!(
            settle_title("~/dev", false, "wget 1%"),
            TitleSettle::QueueAndWait
        );
    }

    #[test]
    fn a_spinner_frame_that_only_changes_the_status_mark_asks_for_no_repaint() {
        // An agent rewrites its title every few hundred milliseconds while it
        // works, and the tab never draws the mark, so none of these may wait
        // and repaint the whole window for the same label.
        assert_eq!(
            settle_title(
                "\u{25D0} fixing the switcher",
                false,
                "\u{25D1} fixing the switcher"
            ),
            TitleSettle::Revert
        );
        assert_eq!(
            settle_title(
                "\u{25D0} fixing the switcher",
                true,
                "\u{2733} fixing the switcher"
            ),
            TitleSettle::Revert,
            "and a frame that lands while another title waits drops it, as a revert does"
        );
        assert_eq!(
            settle_title(
                "\u{25D0} fixing the switcher",
                false,
                "\u{25D1} fixing the parser"
            ),
            TitleSettle::QueueAndWait,
            "a different title still lands"
        );
    }

    #[test]
    fn a_notification_title_keeps_at_most_two_segments() {
        let ws = || Some("tty7".to_string());
        assert_eq!(
            compose_notification_title(None, Some("build-box".into()), ws()),
            "build-box · tty7"
        );
        // An agent takes the machine's place rather than adding a third part.
        assert_eq!(
            compose_notification_title(Some("Claude".into()), Some("build-box".into()), ws()),
            "Claude · tty7"
        );
        // A local pane has no machine label; a nameless workspace has no name.
        assert_eq!(compose_notification_title(None, None, ws()), "tty7");
        assert_eq!(
            compose_notification_title(Some("Claude".into()), None, None),
            "Claude"
        );
        assert_eq!(compose_notification_title(None, None, None), "ntty7");
    }

    fn wsl_context(distro: &str) -> crate::daemon::protocol::RemoteContext {
        crate::daemon::protocol::RemoteContext {
            kind: RemoteKind::Wsl,
            argv: Vec::new(),
            target: distro.to_string(),
        }
    }

    #[test]
    fn a_wsl_exe_pane_on_this_machine_completes_over_its_own_share() {
        assert_eq!(
            wsl_share_distro(Some(&wsl_context("Ubuntu-24.04")), true),
            Some("Ubuntu-24.04".to_string())
        );
    }

    /// The same pane on a remote machine reaches that machine's distro, which
    /// no share here can list.
    #[test]
    fn a_remote_hosts_wsl_exe_pane_leaves_tab_to_the_shell() {
        assert_eq!(
            wsl_share_distro(Some(&wsl_context("Ubuntu-24.04")), false),
            None
        );
    }

    #[test]
    fn panes_with_no_distro_of_their_own_have_no_share() {
        assert_eq!(wsl_share_distro(None, true), None);
    }

    #[test]
    fn a_remote_pane_stages_its_clipboard_image_on_every_platform() {
        // The SYN path leans on the agent sharing a clipboard with the pane,
        // which a remote agent never does — it reads the clipboard of the host
        // it runs on, and this machine's screenshot is not in it.
        assert!(
            stages_clipboard_image(true),
            "a remote agent cannot see this machine's clipboard"
        );
        // Locally the platform decides: macOS hands the agent the clipboard
        // itself, which is higher fidelity than a staged file.
        assert_eq!(
            stages_clipboard_image(false),
            cfg!(not(target_os = "macos"))
        );
    }

    #[test]
    fn a_wsl_cwd_gets_a_windows_spelling_the_completion_engine_can_list() {
        let share = |posix: &str| wsl_share_path("Ubuntu-24.04", posix);

        // A distro-native path goes through the share.
        assert_eq!(
            share("/home/me/repo"),
            Some(PathBuf::from(r"\\wsl$\Ubuntu-24.04\home\me\repo"))
        );
        assert_eq!(share("/"), Some(PathBuf::from(r"\\wsl$\Ubuntu-24.04\")));

        // The automount stays on the share too: a drive-spelled cwd would
        // send an absolute word (`ls /etc<Tab>`) to `C:\etc` instead of the
        // distro's /etc, because a rooted word completes against its cwd's
        // path prefix.
        assert_eq!(
            share("/mnt/c/Users/me"),
            Some(PathBuf::from(r"\\wsl$\Ubuntu-24.04\mnt\c\Users\me"))
        );

        // No absolute POSIX path, no translation — and a distro name that
        // could break out of the share is refused outright.
        assert_eq!(share("relative/path"), None);
        assert_eq!(wsl_share_path("", "/home/me"), None);
        assert_eq!(wsl_share_path(r"evil\distro", "/home/me"), None);
        assert_eq!(wsl_share_path("evil/distro", "/home/me"), None);
    }

    /// #896: the Files panel handed a WSL pane's `/home/me` to the local
    /// `read_dir`, which on Windows is `C:\home\me`. The share is the only
    /// spelling this machine can list — and drops into the tree land there too.
    #[test]
    fn a_wsl_panes_files_root_goes_through_its_distros_share() {
        assert_eq!(
            files_cwd(None, Some("Ubuntu"), Some(PathBuf::from("/home/me/repo"))),
            Some(PathBuf::from(r"\\wsl$\Ubuntu\home\me\repo"))
        );
        assert_eq!(
            files_cwd(None, Some("Ubuntu"), Some(PathBuf::from("/mnt/c/Users/me"))),
            Some(PathBuf::from(r"\\wsl$\Ubuntu\mnt\c\Users\me"))
        );
    }

    /// A cwd the host resolves itself — a local shell, a workspace pane on its
    /// own server, a WSL workspace's distro daemon — is never rewritten.
    #[test]
    fn a_cwd_on_the_panes_own_host_roots_the_tree_as_is() {
        let native = Some(PathBuf::from("/home/me/repo"));
        assert_eq!(files_cwd(native.clone(), None, None), native);
        assert_eq!(
            files_cwd(
                native.clone(),
                Some("Ubuntu"),
                Some(PathBuf::from("/elsewhere"))
            ),
            native
        );
    }

    /// wsl.exe's own cwd on the Windows side is already a local path.
    #[test]
    fn a_wsl_pane_with_a_windows_cwd_keeps_it() {
        assert_eq!(
            files_cwd(None, Some("Ubuntu"), Some(PathBuf::from(r"C:\Users\me"))),
            Some(PathBuf::from(r"C:\Users\me"))
        );
    }

    /// No host spelling and no local distro — a shell ssh'd onward, or a WSL
    /// pane whose distro could not be named — roots nothing rather than a
    /// path the tree could only fail to read.
    #[test]
    fn a_cwd_no_host_can_read_roots_nothing() {
        assert_eq!(files_cwd(None, None, Some(PathBuf::from("/home/me"))), None);
        assert_eq!(
            files_cwd(None, Some(""), Some(PathBuf::from("/home/me"))),
            None
        );
        assert_eq!(files_cwd(None, Some("Ubuntu"), None), None);
    }

    #[test]
    fn the_pasted_image_name_stands_alone_as_a_remote_path_component() {
        use gpui::{Image, ImageFormat};
        let safe_local_name = |name: &str| {
            !name.is_empty()
                && name != "."
                && name != ".."
                && !name.contains('/')
                && !name.contains('\\')
        };

        let pixel = image::RgbaImage::from_pixel(1, 1, image::Rgba([4, 5, 6, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(pixel)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let path = super::write_clipboard_image(&Image::from_bytes(ImageFormat::Png, png)).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(safe_local_name(&name), "{name} must not traverse or nest");
    }

    #[test]
    fn file_command_template_substitutes_path_line_and_column() {
        let argv = expand_file_command_template(
            "herdr edit {path} --line={line} --column={column}",
            Path::new("/tmp/foo.rs"),
            Some(42),
            Some(7),
        );
        assert_eq!(
            argv,
            vec!["herdr", "edit", "/tmp/foo.rs", "--line=42", "--column=7",]
        );
    }

    #[test]
    fn file_command_template_drops_tokens_for_absent_values() {
        let argv = expand_file_command_template(
            "herdr edit {path} --line={line} --column={column}",
            Path::new("/tmp/foo.rs"),
            None,
            None,
        );
        assert_eq!(argv, vec!["herdr", "edit", "/tmp/foo.rs"]);

        let argv = expand_file_command_template(
            "herdr edit {path} --line={line} --column={column}",
            Path::new("/tmp/foo.rs"),
            Some(42),
            None,
        );
        assert_eq!(argv, vec!["herdr", "edit", "/tmp/foo.rs", "--line=42"]);
    }

    #[test]
    fn file_command_template_keeps_path_only_token_and_unknown_placeholder() {
        let argv = expand_file_command_template(
            "code --goto {path}:{line} {other}",
            Path::new("/tmp/foo.rs"),
            None,
            None,
        );
        assert_eq!(argv, vec!["code", "--goto", "{other}"]);
    }

    /// #542's contract: a failed open is an `Err` the click site can toast,
    /// not a line in a logfile nobody is watching.
    #[test]
    fn a_file_command_that_cannot_spawn_comes_back_as_an_error() {
        let path = Path::new("/tmp/wherever.rs");
        // The binary does not exist, so the spawn itself fails.
        assert!(super::run_file_command("tty7-no-such-binary {path}", path, None, None).is_err());
        // A template whose tokens all expand to nothing is a config error,
        // not a silent no-op.
        assert!(super::run_file_command("{line}", path, None, None).is_err());
    }

    #[test]
    fn clipboard_image_transcodes_bmp_to_png_and_passes_png_through() {
        use gpui::{Image, ImageFormat};

        let pixel = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        let mut bmp = Vec::new();
        image::DynamicImage::ImageRgba8(pixel)
            .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
            .unwrap();
        let path = super::write_clipboard_image(&Image::from_bytes(ImageFormat::Bmp, bmp)).unwrap();
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(&std::fs::read(&path).unwrap()[..8], b"\x89PNG\r\n\x1a\n");

        let png = std::fs::read(&path).unwrap();
        let out = super::write_clipboard_image(&Image::from_bytes(ImageFormat::Png, png.clone()))
            .unwrap();
        assert_eq!(out.extension().unwrap(), "png");
        assert_eq!(std::fs::read(&out).unwrap(), png);
    }

    /// A macOS screenshot reaches the pasteboard as TIFF, which agent vision
    /// rejects the same way it rejects a Windows BMP. The transcode depends on
    /// `image`'s TIFF decoder being compiled in — without it this fails at
    /// runtime by quietly staging nothing, which reads as "paste did nothing".
    #[test]
    fn a_macos_tiff_screenshot_is_staged_as_png() {
        use gpui::{Image, ImageFormat};

        let pixel = image::RgbaImage::from_pixel(1, 1, image::Rgba([4, 5, 6, 255]));
        let mut tiff = Vec::new();
        image::DynamicImage::ImageRgba8(pixel)
            .write_to(
                &mut std::io::Cursor::new(&mut tiff),
                image::ImageFormat::Tiff,
            )
            .unwrap();
        let path =
            super::write_clipboard_image(&Image::from_bytes(ImageFormat::Tiff, tiff)).unwrap();
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(&std::fs::read(&path).unwrap()[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn fallback_chain_pins_bundled_hack_last() {
        let configured = vec!["Menlo".to_string(), "Apple Color Emoji".to_string()];

        let chain = fallback_chain("JetBrains Mono", &configured);
        assert_eq!(chain[..2], ["Menlo", "Apple Color Emoji"]);
        assert_eq!(chain.last().unwrap(), "Hack");

        let chain = fallback_chain("Hack", &configured);
        assert_eq!(chain[..2], ["Menlo", "Apple Color Emoji"]);
        assert!(!chain.iter().any(|f| f == "Hack"));

        let with_hack = vec!["Hack".to_string(), "Menlo".to_string()];
        let chain = fallback_chain("SF Mono", &with_hack);
        assert_eq!(chain[..2], ["Hack", "Menlo"]);

        assert_eq!(
            fallback_chain("Hack Nerd Font", &[]).last().unwrap(),
            "Hack",
            "a Hack-prefixed family name must not suppress the bundled anchor"
        );
    }

    #[test]
    fn apply_fallback_chain_reaches_every_face_a_view_has() {
        // Bold and italic are the ones at risk: they hold a copy taken off the
        // regular face when `alt_font` built them, so a rebuild that wrote only
        // the regular face would strand them on the chain it replaced.
        let mut font = gpui::font("Hack");
        let mut bold = Some(gpui::font("Hack Bold"));
        let mut italic = Some(gpui::font("Hack Italic"));

        super::apply_fallback_chain(vec!["Menlo".to_string()], &mut font, &mut bold, &mut italic);

        for face in [&font, bold.as_ref().unwrap(), italic.as_ref().unwrap()] {
            assert_eq!(face.fallbacks.as_ref().unwrap().fallback_list(), ["Menlo"]);
        }

        // Neither is configured by default, and a view carries `None` for one
        // it was never given.
        let (mut none_bold, mut none_italic) = (None, None);
        super::apply_fallback_chain(
            vec!["Menlo".to_string()],
            &mut font,
            &mut none_bold,
            &mut none_italic,
        );
        assert_eq!(font.fallbacks.unwrap().fallback_list(), ["Menlo"]);
    }

    #[test]
    fn fallback_chain_appends_platform_stock_faces() {
        let stock = crate::core::config::platform_last_resort_fallbacks();
        assert!(!stock.is_empty(), "every platform needs a CJK last resort");

        let legacy = vec![
            "Menlo".to_string(),
            "Hasklug Nerd Font Mono".to_string(),
            "Maple Mono NF CN".to_string(),
            "Apple Color Emoji".to_string(),
        ];
        let chain = fallback_chain("Hack", &legacy);
        for name in stock {
            assert!(
                chain.iter().any(|f| f == name),
                "{name} missing from repaired chain {chain:?}"
            );
        }

        assert_eq!(chain[..legacy.len()], legacy[..]);

        let explicit = vec![stock[0].to_string()];
        let chain = fallback_chain("Hack", &explicit);
        assert_eq!(
            chain.iter().filter(|f| *f == stock[0]).count(),
            1,
            "stock face duplicated in {chain:?}"
        );

        assert!(!fallback_chain(stock[0], &[]).iter().any(|f| f == stock[0]));
    }

    #[test]
    fn wheel_routes_by_negotiated_mode_with_reporting_first() {
        let mouse = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            wheel_route(mouse, false, true),
            WheelRoute::Report { base: 64 }
        );
        assert_eq!(
            wheel_route(mouse, false, false),
            WheelRoute::Report { base: 65 }
        );

        let alt = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        assert_eq!(
            wheel_route(alt, false, true),
            WheelRoute::Arrows { seq: b"\x1b[A" }
        );
        assert_eq!(
            wheel_route(alt, false, false),
            WheelRoute::Arrows { seq: b"\x1b[B" }
        );
        assert_eq!(
            wheel_route(alt | TermMode::APP_CURSOR, false, true),
            WheelRoute::Arrows { seq: b"\x1bOA" }
        );
        assert_eq!(
            wheel_route(alt | TermMode::APP_CURSOR, false, false),
            WheelRoute::Arrows { seq: b"\x1bOB" }
        );

        assert_eq!(
            wheel_route(mouse | alt, false, true),
            WheelRoute::Report { base: 64 }
        );

        assert_eq!(
            wheel_route(TermMode::empty(), false, true),
            WheelRoute::Scrollback
        );
    }

    #[test]
    fn wheel_ignores_alternate_scroll_outside_the_alt_screen() {
        assert_eq!(
            wheel_route(TermMode::ALTERNATE_SCROLL, false, true),
            WheelRoute::Scrollback
        );
    }

    #[test]
    fn shift_wheel_always_scrolls_the_local_scrollback() {
        let everything = TermMode::MOUSE_MOTION
            | TermMode::ALT_SCREEN
            | TermMode::ALTERNATE_SCROLL
            | TermMode::APP_CURSOR;
        assert_eq!(wheel_route(everything, true, true), WheelRoute::Scrollback);
        assert_eq!(wheel_route(everything, true, false), WheelRoute::Scrollback);
    }

    #[test]
    fn right_click_opens_the_host_menu_when_the_app_is_not_reporting_mouse() {
        assert!(should_show_context_menu(false, false));
        assert!(should_show_context_menu(false, true));
    }

    #[test]
    fn right_click_belongs_to_the_app_while_mouse_reporting_is_active() {
        assert!(
            !should_show_context_menu(true, false),
            "an unmodified right-click in mouse mode is the application's event, \
             so tty7 must not also pop its own menu over it"
        );
    }

    #[test]
    fn shift_right_click_stays_the_escape_hatch_in_mouse_mode() {
        assert!(
            should_show_context_menu(true, true),
            "Shift is the documented way to reach tty7's own menu inside a \
             mouse-reporting TUI"
        );
    }

    #[test]
    fn context_menu_and_mouse_report_never_fire_for_the_same_click() {
        // The render-time menu gate and the mouse-down forwarder in
        // `TerminalElement` read the same predicate from opposite sides, so
        // every (mouse_mode, shift) pair must land in exactly one of them.
        for mouse_mode in [false, true] {
            for shift in [false, true] {
                let menu = should_show_context_menu(mouse_mode, shift);
                let reported = mouse_mode && !shift;
                assert_ne!(
                    menu, reported,
                    "mouse_mode={mouse_mode} shift={shift} must route to exactly one consumer"
                );
            }
        }
    }

    #[test]
    fn copy_on_select_copies_the_buffer_the_gesture_touched() {
        assert_eq!(select_end_copy(false, true), SelectEndCopy::None);
        assert_eq!(select_end_copy(true, true), SelectEndCopy::Grid);
        assert_eq!(select_end_copy(true, false), SelectEndCopy::None);
    }

    #[test]
    fn sgr_mouse_reports_one_based_decimal_with_modifier_bits() {
        let plain = Modifiers::default();
        assert_eq!(
            encode_mouse(true, 0, &plain, 4, 8, true).unwrap(),
            b"\x1b[<0;5;9M".to_vec()
        );
        assert_eq!(
            encode_mouse(true, 2, &plain, 4, 8, false).unwrap(),
            b"\x1b[<2;5;9m".to_vec()
        );
        let all = Modifiers {
            shift: true,
            alt: true,
            control: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_mouse(true, 0, &all, 0, 0, true).unwrap(),
            b"\x1b[<28;1;1M".to_vec()
        );
        assert_eq!(
            encode_mouse(true, 64, &plain, 10, 3, true).unwrap(),
            b"\x1b[<64;11;4M".to_vec()
        );
        assert_eq!(
            encode_mouse(true, 35, &plain, 1, 1, true).unwrap(),
            b"\x1b[<35;2;2M".to_vec()
        );
    }

    #[test]
    fn sgr_mouse_has_no_coordinate_cap() {
        let plain = Modifiers::default();
        assert_eq!(
            encode_mouse(true, 0, &plain, 500, 300, true).unwrap(),
            b"\x1b[<0;501;301M".to_vec()
        );
    }

    #[test]
    fn x10_mouse_packs_bytes_and_drops_button_on_release() {
        let plain = Modifiers::default();
        assert_eq!(
            encode_mouse(false, 0, &plain, 4, 8, true).unwrap(),
            vec![0x1b, b'[', b'M', 32, 32 + 1 + 4, 32 + 1 + 8]
        );
        assert_eq!(
            encode_mouse(false, 2, &plain, 4, 8, false).unwrap(),
            vec![0x1b, b'[', b'M', 32 + 3, 32 + 1 + 4, 32 + 1 + 8]
        );
        let ctrl = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_mouse(false, 1, &ctrl, 0, 0, true).unwrap(),
            vec![0x1b, b'[', b'M', 32 + 1 + 16, 33, 33]
        );
    }

    #[test]
    fn x10_mouse_drops_out_of_range_coordinates_whole() {
        let plain = Modifiers::default();
        assert!(encode_mouse(false, 0, &plain, 223, 0, true).is_none());
        assert!(encode_mouse(false, 0, &plain, 0, 223, true).is_none());
        let last = encode_mouse(false, 0, &plain, 222, 222, true).unwrap();
        assert_eq!(&last[4..], &[255, 255]);
    }

    #[test]
    fn focus_reports_only_when_the_app_opted_in() {
        assert_eq!(focus_report_bytes(TermMode::empty(), true), None);
        assert_eq!(focus_report_bytes(TermMode::empty(), false), None);
        let mode = TermMode::FOCUS_IN_OUT;
        assert_eq!(focus_report_bytes(mode, true), Some(b"\x1b[I".as_slice()));
        assert_eq!(focus_report_bytes(mode, false), Some(b"\x1b[O".as_slice()));
        assert_eq!(focus_report_bytes(TermMode::MOUSE_MOTION, true), None);
    }

    #[test]
    fn smooth_scroll_step_accumulates_and_clamps() {
        assert_eq!(smooth_scroll_step(0, 0.0, 0.4, 100), (0, 0.4));
        let (jump, frac) = smooth_scroll_step(0, 0.4, 0.8, 100);
        assert_eq!(jump, 1);
        assert!((frac - 0.2).abs() < 1e-4);
        let (jump, frac) = smooth_scroll_step(5, 0.2, -0.5, 100);
        assert_eq!(jump, -1);
        assert!((frac - 0.7).abs() < 1e-4);
        assert_eq!(smooth_scroll_step(3, 0.5, -10.0, 100), (-3, 0.0));
        assert_eq!(smooth_scroll_step(98, 0.0, 7.3, 100), (2, 0.0));
        assert_eq!(smooth_scroll_step(0, 0.0, 2.5, 0), (0, 0.0));
    }

    #[test]
    fn scroll_anim_step_converges_and_lands() {
        let frame = SCROLL_ANIM_FRAME;
        // A notch is spread over frames instead of being applied whole.
        let (step, last) = scroll_anim_step(3.0, frame);
        assert!(!last);
        assert!(
            step > 0. && step < 3.0,
            "took {step} of 3 lines in one frame"
        );

        // And it converges: no notch is left hanging.
        let mut remaining = 3.0_f32;
        let mut frames = 0u32;
        loop {
            let (step, last) = scroll_anim_step(remaining, frame);
            remaining -= step;
            frames += 1;
            if last {
                break;
            }
            assert!(frames < 200, "still {remaining} lines short after {frames}");
        }
        assert!(remaining.abs() < 1e-4, "landed {remaining} lines off");
        // Slow enough to read as motion, fast enough not to feel like lag.
        let ms = frames * SCROLL_ANIM_FRAME.as_millis() as u32;
        assert!((60..=200).contains(&ms), "a 3-line notch took {ms}ms");
    }

    #[test]
    fn scroll_anim_step_covers_dropped_frames() {
        // A late tick has to make up the ground it missed, or a busy pane would
        // scroll slower than an idle one.
        let (one, _) = scroll_anim_step(10.0, SCROLL_ANIM_FRAME);
        let (four, _) = scroll_anim_step(10.0, SCROLL_ANIM_FRAME * 4);
        assert!(four > one * 2., "{four} should far outpace {one}");
        // But the catch-up is capped, so a tick after a long stall never
        // overshoots what was actually asked for.
        let (stalled, _) = scroll_anim_step(10.0, std::time::Duration::from_secs(5));
        assert!(
            stalled > four && stalled <= 10.0,
            "stalled tick took {stalled}"
        );
    }

    #[test]
    fn scroll_anim_step_lands_on_a_negligible_remainder() {
        let (step, last) = scroll_anim_step(-0.005, SCROLL_ANIM_FRAME);
        assert!(last);
        assert_eq!(step, -0.005);
    }

    #[test]
    fn drag_scroll_step_scales_with_overshoot_and_caps() {
        assert_eq!(drag_scroll_step(0.2), 1);
        assert_eq!(drag_scroll_step(-0.2), -1);
        assert_eq!(drag_scroll_step(3.5), 4);
        assert_eq!(drag_scroll_step(-3.5), -4);
        assert_eq!(drag_scroll_step(50.0), 8);
        assert_eq!(drag_scroll_step(-50.0), -8);
    }

    #[test]
    fn trim_trailing_spaces_strips_per_line_and_preserves_structure() {
        assert_eq!(trim_trailing_spaces("a  \nb\t\nc"), "a\nb\nc");
        assert_eq!(trim_trailing_spaces("a  \n"), "a\n");
        assert_eq!(trim_trailing_spaces("a  "), "a");
        assert_eq!(trim_trailing_spaces("  a  "), "  a");
    }

    #[test]
    fn paste_bytes_strips_esc_to_prevent_bracketed_paste_escape() {
        assert_eq!(
            paste_bytes("ls -la", true),
            b"\x1b[200~ls -la\x1b[201~".to_vec()
        );

        let evil = "foo\x1b[201~\nrm -rf ~\n";
        let out = paste_bytes(evil, true);
        let end = b"\x1b[201~";
        let markers = out.windows(end.len()).filter(|w| *w == end).count();
        assert_eq!(markers, 1);
        let inner = &out[b"\x1b[200~".len()..out.len() - end.len()];
        assert!(!inner.contains(&0x1b));
        assert_eq!(inner, b"foo[201~\nrm -rf ~\n");

        assert_eq!(paste_bytes("a\x1b[201~b", false), b"a\x1b[201~b".to_vec());
    }

    #[test]
    fn paste_bytes_normalizes_newlines_to_cr_without_bracketed_paste() {
        assert_eq!(paste_bytes("a\nb\r\nc\n", false), b"a\rb\rc\r".to_vec());
        assert_eq!(
            paste_bytes("a\nb", true),
            b"\x1b[200~a\nb\x1b[201~".to_vec()
        );
    }

    #[test]
    fn paste_bytes_folds_crlf_so_a_windows_clipboard_pastes_like_any_other() {
        assert_eq!(
            paste_bytes("a\r\nb\r\n", true),
            b"\x1b[200~a\nb\n\x1b[201~".to_vec(),
            "CRLF must reach the app as one line break, not two"
        );
        assert_eq!(
            paste_bytes("a\r\nb", true),
            paste_bytes("a\nb", true),
            "a Windows clipboard must paste exactly like a Unix one"
        );
    }

    #[test]
    fn a_copied_file_is_pasted_by_path_quoted_and_space_joined() {
        let item = ClipboardItem {
            entries: vec![ClipboardEntry::ExternalPaths(ExternalPaths(
                vec![
                    PathBuf::from("/Users/me/My File.txt"),
                    PathBuf::from("/tmp/b.log"),
                ]
                .into(),
            ))],
        };
        let paths = clipboard_paths(&item);
        assert_eq!(
            paths,
            [
                PathBuf::from("/Users/me/My File.txt"),
                PathBuf::from("/tmp/b.log")
            ]
        );
        let spelled: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            super::pasted_paths_text(&spelled, Some("zsh")),
            "'/Users/me/My File.txt' /tmp/b.log "
        );

        // Text is text: nothing on the clipboard names a file.
        let text = ClipboardItem::new_string("echo hi".to_string());
        assert!(clipboard_paths(&text).is_empty());
    }

    /// "Open with Default App" sits beside "Open" only where it adds
    /// something: on a local file, and when "Open" is not already the OS
    /// association.
    #[test]
    fn the_default_app_row_is_offered_only_where_it_differs_from_open() {
        use super::{LinkFileOpen, offers_default_app_open};
        assert!(offers_default_app_open(true, LinkFileOpen::Command));
        assert!(
            !offers_default_app_open(true, LinkFileOpen::System),
            "it would repeat Open"
        );
        for mode in [LinkFileOpen::System, LinkFileOpen::Command] {
            assert!(
                !offers_default_app_open(false, mode),
                "a remote file has nothing here for the OS to open ({mode:?})"
            );
        }
    }

    #[test]
    fn only_a_matching_host_may_answer_for_a_panes_paths() {
        assert!(cwd_is_on_host(false, true));
        assert!(cwd_is_on_host(true, false));

        assert!(!cwd_is_on_host(true, true));
        assert!(!cwd_is_on_host(false, false));
    }

    /// Which machine's spelling a pane's paths are read in. Ungated on
    /// purpose: the bug this settles was a Windows-only one that hid behind a
    /// `#[cfg(unix)]` on the test that covered it.
    #[test]
    fn a_panes_paths_are_read_in_its_own_hosts_spelling() {
        use super::super::search::PathStyle;
        use std::path::Path;

        assert_eq!(
            link_path_style(true, Some(Path::new("/home/u/proj"))),
            PathStyle::NATIVE,
            "a pane on this machine reads its own output this OS's way, \
             whatever its shell spells the cwd like"
        );
        assert_eq!(
            link_path_style(false, Some(Path::new("/home/u/proj"))),
            PathStyle::Posix,
            "an SSH host, a remote workspace or a WSL distro reporting a \
             /-rooted cwd is a POSIX one on every client"
        );
        assert_eq!(
            link_path_style(false, Some(Path::new(r"C:\Users\u\proj"))),
            PathStyle::Windows,
            "and a remote Windows host is not"
        );
        assert_eq!(
            link_path_style(false, None),
            PathStyle::Posix,
            "a remote pane that has not said where it is still has no local \
             drive its paths could hang off"
        );
    }
}

#[cfg(test)]
mod gpui_tests {
    use super::*;
    use crate::daemon::protocol::{ClientMsg, DaemonMsg, WinSize};
    use crate::terminal::remote::PtySource;
    use crate::terminal::remote::TestLink;
    use gpui::{Entity, TestAppContext, point};

    fn harness(cx: &mut TestAppContext) -> (gpui::WindowHandle<TerminalView>, TestLink) {
        let pty = if cfg!(windows) {
            PtySource::LocalConpty
        } else {
            PtySource::Raw
        };
        harness_on(cx, pty)
    }

    fn harness_on(
        cx: &mut TestAppContext,
        pty: PtySource,
    ) -> (gpui::WindowHandle<TerminalView>, TestLink) {
        // Building a view reads the config. Whether that hit the real user
        // directory used to come down to which test happened to pin the
        // scratch dir first.
        crate::core::config::pin_test_config_dir();
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
        });
        let (terminal, daemon_side) = RemoteTerminal::from_pipe(TermSize::new(80, 24), pty);
        let window =
            cx.add_window(move |window, cx| TerminalView::with_terminal(terminal, 1, window, cx));
        (window, daemon_side)
    }

    /// The same pane, but hung under a `gpui_component::Root` the way the real
    /// window hangs it.
    ///
    /// `harness` makes the view its own root, which is enough for anything
    /// that never paints — but gpui-component's text input reaches for `Root`
    /// while painting, so any test that lets a frame draw with the search bar
    /// (or any other input) on screen needs this one instead.
    fn rooted_harness(
        cx: &mut TestAppContext,
    ) -> (
        gpui::WindowHandle<gpui_component::Root>,
        Entity<TerminalView>,
        TestLink,
    ) {
        crate::core::config::pin_test_config_dir();
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
        });
        let (terminal, daemon_side) =
            RemoteTerminal::from_pipe(TermSize::new(80, 24), PtySource::local());
        let built: std::rc::Rc<std::cell::RefCell<Option<Entity<TerminalView>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let out = built.clone();
        let window = cx.add_window(move |window, cx| {
            let view = cx.new(|cx| TerminalView::with_terminal(terminal, 1, window, cx));
            *out.borrow_mut() = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.background_executor.run_until_parked();
        let view = built.borrow_mut().take().expect("the pane was built");
        (window, view, daemon_side)
    }

    fn prompt_ready(
        window: &gpui::WindowHandle<TerminalView>,
        cx: &mut TestAppContext,
        daemon: &TestLink,
    ) {
        daemon.send(DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        });
        for _ in 0..200 {
            if window
                .update(cx, |view, _, _| view.terminal.at_prompt())
                .unwrap()
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the prompt report never reached the view");
    }

    fn alt_screen_ready(
        window: &gpui::WindowHandle<TerminalView>,
        cx: &mut TestAppContext,
        daemon: &TestLink,
    ) {
        daemon.send(DaemonMsg::Output(b"\x1b[?1049h".to_vec()));
        for _ in 0..400 {
            cx.run_until_parked();
            if window
                .update(cx, |view, _, _| view.on_alt_screen())
                .unwrap()
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the alternate-screen switch never reached the grid");
    }

    #[gpui::test]
    fn an_agent_that_has_finished_its_turn_is_not_busy(cx: &mut TestAppContext) {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};

        crate::core::config::pin_test_config_dir();
        let (window, daemon) = harness(cx);
        daemon.send(DaemonMsg::Agent(Some(CLIAgent::Claude)));

        let report = |status: AgentStatus, daemon: &TestLink| {
            daemon.send(DaemonMsg::AgentStatus(Some(AgentSessionState {
                status,
                message: None,
                session_id: Some("sid-abc".into()),
                launch_argv: Some(vec!["claude".into()]),
                rich: true,
                cwd: None,
                activity: 0,
                turns: 0,
                inferred: false,
                readout: Default::default(),
                since_ms: None,
            })));
        };
        let settled = |want: Option<PaneBusy>, cx: &mut TestAppContext| {
            for _ in 0..200 {
                if window.update(cx, |view, _, _| view.busy()).unwrap() == want {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        report(AgentStatus::Working, &daemon);
        assert!(
            settled(Some(PaneBusy::Agent("Claude Code")), cx),
            "a turn in flight is work closing would cut short"
        );

        // The green Done badge is what sends a reader to close the tab. Asking
        // "Claude Code is still working" there is both false and in the way.
        report(AgentStatus::Done, &daemon);
        assert!(
            settled(None, cx),
            "a finished turn must not hold the tab open"
        );
    }

    /// An agent that moves into a git worktree does not `chdir` — the process
    /// stays put and only its hook stream says where the work went. Every
    /// panel that answers "where am I?" reads `effective_cwd`, so this is the
    /// one place that has to prefer the agent's answer over the kernel's.
    #[gpui::test]
    fn a_pane_follows_its_agent_into_a_worktree(cx: &mut TestAppContext) {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus};
        use std::path::PathBuf;

        let launched_in = PathBuf::from("/repo");
        let working_in = PathBuf::from("/repo/.claude/worktrees/wt");

        let (window, daemon) = harness(cx);
        daemon.send(DaemonMsg::Cwd(launched_in.clone()));
        daemon.send(DaemonMsg::AgentStatus(Some(AgentSessionState {
            status: AgentStatus::Working,
            message: None,
            session_id: Some("sid-wt".into()),
            launch_argv: Some(vec!["claude".into()]),
            rich: true,
            cwd: Some(working_in.clone()),
            activity: 0,
            turns: 0,
            inferred: false,
            readout: Default::default(),
            since_ms: None,
        })));
        for _ in 0..200 {
            let seen = window
                .update(cx, |view, _, _| {
                    view.cwd().is_some() && view.agent_session().is_some()
                })
                .unwrap();
            if seen {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, window, cx| {
                assert_eq!(
                    view.cwd(),
                    Some(launched_in.clone()),
                    "the process really is still in the launch directory"
                );
                view.poll_foreground(window, cx);
                assert_eq!(
                    view.effective_cwd(),
                    Some(working_in.clone()),
                    "completion and link resolution root here"
                );
                assert_eq!(view.effective_host_cwd(), Some(working_in.clone()));
            })
            .unwrap();

        daemon.send(
            // Turn over: the agent is gone, and with it any claim about where the
            // work is. Falling back to a stale worktree would be worse than the
            // bug this fixes.
            DaemonMsg::AgentStatus(None),
        );
        for _ in 0..200 {
            let gone = window
                .update(cx, |view, _, _| view.agent_session().is_none())
                .unwrap();
            if gone {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        window
            .update(cx, |view, window, cx| {
                view.poll_foreground(window, cx);
                assert_eq!(view.effective_cwd(), Some(launched_in.clone()));
            })
            .unwrap();
    }

    /// A 1x1 red placement anchored at an absolute scrollback row, built the way
    /// the decode worker hands one to the store.
    fn placed_at(anchor_row: i64) -> crate::terminal::images::PlacedImage {
        use tty7_core::core::kitty_graphics::{Image, WireFormat};

        let mut img = Image {
            id: 1,
            number: 0,
            placement: 0,
            width: 1,
            height: 1,
            cols: 0,
            rows: 0,
            data: vec![0xff, 0x00, 0x00, 0xff],
            format: WireFormat::Rgba,
            compressed: false,
        };
        let (data, width_px, height_px) = crate::terminal::images::decode(&mut img).unwrap();
        crate::terminal::images::PlacedImage {
            data,
            anchor_row,
            anchor_col: 0,
            width_px,
            height_px,
            cols: 0,
            rows: 0,
            id: 1,
            placement: 0,
            painted: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[gpui::test]
    fn clearing_the_scrollback_drops_what_was_anchored_in_it(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);

        // Overflow the 24-row viewport so there is a scrollback to purge.
        let mut out = Vec::new();
        for i in 0..60 {
            out.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        daemon.send(DaemonMsg::Output(out));
        for _ in 0..200 {
            let filled = window
                .update(cx, |view, _, _| {
                    view.terminal.term.lock().grid().history_size() > 0
                })
                .unwrap();
            if filled {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, cx| {
                let history = view.terminal.term.lock().grid().history_size();
                assert!(history > 0, "the test needs a scrollback to clear");

                // Both of these address rows `clear_history` is about to drop:
                // an image anchored by absolute row, and a selection reaching up
                // into the history.
                view.terminal.images().place(placed_at(history as i64));
                view.terminal.term.lock().selection = Some(Selection::new(
                    SelectionType::Simple,
                    Point::new(Line(-1), Column(0)),
                    Side::Left,
                ));

                view.clear_scrollback(cx);

                assert!(
                    view.terminal.images().snapshot().is_empty(),
                    "a stale anchor blits the frame over live output, or off-screen \
                     entirely — the daemon never replays the frame to correct it"
                );
                assert!(
                    view.terminal.term.lock().selection.is_none(),
                    "a selection left pointing at purged rows clamps onto the \
                     viewport and copies whatever text moved into them"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn the_scrollbar_moves_the_viewport_and_follows_it_back(cx: &mut TestAppContext) {
        use gpui_component::scroll::ScrollbarHandle as _;

        let (window, daemon) = harness(cx);

        // Overflow the 24-row viewport so there is a scrollback to scroll.
        let mut out = Vec::new();
        for i in 0..60 {
            out.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        daemon.send(DaemonMsg::Output(out));
        // Wait for the reader to go quiet, not just to start: a scrollback
        // still filling underneath would move every row this test names.
        let mut settled = 0;
        for _ in 0..200 {
            let now = window
                .update(cx, |view, _, _| {
                    view.terminal.term.lock().grid().history_size()
                })
                .unwrap();
            if now > 0 && now == settled {
                break;
            }
            settled = now;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, _| {
                view.sync_scrollbar();
                let history = view.terminal.term.lock().grid().history_size();
                assert!(history > 0, "the test needs a scrollback to scroll");
                let row = view.line_height.as_f32();
                assert_eq!(
                    view.scroll_handle.offset().y,
                    px(-(history as f32) * row),
                    "at the live edge the whole scrollback sits above the viewport"
                );

                // Drag the thumb a third of the way up its track. The bar only
                // records the row; the pane applies it on its next render.
                view.scroll_frac = 0.5;
                view.scroll_handle
                    .set_offset(point(px(0.), px(-(history as f32) * row / 3.)));
                assert_eq!(
                    view.terminal.term.lock().grid().display_offset(),
                    0,
                    "the bar does not reach into the terminal itself"
                );

                view.sync_scrollbar();
                let offset = view.terminal.term.lock().grid().display_offset();
                assert_eq!(
                    offset,
                    history - (history as f32 / 3.).round() as usize,
                    "the viewport lands on the row the thumb was dropped on"
                );
                assert_eq!(
                    view.scroll_frac, 0.,
                    "a sub-line remainder left over from the wheel would paint \
                     the grid off the row the thumb picked"
                );
                assert_eq!(
                    view.scroll_handle.offset().y,
                    px(-((history - offset) as f32) * row),
                    "and the thumb reports the row the grid actually reached"
                );
            })
            .unwrap();
    }

    /// The case from the bug report: a relative path printed by a tool, sitting
    /// in a pane that reported its directory. It resolves; a name that is not
    /// there does not, and comes back as an unresolved *candidate* so the click
    /// can say why rather than doing nothing at all.
    #[gpui::test]
    fn a_relative_path_links_against_the_panes_own_directory(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("tty7-view-link-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("scratchpad")).expect("create scratchpad dir");
        let notes = dir.join("scratchpad/notes.md");
        std::fs::write(&notes, b"# notes").expect("create notes.md");

        let (window, daemon) = harness(cx);
        daemon.send(DaemonMsg::Cwd(dir.clone()));
        daemon.send(DaemonMsg::Output(
            b"ready (scratchpad/notes.md) and (scratchpad/gone.md)\r\n".to_vec(),
        ));
        for _ in 0..200 {
            let seen = window
                .update(cx, |view, _, _| {
                    view.cwd().is_some()
                        && view.terminal.term.lock().grid()[Line(0)][Column(7)].c == 's'
                })
                .unwrap();
            if seen {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, cx| {
                assert_eq!(view.cwd().as_deref(), Some(dir.as_path()));
                assert!(
                    view.hover_link_at(7, 0, true, cx),
                    "the file is right there under the pane's directory"
                );
                assert!(
                    view.hovered_link.as_ref().is_some_and(|link| link.armed),
                    "with the modifier down it is ready to be clicked"
                );
                assert!(
                    view.hover_link_at(7, 0, false, cx),
                    "a path is pointed out before the modifier is down, not after"
                );
                assert!(
                    view.hovered_link.as_ref().is_some_and(|link| !link.armed),
                    "but held back, because a plain click will not follow it"
                );

                let gone = "ready (scratchpad/notes.md) and (".len();
                assert!(
                    !view.hover_link_at(gone, 0, true, cx),
                    "nothing underlines for a name that is not on disk"
                );
                match view.resolve_link_at(gone, 0, true, cx) {
                    LinkAt::Unresolved { candidate } => {
                        assert_eq!(candidate.path, "scratchpad/gone.md");
                        assert!(
                            candidate.looks_like_a_path(view.link_path_style()),
                            "so the click reports it instead of staying silent"
                        );
                    }
                    _ => panic!("expected an unresolved path-shaped candidate"),
                }

                view.record_menu_link(7, 0, cx);
                assert!(
                    matches!(
                        view.menu_link_path(),
                        Some(path) if path.ends_with("scratchpad/notes.md")
                    ),
                    "a right click over a path opens a menu about that path"
                );
                view.record_menu_link(0, 0, cx);
                assert!(
                    view.menu_link_path().is_none(),
                    "and a right click over `ready` opens the ordinary one"
                );

                let mut off = cx.global::<Config>().clone();
                off.link_url = false;
                cx.set_global(off);
                view.record_menu_link(7, 0, cx);
                assert!(
                    view.menu_link_path().is_none(),
                    "and with link detection turned off the menu offers nothing \
                     the underline and the click both refuse"
                );
                cx.set_global(Config::default());

                // `ready (scratchpad...`: the blank between the two words.
                assert!(!view.hover_link_at(5, 0, false, cx));
                assert!(view.hovered_link.is_none(), "a blank holds no link");
                assert_eq!(
                    view.last_hover_cell,
                    Some((5, 0)),
                    "and the pointer is still remembered, so crossing a run \
                     of blanks costs one look each rather than one a frame"
                );
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Absolute paths in a pane that is `ssh`-ed somewhere used to be resolved
    /// against *this* machine's filesystem, so `/etc/hosts` on the far end
    /// opened the local copy without a word about it.
    #[gpui::test]
    fn a_pane_whose_paths_are_elsewhere_does_not_link_local_files(cx: &mut TestAppContext) {
        let file = std::env::temp_dir().join(format!("tty7-elsewhere-{}.txt", std::process::id()));
        std::fs::write(&file, b"local").expect("create local file");
        let line = format!("open {} now\r\n", file.display());

        let (window, daemon) = harness(cx);
        daemon.send(DaemonMsg::Output(line.clone().into_bytes()));
        daemon.send(DaemonMsg::RemoteContext(Some(
            crate::daemon::protocol::RemoteContext {
                kind: crate::daemon::protocol::RemoteKind::Ssh,
                argv: vec!["ssh".into(), "box".into()],
                target: "box".into(),
            },
        )));
        for _ in 0..200 {
            let seen = window
                .update(cx, |view, _, _| view.remote_context().is_some())
                .unwrap();
            if seen {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, cx| {
                assert!(
                    !view.cwd_is_on_host(),
                    "a local host cannot answer for the far side of an ssh session"
                );
                assert!(
                    !view.hover_link_at(6, 0, true, cx),
                    "the local copy of that path is not what the pane printed"
                );
            })
            .unwrap();
        let _ = std::fs::remove_file(&file);
    }

    #[gpui::test]
    fn a_stale_hover_row_does_not_index_the_shrunken_grid(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.hover_link_at(0, 23, true, cx);
                view.terminal.resize(TermSize::new(80, 8), 8, 17);
            })
            .unwrap();
        daemon.send(DaemonMsg::Size(WinSize {
            cols: 80,
            rows: 8,
            cell_w: 8,
            cell_h: 17,
        }));
        for _ in 0..200 {
            let ready = window
                .update(cx, |view, _, _| {
                    view.terminal.term.lock().screen_lines() == 8
                })
                .unwrap();
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            window
                .update(cx, |view, _, _| {
                    view.terminal.term.lock().screen_lines() == 8
                })
                .unwrap(),
            "the size echo never shrank the grid"
        );
        window
            .update(cx, |view, _, cx| {
                // The first hover cached (0, 23) while the grid was 24 rows.
                // Flip the modifier so refresh reads the cell again, now that
                // row 23 is outside the 8-row grid.
                view.link_modifier_down = false;
                view.last_hover_cell = Some((0, 23));
                assert!(
                    !view.refresh_link_hover(true, cx),
                    "a row outside the grid can't hold a link"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_resize_forgets_the_hovered_cell(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.set_grid_size(80, 24, px(8.), px(17.), 1., cx);
                view.hover_link_at(0, 23, true, cx);
                assert_eq!(view.last_hover_cell, Some((0, 23)));
                view.hovered_link = Some(HoveredLink {
                    runs: vec![(
                        Point::new(Line(23), Column(0)),
                        Point::new(Line(23), Column(3)),
                    )],
                    armed: true,
                });
                view.set_grid_size(80, 24, px(8.), px(17.), 1., cx);
                assert_eq!(view.last_hover_cell, Some((0, 23)));
                view.set_grid_size(80, 8, px(8.), px(17.), 1., cx);
                assert!(view.last_hover_cell.is_none(), "the cell is stale");
                assert!(view.hovered_link.is_none(), "so is the link it resolved");
            })
            .unwrap();
    }

    /// The seam is invisible to the user, so it has to be invisible to the
    /// hover too: pointing at either half underlines the whole path, and the
    /// span the element paints reaches across both rows.
    #[gpui::test]
    fn a_path_the_terminal_wrapped_is_hovered_as_one_link(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("tty7-view-wrap-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/bb/ccc/dddd")).expect("create dirs");
        std::fs::write(dir.join("a/bb/ccc/dddd/notes.md"), b"# notes").expect("create notes.md");

        let (window, daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.set_grid_size(20, 6, px(8.), px(17.), 1., cx);
            })
            .unwrap();
        // The grid keeps its old width until the daemon echoes `Size`. The
        // path has to be parsed at 20 columns or it never wraps.
        daemon.send(DaemonMsg::Size(WinSize {
            cols: 20,
            rows: 6,
            cell_w: 8,
            cell_h: 17,
        }));
        for _ in 0..200 {
            let ready = window
                .update(cx, |view, _, _| view.terminal.term.lock().columns() == 20)
                .unwrap();
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            window
                .update(cx, |view, _, _| view.terminal.term.lock().columns() == 20)
                .unwrap(),
            "the size echo never reflowed the grid"
        );
        daemon.send(DaemonMsg::Cwd(dir.clone()));
        daemon.send(DaemonMsg::Output(
            b"see a/bb/ccc/dddd/notes.md here\r\n".to_vec(),
        ));
        for _ in 0..200 {
            let seen = window
                .update(cx, |view, _, _| {
                    // 20 columns: row 0 is `see a/bb/ccc/dddd/no`, row 1 starts at `t`.
                    view.cwd().is_some()
                        && view.terminal.term.lock().grid()[Line(1)][Column(0)].c == 't'
                })
                .unwrap();
            if seen {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, cx| {
                // Row 0 holds `see a/bb/ccc/dddd/no`, row 1 the rest.
                for (col, row, where_) in [(6, 0, "before the seam"), (2, 1, "after it")] {
                    assert!(
                        view.hover_link_at(col, row, true, cx),
                        "the wrapped path is a link from {where_}"
                    );
                    let link = view.hovered_link.as_ref().expect("a span");
                    let (start, end) = (link.runs[0].0, link.runs[link.runs.len() - 1].1);
                    assert_eq!(
                        (start.line, end.line),
                        (Line(0), Line(1)),
                        "and the span the element paints covers both rows"
                    );
                    assert_eq!(start.column, Column(4), "starting at the path itself");
                }
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A wide character owns two columns, and the second one holds a space.
    /// The hover has to read that as part of the glyph, or the underline goes
    /// out on every other column of a path written in CJK.
    #[gpui::test]
    fn the_second_column_of_a_wide_character_still_hovers(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("tty7-view-wide-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("文档")).expect("create dirs");
        std::fs::write(dir.join("文档/笔记.md"), b"# notes").expect("create notes");

        let (window, daemon) = harness(cx);
        daemon.send(DaemonMsg::Cwd(dir.clone()));
        daemon.send(DaemonMsg::Output(
            "see 文档/笔记.md here\r\n".as_bytes().to_vec(),
        ));
        for _ in 0..200 {
            let seen = window
                .update(cx, |view, _, _| {
                    view.cwd().is_some()
                        && view.terminal.term.lock().grid()[Line(0)][Column(4)].c == '文'
                })
                .unwrap();
            if seen {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, cx| {
                // `see 文档/…`: column 4 carries 文, column 5 is its spacer.
                assert!(!view.cell_is_blank(5, 0), "the spacer belongs to the glyph");
                for col in [4, 5] {
                    assert!(
                        view.hover_link_at(col, 0, true, cx),
                        "column {col} of the same character is the same link"
                    );
                }
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A full-screen application drew the grid and is watching the mouse
    /// itself, so tty7 stays out of it until asked.
    #[gpui::test]
    fn a_path_under_a_full_screen_application_waits_for_the_modifier(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("tty7-view-alt-link-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("scratchpad")).expect("create scratchpad dir");
        std::fs::write(dir.join("scratchpad/notes.md"), b"# notes").expect("create notes.md");

        let (window, daemon) = harness(cx);
        daemon.send(DaemonMsg::Cwd(dir.clone()));
        daemon.send(DaemonMsg::Output(
            b"\x1b[?1049hready scratchpad/notes.md\r\n".to_vec(),
        ));
        for _ in 0..200 {
            let seen = window
                .update(cx, |view, _, _| {
                    view.cwd().is_some()
                        && view.on_alt_screen()
                        && view.terminal.term.lock().grid()[Line(0)][Column(6)].c == 's'
                })
                .unwrap();
            if seen {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, cx| {
                assert!(view.on_alt_screen(), "the application took the grid");
                assert!(
                    !view.hover_link_at(6, 0, false, cx),
                    "nothing is pointed out over somebody else's window"
                );
                assert!(
                    view.hover_link_at(6, 0, true, cx),
                    "asking for tty7's reading of the screen still gets it"
                );
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_menu_is_headed_by_the_file_it_is_about() {
        assert_eq!(
            link_menu_label(std::path::Path::new("/a/very/long/way/down/notes.md")),
            "notes.md",
            "the name, so the menu is not as wide as the path"
        );
        assert_eq!(link_menu_label(std::path::Path::new("/")), "/");
    }

    /// Runs out the wait a new title is held for.
    fn settle(cx: &mut TestAppContext) {
        cx.executor().advance_clock(super::TITLE_SETTLE * 2);
        cx.run_until_parked();
    }

    #[gpui::test]
    fn title_events_drive_the_tab_title(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                assert_eq!(view.title, "ntty7");
                view.handle_event(AlacEvent::Title("vim — main.rs".into()), cx);
            })
            .unwrap();
        settle(cx);
        window
            .update(cx, |view, _, cx| {
                assert_eq!(view.title, "vim — main.rs");
                view.handle_event(AlacEvent::ResetTitle, cx);
            })
            .unwrap();
        settle(cx);
        window
            .update(cx, |view, _, _| assert_eq!(view.title, "ntty7"))
            .unwrap();
    }

    /// The flash this wait exists to stop: a prompt framework names the command
    /// it is about to run and puts the directory back at the next prompt, and
    /// for anything that finishes in a blink both edges arrive within a few
    /// frames. The tab used to show the command and snap back, which reads as a
    /// glitch rather than as information.
    #[gpui::test]
    fn a_command_over_before_the_wait_never_reaches_the_tab(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.default_title = "~/dev".into();
                view.title = "~/dev".into();
                view.handle_event(AlacEvent::Title("ls".into()), cx);
                view.handle_event(AlacEvent::Title("~/dev".into()), cx);
                assert_eq!(view.title, "~/dev", "and never flashed on the way");
            })
            .unwrap();
        settle(cx);
        window
            .update(cx, |view, _, _| {
                assert_eq!(
                    view.title, "~/dev",
                    "the command was over before the tab could adopt its name"
                );
            })
            .unwrap();
    }

    /// A program that rewrites its own title faster than the wait — a download
    /// reporting progress — still updates the tab. The wait already running
    /// adopts the newest title rather than starting over, which is what would
    /// leave the tab frozen on the directory for as long as the download ran.
    #[gpui::test]
    fn a_title_rewritten_faster_than_the_wait_still_lands(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.handle_event(AlacEvent::Title("wget 1%".into()), cx);
                view.handle_event(AlacEvent::Title("wget 40%".into()), cx);
                view.handle_event(AlacEvent::Title("wget 99%".into()), cx);
            })
            .unwrap();
        settle(cx);
        window
            .update(cx, |view, _, _| assert_eq!(view.title, "wget 99%"))
            .unwrap();
    }

    /// An SSH pane answers to the host it dialled, and keeps answering to it
    /// after the remote program hands the title back (#438). Before this every
    /// SSH tab in the window read "tty7" until — and only if — the far shell
    /// had integration enough to title itself.
    #[gpui::test]
    fn an_ssh_pane_is_named_after_its_host(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.default_title = "prod-web".into();
                view.title = "prod-web".into();
                view.handle_event(AlacEvent::Title("vim — main.rs".into()), cx);
            })
            .unwrap();
        settle(cx);
        window
            .update(cx, |view, _, cx| {
                assert_eq!(view.title, "vim — main.rs");
                view.handle_event(AlacEvent::ResetTitle, cx);
            })
            .unwrap();
        settle(cx);
        window
            .update(cx, |view, _, _| {
                assert_eq!(
                    view.title, "prod-web",
                    "a reset goes back to the host, not to the app's own name"
                );
            })
            .unwrap();
    }

    fn next_input(daemon: &TestLink) -> Vec<u8> {
        loop {
            match daemon
                .recv_client(std::time::Duration::from_secs(5))
                .expect("client channel stays open")
            {
                ClientMsg::Input(bytes) => return bytes,
                _ => continue,
            }
        }
    }

    fn type_char(
        view: &mut TerminalView,
        ch: &str,
        window: &mut Window,
        cx: &mut Context<TerminalView>,
    ) {
        if cfg!(target_os = "macos") {
            let _ = window;
            view.commit_text(ch, cx);
        } else {
            let ev = KeyDownEvent {
                keystroke: gpui::Keystroke {
                    modifiers: gpui::Modifiers::default(),
                    key: ch.to_string(),
                    key_char: Some(ch.to_string()),
                },
                is_held: false,
                prefer_character_input: false,
            };
            view.on_key_down(&ev, window, cx);
        }
    }

    fn next_input_until_timeout(daemon: &TestLink) -> Option<Vec<u8>> {
        loop {
            match daemon.recv_client(std::time::Duration::from_millis(250)) {
                Ok(ClientMsg::Input(bytes)) => return Some(bytes),
                Ok(_) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("client channel closed before Input")
                }
            }
        }
    }

    /// `next_input_until_timeout` with the executor run between reads: a reply
    /// crosses the reader thread and the view before it is written.
    fn pumped_input(cx: &mut TestAppContext, daemon: &TestLink, tries: usize) -> Option<String> {
        (0..tries)
            .find_map(|_| {
                cx.run_until_parked();
                next_input_until_timeout(daemon)
            })
            .map(|bytes| String::from_utf8(bytes).unwrap())
    }

    #[gpui::test]
    fn a_theme_change_reaches_only_the_panes_that_asked(cx: &mut TestAppContext) {
        let set_background = |cx: &mut TestAppContext, bg: gpui::Hsla| {
            cx.update(|cx| gpui_component::Theme::global_mut(cx).background = bg)
        };
        let (_asked_window, asked) = harness(cx);
        let (_plain_window, plain) = harness(cx);
        set_background(cx, gpui::white());

        asked.send(DaemonMsg::Output(b"\x1b[?2031$p".to_vec()));
        assert_eq!(
            pumped_input(cx, &asked, 20).as_deref(),
            Some("\x1b[?2031;2$y"),
            "DECRQM reports 2031 as supported and reset before ?2031h"
        );

        asked.send(DaemonMsg::Output(
            b"\x1b[?2031h\x1b[?2031$p\x1b[?996n".to_vec(),
        ));
        assert_eq!(
            pumped_input(cx, &asked, 20).as_deref(),
            Some("\x1b[?2031;1$y"),
            "DECRQM reports 2031 as set after ?2031h, ahead of the scheme reply"
        );
        assert_eq!(
            pumped_input(cx, &asked, 20).as_deref(),
            Some("\x1b[?997;2n"),
            "996 answers the current scheme after the DECRQM"
        );

        set_background(cx, gpui::black());
        assert_eq!(
            pumped_input(cx, &asked, 20).as_deref(),
            Some("\x1b[?997;1n")
        );
        asked.send(DaemonMsg::Output(b"\x1b]11;?\x07".to_vec()));
        assert!(
            pumped_input(cx, &asked, 20)
                .is_some_and(|reply| reply.contains("11;rgb:0000/0000/0000")),
            "OSC 11 answers with the background the flip left"
        );

        set_background(cx, gpui::white());
        assert_eq!(
            pumped_input(cx, &asked, 20).as_deref(),
            Some("\x1b[?997;2n")
        );
        assert_eq!(
            pumped_input(cx, &plain, 2),
            None,
            "a pane that never set 2031 is not written to"
        );

        asked.send(DaemonMsg::Output(b"\x1b[?2031l".to_vec()));
        assert_eq!(pumped_input(cx, &asked, 1), None);
        set_background(cx, gpui::black());
        assert_eq!(pumped_input(cx, &asked, 2), None, "2031 switched off");
    }

    /// The reply names the mode at the query, not at the end of the batch, and
    /// a scheme report does not overtake a DECRQM that was asked first. Mixed
    /// with the emulator's own DA1 and DA2, the bytes leave in stream order,
    /// once each.
    #[gpui::test]
    fn decrqm_2031_answers_the_mode_at_the_query_in_stream_order(cx: &mut TestAppContext) {
        let set_background = |cx: &mut TestAppContext, bg: gpui::Hsla| {
            cx.update(|cx| gpui_component::Theme::global_mut(cx).background = bg)
        };

        let (_window, daemon) = harness(cx);
        set_background(cx, gpui::white());
        daemon.send(DaemonMsg::Output(b"\x1b[?2031$p\x1b[?2031h".to_vec()));
        assert_eq!(
            pumped_input(cx, &daemon, 20).as_deref(),
            Some("\x1b[?2031;2$y"),
            "the query landed before ?2031h, so the mode was reset"
        );
        assert_eq!(
            pumped_input(cx, &daemon, 2),
            None,
            "alacritty's ;0$y is not forwarded as a second reply"
        );
        set_background(cx, gpui::black());
        assert_eq!(
            pumped_input(cx, &daemon, 20).as_deref(),
            Some("\x1b[?997;1n"),
            "the batch still ends with the mode on"
        );

        let (_window, daemon) = harness(cx);
        set_background(cx, gpui::white());
        daemon.send(DaemonMsg::Output(
            b"\x1b[?2031h\x1b[?2031$p\x1b[?2031l".to_vec(),
        ));
        assert_eq!(
            pumped_input(cx, &daemon, 20).as_deref(),
            Some("\x1b[?2031;1$y"),
            "the query landed while the mode was on"
        );
        set_background(cx, gpui::black());
        assert_eq!(
            pumped_input(cx, &daemon, 2),
            None,
            "the batch ends with the mode off"
        );

        let (_window, daemon) = harness(cx);
        set_background(cx, gpui::white());
        daemon.send(DaemonMsg::Output(
            b"\x1b[c\x1b[?2031h\x1b[?2031$p\x1b[?996n\x1b[>c".to_vec(),
        ));
        assert_eq!(
            pumped_input(cx, &daemon, 20).as_deref(),
            Some("\x1b[?6c"),
            "DA1 is answered before the later queries"
        );
        assert_eq!(
            pumped_input(cx, &daemon, 20).as_deref(),
            Some("\x1b[?2031;1$y")
        );
        assert_eq!(
            pumped_input(cx, &daemon, 20).as_deref(),
            Some("\x1b[?997;2n"),
            "the scheme reply follows the DECRQM that preceded ?996n"
        );
        let da2 = pumped_input(cx, &daemon, 20).expect("DA2 reply");
        assert!(
            da2.starts_with("\x1b[>0;") && da2.ends_with('c'),
            "DA2 follows the scheme reply, got {da2}"
        );
        assert_eq!(
            pumped_input(cx, &daemon, 2),
            None,
            "each query is answered once"
        );
    }

    #[gpui::test]
    fn ctrl_v_on_the_alternate_screen_reaches_the_pty_as_syn(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("echo hi".into())));
        alt_screen_ready(&window, cx, &daemon);
        window
            .update(cx, |view, window, cx| {
                view.on_key_down(
                    &KeyDownEvent {
                        keystroke: key("ctrl-v"),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
            })
            .unwrap();

        assert_eq!(next_input_until_timeout(&daemon), Some(vec![0x16]));
        assert_eq!(
            next_input_until_timeout(&daemon),
            None,
            "the clipboard must stay where it is: vim's Ctrl+V is blockwise select, not paste"
        );
    }

    fn key(spec: &str) -> gpui::Keystroke {
        gpui::Keystroke::parse(spec).expect("valid keystroke spec")
    }

    #[gpui::test]
    fn shift_enter_reaches_a_foreground_tui_with_kitty_encoding(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, daemon) = harness_on(cx, PtySource::LocalConpty);
        cx.update(crate::ui::keymap::init);
        daemon.send(DaemonMsg::Output(b"\x1b[>1u".to_vec()));
        for _ in 0..200 {
            cx.run_until_parked();
            if window
                .update(cx, |view, _, _| view.key_flags().kitty_active())
                .unwrap()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        window
            .update(cx, |view, window, cx| {
                window.activate_window();
                view.focus_handle.focus(window, cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("shift-enter");

        assert_eq!(
            next_input_until_timeout(&daemon),
            Some(b"\x1b[13;2u".to_vec())
        );

        vcx.simulate_keystrokes("ctrl-j");
        assert_eq!(
            next_input_until_timeout(&daemon),
            Some(b"\x1b[106;5u".to_vec())
        );
    }

    #[gpui::test]
    fn shift_enter_reaches_a_foreground_tui_as_lf_without_kitty(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, daemon) = harness_on(cx, PtySource::Raw);
        cx.update(crate::ui::keymap::init);
        window
            .update(cx, |view, window, cx| {
                window.activate_window();
                view.focus_handle.focus(window, cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("shift-enter");

        assert_eq!(next_input_until_timeout(&daemon), Some(b"\n".to_vec()));

        vcx.simulate_keystrokes("ctrl-j");
        assert_eq!(next_input_until_timeout(&daemon), Some(b"\n".to_vec()));
    }

    #[gpui::test]
    fn newline_chords_reach_conpty_as_ctrl_j_without_kitty(cx: &mut TestAppContext) {
        let (window, daemon) = harness_on(cx, PtySource::LocalConpty);
        cx.update(crate::ui::keymap::init);
        window
            .update(cx, |view, window, cx| {
                window.activate_window();
                view.focus_handle.focus(window, cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        for chord in ["shift-enter", "ctrl-j"] {
            vcx.simulate_keystrokes(chord);
            assert_eq!(
                next_input_until_timeout(&daemon),
                Some(b"\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_".to_vec()),
                "{chord} must preserve Ctrl+J for native console readers"
            );
        }
    }

    #[gpui::test]
    fn alt_enter_keeps_its_legacy_encoding_in_a_foreground_tui(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, daemon) = harness(cx);
        cx.update(crate::ui::keymap::init);
        window
            .update(cx, |view, window, cx| {
                window.activate_window();
                view.focus_handle.focus(window, cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("alt-enter");

        assert_eq!(next_input_until_timeout(&daemon), Some(b"\x1b\r".to_vec()));
    }

    /// The whole chain for #834, through the real dispatch tree: F3 is bound
    /// to Find Next off macOS, and gpui matches bindings before the pane's key
    /// handler. With no find bar open the action gives the key back, the pane
    /// encodes it, and PSReadLine's CharacterSearch gets its `\EOR`.
    ///
    /// F7 has no binding at all and is the control: it takes the same route
    /// with nothing to fall through.
    #[cfg(not(target_os = "macos"))]
    #[gpui::test]
    fn an_unused_find_binding_gives_f3_back_to_the_shell(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        cx.update(|cx| crate::ui::keymap::init(cx));
        prompt_ready(&window, cx, &daemon);
        window
            .update(cx, |view, window, cx| {
                window.activate_window();
                view.focus_handle.focus(window, cx);
                assert!(view.search.is_none(), "no find bar is open");
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        for (chord, seq) in [("f3", b"\x1bOR".to_vec()), ("f7", b"\x1b[18~".to_vec())] {
            vcx.simulate_keystrokes(chord);
            window
                .update(cx, |view, _, _| {
                    assert!(view.search.is_none(), "{chord} did not open the find bar");
                })
                .unwrap();
            assert_eq!(
                next_input_until_timeout(&daemon),
                Some(seq),
                "{chord} reaches the PTY"
            );
        }
    }

    fn scroll_into_history(view: &TerminalView, offset: usize) {
        let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
        let mut term = view.terminal.term.lock();
        parser.advance(&mut *term, &b"line\r\n".repeat(60));
        term.scroll_display(Scroll::Delta(offset as i32));
        assert_eq!(
            term.grid().display_offset(),
            offset,
            "the viewport starts parked in the scrollback"
        );
    }

    fn display_offset(view: &TerminalView) -> usize {
        view.terminal.term.lock().grid().display_offset()
    }

    /// Shaped after what macOS actually delivers, measured on a wheel mouse and
    /// a trackpad: both arrive as pixels, and only the trackpad ever reports a
    /// phase. One wheel detent is ~103px, roughly five lines at a 21px line
    /// height; a trackpad event is a fraction of that but can reach ~3 lines
    /// when flicked, which is why phase and not size decides.
    fn wheel(view: &TerminalView, lines: f32, phase: gpui::TouchPhase) -> ScrollWheelEvent {
        ScrollWheelEvent {
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(lines * view.line_height.as_f32()))),
            touch_phase: phase,
            ..Default::default()
        }
    }

    fn notch(view: &TerminalView, lines: f32) -> ScrollWheelEvent {
        wheel(view, lines, gpui::TouchPhase::Moved)
    }

    /// The whole point of the animation: a detent must not land in one go.
    #[gpui::test]
    fn a_wheel_notch_is_spread_over_frames(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = notch(view, -4.9);
                view.on_scroll(&ev, w, cx);
                assert_eq!(
                    display_offset(view),
                    10,
                    "the detent was applied whole, before a single frame ran"
                );
                assert_eq!(view.scroll_frac, 0., "and not even a sliver of it");
                assert!(view.scroll_anim.is_some(), "nothing was left to animate");
            })
            .unwrap();
    }

    /// Zooming has to take the wheel away from the buffer entirely, or the
    /// grid would slide under the pointer while the font changed size.
    #[gpui::test]
    fn the_zoom_modifier_takes_the_wheel_off_the_scrollback(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        cx.update(|cx| cx.global_mut::<Config>().mouse_zoom_modifier = MouseZoomModifier::Platform);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let mut ev = notch(view, -4.9);
                ev.modifiers = Modifiers::secondary_key();
                view.on_scroll(&ev, w, cx);
                assert_eq!(display_offset(view), 10, "the wheel reached the grid");
                assert!(view.scroll_anim.is_none(), "and queued more of it");
            })
            .unwrap();
    }

    /// #668: the platform modifier is the wrong key to hardwire a zoom to —
    /// on a Mac it is ⌘, which people are already holding for something
    /// else — so which key zooms is a setting, all the way down to none.
    #[test]
    fn the_zoom_modifier_is_configurable_and_can_be_turned_off() {
        let secondary = Modifiers::secondary_key();
        let alt = Modifiers::alt();
        assert!(zoom_wheel(MouseZoomModifier::Platform, &secondary));
        assert!(
            !zoom_wheel(MouseZoomModifier::None, &secondary),
            "off is off"
        );
        assert!(!zoom_wheel(MouseZoomModifier::Alt, &secondary));
        assert!(zoom_wheel(MouseZoomModifier::Alt, &alt));
        assert!(!zoom_wheel(MouseZoomModifier::Platform, &alt));
        assert!(zoom_wheel(MouseZoomModifier::Ctrl, &Modifiers::control()));
        assert_eq!(
            zoom_wheel(MouseZoomModifier::Platform, &Modifiers::control()),
            !cfg!(target_os = "macos"),
            "off macOS the platform modifier is Ctrl itself"
        );
        // A second modifier is somebody else's gesture, whichever key is bound.
        let both = Modifiers { shift: true, ..alt };
        assert!(!zoom_wheel(MouseZoomModifier::Alt, &both));
        assert!(!zoom_wheel(MouseZoomModifier::Platform, &Modifiers::none()));
    }

    /// The point of turning it off: the modifier goes back to being an
    /// ordinary scroll, rather than eating the wheel.
    #[gpui::test]
    fn zooming_off_hands_the_wheel_back_to_the_scrollback(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        cx.update(|cx| {
            cx.global_mut::<Config>().mouse_zoom_modifier = MouseZoomModifier::None;
        });
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let mut ev = notch(view, -4.9);
                ev.modifiers = Modifiers::secondary_key();
                view.on_scroll(&ev, w, cx);
                assert!(
                    view.scroll_anim.is_some(),
                    "the wheel never reached the scrollback"
                );
            })
            .unwrap();
    }

    /// The bug this guards: a two-finger flick coasts long after the fingers
    /// are gone, and every coasting event carries whatever modifiers are down
    /// when it lands. Grabbing ⌘ for something else mid-coast used to read as
    /// a zoom and run the font down to its minimum in a blink (#912).
    #[gpui::test]
    fn a_modifier_pressed_mid_flick_does_not_hijack_it_into_a_zoom(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                view.on_scroll(&wheel(view, -0.5, gpui::TouchPhase::Started), w, cx);
                let before = display_offset(view);
                // The tail, now stamped with ⌘ the hand reached for.
                let mut tail = wheel(view, -3., gpui::TouchPhase::Moved);
                tail.modifiers = Modifiers::secondary_key();
                view.on_scroll(&tail, w, cx);
                assert!(
                    display_offset(view) != before || view.scroll_anim.is_some(),
                    "the tail stayed a scroll, the way the gesture started"
                );
                assert_eq!(view.zoom_debt, 0., "and never paid into the zoom");
            })
            .unwrap();
    }

    /// The same latch the other way round: a zoom gesture that outlives the
    /// modifier keeps zooming rather than dumping its tail into the buffer.
    #[gpui::test]
    fn a_zoom_gesture_keeps_zooming_after_the_modifier_is_released(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        cx.update(|cx| cx.global_mut::<Config>().mouse_zoom_modifier = MouseZoomModifier::Platform);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let mut start = wheel(view, -0.5, gpui::TouchPhase::Started);
                start.modifiers = Modifiers::secondary_key();
                view.on_scroll(&start, w, cx);
                view.on_scroll(&wheel(view, -0.5, gpui::TouchPhase::Moved), w, cx);
                assert_eq!(display_offset(view), 10, "the grid never moved");
                assert!(view.scroll_anim.is_none(), "and nothing was queued for it");
            })
            .unwrap();
    }

    /// The latch is per gesture, not per stream: fingers going down again
    /// start a fresh question. Carrying the last answer over would mean one
    /// ⌘-zoom left every later flick zooming with no modifier held at all.
    #[gpui::test]
    fn a_new_gesture_is_not_bound_by_what_the_last_one_answered(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        cx.update(|cx| cx.global_mut::<Config>().mouse_zoom_modifier = MouseZoomModifier::Platform);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let mut zoom = wheel(view, -0.5, gpui::TouchPhase::Started);
                zoom.modifiers = Modifiers::secondary_key();
                view.on_scroll(&zoom, w, cx);
                assert_eq!(display_offset(view), 10, "that one was a zoom");
                // Fingers down again, nothing held: a plain scroll.
                view.on_scroll(&wheel(view, -3., gpui::TouchPhase::Started), w, cx);
                assert_ne!(
                    display_offset(view),
                    10,
                    "the new gesture asked the question again"
                );
            })
            .unwrap();
    }

    /// A detent is one step however many lines the platform bills it as —
    /// macOS calls a single notch five.
    #[test]
    fn a_wheel_detent_zooms_by_exactly_one_step() {
        assert_eq!(zoom_scroll_steps(4.9, 0., false), (1, 0.));
        assert_eq!(zoom_scroll_steps(-4.9, 0., false), (-1, 0.));
        assert_eq!(zoom_scroll_steps(0., 0., false), (0, 0.));
    }

    /// A trackpad has no detents, so a flick arrives as a stream of slivers.
    /// Paying out a step per sliver would run the font from end to end.
    #[test]
    fn a_trackpad_flick_adds_up_to_whole_steps() {
        let (mut debt, mut steps) = (0., 0);
        for _ in 0..20 {
            let (s, d) = zoom_scroll_steps(1. / 3., debt, true);
            steps += s;
            debt = d;
        }
        assert_eq!(steps, 2, "twenty thirds of a line is two steps, not twenty");
        assert!(debt > 0., "and the remainder was dropped instead of kept");
    }

    /// The one thing typing at a prompt must never do is land where the
    /// person typing cannot see it.
    #[gpui::test]
    fn typing_brings_the_view_back_to_the_prompt(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        prompt_ready(&window, cx, &daemon);
        window
            .update(cx, |view, _w, cx| {
                scroll_into_history(view, 10);
                view.commit_text("l", cx);
                assert_eq!(
                    display_offset(view),
                    0,
                    "the character went in while the viewport stayed in the scrollback"
                );
            })
            .unwrap();
        assert_eq!(next_input_until_timeout(&daemon), Some(b"l".to_vec()));
    }

    /// A paste is a larger change than a keystroke to make out of sight.
    #[gpui::test]
    fn pasting_brings_the_view_back_to_the_prompt(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        prompt_ready(&window, cx, &daemon);
        window
            .update(cx, |view, _w, cx| {
                scroll_into_history(view, 10);
                view.paste("cargo test\n".to_string(), cx);
                assert_eq!(display_offset(view), 0, "the paste landed off screen");
            })
            .unwrap();
        let got = next_input_until_timeout(&daemon).expect("paste reaches the pty");
        assert!(
            got.windows(b"cargo test".len()).any(|w| w == b"cargo test"),
            "{got:?}"
        );
    }

    /// A trackpad is already a continuous stream — animating it would only put
    /// lag between the fingers and the grid. It is told apart by its phase, not
    /// by its delta type or size: a flick moves further in one event than a
    /// slowly inched wheel does.
    #[gpui::test]
    fn a_trackpad_gesture_scrolls_the_instant_it_is_touched(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = wheel(view, -3., gpui::TouchPhase::Started);
                view.on_scroll(&ev, w, cx);
                assert_eq!(display_offset(view), 7, "the gesture was held back");
                assert!(
                    view.scroll_anim.is_none(),
                    "a trackpad started an animation"
                );
            })
            .unwrap();
    }

    /// Lifting the fingers does not end the stream: macOS keeps sending Moved
    /// events for the momentum tail, and they are *larger* than the gesture
    /// that spawned them. Treating those as a wheel would smooth what the
    /// system is already smoothing.
    #[gpui::test]
    fn a_momentum_tail_is_not_mistaken_for_a_wheel(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                for (lines, phase) in [
                    (-0.5, gpui::TouchPhase::Started),
                    (-1.2, gpui::TouchPhase::Moved),
                    (0., gpui::TouchPhase::Ended),
                    (-2.9, gpui::TouchPhase::Moved),
                    (-2.4, gpui::TouchPhase::Moved),
                ] {
                    let ev = wheel(view, lines, phase);
                    view.on_scroll(&ev, w, cx);
                    assert!(
                        view.scroll_anim.is_none(),
                        "the momentum tail was animated at {lines} lines"
                    );
                }
                assert_eq!(display_offset(view), 3, "the tail did not all land");
            })
            .unwrap();
    }

    /// Inching the wheel one detent at a time reads as continuous already.
    #[gpui::test]
    fn a_scroll_too_small_to_see_jump_stays_direct(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = notch(view, -0.57);
                view.on_scroll(&ev, w, cx);
                assert!(view.scroll_anim.is_none(), "half a line was animated");
                assert!(view.scroll_frac > 0., "and it did not move either");
            })
            .unwrap();
    }

    #[gpui::test]
    fn turning_smooth_scrolling_off_restores_the_direct_path(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        cx.update(|cx| cx.global_mut::<Config>().smooth_scroll = false);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = notch(view, -3.);
                view.on_scroll(&ev, w, cx);
                assert_eq!(display_offset(view), 7);
                assert!(view.scroll_anim.is_none());
            })
            .unwrap();
    }

    /// An animation that kept walking after the view was moved out from under
    /// it would drag the user back off the prompt they just jumped to.
    #[gpui::test]
    fn moving_the_viewport_cancels_an_animation_in_flight(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = notch(view, -4.9);
                view.on_scroll(&ev, w, cx);
                assert!(view.scroll_anim.is_some());

                view.jump_to_prompt();
                assert!(
                    view.scroll_anim.is_none(),
                    "the animation outlived the jump"
                );
                assert_eq!(display_offset(view), 0);
            })
            .unwrap();
    }

    /// A frame callback that was already queued when the animation was
    /// cancelled must not walk the viewport on its stale epoch.
    #[gpui::test]
    fn a_stale_frame_after_cancellation_does_not_walk(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = notch(view, -4.9);
                view.on_scroll(&ev, w, cx);
                let epoch = view.scroll_anim_epoch;
                view.cancel_scroll_anim();

                assert!(
                    !view.scroll_anim_frame(epoch, cx),
                    "a stale frame callback kept the animation alive"
                );
                assert_eq!(display_offset(view), 10, "the stale frame moved");
            })
            .unwrap();
    }

    #[gpui::test]
    fn pty_write_events_reach_the_daemon_as_input(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.handle_event(AlacEvent::PtyWrite("ping".into()), cx);
            })
            .unwrap();
        assert_eq!(next_input(&daemon), b"ping".to_vec());
    }

    #[gpui::test]
    fn an_exited_local_pane_still_swallows_every_key(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, window, cx| {
                view.terminal.exited = true;
                let cmd_a = KeyDownEvent {
                    keystroke: gpui::Keystroke {
                        modifiers: Modifiers {
                            platform: true,
                            ..Modifiers::default()
                        },
                        key: "a".into(),
                        key_char: None,
                    },
                    is_held: false,
                    prefer_character_input: false,
                };
                view.on_key_down(&cmd_a, window, cx);
                assert!(
                    view.terminal.term.lock().selection.is_none(),
                    "an exited local pane is finished; its keyboard is unchanged"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn buffer_search_honors_case_and_regex_toggles(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);

        daemon.send(DaemonMsg::Output(
            b"Hello World\r\nhello world\r\nWORLD wide\r\n".to_vec(),
        ));

        for _ in 0..200 {
            let ready = window
                .update(cx, |v, _, _| {
                    let term = v.terminal.term.lock();
                    let grid = term.grid();
                    (0..grid.screen_lines() as i32)
                        .any(|l| (0..grid.columns()).any(|c| grid[Line(l)][Column(c)].c == 'W'))
                })
                .unwrap();
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, window, cx| {
                fn set_query(
                    view: &mut TerminalView,
                    q: &str,
                    window: &mut Window,
                    cx: &mut Context<TerminalView>,
                ) {
                    let input = view.search.as_ref().unwrap().input.clone();
                    input.update(cx, |s, cx| s.set_value(q, window, cx));
                    view.recompute_matches(cx);
                }

                view.open_search(window, cx);
                assert!(view.search.is_some(), "Cmd+F opens the bar");

                set_query(view, "world", window, cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 3);
                assert!(!view.search_regex_error);

                view.search_case_sensitive = true;
                view.recompute_matches(cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 1);
                view.search_case_sensitive = false;

                set_query(view, "wor.d", window, cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 0);
                view.search_regex = true;
                view.recompute_matches(cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 3);

                view.search_regex = true;
                set_query(view, "(", window, cx);
                assert!(view.search_regex_error);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 0);
                view.search_regex = false;
                view.recompute_matches(cx);
                assert!(!view.search_regex_error);

                view.close_search(window, cx);
                assert_eq!(view.search_last_query, "(");
                assert!(view.search.is_none());
            })
            .unwrap();
    }

    /// A match point is an absolute (line, column) against the width it was
    /// scanned at, so a column change reflows the text out from under every
    /// highlight. Output rescans them, but a quiet pane has none coming —
    /// the resize itself has to rescan (#586).
    #[gpui::test]
    fn a_column_resize_rescans_the_open_searchs_highlights(cx: &mut TestAppContext) {
        // Rooted: the open bar's input reaches for `Root` when a frame draws
        // (see `rooted_harness`).
        let (window, view, daemon) = rooted_harness(cx);

        // 76 columns of text: one row at 80 wide, wrapped onto two at 40.
        let mut line = vec![b'a'; 70];
        line.extend_from_slice(b"needle\r\n");
        daemon.send(DaemonMsg::Output(line));

        for _ in 0..200 {
            let ready = cx.update(|cx| {
                let v = view.read(cx);
                let term = v.terminal.term.lock();
                let grid = term.grid();
                (0..grid.screen_lines() as i32)
                    .any(|l| (0..grid.columns()).any(|c| grid[Line(l)][Column(c)].c == 'n'))
            });
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| {
                    v.open_search(window, cx);
                    let input = v.search.as_ref().unwrap().input.clone();
                    input.update(cx, |s, cx| s.set_value("needle", window, cx));
                    v.recompute_matches(cx);
                    let before = *v.search.as_ref().unwrap().matches[0].start();
                    assert_eq!(
                        (before.line.0, before.column.0),
                        (0, 70),
                        "one unwrapped row at 80 columns"
                    );

                    // The grid does not move until the daemon echoes `Size`.
                    // That echo wakes the view, and the debounced rescan is
                    // what puts the highlight on the reflowed needle. Where
                    // exactly the wrapped half lands is the grid's business —
                    // what matters is that the highlight leaves column 70.
                    v.set_grid_size(40, 24, px(8.), px(17.), 1., cx);
                });
            })
            .unwrap();
        daemon.send(DaemonMsg::Size(WinSize {
            cols: 40,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        }));
        for _ in 0..200 {
            let ready = cx.update(|cx| view.read(cx).terminal.term.lock().columns() == 40);
            if ready {
                break;
            }
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            cx.update(|cx| view.read(cx).terminal.term.lock().columns() == 40),
            "the size echo never reflowed the grid"
        );
        for _ in 0..8 {
            cx.executor()
                .advance_clock(super::super::search::SCAN_DEBOUNCE * 2);
            cx.run_until_parked();
        }

        let after = window
            .update(cx, |_, _, cx| {
                view.update(cx, |v, cx| {
                    let after = *v.search.as_ref().unwrap().matches[0].start();
                    assert_ne!(
                        (after.line.0, after.column.0),
                        (0, 70),
                        "column 70 does not even exist at 40 wide — a stale point"
                    );
                    let term = v.terminal.term.lock();
                    let cell = term.grid()[after.line][after.column].c;
                    drop(term);
                    assert_eq!(cell, 'n', "the highlight follows the reflowed text");

                    // A rows-only change reflows nothing. The echo still wakes
                    // a rescan; the highlight has to stay where the column
                    // change put it.
                    v.set_grid_size(40, 12, px(8.), px(17.), 1., cx);
                    (after.line.0, after.column.0)
                })
            })
            .unwrap();
        daemon.send(DaemonMsg::Size(WinSize {
            cols: 40,
            rows: 12,
            cell_w: 8,
            cell_h: 17,
        }));
        for _ in 0..200 {
            let ready = cx.update(|cx| view.read(cx).terminal.term.lock().screen_lines() == 12);
            if ready {
                break;
            }
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            cx.update(|cx| view.read(cx).terminal.term.lock().screen_lines() == 12),
            "the size echo never reflowed the grid"
        );
        for _ in 0..8 {
            cx.executor()
                .advance_clock(super::super::search::SCAN_DEBOUNCE * 2);
            cx.run_until_parked();
        }
        window
            .update(cx, |_, _, cx| {
                view.update(cx, |v, _| {
                    let still = *v.search.as_ref().unwrap().matches[0].start();
                    assert_eq!((still.line.0, still.column.0), after);
                });
            })
            .unwrap();
    }

    /// The composer end to end on a live pane: laid over the agent's input
    /// once the input is on screen, a message leaves as one paste and then its
    /// own Enter, the box steps aside — keyboard and all — while the agent
    /// shows something else where its input goes, comes back when the input
    /// does, and is gone when the agent quits.
    #[gpui::test]
    fn the_composer_stands_in_for_the_agents_input_and_steps_aside_for_its_prompts(
        cx: &mut TestAppContext,
    ) {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};

        let (window, view, daemon) = rooted_harness(cx);
        let draw = |cx: &mut TestAppContext| {
            window.update(cx, |_, window, _| window.refresh()).unwrap();
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(400));
            cx.run_until_parked();
        };
        let wait_for = |cx: &mut TestAppContext, want: &dyn Fn(&TerminalView) -> bool| {
            for _ in 0..200 {
                if cx.update(|cx| want(view.read(cx))) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("the pane never got there");
        };
        let screen_has = |v: &TerminalView, needle: char| {
            let term = v.terminal.term.lock();
            let grid = term.grid();
            (0..grid.screen_lines() as i32)
                .any(|l| (0..grid.columns()).any(|c| grid[Line(l)][Column(c)].c == needle))
        };
        // Let the window lay the grid out first: the agent draws for the size
        // it is given, as Claude Code does after every resize.
        draw(cx);
        let (cols, lines) = cx.update(|cx| {
            let term = view.read(cx).terminal.term.lock();
            (term.columns(), term.screen_lines())
        });
        let rule = "─".repeat(cols);
        // Claude Code's input, as it draws it: the prompt between two rules
        // at the bottom of the screen, its status row under them.
        let input = format!(
            "\x1b[2J\x1b[H⏺ Ready.\x1b[{};1H{rule}\x1b[{};1H❯ \x1b[{};1H{rule}\x1b[{};1H  ? for shortcuts",
            lines - 3,
            lines - 2,
            lines - 1,
            lines,
        );

        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| v.toggle_composer(window, cx));
            })
            .unwrap();
        assert!(
            cx.update(|cx| view.read(cx).composer.is_none()),
            "a shell has nobody to compose for"
        );

        daemon.send(DaemonMsg::Agent(Some(CLIAgent::Claude)));
        daemon.send(DaemonMsg::Output(
            format!("\x1b[?2004h{input}").into_bytes(),
        ));
        wait_for(cx, &|v| {
            v.agent() == Some(CLIAgent::Claude)
                && screen_has(v, '❯')
                && v.terminal
                    .term
                    .lock()
                    .mode()
                    .contains(TermMode::BRACKETED_PASTE)
        });
        let rows = cx.update(|cx| view.read(cx).terminal.size().rows);

        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| v.toggle_composer(window, cx));
            })
            .unwrap();
        draw(cx);
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| {
                    assert!(
                        v.composer_shown(),
                        "the input is on screen, so the box is over it"
                    );
                    let input = v.composer.as_ref().unwrap().input.clone();
                    assert!(
                        input.read(cx).focus_handle(cx).is_focused(window),
                        "opening the box puts the caret in it"
                    );
                    input.update(cx, |s, cx| s.set_value("one\ntwo", window, cx));
                });
            })
            .unwrap();
        assert_eq!(
            cx.update(|cx| view.read(cx).terminal.size().rows),
            rows,
            "laid over the input, the box leaves the grid its rows"
        );

        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| v.submit_composer(window, cx));
            })
            .unwrap();
        draw(cx);
        assert_eq!(next_input(&daemon), b"\x1b[200~one\ntwo\x1b[201~");
        assert_eq!(next_input(&daemon), b"\r", "Enter goes as its own write");
        assert_eq!(
            cx.update(|cx| view
                .read(cx)
                .composer
                .as_ref()
                .unwrap()
                .input
                .read(cx)
                .value()),
            "",
            "a sent message leaves the box"
        );

        // A click on the grid takes the keyboard out of the box, and it
        // stays out: the box does not pull it back on the next frame.
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| window.focus(&v.focus_handle, cx));
            })
            .unwrap();
        draw(cx);
        draw(cx);
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, _| {
                    assert!(
                        v.focus_handle.is_focused(window),
                        "the keyboard stays where the click put it"
                    );
                    assert!(!v.composer_focused);
                });
            })
            .unwrap();

        // A paste at the grid is for the input under the box: it goes into
        // the box, and the keyboard with it — never unseen into the agent.
        view.update(cx, |v, cx| v.paste("pasted\nhere".into(), cx));
        draw(cx);
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| {
                    let input = v.composer.as_ref().unwrap().input.clone();
                    assert_eq!(input.read(cx).value(), "pasted\nhere");
                    assert!(input.read(cx).focus_handle(cx).is_focused(window));
                    input.update(cx, |s, cx| s.set_value("", window, cx));
                });
            })
            .unwrap();

        daemon.send(
            // A picker takes the input's place. Past the grace period the box
            // steps aside and the keyboard goes to the TUI.
            DaemonMsg::Output(
                b"\x1b[2J\x1b[H Select model\r\n \xe2\x9d\xaf 1. Opus\r\n   2. Sonnet".to_vec(),
            ),
        );
        wait_for(cx, &|v| !screen_has(v, '─'));
        draw(cx);
        std::thread::sleep(super::super::composer::STEP_ASIDE_AFTER);
        draw(cx);
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, _| {
                    assert!(!v.composer_shown(), "the picker is not covered");
                    assert!(
                        v.focus_handle.is_focused(window),
                        "the picker has the keyboard"
                    );
                });
            })
            .unwrap();

        daemon.send(
            // The input comes back, and so do the box and its caret.
            DaemonMsg::Output(input.clone().into_bytes()),
        );
        wait_for(cx, &|v| screen_has(v, '─'));
        draw(cx);
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| {
                    assert!(v.composer_shown());
                    let input = v.composer.as_ref().unwrap().input.clone();
                    assert!(input.read(cx).focus_handle(cx).is_focused(window));
                });
            })
            .unwrap();

        daemon.send(
            // A question the hooks report steps it aside at once, and nothing in
            // the box is sent as the answer.
            DaemonMsg::AgentStatus(Some(AgentSessionState {
                status: AgentStatus::Waiting,
                rich: true,
                ..Default::default()
            })),
        );
        wait_for(cx, &|v| {
            v.agent_session()
                .is_some_and(|s| s.status == AgentStatus::Waiting)
        });
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| {
                    let input = v.composer.as_ref().unwrap().input.clone();
                    input.update(cx, |s, cx| s.set_value("yes", window, cx));
                    v.submit_composer(window, cx);
                });
            })
            .unwrap();
        draw(cx);
        assert_eq!(
            next_input_until_timeout(&daemon),
            None,
            "a question in the TUI is not answered from the box"
        );
        assert!(cx.update(|cx| !view.read(cx).composer_shown()));
        assert_eq!(
            cx.update(|cx| view
                .read(cx)
                .composer
                .as_ref()
                .unwrap()
                .input
                .read(cx)
                .value()),
            "yes",
            "the held message stays in the box"
        );

        daemon.send(DaemonMsg::Agent(None));
        wait_for(cx, &|v| v.agent().is_none());
        draw(cx);
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, _| {
                    assert!(!v.composer_shown(), "no agent, no box");
                    assert!(
                        v.focus_handle.is_focused(window),
                        "the keyboard goes back to the terminal"
                    );
                });
            })
            .unwrap();
    }

    /// The selection that seeds the search query is the thing being searched
    /// for — opening the bar must not erase it, and closing the bar must not
    /// either (#584). Only *changing* the query retires it.
    #[gpui::test]
    fn the_search_bar_opens_and_closes_around_a_grid_selection(cx: &mut TestAppContext) {
        let (window, view, daemon) = rooted_harness(cx);

        daemon.send(DaemonMsg::Output(
            b"some needle in the haystack\r\n".to_vec(),
        ));
        for _ in 0..200 {
            let ready = cx.update(|cx| {
                let v = view.read(cx);
                let term = v.terminal.term.lock();
                let grid = term.grid();
                (0..grid.screen_lines() as i32)
                    .any(|l| (0..grid.columns()).any(|c| grid[Line(l)][Column(c)].c == 'n'))
            });
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| {
                    // Select "needle" (row 0, columns 5..=10 — the end side
                    // includes its cell) — the seed the bar picks up.
                    let mut sel = Selection::new(
                        SelectionType::Simple,
                        Point::new(Line(0), Column(5)),
                        Side::Left,
                    );
                    sel.update(Point::new(Line(0), Column(10)), Side::Right);
                    v.terminal.term.lock().selection = Some(sel);

                    v.open_search(window, cx);
                    assert_eq!(
                        v.search.as_ref().unwrap().input.read(cx).value(),
                        "needle",
                        "the bar opens on the selection as its query"
                    );
                    assert!(
                        v.terminal.term.lock().selection.is_some(),
                        "opening the bar must not eat the selection that seeded it"
                    );

                    v.close_search(window, cx);
                    assert!(
                        v.terminal.term.lock().selection.is_some(),
                        "closing the bar must not eat it either"
                    );

                    // But a query the user *changed* retires the old selection:
                    // it no longer names what the search is about.
                    v.open_search(window, cx);
                    let input = v.search.as_ref().unwrap().input.clone();
                    input.update(cx, |s, cx| s.set_value("haystack", window, cx));
                    v.recompute_matches(cx);
                    assert!(
                        v.terminal.term.lock().selection.is_none(),
                        "a changed query retires the stale selection"
                    );
                });
            })
            .unwrap();
    }

    #[gpui::test]
    fn output_under_an_open_search_bar_is_searched_too(cx: &mut TestAppContext) {
        let (window, view, daemon) = rooted_harness(cx);

        fn wait_for(cx: &mut TestAppContext, view: &Entity<TerminalView>, needle: char) {
            for _ in 0..200 {
                let seen = cx.update(|cx| {
                    let v = view.read(cx);
                    let term = v.terminal.term.lock();
                    let grid = term.grid();
                    (0..grid.screen_lines() as i32)
                        .any(|l| (0..grid.columns()).any(|c| grid[Line(l)][Column(c)].c == needle))
                });
                if seen {
                    return;
                }
                cx.run_until_parked();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("the pane never printed {needle:?}");
        }

        daemon.send(DaemonMsg::Output(b"world 1\r\n".to_vec()));
        wait_for(cx, &view, '1');

        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| {
                    v.open_search(window, cx);
                    let input = v.search.as_ref().unwrap().input.clone();
                    input.update(cx, |s, cx| s.set_value("world", window, cx));
                    v.recompute_matches(cx);
                    assert_eq!(v.search.as_ref().unwrap().matches.len(), 1);
                });
            })
            .unwrap();

        daemon.send(
            // A second line arrives while the bar is up. Until it is rescanned the
            // count still says 1, and the one highlight it does draw has slid onto
            // whatever line took the old one's place.
            DaemonMsg::Output(b"world 2\r\n".to_vec()),
        );
        wait_for(cx, &view, '2');
        for _ in 0..8 {
            cx.executor()
                .advance_clock(super::super::search::SCAN_DEBOUNCE * 2);
            cx.run_until_parked();
        }

        cx.update(|cx| {
            let v = view.read(cx);
            let search = v.search.as_ref().expect("the bar is still open");
            assert_eq!(
                search.matches.len(),
                2,
                "the line printed under the open bar has to be counted too"
            );
            assert_eq!(
                search.current_index,
                Some(0),
                "and the match the user was standing on is still the one they are on"
            );
            assert!(
                !v.search_scan_armed,
                "the debounce has to settle, not rescan forever"
            );
        });
    }

    /// Print lines into the grid and post the wakeup the reader thread would
    /// have posted behind them.
    ///
    /// Going through the parser directly rather than the socket keeps the two
    /// tests below off both real time and the clock: the pane has printed by
    /// the time this returns, and no timer has had a chance to fire.
    fn print(cx: &mut TestAppContext, view: &Entity<TerminalView>, lines: Vec<String>) {
        cx.update(|cx| {
            view.update(cx, |v, cx| {
                {
                    use alacritty_terminal::vte::ansi::Handler as _;
                    let mut term = v.terminal.term.lock();
                    for line in lines {
                        for c in line.chars() {
                            term.input(c);
                        }
                        term.carriage_return();
                        term.linefeed();
                    }
                }
                v.handle_event(AlacEvent::Wakeup, cx);
            })
        });
    }

    /// Filler that scrolls the grid without ever matching the query.
    fn filler(lines: usize) -> Vec<String> {
        (0..lines).map(|i| format!("filler {i}")).collect()
    }

    /// What the grid holds where the current match says its word starts, so a
    /// test can ask the question the eye asks: is the highlight still on it?
    fn word_at_the_match(
        cx: &mut TestAppContext,
        view: &Entity<TerminalView>,
        len: usize,
    ) -> String {
        cx.update(|cx| {
            let v = view.read(cx);
            let start = *v
                .search
                .as_ref()
                .and_then(|s| s.current())
                .expect("a match to stand on")
                .start();
            let term = v.terminal.term.lock();
            let grid = term.grid();
            (0..len)
                .map(|i| grid[start.line][Column(start.column.0 + i)].c)
                .collect()
        })
    }

    /// Print the searched-for word and bury it in the scrollback, then open the
    /// bar on it. Returns with exactly one match, sitting in history.
    fn search_a_buried_word(
        window: &gpui::WindowHandle<gpui_component::Root>,
        cx: &mut TestAppContext,
        view: &Entity<TerminalView>,
    ) {
        let rows = cx.update(|cx| view.read(cx).terminal.term.lock().grid().screen_lines());
        print(cx, view, vec!["world 1".to_string()]);
        print(cx, view, filler(rows + 8));
        cx.update(|cx| {
            assert!(
                view.read(cx).terminal.term.lock().grid().history_size() > 0,
                "the word has to be in the scrollback for the grid to scroll under it"
            );
        });

        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| {
                    v.open_search(window, cx);
                    let input = v.search.as_ref().unwrap().input.clone();
                    input.update(cx, |s, cx| s.set_value("world", window, cx));
                    v.recompute_matches(cx);
                    assert_eq!(v.search.as_ref().unwrap().matches.len(), 1);
                });
            })
            .unwrap();
    }

    /// Drawing must never queue for the grid lock.
    ///
    /// One UI thread paints every pane in every window, and the thread holding
    /// this lock is the pane's own reader part-way through feeding a batch of
    /// output into the emulator. A draw that waited for it would wire one
    /// pane's write speed to the frame rate of the whole window — the read-side
    /// twin of #709, which was this same thread parked in `write(2)`.
    ///
    /// No second thread and no timing: `try_lock` fails against a lock this
    /// thread already holds, so "the reader has it" is reproduced exactly, with
    /// nothing to race. The cost of that trade is what a regression looks like
    /// — put `lock()` back and this test hangs on the re-entry rather than
    /// failing, which reads as a CI timeout on exactly this name.
    #[gpui::test]
    fn a_frame_that_cannot_have_the_grid_leaves_the_previous_one_alone(cx: &mut TestAppContext) {
        use super::super::element::PaintColors;

        let (_window, view, _daemon) = rooted_harness(cx);
        let element = TerminalElement::new(view.clone());
        let mut buf = Vec::new();
        let build = |cx: &mut TestAppContext, buf: &mut Vec<RenderCell>, must_block: bool| {
            cx.update(|cx| {
                let colors = PaintColors::resolve(cx.theme(), cx);
                element.build_grid(&colors, buf, 24, 80, false, cx, must_block)
            })
        };

        assert!(
            build(cx, &mut buf, true).is_some(),
            "the first frame has no previous grid to stand in for it, so it waits and builds"
        );
        assert_eq!(buf.len(), 24 * 80);

        // Shortened so the next call cannot touch the buffer without saying so:
        // building would `clear` and `resize` it back to a full grid.
        buf.truncate(3);
        let term = cx.update(|cx| view.read(cx).terminal.term.clone());
        let held = term.lock();
        let refused = build(cx, &mut buf, false);
        drop(held);

        assert!(
            refused.is_none(),
            "a frame that cannot have the lock says so instead of waiting for it"
        );
        assert_eq!(
            buf.len(),
            3,
            "the previous frame's cells have to survive for that frame to be painted again"
        );

        assert!(
            build(cx, &mut buf, false).is_some(),
            "with the lock free, a frame builds without being told to wait"
        );
        assert_eq!(buf.len(), 24 * 80);
    }

    #[gpui::test]
    fn a_highlight_follows_its_text_as_output_scrolls_under_it(cx: &mut TestAppContext) {
        let (window, view, _daemon) = rooted_harness(cx);
        search_a_buried_word(&window, cx, &view);
        assert_eq!(
            word_at_the_match(cx, &view, 5),
            "world",
            "the scan has to name its own word"
        );

        // Output arrives and the clock never moves, so nothing rescans: the
        // stored point is all the highlight has to go on. Every line printed
        // here slid the grid up under it, and it has to have come along.
        print(cx, &view, filler(7));
        cx.update(|cx| {
            assert!(
                view.read(cx).search_scan_armed,
                "the rescan is still waiting for the pane to go quiet"
            );
        });
        assert_eq!(
            word_at_the_match(cx, &view, 5),
            "world",
            "a match point has to name the row its text scrolled to, \
             not the row it was read from"
        );
    }

    #[gpui::test]
    fn a_pane_that_never_pauses_is_rescanned_anyway(cx: &mut TestAppContext) {
        let (window, view, _daemon) = rooted_harness(cx);
        search_a_buried_word(&window, cx, &view);

        // A pane mid-flood: something new lands every half debounce window, so
        // the pause the rescan is waiting for never comes.
        for i in 0..(super::super::search::SCAN_LAG_LIMIT * 2 + 2) {
            print(cx, &view, vec![format!("world {}", i + 2)]);
            cx.executor()
                .advance_clock(super::super::search::SCAN_DEBOUNCE / 2);
            cx.run_until_parked();
        }

        cx.update(|cx| {
            let search = view
                .read(cx)
                .search
                .as_ref()
                .expect("the bar is still open");
            assert!(
                search.matches.len() > 1,
                "printing without a pause must not hold the scan off forever, \
                 got {} matches",
                search.matches.len()
            );
        });
    }

    #[gpui::test]
    fn child_exit_marks_the_view_exited(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.handle_event(AlacEvent::Exit, cx);
                assert!(view.terminal.exited);
                assert_eq!(view.title, "ntty7 — process exited");
            })
            .unwrap();
    }

    /// Holding Backspace on an empty bash prompt (or Tab with nothing to
    /// complete) rings the bell at key-repeat rate. Every flash used to arm its
    /// own clear timer, so the first bell's timer blanked a flash the fifth bell
    /// had just re-lit, and the pane strobed for as long as the key was held
    /// (#874).
    #[gpui::test]
    fn a_bell_rung_at_key_repeat_rate_holds_one_steady_flash(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        let lit =
            |cx: &mut TestAppContext| window.update(cx, |view, _, _| view.bell_flash).unwrap();

        let repeat = std::time::Duration::from_millis(33);
        let mut dark = Vec::new();
        for i in 0..30 {
            window
                .update(cx, |view, _, cx| view.handle_event(AlacEvent::Bell, cx))
                .unwrap();
            cx.executor().advance_clock(repeat);
            cx.run_until_parked();
            if !lit(cx) {
                dark.push(i);
            }
        }
        assert!(
            dark.is_empty(),
            "the flash went dark between bells after repeats {dark:?}"
        );

        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        assert!(!lit(cx), "the flash outlived the last bell");
    }

    #[gpui::test]
    fn text_area_size_request_replies_with_the_current_geometry(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        let want = window
            .update(cx, |view, _, cx| {
                let size = view.terminal.size();
                let fmt = std::sync::Arc::new(|ws: alacritty_terminal::event::WindowSize| {
                    format!("{}x{}", ws.num_cols, ws.num_lines)
                });
                view.handle_event(AlacEvent::TextAreaSizeRequest(fmt), cx);
                format!("{}x{}", size.cols, size.rows)
            })
            .unwrap();
        assert_eq!(next_input(&daemon), want.into_bytes());
    }

    #[gpui::test]
    fn daemon_output_reaches_the_grid_through_the_event_pump(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);

        let read_row = |cx: &mut TestAppContext, len: usize| -> String {
            window
                .update(cx, |view, _, _| {
                    let term = view.terminal.term.clone();
                    let term = term.lock();
                    let grid = term.grid();
                    (0..len)
                        .map(|c| grid[alacritty_terminal::index::Line(0)][Column(c)].c)
                        .collect()
                })
                .unwrap()
        };
        let wait_for = |cx: &mut TestAppContext, want: &str| {
            let mut got = String::new();
            for _ in 0..400 {
                cx.run_until_parked();
                got = read_row(cx, want.chars().count());
                if got == want {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            got
        };

        daemon.send(DaemonMsg::Output(b"hello".to_vec()));
        assert_eq!(wait_for(cx, "hello"), "hello");

        daemon.send(DaemonMsg::Output(b" again".to_vec()));
        assert_eq!(wait_for(cx, "hello again"), "hello again");
    }

    #[gpui::test]
    fn copy_on_select_writes_the_clipboard_at_mouse_up(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);

        daemon.send(DaemonMsg::Output(b"hello world".to_vec()));
        for _ in 0..400 {
            cx.run_until_parked();
            let row: String = window
                .update(cx, |view, _, _| {
                    let term = view.terminal.term.clone();
                    let term = term.lock();
                    (0..11)
                        .map(|c| term.grid()[alacritty_terminal::index::Line(0)][Column(c)].c)
                        .collect()
                })
                .unwrap();
            if row == "hello world" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let drag_hello = |cx: &mut TestAppContext| {
            window
                .update(cx, |view, _, cx| {
                    view.on_select_start(0, 0, true, 1, false, cx);
                    view.on_select_update(4, 0, false, cx);
                    view.on_select_end(cx);
                })
                .unwrap();
        };
        drag_hello(cx);
        assert_eq!(
            cx.update(|cx| cx.read_from_clipboard()),
            None,
            "default-off must never write the clipboard"
        );

        cx.update(|cx| cx.update_global::<Config, _>(|cfg, _| cfg.copy_on_select = true));
        drag_hello(cx);
        let text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("hello"));

        let selected = window
            .update(cx, |view, _, _| {
                view.terminal.term.lock().selection.is_some()
            })
            .unwrap();
        assert!(
            selected,
            "copy-on-select must keep the selection highlighted"
        );
    }

    #[gpui::test]
    fn ctrl_c_copy_consumes_the_selection_so_the_next_press_is_sigint(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);

        daemon.send(DaemonMsg::Output(b"hello world".to_vec()));
        for _ in 0..400 {
            cx.run_until_parked();
            let row: String = window
                .update(cx, |view, _, _| {
                    let term = view.terminal.term.clone();
                    let term = term.lock();
                    (0..11)
                        .map(|c| term.grid()[alacritty_terminal::index::Line(0)][Column(c)].c)
                        .collect()
                })
                .unwrap();
            if row == "hello world" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, window, cx| {
                view.on_select_start(0, 0, true, 1, false, cx);
                view.on_select_update(4, 0, false, cx);
                view.on_select_end(cx);
                assert!(view.has_selection(), "the drag must leave a selection");

                let consumed = view.handle_cmd_shortcut(&key("ctrl-c"), window, cx);
                assert!(matches!(consumed, CmdKey::Consumed));
                assert!(
                    !view.has_selection(),
                    "the Ctrl+C copy must consume the selection"
                );

                let fell_through = view.handle_cmd_shortcut(&key("ctrl-c"), window, cx);
                assert!(matches!(fell_through, CmdKey::FallThrough));
            })
            .unwrap();
        let text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("hello"));
    }

    #[gpui::test]
    fn cmd_v_pastes_on_the_alternate_screen(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("echo hi".into())));
        alt_screen_ready(&window, cx, &daemon);

        window
            .update(cx, |view, window, cx| {
                let pasted = view.handle_cmd_shortcut(&key("cmd-v"), window, cx);
                assert!(
                    matches!(pasted, CmdKey::Consumed),
                    "Cmd+V carries no control code and pastes on every screen"
                );
            })
            .unwrap();
        assert_eq!(next_input(&daemon), b"echo hi".to_vec());
        assert_eq!(next_input_until_timeout(&daemon), None);
    }

    /// The Ctrl+V half of the same key, through the real keymap: whether it
    /// pastes is the `AlternatePaste` binding's decision, not this view's, so
    /// these two drive it the way a user does rather than calling in.
    #[cfg(not(target_os = "macos"))]
    #[gpui::test]
    fn ctrl_v_pastes_at_a_prompt(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, daemon) = harness(cx);
        cx.update(|cx| crate::ui::keymap::init(cx));
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("echo hi".into())));
        window
            .update(cx, |view, window, cx| {
                assert!(!view.on_alt_screen());
                window.activate_window();
                view.focus_handle.focus(window, cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("ctrl-v");

        assert_eq!(next_input_until_timeout(&daemon), Some(b"echo hi".to_vec()));
    }

    #[cfg(not(target_os = "macos"))]
    #[gpui::test]
    fn ctrl_v_reaches_a_full_screen_program_as_syn(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, daemon) = harness(cx);
        cx.update(|cx| crate::ui::keymap::init(cx));
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("echo hi".into())));
        alt_screen_ready(&window, cx, &daemon);
        window
            .update(cx, |view, window, cx| {
                window.activate_window();
                view.focus_handle.focus(window, cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("ctrl-v");

        assert_eq!(next_input_until_timeout(&daemon), Some(vec![0x16]));
    }

    /// The other half of that rule, which the binding's context cannot state.
    ///
    /// gpui matches a keystroke against the frame it last *painted*, so
    /// `!alt_screen` outlives the switch by a frame: launch a full-screen
    /// program and hit Ctrl+V before the next paint and the keymap still
    /// believes the pane is at a prompt. The action asks the grid itself, so
    /// the clipboard never lands in a program that would run it as commands.
    #[gpui::test]
    fn alternate_paste_asks_the_grid_and_not_the_last_frame(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("echo hi".into())));

        window
            .update(cx, |view, _window, cx| {
                assert!(!view.on_alt_screen());
                view.alternate_paste(cx);
            })
            .unwrap();
        assert_eq!(next_input(&daemon), b"echo hi".to_vec());

        alt_screen_ready(&window, cx, &daemon);
        window
            .update(cx, |view, _window, cx| view.alternate_paste(cx))
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&daemon),
            None,
            "the paste is withheld even when the frame that matched said otherwise"
        );
    }

    /// `Ctrl-^`, which used to die in a hardcoded block that swallowed every
    /// Ctrl+digit off macOS — nothing has claimed those chords since tabs
    /// moved to Alt+1..9.
    #[gpui::test]
    fn ctrl_6_reaches_the_pty_as_rs(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, daemon) = harness(cx);
        cx.update(crate::ui::keymap::init);
        window
            .update(cx, |view, window, cx| {
                window.activate_window();
                view.focus_handle.focus(window, cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("ctrl-6");

        assert_eq!(next_input_until_timeout(&daemon), Some(vec![0x1e]));
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn cmd_backspace_reaches_a_foreground_tui_as_ctrl_u(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, daemon) = harness(cx);
        window
            .update(cx, |view, window, cx| {
                view.on_key_down(
                    &KeyDownEvent {
                        keystroke: gpui::Keystroke {
                            modifiers: Modifiers {
                                platform: true,
                                ..Modifiers::default()
                            },
                            key: "backspace".into(),
                            key_char: None,
                        },
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
            })
            .unwrap();

        assert_eq!(next_input_until_timeout(&daemon), Some(vec![0x15]));
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn cmd_navigation_reaches_a_foreground_tui_as_readline_controls(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, daemon) = harness(cx);

        for (key, expected) in [("left", 0x01), ("right", 0x05), ("delete", 0x0b)] {
            window
                .update(cx, |view, window, cx| {
                    view.on_key_down(
                        &KeyDownEvent {
                            keystroke: gpui::Keystroke {
                                modifiers: Modifiers {
                                    platform: true,
                                    ..Modifiers::default()
                                },
                                key: key.into(),
                                key_char: None,
                            },
                            is_held: false,
                            prefer_character_input: false,
                        },
                        window,
                        cx,
                    );
                })
                .unwrap();
            assert_eq!(next_input_until_timeout(&daemon), Some(vec![expected]));
        }
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn cmd_backspace_releases_held_input_before_ctrl_u(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, daemon) = harness(cx);
        daemon.send(DaemonMsg::Prompt {
            active: true,
            at_prompt: false,
            last_exit: None,
        });
        for _ in 0..200 {
            cx.run_until_parked();
            let gap = window
                .update(cx, |view, _, _| {
                    view.terminal.shell_active() && !view.terminal.at_prompt()
                })
                .unwrap();
            if gap {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, window, cx| {
                view.commit_text("ls", cx);
                view.on_key_down(
                    &KeyDownEvent {
                        keystroke: gpui::Keystroke {
                            modifiers: Modifiers {
                                platform: true,
                                ..Modifiers::default()
                            },
                            key: "backspace".into(),
                            key_char: None,
                        },
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
            })
            .unwrap();

        assert_eq!(
            next_input_until_timeout(&daemon),
            Some(b"ls".to_vec()),
            "held text must reach the PTY before the line-kill"
        );
        assert_eq!(
            next_input_until_timeout(&daemon),
            Some(vec![0x15]),
            "Ctrl-U must follow the text it clears"
        );
    }

    #[gpui::test]
    fn paste_to_the_pty_consumes_the_selection(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.select_all(cx);
                assert!(view.has_selection());
                view.paste("echo hi".into(), cx);
                assert!(
                    !view.has_selection(),
                    "a PTY paste must consume the selection"
                );
            })
            .unwrap();
        assert_eq!(next_input(&daemon), b"echo hi".to_vec());
    }

    /// #844: a TUI that resets DECTCEM and draws its own reverse-video caret
    /// gets no terminal caret painted over it — focused, unfocused, and after
    /// a re-attach replays its screen — and `?25h` brings the caret back.
    ///
    /// The stream is the reporter's shape: an alternate screen and 69 `?25l`
    /// interleaved with 75 `?25h`, the last one a hide. What is checked is the
    /// snapshot a real paint left behind, through the same `painted_cursor`
    /// the element paints from.
    #[gpui::test]
    fn dectcem_reset_paints_no_terminal_caret_over_the_tuis_own(cx: &mut TestAppContext) {
        use crate::core::config::CursorStyle;

        // An Ink-style frame: our own caret as one reverse-video cell at
        // row 5 column 13, and the real cursor parked on it.
        const FRAME: &[u8] = b"\x1b[5;1H\x1b[2K> type here \x1b[7m \x1b[27m\x1b[5;13H";
        let mut stream = b"\x1b[?1049h".to_vec();
        stream.extend(std::iter::repeat_n(&b"\x1b[?25h"[..], 7).flatten());
        for _ in 0..68 {
            stream.extend_from_slice(b"\x1b[?25l");
            stream.extend_from_slice(FRAME);
            stream.extend_from_slice(b"\x1b[?25h");
        }
        stream.extend_from_slice(b"\x1b[?25l");
        stream.extend_from_slice(FRAME);
        assert_eq!(stream.windows(6).filter(|w| w == b"\x1b[?25l").count(), 69);
        assert_eq!(stream.windows(6).filter(|w| w == b"\x1b[?25h").count(), 75);

        type Painted = Option<(usize, usize, CursorStyle)>;
        // Paints frames until the one the pane settles on matches `want`, and
        // returns the last painted caret either way.
        let paint_until = |window: &gpui::WindowHandle<TerminalView>,
                           cx: &mut TestAppContext,
                           focused: bool,
                           want: &dyn Fn(Painted) -> bool| {
            let mut painted = None;
            for _ in 0..400 {
                window
                    .update(cx, |view, window, cx| {
                        if focused {
                            window.activate_window();
                            view.focus_handle.focus(window, cx);
                        } else {
                            window.blur();
                        }
                        cx.notify();
                    })
                    .unwrap();
                let mut vcx = gpui::VisualTestContext::from_window((*window).into(), cx);
                vcx.update(|window, _| window.refresh());
                vcx.run_until_parked();
                let (text, snap) = window
                    .update(cx, |view, window, _| {
                        assert_eq!(view.focus_handle.is_focused(window), focused);
                        use alacritty_terminal::grid::Dimensions as _;
                        use alacritty_terminal::index::{Column, Line};
                        let term = view.terminal.term.lock();
                        let row = &term.grid()[Line(4)];
                        let text = (0..term.grid().columns())
                            .map(|col| row[Column(col)].c)
                            .collect::<String>();
                        (text, view.grid_snap.as_ref().map(|s| s.painted_cursor()))
                    })
                    .unwrap();
                if text.starts_with("> type here")
                    && let Some(p) = snap
                {
                    painted = p;
                    if want(p) {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            painted
        };

        let (window, daemon) = harness(cx);
        daemon.send(DaemonMsg::Output(stream.clone()));
        let focused = paint_until(&window, cx, true, &|p| p.is_none());
        assert_eq!(
            focused, None,
            "a focused pane painted its caret over a TUI that reset DECTCEM"
        );
        let unfocused = paint_until(&window, cx, false, &|p| p.is_none());
        assert_eq!(
            unfocused, None,
            "an unfocused pane painted its hollow caret over a TUI that reset DECTCEM"
        );

        daemon.send(DaemonMsg::Output(b"\x1b[?25h".to_vec()));
        let shown = paint_until(&window, cx, true, &|p| p.is_some());
        assert_eq!(
            shown.map(|(row, col, _)| (row, col)),
            Some((4, 12)),
            "`?25h` must bring the caret back where the program parked it"
        );
    }

    #[gpui::test]
    fn child_exit_emits_the_close_event_but_disconnect_does_not(cx: &mut TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;

        let subscribe = |window: &gpui::WindowHandle<TerminalView>, cx: &mut TestAppContext| {
            let got = Rc::new(Cell::new(false));
            let seen = got.clone();
            window
                .update(cx, |_, _, cx| {
                    let this = cx.entity();
                    cx.subscribe(&this, move |_, _, _: &ChildExited, _| seen.set(true))
                        .detach();
                })
                .unwrap();
            got
        };
        let wait_exited = |window: &gpui::WindowHandle<TerminalView>, cx: &mut TestAppContext| {
            for _ in 0..400 {
                cx.run_until_parked();
                let exited = window
                    .update(cx, |view, _, _| view.terminal.exited)
                    .unwrap();
                if exited {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("the view never noticed the exit");
        };

        let (window, daemon) = harness(cx);
        let got = subscribe(&window, cx);
        daemon.send(DaemonMsg::Exited { code: Some(0) });
        wait_exited(&window, cx);
        assert!(got.get(), "a genuine child exit must emit ChildExited");

        let (window, daemon) = harness(cx);
        let got = subscribe(&window, cx);
        daemon.close();
        wait_exited(&window, cx);
        assert!(!got.get(), "a daemon disconnect must not emit ChildExited");
    }

    /// #958: `prompt_cursor_style` shapes the shell's own caret at a prompt,
    /// leaves the running program's cursor alone, and steps aside for a vi-mode
    /// prompt, whose insert/normal shapes are the shell's to set.
    #[gpui::test]
    fn prompt_cursor_style_shapes_only_the_prompt(cx: &mut TestAppContext) {
        use crate::core::config::{CursorStyle, PromptCursorStyle};

        let (window, daemon) = harness(cx);
        let shape = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                let view = window.entity(cx).unwrap();
                TerminalElement::new(view).prompt_cursor_shape(cx)
            })
        };
        let wait_for = |cx: &mut TestAppContext, want: &dyn Fn(&TerminalView) -> bool| {
            for _ in 0..200 {
                cx.run_until_parked();
                if window.update(cx, |view, _, _| want(view)).unwrap() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("the pane never reached the expected prompt state");
        };
        cx.update(|cx| cx.global_mut::<Config>().prompt_cursor_style = PromptCursorStyle::Bar);

        assert_eq!(
            shape(cx),
            None,
            "no prompt reported yet: a program owns the cursor"
        );

        daemon.send(DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        });
        wait_for(cx, &|view| view.terminal.at_prompt());
        assert_eq!(shape(cx), Some(CursorStyle::Bar));

        cx.update(|cx| cx.global_mut::<Config>().prompt_cursor_style = PromptCursorStyle::Follow);
        assert_eq!(shape(cx), None, "follow leaves the prompt on cursor_style");
        cx.update(|cx| cx.global_mut::<Config>().prompt_cursor_style = PromptCursorStyle::Bar);

        daemon.send(DaemonMsg::Output(
            b"\x1b]133;V;1\x07\x1b]133;B\x07".to_vec(),
        ));
        wait_for(cx, &|view| view.terminal.shell_vi_mode());
        assert_eq!(
            shape(cx),
            None,
            "a vi-mode prompt keeps the shell's own shapes"
        );

        daemon.send(DaemonMsg::Prompt {
            active: true,
            at_prompt: false,
            last_exit: None,
        });
        wait_for(cx, &|view| !view.terminal.at_prompt());
        assert_eq!(shape(cx), None, "a running command gets cursor_style back");
    }

    /// Typing, arrows, Tab and Ctrl-R at an OSC 133 prompt all reach the PTY.
    /// The shell's own line editor owns the line.
    #[gpui::test]
    fn a_prompt_sends_typing_arrows_tab_and_ctrl_r_to_the_pty(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        prompt_ready(&window, cx, &daemon);
        window
            .update(cx, |view, window, cx| {
                assert!(
                    view.terminal.at_prompt(),
                    "OSC 133 prompt tracking stays live"
                );
                type_char(view, "h", window, cx);
                view.on_key_down(
                    &KeyDownEvent {
                        keystroke: key("up"),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
                view.tab_pressed(true, cx);
                view.on_key_down(
                    &KeyDownEvent {
                        keystroke: key("ctrl-r"),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
            })
            .unwrap();
        assert_eq!(next_input_until_timeout(&daemon), Some(b"h".to_vec()));
        assert_eq!(next_input_until_timeout(&daemon), Some(b"\x1b[A".to_vec()));
        assert_eq!(next_input_until_timeout(&daemon), Some(b"\t".to_vec()));
        assert_eq!(next_input_until_timeout(&daemon), Some(vec![0x12]));
    }

    /// An SSH session that dies on the alternate screen must come back to the
    /// host prompt: off the alt screen, cursor shown, OSC 133 prompt live.
    #[gpui::test]
    fn ssh_drop_mid_tui_recovers_at_the_next_prompt(cx: &mut TestAppContext) {
        use alacritty_terminal::vte::ansi::CursorShape;

        let (window, daemon) = harness(cx);

        daemon.send(DaemonMsg::Output(b"\x1b[?1049h\x1b[?25l".to_vec()));
        daemon.send(DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(255),
        });

        let mut state = (false, true, true);
        for _ in 0..400 {
            cx.run_until_parked();
            state = window
                .update(cx, |view, _, _| {
                    let hidden = matches!(
                        view.terminal.term.lock().renderable_content().cursor.shape,
                        CursorShape::Hidden
                    );
                    (view.terminal.at_prompt(), view.on_alt_screen(), hidden)
                })
                .unwrap();
            if state == (true, false, false) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let (at_prompt, on_alt, hidden) = state;
        assert!(at_prompt, "the host shell is back at its prompt");
        assert!(
            !on_alt,
            "the prompt report must pull the grid off the stranded alt screen"
        );
        assert!(
            !hidden,
            "the prompt report must re-show the DECTCEM-hidden cursor"
        );
    }
}

/// Title lifetime, the wheel report under the pointer, the IME anchor, and
/// which pane hears a drag. These drive a real `TerminalView` over a pane
/// link on every platform.
#[cfg(test)]
mod prompt_handover_tests {
    use super::*;
    use crate::daemon::protocol::{ClientMsg, DaemonMsg};
    use crate::terminal::remote::TestLink;
    use gpui::TestAppContext;

    fn harness(cx: &mut TestAppContext) -> (gpui::WindowHandle<TerminalView>, TestLink) {
        crate::core::config::pin_test_config_dir();
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
        });
        let (terminal, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::local(),
        );
        let window =
            cx.add_window(move |window, cx| TerminalView::with_terminal(terminal, 1, window, cx));
        (window, daemon_side)
    }

    /// Everything the pane has written to the PTY, in order.
    fn drain(daemon: &TestLink) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            match daemon.recv_client(std::time::Duration::from_millis(150)) {
                Ok(ClientMsg::Input(bytes)) => out.extend_from_slice(&bytes),
                Ok(_) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("pane link failed: disconnected")
                }
            }
        }
        out
    }

    fn settle(
        cx: &mut TestAppContext,
        window: &gpui::WindowHandle<TerminalView>,
        what: &str,
        f: impl Fn(&TerminalView) -> bool,
    ) {
        for _ in 0..300 {
            cx.run_until_parked();
            if window.update(cx, |view, _, _| f(view)).unwrap() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("never settled: {what}");
    }

    /// Bytes from the pane's pty, the way the daemon forwards them.
    fn output(daemon: &TestLink, bytes: &[u8]) {
        daemon.send(DaemonMsg::Output(bytes.to_vec()));
    }

    /// Waits for the tab's reading of what the pane calls itself to become
    /// `expect`, running out the wait a new title is held for on each pass.
    fn titled(
        cx: &mut TestAppContext,
        window: &gpui::WindowHandle<TerminalView>,
        expect: Option<&str>,
    ) {
        for _ in 0..300 {
            cx.run_until_parked();
            cx.executor().advance_clock(TITLE_SETTLE * 2);
            cx.run_until_parked();
            let showing = window
                .update(cx, |view, _, _| view.stated_title().map(str::to_string))
                .unwrap();
            if showing.as_deref() == expect {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the tab never came to read {expect:?}");
    }

    /// #889: a tab went on reading the name Claude Code had left on it long
    /// after Claude exited and the pane was back at its own prompt in a real
    /// directory. An OSC 0/2 had no end — only another OSC 0/2 replaced it —
    /// so the last title any program wrote in a pane outlived it forever.
    #[gpui::test]
    fn a_title_a_command_set_is_retired_when_that_command_finishes(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        output(&daemon, b"\x1b]133;A\x07\x1b]133;B\x07");
        output(
            &daemon,
            b"\x1b]133;C;claude\x07\x1b]2;\xe2\x9c\xb3 fixing the switcher\x1b\\",
        );
        titled(cx, &window, Some("✳ fixing the switcher"));

        // Claude exits and the shell reports the command finished. Nothing
        // titles the pane after it, so the tab has to fall back down the
        // label ladder — `stated_title` saying nothing is how it does that.
        output(&daemon, b"\x1b]133;D;0\x07");
        titled(cx, &window, None);
    }

    /// The two cases a title is *supposed* to outlive: a program that is still
    /// running, and a shell that titles its own prompt (which it does between
    /// the `D` and the `A`, so it is the last word rather than a thing undone).
    #[gpui::test]
    fn a_running_program_and_a_shells_own_prompt_title_both_keep_theirs(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        output(
            &daemon,
            b"\x1b]133;C;vim\x07\x1b]2;vim \xe2\x80\x94 main.rs\x1b\\",
        );
        titled(cx, &window, Some("vim — main.rs"));

        output(
            &daemon,
            b"\x1b]133;D;0\x07\x1b]0;me@box:~/dev\x07\x1b]133;A\x07\x1b]133;B\x07",
        );
        titled(cx, &window, Some("me@box:~/dev"));
        settle(cx, &window, "the prompt is reading", |view| {
            view.terminal.running_command().is_empty()
        });

        // And that prompt title is nobody's command to retire. Both marks are
        // chased by an edge the pane reports, so the assertion below cannot
        // pass on bytes that have not landed yet.
        output(&daemon, b"\x1b]133;C;ls\x07");
        settle(cx, &window, "a command takes the pane", |view| {
            view.terminal.running_command() == "ls"
        });
        output(&daemon, b"\x1b]133;D;0\x07\x1b]133;B\x07");
        settle(cx, &window, "the prompt comes back", |view| {
            view.terminal.running_command().is_empty()
        });
        titled(cx, &window, Some("me@box:~/dev"));
    }

    /// #1131: a wheel turned over one part of a TUI scrolled whichever part
    /// was last clicked. The wheel report carried the cell of the last button
    /// press, and only a click — or motion, for the few programs that ask for
    /// every move — ever updated it. tmux asks for button events only (1002),
    /// so after clicking its status line every wheel notch over the panes
    /// above was reported as being over the status line.
    #[gpui::test]
    fn a_wheel_report_names_the_cell_under_the_pointer_not_the_last_click(cx: &mut TestAppContext) {
        let (window, daemon) = harness(cx);
        output(&daemon, b"\x1b[?1002h\x1b[?1006h");
        settle(cx, &window, "the program asked for the mouse", |view| {
            view.mouse_mode()
        });

        window
            .update(cx, |view, w, cx| {
                view.cell_geom = Some(super::super::element::CellGeom {
                    origin: gpui::point(px(0.), px(0.)),
                    cell_width: px(10.),
                    line_height: px(20.),
                    cols: 80,
                    rows: 24,
                });
                // A click on the bottom row, where tmux keeps its window list.
                view.mouse_press(MouseButton::Left, 3, 23, &Modifiers::default());
                view.mouse_release(MouseButton::Left, 3, 23, &Modifiers::default());
                // Then one notch up, with the pointer up in the pane content.
                let ev = ScrollWheelEvent {
                    position: gpui::point(px(105.), px(110.)),
                    delta: gpui::ScrollDelta::Lines(gpui::point(0., 1.)),
                    ..Default::default()
                };
                view.on_scroll(&ev, w, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            String::from_utf8(drain(&daemon)).unwrap(),
            "\x1b[<0;4;24M\x1b[<0;4;24m\x1b[<64;11;6M",
            "the notch is reported at the cell the pointer is over"
        );
    }

    /// #1131: on Linux the candidate window only follows the caret if the
    /// rectangle is pushed, so every move has to be pushed — once — and a
    /// pane coming back into focus has to push again even if the caret never
    /// moved, because another pane's anchor was the last one sent.
    #[gpui::test]
    fn the_ime_anchor_is_pushed_when_it_moves_and_again_after_a_refocus(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        let cell = |col: f32, row: f32| {
            Some(gpui::Bounds::new(
                gpui::point(px(col * 10.), px(row * 20.)),
                gpui::size(px(10.), px(20.)),
            ))
        };
        window
            .update(cx, |view, _, _| {
                assert!(view.ime_anchor_moved(cell(2., 0.)), "the first frame");
                assert!(!view.ime_anchor_moved(cell(2., 0.)), "nothing moved");
                assert!(view.ime_anchor_moved(cell(9., 4.)), "the TUI moved it");
                assert!(!view.ime_anchor_moved(None), "focus left: nothing to push");
                assert!(
                    view.ime_anchor_moved(cell(9., 4.)),
                    "focus came back to a caret that never moved"
                );
            })
            .unwrap();
    }

    /// #1182: dragging anywhere in the window — the title bar or the pane
    /// next door — reached every pane whose program had asked for
    /// the mouse. The drag handler listens on the whole window and pinned the
    /// pointer to the nearest cell, so tmux (`mouse on`) saw a drag along its
    /// top row it never saw a press for, dropped into copy-mode and started
    /// selecting. A click anywhere likewise sent a release to every such pane.
    ///
    /// Only a press the pane itself reported starts a drag it reports; that
    /// drag follows the pointer out of the pane (clamped, as xterm does) and
    /// ends with the release, and nothing reaches the pane beside it.
    #[gpui::test]
    fn only_the_pane_a_press_landed_in_hears_the_drag(cx: &mut TestAppContext) {
        use gpui::{MouseMoveEvent, MouseUpEvent, PlatformInput};

        struct Split {
            a: gpui::Entity<TerminalView>,
            b: gpui::Entity<TerminalView>,
        }
        impl Render for Split {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .flex()
                    .flex_row()
                    // Stands in for chrome that is not a pane: no hitbox of either
                    // terminal covers it.
                    .child(div().w(px(200.)).h_full())
                    .child(div().flex_1().h_full().child(self.a.clone()))
                    .child(div().flex_1().h_full().child(self.b.clone()))
            }
        }

        crate::core::config::pin_test_config_dir();
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
        });
        let (a_terminal, a_pty) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::local(),
        );
        let (b_terminal, b_pty) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::local(),
        );
        let window = cx.add_window(move |window, cx| {
            let pane = |terminal, window: &mut Window, cx: &mut Context<Split>| {
                cx.new(|cx| TerminalView::with_terminal(terminal, 1, window, cx))
            };
            let a = pane(a_terminal, window, cx);
            let b = pane(b_terminal, window, cx);
            Split { a, b }
        });
        let (a, b) = window
            .update(cx, |split, _, _| (split.a.clone(), split.b.clone()))
            .unwrap();

        // Both panes run tmux with `mouse on`.
        output(&a_pty, b"\x1b[?1002h\x1b[?1006h");
        output(&b_pty, b"\x1b[?1002h\x1b[?1006h");
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        for _ in 0..300 {
            vcx.update(|window, _| window.refresh());
            vcx.run_until_parked();
            let ready = vcx.update(|_, cx| {
                [&a, &b].iter().all(|v| {
                    let v = v.read(cx);
                    v.mouse_mode() && v.cell_geom.is_some()
                })
            });
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let geom = |v: &gpui::Entity<TerminalView>, vcx: &mut gpui::VisualTestContext| {
            vcx.update(|_, cx| v.read(cx).cell_geom.expect("the pane was painted"))
        };
        let (ga, gb) = (geom(&a, &mut vcx), geom(&b, &mut vcx));
        assert!(
            ga.origin.x > px(200.) && gb.origin.x > ga.origin.x,
            "gutter, then A, then B: {:?} {:?}",
            ga.origin,
            gb.origin
        );
        // Cell (col, row) of a pane, as a window position in its middle.
        let at = |g: super::super::element::CellGeom, col: f32, row: f32| {
            gpui::point(
                g.origin.x + g.cell_width * (col + 0.5),
                g.origin.y + g.line_height * (row + 0.5),
            )
        };

        let hover = |vcx: &mut gpui::VisualTestContext, position| {
            vcx.update(|window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: None,
                        modifiers: Modifiers::none(),
                    }),
                    cx,
                );
            });
            vcx.run_until_parked();
        };
        let down = |vcx: &mut gpui::VisualTestContext, position| {
            vcx.update(|window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers: Modifiers::none(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
            });
            vcx.run_until_parked();
        };
        let drag = |vcx: &mut gpui::VisualTestContext, position| {
            vcx.update(|window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers::none(),
                    }),
                    cx,
                );
            });
            vcx.run_until_parked();
        };
        let up = |vcx: &mut gpui::VisualTestContext, position| {
            vcx.update(|window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers: Modifiers::none(),
                        click_count: 1,
                    }),
                    cx,
                );
            });
            vcx.run_until_parked();
        };
        let text = |pty: &TestLink| String::from_utf8(drain(pty)).unwrap();
        // Whatever the panes said while settling is not what this is about.
        let _ = (text(&a_pty), text(&b_pty));

        // 1. A drag across the gutter, the way one would drag a window by
        //    its title bar: neither pane saw the press, so neither hears it.
        let side = |y: f32| gpui::point(px(50.), px(y));
        hover(&mut vcx, side(100.));
        down(&mut vcx, side(100.));
        drag(&mut vcx, side(140.));
        drag(&mut vcx, side(180.));
        up(&mut vcx, side(180.));
        assert_eq!(
            (text(&a_pty), text(&b_pty)),
            (String::new(), String::new()),
            "a drag outside every pane is no pane's business"
        );

        // 2. A drag that starts in A and wanders into B: A hears all of it,
        //    pinned to its own edge once the pointer has left it, and B none.
        hover(&mut vcx, at(ga, 2., 3.));
        down(&mut vcx, at(ga, 2., 3.));
        drag(&mut vcx, at(ga, 4., 3.));
        drag(&mut vcx, at(gb, 1., 5.));
        up(&mut vcx, at(gb, 1., 5.));
        let last = ga.cols; // 1-based: the column A's grid ends on.
        assert_eq!(
            text(&a_pty),
            format!("\x1b[<0;3;4M\x1b[<32;5;4M\x1b[<32;{last};6M\x1b[<0;{last};6m"),
            "the pane the press landed in keeps the drag, clamped to its edge"
        );
        assert_eq!(
            text(&b_pty),
            "",
            "the pane beside it never saw the press, so it hears nothing"
        );
    }
}
