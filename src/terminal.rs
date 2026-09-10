//! The real terminal and the PTY's size. Spec §7.
//!
//! What is here is the measurement-independent half: the scroll-region
//! sequences and the scanner that decides when the region has been destroyed
//! and must be re-applied. Both are pure, so [`crate::engine`]'s invariant I12
//! is testable without a terminal.
//!
//! Raw mode, the PTY, the bar and teardown are tasks 1.1/1.3/1.4. What the
//! child actually does to the screen is measured in `docs/measurements.md`.

/// Spec §7. The bottom rows the wrapper owns; Claude gets the rest.
///
/// Rows, not pixels — a terminal is a cell grid. One row holds
/// `auto-approve OFF  delay 0s` with room to spare. The constant exists so a
/// second row is a one-line change rather than a rewrite.
pub const STATUS_ROWS: u16 = 1;

/// `DECSTBM` restoring the full-height scroll region.
///
/// Teardown writes this on every exit path (§13 I13). A leftover margin leaves
/// the user's shell rendering into a box, which looks like the shell broke.
pub const RESET_MARGIN: &[u8] = b"\x1b[r";

/// A CSI carries at most this many parameter bytes before the rest are
/// dropped. xterm caps the parameter *list* at 16; this caps the raw bytes,
/// which is the same defence against an unterminated CSI in binary output
/// growing a buffer without limit. Real sequences are a dozen bytes at most.
const PARAM_CAP: usize = 64;

/// `DECSTBM` for a terminal this tall — scrolling confined to
/// `1 ..= height - STATUS_ROWS`.
///
/// This is the mechanism that makes the reservation *physical*. Sizing the PTY
/// short is not enough: a `winsize` is advisory, the child's bytes still reach
/// the real terminal, and one scroll past the bottom takes the bar with it.
/// That is the bug this port exists to fix (D7).
///
/// **Setting a region homes the cursor.** At startup that is free — the screen
/// is being cleared anyway. Mid-stream it is not, which is what [`remargin`]
/// is for.
#[must_use]
pub fn margin(height: u16) -> Vec<u8> {
    let bottom = height.saturating_sub(STATUS_ROWS).max(1);
    format!("\x1b[1;{bottom}r").into_bytes()
}

/// [`margin`] wrapped in `DECSC`/`DECRC`, for re-applying mid-stream.
///
/// The bare form homes the cursor, so re-asserting it in the middle of the
/// child's paint would leave every subsequent byte landing at the top-left
/// until the next repaint.
///
/// It costs sharing the terminal's single cursor-save slot with the child.
/// Measured 2026-09-10: the child uses that slot exactly once, as an adjacent
/// pair in its first eight bytes, so nothing of ours can land inside it. That
/// is a measurement, not a guarantee — if `docs/measurements.md` stops saying
/// it, this is the line that has to change.
#[must_use]
pub fn remargin(height: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(b"\x1b7");
    out.extend_from_slice(&margin(height));
    out.extend_from_slice(b"\x1b8");
    out
}

/// Watches the child's output for anything that destroys the scroll region.
///
/// A margin is not state a wrapper sets once. Spec §7 lists what clears it:
/// the alternate screen, the child's own `DECSTBM`, and the two resets. This
/// is the reading half of §13 I1 — the stream is inspected, never rewritten.
///
/// It is a byte-level state machine rather than a `contains`, because an escape
/// sequence can be split across two PTY reads at any point. The machine *is*
/// the carry: feed it 4 KiB at a time and a sequence straddling the boundary is
/// still recognised.
///
/// Known limit: there is no string/OSC state, so the body of an OSC is scanned
/// as ordinary text. An OSC payload containing a `DECSTBM`-shaped substring
/// would register. Nothing emits that; the cost of being wrong is one
/// [`remargin`], which is why that form exists rather than the bare one.
#[derive(Debug, Default)]
pub struct MarginWatch {
    state: State,
    /// Parameter bytes of the CSI being read, `0x30..=0x3F`, capped.
    params: Vec<u8>,
    /// `?` — a private-mode sequence. `DECSTBM` is never private; `XTRESTORE`
    /// (`CSI ? Pm r`) always is, and must not be mistaken for it.
    private: bool,
    /// `!` intermediate — what makes `p` a soft reset rather than anything else.
    bang: bool,
    /// Any other intermediate, e.g. `$` in `DECCARA` (`CSI … $ r`), which is
    /// not a `DECSTBM` however much its final byte looks like one.
    other_intermediate: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Csi,
}

impl MarginWatch {
    /// A scanner sitting in the ground state, mid-sequence in nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk of child output. `true` if the scroll region is gone and
    /// the caller must re-apply [`remargin`].
    ///
    /// Reports at most once per chunk — the caller re-applies once either way,
    /// and the whole chunk is scanned regardless to keep the machine's state
    /// correct for the next one.
    ///
    /// `#[must_use]`: dropping this verdict is a silent I12 failure with no
    /// test and no compiler signal, and `must_use_candidate` is allowed
    /// workspace-wide so nothing else would catch it.
    #[must_use]
    pub fn feed(&mut self, chunk: &[u8]) -> bool {
        let mut clobbered = false;
        let mut rest = chunk;
        loop {
            // In Ground the only interesting byte is ESC. Skipping to it beats
            // a match arm per byte on a megabyte of build output, and this runs
            // in the pass-through path.
            if self.state == State::Ground {
                let Some(offset) = rest.iter().position(|&b| b == 0x1B) else {
                    break;
                };
                rest = rest.get(offset..).unwrap_or_default();
            }
            let Some((&byte, tail)) = rest.split_first() else {
                break;
            };
            clobbered |= self.step(byte);
            rest = tail;
        }
        clobbered
    }

    fn step(&mut self, byte: u8) -> bool {
        // ESC is unconditional in every state — a real terminal abandons
        // whatever it was parsing and starts a new sequence. Without this, a
        // truncated CSI followed by a real trigger (`ESC[38;5` then
        // `ESC[?1049h`, routine in garbled tool output) is scanned as prose
        // and missed.
        if byte == 0x1B {
            self.state = State::Escape;
            return false;
        }

        match self.state {
            State::Ground => false,
            State::Escape => match byte {
                // RIS — a hard reset takes the margins with everything else.
                b'c' => {
                    self.state = State::Ground;
                    true
                }
                b'[' => {
                    self.state = State::Csi;
                    self.params.clear();
                    self.private = false;
                    self.bang = false;
                    self.other_intermediate = false;
                    false
                }
                _ => {
                    self.state = State::Ground;
                    false
                }
            },
            State::Csi => self.step_csi(byte),
        }
    }

    fn step_csi(&mut self, byte: u8) -> bool {
        match byte {
            0x30..=0x3F => {
                if byte == b'?' && self.params.is_empty() {
                    self.private = true;
                } else if self.params.len() < PARAM_CAP {
                    self.params.push(byte);
                }
                false
            }
            0x20..=0x2F => {
                if byte == b'!' {
                    self.bang = true;
                } else {
                    self.other_intermediate = true;
                }
                false
            }
            0x40..=0x7E => {
                self.state = State::Ground;
                self.is_clobbering_final(byte)
            }
            // C0 controls and DEL: a real terminal executes or ignores them and
            // keeps parsing the sequence. Aborting here would miss
            // `ESC[?1049\0h`, which NUL-padded or truncated output produces.
            // ESC is already handled above, so this cannot wedge.
            _ => false,
        }
    }

    fn is_clobbering_final(&self, final_byte: u8) -> bool {
        match final_byte {
            // DECSTBM. Not `CSI ? Pm r` (XTRESTORE) and not `CSI … $ r`
            // (DECCARA) — both end in `r` and neither touches the margins.
            b'r' => !self.private && !self.bang && !self.other_intermediate,
            // DECSET/DECRST for an alternate screen, which has its own margins.
            // Parameters are a `;`-separated list and a legal DECSET combines
            // them — `ESC[?1049;1006h` enters the alt screen just as much as
            // `ESC[?1049h` does — so this is membership, not equality. 1047
            // and 47 are the older forms, still emitted by anything built
            // against a termcap without 1049.
            b'h' | b'l' => {
                self.private
                    && (self.has_param(b"1049") || self.has_param(b"1047") || self.has_param(b"47"))
            }
            // DECSTR, a soft reset. `!` is the intermediate that identifies it.
            b'p' => self.bang,
            _ => false,
        }
    }

    fn has_param(&self, want: &[u8]) -> bool {
        self.params.split(|&b| b == b';').any(|p| p == want)
    }
}

#[cfg(test)]
mod tests {
    use super::{MarginWatch, PARAM_CAP, RESET_MARGIN, STATUS_ROWS, margin, remargin};

    fn clobbers(chunks: &[&[u8]]) -> bool {
        let mut watch = MarginWatch::new();
        chunks.iter().fold(false, |acc, c| acc | watch.feed(c))
    }

    #[test]
    fn margin_reserves_the_bottom_rows() {
        assert_eq!(margin(24), b"\x1b[1;23r");
        assert_eq!(margin(50), b"\x1b[1;49r");
    }

    /// A terminal too short to split still gets a valid region rather than
    /// `ESC[1;0r`, which is malformed and which some terminals act on anyway.
    #[test]
    fn margin_never_emits_a_zero_row_region() {
        assert_eq!(margin(1), b"\x1b[1;1r");
        assert_eq!(margin(0), b"\x1b[1;1r");
        assert_eq!(margin(STATUS_ROWS), b"\x1b[1;1r");
    }

    #[test]
    fn reset_margin_is_the_full_height_form() {
        assert_eq!(RESET_MARGIN, b"\x1b[r");
    }

    /// Setting a region homes the cursor, so the mid-stream form has to put it
    /// back or the child's next byte lands at the top-left.
    #[test]
    fn remargin_restores_the_cursor_around_the_region() {
        assert_eq!(remargin(24), b"\x1b7\x1b[1;23r\x1b8");
    }

    // ── the triggers, spec §7 ────────────────────────────────────────────────

    #[test]
    fn entering_or_leaving_the_alternate_screen_clobbers_the_margin() {
        assert!(clobbers(&[b"\x1b[?1049h"]));
        assert!(clobbers(&[b"\x1b[?1049l"]));
    }

    /// A DECSET may carry several private modes at once and applies each. The
    /// alt screen is entered either way, so membership is the test, not
    /// equality against the whole parameter string.
    #[test]
    fn a_combined_decset_containing_1049_clobbers_the_margin() {
        assert!(clobbers(&[b"\x1b[?1049;1006h"]));
        assert!(clobbers(&[b"\x1b[?1006;1049h"]));
        assert!(clobbers(&[b"\x1b[?25;1049;2004l"]));
    }

    /// The pre-1049 spellings. Anything built against an older termcap still
    /// emits these, and §13 I12 does not qualify which sequence it means.
    #[test]
    fn the_legacy_alternate_screen_forms_clobber_the_margin() {
        assert!(clobbers(&[b"\x1b[?1047h"]));
        assert!(clobbers(&[b"\x1b[?47h"]));
    }

    /// Measured: this is Claude Code's first eight bytes, a full-height reset
    /// wrapped in DECSC/DECRC. It destroys a margin set before the child
    /// started, which is why §4 applies ours after.
    #[test]
    fn the_childs_startup_margin_reset_is_caught() {
        assert!(clobbers(&[b"\x1b7\x1b[r\x1b8"]));
    }

    #[test]
    fn the_child_setting_a_region_of_its_own_clobbers_the_margin() {
        assert!(clobbers(&[b"\x1b[1;40r"]));
    }

    #[test]
    fn a_soft_or_hard_reset_clobbers_the_margin() {
        assert!(clobbers(&[b"\x1b[!p"]));
        assert!(clobbers(&[b"\x1bc"]));
    }

    // ── what must not fire ───────────────────────────────────────────────────

    /// A false positive costs a `remargin` write in the middle of the child's
    /// paint. Cheap, but not free, and on every colour change it would be
    /// neither.
    #[test]
    fn ordinary_output_leaves_the_margin_alone() {
        assert!(!clobbers(&[b"Do you want to proceed?\n 1. Yes\n 2. No\n"]));
        assert!(!clobbers(&[b"\x1b[32mgreen\x1b[0m \x1b[1mbold\x1b[0m"]));
        assert!(!clobbers(&[b"\x1b[2J\x1b[H\x1b[10;20H\x1b[K"]));
        assert!(!clobbers(&[b"\x1b[?25l\x1b[?25h"]), "cursor hide/show");
        assert!(!clobbers(&[b"\x1b]0;a title\x07"]), "OSC title");
    }

    /// Everything else Claude Code turns on at startup, measured 2026-09-10.
    /// None of it touches the margins and all of it is frequent.
    #[test]
    fn the_childs_other_startup_sequences_leave_the_margin_alone() {
        assert!(!clobbers(&[b"\x1b[?2004h"]), "bracketed paste");
        assert!(!clobbers(&[b"\x1b[?1004h"]), "focus reporting");
        assert!(!clobbers(&[b"\x1b[?2031h"]), "theme notifications");
        assert!(!clobbers(&[b"\x1b[>4;2m"]), "modifyOtherKeys");
        assert!(!clobbers(&[b"\x1b[>1u\x1b[<u"]), "kitty keyboard");
        assert!(!clobbers(&[b"\x1b[>0q"]), "XTVERSION query");
        assert!(!clobbers(&[b"\x1b[c"]), "primary DA query");
    }

    #[test]
    fn other_private_modes_leave_the_margin_alone() {
        assert!(!clobbers(&[b"\x1b[?1000h"]), "mouse reporting");
        assert!(!clobbers(&[b"\x1b[4h"]), "insert mode");
        // These contain the digits without being the modes.
        assert!(!clobbers(&[b"\x1b[?10490h"]));
        assert!(!clobbers(&[b"\x1b[?471h"]));
    }

    /// Three finals that look like triggers and are not: `p` without `!`,
    /// `r` with the private marker (XTRESTORE), `r` with an intermediate
    /// (DECCARA).
    #[test]
    fn near_miss_finals_leave_the_margin_alone() {
        assert!(!clobbers(&[b"\x1b[0p"]), "p without the ! intermediate");
        assert!(!clobbers(&[b"\x1b[?2r"]), "XTRESTORE, not DECSTBM");
        assert!(!clobbers(&[b"\x1b[1;2;3;4$r"]), "DECCARA, not DECSTBM");
    }

    // ── split across reads ───────────────────────────────────────────────────

    /// A PTY read boundary lands wherever it lands. This is why the scanner is
    /// a state machine and not a `contains` over the chunk.
    #[test]
    fn a_sequence_split_across_two_reads_is_still_caught() {
        assert!(clobbers(&[b"\x1b[?10", b"49h"]));
        assert!(clobbers(&[b"\x1b", b"[?1049h"]));
        assert!(clobbers(&[b"\x1b[?1049", b"h"]));
        assert!(clobbers(&[b"\x1b", b"c"]));
        assert!(clobbers(&[b"\x1b[", b"!", b"p"]));
    }

    /// Byte at a time is the worst case the boundary can produce.
    #[test]
    fn a_sequence_split_at_every_byte_is_still_caught() {
        for seq in [
            &b"\x1b[?1049h"[..],
            &b"\x1b[?1049;1006h"[..],
            &b"\x1b[!p"[..],
            &b"\x1b[1;40r"[..],
            &b"\x1bc"[..],
        ] {
            let chunks: Vec<&[u8]> = seq.chunks(1).collect();
            assert!(clobbers(&chunks), "missed {seq:?} split byte-at-a-time");
        }
    }

    // ── robustness against garbage ───────────────────────────────────────────

    /// A real terminal treats ESC as an unconditional restart. A truncated CSI
    /// immediately followed by a trigger — routine when the child echoes
    /// binary or half a frame — must not swallow the trigger.
    #[test]
    fn an_escape_inside_a_csi_restarts_the_sequence() {
        assert!(clobbers(&[b"\x1b[38;5\x1b[?1049h"]));
        assert!(clobbers(&[b"\x1b[1;2;3\x1bc"]));
    }

    /// C0 and DEL inside a CSI are executed or ignored by a real terminal,
    /// which then keeps parsing. NUL padding and stray BEL are common in
    /// truncated tool output.
    #[test]
    fn a_control_byte_inside_a_csi_does_not_abort_it() {
        assert!(clobbers(&[b"\x1b[?1049\x00h"]));
        assert!(clobbers(&[b"\x1b[?1049\x07h"]));
        assert!(clobbers(&[b"\x1b[?1049\x7fh"]));
    }

    /// An unterminated CSI in binary output must not grow the parameter buffer
    /// without limit inside a long-lived wrapper process.
    #[test]
    fn an_unterminated_csi_cannot_grow_the_parameter_buffer() {
        let mut watch = MarginWatch::new();
        let mut garbage = vec![0x1B, b'['];
        garbage.extend(std::iter::repeat_n(b'1', 10_000));
        assert!(!watch.feed(&garbage));
        assert!(watch.params.len() <= PARAM_CAP);
        // And it still works afterwards.
        assert!(watch.feed(b"\x1b[?1049h"));
    }

    /// The machine must not stay armed after a sequence ends, or the next
    /// ordinary `r` in prose would read as a `DECSTBM`.
    #[test]
    fn the_scanner_returns_to_ground_after_a_sequence() {
        let mut watch = MarginWatch::new();
        assert!(watch.feed(b"\x1b[?1049h"));
        assert!(!watch.feed(b"regular text with an r in it"));
    }

    #[test]
    fn a_malformed_sequence_does_not_wedge_the_scanner() {
        let mut watch = MarginWatch::new();
        assert!(!watch.feed(b"\x1bZ"));
        assert!(watch.feed(b"\x1b[?1049h"));
    }

    /// The whole chunk is scanned, not abandoned at the first hit, so state is
    /// correct for the next one.
    #[test]
    fn a_trigger_after_a_trigger_in_one_chunk_leaves_ground_state() {
        let mut watch = MarginWatch::new();
        assert!(watch.feed(b"\x1b[?1049h text \x1b[!p more"));
        assert!(!watch.feed(b"plain"));
    }
}

// ── The real terminal (tasks 1.1, 1.3, 1.4) ──────────────────────────────────

use std::io::{self, Write};

/// The wrapper's claim on the real terminal: raw mode, the scroll region, and
/// the status rows.
///
/// Acquiring it changes global state, so releasing it is `Drop`'s job and not
/// a method anyone can forget to call (§13 I13).
#[derive(Debug)]
pub struct Terminal {
    width: u16,
    height: u16,
    watch: MarginWatch,
    raw: bool,
}

impl Terminal {
    /// Raw mode on, size read. The margin is **not** applied yet: the child
    /// resets it as its first act (measured, §7), so [`start`](Self::start)
    /// runs after the spawn.
    pub fn acquire() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let (width, height) = crossterm::terminal::size()?;
        Ok(Self {
            width,
            height,
            watch: MarginWatch::new(),
            raw: true,
        })
    }

    /// Clear the screen and take the scroll region. Spec §4 step 4.
    pub fn start(&mut self) -> io::Result<()> {
        let mut out = io::stderr();
        out.write_all(b"\x1b[2J\x1b[H")?;
        out.write_all(&margin(self.height))?;
        out.flush()
    }

    /// Columns. The child gets all of them; only rows are reserved.
    #[must_use]
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Rows the child gets — everything but ours, and never zero.
    #[must_use]
    pub fn pty_rows(&self) -> u16 {
        self.height.saturating_sub(STATUS_ROWS).max(1)
    }

    /// Re-read the size after a `SIGWINCH`. Spec §12.
    pub fn refresh_size(&mut self) -> io::Result<()> {
        let (width, height) = crossterm::terminal::size()?;
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Re-apply the scroll region, cursor preserved.
    pub fn apply_margin(&mut self) -> io::Result<()> {
        let mut out = io::stderr();
        out.write_all(&remargin(self.height))?;
        out.flush()
    }

    /// The child's bytes, byte for byte, to stdout — then the re-margin scan.
    ///
    /// Our own control sequences go to **stderr**, never stdout, so stdout
    /// stays exactly what the child wrote (§13 I1). Both land on the same tty
    /// and the loop is single-threaded, so ordering is the write order.
    pub fn pass_through(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut out = io::stdout();
        out.write_all(bytes)?;
        out.flush()?;
        if self.watch.feed(bytes) {
            self.apply_margin()?;
        }
        Ok(())
    }

    /// Draw the status bar on the row the child cannot reach. Spec §10.
    pub fn draw_status(&mut self, text: &str, colour: &str) -> io::Result<()> {
        if self.height < 1 {
            return Ok(());
        }
        let mut out = io::stderr();
        // Save cursor, jump to the reserved row, clear it, reset the character
        // set (ESC(B) so a child that switched to line-drawing does not turn
        // the bar into boxes, write, restore.
        write!(
            out,
            "\x1b7\x1b[{};1H\x1b[K\x1b(B\x1b[{colour}m{text}\x1b[0m\x1b8",
            self.height
        )?;
        out.flush()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Every exit path, including panic (§13 I13). A leftover margin leaves
        // the user's shell rendering into a box.
        let mut out = io::stderr();
        crate::ignore(out.write_all(RESET_MARGIN));
        if self.height >= 1 {
            crate::ignore(write!(out, "\x1b[{};1H\x1b[K", self.height));
        }
        crate::ignore(out.flush());
        if self.raw {
            crate::ignore(crossterm::terminal::disable_raw_mode());
        }
    }
}
