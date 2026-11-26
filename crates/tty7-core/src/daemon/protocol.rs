//! The frames a pane and its in-process subscriber still exchange.
//!
//! ntty7 has one subscriber and no socket daemon. The codec stays only so the
//! terminal tests can keep feeding a `from_stream` fixture until that harness
//! moves onto the in-process pipe. Kind numbers are the ones those fixtures
//! already write.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Largest payload a frame may carry. Kitty graphics sizes its images against
/// this, so a full-window frame still fits one message.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// A frame is a little-endian `u32` length, a one-byte kind, then the payload.
const HEADER: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u16,
    pub cell_h: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub args_are_tty7_defaults: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: u64,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub title: String,
    /// See [`crate::core::machine::PaneRecord::osc_title`] — the terminal's own
    /// title, not the foreground process name `title` carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub osc_title: Option<String>,
    pub alive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteContext {
    pub kind: RemoteKind,
    pub argv: Vec<String>,
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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

mod kind {
    pub const INPUT: u8 = 3;
    pub const RESIZE: u8 = 4;

    pub const OUTPUT: u8 = 3;
    pub const CWD: u8 = 4;
    pub const PROMPT: u8 = 5;
    pub const EXITED: u8 = 6;
    pub const SIZE: u8 = 9;
    pub const REMOTE_CONTEXT: u8 = 10;
    pub const AGENT: u8 = 21;
    pub const AGENT_STATUS: u8 = 22;
    pub const IMAGE: u8 = 60;
    pub const DELETE_IMAGE: u8 = 61;
    pub const CLIPBOARD_WRITE: u8 = 62;
}

pub fn write_frame<W: Write>(w: &mut W, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = payload.len();
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame payload exceeds MAX_FRAME",
        ));
    }
    // One write, not three. A writer that does not implement `write_vectored`
    // falls back to the first slice, so the loop still terminates.
    let mut header = [0u8; HEADER];
    header[..4].copy_from_slice(&(len as u32).to_le_bytes());
    header[4] = kind;
    let mut bufs = [io::IoSlice::new(&header), io::IoSlice::new(payload)];
    let mut rest: &mut [io::IoSlice<'_>] = &mut bufs;
    while !rest.is_empty() {
        match w.write_vectored(rest) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the frame could not be written in full",
                ));
            }
            Ok(n) => io::IoSlice::advance_slices(&mut rest, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn read_frame<R: Read>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame payload exceeds MAX_FRAME",
        ));
    }
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((kind[0], payload))
}

pub fn take_frame(buf: &mut Vec<u8>) -> io::Result<Option<(u8, Vec<u8>)>> {
    if buf.len() < HEADER {
        return Ok(None);
    }
    let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame payload exceeds MAX_FRAME",
        ));
    }
    if buf.len() < HEADER + len {
        return Ok(None);
    }
    let kind = buf[4];
    let payload = buf[HEADER..HEADER + len].to_vec();
    buf.drain(..HEADER + len);
    Ok(Some((kind, payload)))
}

fn to_json<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn from_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<T> {
    serde_json::from_slice(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

impl ClientMsg {
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            ClientMsg::Input(bytes) => write_frame(w, kind::INPUT, bytes),
            ClientMsg::Resize(size) => write_frame(w, kind::RESIZE, &to_json(size)?),
        }
    }

    pub fn read<R: Read>(r: &mut R) -> io::Result<Self> {
        let (k, payload) = read_frame(r)?;
        Self::from_frame(k, payload)
    }

    pub fn from_frame(k: u8, payload: Vec<u8>) -> io::Result<Self> {
        Ok(match k {
            kind::INPUT => ClientMsg::Input(payload),
            kind::RESIZE => ClientMsg::Resize(from_json(&payload)?),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown client frame kind {k}"),
                ));
            }
        })
    }
}

impl DaemonMsg {
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            DaemonMsg::Size(size) => write_frame(w, kind::SIZE, &to_json(size)?),
            DaemonMsg::Output(bytes) => write_frame(w, kind::OUTPUT, bytes),
            DaemonMsg::Image(frame) => write_frame(w, kind::IMAGE, frame),
            DaemonMsg::DeleteImage(sel) => write_frame(w, kind::DELETE_IMAGE, sel),
            DaemonMsg::ClipboardWrite(frame) => write_frame(w, kind::CLIPBOARD_WRITE, frame),
            DaemonMsg::Cwd(path) => write_frame(w, kind::CWD, &to_json(path)?),
            DaemonMsg::Prompt {
                active,
                at_prompt,
                last_exit,
            } => write_frame(w, kind::PROMPT, &to_json(&(active, at_prompt, last_exit))?),
            DaemonMsg::Exited { code } => write_frame(w, kind::EXITED, &to_json(code)?),
            DaemonMsg::RemoteContext(remote) => {
                write_frame(w, kind::REMOTE_CONTEXT, &to_json(remote)?)
            }
            DaemonMsg::Agent(agent) => write_frame(w, kind::AGENT, &to_json(agent)?),
            DaemonMsg::AgentStatus(state) => write_frame(w, kind::AGENT_STATUS, &to_json(state)?),
        }
    }

    pub fn read<R: Read>(r: &mut R) -> io::Result<Self> {
        let (k, payload) = read_frame(r)?;
        Self::from_frame(k, payload)
    }

    pub fn from_frame(k: u8, payload: Vec<u8>) -> io::Result<Self> {
        Ok(match k {
            kind::SIZE => DaemonMsg::Size(from_json(&payload)?),
            kind::OUTPUT => DaemonMsg::Output(payload),
            kind::IMAGE => DaemonMsg::Image(payload),
            kind::DELETE_IMAGE => DaemonMsg::DeleteImage(payload),
            kind::CLIPBOARD_WRITE => DaemonMsg::ClipboardWrite(payload),
            kind::CWD => DaemonMsg::Cwd(from_json(&payload)?),
            kind::PROMPT => {
                let (active, at_prompt, last_exit) = from_json(&payload)?;
                DaemonMsg::Prompt {
                    active,
                    at_prompt,
                    last_exit,
                }
            }
            kind::EXITED => DaemonMsg::Exited {
                code: from_json(&payload)?,
            },
            kind::REMOTE_CONTEXT => DaemonMsg::RemoteContext(from_json(&payload)?),
            kind::AGENT => DaemonMsg::Agent(from_json(&payload)?),
            kind::AGENT_STATUS => DaemonMsg::AgentStatus(from_json(&payload)?),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown daemon frame kind {k}"),
                ));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_client(msg: &ClientMsg) {
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        assert_eq!(&ClientMsg::read(&mut cur).unwrap(), msg);
    }

    fn round_trip_daemon(msg: &DaemonMsg) {
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let mut pending = buf;
        let (kind, payload) = take_frame(&mut pending).unwrap().unwrap();
        assert!(pending.is_empty());
        assert_eq!(DaemonMsg::from_frame(kind, payload).unwrap(), *msg);
    }

    #[test]
    fn live_frames_round_trip_and_unknown_kinds_are_rejected() {
        let size = WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        };
        round_trip_client(&ClientMsg::Input(b"ls\r".to_vec()));
        round_trip_client(&ClientMsg::Resize(size));

        round_trip_daemon(&DaemonMsg::Size(size));
        round_trip_daemon(&DaemonMsg::Output(b"hello\r\n".to_vec()));
        round_trip_daemon(&DaemonMsg::Image(vec![1, 2, 3]));
        round_trip_daemon(&DaemonMsg::DeleteImage(vec![9]));
        round_trip_daemon(&DaemonMsg::ClipboardWrite(vec![4, 5]));
        round_trip_daemon(&DaemonMsg::Cwd(PathBuf::from("/tmp")));
        round_trip_daemon(&DaemonMsg::Prompt {
            active: true,
            at_prompt: false,
            last_exit: Some(1),
        });
        round_trip_daemon(&DaemonMsg::Exited { code: None });
        round_trip_daemon(&DaemonMsg::RemoteContext(Some(RemoteContext {
            kind: RemoteKind::Ssh,
            argv: vec!["ssh".into(), "box".into()],
            target: "box".into(),
        })));
        round_trip_daemon(&DaemonMsg::RemoteContext(None));
        round_trip_daemon(&DaemonMsg::Agent(Some(
            crate::core::cli_agent::CLIAgent::Claude,
        )));
        round_trip_daemon(&DaemonMsg::Agent(None));
        round_trip_daemon(&DaemonMsg::AgentStatus(Some(
            crate::core::cli_agent::AgentSessionState {
                status: crate::core::cli_agent::AgentStatus::Waiting,
                message: Some("permission".into()),
                session_id: Some("abc".into()),
                launch_argv: None,
                rich: false,
                cwd: None,
                activity: 1,
                turns: 0,
                inferred: false,
                readout: Default::default(),
                since_ms: None,
            },
        )));
        round_trip_daemon(&DaemonMsg::AgentStatus(None));

        let mut unknown = Vec::new();
        write_frame(&mut unknown, 99, b"nope").unwrap();
        let (kind, payload) = read_frame(&mut std::io::Cursor::new(&unknown)).unwrap();
        assert!(DaemonMsg::from_frame(kind, payload).is_err());
        assert!(ClientMsg::from_frame(2, Vec::new()).is_err());
    }
}
