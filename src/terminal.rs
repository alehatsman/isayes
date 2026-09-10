//! The real terminal and the PTY's size. Spec §7.
//!
//! What is here is the measurement-independent half: the scroll-region
//! sequences and the scanner that decides when the region has been destroyed
//! and must be re-applied. Both are pure, so [`crate::engine`]'s invariant I12
//! is testable without a terminal.
//!
//! Raw mode, the PTY, the bar and teardown land once `docs/plan.md` task 1.0
//! has answered what Claude Code actually does to the screen — see D7's
//! overturn condition.

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

/// `DECSTBM` for a terminal this tall — scrolling confined to
/// `1 ..= height - STATUS_ROWS`.
///
/// This is the mechanism that makes the reservation *physical*. Sizing the PTY
/// short is not enough: a `winsize` is advisory, the child's bytes still reach
/// the real terminal, and one scroll past the bottom takes the bar with it.
/// That is the bug this port exists to fix (D7).
///
/// Setting the region homes the cursor, so a redraw belongs after it.
#[must_use]
pub fn margin(height: u16) -> Vec<u8> {
    let bottom = height.saturating_sub(STATUS_ROWS).max(1);
    format!("\x1b[1;{bottom}r").into_bytes()
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
/// Known limit: the body of an OSC string is scanned as ordinary text, so an
/// OSC payload that itself contained a `DECSTBM` would register. Nothing emits
/// that, and the cost of being wrong is one redundant re-margin.
#[derive(Debug, Default)]
pub struct MarginWatch {
    state: State,
    /// Parameter bytes of the CSI being read, `0x30..=0x3F`.
    params: Vec<u8>,
    /// Intermediate bytes, `0x20..=0x2F`. `!` is what makes `ESC[!p` a reset.
    intermediates: Vec<u8>,
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
    /// the caller must re-apply [`margin`].
    ///
    /// Reports at most once per chunk — the caller re-applies once either way,
    /// and scanning the whole chunk keeps the machine's state correct for the
    /// next one.
    pub fn feed(&mut self, chunk: &[u8]) -> bool {
        let mut clobbered = false;
        for &byte in chunk {
            clobbered |= self.step(byte);
        }
        clobbered
    }

    fn step(&mut self, byte: u8) -> bool {
        match self.state {
            State::Ground => {
                if byte == 0x1B {
                    self.state = State::Escape;
                }
                false
            }
            State::Escape => match byte {
                // RIS — a hard reset takes the margins with everything else.
                b'c' => {
                    self.state = State::Ground;
                    true
                }
                b'[' => {
                    self.state = State::Csi;
                    self.params.clear();
                    self.intermediates.clear();
                    false
                }
                // A nested ESC restarts the sequence rather than aborting it.
                0x1B => false,
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
                self.params.push(byte);
                false
            }
            0x20..=0x2F => {
                self.intermediates.push(byte);
                false
            }
            0x40..=0x7E => {
                self.state = State::Ground;
                self.is_clobbering_final(byte)
            }
            // Anything else aborts the sequence — that is what a real terminal
            // does with a malformed CSI.
            _ => {
                self.state = State::Ground;
                false
            }
        }
    }

    fn is_clobbering_final(&self, final_byte: u8) -> bool {
        match final_byte {
            // DECSTBM: the child set a region of its own, over ours.
            b'r' => true,
            // DECSET/DECRST 1049 — the alternate screen has its own margins.
            b'h' | b'l' => self.params == b"?1049",
            // DECSTR, a soft reset. `!` is the intermediate that identifies it.
            b'p' => self.intermediates.contains(&b'!'),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MarginWatch, RESET_MARGIN, STATUS_ROWS, margin};

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
    /// `ESC[1;0r`, which is a malformed sequence some terminals act on.
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

    // ── the four triggers, spec §7 ───────────────────────────────────────────

    #[test]
    fn entering_the_alternate_screen_clobbers_the_margin() {
        assert!(clobbers(&[b"\x1b[?1049h"]));
    }

    #[test]
    fn leaving_the_alternate_screen_clobbers_the_margin() {
        assert!(clobbers(&[b"\x1b[?1049l"]));
    }

    #[test]
    fn the_child_setting_its_own_region_clobbers_the_margin() {
        assert!(clobbers(&[b"\x1b[1;40r"]));
        assert!(clobbers(&[b"\x1b[r"]));
    }

    #[test]
    fn a_soft_reset_clobbers_the_margin() {
        assert!(clobbers(&[b"\x1b[!p"]));
    }

    #[test]
    fn a_hard_reset_clobbers_the_margin() {
        assert!(clobbers(&[b"\x1bc"]));
    }

    // ── what must not fire ───────────────────────────────────────────────────

    /// The overwhelming majority of what Claude emits. A false positive here
    /// costs a redundant repaint on every colour change.
    #[test]
    fn ordinary_output_leaves_the_margin_alone() {
        assert!(!clobbers(&[b"Do you want to proceed?\n 1. Yes\n 2. No\n"]));
        assert!(!clobbers(&[b"\x1b[32mgreen\x1b[0m \x1b[1mbold\x1b[0m"]));
        assert!(!clobbers(&[b"\x1b[2J\x1b[H\x1b[10;20H\x1b[K"]));
        assert!(!clobbers(&[b"\x1b[?25l\x1b[?25h"]), "cursor hide/show");
        assert!(!clobbers(&[b"\x1b]0;a title\x07"]), "OSC title");
    }

    /// `?1049` is the alternate screen; `?1000` is mouse reporting and has
    /// nothing to do with margins. The parameter has to be compared, not
    /// searched for.
    #[test]
    fn other_private_modes_leave_the_margin_alone() {
        assert!(!clobbers(&[b"\x1b[?1000h"]));
        assert!(!clobbers(&[b"\x1b[?2004h"]), "bracketed paste");
        assert!(!clobbers(&[b"\x1b[4h"]), "insert mode");
    }

    /// `p` is only a reset with the `!` intermediate. Bare `ESC[0p` is not.
    #[test]
    fn p_without_the_bang_intermediate_is_not_a_reset() {
        assert!(!clobbers(&[b"\x1b[0p"]));
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
        let seq = b"\x1b[?1049h";
        let chunks: Vec<&[u8]> = seq.chunks(1).collect();
        assert!(clobbers(&chunks));
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
        // ESC followed by something that starts nothing.
        assert!(!watch.feed(b"\x1bZ"));
        // The scanner still works afterwards.
        assert!(watch.feed(b"\x1b[?1049h"));
    }
}
