# fmt-rs

[![crates.io](https://img.shields.io/crates/v/fmt-rs.svg)](https://crates.io/crates/fmt-rs)
[![docs.rs](https://img.shields.io/docsrs/fmt-rs)](https://docs.rs/fmt-rs)
[![license](https://img.shields.io/crates/l/fmt-rs.svg)](LICENSE)

A tiny, dependency-free **Bash command formatter** written in Rust.

It pretty-prints shell commands so they are easy to read — its purpose is to
beautify the commands shown in [Claude Code](https://claude.com/claude-code)'s
permission dialog *before* you approve running them. Model-generated commands
often arrive cramped onto one line with messy spacing and stacked `;`
statements; fmt-rs turns them into a clean, reviewable layout.

```bash
$ echo 'cd /repo&&npm ci&&npm run build&&npm test&&npm run deploy' | fmt-rs
```
```bash
cd /repo && npm ci && npm run build && npm test && npm run deploy
```

…and when a chain is too wide for the line, it reflows at the operators:

```bash
$ echo 'cd /repo&&npm ci&&npm run build&&npm test&&npm run deploy' | FMTRS_WIDTH=40 fmt-rs
```
```bash
cd /repo &&
  npm ci &&
  npm run build &&
  npm test &&
  npm run deploy
```

## Why

The original implementation was a Node hook wrapping
[`mvdan-sh`](https://github.com/mvdan/sh) (the GopherJS build of `shfmt`):
~1.7 MiB of JavaScript and a Node cold-start on every command — hundreds of
milliseconds. fmt-rs is a single ~600 KiB static binary with **zero runtime
dependencies** that runs in a few milliseconds.

## Design

The pipeline mirrors `shfmt`'s stages, but the printer is **width-driven**
rather than position-driven:

```
command ──lexer──▶ tokens ──parser──▶ AST ──printer──▶ Doc ──pretty──▶ formatted
```

- **Lexer** (`src/lexer.rs`) — tokenizes words, operators, quotes, and
  redirections. Quotes and expansions (`'…'`, `"…"`, `$(…)`, `${…}`,
  `` `…` ``, `<(…)`, extglobs) are scanned as opaque, quote-aware regions so an
  operator *inside* them never splits a word. Here-documents are detected and
  flushed so the rest of the script is never mistaken for body text.
- **Parser** (`src/parser.rs`) — recursive descent over `&&`/`||` lists,
  `|`/`|&` pipelines, redirections, subshells, brace blocks, and
  `if`/`while`/`until`/`for`. `[[ … ]]` and `(( … ))` are sliced verbatim from
  the source.
- **Doc engine** (`src/doc.rs`) — a Wadler/Prettier-style pretty-printing
  algebra (`text`/`line`/`group`/`nest`/…). A `group` lays out on one line if it
  fits the target width, otherwise breaks. This is what powers long-chain
  reflow.
- **Printer** (`src/printer.rs`) — translates the AST into a `Doc`.

### Safety contract

fmt-rs never changes what a command *means*. It only reflows whitespace and
line breaks. On anything it is not fully confident about, it returns the
**original command unchanged** rather than risk displaying something different
from what will run:

- unparseable input, here-documents, function declarations, and `case`/`select`
  → passed through untouched;
- **comments are preserved** — a command containing `#` is passed through
  verbatim (a comment can be the reason you approve a command, so it must never
  silently vanish);
- after formatting, the output is re-checked to confirm it carries the exact
  same words as the input; if not, the original is used;
- inputs over 8 KiB are passed through as-is.

It is validated against a corpus of 200+ real-world and adversarial commands
(quotes hiding `)`/`]]`, nested `$()`, extglobs, fd redirections, …) with zero
corruption, plus 70+ unit tests.

## Install

From [crates.io](https://crates.io/crates/fmt-rs):

```bash
cargo install fmt-rs        # installs the `fmt-rs` binary
```

As a library:

```bash
cargo add fmt-rs
```

Requires Rust 1.88 or newer.

## Build from source

```bash
cargo build --release
# binary at target/release/fmt-rs
```

## Usage

Read a command on stdin, write the formatted command to stdout:

```bash
echo 'x=1;y=2; ls  -la  >/dev/null 2>&1' | fmt-rs
```

The target width defaults to 80 columns and can be overridden:

```bash
FMTRS_WIDTH=60 fmt-rs < command.sh
```

### As a Claude Code hook

In `--hook` mode, fmt-rs speaks the PreToolUse hook protocol: it reads the
hook's JSON payload on stdin and emits an `ask` + `updatedInput` response when
it reformats a Bash command, or `{}` (no-op) otherwise.

```jsonc
// ~/.claude/settings.json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "/path/to/fmt-rs --hook", "timeout": 10 }
        ]
      }
    ]
  }
}
```

The permission dialog then shows the formatted command for review. Commands
that are already clean fall through normally and can still be auto-allowed.

## Status

The formatter is complete and in use as a hook. See
[`PROGRESS.md`](PROGRESS.md) for the full design rationale, scope decisions
(what is reformatted vs. preserved verbatim vs. passed through), and milestones.

## Acknowledgements

The AST and formatting behavior are modeled on Daniel Martí's
[`mvdan/sh`](https://github.com/mvdan/sh). fmt-rs is an independent
reimplementation focused narrowly on the permission-dialog use case.
