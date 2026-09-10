//! `isayes` — run `claude` inside a PTY and answer its permission dialogs.
//!
//! The contract is `docs/spec.md`. What is here is §3, the CLI surface. The
//! event loop, the detector and the terminal (§4–§11) are not written yet;
//! until they are, the binary parses its arguments and says so.

use std::process::ExitCode;

use clap::Parser;

/// Exit code for "parsed fine, but the wrapper does not exist yet" (spec §12).
/// Deleted along with the stub in `main`.
const EX_NOT_IMPLEMENTED: u8 = 3;

const KEYS: &str = "\
Keys, once the wrapper runs:
  Ctrl+A       toggle auto-approve
  Ctrl+Up      delay + 1s          Ctrl+Down    delay - 1s
  Enter        approve now         any key      cancel the countdown

Examples:
  isayes
  isayes --delay 3
  isayes -- 'refactor this module'
  isayes -- --help          # Claude's help, not ours";

/// Run `claude` under a PTY and answer its permission prompts for you.
#[derive(Debug, Parser)]
#[command(name = "isayes", version, about, after_help = KEYS)]
struct Cli {
    /// Seconds to wait before answering a prompt; 0 answers immediately.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=60))]
    delay: u8,

    /// Arguments handed to `claude` verbatim. Put them after `--`.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CLAUDE_ARGS"
    )]
    claude_args: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    eprintln!("isayes: the wrapper is not implemented yet — see docs/spec.md §4.");
    eprintln!(
        "        parsed: delay={}s, claude args {:?}",
        cli.delay, cli.claude_args
    );

    ExitCode::from(EX_NOT_IMPLEMENTED)
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;
    use clap::error::ErrorKind;

    #[test]
    fn delay_defaults_to_zero() {
        assert_eq!(Cli::parse_from(["isayes"]).delay, 0);
    }

    #[test]
    fn delay_accepts_the_whole_range() {
        assert_eq!(Cli::parse_from(["isayes", "--delay", "60"]).delay, 60);
    }

    #[test]
    fn delay_above_sixty_is_a_usage_error() {
        let err = Cli::try_parse_from(["isayes", "--delay", "61"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn delay_below_zero_is_a_usage_error() {
        let err = Cli::try_parse_from(["isayes", "--delay", "-1"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn trailing_args_pass_through_verbatim() {
        let cli = Cli::parse_from(["isayes", "--", "--help"]);
        assert_eq!(cli.claude_args, ["--help"]);
    }

    #[test]
    fn a_bare_prompt_is_a_trailing_arg() {
        let cli = Cli::parse_from(["isayes", "--delay", "3", "--", "fix the build"]);
        assert_eq!(cli.delay, 3);
        assert_eq!(cli.claude_args, ["fix the build"]);
    }
}
