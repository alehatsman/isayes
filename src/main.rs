//! `isayes` — run `claude` inside a PTY and answer its permission dialogs.
//!
//! The contract is `docs/spec.md`. This file is §3 (the CLI) and §4 (the
//! wiring) and holds no logic of its own: the engine decides, this acts.

use std::process::ExitCode;

use clap::Parser;
use isayes::debug::DebugLog;
use isayes::engine::{Action, Engine};
use isayes::events::{Child, Event, install_signals};
use isayes::ignore;
use isayes::terminal::Terminal;

const KEYS: &str = "\
Keys:
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
    match run(&cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("isayes: {err}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<u8> {
    // Signals before anything is acquired. Once raw mode is on, a SIGTERM
    // with the default disposition leaves the shell raw and boxed in; there
    // must be no window between taking the terminal and being able to give it
    // back (§13 I13).
    let signals = install_signals()?;

    // Raw mode and the size next: the PTY is sized from what is left after
    // the status rows (§4 step 1).
    let mut term = Terminal::acquire()?;
    let (mut child, events) =
        Child::spawn(&cli.claude_args, term.pty_rows(), term.width(), signals)?;

    // The margin goes on after the spawn, because the child's first act is a
    // full-height DECSTBM reset (measured — §7). MarginWatch catches that
    // reset and pass_through re-applies, which is what makes it stick.
    term.start()?;

    let mut log = DebugLog::open();
    log.line(&format!(
        "start delay={}s size={}x{} args={:?}",
        cli.delay,
        term.width(),
        term.pty_rows(),
        cli.claude_args
    ));

    let mut engine = Engine::new(cli.delay);
    let mut exit = 0u8;

    while let Ok(event) = events.recv() {
        // The producer already stamped it. Nothing in this loop reads the
        // clock (D8, audited in CLAUDE.md) — that is what keeps the engine
        // testable.
        let stamp = event.stamp();

        // The child's bytes reach the terminal before anything looks at them.
        // Detection never sits in the output path (§5, §13 I1).
        if let Event::Output(bytes, _) = &event {
            term.pass_through(bytes)?;
        }

        match &event {
            Event::Eof => {
                exit = child.wait();
                break;
            }
            Event::Winch(_) => {
                term.refresh_size()?;
                child.resize(term.pty_rows(), term.width());
                term.apply_margin()?;
            }
            _ => {}
        }

        let mut actions = engine.handle(event);
        if log.enabled()
            && let Some(detection) = engine.take_detection()
        {
            log.detection(&detection);
        }
        // A failed write comes back as an event rather than a retry, so the
        // loop cannot spin on a dead child (§13 I8).
        let mut failed_at = None;
        for action in actions.drain(..) {
            match action {
                Action::Answer(bytes) => {
                    log.line(&format!("ANSWER {bytes:?} (#{})", engine.approvals()));
                    if child.write(&bytes).is_err() {
                        log.line("ANSWER FAILED — child is gone");
                        // An answer only ever comes from a stamped event.
                        failed_at = stamp;
                    }
                }
                Action::Forward(bytes) => {
                    // Best effort: a child that cannot be written to is a
                    // child that is exiting, and `Eof` reports that properly a
                    // moment later.
                    ignore(child.write(&bytes));
                }
                Action::Status { text, colour } => term.draw_status(&text, colour)?,
                Action::ForceRedraw => {
                    log.line("force redraw");
                    child.force_redraw(term.pty_rows(), term.width());
                }
                Action::Exit(code) => {
                    child.kill();
                    return Ok(code);
                }
            }
        }
        if let Some(now) = failed_at {
            for action in engine.handle(Event::AnswerFailed(now)) {
                if let Action::Status { text, colour } = action {
                    term.draw_status(&text, colour)?;
                }
            }
        }
    }

    Ok(exit)
    // `term` drops here: full-height scroll region, cleared status rows, raw
    // mode off. Also on the `?` paths above and on a panic (§13 I13).
}
