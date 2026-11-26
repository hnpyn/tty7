//! Messages a pane and its in-process subscriber exchange.
//!
//! There is no socket between them. The subscriber shares the pane's output
//! pipe, and these are the values on it. [`MAX_FRAME`] stays because kitty
//! graphics sizes an image against it.

use std::path::PathBuf;

/// Largest payload a subscriber will queue for one message. Kitty graphics
/// sizes its images against this, so a full-window frame still fits.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u16,
    pub cell_h: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSpec {
    pub program: String,
    pub args: Vec<String>,
    pub args_are_tty7_defaults: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    pub pane_id: u64,
    pub cwd: Option<PathBuf>,
    pub title: String,
    /// See [`crate::core::machine::PaneRecord::osc_title`] — the terminal's own
    /// title, not the foreground process name `title` carries.
    pub osc_title: Option<String>,
    pub alive: bool,
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteContext {
    pub kind: RemoteKind,
    pub argv: Vec<String>,
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    Ssh,
    Wsl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    Input(Vec<u8>),
    Resize(WinSize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonMsg {
    /// Echoed, under the pane lock, when a resize is applied. The subscriber
    /// reflows its grid at this frame so bytes queued at the old width are
    /// parsed before the geometry changes.
    Size(WinSize),
    Output(Vec<u8>),
    /// A kitty graphics image lifted out of the PTY stream. The payload is the
    /// compact binary frame, still compressed.
    Image(Vec<u8>),
    /// A kitty graphics delete (`a=d`) lifted out of the PTY stream.
    DeleteImage(Vec<u8>),
    /// A completed OSC 5522 clipboard write. The GUI decides whether it may
    /// touch the clipboard.
    ClipboardWrite(Vec<u8>),
    Cwd(PathBuf),
    Prompt {
        active: bool,
        at_prompt: bool,
        last_exit: Option<i32>,
    },
    Exited {
        code: Option<i32>,
    },
    RemoteContext(Option<RemoteContext>),
    Agent(Option<crate::core::cli_agent::CLIAgent>),
    AgentStatus(Option<crate::core::cli_agent::AgentSessionState>),
}
