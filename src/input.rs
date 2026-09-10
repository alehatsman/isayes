//! Splitting stdin into units and classifying them. Spec §9, decision D11.
//!
//! The mirror of [`crate::terminal::MarginWatch`]: the same byte-level state
//! machine, pointed at what arrives from the terminal instead of at what the
//! child emits. Pure — no I/O, no clock.
//!
//! It exists because matching raw bytes against §9's key table is wrong twice
//! over, both measured (`docs/measurements.md`):
//!
//! - The child turns on `modifyOtherKeys=2` and the Kitty keyboard protocol,
//!   so `Ctrl+A` does not arrive as `0x01` on any terminal that honours either.
//! - The child turns on focus reporting, bracketed paste and theme
//!   notifications, and queries the terminal twice. Those replies are bytes on
//!   stdin that are not keystrokes, and treating them as "any other key" both
//!   cancels a countdown nobody asked to cancel and eats a reply the child is
//!   waiting for.

/// A key this wrapper acts on itself. Never forwarded to the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// `Ctrl+A` — toggle auto-approve.
    Toggle,
    /// `Ctrl+Up` — one second more.
    DelayUp,
    /// `Ctrl+Down` — one second less.
    DelayDown,
}

/// One complete thing read from stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unit {
    /// Acted on here (§9). Never reaches the child.
    Hotkey(Hotkey),
    /// A real keystroke. Forwarded, and cancels a running countdown.
    Key(Vec<u8>),
    /// Terminal-to-application traffic — a focus event, a paste marker, a
    /// reply to something the child asked. Forwarded, and **never** cancels:
    /// the operator did not press anything.
    Report(Vec<u8>),
}

impl Unit {
    /// Is this the `Enter` key, in any encoding the child may have asked for?
    ///
    /// `Enter` is not a [`Hotkey`]: it belongs to the child except during a
    /// countdown, and whether a countdown is running is engine state the
    /// parser has no business knowing. So it stays a [`Unit::Key`] and the
    /// engine asks.
    #[must_use]
    pub fn is_enter(&self) -> bool {
        matches!(self, Self::Key(bytes) if is_enter(bytes))
    }

    /// The bytes to forward to the child, if any.
    #[must_use]
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Hotkey(_) => None,
            Self::Key(b) | Self::Report(b) => Some(b),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Csi,
    /// `ESC O` — SS3, one byte follows. `ESC O A` is Up in application mode,
    /// and is not the same thing as `ESC [ O`, which is focus-out.
    Ss3,
    /// DCS, OSC, APC, PM or SOS: a string that runs until `ESC \` or `BEL`.
    StringSeq,
    /// Inside a string sequence, having just seen an `ESC` that may be the ST.
    StringSeqEsc,
}

/// Splits stdin into [`Unit`]s across read boundaries.
#[derive(Debug, Default)]
pub struct InputParser {
    state: State,
    /// The sequence being assembled, `ESC` included.
    pending: Vec<u8>,
}

/// A sequence longer than this is not one. Bounds `pending` against a terminal
/// that starts a string sequence and never terminates it.
const PENDING_CAP: usize = 1024;

impl InputParser {
    /// A parser in the ground state, holding nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one read from stdin. Returns every unit that completed within it;
    /// a partial sequence is held for the next call.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Unit> {
        let mut out = Vec::new();
        for &byte in chunk {
            self.step(byte, &mut out);
        }
        out
    }

    /// Resolve whatever is held. Call this when a read did not continue a
    /// pending sequence — on the next tick is soon enough.
    ///
    /// This is what makes a bare `Esc` work at all: `ESC` alone is
    /// indistinguishable from the start of `ESC[A` until either more bytes
    /// arrive or they do not. §9 binds nothing to bare `Esc`, so deciding it a
    /// read late costs nothing (D11).
    pub fn flush(&mut self) -> Vec<Unit> {
        self.state = State::Ground;
        if self.pending.is_empty() {
            return Vec::new();
        }
        let bytes = std::mem::take(&mut self.pending);
        vec![Unit::Key(bytes)]
    }

    /// Is a sequence half-read? The caller uses this to decide whether
    /// [`flush`](Self::flush) has anything to do.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn step(&mut self, byte: u8, out: &mut Vec<Unit>) {
        if self.pending.len() >= PENDING_CAP {
            // Not a sequence. Give the bytes back rather than growing forever.
            let bytes = std::mem::take(&mut self.pending);
            out.push(Unit::Key(bytes));
            self.state = State::Ground;
        }

        match self.state {
            State::Ground => self.step_ground(byte, out),
            State::Escape => self.step_escape(byte, out),
            State::Csi => self.step_csi(byte, out),
            State::Ss3 => {
                self.pending.push(byte);
                let bytes = std::mem::take(&mut self.pending);
                self.state = State::Ground;
                out.push(Unit::Key(bytes));
            }
            State::StringSeq => {
                self.pending.push(byte);
                match byte {
                    0x07 => self.finish_string(out),
                    0x1B => self.state = State::StringSeqEsc,
                    _ => {}
                }
            }
            State::StringSeqEsc => {
                self.pending.push(byte);
                if byte == b'\\' {
                    self.finish_string(out);
                } else {
                    self.state = State::StringSeq;
                }
            }
        }
    }

    fn step_ground(&mut self, byte: u8, out: &mut Vec<Unit>) {
        if byte == 0x1B {
            self.state = State::Escape;
            self.pending.push(byte);
            return;
        }
        out.push(match byte {
            // `Ctrl+A` as the terminal sends it when no keyboard protocol is
            // in play. Still the common case: Terminal.app implements neither.
            0x01 => Unit::Hotkey(Hotkey::Toggle),
            _ => Unit::Key(vec![byte]),
        });
    }

    fn step_escape(&mut self, byte: u8, out: &mut Vec<Unit>) {
        match byte {
            b'[' => {
                self.state = State::Csi;
                self.pending.push(byte);
            }
            b'O' => {
                self.state = State::Ss3;
                self.pending.push(byte);
            }
            // DCS, OSC, SOS, PM, APC — everything that runs until a terminator.
            b'P' | b']' | b'X' | b'^' | b'_' => {
                self.state = State::StringSeq;
                self.pending.push(byte);
            }
            // A second ESC: the first one was a real `Esc` press.
            0x1B => {
                let bytes = std::mem::take(&mut self.pending);
                out.push(Unit::Key(bytes));
                self.pending.push(byte);
            }
            // A complete two-byte escape, e.g. `Alt+x`.
            _ => {
                self.pending.push(byte);
                let bytes = std::mem::take(&mut self.pending);
                self.state = State::Ground;
                out.push(Unit::Key(bytes));
            }
        }
    }

    fn step_csi(&mut self, byte: u8, out: &mut Vec<Unit>) {
        self.pending.push(byte);
        // Parameters, intermediates, then a final byte ends it. An ESC restarts
        // — the same rule MarginWatch needs, for the same reason.
        if byte == 0x1B {
            let mut bytes = std::mem::take(&mut self.pending);
            bytes.pop();
            out.push(Unit::Key(bytes));
            self.pending.push(0x1B);
            self.state = State::Escape;
            return;
        }
        if (0x40..=0x7E).contains(&byte) {
            let bytes = std::mem::take(&mut self.pending);
            self.state = State::Ground;
            out.push(classify_csi(bytes));
        }
    }

    fn finish_string(&mut self, out: &mut Vec<Unit>) {
        let bytes = std::mem::take(&mut self.pending);
        self.state = State::Ground;
        // Every string sequence arriving on stdin is an answer to something the
        // child asked — XTVERSION's DCS reply is the measured one.
        out.push(Unit::Report(bytes));
    }
}

/// Is this the `Enter` key, in any encoding the child may have asked for?
///
/// Free rather than a method so the engine can ask before it consumes the
/// bytes it is about to forward.
#[must_use]
pub fn is_enter(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        b"\r" | b"\n" | b"\x1b[13u" | b"\x1b[27;1;13~" | b"\x1b[27;5;13~"
    )
}

/// Decide what a complete CSI sequence is. `seq` includes `ESC[` and the final.
fn classify_csi(seq: Vec<u8>) -> Unit {
    let Some((&final_byte, rest)) = seq.split_last() else {
        return Unit::Key(seq);
    };
    let body = rest.get(2..).unwrap_or_default();
    let private = body
        .first()
        .is_some_and(|&b| b == b'?' || b == b'>' || b == b'<');
    let params = parse_params(body);

    // Reports first: a query reply that looked like a key would be swallowed,
    // and the child is blocking on it.
    if private
        || final_byte == b'R'
        || final_byte == b'n'
        || final_byte == b'I'
        || final_byte == b'O'
    {
        return Unit::Report(seq);
    }
    // Bracketed paste markers bracket the paste; they are not keystrokes.
    if final_byte == b'~' && matches!(params.first(), Some(200 | 201)) {
        return Unit::Report(seq);
    }

    if let Some(hotkey) = csi_hotkey(final_byte, &params) {
        return Unit::Hotkey(hotkey);
    }
    Unit::Key(seq)
}

/// `Ctrl` is bit 2 of the xterm/kitty modifier encoding, which is `1 + mask`.
fn has_ctrl(modifier: Option<u16>) -> bool {
    modifier.is_some_and(|m| m.saturating_sub(1) & 0b100 != 0)
}

fn csi_hotkey(final_byte: u8, params: &[u16]) -> Option<Hotkey> {
    match final_byte {
        // Arrows with modifiers: `ESC[1;5A`. Kitty's disambiguate mode keeps
        // this legacy form for functional keys, so one arm covers both.
        b'A' if has_ctrl(params.get(1).copied()) => Some(Hotkey::DelayUp),
        b'B' if has_ctrl(params.get(1).copied()) => Some(Hotkey::DelayDown),
        // modifyOtherKeys=2: `ESC[27;<mods>;<codepoint>~`.
        b'~' if params.first() == Some(&27) => {
            let code = params.get(2).copied();
            let ctrl = has_ctrl(params.get(1).copied());
            match code {
                Some(97 | 65) if ctrl => Some(Hotkey::Toggle),
                _ => None,
            }
        }
        // Kitty keyboard: `ESC[<codepoint>;<mods>u`.
        b'u' => {
            let ctrl = has_ctrl(params.get(1).copied());
            match params.first() {
                Some(97 | 65) if ctrl => Some(Hotkey::Toggle),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `;`-separated decimal parameters. An empty parameter is 0, as in xterm.
fn parse_params(body: &[u8]) -> Vec<u16> {
    let digits: &[u8] = if body.first().is_some_and(|&b| !b.is_ascii_digit()) {
        body.get(1..).unwrap_or_default()
    } else {
        body
    };
    digits
        .split(|&b| b == b';')
        .map(|part| {
            part.iter()
                .filter(|b| b.is_ascii_digit())
                .fold(0u16, |acc, &b| {
                    acc.saturating_mul(10).saturating_add(u16::from(b - b'0'))
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Hotkey, InputParser, Unit};

    fn units(chunks: &[&[u8]]) -> Vec<Unit> {
        let mut parser = InputParser::new();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(parser.feed(chunk));
        }
        out.extend(parser.flush());
        out
    }

    fn one(bytes: &[u8]) -> Unit {
        let mut got = units(&[bytes]);
        assert_eq!(
            got.len(),
            1,
            "expected one unit from {bytes:?}, got {got:?}"
        );
        got.remove(0)
    }

    // ── hotkeys, in all three encodings (D11) ────────────────────────────────

    #[test]
    fn ctrl_a_raw() {
        assert_eq!(one(b"\x01"), Unit::Hotkey(Hotkey::Toggle));
    }

    /// `modifyOtherKeys=2`, which the child turns on at startup. Without this
    /// arm the toggle is dead on every terminal that honours it.
    #[test]
    fn ctrl_a_modify_other_keys() {
        assert_eq!(one(b"\x1b[27;5;97~"), Unit::Hotkey(Hotkey::Toggle));
        assert_eq!(one(b"\x1b[27;5;65~"), Unit::Hotkey(Hotkey::Toggle));
    }

    /// Kitty keyboard protocol, also turned on at startup.
    #[test]
    fn ctrl_a_kitty() {
        assert_eq!(one(b"\x1b[97;5u"), Unit::Hotkey(Hotkey::Toggle));
    }

    /// Enter stays a key — it is the child's unless a countdown is running,
    /// which the parser does not know about. `is_enter` is how the engine asks.
    #[test]
    fn enter_is_recognised_in_every_encoding_but_stays_a_key() {
        for form in [
            &b"\r"[..],
            &b"\n"[..],
            &b"\x1b[13u"[..],
            &b"\x1b[27;1;13~"[..],
        ] {
            let unit = one(form);
            assert!(unit.is_enter(), "{form:?} was not recognised as Enter");
            assert!(unit.bytes().is_some(), "{form:?} must still be forwardable");
        }
        assert!(!one(b"a").is_enter());
        assert!(!one(b"\x1b[A").is_enter());
    }

    #[test]
    fn ctrl_arrows_change_the_delay() {
        assert_eq!(one(b"\x1b[1;5A"), Unit::Hotkey(Hotkey::DelayUp));
        assert_eq!(one(b"\x1b[1;5B"), Unit::Hotkey(Hotkey::DelayDown));
        // Ctrl+Shift is still Ctrl.
        assert_eq!(one(b"\x1b[1;6A"), Unit::Hotkey(Hotkey::DelayUp));
    }

    /// Plain arrows are the child's business — they move its selection.
    #[test]
    fn unmodified_arrows_are_ordinary_keys() {
        assert_eq!(one(b"\x1b[A"), Unit::Key(b"\x1b[A".to_vec()));
        assert_eq!(one(b"\x1b[B"), Unit::Key(b"\x1b[B".to_vec()));
        assert_eq!(one(b"\x1bOA"), Unit::Key(b"\x1bOA".to_vec()));
        // Shift+Up: a modifier, but not Ctrl.
        assert_eq!(one(b"\x1b[1;2A"), Unit::Key(b"\x1b[1;2A".to_vec()));
    }

    /// `a` is not `Ctrl+A`, in any encoding.
    #[test]
    fn an_unmodified_key_is_not_the_hotkey() {
        assert_eq!(one(b"a"), Unit::Key(b"a".to_vec()));
        assert_eq!(one(b"\x1b[97;1u"), Unit::Key(b"\x1b[97;1u".to_vec()));
    }

    // ── reports: forwarded, never a cancel (D11) ─────────────────────────────

    /// The sharp one. Clicking away from the window during a countdown must not
    /// cancel the approval, and the child asked for these events.
    #[test]
    fn focus_events_are_reports() {
        assert_eq!(one(b"\x1b[I"), Unit::Report(b"\x1b[I".to_vec()));
        assert_eq!(one(b"\x1b[O"), Unit::Report(b"\x1b[O".to_vec()));
    }

    #[test]
    fn paste_markers_are_reports_and_the_paste_itself_is_not() {
        let got = units(&[b"\x1b[200~hi\x1b[201~"]);
        assert_eq!(
            got,
            vec![
                Unit::Report(b"\x1b[200~".to_vec()),
                Unit::Key(b"h".to_vec()),
                Unit::Key(b"i".to_vec()),
                Unit::Report(b"\x1b[201~".to_vec()),
            ]
        );
    }

    /// Replies to the two queries the child makes at startup. Swallowing one
    /// leaves the child waiting for an answer that never comes.
    #[test]
    fn query_replies_are_reports() {
        assert_eq!(
            one(b"\x1b[?62;1;6c"),
            Unit::Report(b"\x1b[?62;1;6c".to_vec())
        );
        assert_eq!(
            one(b"\x1bP>|WezTerm\x1b\\"),
            Unit::Report(b"\x1bP>|WezTerm\x1b\\".to_vec())
        );
        assert_eq!(one(b"\x1b[?997;1n"), Unit::Report(b"\x1b[?997;1n".to_vec()));
        assert_eq!(one(b"\x1b[12;40R"), Unit::Report(b"\x1b[12;40R".to_vec()));
    }

    /// A kitty *key* is `ESC[97;5u`; the kitty *flags reply* is `ESC[?1u`. The
    /// private marker is the only thing telling them apart.
    #[test]
    fn the_kitty_flags_reply_is_a_report_not_a_key() {
        assert_eq!(one(b"\x1b[?1u"), Unit::Report(b"\x1b[?1u".to_vec()));
    }

    // ── split reads ──────────────────────────────────────────────────────────

    #[test]
    fn a_sequence_split_across_reads_is_assembled() {
        assert_eq!(
            units(&[b"\x1b[27;5", b";97~"]),
            vec![Unit::Hotkey(Hotkey::Toggle)]
        );
        assert_eq!(
            units(&[b"\x1b", b"[1;5A"]),
            vec![Unit::Hotkey(Hotkey::DelayUp)]
        );
        assert_eq!(
            units(&[b"\x1b[2", b"0", b"0~"]),
            vec![Unit::Report(b"\x1b[200~".to_vec())]
        );
    }

    #[test]
    fn a_sequence_split_at_every_byte_is_assembled() {
        for seq in [&b"\x1b[97;5u"[..], &b"\x1b[27;5;97~"[..], &b"\x1b[1;5B"[..]] {
            let chunks: Vec<&[u8]> = seq.chunks(1).collect();
            let got = units(&chunks);
            assert!(
                matches!(got.as_slice(), [Unit::Hotkey(_)]),
                "byte-at-a-time {seq:?} gave {got:?}"
            );
        }
    }

    // ── the lone Esc problem (D11) ───────────────────────────────────────────

    /// `ESC` alone is indistinguishable from the start of `ESC[A` until more
    /// bytes arrive or they do not. It is held, then resolved on flush.
    #[test]
    fn a_lone_escape_is_held_then_resolved_as_a_key() {
        let mut parser = InputParser::new();
        assert!(parser.feed(b"\x1b").is_empty());
        assert!(parser.is_pending());
        assert_eq!(parser.flush(), vec![Unit::Key(b"\x1b".to_vec())]);
        assert!(!parser.is_pending());
    }

    #[test]
    fn escape_then_a_real_sequence_yields_both() {
        assert_eq!(
            units(&[b"\x1b\x1b[1;5A"]),
            vec![Unit::Key(b"\x1b".to_vec()), Unit::Hotkey(Hotkey::DelayUp)]
        );
    }

    #[test]
    fn nothing_pending_means_nothing_to_flush() {
        let mut parser = InputParser::new();
        assert_eq!(parser.feed(b"abc").len(), 3);
        assert!(!parser.is_pending());
        assert!(parser.flush().is_empty());
    }

    // ── ordinary typing ──────────────────────────────────────────────────────

    #[test]
    fn typing_is_one_unit_per_byte_and_all_forwardable() {
        let got = units(&[b"ls -la"]);
        assert_eq!(got.len(), 6);
        assert!(got.iter().all(|u| u.bytes().is_some()));
    }

    #[test]
    fn a_hotkey_is_never_forwarded() {
        assert_eq!(Unit::Hotkey(Hotkey::Toggle).bytes(), None);
    }

    #[test]
    fn alt_x_is_a_two_byte_key() {
        assert_eq!(one(b"\x1bx"), Unit::Key(b"\x1bx".to_vec()));
    }

    // ── robustness ───────────────────────────────────────────────────────────

    /// An unterminated string sequence must not grow the buffer without limit
    /// inside a long-lived process.
    #[test]
    fn an_unterminated_sequence_is_bounded_and_recovers() {
        let mut parser = InputParser::new();
        let mut garbage = vec![0x1B, b']'];
        garbage.extend(std::iter::repeat_n(b'x', 4_000));
        let out = parser.feed(&garbage);
        assert!(!out.is_empty(), "the buffer was never handed back");
        assert_eq!(parser.feed(b"\x01"), vec![Unit::Hotkey(Hotkey::Toggle)]);
    }

    #[test]
    fn an_escape_inside_a_csi_restarts_it() {
        assert_eq!(
            units(&[b"\x1b[1;\x1b[1;5A"]),
            vec![
                Unit::Key(b"\x1b[1;".to_vec()),
                Unit::Hotkey(Hotkey::DelayUp)
            ]
        );
    }
}
