//! Recursive-descent parser: token stream → [`ast::File`].
//!
//! Scope (per the permission-dialog design decisions):
//! - **Words are kept raw.** Each word token becomes a `Word` of a single
//!   `Lit` holding the verbatim source text — quoting and expansions ride along
//!   untouched (the OPAQUE strategy). Assignment prefixes like `FOO=bar` are
//!   just words too; they render identically, so they need no special node.
//! - **`[[ … ]]` and `(( … ))` are sliced opaquely** from the source by byte
//!   offset and carried as one raw word, so their interiors are never
//!   reformatted (and never mis-parsed).
//! - **Bail, never guess.** Comments, here-documents, function declarations,
//!   and the rare `case`/`select`/`coproc`/`time` keywords return an error so
//!   the driver shows the original command unchanged. Any unexpected token is
//!   likewise an error.
//!
//! What it does structure: `&&`/`||` lists, `|`/`|&` pipelines, redirections,
//! subshells, brace blocks, and `if`/`while`/`until`/`for`. That covers the
//! 80/20 of commands an agent emits.

use crate::ast::*;
use crate::lexer::{self, Op, TokKind, Token};

/// A valid placeholder position. The printer ignores byte offsets entirely and
/// only checks `Pos::is_valid()` in two spots (elif-vs-else, `for … in`), so we
/// use this where "present" must be signalled and [`P0`] where "absent" is.
const PV: Pos = Pos::new(0, 1, 1);
const P0: Pos = Pos::new(0, 0, 0);

/// A parse failure. The driver treats any error as "bail and show original".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: &'static str,
    pub offset: usize,
}

type R<T> = Result<T, ParseError>;

/// Parse a token stream (from [`lexer::tokenize`] over `src`) into a [`File`].
pub fn parse(src: &str, tokens: &[Token]) -> R<File> {
    Parser { src, toks: tokens, i: 0 }.parse_file()
}

/// What ends a statement list in a given context.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Word(&'static str),
    RParen,
}

struct Parser<'a> {
    src: &'a str,
    toks: &'a [Token],
    i: usize,
}

impl<'a> Parser<'a> {
    fn cur(&self) -> &Token {
        // The token stream always ends with Eof, so this never overruns once
        // construction guarantees a non-empty slice. Guard anyway.
        &self.toks[self.i.min(self.toks.len() - 1)]
    }

    fn kind(&self) -> TokKind {
        self.cur().kind
    }

    fn text(&self) -> &'a str {
        let t = self.cur();
        &self.src[t.start..t.end]
    }

    fn at_eof(&self) -> bool {
        self.kind() == TokKind::Eof
    }

    fn is_word(&self, w: &str) -> bool {
        self.kind() == TokKind::Word && self.text() == w
    }

    fn is_op(&self, op: Op) -> bool {
        self.kind() == TokKind::Op(op)
    }

    fn bump(&mut self) {
        if self.i < self.toks.len() - 1 {
            self.i += 1;
        }
    }

    fn err<T>(&self, message: &'static str) -> R<T> {
        Err(ParseError { message, offset: self.cur().start })
    }

    fn at_stop(&self, stops: &[Stop]) -> bool {
        stops.iter().any(|s| match s {
            Stop::Word(w) => self.is_word(w),
            Stop::RParen => self.is_op(Op::RParen),
        })
    }

    fn expect_word(&mut self, w: &'static str) -> R<()> {
        if self.is_word(w) {
            self.bump();
            Ok(())
        } else {
            self.err("expected keyword")
        }
    }

    // -- top level --------------------------------------------------------

    fn parse_file(&mut self) -> R<File> {
        let stmts = self.parse_list(&[])?;
        if !self.at_eof() {
            return self.err("unexpected token at top level");
        }
        Ok(File { name: None, stmts, last: vec![] })
    }

    /// Consume blank separators: newlines and bare `;`.
    fn skip_separators(&mut self) {
        loop {
            match self.kind() {
                TokKind::Newline => self.bump(),
                TokKind::Op(Op::Semi) => self.bump(),
                _ => return,
            }
        }
    }

    /// Parse a list of statements until a stop keyword/token or EOF.
    fn parse_list(&mut self, stops: &[Stop]) -> R<Vec<Stmt>> {
        let mut out = Vec::new();
        loop {
            self.skip_separators();
            if self.at_eof() || self.at_stop(stops) {
                break;
            }
            let mut stmt = self.parse_and_or()?;
            // Trailing terminator.
            match self.kind() {
                TokKind::Op(Op::Amp) => {
                    stmt.background = true;
                    self.bump();
                }
                TokKind::Op(Op::Semi) | TokKind::Newline => self.bump(),
                _ => {}
            }
            out.push(stmt);
        }
        Ok(out)
    }

    // -- and-or / pipeline ------------------------------------------------

    fn parse_and_or(&mut self) -> R<Stmt> {
        let mut left = self.parse_pipeline()?;
        loop {
            let op = if self.is_op(Op::AndIf) {
                BinCmdOperator::AndStmt
            } else if self.is_op(Op::OrIf) {
                BinCmdOperator::OrStmt
            } else {
                break;
            };
            self.bump();
            self.skip_newlines();
            let right = self.parse_pipeline()?;
            left = self.binary(op, left, right);
        }
        Ok(left)
    }

    fn parse_pipeline(&mut self) -> R<Stmt> {
        let mut left = self.parse_command_stmt()?;
        loop {
            let op = if self.is_op(Op::Pipe) {
                BinCmdOperator::Pipe
            } else if self.is_op(Op::PipeAmp) {
                BinCmdOperator::PipeAll
            } else {
                break;
            };
            self.bump();
            self.skip_newlines();
            let right = self.parse_command_stmt()?;
            left = self.binary(op, left, right);
        }
        Ok(left)
    }

    fn skip_newlines(&mut self) {
        while self.kind() == TokKind::Newline {
            self.bump();
        }
    }

    fn binary(&self, op: BinCmdOperator, x: Stmt, y: Stmt) -> Stmt {
        self.stmt(Command::Binary(Box::new(BinaryCmd { op_pos: P0, op, x, y })))
    }

    fn stmt(&self, cmd: Command) -> Stmt {
        Stmt {
            comments: vec![],
            cmd: Some(cmd),
            position: P0,
            semicolon: P0,
            negated: false,
            background: false,
            coprocess: false,
            redirs: vec![],
        }
    }

    // -- a single command (+ leading `!`, + redirects) --------------------

    fn parse_command_stmt(&mut self) -> R<Stmt> {
        let negated = if self.is_word("!") {
            self.bump();
            true
        } else {
            false
        };
        let (cmd, redirs) = self.parse_command()?;
        let mut s = self.stmt(cmd);
        s.negated = negated;
        s.redirs = redirs;
        Ok(s)
    }

    fn parse_command(&mut self) -> R<(Command, Vec<Redirect>)> {
        // Compound forms starting with '('.
        if self.is_op(Op::LParen) {
            if self.next_is_adjacent_lparen() {
                let cmd = self.parse_arith_opaque()?;
                let redirs = self.parse_trailing_redirs()?;
                return Ok((cmd, redirs));
            }
            let cmd = Command::Subshell(self.parse_subshell()?);
            let redirs = self.parse_trailing_redirs()?;
            return Ok((cmd, redirs));
        }
        // Compound forms / bail keywords starting with a reserved word.
        if self.kind() == TokKind::Word {
            match self.text() {
                "{" => {
                    let b = self.parse_block()?;
                    let redirs = self.parse_trailing_redirs()?;
                    return Ok((Command::Block(b), redirs));
                }
                "if" => {
                    let c = self.parse_if()?;
                    let redirs = self.parse_trailing_redirs()?;
                    return Ok((Command::If(Box::new(c)), redirs));
                }
                "while" => {
                    let c = self.parse_while(false)?;
                    let redirs = self.parse_trailing_redirs()?;
                    return Ok((Command::While(Box::new(c)), redirs));
                }
                "until" => {
                    let c = self.parse_while(true)?;
                    let redirs = self.parse_trailing_redirs()?;
                    return Ok((Command::While(Box::new(c)), redirs));
                }
                "for" => {
                    let c = self.parse_for()?;
                    let redirs = self.parse_trailing_redirs()?;
                    return Ok((Command::For(Box::new(c)), redirs));
                }
                "[[" => {
                    let cmd = self.parse_test_opaque()?;
                    let redirs = self.parse_trailing_redirs()?;
                    return Ok((cmd, redirs));
                }
                "case" | "select" | "function" | "coproc" | "time" => {
                    return self.err("unsupported construct (bail)");
                }
                _ => {}
            }
        }
        // Otherwise: a simple command.
        self.parse_simple_command()
    }

    fn next_is_adjacent_lparen(&self) -> bool {
        // current is '('; is the next token also '(' with no gap?
        let cur = self.cur();
        if self.i + 1 >= self.toks.len() {
            return false;
        }
        let nxt = &self.toks[self.i + 1];
        nxt.kind == TokKind::Op(Op::LParen) && nxt.start == cur.end
    }

    // -- simple command ---------------------------------------------------

    fn parse_simple_command(&mut self) -> R<(Command, Vec<Redirect>)> {
        let mut args: Vec<Word> = Vec::new();
        let mut redirs: Vec<Redirect> = Vec::new();
        // (end_offset, text) of the most recently pushed arg, for fd detection.
        let mut last_arg: Option<(usize, String)> = None;

        loop {
            match self.kind() {
                TokKind::Word => {
                    // Function declaration `name ()` → bail.
                    if args.is_empty()
                        && redirs.is_empty()
                        && self.next_two_are_paren_pair()
                    {
                        return self.err("function declaration (bail)");
                    }
                    let t = self.cur();
                    let text = self.src[t.start..t.end].to_string();
                    last_arg = Some((t.end, text.clone()));
                    args.push(raw_word(&text));
                    self.bump();
                }
                TokKind::Op(op) if is_redirect(op) => {
                    if op == Op::DLess || op == Op::DLessDash {
                        return self.err("here-document (bail)");
                    }
                    // fd attaches if the previous arg is adjacent and a valid fd.
                    let op_start = self.cur().start;
                    let mut n = None;
                    if let Some((end, text)) = &last_arg
                        && *end == op_start
                        && is_fd(text)
                    {
                        n = Some(lit(text));
                        args.pop();
                    }
                    let mut r = self.parse_one_redirect()?;
                    r.n = n;
                    redirs.push(r);
                    last_arg = None;
                }
                _ => break,
            }
        }

        if args.is_empty() && redirs.is_empty() {
            return self.err("expected a command");
        }
        Ok((Command::Call(CallExpr { assigns: vec![], args }), redirs))
    }

    fn next_two_are_paren_pair(&self) -> bool {
        // current is a word; are the next two tokens `(` `)`?
        let a = self.toks.get(self.i + 1);
        let b = self.toks.get(self.i + 2);
        matches!(a.map(|t| t.kind), Some(TokKind::Op(Op::LParen)))
            && matches!(b.map(|t| t.kind), Some(TokKind::Op(Op::RParen)))
    }

    /// Parse one `op word` redirect (fd handled by the caller). Heredoc ops are
    /// rejected before reaching here.
    fn parse_one_redirect(&mut self) -> R<Redirect> {
        let op = match self.kind() {
            TokKind::Op(o) if is_redirect(o) => o,
            _ => return self.err("expected redirection"),
        };
        self.bump();
        if self.kind() != TokKind::Word {
            return self.err("redirection without target");
        }
        let word = raw_word(self.text());
        self.bump();
        Ok(Redirect {
            op_pos: P0,
            op: map_redir(op),
            n: None,
            word: Some(word),
            hdoc: None,
        })
    }

    fn parse_trailing_redirs(&mut self) -> R<Vec<Redirect>> {
        let mut redirs = Vec::new();
        while let TokKind::Op(o) = self.kind() {
            if !is_redirect(o) {
                break;
            }
            if o == Op::DLess || o == Op::DLessDash {
                return self.err("here-document (bail)");
            }
            redirs.push(self.parse_one_redirect()?);
        }
        Ok(redirs)
    }

    // -- compound commands ------------------------------------------------

    fn parse_subshell(&mut self) -> R<Subshell> {
        // current is '('
        self.bump();
        let stmts = self.parse_list(&[Stop::RParen])?;
        if !self.is_op(Op::RParen) {
            return self.err("unterminated subshell");
        }
        self.bump();
        Ok(Subshell { lparen: P0, rparen: P0, stmts, last: vec![] })
    }

    fn parse_block(&mut self) -> R<Block> {
        self.expect_word("{")?;
        let stmts = self.parse_list(&[Stop::Word("}")])?;
        self.expect_word("}")?;
        Ok(Block { lbrace: P0, rbrace: P0, stmts, last: vec![] })
    }

    fn parse_if(&mut self) -> R<IfClause> {
        self.expect_word("if")?;
        let cond = self.parse_list(&[Stop::Word("then")])?;
        self.expect_word("then")?;
        let then = self.parse_list(&[Stop::Word("elif"), Stop::Word("else"), Stop::Word("fi")])?;
        let else_ = self.parse_if_tail()?;
        self.expect_word("fi")?;
        Ok(IfClause {
            position: PV,
            then_pos: PV,
            fi_pos: PV,
            cond,
            cond_last: vec![],
            then,
            then_last: vec![],
            else_,
            last: vec![],
        })
    }

    fn parse_if_tail(&mut self) -> R<Option<Box<IfClause>>> {
        if self.is_word("elif") {
            self.bump();
            let cond = self.parse_list(&[Stop::Word("then")])?;
            self.expect_word("then")?;
            let then =
                self.parse_list(&[Stop::Word("elif"), Stop::Word("else"), Stop::Word("fi")])?;
            let else_ = self.parse_if_tail()?;
            Ok(Some(Box::new(IfClause {
                position: PV,
                then_pos: PV, // valid ⇒ printed as `elif`
                fi_pos: PV,
                cond,
                cond_last: vec![],
                then,
                then_last: vec![],
                else_,
                last: vec![],
            })))
        } else if self.is_word("else") {
            self.bump();
            let then = self.parse_list(&[Stop::Word("fi")])?;
            Ok(Some(Box::new(IfClause {
                position: PV,
                then_pos: P0, // invalid ⇒ printed as `else`
                fi_pos: PV,
                cond: vec![],
                cond_last: vec![],
                then,
                then_last: vec![],
                else_: None,
                last: vec![],
            })))
        } else {
            Ok(None)
        }
    }

    fn parse_while(&mut self, until: bool) -> R<WhileClause> {
        self.bump(); // while/until
        let cond = self.parse_list(&[Stop::Word("do")])?;
        self.expect_word("do")?;
        let do_ = self.parse_list(&[Stop::Word("done")])?;
        self.expect_word("done")?;
        Ok(WhileClause {
            while_pos: P0,
            do_pos: P0,
            done_pos: P0,
            until,
            cond,
            cond_last: vec![],
            do_,
            do_last: vec![],
        })
    }

    fn parse_for(&mut self) -> R<ForClause> {
        self.expect_word("for")?;
        // C-style `for (( … ))` is not supported → bail.
        if self.is_op(Op::LParen) {
            return self.err("C-style for loop (bail)");
        }
        if self.kind() != TokKind::Word {
            return self.err("expected loop variable");
        }
        let name = lit(self.text());
        self.bump();

        let mut items = Vec::new();
        let mut in_pos = P0;
        if self.is_word("in") {
            in_pos = PV;
            self.bump();
            while self.kind() == TokKind::Word && !self.is_word("do") {
                items.push(raw_word(self.text()));
                self.bump();
            }
        }
        self.skip_separators();
        self.expect_word("do")?;
        let do_ = self.parse_list(&[Stop::Word("done")])?;
        self.expect_word("done")?;
        Ok(ForClause {
            for_pos: P0,
            do_pos: P0,
            done_pos: P0,
            select: false,
            braces: false,
            loop_: Loop::Word(WordIter { name, in_pos, items }),
            do_,
            do_last: vec![],
        })
    }

    // -- opaque [[ … ]] and (( … )) --------------------------------------

    fn parse_test_opaque(&mut self) -> R<Command> {
        let start = self.cur().start; // the "[[" word
        self.bump();
        loop {
            if self.at_eof() {
                return self.err("unterminated [[ ]]");
            }
            if self.kind() == TokKind::Word && self.text() == "]]" {
                let end = self.cur().end;
                self.bump();
                let raw = &self.src[start..end];
                return Ok(Command::Call(CallExpr { assigns: vec![], args: vec![raw_word(raw)] }));
            }
            self.bump();
        }
    }

    fn parse_arith_opaque(&mut self) -> R<Command> {
        let start = self.cur().start; // first '('
        let end = lexer::scan_parens(self.src.as_bytes(), start)
            .map_err(|e| ParseError { message: "unbalanced (( ))", offset: e.offset })?;
        // Advance the cursor past every token inside the sliced range.
        while !self.at_eof() && self.cur().start < end {
            self.bump();
        }
        let raw = &self.src[start..end];
        Ok(Command::Call(CallExpr { assigns: vec![], args: vec![raw_word(raw)] }))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn raw_word(s: &str) -> Word {
    Word { parts: vec![WordPart::Lit(lit(s))] }
}

fn lit(s: &str) -> Lit {
    Lit { value_pos: P0, value_end: P0, value: s.to_string() }
}

fn is_fd(text: &str) -> bool {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        || (text.starts_with('{') && text.ends_with('}') && text.len() > 2)
}

fn is_redirect(op: Op) -> bool {
    matches!(
        op,
        Op::Less
            | Op::Great
            | Op::DGreat
            | Op::LessAnd
            | Op::GreatAnd
            | Op::LessGreat
            | Op::Clobber
            | Op::AndGreat
            | Op::AndDGreat
            | Op::DLess
            | Op::DLessDash
            | Op::TLess
    )
}

fn map_redir(op: Op) -> RedirOperator {
    match op {
        Op::Less => RedirOperator::RdrIn,
        Op::Great => RedirOperator::RdrOut,
        Op::DGreat => RedirOperator::AppOut,
        Op::LessAnd => RedirOperator::DplIn,
        Op::GreatAnd => RedirOperator::DplOut,
        Op::LessGreat => RedirOperator::RdrInOut,
        Op::Clobber => RedirOperator::ClbOut,
        Op::AndGreat => RedirOperator::RdrAll,
        Op::AndDGreat => RedirOperator::AppAll,
        Op::TLess => RedirOperator::WordHdoc,
        // DLess/DLessDash never reach here (we bail earlier).
        _ => RedirOperator::RdrOut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::printer;

    /// Full pipeline: tokenize → parse → print, at width 80.
    fn fmt(src: &str) -> String {
        let toks = lexer::tokenize(src).expect("lex");
        let file = parse(src, &toks).expect("parse");
        printer::format(&file, 80)
    }

    fn parses(src: &str) -> bool {
        match lexer::tokenize(src) {
            Ok(toks) => parse(src, &toks).is_ok(),
            Err(_) => false,
        }
    }

    #[test]
    fn simple_command_with_redirects() {
        assert_eq!(fmt("ls  -la   /tmp   >/dev/null   2>&1"), "ls -la /tmp >/dev/null 2>&1\n");
    }

    #[test]
    fn assignment_prefix_is_a_word() {
        assert_eq!(fmt("FOO=bar  ./run.sh"), "FOO=bar ./run.sh\n");
    }

    #[test]
    fn and_or_pipe_precedence() {
        // a | b && c  ⇒  (a | b) && c
        assert_eq!(fmt("a|b&&c"), "a | b && c\n");
    }

    #[test]
    fn semicolons_split_into_lines() {
        assert_eq!(fmt("x=1;   y=2 ;  echo done"), "x=1\ny=2\necho done\n");
    }

    #[test]
    fn for_loop_inline() {
        assert_eq!(
            fmt("for i in 1 2 3; do echo \"iteration $i\" ; done"),
            "for i in 1 2 3; do echo \"iteration $i\"; done\n"
        );
    }

    #[test]
    fn if_inline() {
        assert_eq!(
            fmt("if [ \"$x\" -lt \"$y\" ]; then echo   \"x is smaller\" ; fi"),
            "if [ \"$x\" -lt \"$y\" ]; then echo \"x is smaller\"; fi\n"
        );
    }

    #[test]
    fn opaque_test_clause_preserved_verbatim() {
        assert_eq!(fmt("[[ -f x && $y == *.json ]] && cat x"), "[[ -f x && $y == *.json ]] && cat x\n");
    }

    #[test]
    fn opaque_arith_command_preserved_verbatim() {
        assert_eq!(fmt("(( count++ ))"), "(( count++ ))\n");
    }

    #[test]
    fn subshell_and_block() {
        assert_eq!(fmt("(cd /tmp && ls)"), "(cd /tmp && ls)\n");
        assert_eq!(fmt("{ a; b; }"), "{\n  a\n  b\n}\n");
    }

    #[test]
    fn cmdsubst_word_preserved() {
        assert_eq!(fmt("echo $(git rev-parse HEAD)"), "echo $(git rev-parse HEAD)\n");
    }

    #[test]
    fn the_progress_seed_case() {
        let input = "x=1;   y=2 ;  for i in 1 2 3; do echo \"iteration $i\" ; done ;   if [ \"$x\" -lt \"$y\" ]; then echo   \"x is smaller\" ; fi ;  ls -la /tmp  >/dev/null   2>&1 ;   echo    \"all done\"";
        let expected = "x=1\ny=2\nfor i in 1 2 3; do echo \"iteration $i\"; done\nif [ \"$x\" -lt \"$y\" ]; then echo \"x is smaller\"; fi\nls -la /tmp >/dev/null 2>&1\necho \"all done\"\n";
        assert_eq!(fmt(input), expected);
    }

    #[test]
    fn long_chain_wraps_at_width() {
        let toks = lexer::tokenize("aaaaa && bbbbb && ccccc && ddddd").unwrap();
        let file = parse("aaaaa && bbbbb && ccccc && ddddd", &toks).unwrap();
        assert_eq!(
            printer::format(&file, 12),
            "aaaaa &&\n  bbbbb &&\n  ccccc &&\n  ddddd\n"
        );
    }

    #[test]
    fn bail_on_function_declaration() {
        assert!(!parses("foo() { echo hi; }"));
    }

    #[test]
    fn bail_on_case() {
        assert!(!parses("case $x in a) echo a;; esac"));
    }

    #[test]
    fn bail_on_heredoc() {
        assert!(!parses("cat <<EOF\nhi\nEOF"));
    }
}
