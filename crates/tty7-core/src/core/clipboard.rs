//! OSC 5522 clipboard writes, intercepted before they reach the VT stream.
//!
//! ntty7 does not apply a program's clipboard write. The sequence is still
//! taken out of the passthrough — a payload left in the stream would paint as
//! text — and a `type=write` is answered at once with `EPERM`, so the sender
//! is not left waiting on a reply that will never come. Reads are answered
//! `ENOSYS`. A capability probe (`CSI ? 5522 $ p`) is answered as a mode this
//! terminal knows and has switched off.

const MAX_OSC_PAYLOAD: usize = 8 << 10;

/// Answers one clipboard control. No memory of earlier transfers: an
/// oversized OSC is dropped by the tokenizer and never becomes a reply.
#[derive(Default)]
struct Parser;

impl Parser {
    fn feed(&self, payload: &[u8]) -> Option<Vec<u8>> {
        if payload == b"probe" {
            return Some(capability_reply());
        }
        let metadata = match payload.iter().position(|&b| b == b';') {
            Some(at) => &payload[..at],
            None => payload,
        };
        let metadata = std::str::from_utf8(metadata).ok()?;
        let fields = metadata
            .split(':')
            .filter_map(|field| field.split_once('='))
            .collect::<Vec<_>>();
        let kind = fields.iter().find(|(k, _)| *k == "type")?.1;
        let id = fields
            .iter()
            .find(|(k, _)| *k == "id")
            .map(|(_, value)| sanitize_id(value))
            .filter(|value| !value.is_empty());

        match kind {
            "read" => Some(response_for("read", id.as_deref(), "ENOSYS")),
            // Refused before any image bytes are kept. `wdata` that follows
            // is still OSC 5522, so the tokenizer strips it; answering again
            // would be a second reply for the same transfer.
            "write" => Some(response(id.as_deref(), "EPERM")),
            _ => None,
        }
    }
}

/// The DECRPM answer to `CSI ? 5522 $ p`: 2, a mode this terminal knows and
/// has switched off. A terminal that never heard of OSC 5522 answers 0, so a
/// sender can still tell "not supported" from "not permitted here".
fn capability_reply() -> Vec<u8> {
    b"\x1b[?5522;2$y".to_vec()
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.'))
        .collect()
}

pub fn response(id: Option<&str>, status: &str) -> Vec<u8> {
    response_for("write", id, status)
}

fn response_for(kind: &str, id: Option<&str>, status: &str) -> Vec<u8> {
    let id = id
        .map(sanitize_id)
        .filter(|value| !value.is_empty())
        .map(|value| format!(":id={value}"))
        .unwrap_or_default();
    format!("\x1b]5522;type={kind}:status={status}{id}\x1b\\").into_bytes()
}

#[derive(Default)]
pub struct ClipboardSniffer {
    tokenizer: Tokenizer,
    parser: Parser,
}

impl ClipboardSniffer {
    pub fn is_ground(&self) -> bool {
        self.tokenizer.ground()
    }

    pub fn sniff<'a>(&mut self, bytes: &'a [u8]) -> Sniffed<'a> {
        if self.tokenizer.ground() && !might_contain_protocol(bytes) {
            return Sniffed::Plain(bytes);
        }
        let Self { tokenizer, parser } = self;
        let segments = std::cell::RefCell::new(Vec::new());
        tokenizer.feed(
            bytes,
            |run| push_output(&mut segments.borrow_mut(), run),
            |payload| {
                if let Some(reply) = parser.feed(payload) {
                    segments.borrow_mut().push(Segment::Reply(reply));
                }
            },
        );
        Sniffed::Segments(segments.into_inner())
    }
}

fn might_contain_protocol(bytes: &[u8]) -> bool {
    const PREFIXES: [&[u8]; 2] = [b"\x1b]5522;", b"\x1b[?5522$p"];
    PREFIXES.iter().any(|prefix| {
        memchr::memmem::find(bytes, prefix).is_some()
            || (1..prefix.len())
                .any(|len| bytes.len() >= len && bytes[bytes.len() - len..] == prefix[..len])
    })
}

fn push_output(segments: &mut Vec<Segment>, run: &[u8]) {
    if run.is_empty() {
        return;
    }
    if let Some(Segment::Output(out)) = segments.last_mut() {
        out.extend_from_slice(run);
    } else {
        segments.push(Segment::Output(run.to_vec()));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Output(Vec<u8>),
    Reply(Vec<u8>),
}

pub enum Sniffed<'a> {
    Plain(&'a [u8]),
    Segments(Vec<Segment>),
}

#[derive(Default)]
struct Tokenizer {
    state: TokenState,
    buf: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum TokenState {
    #[default]
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    PassOsc,
    PassOscEsc,
    Drop,
    DropEsc,
}

impl Tokenizer {
    fn ground(&self) -> bool {
        self.state == TokenState::Ground
    }

    fn feed(
        &mut self,
        bytes: &[u8],
        mut on_output: impl FnMut(&[u8]),
        mut on_payload: impl FnMut(&[u8]),
    ) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                TokenState::Ground => match memchr::memchr(0x1b, &bytes[i..]) {
                    Some(off) => {
                        on_output(&bytes[i..i + off]);
                        self.state = TokenState::Esc;
                        i += off + 1;
                    }
                    None => {
                        on_output(&bytes[i..]);
                        return;
                    }
                },
                TokenState::Esc => {
                    if bytes[i] == b']' {
                        self.buf.clear();
                        self.state = TokenState::Osc;
                        i += 1;
                    } else if bytes[i] == b'[' {
                        self.buf.clear();
                        self.state = TokenState::Csi;
                        i += 1;
                    } else {
                        on_output(b"\x1b");
                        self.state = TokenState::Ground;
                    }
                }
                TokenState::Csi => {
                    const QUERY: &[u8] = b"?5522$p";
                    self.buf.push(bytes[i]);
                    i += 1;
                    if self.buf.as_slice() == QUERY {
                        on_payload(b"probe");
                        self.buf.clear();
                        self.state = TokenState::Ground;
                    } else if !QUERY.starts_with(&self.buf) {
                        on_output(b"\x1b[");
                        on_output(&self.buf);
                        self.buf.clear();
                        self.state = TokenState::Ground;
                    }
                }
                TokenState::Osc => match bytes[i] {
                    0x07 => {
                        self.finish(false, &mut on_output, &mut on_payload);
                        i += 1;
                    }
                    0x1b => {
                        self.state = TokenState::OscEsc;
                        i += 1;
                    }
                    b => {
                        self.buf.push(b);
                        i += 1;
                        let target = b"5522";
                        let wrong_id = match self.buf.iter().position(|&b| b == b';') {
                            Some(pos) => &self.buf[..pos] != target,
                            None => !target.starts_with(&self.buf),
                        };
                        if wrong_id {
                            on_output(b"\x1b]");
                            on_output(&self.buf);
                            self.buf.clear();
                            self.state = TokenState::PassOsc;
                        } else if self.buf.len() > MAX_OSC_PAYLOAD {
                            self.buf.clear();
                            self.state = TokenState::Drop;
                        }
                    }
                },
                TokenState::OscEsc => {
                    if bytes[i] == b'\\' {
                        self.finish(true, &mut on_output, &mut on_payload);
                        i += 1;
                    } else {
                        // Any other byte means the `ESC` cut the OSC short and
                        // opened a sequence of its own. A clipboard write it
                        // interrupted is ours and goes unanswered, but anything
                        // held while the identifier was still undecided was
                        // the terminal's, and so is the `ESC`: hand them on and
                        // let `Esc` judge this byte. Dropping them left the
                        // tail of the new sequence (`[31m`) printed as text.
                        if !self.buf.starts_with(b"5522;") {
                            on_output(b"\x1b]");
                            on_output(&self.buf);
                        }
                        self.buf.clear();
                        self.state = TokenState::Esc;
                    }
                }
                TokenState::PassOsc => match memchr::memchr2(0x07, 0x1b, &bytes[i..]) {
                    Some(off) => {
                        on_output(&bytes[i..i + off + 1]);
                        self.state = if bytes[i + off] == 0x07 {
                            TokenState::Ground
                        } else {
                            TokenState::PassOscEsc
                        };
                        i += off + 1;
                    }
                    None => {
                        on_output(&bytes[i..]);
                        return;
                    }
                },
                TokenState::PassOscEsc => {
                    on_output(&bytes[i..i + 1]);
                    self.state = if bytes[i] == b'\\' {
                        TokenState::Ground
                    } else {
                        TokenState::PassOsc
                    };
                    i += 1;
                }
                // Past `MAX_OSC_PAYLOAD` the rest of this OSC is discarded.
                // No reply: a `type=write` already answered `EPERM`, and a
                // later oversized sequence has no id of its own to name.
                TokenState::Drop => match memchr::memchr2(0x07, 0x1b, &bytes[i..]) {
                    Some(off) => {
                        self.state = if bytes[i + off] == 0x07 {
                            TokenState::Ground
                        } else {
                            TokenState::DropEsc
                        };
                        i += off + 1;
                    }
                    None => return,
                },
                TokenState::DropEsc => {
                    if bytes[i] == b'\\' {
                        self.state = TokenState::Ground;
                    } else if bytes[i] == b']' {
                        self.buf.clear();
                        self.state = TokenState::Osc;
                    } else {
                        self.state = TokenState::Drop;
                    }
                    i += 1;
                }
            }
        }
    }

    fn finish(
        &mut self,
        st: bool,
        on_output: &mut impl FnMut(&[u8]),
        on_payload: &mut impl FnMut(&[u8]),
    ) {
        if let Some(payload) = self.buf.strip_prefix(b"5522;") {
            on_payload(payload);
        } else {
            on_output(b"\x1b]");
            on_output(&self.buf);
            on_output(if st { b"\x1b\\" } else { b"\x07" });
        }
        self.buf.clear();
        self.state = TokenState::Ground;
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    use super::*;

    fn osc(metadata: &str, payload: &str) -> Vec<u8> {
        let separator = if payload.is_empty() { "" } else { ";" };
        format!("\x1b]5522;{metadata}{separator}{payload}\x1b\\").into_bytes()
    }

    fn collect(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut sniffer = ClipboardSniffer::default();
        let mut output = Vec::new();
        let mut replies = Vec::new();
        for chunk in chunks {
            match sniffer.sniff(chunk) {
                Sniffed::Plain(bytes) => output.extend_from_slice(bytes),
                Sniffed::Segments(parts) => {
                    for part in parts {
                        match part {
                            Segment::Output(bytes) => output.extend(bytes),
                            Segment::Reply(reply) => replies.push(reply),
                        }
                    }
                }
            }
        }
        (output, replies)
    }

    /// A completed image write used to be reassembled and forwarded. It is
    /// refused instead: the bytes never reach the terminal, and the sender
    /// gets one `EPERM` so it does not wait.
    #[test]
    fn osc_5522_write_is_stripped_and_answered_eperm() {
        let mime = BASE64.encode("image/png");
        let mut stream = b"before".to_vec();
        stream.extend(osc("type=write:id=req.1", ""));
        stream.extend(osc(
            &format!("type=wdata:mime={mime}"),
            &BASE64.encode(b"png-data"),
        ));
        stream.extend(osc("type=wdata", ""));
        stream.extend_from_slice(b"after");

        let split = stream.len() / 2;
        let (output, replies) = collect(&[&stream[..split], &stream[split..]]);
        assert_eq!(output, b"beforeafter");
        assert!(
            !output.windows(b"png-data".len()).any(|w| w == b"png-data"),
            "the image payload leaked into the VT stream: {output:?}"
        );
        assert_eq!(replies, vec![response(Some("req.1"), "EPERM")]);
    }

    #[test]
    fn non_clipboard_osc_passes_through() {
        let input = b"a\x1b]0;title\x07b\x1b]133;A\x1b\\c";
        let (output, replies) = collect(&[input]);
        assert_eq!(output, input);
        assert!(replies.is_empty());
    }

    /// Everything that is not OSC 5522 must come out byte for byte however the
    /// reads split it — a held `ESC ]` the tokenizer loses turns the rest of the
    /// sequence into text on screen (#857). Pi's line marks around CJK text, an
    /// OSC an `ESC` interrupts before its identifier is known, and one
    /// interrupted by a new OSC, at every split point and a byte at a time.
    #[test]
    fn foreign_sequences_survive_every_split() {
        let streams: [&[u8]; 5] = [
            "\x1b[?2026h\x1b]133;B\x07\x1b]133;C\x07下一步。\r\n\x1b[?2026l".as_bytes(),
            b"a\x1b]\x1b[31mred",
            b"a\x1b]55\x1b[0mb",
            b"a\x1b]552\x1b]0;title\x07b",
            b"a\x1b]5522\x1bXb",
        ];
        for input in streams {
            for cut in 0..=input.len() {
                let (output, replies) = collect(&[&input[..cut], &input[cut..]]);
                assert_eq!(output, input, "split at byte {cut} of {input:?}");
                assert!(replies.is_empty());
            }
            let chunks: Vec<&[u8]> = input.chunks(1).collect();
            assert_eq!(collect(&chunks).0, input);
        }
    }

    /// An `ESC` that breaks into a clipboard write abandons the write, whose
    /// bytes are ours to drop — but the escape it starts is the terminal's.
    #[test]
    fn an_escape_interrupting_a_write_still_reaches_the_terminal() {
        let input = b"a\x1b]5522;type=write\x1b[31mred";
        for cut in 0..=input.len() {
            let (output, _) = collect(&[&input[..cut], &input[cut..]]);
            assert_eq!(output, b"a\x1b[31mred", "split at byte {cut}");
        }
    }

    /// One refusal, then silence. Further chunks of the same write — including
    /// a mime we would never have accepted — must not leak and must not be
    /// answered a second time.
    #[test]
    fn a_refused_write_is_answered_once_and_its_data_stays_out() {
        let mime = BASE64.encode("image/svg+xml");
        let (output, replies) = collect(&[
            &osc("type=write:id=bad/one", ""),
            &osc(
                &format!("type=wdata:mime={mime}"),
                &BASE64.encode(b"<svg/>"),
            ),
            &osc(
                &format!("type=wdata:mime={mime}"),
                &BASE64.encode(b"ignored"),
            ),
            &osc("type=wdata", ""),
        ]);
        assert!(output.is_empty());
        assert_eq!(replies, vec![response(Some("badone"), "EPERM")]);
    }

    #[test]
    fn an_oversized_chunk_does_not_reach_the_terminal() {
        let mime = BASE64.encode("image/png");
        let huge = "A".repeat(MAX_OSC_PAYLOAD + 64);
        let mut stream = osc("type=write:id=big", "");
        stream.extend(format!("\x1b]5522;type=wdata:mime={mime};{huge}\x1b\\").into_bytes());
        stream.extend_from_slice(b"tail");
        let (output, replies) = collect(&[&stream]);
        assert_eq!(output, b"tail");
        assert_eq!(replies, vec![response(Some("big"), "EPERM")]);
        assert!(
            !output.windows(8).any(|w| w == b"AAAAAAAA"),
            "oversized clipboard bytes leaked: {output:?}"
        );
    }

    /// A later oversized OSC is its own sequence. It must not grow a second
    /// reply, and it must not borrow the id of the write we already refused.
    #[test]
    fn an_oversized_chunk_after_a_refused_write_stays_silent() {
        let huge = "B".repeat(MAX_OSC_PAYLOAD + 64);
        let mut stream = osc("type=write:id=kept", "");
        stream.extend_from_slice(b"mid");
        stream.extend(format!("\x1b]5522;type=wdata:mime=xx;{huge}\x1b\\").into_bytes());
        stream.extend_from_slice(b"tail");
        let (output, replies) = collect(&[&stream]);
        assert_eq!(output, b"midtail");
        assert_eq!(replies, vec![response(Some("kept"), "EPERM")]);
        assert!(
            replies
                .iter()
                .all(|reply| !reply.windows(6).any(|w| w == b"EINVAL")),
            "an oversized OSC must not answer EINVAL: {replies:?}"
        );
    }

    #[test]
    fn a_primary_selection_write_is_refused() {
        let (output, replies) = collect(&[&osc("type=write:loc=primary:id=x", "")]);
        assert!(output.is_empty());
        assert_eq!(replies, vec![response(Some("x"), "EPERM")]);
    }

    #[test]
    fn byte_at_a_time_strips_the_write_and_keeps_the_text() {
        let mime = BASE64.encode("image/png");
        let data = BASE64.encode(b"png");
        let stream = format!(
            "a\x1b]5522;type=write\x1b\\\
             \x1b]5522;type=wdata:mime={mime};{data}\x1b\\\
             \x1b]5522;type=wdata\x1b\\b"
        );
        let chunks: Vec<&[u8]> = stream.as_bytes().chunks(1).collect();
        let (output, replies) = collect(&chunks);
        assert_eq!(output, b"ab");
        assert_eq!(replies, vec![response(None, "EPERM")]);
    }

    /// The probe has to answer even when writes are off. A sender that reads
    /// DECRPM sees 2 — "this terminal knows the mode and it is off" — and
    /// stops there instead of pushing an image nobody will take. `0` would
    /// mean the terminal had never heard of OSC 5522.
    #[test]
    fn capability_probe_is_stripped_and_reports_the_mode_off() {
        let (output, replies) = collect(&[b"a\x1b[?55", b"22$p", b"b"]);
        assert_eq!(output, b"ab");
        assert_eq!(replies, vec![capability_reply()]);
        assert_eq!(capability_reply(), b"\x1b[?5522;2$y");
        assert_ne!(capability_reply(), b"\x1b[?5522;1$y");
        assert_ne!(capability_reply(), b"\x1b[?5522;0$y");
    }

    #[test]
    fn clipboard_reads_are_explicitly_unsupported() {
        let (output, replies) =
            collect(&[&osc("type=read:id=read-1", &BASE64.encode("image/png"))]);
        assert!(output.is_empty());
        assert_eq!(
            replies,
            vec![response_for("read", Some("read-1"), "ENOSYS")]
        );
    }
}
