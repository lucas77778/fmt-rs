//! Tokenizer for fmt-rs.
//!
//! The lexer turns a raw command string into a flat token stream that the
//! parser groups into an [`ast`](crate::ast) tree. Its design follows the scope
//! decisions for the permission-dialog use case:
//!
//! - **Words are kept raw.** A word is a maximal run of non-operator,
//!   non-blank bytes, but quotes and expansions inside it (`'…'`, `"…"`,
//!   `$(…)`, `${…}`, `` `…` ``, `<(…)`, `=(…)`) are scanned as opaque nested
//!   regions so an operator *inside* them never splits the word. The word's
//!   bytes are preserved verbatim — the printer emits them unchanged. This is
//!   the OPAQUE strategy: we never reformat the inside of an expansion, so we
//!   never risk changing its meaning.
//! - **Never guess.** Any unterminated quote / expansion / heredoc is a hard
//!   [`LexError`]; the caller bails and shows the original command. A silent
//!   mis-scan (which could display a semantically different command) is the one
//!   thing we must never do, so the balanced scanners are quote-aware and
//!   depth-aware rather than naive bracket counters.
//! - **Heredocs are detected and flushed.** `<<`/`<<-` capture their body up to
//!   the delimiter line, so the rest of the script is not mistaken for body
//!   text. The body is kept raw (OPAQUE) on the operator token.
//! - **Comments are detected** (so the parser can bail — comments are part of
//!   what the user approves and must not silently vanish).
//!
//! Byte offsets are recorded on every token so the parser can slice the
//! original source for constructs handled by raw passthrough (`[[ … ]]`,
//! `(( … ))`, …).

/// A structural operator — the tokens whose layout fmt-rs actually rewrites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    AndIf,    // &&
    OrIf,     // ||
    Pipe,     // |
    PipeAmp,  // |&
    Amp,      // &
    Semi,     // ;
    DSemi,    // ;;
    SemiAmp,  // ;&
    DSemiAmp, // ;;&
    SemiPipe, // ;|
    LParen,   // (
    RParen,   // )
    // redirections
    Less,      // <
    Great,     // >
    DGreat,    // >>
    LessAnd,   // <&
    GreatAnd,  // >&
    LessGreat, // <>
    Clobber,   // >|
    AndGreat,  // &>
    AndDGreat, // &>>
    DLess,     // <<  (heredoc)
    DLessDash, // <<- (heredoc)
    TLess,     // <<< (here-string)
}

/// The kind of a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
    Word,
    Op(Op),
    Comment,
    Newline,
    Eof,
}

/// A token plus its source span and (for heredoc operators) captured body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokKind,
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub col: u32,
    /// Raw here-document body, set on `<<`/`<<-` tokens after flushing.
    pub hdoc_body: Option<String>,
}

impl Token {
    /// The token's own source text.
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }
}

/// A fatal lexing error. On any of these the caller must bail (output the
/// original command unchanged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub message: &'static str,
    pub offset: usize,
}

impl core::fmt::Display for LexError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "lex error at byte {}: {}", self.offset, self.message)
    }
}

type R<T> = Result<T, LexError>;

fn err<T>(message: &'static str, offset: usize) -> R<T> {
    Err(LexError { message, offset })
}

/// Tokenize `src` into a token stream ending with [`TokKind::Eof`].
pub fn tokenize(src: &str) -> R<Vec<Token>> {
    Lexer::new(src).run()
}

struct Lexer<'a> {
    b: &'a [u8],
    pos: usize,
    line: u32,
    line_start: usize,
    tokens: Vec<Token>,
    /// `<<`/`<<-` operators awaiting their delimiter word and then their body.
    pending: Vec<Pending>,
}

struct Pending {
    tok_index: usize,
    strip_tabs: bool,
    delim: Option<String>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Lexer {
            b: src.as_bytes(),
            pos: 0,
            line: 1,
            line_start: 0,
            tokens: Vec::new(),
            pending: Vec::new(),
        }
    }

    fn peek(&self, off: usize) -> u8 {
        let i = self.pos + off;
        if i < self.b.len() { self.b[i] } else { 0 }
    }

    /// Advance `pos` to `np`, updating line/column bookkeeping for any newlines
    /// crossed. All position jumps go through here.
    fn bump_to(&mut self, np: usize) {
        let mut i = self.pos;
        while i < np {
            if self.b[i] == b'\n' {
                self.line += 1;
                self.line_start = i + 1;
            }
            i += 1;
        }
        self.pos = np;
    }

    fn push(&mut self, kind: TokKind, start: usize, end: usize, line: u32, col: u32) {
        self.tokens.push(Token { kind, start, end, line, col, hdoc_body: None });
    }

    fn run(mut self) -> R<Vec<Token>> {
        loop {
            self.skip_blanks();
            if self.pos >= self.b.len() {
                break;
            }
            let start = self.pos;
            let line = self.line;
            let col = (self.pos - self.line_start + 1) as u32;
            let c = self.b[self.pos];

            if c == b'\n' {
                self.bump_to(self.pos + 1);
                self.push(TokKind::Newline, start, start + 1, line, col);
                if !self.pending.is_empty() {
                    self.flush_heredocs()?;
                }
                continue;
            }
            if c == b'#' {
                let end = self.scan_comment();
                self.bump_to(end);
                self.push(TokKind::Comment, start, end, line, col);
                continue;
            }
            // Process substitution `<(` / `>(` is a WORD, not a redirect.
            if (c == b'<' || c == b'>') && self.peek(1) == b'(' {
                let end = scan_word(self.b, self.pos)?;
                self.bump_to(end);
                self.push(TokKind::Word, start, end, line, col);
                continue;
            }
            if let Some((op, len)) = match_operator(self.b, self.pos) {
                let end = self.pos + len;
                self.bump_to(end);
                self.push(TokKind::Op(op), start, end, line, col);
                self.note_heredoc_op(op);
                continue;
            }
            // Otherwise: a word.
            let end = scan_word(self.b, self.pos)?;
            if end == start {
                // No progress would loop forever; treat as a lex error.
                return err("unexpected byte", self.pos);
            }
            self.bump_to(end);
            self.push(TokKind::Word, start, end, line, col);
            self.set_heredoc_delim(start, end);
        }
        let p = self.pos;
        let line = self.line;
        let col = (p - self.line_start + 1) as u32;
        if !self.pending.is_empty() {
            return err("unterminated here-document", p);
        }
        self.push(TokKind::Eof, p, p, line, col);
        Ok(self.tokens)
    }

    fn skip_blanks(&mut self) {
        loop {
            if self.pos >= self.b.len() {
                return;
            }
            match self.b[self.pos] {
                b' ' | b'\t' | b'\r' => self.bump_to(self.pos + 1),
                // line continuation: backslash-newline joins lines
                b'\\' if self.peek(1) == b'\n' => self.bump_to(self.pos + 2),
                _ => return,
            }
        }
    }

    fn scan_comment(&self) -> usize {
        let mut j = self.pos;
        while j < self.b.len() && self.b[j] != b'\n' {
            j += 1;
        }
        j
    }

    fn note_heredoc_op(&mut self, op: Op) {
        let strip_tabs = match op {
            Op::DLess => false,
            Op::DLessDash => true,
            _ => return,
        };
        let tok_index = self.tokens.len() - 1;
        self.pending.push(Pending { tok_index, strip_tabs, delim: None });
    }

    /// If a heredoc operator is awaiting its delimiter, the just-scanned word
    /// supplies it.
    fn set_heredoc_delim(&mut self, start: usize, end: usize) {
        if let Some(p) = self.pending.iter_mut().rev().find(|p| p.delim.is_none()) {
            let raw = std::str::from_utf8(&self.b[start..end]).unwrap_or("");
            p.delim = Some(heredoc_delim(raw));
        }
    }

    /// After the newline that ends a line containing heredoc operators, consume
    /// each pending body up to its delimiter line. Bodies are kept raw.
    fn flush_heredocs(&mut self) -> R<()> {
        let pendings = std::mem::take(&mut self.pending);
        for p in pendings {
            let delim = match &p.delim {
                Some(d) => d.clone(),
                None => return err("here-document without delimiter", self.pos),
            };
            let body_start = self.pos;
            let mut body_end = body_start;
            let mut found = false;
            loop {
                if self.pos >= self.b.len() {
                    break;
                }
                let line_start = self.pos;
                let mut j = line_start;
                while j < self.b.len() && self.b[j] != b'\n' {
                    j += 1;
                }
                let line = &self.b[line_start..j];
                let cmp = if p.strip_tabs {
                    let mut k = 0;
                    while k < line.len() && line[k] == b'\t' {
                        k += 1;
                    }
                    &line[k..]
                } else {
                    line
                };
                if cmp == delim.as_bytes() {
                    // Delimiter line: ends the body. Consume it (and its newline).
                    body_end = line_start;
                    let next = if j < self.b.len() { j + 1 } else { j };
                    self.bump_to(next);
                    found = true;
                    break;
                }
                // Body line: include it (and its newline) and continue.
                let next = if j < self.b.len() { j + 1 } else { j };
                self.bump_to(next);
                body_end = next;
            }
            if !found {
                return err("unterminated here-document", body_start);
            }
            let body = std::str::from_utf8(&self.b[body_start..body_end])
                .unwrap_or("")
                .to_string();
            self.tokens[p.tok_index].hdoc_body = Some(body);
        }
        Ok(())
    }
}

/// Reduce a heredoc delimiter word to its match key by removing the quoting
/// that only affects whether the body is expanded (e.g. `<<'EOF'`, `<<"EOF"`,
/// `<<\EOF` all match a line `EOF`).
fn heredoc_delim(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => i += 1,
            b'\\' if i + 1 < b.len() => {
                out.push(b[i + 1] as char);
                i += 2;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

/// Longest-match a structural operator at `b[i]`. Returns `(op, len)`.
/// Note: `<(` / `>(` are intentionally NOT matched here — they begin a word
/// (process substitution) and are handled by the caller.
fn match_operator(b: &[u8], i: usize) -> Option<(Op, usize)> {
    let c = b[i];
    let p1 = if i + 1 < b.len() { b[i + 1] } else { 0 };
    let p2 = if i + 2 < b.len() { b[i + 2] } else { 0 };
    match c {
        b'&' => match (p1, p2) {
            (b'&', _) => Some((Op::AndIf, 2)),
            (b'>', b'>') => Some((Op::AndDGreat, 3)),
            (b'>', _) => Some((Op::AndGreat, 2)),
            _ => Some((Op::Amp, 1)),
        },
        b'|' => match p1 {
            b'|' => Some((Op::OrIf, 2)),
            b'&' => Some((Op::PipeAmp, 2)),
            _ => Some((Op::Pipe, 1)),
        },
        b';' => match (p1, p2) {
            (b';', b'&') => Some((Op::DSemiAmp, 3)),
            (b';', _) => Some((Op::DSemi, 2)),
            (b'&', _) => Some((Op::SemiAmp, 2)),
            (b'|', _) => Some((Op::SemiPipe, 2)),
            _ => Some((Op::Semi, 1)),
        },
        b'<' => match (p1, p2) {
            (b'<', b'-') => Some((Op::DLessDash, 3)),
            (b'<', b'<') => Some((Op::TLess, 3)),
            (b'<', _) => Some((Op::DLess, 2)),
            (b'&', _) => Some((Op::LessAnd, 2)),
            (b'>', _) => Some((Op::LessGreat, 2)),
            _ => Some((Op::Less, 1)),
        },
        b'>' => match (p1, p2) {
            (b'>', _) => Some((Op::DGreat, 2)),
            (b'&', _) => Some((Op::GreatAnd, 2)),
            (b'|', _) => Some((Op::Clobber, 2)),
            _ => Some((Op::Great, 1)),
        },
        b'(' => Some((Op::LParen, 1)),
        b')' => Some((Op::RParen, 1)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Word scanning + the shared quote-aware balanced scanners.
//
// All scanners take the full byte slice and an index, and return the index just
// past the construct (or a LexError on anything unterminated). They only ever
// match ASCII bytes and copy multibyte UTF-8 through untouched, so operating on
// bytes is safe for non-ASCII input.
// ---------------------------------------------------------------------------

fn at(b: &[u8], i: usize) -> u8 {
    if i < b.len() { b[i] } else { 0 }
}

/// Scan a maximal word starting at `start`. Quotes and expansions inside are
/// consumed as opaque regions so embedded operators do not split the word.
pub fn scan_word(b: &[u8], start: usize) -> R<usize> {
    let mut j = start;
    while j < b.len() {
        let c = b[j];
        // Opaque/extending regions:
        match c {
            b'\\' => {
                j = (j + 2).min(b.len());
                continue;
            }
            b'\'' => {
                j = single_quote(b, j)?;
                continue;
            }
            b'"' => {
                j = double_quote(b, j)?;
                continue;
            }
            b'`' => {
                j = backtick(b, j)?;
                continue;
            }
            b'$' if at(b, j + 1) == b'(' => {
                j = dollar_paren(b, j)?;
                continue;
            }
            b'$' if at(b, j + 1) == b'{' => {
                j = dollar_brace(b, j)?;
                continue;
            }
            b'=' if at(b, j + 1) == b'(' => {
                // array assignment RHS: arr=(...)
                j = paren_group(b, j + 1)?;
                continue;
            }
            b'<' | b'>' if at(b, j + 1) == b'(' => {
                // process substitution as (part of) a word
                j = paren_group(b, j + 1)?;
                continue;
            }
            b'@' | b'?' | b'*' | b'+' | b'!' if at(b, j + 1) == b'(' => {
                // extended glob: @(…) ?(…) *(…) +(…) !(…) — the paren group is
                // part of the word, and nests (e.g. !(*.@(jpg|png))).
                j = paren_group(b, j + 1)?;
                continue;
            }
            _ => {}
        }
        // Terminators (unquoted): whitespace, newline, structural operators.
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => break,
            b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')' => break,
            _ => j += 1, // ordinary char, including { } [ ] # = $name
        }
    }
    Ok(j)
}

fn single_quote(b: &[u8], i: usize) -> R<usize> {
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == b'\'' {
            return Ok(j + 1);
        }
        j += 1;
    }
    err("unterminated single quote", i)
}

fn double_quote(b: &[u8], i: usize) -> R<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'"' => return Ok(j + 1),
            b'\\' => j = (j + 2).min(b.len()),
            b'`' => j = backtick(b, j)?,
            b'$' if at(b, j + 1) == b'(' => j = dollar_paren(b, j)?,
            b'$' if at(b, j + 1) == b'{' => j = dollar_brace(b, j)?,
            _ => j += 1,
        }
    }
    err("unterminated double quote", i)
}

fn backtick(b: &[u8], i: usize) -> R<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j = (j + 2).min(b.len()),
            b'`' => return Ok(j + 1),
            _ => j += 1,
        }
    }
    err("unterminated backquote", i)
}

/// `$(...)` or `$((...))` — skips the `$`, then balances parens.
fn dollar_paren(b: &[u8], i: usize) -> R<usize> {
    paren_group(b, i + 1)
}

/// Scan a balanced parenthesis group beginning at `at` (`src[at] == b'('`),
/// quote-aware. Returns the index just past the matching `)`. The parser uses
/// this to slice `(( … ))` arithmetic commands opaquely.
pub fn scan_parens(src: &[u8], at: usize) -> R<usize> {
    if at >= src.len() || src[at] != b'(' {
        return err("expected '('", at);
    }
    paren_group(src, at)
}

/// Balance a parenthesized group starting at `b[i] == '('`, quote-aware and
/// recursing into nested expansions so a `)` inside a quote or `${…}` does not
/// close it prematurely. Covers subshells, `$()`, `$(())`, `(())`, `<()`,
/// `=()`.
fn paren_group(b: &[u8], i: usize) -> R<usize> {
    debug_assert_eq!(b[i], b'(');
    let mut j = i + 1;
    let mut depth = 1usize;
    while j < b.len() {
        match b[j] {
            b'\\' => j = (j + 2).min(b.len()),
            b'\'' => j = single_quote(b, j)?,
            b'"' => j = double_quote(b, j)?,
            b'`' => j = backtick(b, j)?,
            b'$' if at(b, j + 1) == b'(' => j = dollar_paren(b, j)?,
            b'$' if at(b, j + 1) == b'{' => j = dollar_brace(b, j)?,
            b'(' => {
                depth += 1;
                j += 1;
            }
            b')' => {
                depth -= 1;
                j += 1;
                if depth == 0 {
                    return Ok(j);
                }
            }
            _ => j += 1,
        }
    }
    err("unterminated parenthesis", i)
}

/// Balance a `${...}` group starting at `b[i] == '$'`, `b[i+1] == '{'`.
fn dollar_brace(b: &[u8], i: usize) -> R<usize> {
    debug_assert_eq!(b[i], b'$');
    debug_assert_eq!(b[i + 1], b'{');
    let mut j = i + 2;
    let mut depth = 1usize;
    while j < b.len() {
        match b[j] {
            b'\\' => j = (j + 2).min(b.len()),
            b'\'' => j = single_quote(b, j)?,
            b'"' => j = double_quote(b, j)?,
            b'`' => j = backtick(b, j)?,
            b'$' if at(b, j + 1) == b'(' => j = dollar_paren(b, j)?,
            b'$' if at(b, j + 1) == b'{' => j = dollar_brace(b, j)?,
            b'{' => {
                depth += 1;
                j += 1;
            }
            b'}' => {
                depth -= 1;
                j += 1;
                if depth == 0 {
                    return Ok(j);
                }
            }
            _ => j += 1,
        }
    }
    err("unterminated parameter expansion", i)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns (kind, text) pairs excluding the trailing Eof.
    fn toks(src: &str) -> Vec<(TokKind, String)> {
        tokenize(src)
            .unwrap()
            .into_iter()
            .filter(|t| t.kind != TokKind::Eof)
            .map(|t| (t.kind, src[t.start..t.end].to_string()))
            .collect()
    }

    fn kinds(src: &str) -> Vec<TokKind> {
        toks(src).into_iter().map(|(k, _)| k).collect()
    }

    fn words(src: &str) -> Vec<String> {
        toks(src)
            .into_iter()
            .filter(|(k, _)| *k == TokKind::Word)
            .map(|(_, s)| s)
            .collect()
    }

    #[test]
    fn simple_command() {
        assert_eq!(words("ls -la /tmp"), ["ls", "-la", "/tmp"]);
    }

    #[test]
    fn operators_and_pipes() {
        assert_eq!(
            kinds("a && b | c"),
            [
                TokKind::Word,
                TokKind::Op(Op::AndIf),
                TokKind::Word,
                TokKind::Op(Op::Pipe),
                TokKind::Word,
            ]
        );
    }

    #[test]
    fn redirections() {
        assert_eq!(
            toks("cmd >out 2>&1"),
            [
                (TokKind::Word, "cmd".into()),
                (TokKind::Op(Op::Great), ">".into()),
                (TokKind::Word, "out".into()),
                (TokKind::Word, "2".into()),
                (TokKind::Op(Op::GreatAnd), ">&".into()),
                (TokKind::Word, "1".into()),
            ]
        );
    }

    #[test]
    fn quotes_are_single_words() {
        assert_eq!(words("echo \"a b\" 'c d'"), ["echo", "\"a b\"", "'c d'"]);
    }

    #[test]
    fn command_substitution_is_one_word() {
        assert_eq!(words("echo $(date +%s)"), ["echo", "$(date +%s)"]);
    }

    #[test]
    fn nested_quotes_in_cmdsubst_in_dquotes() {
        assert_eq!(words("echo \"x $(foo \"y\") z\""), ["echo", "\"x $(foo \"y\") z\""]);
    }

    #[test]
    fn adversarial_param_expansion_not_truncated() {
        // The case the scope review flagged: a `}` and `)` live inside nested
        // expansion/quotes and must NOT close the ${...} early.
        let w = words("echo ${VAR:-$(awk 'END{print NR}' f)}");
        assert_eq!(w, ["echo", "${VAR:-$(awk 'END{print NR}' f)}"]);
    }

    #[test]
    fn array_assignment_is_one_word() {
        assert_eq!(words("arr=(1 2 3)"), ["arr=(1 2 3)"]);
    }

    #[test]
    fn extglob_is_one_word_and_nests() {
        assert_eq!(words("ls !(*.@(jpg|png))"), ["ls", "!(*.@(jpg|png))"]);
        assert_eq!(words("cp @(a|b).txt dest"), ["cp", "@(a|b).txt", "dest"]);
        assert_eq!(words("rm *.+(tmp|bak)"), ["rm", "*.+(tmp|bak)"]);
    }

    #[test]
    fn process_substitution_is_word_not_redirect() {
        assert_eq!(
            kinds("diff <(sort a) <(sort b)"),
            [TokKind::Word, TokKind::Word, TokKind::Word]
        );
        assert_eq!(words("diff <(sort a) <(sort b)"), ["diff", "<(sort a)", "<(sort b)"]);
    }

    #[test]
    fn comment_detected() {
        assert_eq!(
            toks("ls # list"),
            [(TokKind::Word, "ls".into()), (TokKind::Comment, "# list".into())]
        );
    }

    #[test]
    fn hash_inside_word_is_literal() {
        assert_eq!(words("echo a#b"), ["echo", "a#b"]);
    }

    #[test]
    fn subshell_parens_are_operators() {
        assert_eq!(
            kinds("(a; b)"),
            [
                TokKind::Op(Op::LParen),
                TokKind::Word,
                TokKind::Op(Op::Semi),
                TokKind::Word,
                TokKind::Op(Op::RParen),
            ]
        );
    }

    #[test]
    fn line_continuation_joins() {
        assert_eq!(words("a \\\n b"), ["a", "b"]);
    }

    #[test]
    fn here_string_is_not_a_heredoc() {
        assert_eq!(
            toks("cmd <<<word"),
            [
                (TokKind::Word, "cmd".into()),
                (TokKind::Op(Op::TLess), "<<<".into()),
                (TokKind::Word, "word".into()),
            ]
        );
    }

    #[test]
    fn heredoc_body_captured_and_flushed() {
        let src = "cat <<EOF\nhello\nworld\nEOF\necho done";
        let tokens = tokenize(src).unwrap();
        // The << op carries the body; the rest of the script is tokenized
        // normally (not swallowed as body).
        let dless = tokens.iter().find(|t| t.kind == TokKind::Op(Op::DLess)).unwrap();
        assert_eq!(dless.hdoc_body.as_deref(), Some("hello\nworld\n"));
        let after: Vec<String> = tokens
            .iter()
            .filter(|t| t.kind == TokKind::Word)
            .map(|t| t.text(src).to_string())
            .collect();
        assert_eq!(after, ["cat", "EOF", "echo", "done"]);
    }

    #[test]
    fn heredoc_dash_strips_leading_tabs_for_delim_match() {
        let src = "cat <<-END\n\thi\n\tEND\n";
        let tokens = tokenize(src).unwrap();
        let d = tokens.iter().find(|t| t.kind == TokKind::Op(Op::DLessDash)).unwrap();
        assert_eq!(d.hdoc_body.as_deref(), Some("\thi\n"));
    }

    #[test]
    fn quoted_heredoc_delimiter_matches_unquoted_line() {
        let src = "cat <<'EOF'\n$notexpanded\nEOF\n";
        let tokens = tokenize(src).unwrap();
        let d = tokens.iter().find(|t| t.kind == TokKind::Op(Op::DLess)).unwrap();
        assert_eq!(d.hdoc_body.as_deref(), Some("$notexpanded\n"));
    }

    #[test]
    fn unterminated_quote_is_error() {
        assert!(tokenize("echo 'unclosed").is_err());
        assert!(tokenize("echo \"unclosed").is_err());
        assert!(tokenize("echo $(unclosed").is_err());
        assert!(tokenize("echo ${unclosed").is_err());
    }

    #[test]
    fn unterminated_heredoc_is_error() {
        assert!(tokenize("cat <<EOF\nbody but no terminator\n").is_err());
    }

    #[test]
    fn utf8_words_preserved() {
        assert_eq!(words("echo 日本語 café"), ["echo", "日本語", "café"]);
    }

    /// The cardinal safety invariant: for ANY input, tokenizing either fails
    /// (→ caller bails) or the token slices + gaps reconstruct the input byte
    /// for byte. The lexer must never silently alter or drop source bytes.
    #[test]
    fn corpus_round_trip_or_bail_never_corrupts() {
        let corpus = [
            "ls -la",
            "cd /tmp && grep -rn TODO . | head -20",
            "git log --oneline | head",
            "FOO=bar BAZ=qux ./run.sh >out 2>&1",
            "echo \"hello $USER, $(date +%F)\"",
            "echo ${VAR:-$(awk 'END{print NR}' file)}",
            "diff <(sort a.txt) <(sort b.txt)",
            "arr=(one 'two three' $(echo four)); echo \"${arr[@]}\"",
            "find . -name '*.rs' -exec grep -l 'unsafe' {} \\;",
            "for f in *.txt; do mv \"$f\" \"${f%.txt}.md\"; done",
            "if [ -f .env ]; then source .env; fi",
            "cat <<EOF > out.txt\nline one\nline two with ) and } and ]]\nEOF",
            "python3 <<'PY'\nprint('a' if x else 'b')\nPY",
            "tar czf - dir | ssh host 'cat > backup.tgz'",
            "echo a#b c # trailing comment",
            "x=$((1 + 2 * 3)); echo $x",
            "[[ $x == *.json && -s $x ]] && jq . \"$x\"",
            "printf '%s\\n' \"${items[@]}\"",
            "日本語=テスト; echo \"$日本語\"",
            "",
            "   ",
            "echo )))",            // unbalanced — should bail, not corrupt
            "echo 'unterminated",  // unterminated — should bail
        ];
        for src in corpus {
            match tokenize(src) {
                Err(_) => {} // bail path: safe
                Ok(tokens) => {
                    let mut rebuilt = String::new();
                    let mut cursor = 0;
                    for t in &tokens {
                        rebuilt.push_str(&src[cursor..t.start]);
                        rebuilt.push_str(&src[t.start..t.end]);
                        cursor = t.end;
                    }
                    rebuilt.push_str(&src[cursor..]);
                    assert_eq!(rebuilt, src, "round-trip mismatch for: {src:?}");
                }
            }
        }
    }

    #[test]
    fn offsets_round_trip_reconstructs_source() {
        // Concatenating each token's source slice plus the gaps between them
        // must reproduce the input exactly.
        let src = "foo && bar | baz >out # c";
        let tokens = tokenize(src).unwrap();
        let mut rebuilt = String::new();
        let mut cursor = 0;
        for t in &tokens {
            rebuilt.push_str(&src[cursor..t.start]); // inter-token whitespace
            rebuilt.push_str(&src[t.start..t.end]);
            cursor = t.end;
        }
        rebuilt.push_str(&src[cursor..]);
        assert_eq!(rebuilt, src);
    }
}
