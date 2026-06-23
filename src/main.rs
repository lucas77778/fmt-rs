//! fmt-rs CLI.
//!
//! Two modes:
//!
//! - default: read a Bash command on stdin, write the formatted command to
//!   stdout. On anything it can't confidently format, it echoes the input back
//!   unchanged (the "never block" contract).
//! - `--hook`: act as a Claude Code PreToolUse hook. Read the hook's JSON
//!   payload on stdin and write the PreToolUse response on stdout — an
//!   `ask` + `updatedInput` envelope when the Bash command was reformatted, or
//!   `{}` (no-op) otherwise.
//!
//! Width defaults to 80 and can be overridden with `FMTRS_WIDTH`.

use std::io::{Read, Write};

use fmt_rs::format::{format_command, hook_response, HOOK_NOOP};
use fmt_rs::printer::DEFAULT_WIDTH;

fn main() {
    let hook_mode = std::env::args().skip(1).any(|a| a == "--hook");

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        // Can't read stdin: in hook mode stay invisible; otherwise emit nothing.
        if hook_mode {
            let _ = std::io::stdout().write_all(HOOK_NOOP.as_bytes());
        }
        return;
    }

    let width = std::env::var("FMTRS_WIDTH")
        .ok()
        .and_then(|s| s.trim().parse::<isize>().ok())
        .filter(|w| *w > 0)
        .unwrap_or(DEFAULT_WIDTH);

    let out = if hook_mode {
        hook_response(&input, width)
    } else {
        format_command(&input, width)
    };
    let _ = std::io::stdout().write_all(out.as_bytes());
}
