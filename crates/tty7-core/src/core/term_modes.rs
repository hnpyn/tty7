//! Tracks DEC private mode 2031 and the queries a program asks while it is on.
//!
//! The window answers colour-scheme queries (`CSI ? 996 n`, DECRQM for 2031,
//! primary device attributes) from the same bytes the pane just wrote. A shell
//! prompt mark retires 2031: the program that switched it on is gone, and a
//! later report would be typed into the command line.

/// The only mode this tracker follows. Everything else is the emulator's.
const TRACKED: &[u16] = &[COLOR_SCHEME_UPDATES];

/// DEC mode 2031: report light/dark changes as `CSI ? 997 ; 1|2 n`.
pub const COLOR_SCHEME_UPDATES: u16 = 2031;

/// A CSI longer than this is not a mode set; keep the buffer bounded.
const MAX_PARAMS: usize = 64;

/// `mode` with optional leading zeros (`02031` is `2031`). A run of zeros is
/// not that mode, and any digit left after the zeros makes a different number.
/// The buffer is already refused past [`MAX_PARAMS`], so this compare stays
/// bounded.
fn csi_param_is(params: &[u8], mode: &[u8]) -> bool {
    let Some(start) = params.iter().position(|&b| b != b'0') else {
        return false;
    };
    params[start..] == *mode
}

/// The private modes currently on.
#[derive(Debug, Default, Clone)]
pub struct TerminalModes {
    on: Vec<u16>,
    state: State,
    params: Vec<u8>,
    /// Queries seen and not yet taken — see [`Self::take_queries`].
    queries: Vec<Query>,
}

/// A query whose answer the program waits on, which the tracker spots so
/// whoever owns the pane's input can answer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    /// `CSI c` / `CSI 0 c`: primary device attributes.
    DeviceAttributes,
    /// `CSI ? 996 n`: is the colour scheme dark or light?
    ColorScheme,
    /// `CSI ? 2031 $ p`: DECRQM for colour-scheme update notifications, with
    /// whether mode 2031 was on at the point in the stream the query arrived.
    ColorSchemeMode { set: bool },
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Text,
    Esc,
    /// A CSI whose parameter bytes are being read. `marker` is the private
    /// marker it opened with, `0` for none: `?` makes it a DEC private mode.
    /// `dollar` is the `$` intermediate of a DECRQM (`CSI ? Ps $ p`).
    Csi {
        marker: u8,
        dollar: bool,
    },
    Osc,
    OscEsc,
}

impl TerminalModes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `mode` is on. Only ever true for a mode in [`TRACKED`].
    pub fn is_on(&self, mode: u16) -> bool {
        self.on.contains(&mode)
    }

    /// The queries fed since the last call, in the order they were asked.
    /// Every feeder must take them, or they pile up; answering is the
    /// caller's job.
    pub fn take_queries(&mut self) -> Vec<Query> {
        std::mem::take(&mut self.queries)
    }

    pub fn is_ground(&self) -> bool {
        self.state == State::Text
    }

    pub fn feed_reporting(&mut self, bytes: &[u8], mut on_query: impl FnMut(usize, Query)) {
        let mut i = 0;
        while i < bytes.len() {
            if self.state == State::Text {
                let Some(off) = memchr::memchr(0x1b, &bytes[i..]) else {
                    return;
                };
                self.state = State::Esc;
                i += off + 1;
                continue;
            }
            let b = bytes[i];
            match self.state {
                State::Text => unreachable!(),
                State::Esc => match b {
                    b'[' => {
                        self.params.clear();
                        self.state = State::Csi {
                            marker: 0,
                            dollar: false,
                        };
                    }
                    b']' => {
                        self.params.clear();
                        self.state = State::Osc;
                    }
                    // RIS. Everything this tracker knows goes back to default,
                    // exactly as it does in the client's emulator.
                    b'c' => {
                        self.on.clear();
                        self.state = State::Text;
                    }
                    0x1b => {}
                    _ => self.state = State::Text,
                },
                State::Csi { marker, dollar } => match b {
                    b'?' | b'>' | b'<' | b'='
                        if marker == 0 && !dollar && self.params.is_empty() =>
                    {
                        self.state = State::Csi {
                            marker: b,
                            dollar: false,
                        }
                    }
                    b'0'..=b'9' | b';' | b':' if !dollar => {
                        self.params.push(b);
                        if self.params.len() > MAX_PARAMS {
                            self.state = State::Text;
                        }
                    }
                    b'$' if !dollar => {
                        self.state = State::Csi {
                            marker,
                            dollar: true,
                        }
                    }
                    // ESC cancels this sequence and starts another, the same way
                    // Esc, Osc and OscEsc already keep a bare ESC from falling
                    // through into "end of sequence".
                    0x1b => self.state = State::Esc,
                    b'p' if dollar => {
                        if marker == b'?' && csi_param_is(&self.params, b"2031") {
                            let query = Query::ColorSchemeMode {
                                set: self.is_on(COLOR_SCHEME_UPDATES),
                            };
                            self.queries.push(query);
                            on_query(i + 1, query);
                        }
                        self.state = State::Text;
                    }
                    b'h' | b'l' if !dollar => {
                        if marker == b'?' {
                            self.apply(b == b'h');
                        }
                        self.state = State::Text;
                    }
                    b'n' if !dollar => {
                        if marker == b'?' && self.params == b"996" {
                            self.queries.push(Query::ColorScheme);
                            on_query(i + 1, Query::ColorScheme);
                        }
                        self.state = State::Text;
                    }
                    b'c' if !dollar => {
                        if marker == 0 && matches!(self.params.as_slice(), b"" | b"0") {
                            self.queries.push(Query::DeviceAttributes);
                            on_query(i + 1, Query::DeviceAttributes);
                        }
                        self.state = State::Text;
                    }
                    // Any other final or intermediate byte ends a sequence that
                    // is neither a tracked mode set nor a tracked query.
                    _ => self.state = State::Text,
                },
                State::Osc => match b {
                    0x07 => self.osc_end(),
                    0x1b => self.state = State::OscEsc,
                    _ if self.params.len() < 5 => self.params.push(b),
                    _ => {}
                },
                State::OscEsc => match b {
                    b'\\' => self.osc_end(),
                    0x1b => {}
                    _ => self.state = State::Osc,
                },
            }
            i += 1;
        }
    }

    /// A shell prompt mark (OSC 133 `A`, `B` or `D`) means the shell owns the
    /// terminal again, so whatever program switched 2031 on is gone — possibly
    /// killed before it could send `?2031l`. A report after that would be
    /// typed into the shell's command line.
    fn osc_end(&mut self) {
        if matches!(self.params.as_slice(), b"133;A" | b"133;B" | b"133;D") {
            self.on.retain(|m| *m != COLOR_SCHEME_UPDATES);
        }
        self.params.clear();
        self.state = State::Text;
    }

    fn apply(&mut self, on: bool) {
        for param in self.params.split(|b| *b == b';') {
            let Ok(text) = std::str::from_utf8(param) else {
                continue;
            };
            let Ok(mode) = text.parse::<u16>() else {
                continue;
            };
            if !TRACKED.contains(&mode) {
                continue;
            }
            self.on.retain(|m| *m != mode);
            if on {
                self.on.push(mode);
            }
        }
        self.params.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(modes: &mut TerminalModes, bytes: &[u8]) {
        modes.feed_reporting(bytes, |_, _| {});
    }

    #[test]
    fn spots_primary_device_attribute_queries() {
        let mut modes = TerminalModes::new();
        feed(&mut modes, b"\x1b]11;?\x07\x1b[");
        feed(&mut modes, b"c\x1b[0c\x1b[>c\x1b[=c\x1b[?6c");
        assert_eq!(
            modes.take_queries(),
            [Query::DeviceAttributes; 2],
            "a split DA1 counts, DA2, DA3 and a DA1 reply do not"
        );
        assert!(modes.take_queries().is_empty());
    }

    #[test]
    fn follows_colour_scheme_updates_and_spots_its_queries() {
        let mut modes = TerminalModes::new();
        feed(&mut modes, b"\x1b[?2031$p\x1b[?2031h\x1b[?2031$");
        assert!(modes.is_on(COLOR_SCHEME_UPDATES));
        feed(&mut modes, b"p\x1b[?996n\x1b[996n\x1b[?997;1n\x1b[?2004$p");
        assert_eq!(
            modes.take_queries(),
            [
                Query::ColorSchemeMode { set: false },
                Query::ColorSchemeMode { set: true },
                Query::ColorScheme,
            ],
            "DECRQM for 2031 reports the mode state at the query; other modes stay with the emulator"
        );
        let mut modes = TerminalModes::new();
        let bytes = b"\x1b[?2031$p\x1b[?2031h\x1b[?996n";
        let mut seen = Vec::new();
        modes.feed_reporting(bytes, |off, query| seen.push((off, query)));
        assert_eq!(
            seen,
            vec![
                (
                    "\u{1b}[?2031$p".len(),
                    Query::ColorSchemeMode { set: false }
                ),
                (bytes.len(), Query::ColorScheme),
            ]
        );
        assert_eq!(modes.take_queries().len(), 2);
        assert!(modes.take_queries().is_empty());
        feed(&mut modes, b"\x1b[?2031l");
        assert!(!modes.is_on(COLOR_SCHEME_UPDATES));

        let mut modes = TerminalModes::new();
        feed(
            &mut modes,
            b"\x1b[?02031$p\x1b[?0000$p\x1b[?20310$p\x1b[?\x1b[?2031$p",
        );
        assert_eq!(
            modes.take_queries(),
            [
                Query::ColorSchemeMode { set: false },
                Query::ColorSchemeMode { set: false },
            ],
            "leading zeros are the same mode, all zeros and a longer number are not, and ESC restarts the sequence"
        );

        let mut modes = TerminalModes::new();
        let mut early = Vec::new();
        modes.feed_reporting(b"\x1b[?2031$", |off, query| early.push((off, query)));
        assert!(
            early.is_empty(),
            "a query split before its final byte is not done"
        );
        let mut seen = Vec::new();
        modes.feed_reporting(b"p...", |off, query| seen.push((off, query)));
        assert_eq!(seen, vec![(1, Query::ColorSchemeMode { set: false })]);
    }

    #[test]
    fn a_shell_prompt_ends_colour_scheme_updates() {
        for mark in [
            &b"\x1b]133;A\x07"[..],
            b"\x1b]133;B\x1b\\",
            b"\x1b]133;D;0\x07",
        ] {
            let mut modes = TerminalModes::new();
            feed(&mut modes, b"\x1b[?2031h\x1b]133;C\x07");
            assert!(
                modes.is_on(COLOR_SCHEME_UPDATES),
                "a command start keeps it"
            );
            feed(&mut modes, mark);
            assert!(!modes.is_on(COLOR_SCHEME_UPDATES));
        }
        let mut modes = TerminalModes::new();
        feed(&mut modes, b"\x1b[?2031h\x1b]0;133;A\x07\x1b]1337;A\x07");
        assert!(
            modes.is_on(COLOR_SCHEME_UPDATES),
            "other OSCs leave it alone"
        );
    }

    #[test]
    fn a_full_reset_clears_everything() {
        let mut modes = TerminalModes::new();
        feed(&mut modes, b"\x1b[?2031h");
        assert!(modes.is_on(COLOR_SCHEME_UPDATES));
        feed(&mut modes, b"\x1bc");
        assert!(!modes.is_on(COLOR_SCHEME_UPDATES));
    }

    #[test]
    fn an_osc_payload_that_looks_like_a_mode_set_is_not_one() {
        let mut modes = TerminalModes::new();
        feed(&mut modes, b"\x1b]0;\x1b[?2031h\x07");
        assert!(
            !modes.is_on(COLOR_SCHEME_UPDATES),
            "a mode set inside an OSC payload is text"
        );
    }

    #[test]
    fn one_csi_may_carry_several_modes() {
        let mut modes = TerminalModes::new();
        feed(&mut modes, b"\x1b[?1000;2031h");
        assert!(modes.is_on(COLOR_SCHEME_UPDATES));
        feed(&mut modes, b"\x1b[?1000;2031l");
        assert!(!modes.is_on(COLOR_SCHEME_UPDATES));
    }

    #[test]
    fn a_sequence_split_across_chunks_is_still_seen() {
        let mut modes = TerminalModes::new();
        feed(&mut modes, b"\x1b[?20");
        assert!(!modes.is_on(COLOR_SCHEME_UPDATES));
        feed(&mut modes, b"31h");
        assert!(modes.is_on(COLOR_SCHEME_UPDATES));
    }
}
