//! The top-level formatting entry point and its safety gates.
//!
//! [`format_command`] is the single function callers use. It embodies the
//! "never block, never mislead" contract: on anything it is not fully
//! confident about, it returns the original command unchanged. The gates, in
//! order:
//!
//! 1. Oversize input (> [`MAX_INPUT`]) → passthrough (large heredocs etc.).
//! 2. Lex error → bail.
//! 3. Any comment token → bail (comments are part of what the user approves and
//!    must never silently vanish).
//! 4. Parse error → bail.
//! 5. **Output verification** → re-lex the formatted output and confirm it
//!    carries the exact same words as the input. Formatting may only reflow
//!    whitespace/operators; if a word changed or vanished, that is a bug and we
//!    bail rather than display a command that differs from what will run.

use crate::json;
use crate::lexer::{self, TokKind};
use crate::parser;
use crate::printer;

/// Inputs larger than this are passed through unchanged.
pub const MAX_INPUT: usize = 8000;

/// The no-op hook response: emit this to leave a tool call untouched.
pub const HOOK_NOOP: &str = "{}";

/// Build the PreToolUse hook response for a raw payload (the JSON Claude Code
/// pipes to a hook on stdin). Mirrors the v0 contract exactly:
///
/// - Only the `Bash` tool is touched; everything else is a no-op.
/// - When formatting changes the command, return `permissionDecision: "ask"`
///   with `updatedInput.command` so the dialog shows the formatted version.
/// - When nothing changes (or anything goes wrong), return `{}` so simple
///   commands follow the normal flow and can still be auto-allowed.
///
/// This function never fails — every error path collapses to [`HOOK_NOOP`].
pub fn hook_response(raw: &str, width: isize) -> String {
    if raw.trim().is_empty() {
        return HOOK_NOOP.to_string();
    }
    let Ok(payload) = json::parse(raw) else {
        return HOOK_NOOP.to_string();
    };
    if payload.get("tool_name").and_then(|v| v.as_str()) != Some("Bash") {
        return HOOK_NOOP.to_string();
    }
    let original = match payload
        .get("tool_input")
        .and_then(|t| t.get("command"))
        .and_then(|c| c.as_str())
    {
        Some(s) if !s.trim().is_empty() => s,
        _ => return HOOK_NOOP.to_string(),
    };

    let formatted = format_command(original, width);
    // format_command appends a trailing newline on success; normalize before
    // comparing so an already-clean command is correctly seen as unchanged.
    let cleaned = formatted.strip_suffix('\n').unwrap_or(&formatted);
    if cleaned == original {
        return HOOK_NOOP.to_string();
    }

    format!(
        concat!(
            "{{\"hookSpecificOutput\":{{",
            "\"hookEventName\":\"PreToolUse\",",
            "\"permissionDecision\":\"ask\",",
            "\"updatedInput\":{{\"command\":{}}}",
            "}}}}"
        ),
        json::encode_string(cleaned)
    )
}

/// Format a single Bash command for display. Never panics; never alters
/// meaning — falls back to the original string on any doubt.
pub fn format_command(input: &str, width: isize) -> String {
    match try_format(input, width) {
        Some(out) => out,
        None => input.to_string(),
    }
}

fn try_format(input: &str, width: isize) -> Option<String> {
    if input.len() > MAX_INPUT {
        return None;
    }
    let tokens = lexer::tokenize(input).ok()?;
    if tokens.iter().any(|t| t.kind == TokKind::Comment) {
        return None;
    }
    let file = parser::parse(input, &tokens).ok()?;
    let out = printer::format(&file, width);

    // Don't collapse a non-empty command to nothing.
    if out.trim().is_empty() && !input.trim().is_empty() {
        return None;
    }
    // Verification: the set of words must be identical before and after.
    if !same_words(input, &out) {
        return None;
    }
    Some(out)
}

/// Whether `a` and `b` contain the same multiset of word tokens. Operators and
/// whitespace may legitimately reflow; words must not change. Comparison is
/// order-insensitive so that legitimately reordered redirections (`>f cmd` →
/// `cmd >f`) don't trip the gate, while any dropped/altered word still does.
fn same_words(a: &str, b: &str) -> bool {
    let (Ok(ta), Ok(tb)) = (lexer::tokenize(a), lexer::tokenize(b)) else {
        return false;
    };
    let mut wa = word_texts(a, &ta);
    let mut wb = word_texts(b, &tb);
    wa.sort();
    wb.sort();
    wa == wb
}

fn word_texts(src: &str, toks: &[crate::lexer::Token]) -> Vec<String> {
    toks.iter()
        .filter(|t| t.kind == TokKind::Word)
        .map(|t| src[t.start..t.end].to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_a_normal_command() {
        assert_eq!(format_command("ls   -la", 80), "ls -la\n");
    }

    #[test]
    fn bails_to_original_on_comment() {
        let cmd = "rm -rf /tmp/build  # safe: only build artifacts";
        assert_eq!(format_command(cmd, 80), cmd);
    }

    #[test]
    fn bails_to_original_on_heredoc() {
        let cmd = "cat <<EOF\nhi\nEOF";
        assert_eq!(format_command(cmd, 80), cmd);
    }

    #[test]
    fn bails_to_original_on_function() {
        let cmd = "foo() { echo hi; }";
        assert_eq!(format_command(cmd, 80), cmd);
    }

    #[test]
    fn bails_on_oversize_input() {
        let big = format!("echo {}", "x".repeat(MAX_INPUT));
        assert_eq!(format_command(&big, 80), big);
    }

    #[test]
    fn bails_on_unparseable_without_panicking() {
        let cmd = "echo 'unterminated";
        assert_eq!(format_command(cmd, 80), cmd);
    }

    #[test]
    fn words_are_preserved_through_formatting() {
        let cmd = "cd /tmp && grep -rn TODO . | head";
        let out = format_command(cmd, 80);
        assert!(super::same_words(cmd, &out));
    }

    // -- hook response ----------------------------------------------------

    #[test]
    fn hook_formats_bash_command() {
        let raw = r#"{"tool_name":"Bash","tool_input":{"command":"ls   -la"}}"#;
        let out = hook_response(raw, 80);
        assert_eq!(
            out,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","updatedInput":{"command":"ls -la"}}}"#
        );
    }

    #[test]
    fn hook_noop_when_already_clean() {
        let raw = r#"{"tool_name":"Bash","tool_input":{"command":"ls -la"}}"#;
        assert_eq!(hook_response(raw, 80), HOOK_NOOP);
    }

    #[test]
    fn hook_noop_for_non_bash_tool() {
        let raw = r#"{"tool_name":"Read","tool_input":{"file_path":"/x"}}"#;
        assert_eq!(hook_response(raw, 80), HOOK_NOOP);
    }

    #[test]
    fn hook_noop_on_comment_command() {
        let raw = r#"{"tool_name":"Bash","tool_input":{"command":"rm x  # careful"}}"#;
        assert_eq!(hook_response(raw, 80), HOOK_NOOP);
    }

    #[test]
    fn hook_noop_on_garbage_payload() {
        assert_eq!(hook_response("not json", 80), HOOK_NOOP);
        assert_eq!(hook_response("", 80), HOOK_NOOP);
        assert_eq!(hook_response("{}", 80), HOOK_NOOP);
    }

    #[test]
    fn hook_preserves_quotes_through_json_round_trip() {
        let raw = r#"{"tool_name":"Bash","tool_input":{"command":"a=1;echo \"hi $a\""}}"#;
        let out = hook_response(raw, 80);
        // The formatted command (split on ;) is re-encoded as a JSON string.
        let parsed = crate::json::parse(&out).unwrap();
        let cmd = parsed
            .get("hookSpecificOutput")
            .and_then(|h| h.get("updatedInput"))
            .and_then(|u| u.get("command"))
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(cmd, "a=1\necho \"hi $a\"");
    }
}
