//! fmt-rs — a small, dependency-light shell-command formatter.
//!
//! It pretty-prints Bash/POSIX commands for display in Claude Code's
//! permission dialog. The pipeline mirrors mvdan/sh: parse a command into the
//! [`ast`] tree, then render it with the printer.

pub mod ast;
pub mod doc;
pub mod format;
pub mod json;
pub mod lexer;
pub mod parser;
pub mod printer;
