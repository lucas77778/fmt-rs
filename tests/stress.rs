//! Corpus-based stress test.
//!
//! Runs the formatter over a corpus of 200+ real-world and adversarial Bash
//! commands (`tests/corpus.json`) and asserts the safety invariants that must
//! hold for *every* input:
//!
//! 1. **No panic** — `format_command` returns for every input (implicit: a
//!    panic fails the test).
//! 2. **No corruption** — when the command is reformatted (output differs from
//!    input), the output carries the exact same multiset of word tokens. When
//!    it is left unchanged, it bailed, which is always safe.
//! 3. **Idempotence** — formatting the output again yields the same output.
//!
//! This runs purely in-process (no `bash`, no Python), so it executes on every
//! `cargo test`. For an additional `bash -n` syntax check, see
//! `scripts/stress_verify.py`.

use fmt_rs::format::format_command;
use fmt_rs::json::{self, Value};
use fmt_rs::lexer::{self, TokKind};

const WIDTH: isize = 80;

fn corpus() -> Vec<String> {
    let raw = include_str!("corpus.json");
    match json::parse(raw).expect("corpus.json must be valid JSON") {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => panic!("corpus.json must be a JSON array of strings"),
    }
}

/// Sorted multiset of word-token texts. Operators and whitespace may reflow;
/// words must be preserved exactly.
fn words(src: &str) -> Option<Vec<String>> {
    let toks = lexer::tokenize(src).ok()?;
    let mut w: Vec<String> = toks
        .iter()
        .filter(|t| t.kind == TokKind::Word)
        .map(|t| src[t.start..t.end].to_string())
        .collect();
    w.sort();
    Some(w)
}

#[test]
fn corpus_is_nonempty() {
    assert!(corpus().len() >= 200, "expected a substantial corpus");
}

#[test]
fn no_corruption_and_idempotent() {
    let mut formatted = 0usize;
    let mut bailed = 0usize;

    for cmd in corpus() {
        let out = format_command(&cmd, WIDTH);

        if out == cmd {
            bailed += 1;
            continue;
        }
        formatted += 1;

        // Words must be preserved exactly across formatting.
        let (Some(wi), Some(wo)) = (words(&cmd), words(&out)) else {
            panic!("re-lexing failed for: {cmd:?}");
        };
        assert_eq!(wi, wo, "word multiset changed while formatting: {cmd:?}\n -> {out:?}");

        // Formatting must be a fixed point.
        let out2 = format_command(&out, WIDTH);
        assert_eq!(out2, out, "not idempotent: {cmd:?}\n  once: {out:?}\n  twice: {out2:?}");
    }

    eprintln!("stress: {formatted} formatted, {bailed} bailed (passthrough)");
    assert!(formatted > 0, "expected at least some commands to be reformatted");
}
