use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, CursorShape, CursorStyle};

use crate::terminal::command_cursor::{CommandCursorStyle, CommandMark};
use crate::terminal::parked_cursor::{CursorCut, ParkedCursorRepair, ParkedCursorScanner};
use crate::terminal::prompt_reflow::{PromptBreak, PromptMark};

use std::collections::VecDeque;

use crate::core::cli_agent::{AgentSessionState, CLIAgent};
use crate::core::config::CursorStyle as ConfigCursorStyle;
use crate::core::osc::{Note, NoteBudget, OscTokenizer, TitleEffect, TitleLifetime};
use crate::daemon::protocol::{ClientMsg, DaemonMsg, RemoteContext, ShellSpec, WinSize};
use tty7_core::core::term_modes::{Query, TerminalModes};

use super::size::TermSize;

#[derive(Clone)]
pub struct EventProxy {
    tx: smol::channel::Sender<AlacEvent>,
    replaying: Arc<AtomicBool>,
    /// One `Wakeup` is queued until the view dequeues it, so a burst does not enqueue one per batch.
    wakeup_pending: Arc<AtomicBool>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacEvent) {
        // alacritty answers an unknown private mode with `CSI ? 2031 ; 0 $ y`.
        // The reader has already queued the answer for the mode at that byte,
        // so this copy must not reach the view or a test draining the channel.
        if matches!(&event, AlacEvent::PtyWrite(text) if text == "\x1b[?2031;0$y") {
            return;
        }
        if self.replaying.load(Ordering::Relaxed)
            && matches!(
                event,
                AlacEvent::PtyWrite(_)
                    | AlacEvent::ColorRequest(..)
                    | AlacEvent::TextAreaSizeRequest(_)
                    | AlacEvent::ClipboardStore(..)
                    | AlacEvent::ClipboardLoad(..)
                    | AlacEvent::Bell
            )
        {
            return;
        }
        // A second `Wakeup` waits until this one is dequeued, which is before the view reads the grid.
        if matches!(event, AlacEvent::Wakeup) && self.wakeup_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.tx.try_send(event);
    }
}

/// The view's end of [`EventProxy`]. Receiving a `Wakeup` clears the coalesce
/// flag, including when a test drains the channel with `try_recv`.
#[derive(Clone)]
pub struct EventRx {
    rx: smol::channel::Receiver<AlacEvent>,
    wakeup_pending: Arc<AtomicBool>,
}

impl EventRx {
    pub async fn recv(&self) -> Result<AlacEvent, smol::channel::RecvError> {
        let event = self.rx.recv().await?;
        self.observe(&event);
        Ok(event)
    }

    pub fn try_recv(&self) -> Result<AlacEvent, smol::channel::TryRecvError> {
        let event = self.rx.try_recv()?;
        self.observe(&event);
        Ok(event)
    }

    fn observe(&self, event: &AlacEvent) {
        if matches!(event, AlacEvent::Wakeup) {
            self.wakeup_pending.store(false, Ordering::Release);
        }
    }
}

#[derive(Default, Clone, Copy)]
struct ShellState {
    active: bool,
    at_prompt: bool,
    last_exit: Option<i32>,
    seq: u64,
    cycle: u64,
}

/// The pane's agent status as the client last heard it.
#[derive(Default)]
struct AgentSlot {
    state: Option<AgentSessionState>,
}

struct ReaderSignals {
    cwd: Arc<Mutex<Option<PathBuf>>>,
    shell: Arc<Mutex<ShellState>>,
    remote: Arc<Mutex<Option<RemoteContext>>>,
    agent: Arc<Mutex<Option<CLIAgent>>>,
    agent_session: Arc<Mutex<AgentSlot>>,
    exited: Arc<AtomicBool>,
    child_exited: Arc<AtomicBool>,
    zle_reading: Arc<AtomicBool>,
    shell_vi_mode: Arc<AtomicBool>,
    running_command: Arc<Mutex<String>>,
    /// Kitty-graphics images the daemon lifted out of the stream (issue #213),
    /// anchored to the grid for the paint path to blit. Shared with the reader,
    /// which places/deletes them as `DaemonMsg::Image`/`DeleteImage` frames land.
    images: crate::terminal::images::ImageStore,
    clipboard_writes: Arc<Mutex<VecDeque<tty7_core::core::clipboard::ClipboardWrite>>>,
    osc_notes: OscNotes,
    clipboard_write_busy: Arc<AtomicBool>,
    /// Whether this pane's pty is one a conhost renders into, and so whether
    /// the reader puts back the cursor a repaint parked. Decided per pane from
    /// its [`PtySource`], and shared rather than copied because the reader can
    /// learn better mid-stream — see the `RemoteContext` arm.
    local_conpty: Arc<AtomicBool>,
    /// Whether the program in the pane switched on DEC mode 2031.
    color_scheme_updates: Arc<AtomicBool>,
}

/// What kind of pty is at the far end of a pane's link, which is what decides
/// whether a conhost stands between the application and us.
///
/// The distinction is not the platform this client was built for. A Windows
/// client's panes are a mix: a local shell — or `wsl.exe`, or an `ssh` client,
/// which are ordinary programs inside the same ConPTY — is rendered by conhost,
/// while a pane on a remote `tty7-server` (Linux or macOS only, see
/// [`tty7_core::daemon::install::asset::asset_for_uname`]) or on a native-SSH
/// channel is a raw unix pty whose bytes reach us untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtySource {
    /// A pty this machine's Windows daemon opened: a ConPTY, with conhost
    /// painting frames into it.
    LocalConpty,
    /// A pty nothing repaints on our behalf — every pty on unix, and every pty
    /// reached over a link.
    Raw,
}

impl PtySource {
    /// The source a pane spawned or attached on `route` reads from. The
    /// platform enters here and nowhere else: a local route means a pty this
    /// machine opened, and only on Windows is that a ConPTY.
    ///
    /// `Unroutable` is a route that could not be resolved, so it never gets a
    /// pty at all; answering as if it were local costs nothing and keeps the
    /// match total.
    pub(super) fn for_route(route: &PaneRoute) -> PtySource {
        let _ = route;
        if cfg!(windows) {
            PtySource::LocalConpty
        } else {
            PtySource::Raw
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PaneRoute {
    #[default]
    Local,
}

impl PaneRoute {
    pub fn is_local(&self) -> bool {
        true
    }
}

/// How long a resize the daemon promised to echo is taken to still be on its
/// way. Past this, a grid that has not reached the requested geometry is not
/// waiting on anything: the request or its echo went missing, and the size is
/// sent again. The local daemon echoes in well under a millisecond and a
/// routed one within a round trip; a resend that beats a slow echo costs one
/// `TIOCSWINSZ` of the size the pty already has, which signals nobody.
const RESIZE_ECHO_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether `term`'s grid is `size`, as the emulator would size it — it never
/// goes below `MIN_COLUMNS` × `MIN_SCREEN_LINES`, so a sliver of a pane asking
/// for one column is already as narrow as it can get.
fn grid_holds(term: &Term<EventProxy>, size: TermSize) -> bool {
    use alacritty_terminal::grid::Dimensions as _;
    use alacritty_terminal::term::{MIN_COLUMNS, MIN_SCREEN_LINES};
    term.columns() == size.cols.max(MIN_COLUMNS)
        && term.screen_lines() == size.rows.max(MIN_SCREEN_LINES)
}

struct PaneHold {
    pane: Mutex<Option<Arc<tty7_core::daemon::pane::DaemonPane>>>,
}

fn live_panes() -> &'static Mutex<std::collections::HashMap<u64, Arc<PaneHold>>> {
    static LIVE: std::sync::OnceLock<Mutex<std::collections::HashMap<u64, Arc<PaneHold>>>> =
        std::sync::OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

static NEXT_PANE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn release_hold(hold: &PaneHold) {
    if let Some(pane) = hold.pane.lock().unwrap_or_else(|e| e.into_inner()).take() {
        pane.kill();
    }
}

/// Kill every live pane and drop it on this thread, so per-pane history is
/// merged back before the process exits.
pub fn shutdown_panes() {
    let holds: Vec<_> = live_panes()
        .lock()
        .unwrap()
        .drain()
        .map(|(_, hold)| hold)
        .collect();
    for hold in holds {
        release_hold(&hold);
    }
}

enum Downlink {
    /// The pane lives in this process. Input and resize go straight to it.
    Pane(Arc<PaneHold>),
    /// Tests: keystrokes and resizes land in a channel the fixture reads.
    #[cfg(test)]
    TestSink(std::sync::mpsc::Sender<ClientMsg>),
}

impl Downlink {
    fn send(&self, msg: ClientMsg) {
        match self {
            Downlink::Pane(hold) => {
                let Some(pane) = hold.pane.lock().ok().and_then(|guard| guard.clone()) else {
                    return;
                };
                match msg {
                    ClientMsg::Input(bytes) => pane.write_input(&bytes),
                    ClientMsg::Resize(size) => pane.resize(size),
                }
            }
            #[cfg(test)]
            Downlink::TestSink(tx) => {
                let _ = tx.send(msg);
            }
        }
    }

    fn pane_hold(&self) -> Option<&Arc<PaneHold>> {
        match self {
            Downlink::Pane(hold) => Some(hold),
            #[cfg(test)]
            Downlink::TestSink(_) => None,
        }
    }
}

/// The pane reader's inbox: the coalescing pipe shared with the producer.
type Inbox = tty7_core::daemon::pane::OutputRx;

/// Bytes the output gate charged for this frame, including output appended into one buffer.
fn queued_gate_len(msg: &DaemonMsg) -> usize {
    match msg {
        DaemonMsg::Output(bytes) | DaemonMsg::Image(bytes) | DaemonMsg::ClipboardWrite(bytes) => {
            bytes.len()
        }
        _ => 0,
    }
}

/// Bidirectional stand-in for the pane link. `send` feeds the same coalescing
/// pipe the live reader drains; `recv_client` is what the pane would have read.
#[cfg(test)]
pub(crate) struct TestLink {
    tx: tty7_core::daemon::pane::OutputTx,
    client_rx: std::sync::mpsc::Receiver<ClientMsg>,
}

#[cfg(test)]
impl TestLink {
    /// Push one daemon message. `Output` goes through the pipe's append path, so
    /// consecutive output coalesces the way a live pane's pipe does.
    pub(crate) fn send(&self, msg: DaemonMsg) {
        let _ = self.tx.push(msg);
    }

    pub(crate) fn recv_client(
        &self,
        timeout: std::time::Duration,
    ) -> Result<ClientMsg, std::sync::mpsc::RecvTimeoutError> {
        self.client_rx.recv_timeout(timeout)
    }

    pub(crate) fn try_recv_client(&self) -> Result<ClientMsg, std::sync::mpsc::TryRecvError> {
        self.client_rx.try_recv()
    }

    /// Drop the output sender so the reader sees the pipe close.
    pub(crate) fn close(self) {
        drop(self.tx);
    }
}

pub struct RemoteTerminal {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub events: EventRx,
    pub palette: [alacritty_terminal::vte::ansi::Rgb; 256],
    pub exited: bool,
    size: TermSize,
    synced_size: bool,
    /// The `(cell_w, cell_h)` last sent to the daemon, in device pixels. Tracked
    /// alongside `size` so a display-scale change still reaches the child even
    /// when the grid dimensions are unchanged.
    synced_cell: (u16, u16),
    /// When the last `ClientMsg::Resize` went down the link. On an echoing
    /// route it is what tells "the echo is still in flight" apart from "the
    /// echo is never coming" — see [`RESIZE_ECHO_GRACE`].
    resize_sent_at: Option<std::time::Instant>,
    link: Downlink,
    cwd: Arc<Mutex<Option<PathBuf>>>,
    shell_state: Arc<Mutex<ShellState>>,
    remote_context: Arc<Mutex<Option<RemoteContext>>>,
    exited_flag: Arc<AtomicBool>,
    child_exited: Arc<AtomicBool>,
    zle_reading: Arc<AtomicBool>,
    shell_vi_mode: Arc<AtomicBool>,
    /// The line the shell said it is running, from the OSC 133;C mark, still
    /// percent-escaped as the integration sent it. Empty while at a prompt.
    ///
    /// This is the only place the *text* of a running command exists on the
    /// client: the frame the daemon sends says a command runs, not which one.
    /// The close confirmation has to name what it is about to end.
    running_command: Arc<Mutex<String>>,
    agent: Arc<Mutex<Option<CLIAgent>>>,
    agent_session: Arc<Mutex<AgentSlot>>,
    /// Kitty-graphics images placed on this pane's grid (issue #213).
    /// Written by the reader thread from out-of-band `Image`/`DeleteImage`
    /// frames, read by the paint path — only the client holds the grid the
    /// anchors are relative to, so the store lives here rather than in the daemon.
    images: crate::terminal::images::ImageStore,
    clipboard_writes: Arc<Mutex<VecDeque<tty7_core::core::clipboard::ClipboardWrite>>>,
    osc_notes: OscNotes,
    /// On the pane rather than its view so it survives a relink; only the
    /// view's poll touches it.
    note_budget: Mutex<NoteBudget>,
    clipboard_write_busy: Arc<AtomicBool>,
    route: PaneRoute,
    /// Kept so the event channel stays open for the life of the pane.
    #[allow(dead_code)]
    proxy: EventProxy,
    reader_thread: Option<JoinHandle<()>>,
    /// Tells the current reader thread to exit. Detach does not join it: the
    /// reader blocks on the output pipe, and waiting on a silent pipe would
    /// stall the UI thread. The reader checks this flag before it mutates the
    /// grid, so once it is set the abandoned thread only exits.
    reader_quit: Arc<AtomicBool>,
    /// Whether this pane's pty is a ConPTY, for input encoding and repairing
    /// the cursor a repaint parks. Held here so a relink hands the same answer
    /// to the reader it starts: a pane's pty does not change kind when the link
    /// to it is rebuilt, and the route a relink carries cannot tell a
    /// native-SSH pane from a local shell.
    local_conpty: Arc<AtomicBool>,
    /// Whether the program in the pane switched on DEC mode 2031.
    color_scheme_updates: Arc<AtomicBool>,
    /// This pane's child is a `DaemonPane` in this process, so a resize is
    /// applied here and echoed back as `DaemonMsg::Size`.
    direct: bool,
    pane_id: Option<u64>,
}

/// The workspace id a spawn carries, so the pane's shell gets `$NTTY7_WS` and a
/// CLI running in it can use workspace-scoped verbs with no argument.
///
/// This is the same id as `owner`, but decided separately: `owner` is also
/// gated on FEATURE_PANE_OWNER, while the workspace field rides the owned spawn
/// kind and needs no feature probe. Local routes only — a remote server keeps
/// its own machine tree, and this id names a workspace in ours.
fn spawn_workspace(owner: Option<&str>, route: &PaneRoute) -> Option<String> {
    owner.filter(|_| route.is_local()).map(|id| id.to_string())
}

impl RemoteTerminal {
    #[cfg(test)]
    pub fn spawn(
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        shell: Option<ShellSpec>,
    ) -> anyhow::Result<(Self, u64)> {
        Self::spawn_on(
            &PaneRoute::Local,
            size,
            cell_w,
            cell_h,
            cwd,
            shell,
            None,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_on(
        route: &PaneRoute,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        shell: Option<ShellSpec>,
        owner: Option<String>,
        use_config_shell: bool,
    ) -> anyhow::Result<(Self, u64)> {
        Self::spawn_once(
            route,
            size,
            cell_w,
            cell_h,
            cwd,
            shell,
            owner,
            use_config_shell,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_once(
        route: &PaneRoute,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        shell: Option<ShellSpec>,
        owner: Option<String>,
        use_config_shell: bool,
    ) -> anyhow::Result<(Self, u64)> {
        let id = NEXT_PANE.fetch_add(1, Ordering::Relaxed);
        let win = win_size(size, cell_w, cell_h);
        let workspace = spawn_workspace(owner.as_deref(), route);
        let (tx, inbox) = tty7_core::daemon::pane::output_pipe();
        // The subscriber is installed before the reader starts, so the first
        // output byte is delivered live. There is no replay buffer behind it.
        let pane = tty7_core::daemon::pane::DaemonPane::spawn_subscribed(
            id,
            cwd.clone(),
            win,
            shell,
            owner,
            workspace,
            false,
            use_config_shell,
            tty7_core::daemon::pane::PaneLink::coalesced(tx),
            || {},
        )?;
        let drain = pane.gate();
        let hold = Arc::new(PaneHold {
            pane: Mutex::new(Some(pane)),
        });
        live_panes()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Arc::clone(&hold));
        let mut term = Self::assemble(
            inbox,
            Some(drain),
            size,
            PtySource::for_route(route),
            Downlink::Pane(hold),
        );
        term.direct = true;
        term.pane_id = Some(id);
        term.route = route.clone();
        term.seed_cwd(cwd);
        Ok((term, id))
    }

    /// Remember the directory the daemon was asked to spawn in, so everything
    /// that keys off a pane's cwd — the sidebar's repo grouping above all —
    /// has an answer before the shell gets far enough to report its own via
    /// OSC 7. The reader thread is already running, so a report that beat us
    /// here wins: it describes where the shell actually landed, which is not
    /// always where we asked (a missing directory sends the daemon home, an
    /// rc file may `cd` on its own).
    pub(crate) fn seed_cwd(&self, cwd: Option<PathBuf>) {
        let Some(cwd) = cwd else { return };
        if let Ok(mut guard) = self.cwd.lock() {
            guard.get_or_insert(cwd);
        }
    }

    /// A terminal whose pane link is an in-memory pipe. `direct` stays false:
    /// a test that wants resize-echo semantics sets it.
    #[cfg(test)]
    pub(crate) fn from_pipe(size: TermSize, pty: PtySource) -> (Self, TestLink) {
        let (tx, rx) = tty7_core::daemon::pane::output_pipe();
        let (client_tx, client_rx) = std::sync::mpsc::channel();
        let term = Self::assemble(rx, None, size, pty, Downlink::TestSink(client_tx));
        (term, TestLink { tx, client_rx })
    }

    fn assemble(
        inbox: Inbox,
        drain: Option<Arc<tty7_core::daemon::pane::OutputGate>>,
        size: TermSize,
        pty: PtySource,
        link: Downlink,
    ) -> Self {
        let (tx, rx) = smol::channel::unbounded();
        let wakeup_pending = Arc::new(AtomicBool::new(false));
        let proxy = EventProxy {
            tx,
            replaying: Arc::new(AtomicBool::new(false)),
            wakeup_pending: Arc::clone(&wakeup_pending),
        };
        let events = EventRx { rx, wakeup_pending };

        let user_config = crate::core::config::Config::load();
        let config = terminal_config_from_user(&user_config);
        let term = Term::new(config, &size, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let cwd: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let shell_state: Arc<Mutex<ShellState>> = Arc::new(Mutex::new(ShellState::default()));
        let remote_context: Arc<Mutex<Option<RemoteContext>>> = Arc::new(Mutex::new(None));
        let agent: Arc<Mutex<Option<CLIAgent>>> = Arc::new(Mutex::new(None));
        let agent_session: Arc<Mutex<AgentSlot>> = Arc::new(Mutex::new(AgentSlot::default()));
        let exited_flag = Arc::new(AtomicBool::new(false));
        let child_exited = Arc::new(AtomicBool::new(false));
        let zle_reading = Arc::new(AtomicBool::new(false));
        let shell_vi_mode = Arc::new(AtomicBool::new(false));
        let running_command: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let images = crate::terminal::images::ImageStore::new();
        let clipboard_writes = Arc::new(Mutex::new(VecDeque::new()));
        let osc_notes = OscNotes::default();
        let clipboard_write_busy = Arc::new(AtomicBool::new(false));

        let reader_quit = Arc::new(AtomicBool::new(false));
        let local_conpty = Arc::new(AtomicBool::new(pty == PtySource::LocalConpty));
        let color_scheme_updates = Arc::new(AtomicBool::new(false));
        let reader_thread = Self::spawn_reader(
            term.clone(),
            proxy.clone(),
            inbox,
            drain,
            reader_quit.clone(),
            ReaderSignals {
                cwd: cwd.clone(),
                shell: shell_state.clone(),
                remote: remote_context.clone(),
                agent: agent.clone(),
                agent_session: agent_session.clone(),
                exited: exited_flag.clone(),
                child_exited: child_exited.clone(),
                zle_reading: zle_reading.clone(),
                shell_vi_mode: shell_vi_mode.clone(),
                running_command: running_command.clone(),
                images: images.clone(),
                clipboard_writes: clipboard_writes.clone(),
                osc_notes: osc_notes.clone(),
                clipboard_write_busy: clipboard_write_busy.clone(),
                local_conpty: local_conpty.clone(),
                color_scheme_updates: color_scheme_updates.clone(),
            },
        );

        Self {
            term,
            events,
            palette: super::palette::build(),
            exited: false,
            size,
            synced_size: false,
            synced_cell: (0, 0),
            resize_sent_at: None,
            link,
            cwd,
            shell_state,
            remote_context,
            exited_flag,
            child_exited,
            zle_reading,
            shell_vi_mode,
            running_command,
            agent,
            agent_session,
            images,
            clipboard_writes,
            osc_notes,
            note_budget: Mutex::default(),
            clipboard_write_busy,
            route: PaneRoute::Local,
            proxy,
            reader_thread: Some(reader_thread),
            reader_quit,
            local_conpty,
            color_scheme_updates,
            direct: false,
            pane_id: None,
        }
    }

    pub fn detach_link(&mut self) {
        self.stop_reader();
        self.poll_exited();
    }

    /// Retires the current reader thread without waiting for it. Joining is
    /// not an option here: the callers run on the UI thread, and the reader
    /// is parked on the output pipe. It is told to quit and abandoned; the
    /// next poll sees the flag and the thread exits without touching the grid.
    fn stop_reader(&mut self) {
        self.reader_quit.store(true, Ordering::SeqCst);
        drop(self.reader_thread.take());
    }

    pub fn apply_user_config(&self, user_config: &crate::core::config::Config) {
        let mut term = self.term.lock();
        term.set_options(terminal_config_from_user(user_config));
    }

    fn spawn_reader(
        term: Arc<FairMutex<Term<EventProxy>>>,
        proxy: EventProxy,
        inbox: Inbox,
        drain: Option<Arc<tty7_core::daemon::pane::OutputGate>>,
        quit: Arc<AtomicBool>,
        signals: ReaderSignals,
    ) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("tty7-remote-reader".to_string())
            .spawn(move || {
                let ReaderSignals {
                    cwd,
                    shell,
                    remote,
                    agent,
                    agent_session,
                    exited: exited_flag,
                    child_exited,
                    zle_reading,
                    shell_vi_mode,
                    running_command,
                    images,
                    clipboard_writes,
                    osc_notes,
                    clipboard_write_busy,
                    local_conpty,
                    color_scheme_updates,
                } = signals;
                crate::core::threads::promote_to_user_interactive();
                let mut processor: ansi::Processor = ansi::Processor::new();
                let mut osc = OscNotifyScanner::default();
                let mut mode_tok = OscTokenizer::new(&[b"133"]);
                let mut zle_tok = OscTokenizer::new(&[b"133"]);
                // #889: an OSC 0/2 the emulator above has already adopted, read
                // a second time only to learn whether the program that wrote it
                // has since exited. `TitleLifetime` is the daemon's rule too,
                // so a window's tab strip and the switcher reading the tree can
                // never disagree about whether a title is still current.
                let mut title_tok = OscTokenizer::new(&[b"0", b"2", b"133"]);
                let mut title_life = TitleLifetime::default();
                let mut cursor_scan = ParkedCursorScanner::new();
                let mut parked_cursor = ParkedCursorRepair::default();
                // #837: the command marks, cut at exactly where they land so
                // the cursor style a finished command left behind is judged
                // against the bytes before its `D`, not the whole batch.
                let mut command_tok = OscTokenizer::new(&[b"133"]);
                let mut command_cursor = CommandCursorStyle::default();
                let mut prompt_break = PromptBreak::default();
                // A new link replays the pane from scratch, 2031 included
                // (`term_modes` restores it ahead of the ring).
                let mut modes = TerminalModes::new();
                color_scheme_updates.store(false, Ordering::Relaxed);
                let mut queued: VecDeque<DaemonMsg> = VecDeque::new();
                // Kitty-graphics decode runs on its own thread with newest-frame
                // coalescing (issue #213): inflating a full-window browser frame
                // is ~42 ms, and doing it inline here would block PTY output and
                // scrolling for that long every frame. The worker owns the inflate
                // + BGRA swap + placement; the reader only captures the grid
                // anchor and hands off the still-compressed frame. Dropped when
                // the loop ends, which joins the thread.
                let image_decoder = {
                    let proxy = proxy.clone();
                    crate::terminal::images::DecodeWorker::spawn(images.clone(), move || {
                        proxy.send_event(AlacEvent::Wakeup);
                    })
                };

                let trace = std::env::var("NTTY7_TRACE").is_ok_and(|v| !v.is_empty() && v != "0");
                let mut tr_last = std::time::Instant::now();
                let mut tr_bytes: u64 = 0;
                let mut tr_reads: u32 = 0;
                let mut tr_read_t = std::time::Duration::ZERO;
                let mut tr_lock_t = std::time::Duration::ZERO;
                let mut tr_adv_t = std::time::Duration::ZERO;
                let mut tr_frames: u32 = 0;

                let teardown = |reason: Option<&str>| {
                    // A retired reader exits silently: the pane is being
                    // relinked or released, not dying — marking it exited
                    // would wrongly kill the freshly adopted link.
                    if quit.load(Ordering::SeqCst) {
                        return;
                    }
                    // Said out loud because every way a link dies looks the
                    // same from the window — a pane suddenly "disconnected" —
                    // and this line is the only record of which way it was.
                    if let Some(reason) = reason {
                        log::warn!("a pane's link died: {reason}");
                    }
                    term.lock().exit();
                    exited_flag.store(true, Ordering::SeqCst);
                    proxy.send_event(AlacEvent::Wakeup);
                    proxy.send_event(AlacEvent::Exit);
                };

                let mut out_batch: Vec<u8> = Vec::with_capacity(256 * 1024);

                // Whether this chunk ends with the title the pane is showing
                // belonging to a command that has finished. The emulator has
                // already parsed the same bytes, so a `true` here is sent on
                // as a `ResetTitle` *after* every `Title` the chunk produced —
                // which is the whole ordering question: a shell that re-titles
                // itself in `precmd` writes its OSC 0/2 after the `D`, and
                // that title is the last word rather than a thing to undo.
                macro_rules! chunk_retires_the_title {
                    ($bytes:expr) => {{
                        let mut retire = false;
                        title_tok.feed($bytes, |payload| match title_life.saw(payload) {
                            TitleEffect::Retire => retire = true,
                            TitleEffect::Set => retire = false,
                            TitleEffect::None => {}
                        });
                        retire
                    }};
                }

                'main: loop {
                    macro_rules! flush_batch {
                        () => {
                            if !out_batch.is_empty() {
                                // One scan. An idle machine waits on `0x1b`; one left mid-sequence still has to see the next chunk.
                                let has_esc = crate::core::osc::contains_esc(&out_batch);
                                // End offsets of the queries in this batch. The
                                // replies go out from `feed_grid` at those
                                // offsets, after the emulator has parsed the
                                // query, so they stay in stream order with DA1,
                                // DA2 and every other `PtyWrite`.
                                let mut query_cuts: Vec<(usize, Query)> = Vec::new();
                                if has_esc || !modes.is_ground() {
                                    super::color_scheme::fold_reporting(
                                        &mut modes,
                                        &out_batch,
                                        &color_scheme_updates,
                                        |off, query| query_cuts.push((off, query)),
                                    );
                                    // Recorded for the offline daemon path. The
                                    // cuts above are the online answers.
                                    modes.take_queries();
                                }
                                // The scanner reports an offset one past each
                                // sequence it matched, in ascending order, so
                                // the batch splits at each of them: advance the
                                // emulator to the cut, act on the state that
                                // sequence left behind, carry on.
                                let mut cuts: Vec<(usize, ReaderCut)> = Vec::new();
                                if local_conpty.load(Ordering::Relaxed) {
                                    if has_esc || !cursor_scan.is_ground() {
                                        cursor_scan.feed(&out_batch, |off, c| {
                                            cuts.push((off, ReaderCut::Parked(c)))
                                        });
                                    } else {
                                        cursor_scan.note_ground_text(&out_batch);
                                    }
                                }
                                if has_esc || !command_tok.is_ground() {
                                    command_cuts(&mut command_tok, &out_batch, &mut cuts);
                                }
                                for (off, query) in query_cuts {
                                    if matches!(
                                        query,
                                        Query::ColorScheme | Query::ColorSchemeMode { .. }
                                    ) {
                                        cuts.push((off, ReaderCut::Reply(query)));
                                    }
                                }
                                if cuts.len() > 1 {
                                    cuts.sort_by_key(|(off, _)| *off);
                                }
                                {
                                    let t0 = trace.then(std::time::Instant::now);
                                    let Some(waited) = feed_grid(
                                        &term,
                                        &mut processor,
                                        &out_batch,
                                        cuts,
                                        &quit,
                                        |term, cut| match cut {
                                            ReaderCut::Parked(cut) => {
                                                parked_cursor.apply(term, cut)
                                            }
                                            ReaderCut::Command(mark) => {
                                                command_cursor.apply(term, mark)
                                            }
                                            ReaderCut::Prompt(mark) => {
                                                prompt_break.apply(term, mark)
                                            }
                                            ReaderCut::Reply(query) => {
                                                answer_stream_query(&proxy, query)
                                            }
                                        },
                                    ) else {
                                        return;
                                    };
                                    if let Some(t0) = t0 {
                                        tr_lock_t += waited;
                                        tr_adv_t += t0.elapsed() - waited;
                                    }
                                }
                                if has_esc || !osc.is_ground() {
                                    if let Ok(mut notes) = osc_notes.lock() {
                                        osc.feed(&out_batch, &mut *notes);
                                    }
                                }
                                if has_esc || !mode_tok.is_ground() {
                                    mode_tok.feed(&out_batch, |payload| {
                                        if let Some(mode) = payload.strip_prefix(b"133;V;") {
                                            shell_vi_mode.store(
                                                mode.first() == Some(&b'1'),
                                                Ordering::Relaxed,
                                            );
                                        }
                                    });
                                }
                                if has_esc || !zle_tok.is_ground() {
                                    zle_tok.feed(&out_batch, |payload| {
                                        if let Some(mark) = payload.strip_prefix(b"133;") {
                                            match mark.first() {
                                                Some(b'B') => {
                                                    zle_reading.store(true, Ordering::Relaxed);
                                                    // Back at a prompt: whatever
                                                    // ran is over, so the name of
                                                    // it must not outlive it into
                                                    // the next close question.
                                                    if let Ok(mut cmd) = running_command.lock() {
                                                        cmd.clear();
                                                    }
                                                }
                                                Some(b'V') => {
                                                    shell_vi_mode.store(
                                                        mark.strip_prefix(b"V;")
                                                            .is_some_and(|v| {
                                                                v.first() == Some(&b'1')
                                                            }),
                                                        Ordering::Relaxed,
                                                    );
                                                }
                                                Some(b'C') => {
                                                    zle_reading.store(false, Ordering::Relaxed);
                                                    // `C` alone is a command start
                                                    // with no line to report — the
                                                    // PowerShell path sends that —
                                                    // and leaves the field empty
                                                    // rather than holding a stale
                                                    // one.
                                                    let line = mark
                                                        .strip_prefix(b"C;")
                                                        .map(|c| {
                                                            String::from_utf8_lossy(c).into_owned()
                                                        })
                                                        .unwrap_or_default();
                                                    if let Ok(mut cmd) = running_command.lock() {
                                                        *cmd = line;
                                                    }
                                                }
                                                _ => zle_reading.store(false, Ordering::Relaxed),
                                            }
                                        }
                                    });
                                }
                                if (has_esc || !title_tok.is_ground())
                                    && chunk_retires_the_title!(&out_batch)
                                {
                                    proxy.send_event(AlacEvent::ResetTitle);
                                }
                                proxy.send_event(AlacEvent::Wakeup);
                                out_batch.clear();
                            }
                        };
                    }

                    loop {
                        // Checked per frame, not just per read: a retired
                        // reader may still hold complete frames in `pending`,
                        // and every arm below writes shared state that
                        // outlives a relink (cwd, prompt, agent — and
                        // `Exited`, which would close the freshly adopted
                        // pane via `child_exited`).
                        if quit.load(Ordering::SeqCst) {
                            return;
                        }
                        let Some(msg) = queued.pop_front() else {
                            break;
                        };
                        match msg {
                            // Geometry, applied at this exact stream position.
                            // During replay each ring segment is preceded by
                            // its Size; live, the daemon echoes one when it
                            // applies our Resize (FEATURE_RESIZE_ECHO), which
                            // is the point in the stream where the bytes stop
                            // being old-width — everything still queued before
                            // this frame must be parsed into the old grid.
                            DaemonMsg::Size(ws) => {
                                flush_batch!();
                                // A live echo lands just before the shell's
                                // SIGWINCH redraw.
                                let shell_redraws = !local_conpty.load(Ordering::Relaxed)
                                    && shell.lock().is_ok_and(|s| s.active && s.at_prompt);
                                {
                                    let mut term = term.lock();
                                    if quit.load(Ordering::SeqCst) {
                                        return;
                                    }
                                    super::prompt_reflow::resize(
                                        &mut term,
                                        TermSize::new(ws.cols as usize, ws.rows as usize),
                                        shell_redraws,
                                    );
                                }
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::Output(bytes) => {
                                out_batch.extend_from_slice(&bytes);
                                inbox.recycle(bytes);
                                tr_frames += 1;
                            }
                            // Kitty graphics (issue #213): the daemon lifted an
                            // image out of the stream and forwarded it out-of-band,
                            // interleaved *in stream order* with the Output frames
                            // around it. Flush the pending text first so the grid
                            // cursor sits where the sender drew the image, then
                            // anchor the placement to that cell in scroll-stable
                            // absolute-row coordinates (`history_size -
                            // display_offset + cursor_line`), so it tracks
                            // scrolling.
                            DaemonMsg::Image(frame) => {
                                flush_batch!();
                                if let Some(img) =
                                    tty7_core::core::kitty_graphics::Image::decode_frame_owned(
                                        frame,
                                    )
                                {
                                    // Capture the anchor *now*, at the cursor cell
                                    // the transmission arrived on; the decode is
                                    // deferred to the worker thread but must land
                                    // at this position, not wherever the cursor has
                                    // scrolled to by the time inflate finishes.
                                    let (anchor_row, anchor_col) = {
                                        use alacritty_terminal::grid::Dimensions as _;
                                        let term = term.lock();
                                        if quit.load(Ordering::SeqCst) {
                                            return;
                                        }
                                        let grid = term.grid();
                                        let row = grid.history_size() as i64
                                            - grid.display_offset() as i64
                                            + i64::from(grid.cursor.point.line.0);
                                        (row, grid.cursor.point.column.0)
                                    };
                                    // Hand off without blocking the reader: the
                                    // worker inflates, swaps, places, and wakes the
                                    // view. Stale frames coalesce away there.
                                    image_decoder.submit(
                                        crate::terminal::images::PendingFrame {
                                            img,
                                            anchor_row,
                                            anchor_col,
                                        },
                                    );
                                }
                            }
                            // An `a=d` delete, lifted out the same way. Order with
                            // the surrounding output does not matter for a delete
                            // (it targets by id/placement, not cursor position),
                            // but flushing keeps a delete-then-retransmit in the
                            // same read from racing its own replacement.
                            DaemonMsg::DeleteImage(sel) => {
                                flush_batch!();
                                if let Some(del) =
                                    tty7_core::core::kitty_graphics::ImageDelete::decode(&sel)
                                {
                                    image_decoder.delete(&del);
                                    proxy.send_event(AlacEvent::Wakeup);
                                }
                            }
                            DaemonMsg::ClipboardWrite(frame) => {
                                flush_batch!();
                                if let Some(write) =
                                    tty7_core::core::clipboard::ClipboardWrite::decode_frame(frame)
                                {
                                    if clipboard_write_busy
                                        .compare_exchange(
                                            false,
                                            true,
                                            Ordering::AcqRel,
                                            Ordering::Acquire,
                                        )
                                        .is_ok()
                                    {
                                        if let Ok(mut pending) = clipboard_writes.lock() {
                                            pending.push_back(write);
                                            proxy.send_event(AlacEvent::Wakeup);
                                        } else {
                                            clipboard_write_busy.store(false, Ordering::Release);
                                        }
                                    } else {
                                        let reply = tty7_core::core::clipboard::response(
                                            write.id.as_deref(),
                                            "EBUSY",
                                        );
                                        proxy.send_event(AlacEvent::PtyWrite(
                                            String::from_utf8(reply)
                                                .expect("OSC 5522 replies are ASCII"),
                                        ));
                                    }
                                } else {
                                    clipboard_write_busy.store(false, Ordering::Release);
                                }
                            }
                            DaemonMsg::Cwd(path) => {
                                flush_batch!();
                                if let Ok(mut guard) = cwd.lock() {
                                    *guard = Some(path);
                                }
                            }
                            DaemonMsg::Prompt {
                                active,
                                at_prompt,
                                last_exit,
                            } => {
                                flush_batch!();
                                if let Ok(mut guard) = shell.lock() {
                                    *guard = ShellState {
                                        active,
                                        at_prompt,
                                        last_exit,
                                        seq: guard.seq + 1,
                                        cycle: guard.cycle
                                            + u64::from(at_prompt && !guard.at_prompt),
                                    };
                                }
                                if active && at_prompt {
                                    let mut term = term.lock();
                                    if quit.load(Ordering::SeqCst) {
                                        return;
                                    }
                                    let resets = stale_mode_resets(*term.mode());
                                    if !resets.is_empty() {
                                        processor.advance(&mut *term, &resets);
                                    }
                                    // Nothing in front of the shell could have
                                    // negotiated kitty keyboard flags, so they
                                    // all go — both screens' stacks, not just
                                    // the flags in effect. `CSI = 0 u` cannot
                                    // do that: a program killed on the
                                    // alternate screen left its push on that
                                    // screen's stack, and the next program to
                                    // enter it got the flags back and its
                                    // Escape as `CSI 27 u` — which a ConPTY
                                    // reader sees as no key at all (#1155).
                                    term.reset_keyboard_modes();
                                    drop(term);
                                    if !resets.is_empty() {
                                        proxy.send_event(AlacEvent::Wakeup);
                                    }
                                }
                            }
                            DaemonMsg::RemoteContext(ctx) => {
                                flush_batch!();
                                // The directory belonged to the previous side of the hop.
                                if let Ok(mut guard) = cwd.lock() {
                                    *guard = None;
                                }
                                if let Ok(mut guard) = remote.lock() {
                                    *guard = ctx;
                                }
                            }
                            DaemonMsg::Agent(a) => {
                                flush_batch!();
                                if let Ok(mut guard) = agent.lock() {
                                    *guard = a;
                                }
                            }
                            DaemonMsg::AgentStatus(state) => {
                                flush_batch!();
                                if let Ok(mut guard) = agent_session.lock() {
                                    guard.state = state;
                                }
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::Exited { .. } => {
                                flush_batch!();
                                child_exited.store(true, Ordering::SeqCst);
                                // Routine — the child ended — so no log line.
                                teardown(None);
                                break 'main;
                            }
                        }
                    }
                    flush_batch!();

                    // The read always times out within QUIT_POLL so a reader
                    // parked on a silent pipe still notices `quit` promptly.
                    // Detach never joins this thread.
                    const QUIT_POLL: std::time::Duration = std::time::Duration::from_millis(500);
                    let timeout = match processor.sync_timeout().sync_timeout() {
                        Some(deadline) => {
                            let left =
                                deadline.saturating_duration_since(std::time::Instant::now());
                            if left.is_zero() {
                                let mut term = term.lock();
                                if quit.load(Ordering::SeqCst) {
                                    return;
                                }
                                processor.stop_sync(&mut *term);
                                drop(term);
                                proxy.send_event(AlacEvent::Wakeup);
                                continue;
                            }
                            left.min(QUIT_POLL)
                        }
                        None => QUIT_POLL,
                    };
                    if trace && tr_last.elapsed() >= std::time::Duration::from_secs(1) {
                        eprintln!(
                            "[trace client] {:.1} MB/s | {} reads ({} B/read) {} frames | read wait {:?} lock wait {:?} advance {:?}",
                            tr_bytes as f64 / tr_last.elapsed().as_secs_f64() / 1e6,
                            tr_reads,
                            if tr_reads > 0 { tr_bytes / tr_reads as u64 } else { 0 },
                            tr_frames,
                            tr_read_t,
                            tr_lock_t,
                            tr_adv_t,
                        );
                        tr_last = std::time::Instant::now();
                        tr_bytes = 0;
                        tr_reads = 0;
                        tr_frames = 0;
                        tr_read_t = std::time::Duration::ZERO;
                        tr_lock_t = std::time::Duration::ZERO;
                        tr_adv_t = std::time::Duration::ZERO;
                    }
                    let tr0 = trace.then(std::time::Instant::now);
                    match inbox.recv_timeout(timeout) {
                        Ok(msg) => {
                            if let Some(tr0) = tr0 {
                                tr_read_t += tr0.elapsed();
                                tr_reads += 1;
                                if let DaemonMsg::Output(bytes) = &msg {
                                    tr_bytes += bytes.len() as u64;
                                }
                            }
                            let mut gated = queued_gate_len(&msg);
                            queued.push_back(msg);
                            while let Ok(more) = inbox.try_recv() {
                                if tr0.is_some()
                                    && let DaemonMsg::Output(bytes) = &more
                                {
                                    tr_bytes += bytes.len() as u64;
                                }
                                gated += queued_gate_len(&more);
                                queued.push_back(more);
                            }
                            if gated > 0
                                && let Some(gate) = &drain
                            {
                                gate.sub(gated);
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            teardown(Some("the far end closed it (EOF)"));
                            break;
                        }
                    }
                }
            })
            .expect("spawn remote reader thread")
    }

    pub fn poll_exited(&mut self) {
        if self.exited_flag.load(Ordering::SeqCst) {
            self.exited = true;
        }
    }

    pub fn child_exited(&self) -> bool {
        self.child_exited.load(Ordering::SeqCst)
    }

    pub(super) fn is_local_conpty(&self) -> bool {
        self.local_conpty.load(Ordering::Relaxed)
    }

    pub(super) fn color_scheme_updates(&self) -> bool {
        self.color_scheme_updates.load(Ordering::Relaxed)
    }

    /// Queues a keystroke — or a paste, or a mouse report — for the pane.
    ///
    /// Callers are gpui event handlers on the UI thread, so this returns
    /// without waiting on the child.
    pub fn write<B: Into<Cow<'static, [u8]>>>(&self, bytes: B) {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return;
        }
        self.link.send(ClientMsg::Input(bytes.into_owned()));
    }

    /// Whether the daemon behind this pane echoes a `DaemonMsg::Size` into the
    /// output stream when it applies our `ClientMsg::Resize`. When it does, the
    /// local grid reflow is deferred to that echo on the reader thread: any
    /// backlog still queued between daemon and client was produced at the old
    /// geometry, and reflowing before it drains parses old-width bytes into a
    /// new-width grid (maximize during a burst of output garbled the pane).
    ///
    /// The local daemon is probed directly. A routed pane consults the answer
    /// its route carries, read off the host's control hello when the route was
    /// built — the daemon serves one `ClientMsg` per connection, so asking
    /// `Version` per pane would cost a whole routed connection each time. The
    /// remaining cases never defer: an older server does not advertise the
    /// echo, an unroutable route reaches no daemon at all, and deferral must
    /// not hang the grid on an echo nobody promised.
    fn resize_echoed(&self) -> bool {
        // The pane's child is in this process and echoes `DaemonMsg::Size`
        // when the resize lands. A test link does not, unless the test says
        // it is playing that daemon (`direct`).
        self.direct
    }

    pub fn resize(&mut self, size: TermSize, cell_w: u16, cell_h: u16) {
        let echoed = self.resize_echoed();
        // The cell size has to be part of the early-out, not just cols/rows: it
        // is reported in *device* pixels, so moving the window between a 2x and
        // a 1x display changes `ws_xpixel`/`ws_ypixel` while the grid stays
        // exactly the same. Comparing only `size` there would skip the resize
        // and leave a pixel-aware child rendering for the old framebuffer.
        let cell = (cell_w, cell_h);
        if self.synced_size && size == self.size && cell == self.synced_cell {
            // Already asked for, so the only question is whether the grid got
            // there. It is not enough to trust the request (#893): on an
            // echoing route the grid moves only when the daemon's `Size` comes
            // back, and a request or an echo that went missing left a pane
            // painting a grid shorter than its bounds — the bottom rows blank,
            // the child still drawing for the old size — with every later
            // frame asking for the same size and being skipped here, until a
            // divider drag asked for a different one.
            if echoed {
                // Disagreement while an echo can still be on its way is just
                // the echo (or a replay segment mid-apply) not having landed
                // yet. Asked first because it needs no lock.
                if self
                    .resize_sent_at
                    .is_some_and(|at| at.elapsed() < RESIZE_ECHO_GRACE)
                {
                    return;
                }
                // Tried rather than waited for, as the painter does: this runs
                // every frame, and a held lock is the reader mid-batch — the
                // next frame asks again.
                match self.term.try_lock_unfair() {
                    Some(term) if !grid_holds(&term, size) => {}
                    _ => return,
                }
            } else if grid_holds(&self.term.lock(), size) {
                return;
            }
        }
        self.synced_size = true;
        self.size = size;
        self.synced_cell = cell;
        if !echoed {
            let shell_redraws = !self.is_local_conpty() && self.at_prompt();
            super::prompt_reflow::resize(&mut self.term.lock(), size, shell_redraws);
        }

        let win = win_size(size, cell_w, cell_h);
        self.link.send(ClientMsg::Resize(win));
        self.resize_sent_at = Some(std::time::Instant::now());
    }

    pub fn foreground_cwd(&self) -> Option<PathBuf> {
        self.cwd.lock().ok().and_then(|g| g.clone())
    }

    pub fn remote_context(&self) -> Option<RemoteContext> {
        self.remote_context.lock().ok().and_then(|g| g.clone())
    }

    pub fn at_prompt(&self) -> bool {
        self.shell_state
            .lock()
            .map(|s| s.active && s.at_prompt)
            .unwrap_or(false)
    }

    pub fn prompt_seq(&self) -> u64 {
        self.shell_state.lock().map(|s| s.seq).unwrap_or(0)
    }

    pub fn prompt_cycle(&self) -> u64 {
        self.shell_state.lock().map(|s| s.cycle).unwrap_or(0)
    }

    pub fn last_exit_code(&self) -> Option<i32> {
        self.shell_state.lock().ok().and_then(|s| s.last_exit)
    }

    pub fn shell_active(&self) -> bool {
        self.shell_state.lock().map(|s| s.active).unwrap_or(false)
    }

    pub fn foreground_agent(&self) -> Option<CLIAgent> {
        self.agent.lock().ok().and_then(|g| *g)
    }

    /// The kitty-graphics image store for this pane. Cheap handle clone — the
    /// store is an `Arc<Mutex<..>>` shared with the reader thread, which places
    /// and deletes images as out-of-band frames arrive from the daemon.
    pub fn images(&self) -> crate::terminal::images::ImageStore {
        self.images.clone()
    }

    /// Desktop notifications the program wrote (OSC 9, 99, 777), oldest
    /// first; the view decides whether each is shown.
    pub fn take_osc_notes(&self) -> Vec<Note> {
        self.osc_notes
            .lock()
            .map(|mut notes| notes.drain(..).collect())
            .unwrap_or_default()
    }

    /// Of `notes` about to be shown, the ones this pane's [`NoteBudget`] lets
    /// through, and whether notes it dropped earlier are due to be said.
    pub fn pace_osc_notes(
        &self,
        mut notes: Vec<Note>,
        now: std::time::Instant,
    ) -> (Vec<Note>, bool) {
        let Ok(mut budget) = self.note_budget.lock() else {
            return (notes, false);
        };
        let (show, dropped) = budget.admit(now, notes.len());
        notes.truncate(show);
        (notes, dropped)
    }

    pub fn pop_clipboard_write(&self) -> Option<tty7_core::core::clipboard::ClipboardWrite> {
        self.clipboard_writes
            .lock()
            .ok()
            .and_then(|mut pending| pending.pop_front())
    }

    pub fn finish_clipboard_write(&self) {
        self.clipboard_write_busy.store(false, Ordering::Release);
    }

    pub fn agent_session(&self) -> Option<AgentSessionState> {
        self.agent_session.lock().ok().and_then(|g| g.state.clone())
    }

    pub fn zle_reading(&self) -> bool {
        self.zle_reading.load(Ordering::Relaxed)
    }

    pub fn shell_vi_mode(&self) -> bool {
        self.shell_vi_mode.load(Ordering::Relaxed)
    }

    /// The line the shell reported running, still percent-escaped. Empty at a
    /// prompt, and empty for a shell whose integration marks the start of a
    /// command without naming it.
    pub fn running_command(&self) -> String {
        self.running_command
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default()
    }

    pub fn size(&self) -> TermSize {
        self.size
    }

    pub fn list_panes_on(route: &PaneRoute) -> Vec<crate::daemon::protocol::PaneInfo> {
        Self::try_list_panes_on(route).unwrap_or_default()
    }

    pub fn try_list_panes_on(
        _route: &PaneRoute,
    ) -> anyhow::Result<Vec<crate::daemon::protocol::PaneInfo>> {
        let holds = live_panes().lock().unwrap_or_else(|e| e.into_inner());
        Ok(holds
            .values()
            .filter_map(|hold| {
                hold.pane
                    .lock()
                    .ok()
                    .and_then(|guard| guard.as_ref().map(|pane| pane.info()))
            })
            .collect())
    }

    pub fn kill_pane(pane_id: u64) {
        Self::kill_pane_on(&PaneRoute::Local, pane_id)
    }

    pub fn kill_pane_on(_route: &PaneRoute, pane_id: u64) {
        let hold = live_panes()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pane_id)
            .cloned();
        if let Some(hold) = hold
            && let Some(pane) = hold.pane.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        {
            pane.kill();
        }
    }
}

impl Drop for RemoteTerminal {
    fn drop(&mut self) {
        if let Some(hold) = self.link.pane_hold() {
            release_hold(hold);
            if let Some(id) = self.pane_id {
                live_panes()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
            }
        }
        self.stop_reader();
    }
}

fn stale_mode_resets(mode: TermMode) -> Vec<u8> {
    let mut seq = Vec::new();
    if mode.contains(TermMode::ALT_SCREEN) {
        seq.extend_from_slice(b"\x1b[?1049l");
    }
    if !mode.contains(TermMode::SHOW_CURSOR) {
        seq.extend_from_slice(b"\x1b[?25h");
    }
    if mode.intersects(TermMode::MOUSE_MODE) {
        seq.extend_from_slice(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l");
    }
    if mode.contains(TermMode::SGR_MOUSE) {
        seq.extend_from_slice(b"\x1b[?1006l");
    }
    if mode.contains(TermMode::UTF8_MOUSE) {
        seq.extend_from_slice(b"\x1b[?1005l");
    }
    if mode.contains(TermMode::FOCUS_IN_OUT) {
        seq.extend_from_slice(b"\x1b[?1004l");
    }
    // The kitty keyboard flags are not reset here: no sequence clears their
    // stacks, so the caller does it through `Term::reset_keyboard_modes`.
    seq
}

type OscNotes = Arc<Mutex<VecDeque<Note>>>;

/// Notes queued for a view that has not polled: a burst keeps only its
/// newest few, so it shows as a few rather than a spray.
const MAX_OSC_NOTES: usize = 3;

struct OscNotifyScanner {
    tok: OscTokenizer,
    notes: crate::core::osc::Notifications,
}

impl Default for OscNotifyScanner {
    fn default() -> Self {
        Self {
            tok: OscTokenizer::new(&[b"9", b"99", b"777"]),
            notes: Default::default(),
        }
    }
}

impl OscNotifyScanner {
    fn is_ground(&self) -> bool {
        self.tok.is_ground()
    }

    fn feed(&mut self, bytes: &[u8], out: &mut VecDeque<Note>) {
        let notes = &mut self.notes;
        self.tok.feed(bytes, |payload| {
            if let Some(note) = parse_osc_notification(notes, payload) {
                if out.len() >= MAX_OSC_NOTES {
                    out.pop_front();
                }
                out.push_back(note);
            }
        });
    }
}

fn parse_osc_notification(
    notes: &mut crate::core::osc::Notifications,
    payload: &[u8],
) -> Option<Note> {
    if crate::core::cli_agent::parse_agent_event(payload).is_some() {
        return None;
    }
    let (title, body) = notes.parse(payload)?;
    if title.as_deref() == Some(crate::core::cli_agent::AGENT_EVENT_SENTINEL) {
        return None;
    }
    Some((title, body))
}

fn terminal_config_from_user(user_config: &crate::core::config::Config) -> Config {
    Config {
        scrolling_history: user_config.scrollback_limit,
        default_cursor_style: alacritty_cursor_style(user_config.cursor_style),
        semantic_escape_chars: user_config.word_separators.clone(),
        kitty_keyboard: true,
        // Every pane on Windows is presented through ConPTY (a shell, wsl.exe,
        // ssh.exe — conhost mediates them all), which repaints nothing after a
        // resize and keeps addressing the screen against its own re-anchored
        // layout: grow keeps rows/cursor pinned and opens blank rows below,
        // shrink scrolls the last written row to the new bottom. The grid must
        // resize the same way or every later absolute-CUP paint (PSReadLine
        // redraws its prompt that way per keystroke) lands rows off, shredding
        // the screen the first time a maximized pane draws anything.
        conpty_resize: cfg!(windows),
        ..Config::default()
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    /// The whole conhost-semantics fix hangs on this one field: it defaults to
    /// off in `alacritty_terminal`, so dropping the line compiles clean, passes
    /// every other test, and quietly resurrects the maximize garbling — which
    /// is exactly how it shipped disabled once (#415 follow-up).
    #[test]
    fn windows_panes_opt_into_conpty_resize_semantics() {
        let config = terminal_config_from_user(&crate::core::config::Config::default());
        assert_eq!(config.conpty_resize, cfg!(windows));
        #[cfg(windows)]
        assert!(config.conpty_resize);
    }
}

/// Issue #857 asked whether a read boundary can leave the grid different from
/// the one the same bytes parse into in one go — a Pi response streamed as
/// synchronized differential frames (cursor-up, erase-line, rewrite) with
/// OSC 133 marks and CJK text. The reader hands the emulator whatever the
/// socket delivers, so the grid has to come out the same for every split.
#[cfg(test)]
mod chunking_tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions as _;
    use alacritty_terminal::index::{Column, Line};

    const COLS: usize = 24;
    const ROWS: usize = 8;

    fn parse(chunks: &[&[u8]]) -> Vec<String> {
        let config = terminal_config_from_user(&crate::core::config::Config::default());
        let mut term = Term::new(config, &TermSize::new(COLS, ROWS), VoidListener);
        let mut processor: ansi::Processor = ansi::Processor::new();
        for chunk in chunks {
            processor.advance(&mut term, chunk);
        }
        assert!(
            processor.sync_timeout().sync_timeout().is_none(),
            "every frame closes its synchronized update"
        );
        let grid = term.grid();
        let mut rows: Vec<String> = (0..grid.screen_lines() as i32)
            .map(|line| {
                (0..COLS)
                    .map(|col| {
                        let cell = &grid[Line(line)][Column(col)];
                        let mut s = format!("{}{:?}", cell.c, cell.flags);
                        for z in cell.zerowidth().unwrap_or(&[]) {
                            s.push(*z);
                        }
                        s
                    })
                    .collect()
            })
            .collect();
        rows.push(format!("{:?}", grid.cursor.point));
        rows
    }

    /// Three frames of a response streaming in, each redrawing the lines it
    /// touched the way Pi's main-screen renderer does.
    fn stream() -> Vec<u8> {
        let mark = "\x1b]133;B\x07\x1b]133;C\x07";
        let frames = [
            format!("\x1b[?2026h\r\x1b[2K例{mark}Ex\x1b[?2026l"),
            format!(
                "\x1b[?2026h\r\x1b[2K例子：Example sentence.\r\n\r\n\x1b[2K## 资源{mark}N\x1b[?2026l"
            ),
            format!(
                "\x1b[?2026h\x1b[2A\r\x1b[2K例子：Example.❤️\r\n\r\n\x1b[2K## 资源\r\n\
                 \x1b[2K{mark}下一步：Next step.\x1b[?2026l"
            ),
        ];
        frames.concat().into_bytes()
    }

    #[test]
    fn a_streamed_response_parses_the_same_at_every_split() {
        let bytes = stream();
        let whole = parse(&[&bytes]);
        assert!(
            whole.iter().all(|row| !row.contains("33;")),
            "no mark reaches the grid as text"
        );
        for cut in 0..=bytes.len() {
            assert_eq!(
                parse(&[&bytes[..cut], &bytes[cut..]]),
                whole,
                "split at byte {cut}"
            );
        }
        let bytewise: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(parse(&bytewise), whole, "a byte at a time");
    }

    /// A batch several lock-holds long, colored and multi-byte so slices end
    /// mid-sequence and mid-character.
    fn long_batch() -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut n = 0;
        while bytes.len() < 3 * MAX_LOCKED_FEED + 777 {
            bytes.extend_from_slice(format!("\x1b[3{}m行{n}\x1b[0m\r\n", n % 8).as_bytes());
            n += 1;
        }
        bytes
    }

    fn fresh() -> Term<VoidListener> {
        let config = terminal_config_from_user(&crate::core::config::Config::default());
        Term::new(config, &TermSize::new(COLS, ROWS), VoidListener)
    }

    /// Where the cursor stood at each cut, and the screen after the whole batch.
    type Seen = (Vec<(i32, usize)>, Vec<String>);

    fn screen(term: &Term<VoidListener>) -> Vec<String> {
        let grid = term.grid();
        (0..ROWS as i32)
            .map(|line| {
                (0..COLS)
                    .map(|col| grid[Line(line)][Column(col)].c)
                    .collect()
            })
            .collect()
    }

    fn cursor(term: &Term<VoidListener>) -> (i32, usize) {
        let point = term.grid().cursor.point;
        (point.line.0, point.column.0)
    }

    /// The reader lets go of the grid every `MAX_LOCKED_FEED` bytes, and not
    /// one of those slices may show: the screen, and the state each cut acts
    /// on, come out as if the batch had been parsed in one hold — including a
    /// cut that lands exactly on a slice boundary.
    #[test]
    fn a_batch_fed_in_slices_parses_as_one() {
        let bytes = long_batch();
        let offsets = [
            0,
            5,
            MAX_LOCKED_FEED,
            MAX_LOCKED_FEED + 1,
            2 * MAX_LOCKED_FEED + 9,
            bytes.len(),
        ];

        let whole: Seen = {
            let mut term = fresh();
            let mut processor: ansi::Processor = ansi::Processor::new();
            let mut at = 0;
            let mut stops = Vec::new();
            for &off in &offsets {
                processor.advance(&mut term, &bytes[at..off]);
                at = off;
                stops.push(cursor(&term));
            }
            (stops, screen(&term))
        };

        let term = FairMutex::new(fresh());
        let mut processor: ansi::Processor = ansi::Processor::new();
        let cuts = offsets
            .iter()
            .map(|&off| (off, ReaderCut::Command(CommandMark::Finished)))
            .collect();
        let mut stops = Vec::new();
        let fed = feed_grid(
            &term,
            &mut processor,
            &bytes,
            cuts,
            &AtomicBool::new(false),
            |term, _| stops.push(cursor(term)),
        );
        assert!(fed.is_some());
        assert_eq!((stops, screen(&term.lock())), whole);
    }

    /// A reader retired while it waited for the grid writes nothing into it:
    /// the grid belongs to its successor now.
    #[test]
    fn a_retired_reader_leaves_the_grid_alone() {
        let term = FairMutex::new(fresh());
        let before = screen(&term.lock());
        let mut processor: ansi::Processor = ansi::Processor::new();
        let fed = feed_grid(
            &term,
            &mut processor,
            b"stale",
            Vec::new(),
            &AtomicBool::new(true),
            |_, _| {},
        );
        assert!(fed.is_none());
        assert_eq!(screen(&term.lock()), before);
    }
}

/// A point in a batch of pty output where the reader stops advancing the
/// emulator to act on the state the bytes before it left behind.
enum ReaderCut {
    Parked(CursorCut),
    Command(CommandMark),
    Prompt(PromptMark),
    /// A tracked query ended here. Answered after the emulator has parsed that
    /// byte, so the reply follows any `PtyWrite` the byte produced and precedes
    /// whatever the rest of the batch asks.
    Reply(Query),
}

/// Adds the batch's command marks to `cuts`, keeping them in stream order
/// alongside whatever cuts are already there.
fn command_cuts(tok: &mut OscTokenizer, bytes: &[u8], cuts: &mut Vec<(usize, ReaderCut)>) {
    let before = cuts.len();
    tok.feed_at(bytes, |off, payload| {
        if let Some(mark) = CommandMark::parse(payload) {
            cuts.push((off, ReaderCut::Command(mark)));
        } else if let Some(mark) = PromptMark::parse(payload) {
            cuts.push((off, ReaderCut::Prompt(mark)));
        }
    });
    if before > 0 && cuts.len() > before {
        // Stable, so a mark that ends where a cursor show does keeps its place
        // after it.
        cuts.sort_by_key(|(off, _)| *off);
    }
}

/// Bytes parsed per hold of the grid lock, so a UI handler queued on it can run between slices.
const MAX_LOCKED_FEED: usize = 256 * 1024;

/// Reply to a query the mode tracker saw, in the form the view writes to the
/// pty. Device attributes are left to the emulator, which has already emitted
/// its own `PtyWrite` for the same byte.
fn answer_stream_query(proxy: &EventProxy, query: Query) {
    match query {
        Query::ColorSchemeMode { set: true } => {
            proxy.send_event(AlacEvent::PtyWrite("\x1b[?2031;1$y".into()));
        }
        Query::ColorSchemeMode { set: false } => {
            proxy.send_event(AlacEvent::PtyWrite("\x1b[?2031;2$y".into()));
        }
        Query::ColorScheme => proxy.send_event(super::color_scheme::query_reply()),
        Query::DeviceAttributes => {}
    }
}

/// Parses `bytes`, running `on_cut` at each cut, and dropping the grid lock every [`MAX_LOCKED_FEED`] bytes.
/// The lock is fair, and vte keeps its state across slices. Returns `None` if `quit` fired while waiting:
/// a retired reader must not write a grid its successor owns.
fn feed_grid<T: EventListener>(
    term: &FairMutex<Term<T>>,
    processor: &mut ansi::Processor,
    bytes: &[u8],
    cuts: Vec<(usize, ReaderCut)>,
    quit: &AtomicBool,
    mut on_cut: impl FnMut(&mut Term<T>, ReaderCut),
) -> Option<std::time::Duration> {
    let mut waited = std::time::Duration::ZERO;
    let mut lock = || {
        let t = std::time::Instant::now();
        let grid = term.lock();
        waited += t.elapsed();
        (!quit.load(Ordering::SeqCst)).then_some(grid)
    };
    let mut grid = lock()?;
    let mut held = 0usize;
    let mut at = 0usize;
    let mut cuts = cuts.into_iter();
    let mut next = cuts.next();
    loop {
        let stop = next.as_ref().map_or(bytes.len(), |(off, _)| *off);
        while at < stop {
            if held == MAX_LOCKED_FEED {
                drop(grid);
                grid = lock()?;
                held = 0;
            }
            let end = stop.min(at + MAX_LOCKED_FEED - held);
            processor.advance(&mut *grid, &bytes[at..end]);
            held += end - at;
            at = end;
        }
        let Some((_, cut)) = next else { break };
        on_cut(&mut grid, cut);
        next = cuts.next();
    }
    drop(grid);
    Some(waited)
}

fn alacritty_cursor_style(style: ConfigCursorStyle) -> CursorStyle {
    let shape = match style {
        ConfigCursorStyle::Block => CursorShape::Block,
        ConfigCursorStyle::Bar => CursorShape::Beam,
        ConfigCursorStyle::Underline => CursorShape::Underline,
    };
    CursorStyle {
        shape,
        blinking: false,
    }
}

fn win_size(size: TermSize, cell_w: u16, cell_h: u16) -> WinSize {
    WinSize {
        cols: size.cols as u16,
        rows: size.rows as u16,
        cell_w,
        cell_h,
    }
}

/// What a re-attach's replay actually leaves in the grid.
///
/// Switching workspaces tears every pane down and attaches to the same daemon
/// pane again (#711), so the whole of a pane's screen has to survive one trip
/// through the replay — several ring segments, each preceded by the geometry it
/// was written at, and a snapshot frame far larger than one socket read. These
/// drive that over a real socket pair rather than a mock, because the loss
/// being hunted is between the wire and the grid; and they are not `cfg(unix)`
/// like their neighbours because nothing about the path is.
#[cfg(test)]
mod replay_tests {
    use super::*;
    use crate::daemon::protocol::DaemonMsg;

    /// Everything the grid holds, scrollback included, one row per line.
    fn all_text(term: &RemoteTerminal) -> String {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line};
        let t = term.term.lock();
        let grid = t.grid();
        let mut out = String::new();
        let top = -(grid.history_size() as i32);
        for line in top..grid.screen_lines() as i32 {
            let row = &grid[Line(line)];
            let mut text = String::new();
            for col in 0..grid.columns() {
                text.push(row[Column(col)].c);
            }
            let text = text.trim_end();
            if !text.is_empty() {
                out.push_str(text);
                out.push('\n');
            }
        }
        out
    }

    /// The grid's width. A grid is born at the size its `RemoteTerminal` was
    /// built with and only ever leaves it on a `DaemonMsg::Size`, so this is
    /// what says which geometry frames were applied.
    fn columns(term: &RemoteTerminal) -> usize {
        use alacritty_terminal::grid::Dimensions as _;
        term.term.lock().grid().columns()
    }

    /// Waits for the reader thread to have applied everything named, then
    /// returns the whole grid either way so a failure can print what landed.
    fn settled(term: &RemoteTerminal, needles: &[&str]) -> String {
        for _ in 0..600 {
            let text = all_text(term);
            if needles.iter().all(|n| text.contains(n)) {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        all_text(term)
    }

    // ---- The switch, end to end, with a real daemon pane ----------------
    //
    // Everything above drives the client half against a scripted daemon. This
    // drives the real one: a live `DaemonPane` over a real pty, in this
    // process, with its `DaemonMsg` stream forwarded onto a socket pair the
    // way `spawn_writer` forwards it, so an attach here is the same attach a
    // re-attach makes. It is the switch (#711) minus gpui: attach, resize to
    // the geometry the window actually has, produce output, drop the client,
    // produce more, attach again — and read the grid the second client ends up
    // with.

    /// What a physical Escape is written to the pane as, by the encoder a
    /// local ConPTY pane uses (#1155 is a ConPTY-only symptom), once the
    /// reader has applied everything up to `barrier`. Also returns the reply
    /// a `CSI ? u` sent after the barrier gets.
    fn escape_after(frames: Vec<DaemonMsg>, barrier: &str) -> (Vec<u8>, Option<String>) {
        use crate::terminal::input::{KeyFlags, keystroke_to_bytes};
        crate::core::config::pin_test_config_dir();
        let (term, daemon) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        for frame in frames {
            daemon.send(frame);
        }
        daemon.send(DaemonMsg::Output(format!("{barrier}\x1b[?u").into_bytes()));
        let text = settled(&term, &[barrier]);
        assert!(text.contains(barrier), "the barrier never arrived:\n{text}");
        let mut reply = None;
        for _ in 0..200 {
            while let Ok(event) = term.events.try_recv() {
                if let AlacEvent::PtyWrite(text) = event {
                    reply = Some(text);
                }
            }
            if reply.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mode = *term.term.lock().mode();
        let flags = KeyFlags::from_mode_with_local_conpty(&mode, true);
        let escape = keystroke_to_bytes(&gpui::Keystroke::parse("escape").unwrap(), flags);
        daemon.close();
        (escape.unwrap_or_default(), reply)
    }

    fn at_prompt() -> DaemonMsg {
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(130),
        }
    }

    /// #1155: a program pushed kitty keyboard flags on the main screen and
    /// again on the alternate one, and was killed there. Once the prompt is
    /// back, the next program to enter the alternate screen must not find the
    /// dead one's flags on its stack.
    #[test]
    fn a_prompt_forgets_kitty_flags_a_killed_program_left_on_either_screen() {
        let (escape, reply) = escape_after(
            vec![
                DaemonMsg::Output(b"\x1b[>1u\x1b[?1049h\x1b[>7u".to_vec()),
                at_prompt(),
                DaemonMsg::Output(b"\x1b[?1049h".to_vec()),
            ],
            "NEXT-PROGRAM",
        );
        assert_eq!(escape, b"\x1b", "Escape must reach the next program bare");
        assert_eq!(
            reply.as_deref(),
            Some("\x1b[?0u"),
            "a flags query must agree with the flags in effect"
        );
    }

    /// #1155: a program that left the alternate screen *before* popping
    /// popped the main screen's stack, so its push stayed on the alternate
    /// one while no flags were in effect at the prompt.
    #[test]
    fn a_prompt_forgets_a_kitty_push_stranded_on_the_inactive_screen() {
        let (escape, _) = escape_after(
            vec![
                DaemonMsg::Output(b"\x1b[?1049h\x1b[>7u\x1b[?1049l\x1b[<u".to_vec()),
                at_prompt(),
                DaemonMsg::Output(b"\x1b[?1049h".to_vec()),
            ],
            "NEXT-PROGRAM",
        );
        assert_eq!(escape, b"\x1b");
    }

    /// A stale entry under the next program's own push must not come back
    /// when it pops.
    #[test]
    fn a_pop_after_the_prompt_does_not_restore_stale_kitty_flags() {
        let (escape, _) = escape_after(
            vec![
                DaemonMsg::Output(b"\x1b[>7u".to_vec()),
                at_prompt(),
                DaemonMsg::Output(b"\x1b[>1u\x1b[<u".to_vec()),
            ],
            "NEXT-PROGRAM",
        );
        assert_eq!(escape, b"\x1b");
    }

    /// The guard on the other side: flags a program negotiates after the
    /// prompt returned are its own and take effect.
    #[test]
    fn kitty_flags_pushed_after_the_prompt_take_effect() {
        let (escape, reply) = escape_after(
            vec![
                DaemonMsg::Output(b"\x1b[?1049h\x1b[>7u".to_vec()),
                at_prompt(),
                DaemonMsg::Output(b"\x1b[?1049h\x1b[>1u".to_vec()),
            ],
            "NEXT-PROGRAM",
        );
        assert_eq!(escape, b"\x1b[27u");
        assert_eq!(reply.as_deref(), Some("\x1b[?1u"));
    }

    #[cfg(unix)]
    #[test]
    fn a_spawned_pane_prints_resizes_and_exits_in_process() {
        crate::core::config::pin_test_config_dir();
        let expected = crate::core::config::config_dir_path()
            .expect("pinned config dir")
            .display()
            .to_string();
        let (term, id) = RemoteTerminal::spawn(
            TermSize::new(80, 24),
            8,
            16,
            None,
            Some(ShellSpec {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "printf 'NTTY7_M2_OK %s\\n' \"$NTTY7_CONFIG_DIR\"; sleep 30".into(),
                ],
                args_are_tty7_defaults: false,
            }),
        )
        .expect("spawn a local shell");
        let text = settled(&term, &["NTTY7_M2_OK"]);
        assert!(
            text.contains(&expected),
            "NTTY7_CONFIG_DIR should be the config dir ({expected}); grid held:\n{text}"
        );
        let mut term = term;
        term.resize(TermSize::new(100, 30), 9, 18);
        for _ in 0..200 {
            if columns(&term) == 100 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            columns(&term),
            100,
            "the pane's Size echo never reflowed the grid"
        );
        drop(term);
        assert!(
            !live_panes()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&id),
            "dropping the terminal must release the pane"
        );
    }

    #[cfg(unix)]
    #[test]
    fn closing_a_pane_retires_its_history_file() {
        crate::core::config::pin_test_config_dir();
        let (term, id) = RemoteTerminal::spawn(
            TermSize::new(80, 24),
            8,
            16,
            None,
            Some(ShellSpec {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 30".into()],
                args_are_tty7_defaults: false,
            }),
        )
        .expect("spawn");
        let path = tty7_core::daemon::history::path_for(id).expect("history path");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let origin = path.parent().unwrap().join(format!("origin-{id}"));
        std::fs::write(&origin, b"").unwrap();
        std::fs::write(&path, b"seeded\nfrom this pane\n").unwrap();
        std::fs::write(format!("{}.seed", path.display()), b"7").unwrap();
        std::fs::write(
            format!("{}.origin", path.display()),
            format!("{}\n", origin.display()),
        )
        .unwrap();
        drop(term);
        let merged = std::fs::read_to_string(&origin).unwrap_or_default();
        assert!(
            merged.contains("from this pane"),
            "retire should append the pane's own lines; got {merged:?}"
        );
        assert!(!path.exists(), "the pane file is removed once it is merged");
    }
}

#[cfg(test)]
mod detach_tests {
    use super::*;

    /// Detach must return without joining the reader. The reader is parked on
    /// the output pipe, and the fixture keeps that pipe open, so a join would
    /// sit here until the link is dropped.
    #[test]
    fn detach_link_returns_promptly_when_the_peer_stays_silent() {
        crate::core::config::pin_test_config_dir();
        let (mut term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            PtySource::for_route(&PaneRoute::Local),
        );
        // Let the reader thread park on the pipe before tearing down.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            term.detach_link();
            let _ = tx.send(());
        });
        let outcome = rx.recv_timeout(std::time::Duration::from_secs(3));
        daemon_side.close();
        assert!(
            outcome.is_ok(),
            "detach_link blocked for over 3s while the pipe stayed open"
        );
    }
}

/// Issue #430, and #774 for the half of it that was still open.
///
/// Whether the reader puts back a parked cursor is a property of the pty this
/// pane is attached to, not of the platform the client was compiled for — so
/// these drive both answers on every platform, over an in-memory pane link. Before
/// #774 the decision was `cfg!(windows)`, which meant a Windows client talking
/// to a Linux host repaired a cursor no conhost had parked, and the `wq` typed
/// after vim's `:` landed on the row being edited.
#[cfg(test)]
mod parked_cursor_tests {
    use super::*;

    fn terminal_on(pty: PtySource, size: TermSize) -> (RemoteTerminal, TestLink) {
        crate::core::config::pin_test_config_dir();
        let (term, daemon_side) = RemoteTerminal::from_pipe(size, pty);
        assert_eq!(term.is_local_conpty(), pty == PtySource::LocalConpty);
        (term, daemon_side)
    }

    /// Feeds one conhost-shaped repaint and reports the cell the cursor ends on,
    /// waiting for the `X` the frame paints so the reader is known to be done.
    fn cursor_after_conpty_frame(pty: PtySource, frame: &[u8]) -> (i32, usize) {
        let (term, daemon_side) = terminal_on(pty, TermSize::new(80, 24));

        daemon_side.send(
            // Where the TUI put the cursor before conhost repainted over it.
            DaemonMsg::Output(b"\x1b[6;4H".to_vec()),
        );
        daemon_side.send(DaemonMsg::Output(frame.to_vec()));

        for _ in 0..600 {
            {
                let t = term.term.lock();
                let painted = t.grid()[alacritty_terminal::index::Line(19)]
                    [alacritty_terminal::index::Column(1)]
                .c;
                if painted == 'X' {
                    let point = t.grid().cursor.point;
                    return (point.line.0, point.column.0);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the reader never applied the frame");
    }

    #[test]
    fn a_local_route_is_a_conpty_only_where_conhost_exists() {
        assert_eq!(
            PtySource::for_route(&PaneRoute::Local),
            if cfg!(windows) {
                PtySource::LocalConpty
            } else {
                PtySource::Raw
            },
        );
    }

    #[test]
    fn a_conpty_frame_that_shows_the_cursor_over_an_erase_keeps_the_cell_it_hid_on() {
        assert_eq!(
            cursor_after_conpty_frame(
                PtySource::LocalConpty,
                b"\x1b[?25l\x1b[20;2HX\x1b[K\x1b[m\x1b[22;42H\x1b[K\x1b[?25h",
            ),
            (5, 3),
            "conhost parked the cursor on the cell it erased last; the cursor \
             belongs where it was when the repaint hid it"
        );
    }

    #[test]
    fn the_same_frame_off_a_raw_pty_leaves_the_cursor_where_the_frame_left_it() {
        assert_eq!(
            cursor_after_conpty_frame(
                PtySource::Raw,
                b"\x1b[?25l\x1b[20;2HX\x1b[K\x1b[m\x1b[22;42H\x1b[K\x1b[?25h",
            ),
            (21, 41),
            "with no conhost in between the stream is the application's own, \
             and the cell it left the cursor on is the cell it meant"
        );
    }

    #[test]
    fn a_conpty_frame_that_moves_the_cursor_before_showing_it_is_obeyed() {
        assert_eq!(
            cursor_after_conpty_frame(
                PtySource::LocalConpty,
                b"\x1b[?25l\x1b[20;2HX\x1b[K\x1b[m\x1b[22;42H\x1b[K\x1b[9;9H\x1b[?25h"
            ),
            (8, 8),
            "the frame painted the cursor somewhere on purpose"
        );
    }

    /// Vim opens its command line with exactly the shape the parked-cursor
    /// scanner calls parked — hide, move around to paint, end on the `:` it
    /// wrote — and then echoes every following keystroke as a bare byte at
    /// wherever that left the cursor. Putting the cursor back on a raw pty
    /// therefore does not straighten out a stray caret, it drops `wq!` onto the
    /// row vim was editing. Bytes below are a capture of vim 9 on a 20x11 pty.
    #[test]
    fn a_raw_pty_repaint_keeps_the_cursor_the_frame_left_so_the_echo_lands_on_it() {
        let (term, daemon_side) = terminal_on(PtySource::Raw, TermSize::new(20, 11));

        let mut stream: Vec<u8> = Vec::new();
        // `vim test.md`: the alternate screen, the file, cursor home.
        stream.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J\x1b[1;1H123456789\x1b[1;1H");
        // Esc, then `:` — two bracketed repaints, the second ending on the `:`
        // vim wrote at the head of the command line.
        stream.extend_from_slice(b"\x1b[?25l\x1b[m\x1b[11;10H^[\x1b[1;1H\x1b[?25h");
        stream.extend_from_slice(b"\x1b[?25l\x1b[11;10H  \x1b[1;1H\x07\x1b[?25h");
        stream.extend_from_slice(
            b"\x1b[?25l\x1b[11;10H:\x1b[1;1H\x1b[11;1H\x1b[K\x1b[11;1H:\x1b[?25h",
        );
        // `w`, `q`, `!`: vim echoes them with no positioning of their own.
        stream.extend_from_slice(b"wq!");
        daemon_side.send(DaemonMsg::Output(stream));

        let row = |t: &Term<EventProxy>, line: i32| -> String {
            (0..20)
                .map(|col| {
                    t.grid()[alacritty_terminal::index::Line(line)]
                        [alacritty_terminal::index::Column(col)]
                    .c
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        };

        // The whole batch is applied under one lock, so the `:` landing on the
        // command line means every byte after it landed too.
        let mut command_line = String::new();
        let mut edited = String::new();
        for _ in 0..600 {
            {
                let t = term.term.lock();
                command_line = row(&t, 10);
                edited = row(&t, 0);
            }
            if command_line.starts_with(':') {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            command_line, ":wq!",
            "the keystrokes belong after the `:` the repaint ended on"
        );
        assert_eq!(
            edited, "123456789",
            "and nothing of them belongs on the row vim was editing"
        );
    }
}

/// Issue #774, from the client's end. A full-screen tool sets its modes once
/// and the replay ring drops them, so the daemon re-sends them from what it
/// folded out of the stream ([`tty7_core::core::term_modes`]). This is the
/// other half of that: the bytes it re-sends have to land the emulator back
/// where the application left it, because those are the modes `wheel_route`
/// reads before it decides the pane has a scrollback to move at all.
#[cfg(test)]
mod replayed_mode_tests {
    use super::*;
    use tty7_core::core::term_modes::TerminalModes;

    /// #1074, checked against the emulator itself rather than against our
    /// reading of it: wherever the ring's front falls, the prelude plus what
    /// the ring still holds has to leave the client's kitty keyboard flags
    /// where the whole stream leaves them — and its stacks too, which only
    /// show once the program pops or switches screens, so those are probed
    /// after.
    #[test]
    fn a_replay_prelude_lands_the_kitty_keyboard_flags_where_the_stream_does() {
        use alacritty_terminal::event::VoidListener;

        fn kitty_after(chunks: &[&[u8]], probes: &[&[u8]]) -> Vec<TermMode> {
            let config = terminal_config_from_user(&crate::core::config::Config::default());
            let mut term = Term::new(config, &TermSize::new(40, 10), VoidListener);
            let mut processor: ansi::Processor = ansi::Processor::new();
            for chunk in chunks {
                processor.advance(&mut term, chunk);
            }
            let mut seen = vec![*term.mode() & TermMode::KITTY_KEYBOARD_PROTOCOL];
            for probe in probes {
                processor.advance(&mut term, probe);
                seen.push(*term.mode() & TermMode::KITTY_KEYBOARD_PROTOCOL);
            }
            seen
        }

        let streams: [&[u8]; 5] = [
            // An agent CLI: one push at startup, a pop and push around a
            // subprocess later on.
            b"\x1b[>1uhello\r\n\x1b[<u$ ls\r\n\x1b[>1uback\r\n",
            b"\x1b[>1u\x1b[?1049h\x1b[>7uframe\x1b[=3uframe\x1b[>5u",
            b"\x1b[>1uidle\x1b[=0;1u\x1b[=1;2utyping",
            b"\x1b[>1u\x1b[>3u\x1b[?1049h\x1b[>7u\x1b[<u\x1b[?1049l\x1b[<u\x1b[>2u",
            b"\x1b[>1u\x1bc\x1b[>4uafter a reset",
        ];
        let probes: [&[u8]; 7] = [
            b"\x1b[<u",
            b"\x1b[?1049h",
            b"\x1b[<u",
            b"\x1b[?1049l",
            b"\x1b[<u",
            b"\x1b[<u",
            b"\x1b[>8u",
        ];
        for stream in streams {
            let whole = {
                let mut m = TerminalModes::new();
                m.feed(stream);
                m
            };
            let expected = kitty_after(&[stream], &probes);
            for cut in (0..=stream.len()).filter(|&i| i == stream.len() || stream[i] == 0x1b) {
                let (dropped, kept) = stream.split_at(cut);
                let (mut head, mut ring) = (TerminalModes::new(), TerminalModes::new());
                head.feed(dropped);
                ring.feed(kept);
                // A ring that starts inside an alternate screen it later leaves
                // is replayed onto the primary one — the DEC restore's own
                // limit, not the keyboard's; there is no stack to push it to.
                let restores_alt = whole.is_on(1049) && !ring.is_on(1049);
                if head.is_on(1049) && !restores_alt {
                    continue;
                }
                let prelude = whole.replay_prelude(&head, &ring).unwrap_or_default();
                assert_eq!(
                    kitty_after(&[&prelude, kept], &probes),
                    expected,
                    "cut at {cut} of {:?}, prelude {:?}",
                    String::from_utf8_lossy(stream),
                    String::from_utf8_lossy(&prelude),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_pane_prefixes_nothing() {
        assert!(PaneRoute::Local.is_local());
        assert!(matches!(PaneRoute::default(), PaneRoute::Local));
    }

    #[test]
    fn a_local_spawn_carries_its_workspace_so_the_shell_gets_tty7_ws() {
        let ws = "0d4e1a54-0000-4000-8000-000000000001";

        assert_eq!(
            spawn_workspace(Some(ws), &PaneRoute::Local).as_deref(),
            Some(ws),
            "without this the pane's shell has no $NTTY7_WS and every \
             workspace-scoped CLI verb in it needs an explicit address"
        );

        assert_eq!(
            spawn_workspace(None, &PaneRoute::Local),
            None,
            "a pane outside any workspace must not claim one"
        );
    }

    #[test]
    fn kitty_keyboard_negotiation_reports_the_requested_mode() {
        let config = terminal_config_from_user(&crate::core::config::Config::default());
        assert!(config.kitty_keyboard);

        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        daemon_side.send(DaemonMsg::Output(b"\x1b[>7u\x1b[?u".to_vec()));

        let mut reply = None;
        for _ in 0..200 {
            while let Ok(event) = term.events.try_recv() {
                if let AlacEvent::PtyWrite(text) = event {
                    reply = Some(text);
                }
            }
            if reply.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert_eq!(reply.as_deref(), Some("\x1b[?7u"));
        assert!(
            term.term
                .lock()
                .mode()
                .contains(TermMode::DISAMBIGUATE_ESC_CODES)
        );
    }

    #[test]
    fn deep_keyboard_mode_pushes_leave_the_reader_alive() {
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        let mut payload = b"\x1b[>1u".repeat(4097);
        payload.extend_from_slice(b"\x1b[?u");
        daemon_side.send(DaemonMsg::Output(payload));

        let mut reply = None;
        for _ in 0..200 {
            while let Ok(event) = term.events.try_recv() {
                if let AlacEvent::PtyWrite(text) = event {
                    reply = Some(text);
                }
            }
            if reply.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert_eq!(
            reply.as_deref(),
            Some("\x1b[?1u"),
            "the reader thread must survive a deep mode-push run and still answer queries"
        );
    }

    #[test]
    fn emoji_presentation_sequences_reserve_two_columns() {
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        daemon_side.send(DaemonMsg::Output("\u{2764}\u{FE0F}x".as_bytes().to_vec()));

        let mut row = String::new();
        for _ in 0..200 {
            {
                let t = term.term.lock();
                let grid = t.grid();
                row.clear();
                for col in 0..3usize {
                    row.push(
                        grid[alacritty_terminal::index::Line(0)]
                            [alacritty_terminal::index::Column(col)]
                        .c,
                    );
                }
            }
            if row.contains('x') {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert_eq!(
            row, "\u{2764} x",
            "❤️ must hold two columns (glyph + spacer) before the next glyph"
        );
    }

    #[test]
    fn reader_feeds_local_grid() {
        let size = TermSize::new(80, 24);
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            size,
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        daemon_side.send(DaemonMsg::Output(b"hello".to_vec()));
        daemon_side.send(DaemonMsg::Cwd(PathBuf::from("/tmp/work")));

        let mut got = String::new();
        for _ in 0..200 {
            {
                let t = term.term.lock();
                let grid = t.grid();
                got.clear();
                for col in 0..5usize {
                    let cell = &grid[alacritty_terminal::index::Line(0)]
                        [alacritty_terminal::index::Column(col)];
                    got.push(cell.c);
                }
            }
            if got == "hello" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got, "hello", "reader thread should have fed the grid");

        let mut cwd = None;
        for _ in 0..200 {
            cwd = term.foreground_cwd();
            if cwd.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(cwd, Some(PathBuf::from("/tmp/work")));

        daemon_side.close();
        for _ in 0..200 {
            if term.exited_flag.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(term.exited_flag.load(Ordering::SeqCst));
    }

    #[test]
    fn cursor_style_sequence_overrides_and_resets_to_user_default() {
        use alacritty_terminal::vte::ansi::CursorShape;

        crate::core::config::pin_test_config_dir();
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        let user_config = crate::core::config::Config(crate::core::config::CoreConfig {
            cursor_style: ConfigCursorStyle::Underline,
            ..crate::core::config::CoreConfig::default()
        });
        term.apply_user_config(&user_config);

        let mut shape = term.term.lock().cursor_style().shape;
        assert_eq!(shape, CursorShape::Underline);

        daemon_side.send(DaemonMsg::Output(b"\x1b[6 q".to_vec()));
        for _ in 0..200 {
            shape = term.term.lock().cursor_style().shape;
            if shape == CursorShape::Beam {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(shape, CursorShape::Beam);

        daemon_side.send(DaemonMsg::Output(b"\x1b[0 q".to_vec()));
        for _ in 0..200 {
            shape = term.term.lock().cursor_style().shape;
            if shape == CursorShape::Underline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(shape, CursorShape::Underline);
    }

    /// Feeds `output` to a pane configured with `configured`, one daemon frame
    /// per chunk, and reports the cursor shape once all of it has been parsed.
    fn cursor_shape_after(
        configured: ConfigCursorStyle,
        output: &[&[u8]],
    ) -> alacritty_terminal::vte::ansi::CursorShape {
        use alacritty_terminal::index::{Column, Line};

        // The pane reads config.json when it is built; keep that off the
        // user's real one.
        crate::core::config::pin_test_config_dir();
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        let user_config = crate::core::config::Config(crate::core::config::CoreConfig {
            cursor_style: configured,
            ..crate::core::config::CoreConfig::default()
        });
        term.apply_user_config(&user_config);

        for chunk in output {
            daemon_side.send(DaemonMsg::Output(chunk.to_vec()));
        }
        daemon_side.send(
            // A sentinel painted after everything else: once it is on the grid,
            // every chunk before it has been parsed, so the shape read below is
            // the final one and not a transient match.
            DaemonMsg::Output(b"\x1b[24;1H#".to_vec()),
        );
        for _ in 0..400 {
            if term.term.lock().grid()[Line(23)][Column(0)].c == '#' {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let term = term.term.lock();
        assert_eq!(
            term.grid()[Line(23)][Column(0)].c,
            '#',
            "output never parsed"
        );
        term.cursor_style().shape
    }

    /// #837: both spellings of the DECSCUSR reset land on `cursor_style`.
    #[test]
    fn cursor_style_reset_returns_to_a_configured_bar_or_underline() {
        use alacritty_terminal::vte::ansi::CursorShape;

        for reset in [&b"\x1b[0 q"[..], &b"\x1b[ q"[..]] {
            for (configured, want) in [
                (ConfigCursorStyle::Bar, CursorShape::Beam),
                (ConfigCursorStyle::Underline, CursorShape::Underline),
            ] {
                assert_eq!(
                    cursor_shape_after(configured, &[b"\x1b[6 q", b"\x1b[2 q", reset]),
                    want,
                    "{configured:?} after {reset:?}"
                );
            }
        }
    }

    /// #837: what nvim actually sends on exit under `xterm-256color` is `Se`,
    /// `\e[2 q` — a literal steady block. The command's `D` hands the prompt
    /// its configured cursor back, whether the block arrives in the same
    /// frame as the marks or in one of its own.
    #[test]
    fn a_block_an_exiting_command_left_is_undone_at_its_finish_mark() {
        use alacritty_terminal::vte::ansi::CursorShape;

        for (configured, want) in [
            (ConfigCursorStyle::Bar, CursorShape::Beam),
            (ConfigCursorStyle::Underline, CursorShape::Underline),
        ] {
            assert_eq!(
                cursor_shape_after(
                    configured,
                    &[b"\x1b]133;C;nvim x\x07\x1b[2 q\x1b]133;D;0\x07$ "],
                ),
                want,
                "{configured:?}, one frame"
            );
            assert_eq!(
                cursor_shape_after(
                    configured,
                    &[
                        b"\x1b]133;C;nvim x\x07",
                        b"\x1b[6 q",
                        b"\x1b[2 q\x1b[?1049l",
                        b"\x1b]133;D;0\x07\x1b]133;A\x07$ ",
                    ],
                ),
                want,
                "{configured:?}, split frames"
            );
        }
    }

    #[test]
    fn child_exit_is_distinguished_from_daemon_disconnect() {
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        daemon_side.send(DaemonMsg::Exited { code: Some(0) });
        for _ in 0..200 {
            if term.exited_flag.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(term.exited_flag.load(Ordering::SeqCst));
        assert!(
            term.child_exited(),
            "an Exited frame is a genuine child exit"
        );

        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        daemon_side.close();
        for _ in 0..200 {
            if term.exited_flag.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(term.exited_flag.load(Ordering::SeqCst));
        assert!(
            !term.child_exited(),
            "a disconnect is not a child exit — auto-close must not fire"
        );
    }

    #[test]
    fn stale_mode_resets_target_only_the_dirty_bits() {
        let clean = TermMode::SHOW_CURSOR | TermMode::LINE_WRAP | TermMode::BRACKETED_PASTE;
        assert!(stale_mode_resets(clean).is_empty());

        let hidden = TermMode::LINE_WRAP;
        assert_eq!(stale_mode_resets(hidden), b"\x1b[?25h");

        let residue = TermMode::ALT_SCREEN | TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        let seq = stale_mode_resets(residue);
        let text = String::from_utf8_lossy(&seq).into_owned();
        assert!(text.starts_with("\x1b[?1049l"));
        assert!(text.contains("\x1b[?25h"));
        assert!(text.contains("\x1b[?1002l"));
        assert!(text.contains("\x1b[?1006l"));
        assert!(
            !text.contains("u"),
            "kitty flags are the caller's: {text:?}"
        );

        let kitty = TermMode::SHOW_CURSOR | TermMode::DISAMBIGUATE_ESC_CODES;
        assert!(stale_mode_resets(kitty).is_empty());
    }

    #[test]
    fn prompt_report_scrubs_stale_tui_modes() {
        crate::core::config::pin_test_config_dir();
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        daemon_side.send(DaemonMsg::Output(
            b"\x1b[?1049h\x1b[?25l\x1b[?1002h\x1b[?1006h".to_vec(),
        ));
        daemon_side.send(DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(255),
        });

        let mut mode = TermMode::NONE;
        for _ in 0..200 {
            mode = *term.term.lock().mode();
            let scrubbed = !mode.contains(TermMode::ALT_SCREEN)
                && mode.contains(TermMode::SHOW_CURSOR)
                && !mode.intersects(TermMode::MOUSE_MODE)
                && !mode.contains(TermMode::SGR_MOUSE);
            if scrubbed && term.at_prompt() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !mode.contains(TermMode::ALT_SCREEN),
            "the prompt report must pull the grid off the stranded alt screen"
        );
        assert!(
            mode.contains(TermMode::SHOW_CURSOR),
            "the prompt report must re-show the DECTCEM-hidden cursor"
        );
        assert!(
            !mode.intersects(TermMode::MOUSE_MODE) && !mode.contains(TermMode::SGR_MOUSE),
            "the prompt report must disable stale mouse reporting"
        );
    }

    #[test]
    fn decrqm_probe_reports_sync_update_supported() {
        crate::core::config::pin_test_config_dir();
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        daemon_side.send(DaemonMsg::Output(b"\x1b[?2026$p".to_vec()));

        let mut reply = None;
        for _ in 0..200 {
            while let Ok(ev) = term.events.try_recv() {
                if let AlacEvent::PtyWrite(text) = ev {
                    reply = Some(text);
                }
            }
            if reply.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            reply.as_deref(),
            Some("\x1b[?2026;2$y"),
            "DECRQM ?2026 must be answered as supported (2 = reset)"
        );
    }

    #[test]
    fn an_unrecognized_2031_reply_is_not_queued() {
        crate::core::config::pin_test_config_dir();
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        daemon_side.send(DaemonMsg::Output(b"\x1b[?2031$p".to_vec()));

        let mut writes = Vec::new();
        for _ in 0..200 {
            while let Ok(ev) = term.events.try_recv() {
                if let AlacEvent::PtyWrite(text) = ev {
                    writes.push(text);
                }
            }
            if !writes.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        while let Ok(ev) = term.events.try_recv() {
            if let AlacEvent::PtyWrite(text) = ev {
                writes.push(text);
            }
        }
        assert_eq!(
            writes,
            vec!["\x1b[?2031;2$y".to_string()],
            "the emulator's ?2031;0$y must be dropped before the channel, leaving the mode at the query"
        );
    }

    #[test]
    fn win_size_carries_grid_and_cell_dims() {
        let ws = win_size(TermSize::new(80, 24), 8, 17);
        assert_eq!(ws.cols, 80);
        assert_eq!(ws.rows, 24);
        assert_eq!(ws.cell_w, 8);
        assert_eq!(ws.cell_h, 17);
    }

    #[test]
    fn write_sends_input_frames() {
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        term.write(Vec::<u8>::new());
        assert!(
            daemon_side.try_recv_client().is_err(),
            "an empty write queues nothing"
        );
        term.write(b"echo hi\r".to_vec());

        match daemon_side
            .recv_client(std::time::Duration::from_secs(5))
            .unwrap()
        {
            ClientMsg::Input(bytes) => assert_eq!(bytes, b"echo hi\r"),
            other => panic!("expected Input, got {other:?}"),
        }
    }

    /// Stopping the reader retires it. A keystroke after that must not be
    /// reported as the pane exiting: the quit flag makes the reader leave
    /// without marking the link dead.
    #[test]
    fn a_write_on_a_retired_link_does_not_mark_the_pane_gone() {
        crate::core::config::pin_test_config_dir();
        let (mut term, _daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        term.stop_reader();

        term.write(b"echo hi\r".to_vec());
        assert!(
            !term.exited_flag.load(Ordering::SeqCst),
            "stopping the reader is not the pane exiting"
        );
    }

    #[test]
    fn first_resize_always_syncs_then_dedups() {
        let (mut term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        term.resize(TermSize::new(80, 24), 8, 17);
        match daemon_side
            .recv_client(std::time::Duration::from_secs(5))
            .unwrap()
        {
            ClientMsg::Resize(ws) => assert_eq!((ws.cols, ws.rows), (80, 24)),
            other => panic!("expected the first Resize to be sent, got {other:?}"),
        }

        term.resize(TermSize::new(80, 24), 8, 17);
        term.write(b"marker".to_vec());
        match daemon_side
            .recv_client(std::time::Duration::from_secs(5))
            .unwrap()
        {
            ClientMsg::Input(bytes) => assert_eq!(bytes, b"marker"),
            other => panic!("expected Input (dup resize sends nothing), got {other:?}"),
        }
    }

    #[test]
    fn sync_update_without_esu_flushes_after_the_deadline() {
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        daemon_side.send(DaemonMsg::Output(b"\x1b[?2026habc".to_vec()));

        let mut got = String::new();
        for _ in 0..600 {
            {
                let t = term.term.lock();
                let grid = t.grid();
                got.clear();
                for col in 0..3usize {
                    got.push(
                        grid[alacritty_terminal::index::Line(0)]
                            [alacritty_terminal::index::Column(col)]
                        .c,
                    );
                }
            }
            if got == "abc" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got, "abc", "dangling BSU must flush on the sync deadline");
    }

    #[test]
    fn resize_updates_size_and_notifies_daemon() {
        let (mut term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        term.resize(TermSize::new(100, 40), 9, 18);
        assert_eq!(term.size(), TermSize::new(100, 40));
        match daemon_side
            .recv_client(std::time::Duration::from_secs(5))
            .unwrap()
        {
            ClientMsg::Resize(ws) => {
                assert_eq!((ws.cols, ws.rows, ws.cell_w, ws.cell_h), (100, 40, 9, 18));
            }
            other => panic!("expected Resize, got {other:?}"),
        }
    }

    #[test]
    fn echoed_resize_defers_the_grid_reflow_to_the_daemons_size_frame() {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line};

        let (mut term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        // A remote route whose host advertised the echo. The local daemon's
        // answer rides the same gate through `local_daemon_supports`, so this
        // covers the deferral for both.
        term.direct = true;

        term.resize(TermSize::new(120, 30), 8, 17);
        assert!(matches!(
            daemon_side.recv_client(std::time::Duration::from_secs(5)).unwrap(),
            ClientMsg::Resize(ws) if ws.cols == 120 && ws.rows == 30
        ));
        assert_eq!(
            term.term.lock().columns(),
            80,
            "the grid keeps the old geometry until the daemon's echo arrives"
        );

        daemon_side.send(
            // Old-width bytes still in flight ahead of the echo: a CUP to column
            // 100 must clamp on the 80-col grid. The same addressing after the
            // echo reaches the real column on the 120-col grid.
            DaemonMsg::Output(b"\x1b[1;100HA".to_vec()),
        );
        daemon_side.send(DaemonMsg::Size(WinSize {
            cols: 120,
            rows: 30,
            cell_w: 8,
            cell_h: 17,
        }));
        daemon_side.send(DaemonMsg::Output(b"\x1b[2;100HB".to_vec()));

        for _ in 0..400 {
            {
                let t = term.term.lock();
                if t.columns() == 120 && t.grid()[Line(1)][Column(99)].c == 'B' {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let t = term.term.lock();
        let grid = t.grid();
        assert_eq!(
            grid[Line(0)][Column(79)].c,
            'A',
            "bytes queued before the echo parse at the old width"
        );
        assert_eq!(
            grid[Line(1)][Column(99)].c,
            'B',
            "bytes after the echo parse at the new width"
        );
    }

    /// A pane that echoes every resize. The test plays the daemon on the
    /// in-memory link. The switch tests build the same route against a real pane.
    fn echoing_pair() -> (RemoteTerminal, TestLink) {
        let (mut term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            PtySource::for_route(&PaneRoute::Local),
        );
        term.direct = true;
        (term, daemon_side)
    }

    /// The geometry of the next resize the pane sent, if it sent one at all.
    fn next_resize(daemon_side: &TestLink) -> Option<(u16, u16)> {
        match daemon_side.recv_client(std::time::Duration::from_millis(150)) {
            Ok(ClientMsg::Resize(ws)) => Some((ws.cols, ws.rows)),
            Ok(other) => panic!("expected a Resize, got {other:?}"),
            Err(_) => None,
        }
    }

    /// Makes the last resize old enough that an echo for it is no longer
    /// expected, without the test sitting out the grace for real.
    fn outlive_the_echo_grace(term: &mut RemoteTerminal) {
        term.resize_sent_at = std::time::Instant::now()
            .checked_sub(RESIZE_ECHO_GRACE + std::time::Duration::from_millis(1));
    }

    fn wait_for_columns(term: &RemoteTerminal, cols: usize) {
        use alacritty_terminal::grid::Dimensions as _;
        for _ in 0..400 {
            if term.term.lock().columns() == cols {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the grid never reached {cols} columns");
    }

    /// #893: a pane came back from sleep or a workspace switch painting a grid
    /// shorter than its bounds, and stayed that way until a divider drag. On an
    /// echoing route the grid only moves when the daemon's `Size` comes back,
    /// and the early-out trusted the request: once asked for, the same size
    /// was never sent again, so a request or echo that went missing stranded
    /// the grid — and the child — at the old geometry for good.
    #[test]
    fn an_echoed_resize_whose_echo_never_came_is_asked_for_again() {
        use alacritty_terminal::grid::Dimensions as _;

        let (mut term, daemon_side) = echoing_pair();
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(next_resize(&daemon_side), Some((120, 30)));

        // Every frame asks again. While the echo can still be on its way that
        // is not a reason to resend.
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&daemon_side),
            None,
            "an echo still in flight must not be chased with duplicates"
        );

        // The echo never comes. Past the grace the grid is still 80x24, and
        // the frame that notices must ask for the size again.
        outlive_the_echo_grace(&mut term);
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&daemon_side),
            Some((120, 30)),
            "a resize whose echo never arrived was never retried, so the grid \
             stays at the old size until the layout asks for a different one"
        );
        assert_eq!(
            term.term.lock().columns(),
            80,
            "the retry still waits for the echo rather than reflowing early"
        );

        daemon_side.send(
            // Once the echo lands the size is settled, however old the request.
            DaemonMsg::Size(WinSize {
                cols: 120,
                rows: 30,
                cell_w: 8,
                cell_h: 17,
            }),
        );
        wait_for_columns(&term, 120);
        assert_eq!(term.term.lock().screen_lines(), 30);
        outlive_the_echo_grace(&mut term);
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&daemon_side),
            None,
            "a grid that reached the requested size must not be resized again"
        );
    }

    /// The other way the grid and the request can part on an echoing route: a
    /// `Size` frame that lands after the echo moves the grid off the size the
    /// layout asked for. The non-echoing path already re-asserts there
    /// (`layout_resize_reasserts_geometry_after_a_late_size_frame`); the
    /// echoing one skipped it, since the request itself had not changed.
    #[test]
    fn an_echoed_resize_reasserts_geometry_after_a_late_size_frame() {
        let (mut term, daemon_side) = echoing_pair();
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(next_resize(&daemon_side), Some((120, 30)));
        for (cols, rows) in [(120, 30), (100, 12)] {
            daemon_side.send(DaemonMsg::Size(WinSize {
                cols,
                rows,
                cell_w: 8,
                cell_h: 17,
            }));
        }
        wait_for_columns(&term, 100);

        outlive_the_echo_grace(&mut term);
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&daemon_side),
            Some((120, 30)),
            "a grid knocked off the requested size was left there"
        );
    }

    #[test]
    fn at_prompt_stays_false_while_shell_integration_is_inactive() {
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        assert!(!term.shell_active(), "no report yet → integration inactive");

        daemon_side.send(DaemonMsg::Prompt {
            active: false,
            at_prompt: true,
            last_exit: None,
        });
        daemon_side.send(DaemonMsg::Output(b"m".to_vec()));

        let mut synced = false;
        for _ in 0..200 {
            let c = term.term.lock().grid()[alacritty_terminal::index::Line(0)]
                [alacritty_terminal::index::Column(0)]
            .c;
            if c == 'm' {
                synced = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(synced, "reader should have applied both frames");
        assert!(!term.shell_active());
        assert!(!term.at_prompt(), "inactive shell must gate at_prompt off");
    }

    #[test]
    fn at_prompt_follows_daemon_prompt_reports() {
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        assert!(!term.at_prompt());

        daemon_side.send(DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        });

        let mut at = false;
        for _ in 0..200 {
            if term.at_prompt() {
                at = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(at, "at_prompt should become true after the Prompt report");
    }

    /// #654, end to end on the reader: a shell at its prompt (the daemon's
    /// `Prompt` report) whose clock sits one column short of the edge, the
    /// pane narrowed by several columns at a time and widened back, each step
    /// followed by zsh's SIGWINCH redraw. Every live echo has to hand the
    /// prompt back before reflowing, or each narrowing strands the head of the
    /// old prompt and the widening joins them into a line of fragments.
    #[test]
    fn a_live_size_echo_at_a_prompt_leaves_one_prompt() {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line};

        crate::core::config::pin_test_config_dir();
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(98, 12),
            crate::terminal::remote::PtySource::Raw,
        );
        // zsh 5.9's redraw for `PROMPT='[~] '` and an `RPROMPT` clock.
        let redraw = |cols: u16, clock: &str| {
            format!(
                "\r\r\x1b[0m\x1b[J[~] \x1b[K\x1b[{}C{clock}\x1b[{}D",
                cols - 13,
                cols - 5
            )
            .into_bytes()
        };
        daemon_side.send(DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        });
        daemon_side.send(DaemonMsg::Output(redraw(98, "16:46:20")));
        for cols in [92, 86, 92, 98] {
            daemon_side.send(DaemonMsg::Size(WinSize {
                cols,
                rows: 12,
                cell_w: 8,
                cell_h: 17,
            }));
            let clock = if cols == 98 { "16:46:33" } else { "16:46:2x" };
            daemon_side.send(DaemonMsg::Output(redraw(cols, clock)));
        }

        let text = || {
            let t = term.term.lock();
            let grid = t.grid();
            let top = -(grid.history_size() as i32);
            (top..grid.screen_lines() as i32)
                .map(|line| {
                    (0..grid.columns())
                        .map(|col| grid[Line(line)][Column(col)].c)
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .filter(|row| !row.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        };
        for _ in 0..400 {
            if term.term.lock().columns() == 98 && text().contains("16:46:33") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let text = text();
        assert!(
            text.contains("16:46:33"),
            "the last redraw never landed:\n{text}"
        );
        assert_eq!(
            text.matches("[~]").count(),
            1,
            "a narrowing stranded part of an old prompt:\n{text}"
        );
    }

    #[test]
    fn foreground_agent_follows_daemon_agent_reports() {
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        assert_eq!(term.foreground_agent(), None, "none before any report");

        let poll = |want: Option<CLIAgent>| {
            for _ in 0..200 {
                if term.foreground_agent() == want {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        daemon_side.send(DaemonMsg::Agent(Some(CLIAgent::Claude)));
        assert!(poll(Some(CLIAgent::Claude)), "agent report should surface");

        daemon_side.send(DaemonMsg::Agent(None));
        assert!(poll(None), "agent exit should clear it");
    }

    #[test]
    fn agent_session_follows_daemon_status_reports() {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus};

        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        assert_eq!(term.agent_session(), None, "none before any report");

        let poll = |want: &dyn Fn(Option<AgentSessionState>) -> bool| {
            for _ in 0..200 {
                if want(term.agent_session()) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        daemon_side.send(DaemonMsg::AgentStatus(Some(AgentSessionState {
            status: AgentStatus::Waiting,
            message: Some("Claude needs your permission".into()),
            session_id: Some("sid-1".into()),
            launch_argv: None,
            rich: true,
            cwd: None,
            activity: 0,
            turns: 0,
            inferred: false,
            readout: Default::default(),
            since_ms: None,
        })));
        assert!(
            poll(&|s| s.is_some_and(|s| s.status == AgentStatus::Waiting
                && s.session_id.as_deref() == Some("sid-1")
                && s.rich)),
            "status report should surface with message + session id"
        );

        daemon_side.send(DaemonMsg::AgentStatus(None));
        assert!(poll(&|s| s.is_none()), "a None report clears the session");
    }

    #[test]
    fn the_spawn_directory_answers_until_the_shell_reports_its_own() {
        crate::core::config::pin_test_config_dir();
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        let poll = |want: Option<&str>| {
            let want = want.map(PathBuf::from);
            for _ in 0..200 {
                if term.foreground_cwd() == want {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        assert_eq!(term.foreground_cwd(), None, "nothing known before the seed");
        term.seed_cwd(Some(PathBuf::from("/repo/tty7")));
        assert_eq!(
            term.foreground_cwd(),
            Some(PathBuf::from("/repo/tty7")),
            "the sidebar can group this pane without waiting for the shell"
        );

        daemon_side.send(DaemonMsg::Cwd(PathBuf::from("/repo/tty7/crates")));
        assert!(
            poll(Some("/repo/tty7/crates")),
            "the shell's own report replaces the seed"
        );
    }

    #[test]
    fn a_shell_report_that_beat_the_seed_wins() {
        crate::core::config::pin_test_config_dir();
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );

        daemon_side.send(DaemonMsg::Cwd(PathBuf::from("/somewhere/else")));
        for _ in 0..200 {
            if term.foreground_cwd().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        term.seed_cwd(Some(PathBuf::from("/repo/tty7")));
        assert_eq!(
            term.foreground_cwd(),
            Some(PathBuf::from("/somewhere/else")),
            "where the shell actually landed beats where we asked it to"
        );
    }

    #[test]
    fn the_running_command_is_the_line_the_shell_marked_and_ends_with_it() {
        crate::core::config::pin_test_config_dir();
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        let poll = |want: &str| {
            for _ in 0..200 {
                if term.running_command() == want {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };
        assert_eq!(term.running_command(), "", "nothing runs before a mark");

        // The escaping is the integration's; this side stores it verbatim and
        // the reader of the field undoes it.
        daemon_side.send(DaemonMsg::Output(
            b"\x1b]133;C;printf '100%25'\x07".to_vec(),
        ));
        assert!(
            poll("printf '100%25'"),
            "C should record the line it carried, got {:?}",
            term.running_command()
        );

        daemon_side.send(
            // Back at a prompt the command is over — holding the name would make
            // the next close question ask about something that already finished.
            DaemonMsg::Output(b"\x1b]133;B\x07".to_vec()),
        );
        assert!(poll(""), "a new prompt clears the running command");

        daemon_side.send(
            // A bare `C` starts a command without naming it (the PowerShell path).
            // It must not leave the previous name standing.
            DaemonMsg::Output(b"\x1b]133;C;cargo build\x07".to_vec()),
        );
        assert!(poll("cargo build"));
        daemon_side.send(DaemonMsg::Output(b"\x1b]133;C\x07".to_vec()));
        assert!(poll(""), "an unnamed command start does not inherit a name");
    }

    #[test]
    fn shell_vi_mode_follows_live_prompt_mode_marks_without_disarming_zle() {
        let (term, daemon_side) = RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        let poll = |vi: bool, zle: bool| {
            for _ in 0..200 {
                if term.shell_vi_mode() == vi && term.zle_reading() == zle {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        assert!(!term.shell_vi_mode(), "conservative false before any mark");
        assert!(!term.zle_reading(), "zle also starts false");

        daemon_side.send(DaemonMsg::Output(b"\x1b]133;B\x07".to_vec()));
        assert!(poll(false, true), "B should arm zle only");

        daemon_side.send(DaemonMsg::Output(b"\x1b]133;V;1\x07".to_vec()));
        assert!(
            poll(true, true),
            "V;1 should set shell vi-mode without disarming zle"
        );

        daemon_side.send(DaemonMsg::Output(b"\x1b]133;V;0\x07".to_vec()));
        assert!(
            poll(false, true),
            "V;0 should clear shell vi-mode without disarming zle"
        );

        daemon_side.send(DaemonMsg::Output(b"\x1b]133;C\x07".to_vec()));
        assert!(poll(false, false), "C still disarms zle");
    }
}

#[cfg(test)]
mod osc_tests {
    use super::{MAX_OSC_NOTES, Note, OscNotifyScanner, VecDeque, parse_osc_notification};

    fn scan(chunks: &[&[u8]]) -> Vec<Note> {
        let mut s = OscNotifyScanner::default();
        let mut out = VecDeque::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        out.into()
    }

    /// A burst of notifications shows only its newest few, once.
    #[test]
    fn a_burst_of_notes_is_handed_over_as_its_newest_few() {
        use crate::daemon::protocol::DaemonMsg;
        use crate::terminal::size::TermSize;

        crate::core::config::pin_test_config_dir();
        let (term, daemon) = super::RemoteTerminal::from_pipe(
            TermSize::new(80, 24),
            crate::terminal::remote::PtySource::for_route(&crate::terminal::PaneRoute::Local),
        );
        let burst: Vec<u8> = (0..40)
            .flat_map(|n| format!("\x1b]9;note {n}\x07").into_bytes())
            .collect();
        daemon.send(DaemonMsg::Output(burst));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let taken = loop {
            let batch = term.take_osc_notes();
            if !batch.is_empty() || std::time::Instant::now() > deadline {
                break batch;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let bodies: Vec<_> = taken.into_iter().map(|(_, body)| body).collect();
        assert_eq!(bodies, ["note 37", "note 38", "note 39"]);
        assert!(term.take_osc_notes().is_empty(), "the rest were dropped");
    }

    #[test]
    fn an_unread_queue_keeps_only_the_newest_notes() {
        let chunks: Vec<Vec<u8>> = (0..MAX_OSC_NOTES + 2)
            .map(|n| format!("\x1b]9;note {n}\x07").into_bytes())
            .collect();
        let got = scan(&chunks.iter().map(Vec::as_slice).collect::<Vec<_>>());
        assert_eq!(got.len(), MAX_OSC_NOTES);
        assert_eq!(got[0], (None, "note 2".to_string()));
    }

    #[test]
    fn osc9_bel_and_st_terminators() {
        assert_eq!(
            scan(&[b"\x1b]9;Build done\x07"]),
            vec![(None, "Build done".to_string())]
        );
        assert_eq!(
            scan(&[b"\x1b]9;Tests passed\x1b\\"]),
            vec![(None, "Tests passed".to_string())]
        );
    }

    #[test]
    fn osc777_notify_title_and_body() {
        assert_eq!(
            scan(&[b"\x1b]777;notify;Title;Body text\x07"]),
            vec![(Some("Title".to_string()), "Body text".to_string())]
        );
        assert_eq!(
            scan(&[b"\x1b]777;notify;Just a message\x1b\\"]),
            vec![(None, "Just a message".to_string())]
        );
    }

    #[test]
    fn split_across_reads_is_reassembled() {
        assert_eq!(
            scan(&[b"\x1b]9;Hel", b"lo wor", b"ld\x07"]),
            vec![(None, "Hello world".to_string())]
        );
        assert_eq!(
            scan(&[b"\x1b]9;Ping\x1b", b"\\"]),
            vec![(None, "Ping".to_string())]
        );
    }

    #[test]
    fn uninteresting_osc_is_ignored_cheaply() {
        assert_eq!(
            scan(&[b"\x1b]52;c;bWFueSBieXRlcw==\x07\x1b]0;my title\x07"]),
            vec![]
        );
        assert_eq!(
            scan(&[b"\x1b]0;title\x07\x1b]9;After\x07"]),
            vec![(None, "After".to_string())]
        );
    }

    #[test]
    fn claude_codes_kitty_and_ghostty_channels() {
        assert_eq!(
            scan(&[
                b"\x1b]99;i=3:d=0:p=title;Claude Code\x07\x1b]99;i=3:p=body;Done\x07",
                b"\x1b]99;i=3:d=1:a=focus;\x07",
            ]),
            vec![(Some("Claude Code".to_string()), "Done".to_string())]
        );
        assert_eq!(
            scan(&[b"\x1b]777;notify;Claude Code;Done\x07"]),
            vec![(Some("Claude Code".to_string()), "Done".to_string())]
        );
    }

    #[test]
    fn conemu_osc9_subcommands_are_not_notifications() {
        assert_eq!(scan(&[b"\x1b]9;4;1;50\x07"]), vec![]);
        assert_eq!(scan(&[b"\x1b]9;9;/home/u\x07"]), vec![]);
        assert_eq!(scan(&[b"\x1b]9;12\x07"]), vec![], "a prompt mark");
        assert_eq!(scan(&[b"\x1b]9;11;a comment\x07"]), vec![]);
        assert_eq!(scan(&[b"\x1b]9;42\x07"]), vec![(None, "42".to_string())]);
    }

    #[test]
    fn parse_rejects_empty_and_unrelated() {
        let n = &mut Default::default();
        assert_eq!(parse_osc_notification(n, b"9;"), None);
        assert_eq!(parse_osc_notification(n, b"777;notify;"), None);
        assert_eq!(parse_osc_notification(n, b"8;;https://example.com"), None);
    }

    #[test]
    fn resyncs_on_new_osc_after_an_unterminated_one() {
        assert_eq!(
            scan(&[b"\x1b]9;dropped\x1b]9;kept\x07"]),
            vec![(None, "kept".to_string())]
        );
        assert_eq!(
            scan(&[b"\x1b]0;title\x1b]9;After title\x07"]),
            vec![(None, "After title".to_string())]
        );
    }
}
