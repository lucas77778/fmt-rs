//! Renders the [`ast`](crate::ast) tree into a [`Doc`](crate::doc) layout tree.
//!
//! This is the bridge between "what the command is" (the AST) and "how it
//! should be laid out" (the Doc engine). Unlike shfmt's printer it never reads
//! source positions: layout is decided entirely by target width, so a group
//! stays on one line when it fits and reflows when it does not.
//!
//! Formatting choices (M0/M1 baseline + M2 wrapping):
//! - Top-level statements each get their own line.
//! - `&&` / `||` / `|` chains lay out on one line if they fit, otherwise break
//!   *after* each operator with a 2-space continuation indent.
//! - Compound bodies (`do…done`, `then…fi`, `{ … }`) stay inline when they
//!   hold a single statement that fits, and expand otherwise.

use crate::ast::*;
use crate::doc::{self, Doc};

const INDENT: isize = 2;

/// Default target width for formatting (permission-dialog friendly).
pub const DEFAULT_WIDTH: isize = 80;

/// Format a whole file to a string, with a trailing newline.
pub fn format(file: &File, width: isize) -> String {
    let mut s = doc::pretty(width, &file_doc(file));
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

fn txt(s: impl Into<String>) -> Doc {
    Doc::text(s.into())
}

fn file_doc(f: &File) -> Doc {
    Doc::join(Doc::hard_line(), f.stmts.iter().map(stmt_doc))
}

// ---------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------

fn stmt_doc(s: &Stmt) -> Doc {
    let mut parts = Vec::new();
    if s.negated {
        parts.push(txt("! "));
    }
    if let Some(cmd) = &s.cmd {
        parts.push(command_doc(cmd));
    }
    for r in &s.redirs {
        parts.push(txt(" "));
        parts.push(redir_doc(r));
    }
    if s.background {
        parts.push(txt(" &"));
    }
    if s.coprocess {
        parts.push(txt(" |&"));
    }
    Doc::concat(parts)
}

/// Statements rendered on a single conceptual line, separated by `; `.
fn inline_stmts(stmts: &[Stmt]) -> Doc {
    Doc::join(txt("; "), stmts.iter().map(stmt_doc))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn command_doc(cmd: &Command) -> Doc {
    match cmd {
        Command::Call(c) => call_doc(c),
        Command::Binary(b) => binary_doc(b),
        Command::Block(b) => keyword_block("{", "}", &b.stmts),
        Command::Subshell(s) => Doc::concat([txt("("), inline_stmts(&s.stmts), txt(")")]),
        Command::If(i) => if_doc(i),
        Command::For(f) => for_doc(f),
        Command::While(w) => while_doc(w),
        Command::Case(c) => case_doc(c),
        Command::Func(f) => func_doc(f),
        Command::Test(t) => Doc::concat([txt("[[ "), test_expr_doc(&t.x), txt(" ]]")]),
        Command::Arithm(a) => Doc::concat([txt("(("), arithm_doc(&a.x, false), txt("))")]),
        Command::Decl(d) => decl_doc(d),
        Command::Let(l) => let_doc(l),
        Command::Time(t) => time_doc(t),
        Command::Coproc(c) => coproc_doc(c),
    }
}

fn call_doc(c: &CallExpr) -> Doc {
    let mut items: Vec<Doc> = Vec::new();
    for a in &c.assigns {
        items.push(assign_doc(a));
    }
    for w in &c.args {
        items.push(word_doc(w));
    }
    Doc::join(txt(" "), items)
}

/// Flattens a same-shape binary chain into `(operator-before, stmt)` segments.
/// The first segment's operator is `None`.
fn flatten_chain<'a>(s: &'a Stmt, out: &mut Vec<(Option<BinCmdOperator>, &'a Stmt)>) {
    if let Some(Command::Binary(b)) = &s.cmd {
        flatten_chain(&b.x, out);
        out.push((Some(b.op), &b.y));
    } else {
        out.push((None, s));
    }
}

fn binary_doc(b: &BinaryCmd) -> Doc {
    // Re-wrap into a Stmt-shaped view so the flattener can recurse uniformly.
    let mut segs: Vec<(Option<BinCmdOperator>, &Stmt)> = Vec::new();
    flatten_chain(&b.x, &mut segs);
    segs.push((Some(b.op), &b.y));

    let mut first = Doc::Nil;
    let mut rest: Vec<Doc> = Vec::new();
    for (i, (op, st)) in segs.iter().enumerate() {
        match op {
            None => first = stmt_doc(st),
            Some(op) => {
                let _ = i;
                // operator-at-end style: ` &&` stays on the previous line, the
                // line break (when broken) precedes the next operand.
                rest.push(Doc::concat([
                    txt(format!(" {}", op.as_str())),
                    Doc::line(),
                    stmt_doc(st),
                ]));
            }
        }
    }
    Doc::group(Doc::concat([first, Doc::nest(INDENT, Doc::concat(rest))]))
}

/// Shared renderer for `do…done`, `then…fi`, and `{ … }` style bodies.
///
/// Inline: `<open> a; b; <close>`. Broken (multi-statement or too wide):
/// ```text
/// <open>
///   a
///   b
/// <close>
/// ```
fn keyword_block(open: &str, close: &str, stmts: &[Stmt]) -> Doc {
    if stmts.is_empty() {
        return Doc::concat([txt(open.to_string()), txt(" "), txt(close.to_string())]);
    }
    let force = stmts.len() > 1;
    let nl = if force { Doc::hard_line() } else { Doc::line() };
    // Separator between statements: `;` only when inline, then the break.
    let sep = Doc::concat([Doc::if_break(Doc::Nil, txt(";")), nl.clone()]);
    let body = Doc::join(sep, stmts.iter().map(stmt_doc));
    Doc::group(Doc::concat([
        txt(open.to_string()),
        Doc::nest(INDENT, Doc::concat([nl.clone(), body])),
        Doc::if_break(Doc::Nil, txt(";")), // trailing `;` before close, inline only
        nl,                                // dedented break before close
        txt(close.to_string()),
    ]))
}

fn for_doc(f: &ForClause) -> Doc {
    let kw = if f.select { "select " } else { "for " };
    Doc::concat([
        txt(kw),
        loop_doc(&f.loop_),
        txt("; "),
        keyword_block("do", "done", &f.do_),
    ])
}

fn while_doc(w: &WhileClause) -> Doc {
    let kw = if w.until { "until " } else { "while " };
    Doc::concat([
        txt(kw),
        inline_stmts(&w.cond),
        txt("; "),
        keyword_block("do", "done", &w.do_),
    ])
}

fn loop_doc(l: &Loop) -> Doc {
    match l {
        Loop::Word(wi) => {
            let mut parts = vec![txt(wi.name.value.clone())];
            if wi.in_pos.is_valid() {
                parts.push(txt(" in "));
                parts.push(Doc::join(txt(" "), wi.items.iter().map(word_doc)));
            }
            Doc::concat(parts)
        }
        Loop::CStyle(c) => {
            let part = |e: &Option<ArithmExpr>| match e {
                Some(x) => arithm_doc(x, false),
                None => Doc::Nil,
            };
            Doc::concat([
                txt("(("),
                part(&c.init),
                txt("; "),
                part(&c.cond),
                txt("; "),
                part(&c.post),
                txt("))"),
            ])
        }
    }
}

fn if_doc(ic: &IfClause) -> Doc {
    let force = needs_break(ic);
    let nl = if force { Doc::hard_line() } else { Doc::line() };

    // `<kw> body` with nested break and inline trailing `;`.
    let then_part = |body: &[Stmt]| {
        let sep = Doc::concat([Doc::if_break(Doc::Nil, txt(";")), nl.clone()]);
        Doc::concat([
            txt("then"),
            Doc::nest(INDENT, Doc::concat([nl.clone(), Doc::join(sep, body.iter().map(stmt_doc))])),
            Doc::if_break(Doc::Nil, txt(";")),
        ])
    };

    let mut items = vec![txt("if "), inline_stmts(&ic.cond), txt("; "), then_part(&ic.then), nl.clone()];

    let mut cur = ic.else_.as_deref();
    while let Some(c) = cur {
        if c.then_pos.is_valid() {
            // elif
            items.push(txt("elif "));
            items.push(inline_stmts(&c.cond));
            items.push(txt("; "));
            items.push(then_part(&c.then));
            items.push(nl.clone());
        } else {
            // else
            let sep = Doc::concat([Doc::if_break(Doc::Nil, txt(";")), nl.clone()]);
            items.push(txt("else"));
            items.push(Doc::nest(
                INDENT,
                Doc::concat([nl.clone(), Doc::join(sep, c.then.iter().map(stmt_doc))]),
            ));
            items.push(Doc::if_break(Doc::Nil, txt(";")));
            items.push(nl.clone());
        }
        cur = c.else_.as_deref();
    }
    items.push(txt("fi"));
    Doc::group(Doc::concat(items))
}

/// An `if` expands if it has any else/elif branch or a multi-statement body.
fn needs_break(ic: &IfClause) -> bool {
    if ic.then.len() > 1 || ic.else_.is_some() {
        return true;
    }
    false
}

fn case_doc(c: &CaseClause) -> Doc {
    let mut items = vec![txt("case "), word_doc(&c.word), txt(" in")];
    for ci in &c.items {
        let pats = Doc::join(txt(" | "), ci.patterns.iter().map(word_doc));
        let body = if ci.stmts.is_empty() {
            Doc::Nil
        } else {
            Doc::nest(
                INDENT,
                Doc::concat([Doc::hard_line(), inline_stmts(&ci.stmts)]),
            )
        };
        items.push(Doc::nest(
            INDENT,
            Doc::concat([
                Doc::hard_line(),
                pats,
                txt(")"),
                body,
                txt(format!(" {}", ci.op.as_str())),
            ]),
        ));
    }
    items.push(Doc::hard_line());
    items.push(txt("esac"));
    Doc::concat(items)
}

fn func_doc(f: &FuncDecl) -> Doc {
    let prefix = if f.rsrv_word { "function " } else { "" };
    Doc::concat([
        txt(format!("{}{}() ", prefix, f.name.value)),
        stmt_doc(&f.body),
    ])
}

fn decl_doc(d: &DeclClause) -> Doc {
    let mut items = vec![txt(d.variant.value.clone())];
    for a in &d.args {
        items.push(txt(" "));
        items.push(assign_doc(a));
    }
    Doc::concat(items)
}

fn let_doc(l: &LetClause) -> Doc {
    let mut items = vec![txt("let")];
    for e in &l.exprs {
        items.push(txt(" "));
        items.push(arithm_doc(e, true));
    }
    Doc::concat(items)
}

fn time_doc(t: &TimeClause) -> Doc {
    let mut items = vec![txt("time")];
    if t.posix_format {
        items.push(txt(" -p"));
    }
    if let Some(s) = &t.stmt {
        items.push(txt(" "));
        items.push(stmt_doc(s));
    }
    Doc::concat(items)
}

fn coproc_doc(c: &CoprocClause) -> Doc {
    let mut items = vec![txt("coproc")];
    if let Some(n) = &c.name {
        items.push(txt(" "));
        items.push(word_doc(n));
    }
    items.push(txt(" "));
    items.push(stmt_doc(&c.stmt));
    Doc::concat(items)
}

// ---------------------------------------------------------------------------
// Assignments and redirections
// ---------------------------------------------------------------------------

fn assign_doc(a: &Assign) -> Doc {
    let mut items = Vec::new();
    if let Some(name) = &a.name {
        items.push(txt(name.value.clone()));
        if let Some(idx) = &a.index {
            items.push(index_doc(idx));
        }
        if a.append {
            items.push(txt("+"));
        }
        if !a.naked {
            items.push(txt("="));
        }
    }
    if let Some(v) = &a.value {
        items.push(word_doc(v));
    } else if let Some(arr) = &a.array {
        items.push(txt("("));
        items.push(Doc::join(txt(" "), arr.elems.iter().map(array_elem_doc)));
        items.push(txt(")"));
    }
    Doc::concat(items)
}

fn array_elem_doc(e: &ArrayElem) -> Doc {
    let mut items = Vec::new();
    if let Some(idx) = &e.index {
        items.push(index_doc(idx));
        items.push(txt("="));
    }
    if let Some(v) = &e.value {
        items.push(word_doc(v));
    }
    Doc::concat(items)
}

fn redir_doc(r: &Redirect) -> Doc {
    let mut items = Vec::new();
    if let Some(n) = &r.n {
        items.push(txt(n.value.clone()));
    }
    items.push(txt(r.op.as_str()));
    if let Some(w) = &r.word {
        items.push(word_doc(w));
    }
    Doc::concat(items)
}

// ---------------------------------------------------------------------------
// Words and word parts
// ---------------------------------------------------------------------------

fn word_doc(w: &Word) -> Doc {
    Doc::concat(w.parts.iter().map(word_part_doc).collect::<Vec<_>>())
}

fn word_part_doc(wp: &WordPart) -> Doc {
    match wp {
        WordPart::Lit(l) => txt(l.value.clone()),
        WordPart::SglQuoted(q) => {
            let dollar = if q.dollar { "$" } else { "" };
            txt(format!("{}'{}'", dollar, q.value))
        }
        WordPart::DblQuoted(q) => {
            let mut items = Vec::new();
            if q.dollar {
                items.push(txt("$"));
            }
            items.push(txt("\""));
            for p in &q.parts {
                items.push(word_part_doc(p));
            }
            items.push(txt("\""));
            Doc::concat(items)
        }
        WordPart::ParamExp(pe) => param_exp_doc(pe),
        WordPart::CmdSubst(cs) => {
            if cs.backquotes {
                Doc::concat([txt("`"), inline_stmts(&cs.stmts), txt("`")])
            } else {
                Doc::concat([txt("$("), inline_stmts(&cs.stmts), txt(")")])
            }
        }
        WordPart::ArithmExp(a) => Doc::concat([txt("$(("), arithm_doc(&a.x, false), txt("))")]),
        WordPart::ProcSubst(ps) => {
            Doc::concat([txt(ps.op.as_str()), inline_stmts(&ps.stmts), txt(")")])
        }
        WordPart::ExtGlob(g) => txt(format!("{}{})", g.op.as_str(), g.pattern.value)),
        WordPart::BraceExp(b) => {
            let sep = if b.sequence { ".." } else { "," };
            Doc::concat([
                txt("{"),
                Doc::join(txt(sep), b.elems.iter().map(word_doc)),
                txt("}"),
            ])
        }
    }
}

fn index_doc(idx: &ArithmExpr) -> Doc {
    Doc::concat([txt("["), arithm_doc(idx, false), txt("]")])
}

fn param_exp_doc(pe: &ParamExp) -> Doc {
    // Naked index: arr[x]
    if pe.short && let Some(idx) = &pe.index {
        return Doc::concat([txt(pe.param.value.clone()), index_doc(idx)]);
    }
    if pe.short {
        return txt(format!("${}", pe.param.value));
    }
    let mut items = vec![txt("${")];
    if pe.length {
        items.push(txt("#"));
    } else if pe.width {
        items.push(txt("%"));
    } else if pe.excl {
        items.push(txt("!"));
    }
    items.push(txt(pe.param.value.clone()));
    if let Some(idx) = &pe.index {
        items.push(index_doc(idx));
    }
    if let Some(sl) = &pe.slice {
        items.push(txt(":"));
        if let Some(o) = &sl.offset {
            items.push(arithm_doc(o, true));
        }
        if let Some(l) = &sl.length {
            items.push(txt(":"));
            items.push(arithm_doc(l, true));
        }
    } else if let Some(r) = &pe.repl {
        if r.all {
            items.push(txt("/"));
        }
        items.push(txt("/"));
        if let Some(o) = &r.orig {
            items.push(word_doc(o));
        }
        items.push(txt("/"));
        if let Some(w) = &r.with {
            items.push(word_doc(w));
        }
    } else if let Some(n) = &pe.names {
        items.push(txt(n.as_str()));
    } else if let Some(exp) = &pe.exp {
        items.push(txt(exp.op.as_str()));
        if let Some(w) = &exp.word {
            items.push(word_doc(w));
        }
    }
    items.push(txt("}"));
    Doc::concat(items)
}

// ---------------------------------------------------------------------------
// Arithmetic and test expressions
// ---------------------------------------------------------------------------

fn arithm_doc(e: &ArithmExpr, compact: bool) -> Doc {
    match e {
        ArithmExpr::Word(w) => word_doc(w),
        ArithmExpr::Paren(p) => Doc::concat([txt("("), arithm_doc(&p.x, compact), txt(")")]),
        ArithmExpr::Unary(u) => {
            if u.post {
                Doc::concat([arithm_doc(&u.x, compact), txt(u.op.as_str())])
            } else {
                Doc::concat([txt(u.op.as_str()), arithm_doc(&u.x, compact)])
            }
        }
        ArithmExpr::Binary(b) => {
            if compact {
                Doc::concat([
                    arithm_doc(&b.x, compact),
                    txt(b.op.as_str()),
                    arithm_doc(&b.y, compact),
                ])
            } else if b.op == BinAritOperator::Comma {
                Doc::concat([
                    arithm_doc(&b.x, compact),
                    txt(format!("{} ", b.op.as_str())),
                    arithm_doc(&b.y, compact),
                ])
            } else {
                Doc::concat([
                    arithm_doc(&b.x, compact),
                    txt(format!(" {} ", b.op.as_str())),
                    arithm_doc(&b.y, compact),
                ])
            }
        }
    }
}

fn test_expr_doc(e: &TestExpr) -> Doc {
    match e {
        TestExpr::Word(w) => word_doc(w),
        TestExpr::Paren(p) => Doc::concat([txt("("), test_expr_doc(&p.x), txt(")")]),
        TestExpr::Unary(u) => Doc::concat([txt(format!("{} ", u.op.as_str())), test_expr_doc(&u.x)]),
        TestExpr::Binary(b) => Doc::concat([
            test_expr_doc(&b.x),
            txt(format!(" {} ", b.op.as_str())),
            test_expr_doc(&b.y),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: Pos = Pos::new(0, 1, 1);

    fn lit(s: &str) -> Lit {
        Lit { value_pos: P, value_end: P, value: s.into() }
    }
    fn w(s: &str) -> Word {
        Word { parts: vec![WordPart::Lit(lit(s))] }
    }
    fn dq_var(name: &str) -> Word {
        // "$name"
        let pe = ParamExp {
            dollar: P, rbrace: P, short: true, excl: false, length: false, width: false,
            param: lit(name), index: None, slice: None, repl: None, names: None, exp: None,
        };
        Word {
            parts: vec![WordPart::DblQuoted(DblQuoted {
                left: P, right: P, dollar: false,
                parts: vec![WordPart::ParamExp(Box::new(pe))],
            })],
        }
    }
    fn call(args: &[&str]) -> Command {
        Command::Call(CallExpr { assigns: vec![], args: args.iter().map(|s| w(s)).collect() })
    }
    fn stmt(cmd: Command) -> Stmt {
        Stmt {
            comments: vec![], cmd: Some(cmd), position: P, semicolon: P,
            negated: false, background: false, coprocess: false, redirs: vec![],
        }
    }
    fn file(stmts: Vec<Stmt>) -> File {
        File { name: None, stmts, last: vec![] }
    }

    #[test]
    fn simple_command_with_redirects() {
        let mut s = stmt(call(&["ls", "-la", "/tmp"]));
        s.redirs = vec![
            Redirect { op_pos: P, op: RedirOperator::RdrOut, n: None, word: Some(w("/dev/null")), hdoc: None },
            Redirect { op_pos: P, op: RedirOperator::DplOut, n: Some(lit("2")), word: Some(w("1")), hdoc: None },
        ];
        assert_eq!(format(&file(vec![s]), 80), "ls -la /tmp >/dev/null 2>&1\n");
    }

    #[test]
    fn chain_stays_inline_when_it_fits() {
        let inner = Command::Binary(Box::new(BinaryCmd {
            op_pos: P, op: BinCmdOperator::AndStmt,
            x: stmt(call(&["aaa"])), y: stmt(call(&["bbb"])),
        }));
        let outer = Command::Binary(Box::new(BinaryCmd {
            op_pos: P, op: BinCmdOperator::AndStmt,
            x: stmt(inner), y: stmt(call(&["ccc"])),
        }));
        assert_eq!(format(&file(vec![stmt(outer)]), 80), "aaa && bbb && ccc\n");
    }

    #[test]
    fn chain_wraps_after_operator_when_too_wide() {
        let inner = Command::Binary(Box::new(BinaryCmd {
            op_pos: P, op: BinCmdOperator::AndStmt,
            x: stmt(call(&["aaa"])), y: stmt(call(&["bbb"])),
        }));
        let outer = Command::Binary(Box::new(BinaryCmd {
            op_pos: P, op: BinCmdOperator::Pipe,
            x: stmt(inner), y: stmt(call(&["ccc"])),
        }));
        assert_eq!(format(&file(vec![stmt(outer)]), 8), "aaa &&\n  bbb |\n  ccc\n");
    }

    #[test]
    fn for_loop_inline() {
        let f = ForClause {
            for_pos: P, do_pos: P, done_pos: P, select: false, braces: false,
            loop_: Loop::Word(WordIter {
                name: lit("i"), in_pos: P, items: vec![w("1"), w("2"), w("3")],
            }),
            do_: vec![stmt(call(&["echo", "hi"]))],
            do_last: vec![],
        };
        assert_eq!(
            format(&file(vec![stmt(Command::For(Box::new(f)))]), 80),
            "for i in 1 2 3; do echo hi; done\n"
        );
    }

    #[test]
    fn if_inline_with_test_command() {
        // if [ "$x" -lt "$y" ]; then echo smaller; fi
        let cond = stmt(Command::Call(CallExpr {
            assigns: vec![],
            args: vec![w("["), dq_var("x"), w("-lt"), dq_var("y"), w("]")],
        }));
        let ic = IfClause {
            position: P, then_pos: P, fi_pos: P,
            cond: vec![cond], cond_last: vec![],
            then: vec![stmt(call(&["echo", "smaller"]))], then_last: vec![],
            else_: None, last: vec![],
        };
        assert_eq!(
            format(&file(vec![stmt(Command::If(Box::new(ic)))]), 80),
            "if [ \"$x\" -lt \"$y\" ]; then echo smaller; fi\n"
        );
    }

    #[test]
    fn multi_statement_block_expands() {
        let b = Block {
            lbrace: P, rbrace: P,
            stmts: vec![stmt(call(&["a"])), stmt(call(&["b"]))],
            last: vec![],
        };
        assert_eq!(
            format(&file(vec![stmt(Command::Block(b))]), 80),
            "{\n  a\n  b\n}\n"
        );
    }

    #[test]
    fn top_level_statements_each_on_their_own_line() {
        let f = file(vec![
            stmt(Command::Call(CallExpr {
                assigns: vec![Assign {
                    append: false, naked: false, name: Some(lit("x")),
                    index: None, value: Some(w("1")), array: None,
                }],
                args: vec![],
            })),
            stmt(call(&["echo", "done"])),
        ]);
        assert_eq!(format(&f, 80), "x=1\necho done\n");
    }
}
