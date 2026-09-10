//! Scoring Claude Code's output for a permission dialog. Spec §6.
//!
//! Pure by construction: no I/O, no state, no clock. The whole module is three
//! functions over `&str`, which is what lets the corpus in
//! `tests/fixtures/detector.toml` be the test suite.

use std::sync::LazyLock;

use regex::Regex;

/// A dialog is detected at this score or above. Spec §6.
///
/// It is the entire false-positive defence: one weak indicator scores 2 and is
/// ignored, the yes/no button pair scores 5 on its own.
pub const THRESHOLD: u32 = 3;

/// Only the last this-many lines of the buffer are scored. A dialog is always
/// the most recent output, and a tail bounds the cost of a scan that runs on
/// every read.
const TAIL_LINES: usize = 50;

/// Cursor movement. Replaced with a *space*, not with nothing: a cursor jump is
/// a visual gap, and collapsing it would join two unrelated words into a match
/// that was never on screen.
static ANSI_CURSOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\[[\d;]*[ABCDEFGHJKfsu]").expect("ANSI_CURSOR is a literal pattern")
});

/// Every other escape: two-character, CSI, and OSC-terminated-by-BEL.
static ANSI_ESCAPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~]|\][^\x07]*\x07)")
        .expect("ANSI_ESCAPE is a literal pattern")
});

/// Control characters, less `\t` (0x09) and `\n` (0x0A). Line structure is
/// load-bearing — the score reads a tail measured in lines.
static CONTROL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\x00-\x08\x0B-\x0C\x0E-\x1F]").expect("CONTROL is a literal pattern")
});

static MULTI_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" +").expect("MULTI_SPACE is a literal pattern"));

/// The "No" half of the button pair. Claude numbers it 2 or 3 depending on
/// whether the dialog offers a third option.
static YES_NO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[23][\.\)]\s*No|•\s*No").expect("YES_NO is a literal pattern"));

/// `(y/n)` at the end of the tail.
///
/// **Not multi-line, deliberately.** `$` means end of the haystack here, in
/// Rust and in Go's RE2 alike. Adding `(?m)` would score any `(y/n)` anywhere
/// in the scrollback at 3 — which is [`THRESHOLD`]. Spec §6 names this trap.
static YN_AT_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(y/n\)\s*$").expect("YN_AT_END is a literal pattern"));

/// A dialog that wants the word rather than a keypress.
///
/// **Not dot-matches-newline, deliberately.** Adding `(?s)` would make
/// `Enter.*yes` match across the lines of a button dialog — `1. Yes` above,
/// `Enter to approve` below — and answer it with a literal `yes`, which the
/// dialog reads as an edit to the prompt. The corpus pins that case.
static NEEDS_YES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Type.*yes|Enter.*yes|\(y/n\)").expect("NEEDS_YES is a literal pattern")
});

/// What [`is_prompt`] concluded about a buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// `score >= THRESHOLD`.
    pub detected: bool,
    /// The additive score from spec §6's table.
    pub score: u32,
    /// The indicators that matched, in spec §6's table order — descending by
    /// weight, so the debug log reads the way the table does.
    ///
    /// Not decoration. When Claude Code moves one of these strings the tool
    /// stops approving with no error at all, and this is what makes the debug
    /// log say *which* indicator went quiet. See `docs/plan.md`.
    pub hits: Vec<&'static str>,
}

/// Remove ANSI escapes and normalise whitespace. Spec §6, in that order.
#[must_use]
pub fn strip_ansi(text: &str) -> String {
    let text = ANSI_CURSOR.replace_all(text, " ");
    let text = ANSI_ESCAPE.replace_all(&text, "");
    let text = CONTROL.replace_all(&text, "");
    let text = text.replace('\r', " ");
    MULTI_SPACE.replace_all(&text, " ").into_owned()
}

/// Score a buffer against the shape of a permission dialog. Spec §6.
///
/// Strips first, then scores only the last 50 lines.
#[must_use]
pub fn is_prompt(text: &str) -> Detection {
    let clean = strip_ansi(text);
    let tail = tail_lines(&clean, TAIL_LINES);

    let mut score = 0;
    // Has anything appeared that can actually be *answered*? Buttons, an
    // "Enter to …" hint, a `(y/n)`. `Permission rule`, `Esc to cancel` and
    // `Tab to amend` are context: they corroborate a dialog, they are not one.
    let mut actionable = false;
    let mut hits = Vec::new();

    let has_yes =
        tail.contains("1. Yes") || tail.contains("1) Yes") || tail.contains("\u{2022} Yes");
    if has_yes && YES_NO.is_match(&tail) {
        score += 5;
        actionable = true;
        hits.push("yes_no_buttons");
    }
    if tail.contains("Enter to approve") || tail.contains("Enter to confirm") {
        score += 3;
        actionable = true;
        hits.push("enter_to_approve");
    }
    if YN_AT_END.is_match(&tail) {
        score += 3;
        actionable = true;
        hits.push("yn_prompt");
    }
    if tail.contains("Permission rule") {
        score += 3;
        hits.push("permission_rule");
    }
    if tail.contains("Esc to cancel") {
        score += 2;
        hits.push("esc_to_cancel");
    }
    if tail.contains("Tab to amend") {
        score += 2;
        hits.push("tab_to_amend");
    }

    Detection {
        // Both conditions. The second was found by running the wrapper against
        // a real PTY: a dialog arrives over several reads, and `Permission
        // rule` lands in an earlier one than the buttons. Scoring 3 on its own,
        // it was answered before there was anything to answer — and then the
        // buttons arrived and were answered again. `Esc to cancel` plus `Tab
        // to amend` sums to 4 and is the same mistake from the other end.
        //
        // Sending `\r` at a dialog that has not finished rendering is the
        // worst thing this tool can do: the keystroke lands somewhere nobody
        // chose.
        detected: score >= THRESHOLD && actionable,
        score,
        hits,
    }
}

/// Does this dialog want `yes\r` rather than `\r`? Spec §6.
///
/// Decides the answer's bytes and nothing else — it never gates whether an
/// answer is sent.
///
/// **The same last 50 lines [`is_prompt`] scores**, and for the same reason. Run over the whole buffer it reads scrollback the detector
/// never looked at: a `type yes to confirm` printed two hundred lines ago is
/// still inside the 10 KB buffer, and it turns an ordinary button dialog's
/// `\r` into a literal `yes` the dialog reads as an edit to the prompt.
#[must_use]
pub fn needs_yes(text: &str) -> bool {
    let clean = strip_ansi(text);
    NEEDS_YES.is_match(&tail_lines(&clean, TAIL_LINES))
}

/// The last `n` lines, rejoined. Fewer than `n` lines yields all of them.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let start = lines.len().saturating_sub(n);
    lines.get(start..).unwrap_or_default().join("\n")
}

#[cfg(test)]
mod tests {
    use super::{Detection, is_prompt, needs_yes, strip_ansi};
    use serde::Deserialize;

    /// `tests/fixtures/detector.toml`, spec §6's contract as data (D9).
    ///
    /// `deny_unknown_fields` throughout: a mistyped category would otherwise be
    /// skipped in silence, and a corpus that quietly runs fewer cases than it
    /// contains is worse than no corpus.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Corpus {
        dialog: Vec<DialogCase>,
        not_dialog: Vec<TextCase>,
        quoted_dialog: Vec<TextCase>,
        strip: Vec<StripCase>,
        needs_yes: Vec<NeedsYesCase>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DialogCase {
        name: String,
        min_score: u32,
        input: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TextCase {
        name: String,
        input: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StripCase {
        name: String,
        input: String,
        want: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct NeedsYesCase {
        name: String,
        input: String,
        want: bool,
    }

    fn corpus() -> Corpus {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/detector.toml");
        let text =
            std::fs::read_to_string(path).expect("the corpus is committed beside the source");
        toml::from_str(&text).expect("the corpus is valid TOML matching Corpus")
    }

    /// A failure has to name the case and show the working. `assertion failed`
    /// on case 14 of 27 wastes the corpus.
    fn describe(name: &str, d: &Detection) -> String {
        format!("{name:?}: score={} hits={:?}", d.score, d.hits)
    }

    /// Guards against the vacuous pass: a corpus that failed to load, or one
    /// that lost a category to a bad edit, would otherwise report green.
    #[test]
    fn corpus_loads_and_is_populated() {
        let c = corpus();
        assert!(!c.dialog.is_empty(), "no dialog cases");
        assert!(!c.not_dialog.is_empty(), "no not_dialog cases");
        assert!(!c.quoted_dialog.is_empty(), "no quoted_dialog cases");
        assert!(!c.strip.is_empty(), "no strip cases");
        assert!(!c.needs_yes.is_empty(), "no needs_yes cases");

        let total = c.dialog.len()
            + c.not_dialog.len()
            + c.quoted_dialog.len()
            + c.strip.len()
            + c.needs_yes.len();
        assert!(total >= 27, "corpus shrank to {total} cases, was 27");
    }

    #[test]
    fn real_dialogs_are_detected() {
        for case in corpus().dialog {
            let got = is_prompt(&case.input);
            assert!(
                got.detected,
                "missed a dialog — {}",
                describe(&case.name, &got)
            );
            assert!(
                got.score >= case.min_score,
                "scored below the floor of {} — {}",
                case.min_score,
                describe(&case.name, &got)
            );
        }
    }

    #[test]
    fn prose_is_not_a_dialog() {
        for case in corpus().not_dialog {
            let got = is_prompt(&case.input);
            assert!(
                !got.detected,
                "false positive — {}",
                describe(&case.name, &got)
            );
        }
    }

    /// Spec §6: a *quoted* dialog scores like a real one, and that is pinned,
    /// not lamented. Nothing in the byte stream separates the two. A change
    /// that makes these stop detecting is a regression — read §6 first.
    #[test]
    fn quoted_dialogs_are_detected_too_and_that_is_the_hazard() {
        for case in corpus().quoted_dialog {
            let got = is_prompt(&case.input);
            assert!(
                got.detected,
                "the hazard case stopped detecting; see spec §6 — {}",
                describe(&case.name, &got)
            );
        }
    }

    #[test]
    fn strip_ansi_matches_the_corpus() {
        for case in corpus().strip {
            let got = strip_ansi(&case.input);
            assert_eq!(
                got.trim(),
                case.want.trim(),
                "strip {:?}: input {:?}",
                case.name,
                case.input
            );
        }
    }

    #[test]
    fn needs_yes_matches_the_corpus() {
        for case in corpus().needs_yes {
            assert_eq!(
                needs_yes(&case.input),
                case.want,
                "needs_yes {:?}: input {:?}",
                case.name,
                case.input
            );
        }
    }

    /// `needs_yes` reads the same 50 lines the score does. Scrollback that
    /// mentions the word must not turn a button dialog's `\r` into `yes\r`.
    #[test]
    fn needs_yes_ignores_scrollback_outside_the_tail() {
        let stale = format!(
            "Type yes to confirm\n{}1. Yes\n2. No\nEnter to approve\n",
            "building\n".repeat(60)
        );
        assert!(is_prompt(&stale).detected, "the dialog itself must score");
        assert!(
            !needs_yes(&stale),
            "a `yes` 60 lines back is not this dialog's"
        );
    }

    /// Spec §6: the tail is 50 lines, so volume before a dialog must not push
    /// it out of scoring range. I5 covers the loop's side of this.
    #[test]
    fn volume_before_a_dialog_does_not_blind_the_score() {
        let noise = "x\n".repeat(5_000);
        let buffer = format!("{noise} 1. Yes\n 2. No\n Enter to approve\n Esc to cancel\n");
        assert!(is_prompt(&buffer).detected);
    }

    /// The 51st line back is out of range. Pins the tail as a real bound rather
    /// than an accident of the corpus being short.
    #[test]
    fn a_dialog_scrolled_past_the_tail_is_not_detected() {
        let dialog = " 1. Yes\n 2. No\n Enter to approve\n Esc to cancel";
        let buffer = format!("{dialog}\n{}", "x\n".repeat(60));
        assert!(!is_prompt(&buffer).detected);
    }

    #[test]
    fn hits_are_reported_in_table_order() {
        let got = is_prompt("Permission rule\n 1. Yes\n 2. No\n Enter to approve\n Esc to cancel");
        assert_eq!(
            got.hits,
            [
                "yes_no_buttons",
                "enter_to_approve",
                "permission_rule",
                "esc_to_cancel"
            ]
        );
    }
}
