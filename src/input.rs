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
    ///
    /// `Enter` is one of these, not a [`Hotkey`]: it belongs to the child
    /// except during a countdown, and whether a countdown is running is engine
    /// state the parser has no business knowing. The engine asks
    /// [`is_enter`].
    Key(Vec<u8>),
    /// Terminal-to-application traffic — a focus event, a paste marker, the
    /// *contents* of a paste, a reply to something the child asked. Forwarded,
    /// and **never** cancels: the operator did not press anything.
    Report(Vec<u8>),
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
    /// Between `ESC[200~` and `ESC[201~`. Everything here is pasted text, not
    /// typing: no byte in it was a keypress, so none of it is a [`Unit::Key`]
    /// and none of it cancels a countdown. Without this state the first
    /// character of a paste cancels the countdown and is swallowed with it,
    /// and the child receives a paste missing its first character inside
    /// intact brackets.
    ///
    /// The escape machine is off in here by design: a paste may contain an
    /// `ESC`, a `0x01`, or a complete-looking CSI, and all of it is literal.
    Paste,
}

/// Starts [`State::Paste`]. Matched exactly — a paste marker has no parameters
/// beyond the 200.
const PASTE_START: &[u8] = b"\x1b[200~";

/// Ends it.
const PASTE_END: &[u8] = b"\x1b[201~";

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
        if self.pending.is_empty() {
            self.state = State::Ground;
            return Vec::new();
        }
        // A paste is not half-read, it is half-*arrived*: the terminator is
        // still coming. Hand the child what has landed and stay in the state.
        if self.state == State::Paste {
            return vec![Unit::Report(std::mem::take(&mut self.pending))];
        }
        self.state = State::Ground;
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
        // Checked before the cap: a paste is legitimately longer than any
        // sequence, and PENDING_CAP is a bound on sequences.
        if self.state == State::Paste {
            self.step_paste(byte, out);
            return;
        }

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
            // Handled above, before the cap.
            State::Paste => {}
        }
    }

    /// Accumulate pasted text until the closing marker. Spec §9, D11.
    fn step_paste(&mut self, byte: u8, out: &mut Vec<Unit>) {
        self.pending.push(byte);
        if self.pending.ends_with(PASTE_END) {
            self.state = State::Ground;
            out.push(Unit::Report(std::mem::take(&mut self.pending)));
            return;
        }
        // A long paste is emitted in pieces rather than buffered whole. Only
        // the bytes that could still be a prefix of the terminator are held
        // back, so `pending` is bounded and no split ever hides the marker.
        if self.pending.len() >= PENDING_CAP {
            let keep = self
                .pending
                .split_off(self.pending.len() - (PASTE_END.len() - 1));
            out.push(Unit::Report(std::mem::replace(&mut self.pending, keep)));
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
            // The opening marker is the one sequence that changes mode.
            self.state = if bytes == PASTE_START {
                State::Paste
            } else {
                State::Ground
            };
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
    use super::{Hotkey, InputParser, PENDING_CAP, Unit, is_enter};

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
            let Unit::Key(bytes) = one(form) else {
                panic!("{form:?} must stay a key, not become a hotkey or a report");
            };
            assert!(is_enter(&bytes), "{form:?} was not recognised as Enter");
        }
        assert!(!is_enter(b"a"));
        assert!(!is_enter(b"\x1b[A"));
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

    /// A paste is terminal traffic end to end. Not one byte of it is a
    /// keystroke, so not one byte of it cancels a countdown — before this, the
    /// first character did both, and vanished into the cancel.
    #[test]
    fn a_paste_is_all_report_content_included() {
        let got = units(&[b"\x1b[200~hi\x1b[201~"]);
        assert_eq!(
            got,
            vec![
                Unit::Report(b"\x1b[200~".to_vec()),
                Unit::Report(b"hi\x1b[201~".to_vec()),
            ]
        );
    }

    /// Pasted text is literal. An `ESC`, a `0x01`, a complete-looking CSI —
    /// inside the brackets they are characters, not input the wrapper acts on.
    #[test]
    fn pasted_control_bytes_are_not_keys_or_hotkeys() {
        let got = units(&[b"\x1b[200~a\x01b\x1b[1;5Ac\x1b[201~"]);
        assert_eq!(
            got,
            vec![
                Unit::Report(b"\x1b[200~".to_vec()),
                Unit::Report(b"a\x01b\x1b[1;5Ac\x1b[201~".to_vec()),
            ]
        );
    }

    /// The terminator may be split across reads, and the pieces before it are
    /// handed on rather than held — a paste can be larger than any buffer.
    #[test]
    fn a_paste_split_across_reads_keeps_every_byte_in_order() {
        let got = units(&[b"\x1b[200~he", b"llo\x1b[2", b"01~"]);
        let joined: Vec<u8> = got
            .iter()
            .flat_map(|u| match u {
                Unit::Report(b) => b.clone(),
                other => panic!("not a report: {other:?}"),
            })
            .collect();
        assert_eq!(joined, b"\x1b[200~hello\x1b[201~");
    }

    /// A paste longer than the sequence cap is emitted in pieces, and the
    /// closing marker still lands — the held-back bytes are exactly the ones
    /// that could be a prefix of it.
    #[test]
    fn a_paste_past_the_cap_is_chunked_without_losing_the_terminator() {
        let body = "x".repeat(PENDING_CAP * 3);
        let mut input = b"\x1b[200~".to_vec();
        input.extend_from_slice(body.as_bytes());
        input.extend_from_slice(b"\x1b[201~");

        let got = units(&[&input]);
        assert!(got.len() > 2, "expected chunking, got {} units", got.len());
        let joined: Vec<u8> = got
            .iter()
            .flat_map(|u| match u {
                Unit::Report(b) => b.clone(),
                other => panic!("not a report: {other:?}"),
            })
            .collect();
        assert_eq!(joined, input);
    }

    /// The tick flushes what has arrived without ending the paste: the
    /// terminator is still coming, and the next byte is still pasted text.
    #[test]
    fn flushing_mid_paste_hands_over_content_and_stays_in_the_paste() {
        let mut parser = InputParser::new();
        assert_eq!(
            parser.feed(b"\x1b[200~ab"),
            vec![Unit::Report(b"\x1b[200~".to_vec())]
        );
        assert_eq!(parser.flush(), vec![Unit::Report(b"ab".to_vec())]);
        assert_eq!(
            parser.feed(b"c\x1b[201~"),
            vec![Unit::Report(b"c\x1b[201~".to_vec())]
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
        assert!(got.iter().all(|u| matches!(u, Unit::Key(_))));
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
