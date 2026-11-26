use std::borrow::Cow;
#[cfg(test)]
use std::io::Read as _;
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
#[cfg(unix)]
pub(crate) type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub(crate) type Stream = std::net::TcpStream;
use gpui::EntityId;
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
    fn for_route(route: &PaneRoute) -> PtySource {
        let _ = route;
        if cfg!(windows) {
            PtySource::LocalConpty
        } else {
            PtySource::Raw
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaneWorkspace {
    pub workspace: crate::core::session::WorkspaceId,
    pub target: crate::core::session::RemoteTarget,
    /// The name the workspace answers to — its own label, or its machine's
    /// when it has none. A pane keeps answering to this name when its link
    /// dies (#438 gave SSH panes their host here; without this, a workspace
    /// pane fell back to the bare app name and the tab read "tty7 —
    /// disconnected").
    pub label: Option<String>,
}

impl PaneWorkspace {
    pub fn shares_localhost(&self) -> bool {
        matches!(self.target, crate::core::session::RemoteTarget::Wsl { .. })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PaneRoute {
    #[default]
    Local,
}

impl PaneRoute {
    pub fn for_workspace(workspace: Option<&PaneWorkspace>) -> PaneRoute {
        let _ = workspace;
        PaneRoute::Local
    }

    pub fn is_local(&self) -> bool {
        true
    }
}

/// How much unsent input a pane holds before it calls the link lost. A link
/// that is draining never gets near this: the sender thread is parked waiting
/// for work, so the queue holds at most the frames of one burst. Reaching it
/// means nothing on the far side has taken a byte for as long as it takes to
/// type — or paste — four megabytes, which is a dead link, not a slow one.
const MAX_BACKLOG: usize = 4 << 20;

/// How long a teardown lets the sender finish what is already queued before it
/// cuts the socket out from under it. `Detach` is the last frame a pane sends
/// and it is worth a moment: on a draining link the sender is idle, so it goes
/// out in microseconds and this returns at once. On a link that has stopped
/// draining it will never go out at all, and closing the pane must not wait
/// around to discover that — the daemon reads the closed socket as a detach
/// anyway.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(50);

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

/// What the sender thread needs to report a link that stopped taking input.
/// The signals the pane holds, cloned out so the failure can be raised from the
/// thread that actually meets it.
#[derive(Clone)]
struct InputLoss {
    /// Set by the first frame *this* link refused, so the loss is said once
    /// rather than once per keystroke. One per link rather than one per pane: a
    /// relink hands the retired sender's last, doomed write and the new
    /// sender's first real one two different flags, so the dying link cannot
    /// spend the new link's one chance to speak.
    said: Arc<AtomicBool>,
    reader_quit: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    proxy: EventProxy,
}

impl InputLoss {
    fn new(reader_quit: Arc<AtomicBool>, exited: Arc<AtomicBool>, proxy: EventProxy) -> InputLoss {
        InputLoss {
            said: Arc::new(AtomicBool::new(false)),
            reader_quit,
            exited,
            proxy,
        }
    }

    /// The link refused a frame. Every keystroke after the first would say the
    /// same thing, so this side says it once; and unless the reader has been
    /// retired for a relink or a release, the pane is marked exited the way the
    /// reader marks it on EOF — it is the same socket, noticed from the writing
    /// side first — so the window shows the pane as gone instead of taking
    /// input into it that nothing will ever read. The reader still raises its
    /// own `Exit` when it finds the same socket closed; the handler is
    /// idempotent, so a link that is genuinely gone may be reported twice.
    ///
    /// This is hardening for a *closed* link, not the cure for #673: a socket
    /// some process holds open and never reads accepts writes into its send
    /// buffer, so they succeed and vanish until the buffer fills, and nothing
    /// here fires until the backlog bound does. What stops that pane existing
    /// at all is `attach_on` refusing to call a silent `Attach` attached.
    fn note(&self, err: &std::io::Error) {
        if self.said.swap(true, Ordering::SeqCst) {
            return;
        }
        log::warn!("the daemon link stopped taking this pane's input: {err}");
        if self.reader_quit.load(Ordering::SeqCst) {
            return;
        }
        self.exited.store(true, Ordering::SeqCst);
        self.proxy.send_event(AlacEvent::Wakeup);
        self.proxy.send_event(AlacEvent::Exit);
    }
}

#[derive(Default)]
struct SendQueue {
    frames: VecDeque<Vec<u8>>,
    /// Bytes queued but not yet on the wire — including the batch the sender
    /// currently holds, which is why this is not just `frames`' total. That
    /// batch is exactly what a stalled link parks in, so leaving it out would
    /// mean the backlog bound could never be reached.
    bytes: usize,
    /// How much of `bytes` belongs to frames that were over the whole bound on
    /// their own, and so were let through on their own terms. The bound is
    /// raised by exactly this while they are outstanding, and put back the
    /// moment the queue empties — otherwise one big paste would leave every
    /// keystroke behind it looking like a dead link.
    oversize: usize,
    /// Set by teardown, or by a sender that has given up: either way the queue
    /// takes nothing more. Teardown's sender writes what is left first.
    closing: bool,
    /// Set by the sender once there is nothing left to write. Teardown waits
    /// on this for `DRAIN_GRACE`, no longer.
    drained: bool,
}

/// A pane's writing half, moved onto a thread of its own.
///
/// The socket underneath is blocking and has no write timeout, so a peer that
/// stops reading parks `write(2)` in the kernel until it starts again. Every
/// caller of `RemoteTerminal::write` is a gpui event handler — a keystroke, a
/// paste, a mouse report, a focus change — and one UI thread draws every
/// window, so a park there is every window frozen. On macOS a unix stream gives
/// up after 8K of send buffer, which is about 1400 keystrokes: a single paste.
///
/// So nothing on the UI thread touches the socket. Frames are encoded, queued,
/// and handed to a sender thread that is welcome to park for as long as the far
/// end makes it.
struct LinkWriter {
    /// A second handle on the same socket, kept for `shutdown` alone.
    /// `shutdown(2)` returns at once even while another thread is parked in
    /// `write(2)` on that socket — which is exactly the state teardown has to
    /// be able to break, and exactly what a handle behind the sender's own lock
    /// could not do.
    closer: Stream,
    queue: Arc<(Mutex<SendQueue>, std::sync::Condvar)>,
    loss: InputLoss,
    thread: Option<JoinHandle<()>>,
}

impl LinkWriter {
    fn new(stream: Stream, loss: InputLoss) -> std::io::Result<LinkWriter> {
        let closer = stream.try_clone()?;
        let queue = Arc::new((Mutex::new(SendQueue::default()), std::sync::Condvar::new()));
        let sending = Arc::clone(&queue);
        let sender_loss = loss.clone();
        let thread = std::thread::Builder::new()
            .name("tty7-pane-writer".into())
            .spawn(move || send_loop(stream, sending, sender_loss))?;
        Ok(LinkWriter {
            closer,
            queue,
            loss,
            thread: Some(thread),
        })
    }

    /// Queues a frame. Never blocks: the socket belongs to the sender thread,
    /// and the only thing that happens here is a push onto a `VecDeque`.
    fn send(&self, msg: ClientMsg) {
        let mut frame = Vec::new();
        if let Err(e) = msg.encode(&mut frame) {
            log::warn!("could not encode a frame for this pane's link: {e}");
            return;
        }
        let (lock, wake) = &*self.queue;
        let Ok(mut q) = lock.lock() else { return };
        if q.closing {
            return;
        }
        // A frame over the bound all by itself is not a backlog — a paste is
        // whatever the clipboard holds, and refusing a big one would kill a
        // perfectly healthy pane. Onto an empty queue it goes through anyway,
        // and lifts the bound by its own size for as long as it is outstanding,
        // so what piles up behind it is still held to the same four megabytes.
        // Onto a queue that already has something on it, the ordinary bound
        // applies: one such frame is a paste, a second one arriving before the
        // first has moved is a link that is not moving.
        let oversize = frame.len() > MAX_BACKLOG && q.bytes == 0;
        if !oversize && q.bytes + frame.len() > MAX_BACKLOG + q.oversize {
            // Dropped rather than queued: a backlog this deep is a link nothing
            // is reading, and growing it only trades a frozen window for an
            // exhausted heap. Reported in the same words, and once, as a write
            // the link refuses outright — the pane is gone either way.
            drop(q);
            self.loss.note(&std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("nothing has drained this pane's link for {MAX_BACKLOG} bytes of input"),
            ));
            return;
        }
        if oversize {
            q.oversize = frame.len();
        }
        q.bytes += frame.len();
        q.frames.push_back(frame);
        q.drained = false;
        drop(q);
        wake.notify_one();
    }

    /// Retires the sender and cuts the link. Gives what is queued `DRAIN_GRACE`
    /// to go out — see the constant — and then shuts the socket down whether it
    /// went or not. Never joins: a sender parked on a dead link would take the
    /// UI thread down with it, the same trap `stop_reader` documents.
    ///
    /// Called twice on the way out — `stop_reader` closes the link, then the
    /// field drop closes it again — so the second call has to be free rather
    /// than another `DRAIN_GRACE` spent waiting for a sender that is already
    /// gone. A retired handle is one whose thread has been let go.
    fn close(&mut self) {
        if self.thread.is_none() {
            return;
        }
        let (lock, wake) = &*self.queue;
        if let Ok(mut q) = lock.lock() {
            q.closing = true;
            wake.notify_one();
            if let Ok((waited, _)) = wake.wait_timeout_while(q, DRAIN_GRACE, |q| !q.drained) {
                drop(waited);
            }
        }
        let _ = self.closer.shutdown(std::net::Shutdown::Both);
        drop(self.thread.take());
    }
}

impl Drop for LinkWriter {
    fn drop(&mut self) {
        self.close();
    }
}

fn send_loop(
    mut stream: Stream,
    queue: Arc<(Mutex<SendQueue>, std::sync::Condvar)>,
    loss: InputLoss,
) {
    use std::io::Write as _;
    let (lock, wake) = &*queue;

    // Abandons whatever is still queued and reports the link settled. Wakes a
    // teardown that is inside `DRAIN_GRACE` waiting to hear it: what is left
    // here is never going out, and there is nothing to be gained by making the
    // window sit out the rest of the grace period to find that out. Closes the
    // queue on the way, too — with no thread left to drain it, anything queued
    // after this is just a keystroke held onto until the pane drops.
    let give_up = || {
        if let Ok(mut q) = lock.lock() {
            q.frames.clear();
            q.bytes = 0;
            q.oversize = 0;
            q.closing = true;
            q.drained = true;
            wake.notify_all();
        }
    };

    loop {
        let batch = {
            let Ok(mut q) = lock.lock() else { return };
            loop {
                if !q.frames.is_empty() {
                    break;
                }
                // Nothing left to write, so the link is as flushed as this
                // thread can make it. Said before the `closing` check, so a
                // teardown racing a sender that has already finished hears it
                // rather than waiting out `DRAIN_GRACE` for nothing.
                q.drained = true;
                wake.notify_all();
                if q.closing {
                    return;
                }
                let Ok(next) = wake.wait(q) else { return };
                q = next;
            }
            std::mem::take(&mut q.frames)
        };
        // Counted against the backlog until it is actually out. Discounting it
        // at the moment it left the `VecDeque` would let a sender parked on the
        // first frame of a huge batch hold the whole thing off the books, and
        // the bound the batch is meant to enforce would never be reached.
        let taken: usize = batch.iter().map(Vec::len).sum();
        // Written with the lock released: parking here is the whole point, and
        // a sender holding the queue lock while it parked would put every
        // `send` — every keystroke — behind the same wait it exists to absorb.
        for frame in batch {
            if let Err(e) = stream.write_all(&frame) {
                loss.note(&e);
                give_up();
                return;
            }
        }
        if let Err(e) = stream.flush() {
            loss.note(&e);
            give_up();
            return;
        }
        if let Ok(mut q) = lock.lock() {
            q.bytes = q.bytes.saturating_sub(taken);
            if q.bytes == 0 {
                q.oversize = 0;
            }
        }
    }
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
    Frames(LinkWriter),
    Pane(Arc<PaneHold>),
}

impl Downlink {
    fn send(&self, msg: ClientMsg) {
        match self {
            Downlink::Frames(writer) => writer.send(msg),
            Downlink::Pane(hold) => {
                let Some(pane) = hold.pane.lock().ok().and_then(|guard| guard.clone()) else {
                    return;
                };
                match msg {
                    ClientMsg::Input(bytes) => pane.write_input(&bytes),
                    ClientMsg::Resize(size) => pane.resize(size),
                }
            }
        }
    }

    fn close(&mut self) {
        if let Downlink::Frames(writer) = self {
            writer.close();
        }
    }

    #[cfg(test)]
    fn shutdown_write(&self) -> std::io::Result<()> {
        match self {
            Downlink::Frames(writer) => writer.closer.shutdown(std::net::Shutdown::Write),
            Downlink::Pane(_) => Ok(()),
        }
    }
}

/// The pane reader's inbox. The live terminal shares a coalescing pipe with
/// the PTY reader; tests and a relink still speak discrete frames.
enum Inbox {
    /// Discrete frames. Built by the tests and a relink; the live pane uses
    /// [`Live`](Self::Live).
    #[cfg_attr(not(test), allow(dead_code))]
    Mpsc(std::sync::mpsc::Receiver<DaemonMsg>),
    Live(tty7_core::daemon::pane::OutputRx),
}

impl Inbox {
    fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<DaemonMsg, std::sync::mpsc::RecvTimeoutError> {
        match self {
            Inbox::Mpsc(rx) => rx.recv_timeout(timeout),
            Inbox::Live(rx) => rx.recv_timeout(timeout),
        }
    }

    fn try_recv(&self) -> Result<DaemonMsg, std::sync::mpsc::TryRecvError> {
        match self {
            Inbox::Mpsc(rx) => rx.try_recv(),
            Inbox::Live(rx) => rx.try_recv(),
        }
    }

    fn recycle(&self, buf: Vec<u8>) {
        if let Inbox::Live(rx) = self {
            rx.recycle(buf);
        }
    }
}

/// Bytes the output gate charged for this frame, including output appended into one buffer.
fn queued_gate_len(msg: &DaemonMsg) -> usize {
    match msg {
        DaemonMsg::Output(bytes) | DaemonMsg::Image(bytes) | DaemonMsg::ClipboardWrite(bytes) => {
            bytes.len()
        }
        _ => 0,
    }
}

#[cfg(test)]
fn start_decoder(stream: Stream, buffered: Vec<u8>) -> std::sync::mpsc::Receiver<DaemonMsg> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("tty7-frame-decode".to_string())
        .spawn(move || decode_frames(stream, buffered, tx))
        .expect("spawn frame decoder");
    rx
}

#[cfg(test)]
fn decode_frames(mut stream: Stream, mut pending: Vec<u8>, tx: std::sync::mpsc::Sender<DaemonMsg>) {
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match crate::daemon::protocol::take_frame(&mut pending) {
            Ok(Some((kind, payload))) => match DaemonMsg::from_frame(kind, payload) {
                Ok(msg) => {
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            },
            Ok(None) => match stream.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => pending.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            },
            Err(_) => return,
        }
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
    /// Tells the *current* reader thread to bow out. Teardown must never wait
    /// for the reader: on Windows, `shutdown()` does not wake a thread parked
    /// in a blocking `read()` on the same socket, so joining it from the UI
    /// thread hangs the whole window whenever the peer has gone silent (the
    /// exact state a zombie SSH leg leaves behind). The reader re-checks this
    /// flag under the term lock before every grid mutation, so once it is set
    /// the abandoned thread can only exit, never write.
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
            Inbox::Live(inbox),
            Some(drain),
            size,
            PtySource::for_route(route),
            None,
            Some(hold),
        )?;
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

    /// A pane on a pty of this machine's own — what the tests build, and what
    /// `spawn_on` narrows with the route it dialled.
    #[cfg(test)]
    pub(super) fn from_stream(stream: Stream, size: TermSize) -> anyhow::Result<Self> {
        Self::from_stream_with(
            stream,
            size,
            Vec::new(),
            PtySource::for_route(&PaneRoute::Local),
        )
    }

    #[cfg(test)]
    pub(super) fn from_stream_with(
        stream: Stream,
        size: TermSize,
        buffered: Vec<u8>,
        pty: PtySource,
    ) -> anyhow::Result<Self> {
        Self::from_stream_parts(stream, size, buffered, pty)
    }

    #[cfg(test)]
    fn from_stream_parts(
        stream: Stream,
        size: TermSize,
        buffered: Vec<u8>,
        pty: PtySource,
    ) -> anyhow::Result<Self> {
        let read_half = stream.try_clone()?;
        let inbox = start_decoder(read_half, buffered);
        Self::assemble(Inbox::Mpsc(inbox), None, size, pty, Some(stream), None)
    }

    fn assemble(
        inbox: Inbox,
        drain: Option<Arc<tty7_core::daemon::pane::OutputGate>>,
        size: TermSize,
        pty: PtySource,
        write_half: Option<Stream>,
        hold: Option<Arc<PaneHold>>,
    ) -> anyhow::Result<Self> {
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

        let link = match (write_half, hold) {
            (Some(stream), _) => Downlink::Frames(LinkWriter::new(
                stream,
                InputLoss::new(reader_quit.clone(), exited_flag.clone(), proxy.clone()),
            )?),
            (None, Some(hold)) => Downlink::Pane(hold),
            (None, None) => {
                return Err(anyhow::anyhow!("a terminal needs a pane or a frame stream"));
            }
        };

        Ok(Self {
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
        })
    }

    pub fn detach_link(&mut self) {
        self.stop_reader();
        self.poll_exited();
    }

    /// Retires the current reader thread without waiting for it. Joining is
    /// not an option here: the callers run on the UI thread, and on Windows
    /// `shutdown()` does not wake a reader parked in a blocking `read()`, so
    /// against a silent peer the join would hang the whole window (the reader
    /// only unblocks when the peer eventually closes — which a zombie SSH leg
    /// never does). Instead the reader is told to quit and abandoned; it wakes
    /// within one read-timeout tick, sees the flag, and exits without ever
    /// touching the grid again.
    fn stop_reader(&mut self) {
        self.reader_quit.store(true, Ordering::SeqCst);
        // The sender owns the socket now, and closing it is its job: `close`
        // gives whatever is queued a brief moment to go out and then shuts the
        // socket down regardless, which is also what wakes the reader.
        self.link.close();
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
                    // parked on a silent link still notices `quit` promptly —
                    // on Windows, `shutdown()` alone would never wake it.
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

    /// Queues a keystroke — or a paste, or a mouse report — for the link.
    ///
    /// Callers are gpui event handlers on the UI thread, so this returns
    /// without touching the socket. A link that has stopped draining is a
    /// problem for the sender thread, not for the window.
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
        // when the resize lands. A socket fixture used by tests does not,
        // unless the test says it is playing that daemon (`direct`).
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

#[cfg(test)]
fn daemon_not_listening(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            )
        })
    })
}

#[cfg(test)]
fn daemon_disconnected_before_spawn_reply(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
            )
        })
    })
}

impl Drop for RemoteTerminal {
    fn drop(&mut self) {
        if let Downlink::Pane(hold) = &self.link {
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

/// Show a desktop notification.
///
/// Naming a `pane` makes the notification clickable where the platform supports
/// it, so activating it reveals that pane. It is an `EntityId` on purpose: the
/// tray dispatch identifies leaves by their gpui entity id, and taking a bare
/// `u64` here is what lets a caller hand over a `pane_id` — a different number,
/// assigned by the daemon — and get a notification that reveals nothing.
///
/// macOS always goes through `macos_notify`, clickable or not. Elsewhere, every
/// other case (no pane, unsupported platform, Windows toast queue full) falls
/// back to the plain `notify-rust` path below, which is why
/// `try_clickable_notification` reports whether it took the job.
pub(crate) fn notify_desktop_for_pane(title: Option<&str>, body: &str, pane: Option<EntityId>) {
    let summary = sanitize_notification_text(title.unwrap_or("ntty7"), NOTIFY_TITLE_MAX);
    let body = sanitize_notification_text(body, NOTIFY_BODY_MAX);

    #[cfg(target_os = "macos")]
    {
        macos_notify::deliver(summary, body, pane.map(|p| p.as_u64()));
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(pane) = pane
            && try_clickable_notification(&summary, &body, pane.as_u64())
        {
            return;
        }

        std::thread::spawn(move || {
            let mut notif = notify_rust::Notification::new();
            notif.summary(&summary).body(&body);
            // Without our own AUMID, the Windows backend falls back to
            // PowerShell's — icon and name included. Only set ours once the
            // shell has indexed a shortcut carrying it: for an AUMID it does
            // not know, `show()` reports success and drops the toast, so the
            // ugly fallback beats the branded one every time we are not sure.
            #[cfg(target_os = "windows")]
            if let Some(app_id) = crate::core::aumid::toast_app_id() {
                notif.app_id(app_id);
            }
            let _ = notif.show();
        });
    }
}

/// Longest title / body we hand to a notification backend.
///
/// The text comes straight off the terminal (OSC 9/777 payloads, window
/// titles, an agent's own status line), so it is attacker-shaped: arbitrarily
/// long, and free to contain control bytes. Every backend truncates in its own
/// ugly way, and on Windows a stray `ESC` makes the toast XML fail to parse and
/// the whole notification disappear — so clamp both here, once, for all paths.
const NOTIFY_TITLE_MAX: usize = 96;
const NOTIFY_BODY_MAX: usize = 512;

fn sanitize_notification_text(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut taken = 0usize;
    for ch in s.chars() {
        // XML 1.0 allows exactly tab / LF / CR out of the control range.
        if ch.is_control() && !matches!(ch, '\t' | '\n' | '\r') {
            continue;
        }
        if taken == max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
        taken += 1;
    }
    out
}

/// What a click on a notification has to carry back: the pane to reveal.
///
/// On macOS it rides in the notification's `identifier`, which is the one
/// field the center hands back verbatim on activation without a dictionary
/// round trip. On Windows it is the toast's `launch` attribute, which comes
/// back as the `invokedArgs` of a COM activation (`core::toast_activator`).
/// Shape: `tty7-pane-<pid>-<leaf>-<seq>`.
///
/// The pid is what makes a stale identifier fail closed. Notifications outlive
/// the process that sent them, and the center hands a click on one of those to
/// whatever process now owns the bundle id — a relaunched tty7, or a second
/// instance running alongside. A leaf id is a gpui entity id, which a fresh
/// process hands out again in the same order, so without the pid that click
/// would reveal an unrelated pane. On Windows that is the *only* kind of click
/// a process COM launched for the purpose can receive. The sequence number
/// keeps two notifications for the same pane apart — the center treats a
/// repeated identifier as "replace the earlier one".
#[cfg(any(target_os = "macos", target_os = "windows", test))]
const NOTIFICATION_ID_PREFIX: &str = "tty7-pane-";

#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn notification_identifier(leaf_id: u64) -> String {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("{NOTIFICATION_ID_PREFIX}{pid}-{leaf_id}-{seq}")
}

#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn leaf_id_in_identifier(identifier: &str) -> Option<u64> {
    let rest = identifier.strip_prefix(NOTIFICATION_ID_PREFIX)?;
    let (pid, rest) = rest.split_once('-')?;
    if pid.parse::<u32>().ok()? != std::process::id() {
        return None;
    }
    let (leaf_id, _seq) = rest.split_once('-')?;
    leaf_id.parse().ok()
}

/// macOS notifications, straight to `NSUserNotificationCenter`.
///
/// This used to go through `mac-notification-sys` with `wait_for_click`, so a
/// click could reveal the pane. The way that crate notices a click is to park
/// the sending thread and, for every notification outstanding, add a repeating
/// 0.5 s timer to the *main* run loop that calls `deliveredNotifications` — a
/// synchronous XPC round trip — to see whether the banner is still there. A
/// banner nobody clicks stays in Notification Center, so its timer never goes
/// away. Sampled with nine outstanding: a fifth of the UI thread inside that
/// XPC, every window juddering, and each agent turn adding one more timer.
///
/// Here the click arrives through the center's delegate, which is what the
/// API is for: no parked thread, no timer, nothing on the main thread until
/// the user actually clicks. The pane rides in the notification's identifier.
///
/// Nothing else may touch the center on this platform. `mac-notification-sys`
/// installs a delegate of its own the first time it sends, the last
/// `setDelegate:` wins, and `notify-rust` is that crate on macOS — so its
/// `show` is never called here, only its `set_application`, which does not
/// install one (that is what names a bare `cargo run` binary to the center).
#[cfg(target_os = "macos")]
#[allow(
    deprecated,
    reason = "UNUserNotificationCenter needs a signed, entitled bundle; NSUserNotification is what a bare binary can use"
)]
mod macos_notify {
    use objc2::rc::{Retained, autoreleasepool};
    use objc2::runtime::ProtocolObject;
    use objc2::{AnyThread, define_class, msg_send};
    use objc2_foundation::{
        NSObject, NSObjectProtocol, NSString, NSUserNotification, NSUserNotificationCenter,
        NSUserNotificationCenterDelegate,
    };

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; `Delegate` has no
        // ivars and no `Drop`.
        #[unsafe(super = NSObject)]
        #[name = "Tty7NotificationDelegate"]
        struct Delegate;

        // SAFETY: `NSObjectProtocol` has no safety requirements.
        unsafe impl NSObjectProtocol for Delegate {}

        // SAFETY: `NSUserNotificationCenterDelegate` has no safety requirements.
        unsafe impl NSUserNotificationCenterDelegate for Delegate {
            /// Runs on the main thread, when the user clicks the banner or the
            /// entry in Notification Center. A channel push, then the clicked
            /// entry is dropped from the center — a one-way message, unlike the
            /// `deliveredNotifications` round trip this module exists to avoid.
            #[unsafe(method(userNotificationCenter:didActivateNotification:))]
            fn did_activate(
                &self,
                center: &NSUserNotificationCenter,
                notification: &NSUserNotification,
            ) {
                if let Some(leaf_id) = notification
                    .identifier()
                    .and_then(|id| super::leaf_id_in_identifier(&id.to_string()))
                {
                    super::reveal_pane(leaf_id);
                }
                center.removeDeliveredNotification(notification);
            }
        }
    );

    fn install_delegate() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // Name the delivering application to the center before it is first
            // touched: the center drops requests from a process with no bundle
            // identity, which is what a bare `cargo run` binary is. This
            // swizzles `-[NSBundle bundleIdentifier]` for the main bundle to
            // `com.github.ntty7` when LaunchServices knows that id (a bundled
            // ntty7.app). It is not tty7's id: a machine that also has
            // tty7.app installed must not have this process's notifications
            // claimed by that bundle. When LaunchServices does not know the
            // id, the swizzle's own default, `com.apple.Terminal`, is what the
            // center sees. It can only be called once per process, so there is
            // no second chance to pass a different name.
            let _ = notify_rust::set_application("com.github.ntty7");
            // SAFETY: `NSObject`'s `init` takes nothing and returns the object.
            let delegate: Retained<Delegate> = unsafe { msg_send![Delegate::alloc(), init] };
            let center = NSUserNotificationCenter::defaultUserNotificationCenter();
            // SAFETY: `delegate` is a `Delegate`, which conforms to the protocol.
            unsafe { center.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
            // The center holds its delegate unretained. Ours lives as long as
            // the process, so it is simply never released.
            std::mem::forget(delegate);
        });
    }

    /// Hands the notification to the center and returns. `deliverNotification:`
    /// is an XPC message, so it goes out on a short-lived thread rather than
    /// from wherever the OSC sequence was parsed — never the UI thread.
    pub(super) fn deliver(title: String, body: String, leaf_id: Option<u64>) {
        std::thread::spawn(move || {
            autoreleasepool(|_| {
                install_delegate();
                let notification = NSUserNotification::new();
                notification.setTitle(Some(&NSString::from_str(&title)));
                notification.setInformativeText(Some(&NSString::from_str(&body)));
                if let Some(leaf_id) = leaf_id {
                    let id = super::notification_identifier(leaf_id);
                    notification.setIdentifier(Some(&NSString::from_str(&id)));
                }
                NSUserNotificationCenter::defaultUserNotificationCenter()
                    .deliverNotification(&notification);
            });
        });
    }
}

#[cfg(all(target_os = "windows", not(test)))]
fn try_clickable_notification(title: &str, body: &str, leaf_id: u64) -> bool {
    // An AUMID the shell has not indexed makes `Show` report success and drops
    // the toast on the floor, so without one the notify-rust path — which
    // deliberately keeps PowerShell's identity rather than lose the toast — is
    // the only one that shows anything at all.
    if crate::core::aumid::toast_app_id().is_none() {
        return false;
    }
    let Some(tx) = toast_thread() else {
        return false;
    };
    tx.try_send((
        title.to_string(),
        body.to_string(),
        notification_identifier(leaf_id),
    ))
    .is_ok()
}

/// Title, body, and the `launch` identifier a click hands back.
#[cfg(all(target_os = "windows", not(test)))]
type ToastRequest = (String, String, String);

/// Toasts are built, shown and kept on one dedicated thread.
///
/// A click reaches us two ways. The one that always works is COM activation
/// (`core::toast_activator`): the shell calls our registered class with the
/// toast's `launch` string, whether the click lands on the popup or on the
/// toast in notification history, and whether or not this process is still
/// the one that sent it. The other is the toast object's own `Activated`
/// event, which fires only for the popup and only while the object is alive
/// here — it is kept as the fallback for a shortcut that predates the toast
/// activator (an all-users install from an older installer, which we cannot
/// rewrite unelevated). Keeping the objects on a single thread also means the
/// WinRT objects never cross a thread boundary.
#[cfg(all(target_os = "windows", not(test)))]
fn toast_thread() -> Option<&'static std::sync::mpsc::SyncSender<ToastRequest>> {
    use std::sync::OnceLock;
    use std::sync::mpsc::sync_channel;

    static TX: OnceLock<Option<std::sync::mpsc::SyncSender<ToastRequest>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = sync_channel::<ToastRequest>(32);
        std::thread::Builder::new()
            .name("tty7-toasts".into())
            .spawn(move || toast_loop(rx))
            .map_err(|e| log::warn!("failed to start the toast thread: {e}"))
            .ok()
            .map(|_| tx)
    })
    .as_ref()
}

#[cfg(all(target_os = "windows", not(test)))]
fn toast_loop(rx: std::sync::mpsc::Receiver<ToastRequest>) {
    use std::collections::VecDeque;
    use windows::UI::Notifications::ToastNotification;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    // MTA lets the Activated callback run on a thread-pool thread without a
    // message loop of our own.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    // How many recent popups keep their `Activated` fallback. Dropping the
    // oldest is what bounds the memory here; the popup is long gone by then,
    // and the COM path does not depend on these objects at all.
    const MAX_LIVE_TOASTS: usize = 32;
    let mut live: VecDeque<ToastNotification> = VecDeque::new();

    while let Ok((title, body, launch)) = rx.recv() {
        if let Some(toast) = show_windows_toast(&title, &body, launch) {
            if live.len() == MAX_LIVE_TOASTS {
                live.pop_front();
            }
            live.push_back(toast);
        }
    }
}

/// The toast's XML. `launch` is what Windows hands back on a click — as the
/// `invokedArgs` of a COM activation, and as the `Arguments` of the in-process
/// `Activated` event.
#[cfg(any(target_os = "windows", test))]
fn toast_xml(title: &str, body: &str, launch: &str) -> String {
    format!(
        r#"<toast launch="{}" activationType="foreground">
  <visual>
    <binding template="ToastText02">
      <text id="1">{}</text>
      <text id="2">{}</text>
    </binding>
  </visual>
</toast>"#,
        xml_escape(launch),
        xml_escape(title),
        xml_escape(body)
    )
}

#[cfg(all(target_os = "windows", not(test)))]
fn show_windows_toast(
    title: &str,
    body: &str,
    launch: String,
) -> Option<windows::UI::Notifications::ToastNotification> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::core::IInspectable;

    let app_id = crate::core::aumid::toast_app_id()?;

    let xml = toast_xml(title, body, &launch);

    let doc = match XmlDocument::new() {
        Ok(d) => {
            if let Err(e) = d.LoadXml(&xml.into()) {
                log::warn!("failed to load toast xml: {e}");
                return None;
            }
            d
        }
        Err(e) => {
            log::warn!("failed to create toast xml document: {e}");
            return None;
        }
    };

    let toast = match ToastNotification::CreateToastNotification(&doc) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("failed to create toast notification: {e}");
            return None;
        }
    };

    if let Err(e) = toast.Activated(&TypedEventHandler::new(
        move |_sender: &Option<ToastNotification>, _args: &Option<IInspectable>| {
            toast_activated(&launch);
            Ok(())
        },
    )) {
        log::warn!("failed to register toast activated handler: {e}");
    }

    let notifier = match ToastNotificationManager::CreateToastNotifierWithId(&app_id.into()) {
        Ok(n) => n,
        Err(e) => {
            log::warn!("failed to create toast notifier: {e}");
            return None;
        }
    };

    if let Err(e) = notifier.Show(&toast) {
        log::warn!("failed to show toast: {e}");
        return None;
    }

    Some(toast)
}

/// What a click on a Windows toast asks for. A pane this process notified
/// about is revealed; anything else — a toast from an earlier tty7, one
/// without a pane, a launch string we do not recognize — still brings tty7
/// forward, which is what the user clicked a tty7 toast for.
/// What a toast click used to ask the tray to do. The tray is gone, so a
/// click is recorded and otherwise ignored; the variants stay so the launch
/// string is still parsed.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, PartialEq, Eq)]
enum ToastClick {
    RevealPane { leaf_id: u64 },
    ShowWindow,
}

#[cfg(any(target_os = "windows", test))]
fn toast_click_action(launch: &str) -> ToastClick {
    match leaf_id_in_identifier(launch) {
        Some(leaf_id) => ToastClick::RevealPane { leaf_id },
        None => ToastClick::ShowWindow,
    }
}

/// The last few `launch` strings acted on. A click on the popup can arrive
/// twice — through COM and through the toast object's `Activated` event — and
/// must reveal once. Identifiers are unique per toast, so a repeat is that
/// same click; an empty one (a toast without a pane) is never deduplicated.
#[cfg(any(target_os = "windows", test))]
#[derive(Default)]
struct RecentActivations(std::collections::VecDeque<String>);

#[cfg(any(target_os = "windows", test))]
impl RecentActivations {
    const KEEP: usize = 8;

    fn first_time(&mut self, launch: &str) -> bool {
        if launch.is_empty() {
            return true;
        }
        if self.0.iter().any(|seen| seen == launch) {
            return false;
        }
        if self.0.len() == Self::KEEP {
            self.0.pop_front();
        }
        self.0.push_back(launch.to_string());
        true
    }
}

/// A click on a tty7 toast, from whichever path delivered it. Runs on a COM or
/// WinRT callback thread, so it only touches the tray channel — queued there
/// when this process has no dispatch loop yet (a launch COM made to deliver
/// the click).
#[cfg(target_os = "windows")]
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn toast_activated(launch: &str) {
    static RECENT: std::sync::Mutex<RecentActivations> =
        std::sync::Mutex::new(RecentActivations(std::collections::VecDeque::new()));
    let first = RECENT
        .lock()
        .map(|mut recent| recent.first_time(launch))
        .unwrap_or(true);
    if first {
        let _ = toast_click_action(launch);
    }
}

/// Ask the tray dispatch loop to bring `leaf_id` to the front. Runs on whatever
/// thread the platform hands the activation to, so it only touches the channel.
#[cfg(target_os = "macos")]
fn reveal_pane(leaf_id: u64) {
    // Click-to-reveal went with the tray. The banner still dismisses.
    let _ = leaf_id;
}

#[cfg(not(any(target_os = "macos", all(target_os = "windows", not(test)))))]
fn try_clickable_notification(_title: &str, _body: &str, _leaf_id: u64) -> bool {
    // Linux notifications go through notify-rust; click-to-reveal would need a
    // D-Bus action listener of its own.
    false
}

#[cfg(any(target_os = "windows", test))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod notification_tests {
    use super::*;

    #[test]
    fn a_notification_identifier_carries_its_pane_back_out() {
        let id = notification_identifier(42);
        assert_eq!(leaf_id_in_identifier(&id), Some(42));
    }

    #[test]
    fn two_notifications_for_one_pane_do_not_replace_each_other() {
        // The center replaces a delivered notification whose identifier repeats.
        assert_ne!(notification_identifier(7), notification_identifier(7));
    }

    #[test]
    fn a_foreign_identifier_reveals_nothing() {
        let pid = std::process::id();
        for id in [
            String::new(),
            "tty7-pane-".into(),
            "tty7-pane-x-1".into(),
            "tty7-pane-42".into(),
            "other-42-1".into(),
            format!("tty7-pane-{pid}"),
            format!("tty7-pane-{pid}-42"),
            format!("tty7-pane-{pid}-x-1"),
        ] {
            assert_eq!(leaf_id_in_identifier(&id), None, "{id:?}");
        }
    }

    #[test]
    fn a_notification_from_another_process_reveals_nothing() {
        // Notifications outlive the process that sent them, and gpui hands out
        // the same entity ids again in a fresh process. A stale click must not
        // land on whatever pane holds that id now.
        let other_pid = std::process::id().wrapping_add(1);
        let stale = format!("{NOTIFICATION_ID_PREFIX}{other_pid}-42-0");
        assert_eq!(leaf_id_in_identifier(&stale), None);
    }

    #[test]
    fn sanitizing_drops_control_bytes_but_keeps_line_breaks() {
        let raw = "build \x1b[31mfailed\x07\nsee log\ttail";
        assert_eq!(
            sanitize_notification_text(raw, NOTIFY_BODY_MAX),
            "build [31mfailed\nsee log\ttail"
        );
    }

    #[test]
    fn sanitizing_clamps_by_chars_not_bytes() {
        let out = sanitize_notification_text(&"账".repeat(200), 8);
        assert_eq!(out, format!("{}…", "账".repeat(8)));
        assert_eq!(sanitize_notification_text("short", 8), "short");
    }

    /// Pulls the `launch` attribute back out the way the shell would see it.
    fn launch_attribute(xml: &str) -> String {
        let start = xml
            .find(r#"launch=""#)
            .expect("toast has a launch attribute")
            + 8;
        let len = xml[start..].find('"').expect("closed attribute");
        xml[start..start + len].to_string()
    }

    /// #1179: a toast clicked from notification history is only reported over
    /// COM, as the toast's `launch` string. Without one the pane is lost.
    #[test]
    fn a_toast_carries_its_pane_in_the_launch_attribute() {
        let xml = toast_xml("title", "body", &notification_identifier(42));
        assert_eq!(
            toast_click_action(&launch_attribute(&xml)),
            ToastClick::RevealPane { leaf_id: 42 }
        );
    }

    #[test]
    fn a_toast_from_another_process_only_shows_the_window() {
        let other_pid = std::process::id().wrapping_add(1);
        let stale = format!("{NOTIFICATION_ID_PREFIX}{other_pid}-42-0");
        let xml = toast_xml("title", "body", &stale);
        assert_eq!(
            toast_click_action(&launch_attribute(&xml)),
            ToastClick::ShowWindow
        );
        assert_eq!(toast_click_action(""), ToastClick::ShowWindow);
    }

    #[test]
    fn one_click_delivered_twice_reveals_once() {
        let mut recent = RecentActivations::default();
        let a = notification_identifier(1);
        let b = notification_identifier(1);
        assert!(recent.first_time(&a));
        assert!(!recent.first_time(&a));
        assert!(recent.first_time(&b));
        // Toasts without a pane all share the empty launch string.
        assert!(recent.first_time(""));
        assert!(recent.first_time(""));
    }

    /// The real WinRT XML parser must accept the document and hand the launch
    /// string back verbatim. Parses only; shows nothing.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_parses_the_toast_and_keeps_its_launch_string() {
        use windows::Data::Xml::Dom::XmlDocument;
        let id = notification_identifier(7);
        let doc = XmlDocument::new().expect("XmlDocument");
        doc.LoadXml(&toast_xml("a & <b>", "\"c\" 'd'", &id).into())
            .expect("toast xml parses");
        let launch = doc
            .DocumentElement()
            .and_then(|root| root.GetAttribute(&"launch".into()))
            .expect("launch attribute");
        assert_eq!(launch.to_string(), id);
    }

    #[test]
    fn xml_escaping_covers_every_entity_once() {
        assert_eq!(
            xml_escape(r#"a & b < c > "d" 'e'"#),
            "a &amp; b &lt; c &gt; &quot;d&quot; &apos;e&apos;"
        );
    }
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

    pub(super) fn socket_pair() -> (Stream, Stream) {
        #[cfg(unix)]
        {
            std::os::unix::net::UnixStream::pair().unwrap()
        }
        #[cfg(windows)]
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let client_side = std::net::TcpStream::connect(addr).unwrap();
            let (daemon_side, _) = listener.accept().unwrap();
            (client_side, daemon_side)
        }
    }

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
        let (client_side, mut daemon) = socket_pair();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        for frame in frames {
            frame.encode(&mut daemon).unwrap();
        }
        DaemonMsg::Output(format!("{barrier}\x1b[?u").into_bytes())
            .encode(&mut daemon)
            .unwrap();
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
        drop(daemon);
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    use super::replay_tests::socket_pair as tcp_pair;

    /// On Windows, `shutdown()` does not wake a thread parked in a blocking
    /// `read` on the same socket (it does on unix). `detach_link`,
    /// `adopt_relink`, and `Drop` all shut the writer down and then `join()`
    /// the reader thread, counting on that wake-up — so when the peer stays
    /// silent (a routed pane whose SSH leg went zombie: nothing arrives, no
    /// FIN ever comes), the join blocks its caller, which in production is
    /// the UI thread. This is the whole-window "not responding" hang right
    /// after a remote workspace reconnects.
    #[test]
    fn detach_link_returns_promptly_when_the_peer_stays_silent() {
        crate::core::config::pin_test_config_dir();
        let (client_side, daemon_side) = tcp_pair();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        // Let the reader thread park in read() before tearing down.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            term.detach_link();
            let _ = tx.send(());
        });
        let outcome = rx.recv_timeout(std::time::Duration::from_secs(3));
        drop(daemon_side);
        assert!(
            outcome.is_ok(),
            "detach_link blocked for over 3s against a silent peer: shutdown() \
             did not unblock the reader, so join() hangs the calling thread"
        );
    }
}

/// Issue #430, and #774 for the half of it that was still open.
///
/// Whether the reader puts back a parked cursor is a property of the pty this
/// pane is attached to, not of the platform the client was compiled for — so
/// these drive both answers on every platform, over a real socket pair. Before
/// #774 the decision was `cfg!(windows)`, which meant a Windows client talking
/// to a Linux host repaired a cursor no conhost had parked, and the `wq` typed
/// after vim's `:` landed on the row being edited.
#[cfg(test)]
mod parked_cursor_tests {
    use super::replay_tests::socket_pair;
    use super::*;
    use std::io::Write as _;

    fn terminal_on(pty: PtySource, size: TermSize) -> (RemoteTerminal, Stream) {
        crate::core::config::pin_test_config_dir();
        let (client_side, daemon_side) = socket_pair();
        let term = RemoteTerminal::from_stream_with(client_side, size, Vec::new(), pty)
            .expect("a terminal over a socket pair");
        assert_eq!(term.is_local_conpty(), pty == PtySource::LocalConpty);
        (term, daemon_side)
    }

    /// Feeds one conhost-shaped repaint and reports the cell the cursor ends on,
    /// waiting for the `X` the frame paints so the reader is known to be done.
    fn cursor_after_conpty_frame(pty: PtySource, frame: &[u8]) -> (i32, usize) {
        let (term, mut daemon_side) = terminal_on(pty, TermSize::new(80, 24));

        // Where the TUI put the cursor before conhost repainted over it.
        DaemonMsg::Output(b"\x1b[6;4H".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Output(frame.to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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
        let (term, mut daemon_side) = terminal_on(PtySource::Raw, TermSize::new(20, 11));

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
        DaemonMsg::Output(stream).encode(&mut daemon_side).unwrap();
        daemon_side.flush().unwrap();

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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    #[test]
    fn a_local_pane_prefixes_nothing() {
        assert!(PaneRoute::Local.is_local());
        assert!(matches!(PaneRoute::for_workspace(None), PaneRoute::Local));
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

        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output(b"\x1b[>7u\x1b[?u".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        let mut payload = b"\x1b[>1u".repeat(4097);
        payload.extend_from_slice(b"\x1b[?u");
        DaemonMsg::Output(payload).encode(&mut daemon_side).unwrap();
        daemon_side.flush().unwrap();

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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output("\u{2764}\u{FE0F}x".as_bytes().to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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
    fn spawn_retry_only_for_daemon_disconnects() {
        let eof: anyhow::Error =
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed").into();
        assert!(daemon_disconnected_before_spawn_reply(&eof));

        let refused = anyhow::anyhow!("daemon refused Spawn: configured shell missing");
        assert!(!daemon_disconnected_before_spawn_reply(&refused));
    }

    #[test]
    fn only_a_dead_daemon_is_worth_starting_one_for() {
        let connect_failed = |kind| -> anyhow::Error {
            anyhow::Error::new(std::io::Error::new(kind, "no listener"))
                .context("connect to daemon at /tmp/tty7.sock")
        };
        assert!(daemon_not_listening(&connect_failed(
            std::io::ErrorKind::ConnectionRefused
        )));
        assert!(daemon_not_listening(&connect_failed(
            std::io::ErrorKind::NotFound
        )));

        let refused = anyhow::anyhow!("daemon refused Spawn: configured shell missing");
        assert!(!daemon_not_listening(&refused));
        let eof: anyhow::Error =
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed").into();
        assert!(!daemon_not_listening(&eof));
    }

    #[test]
    fn reader_feeds_local_grid() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();

        let size = TermSize::new(80, 24);
        let term = RemoteTerminal::from_stream(client_side, size).unwrap();

        DaemonMsg::Output(b"hello".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Cwd(PathBuf::from("/tmp/work"))
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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

        drop(daemon_side);
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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        let user_config = crate::core::config::Config(crate::core::config::CoreConfig {
            cursor_style: ConfigCursorStyle::Underline,
            ..crate::core::config::CoreConfig::default()
        });
        term.apply_user_config(&user_config);

        let mut shape = term.term.lock().cursor_style().shape;
        assert_eq!(shape, CursorShape::Underline);

        DaemonMsg::Output(b"\x1b[6 q".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        for _ in 0..200 {
            shape = term.term.lock().cursor_style().shape;
            if shape == CursorShape::Beam {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(shape, CursorShape::Beam);

        DaemonMsg::Output(b"\x1b[0 q".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        let user_config = crate::core::config::Config(crate::core::config::CoreConfig {
            cursor_style: configured,
            ..crate::core::config::CoreConfig::default()
        });
        term.apply_user_config(&user_config);

        for chunk in output {
            DaemonMsg::Output(chunk.to_vec())
                .encode(&mut daemon_side)
                .unwrap();
        }
        // A sentinel painted after everything else: once it is on the grid,
        // every chunk before it has been parsed, so the shape read below is
        // the final one and not a transient match.
        DaemonMsg::Output(b"\x1b[24;1H#".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        DaemonMsg::Exited { code: Some(0) }
            .encode(&mut daemon_side)
            .unwrap();
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

        let (client_side, daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        drop(daemon_side);
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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output(b"\x1b[?1049h\x1b[?25l\x1b[?1002h\x1b[?1006h".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(255),
        }
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();

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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output(b"\x1b[?2026$p".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output(b"\x1b[?2031$p".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        term.write(Vec::<u8>::new());
        term.write(b"echo hi\r".to_vec());

        match ClientMsg::read(&mut daemon_side).unwrap() {
            ClientMsg::Input(bytes) => assert_eq!(bytes, b"echo hi\r"),
            other => panic!("expected Input, got {other:?}"),
        }
    }

    /// A peer that holds the link open and stops reading is what the router
    /// looks like from here when the far end is congested: `copy_bidirectional`
    /// stops draining our half and the send buffer fills. The socket is
    /// blocking and has no write timeout, so `write(2)` parks — and it used to
    /// park on the UI thread, which draws every window. macOS gives a unix
    /// stream 8K, so it took about 1400 keystrokes, or one paste.
    ///
    /// Typing into such a pane must now cost the window nothing at all.
    #[test]
    fn a_link_that_stopped_reading_does_not_park_the_writing_thread() {
        crate::core::config::pin_test_config_dir();
        let (client_side, daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // Far past the 8K that used to be fatal, one keystroke at a time, on
        // this very thread: a park anywhere in here is a frozen window.
        let started = std::time::Instant::now();
        for _ in 0..64 * 1024 {
            term.write(vec![b'x']);
        }
        let spent = started.elapsed();

        drop(daemon_side);
        assert!(
            spent < std::time::Duration::from_secs(1),
            "64K keystrokes into a link nobody is draining took {spent:?} — \
             the caller is a gpui event handler, so this is the UI thread"
        );
    }

    /// The backlog is bounded, and reaching the bound is not a slow link but a
    /// dead one: nothing has taken a byte for four megabytes of typing. Saying
    /// so is what stops the pane quietly swallowing input forever, and it is
    /// said in the same words — and once — as an outright refused write.
    #[test]
    fn a_backlog_nothing_drains_is_reported_as_a_lost_link() {
        crate::core::config::pin_test_config_dir();
        let (client_side, daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // 8K a shot rather than a byte, so this is a few hundred frames and not
        // a few million: the bound is on bytes queued, not on frames.
        //
        // Twice the bound rather than a frame or two past it. The sender does
        // get some of this onto the wire before it parks — as much as the send
        // buffer holds, which is 8K on macOS but a couple hundred K on Linux —
        // and that much is discounted from the backlog. A margin narrower than
        // the widest of those buffers is a test that passes on one platform and
        // not the other.
        let chunk = vec![b'x'; 8 << 10];
        for _ in 0..2 * MAX_BACKLOG / chunk.len() {
            term.write(chunk.clone());
        }

        assert!(
            term.exited_flag.load(Ordering::SeqCst),
            "a link that has taken nothing for {MAX_BACKLOG} bytes is gone, and the pane must say so"
        );
        let mut exits = 0;
        while let Ok(ev) = term.events.try_recv() {
            exits += usize::from(matches!(ev, AlacEvent::Exit));
        }
        assert_eq!(exits, 1, "said once, not once per keystroke");
        drop(daemon_side);
    }

    /// One frame can be larger than the whole backlog bound: a paste is
    /// whatever the clipboard holds. That is not a link nothing is draining,
    /// and a pane must not die of being pasted into — nor may the keystrokes
    /// that follow read as a backlog merely because the paste is still going.
    #[test]
    fn a_paste_larger_than_the_backlog_bound_is_not_a_dead_link() {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // A peer that keeps reading: the link is healthy, just carrying a lot.
        let draining = std::thread::spawn(move || {
            let _ = std::io::copy(&mut daemon_side, &mut std::io::sink());
        });

        term.write(vec![b'x'; MAX_BACKLOG + (1 << 20)]);
        for _ in 0..1024 {
            term.write(vec![b'y']);
        }

        assert!(
            !term.exited_flag.load(Ordering::SeqCst),
            "a five megabyte paste is a paste, not a link that has stopped taking input"
        );
        drop(term);
        draining.join().unwrap();
    }

    /// Input the link refuses used to vanish: `write` threw the error away, so a
    /// pane whose daemon had stopped reading kept taking keystrokes into
    /// nothing. The refusal now marks the pane exited by the reader's own signal
    /// — only our sending half is shut here, so the reader is still parked on
    /// an open receiving half and the writing side is the one that finds out.
    /// (Shutting the peer's receiving half instead is not portable: Linux
    /// answers the next write with EPIPE, macOS buffers it.)
    ///
    /// The refusal is met on the sender thread now, so the pane learns of it a
    /// moment after the keystroke rather than during it. That is the trade the
    /// queue buys: the window never waits on the socket to find out.
    #[test]
    fn a_write_the_link_refuses_marks_the_pane_gone_once() {
        crate::core::config::pin_test_config_dir();
        let (client_side, _daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        term.link.shutdown_write().unwrap();

        term.write(b"echo hi\r".to_vec());
        let noticed = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !term.exited_flag.load(Ordering::SeqCst) && std::time::Instant::now() < noticed {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            term.exited_flag.load(Ordering::SeqCst),
            "a refused Input is the link gone, and the pane has to say so"
        );
        let mut exits = 0;
        while let Ok(ev) = term.events.try_recv() {
            exits += usize::from(matches!(ev, AlacEvent::Exit));
        }
        assert_eq!(
            exits, 1,
            "reported through the same event the reader raises on EOF"
        );

        term.write(b"echo again\r".to_vec());
        while let Ok(ev) = term.events.try_recv() {
            exits += usize::from(matches!(ev, AlacEvent::Exit));
        }
        assert_eq!(exits, 1, "said once, not once per keystroke");
    }

    /// A link retired for a relink takes no more input — `stop_reader` closes
    /// the queue and shuts the socket down — and that must not read as the pane
    /// dying under the swap, for the same reason the retired reader exits
    /// silently. The frame is now turned away at the queue rather than by the
    /// socket, but the pane has to stay alive either way.
    #[test]
    fn a_write_on_a_retired_link_does_not_mark_the_pane_gone() {
        crate::core::config::pin_test_config_dir();
        let (client_side, _daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        term.stop_reader();

        term.write(b"echo hi\r".to_vec());
        assert!(
            !term.exited_flag.load(Ordering::SeqCst),
            "the pane is being relinked or released, not dying"
        );
    }

    #[test]
    fn first_resize_always_syncs_then_dedups() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        term.resize(TermSize::new(80, 24), 8, 17);
        match ClientMsg::read(&mut daemon_side).unwrap() {
            ClientMsg::Resize(ws) => assert_eq!((ws.cols, ws.rows), (80, 24)),
            other => panic!("expected the first Resize to be sent, got {other:?}"),
        }

        term.resize(TermSize::new(80, 24), 8, 17);
        term.write(b"marker".to_vec());
        match ClientMsg::read(&mut daemon_side).unwrap() {
            ClientMsg::Input(bytes) => assert_eq!(bytes, b"marker"),
            other => panic!("expected Input (dup resize sends nothing), got {other:?}"),
        }
    }

    #[test]
    fn sync_update_without_esu_flushes_after_the_deadline() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output(b"\x1b[?2026habc".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        term.resize(TermSize::new(100, 40), 9, 18);
        assert_eq!(term.size(), TermSize::new(100, 40));
        match ClientMsg::read(&mut daemon_side).unwrap() {
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

        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        // A remote route whose host advertised the echo. The local daemon's
        // answer rides the same gate through `local_daemon_supports`, so this
        // covers the deferral for both.
        term.direct = true;

        term.resize(TermSize::new(120, 30), 8, 17);
        assert!(matches!(
            ClientMsg::read(&mut daemon_side).unwrap(),
            ClientMsg::Resize(ws) if ws.cols == 120 && ws.rows == 30
        ));
        assert_eq!(
            term.term.lock().columns(),
            80,
            "the grid keeps the old geometry until the daemon's echo arrives"
        );

        // Old-width bytes still in flight ahead of the echo: a CUP to column
        // 100 must clamp on the 80-col grid. The same addressing after the
        // echo reaches the real column on the 120-col grid.
        DaemonMsg::Output(b"\x1b[1;100HA".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Size(WinSize {
            cols: 120,
            rows: 30,
            cell_w: 8,
            cell_h: 17,
        })
        .encode(&mut daemon_side)
        .unwrap();
        DaemonMsg::Output(b"\x1b[2;100HB".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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

    /// A route whose daemon promised to echo every resize, over a socket pair
    /// the test plays the daemon on. The switch tests build the same route
    /// against a real pane.
    fn echoing_pair() -> (RemoteTerminal, UnixStream) {
        let (client_side, daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        term.direct = true;
        daemon_side
            .set_read_timeout(Some(std::time::Duration::from_millis(150)))
            .unwrap();
        (term, daemon_side)
    }

    /// The geometry of the next frame the pane sent, if it sent one at all.
    fn next_resize(daemon_side: &mut UnixStream) -> Option<(u16, u16)> {
        match ClientMsg::read(daemon_side) {
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

        let (mut term, mut daemon_side) = echoing_pair();
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(next_resize(&mut daemon_side), Some((120, 30)));

        // Every frame asks again. While the echo can still be on its way that
        // is not a reason to resend.
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&mut daemon_side),
            None,
            "an echo still in flight must not be chased with duplicates"
        );

        // The echo never comes. Past the grace the grid is still 80x24, and
        // the frame that notices must ask for the size again.
        outlive_the_echo_grace(&mut term);
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&mut daemon_side),
            Some((120, 30)),
            "a resize whose echo never arrived was never retried, so the grid \
             stays at the old size until the layout asks for a different one"
        );
        assert_eq!(
            term.term.lock().columns(),
            80,
            "the retry still waits for the echo rather than reflowing early"
        );

        // Once the echo lands the size is settled, however old the request.
        DaemonMsg::Size(WinSize {
            cols: 120,
            rows: 30,
            cell_w: 8,
            cell_h: 17,
        })
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();
        wait_for_columns(&term, 120);
        assert_eq!(term.term.lock().screen_lines(), 30);
        outlive_the_echo_grace(&mut term);
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&mut daemon_side),
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
        let (mut term, mut daemon_side) = echoing_pair();
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(next_resize(&mut daemon_side), Some((120, 30)));
        for (cols, rows) in [(120, 30), (100, 12)] {
            DaemonMsg::Size(WinSize {
                cols,
                rows,
                cell_w: 8,
                cell_h: 17,
            })
            .encode(&mut daemon_side)
            .unwrap();
        }
        daemon_side.flush().unwrap();
        wait_for_columns(&term, 100);

        outlive_the_echo_grace(&mut term);
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&mut daemon_side),
            Some((120, 30)),
            "a grid knocked off the requested size was left there"
        );
    }

    #[test]
    fn at_prompt_stays_false_while_shell_integration_is_inactive() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        assert!(!term.shell_active(), "no report yet → integration inactive");

        DaemonMsg::Prompt {
            active: false,
            at_prompt: true,
            last_exit: None,
        }
        .encode(&mut daemon_side)
        .unwrap();
        DaemonMsg::Output(b"m".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        assert!(!term.at_prompt());

        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        }
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();

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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(98, 12)).unwrap();
        // zsh 5.9's redraw for `PROMPT='[~] '` and an `RPROMPT` clock.
        let redraw = |cols: u16, clock: &str| {
            format!(
                "\r\r\x1b[0m\x1b[J[~] \x1b[K\x1b[{}C{clock}\x1b[{}D",
                cols - 13,
                cols - 5
            )
            .into_bytes()
        };
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        }
        .encode(&mut daemon_side)
        .unwrap();
        DaemonMsg::Output(redraw(98, "16:46:20"))
            .encode(&mut daemon_side)
            .unwrap();
        for cols in [92, 86, 92, 98] {
            DaemonMsg::Size(WinSize {
                cols,
                rows: 12,
                cell_w: 8,
                cell_h: 17,
            })
            .encode(&mut daemon_side)
            .unwrap();
            let clock = if cols == 98 { "16:46:33" } else { "16:46:2x" };
            DaemonMsg::Output(redraw(cols, clock))
                .encode(&mut daemon_side)
                .unwrap();
        }
        daemon_side.flush().unwrap();

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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

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

        DaemonMsg::Agent(Some(CLIAgent::Claude))
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(Some(CLIAgent::Claude)), "agent report should surface");

        DaemonMsg::Agent(None).encode(&mut daemon_side).unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(None), "agent exit should clear it");
    }

    #[test]
    fn agent_session_follows_daemon_status_reports() {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus};

        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
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

        DaemonMsg::AgentStatus(Some(AgentSessionState {
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
        }))
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();
        assert!(
            poll(&|s| s.is_some_and(|s| s.status == AgentStatus::Waiting
                && s.session_id.as_deref() == Some("sid-1")
                && s.rich)),
            "status report should surface with message + session id"
        );

        DaemonMsg::AgentStatus(None)
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(&|s| s.is_none()), "a None report clears the session");
    }

    #[test]
    fn the_spawn_directory_answers_until_the_shell_reports_its_own() {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
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

        DaemonMsg::Cwd(PathBuf::from("/repo/tty7/crates"))
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(
            poll(Some("/repo/tty7/crates")),
            "the shell's own report replaces the seed"
        );
    }

    #[test]
    fn a_shell_report_that_beat_the_seed_wins() {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Cwd(PathBuf::from("/somewhere/else"))
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
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
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
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
        DaemonMsg::Output(b"\x1b]133;C;printf '100%25'\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(
            poll("printf '100%25'"),
            "C should record the line it carried, got {:?}",
            term.running_command()
        );

        // Back at a prompt the command is over — holding the name would make
        // the next close question ask about something that already finished.
        DaemonMsg::Output(b"\x1b]133;B\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(""), "a new prompt clears the running command");

        // A bare `C` starts a command without naming it (the PowerShell path).
        // It must not leave the previous name standing.
        DaemonMsg::Output(b"\x1b]133;C;cargo build\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll("cargo build"));
        DaemonMsg::Output(b"\x1b]133;C\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(""), "an unnamed command start does not inherit a name");
    }

    #[test]
    fn shell_vi_mode_follows_live_prompt_mode_marks_without_disarming_zle() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
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

        DaemonMsg::Output(b"\x1b]133;B\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(false, true), "B should arm zle only");

        DaemonMsg::Output(b"\x1b]133;V;1\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(
            poll(true, true),
            "V;1 should set shell vi-mode without disarming zle"
        );

        DaemonMsg::Output(b"\x1b]133;V;0\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(
            poll(false, true),
            "V;0 should clear shell vi-mode without disarming zle"
        );

        DaemonMsg::Output(b"\x1b]133;C\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
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
        let (client, mut daemon) = crate::terminal::view::test_stream_pair();
        let term = super::RemoteTerminal::from_stream(client, TermSize::new(80, 24)).unwrap();
        let burst: Vec<u8> = (0..40)
            .flat_map(|n| format!("\x1b]9;note {n}\x07").into_bytes())
            .collect();
        DaemonMsg::Output(burst).encode(&mut daemon).unwrap();

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
