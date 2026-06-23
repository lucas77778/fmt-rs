//! Shell syntax tree for fmt-rs.
//!
//! This is a faithful but idiomatic Rust port of the AST used by mvdan/sh
//! (`syntax/nodes.go` + `syntax/tokens.go`), which is the upstream of the
//! `mvdan-sh` engine fmt-rs replaces. Keeping the shapes aligned lets the
//! printer mirror shfmt's proven formatting decisions.
//!
//! Interface types in Go (`Command`, `WordPart`, `ArithmExpr`, `TestExpr`,
//! `Loop`) become Rust enums; concrete node structs are carried inside them.
//! Recursive children are boxed.
//!
//! Positions: every node carries a [`Pos`]. shfmt's printer is *position
//! driven* — it decides whether to break a line by comparing a node's original
//! line number against the line it is currently writing. We keep the same
//! information so M0/M1 can reproduce that behaviour; M2's length-driven
//! wrapping is layered on top in the printer, not here.

/// A position within a shell source file. Mirrors Go's `syntax.Pos`.
///
/// `line`/`col` start at 1; `offset` (byte offset) starts at 0. A position with
/// `line == 0` is invalid (the field was absent in the source), matching Go's
/// `IsValid` convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos {
    pub offset: u32,
    pub line: u32,
    pub col: u32,
}

impl Pos {
    pub const fn new(offset: u32, line: u32, col: u32) -> Self {
        Pos { offset, line, col }
    }

    /// All positions produced by a parser are valid; the zero value is not.
    pub const fn is_valid(self) -> bool {
        self.line > 0
    }

    /// Whether `self` comes after `other` in the source (by byte offset).
    pub const fn after(self, other: Pos) -> bool {
        self.offset > other.offset
    }
}

/// A single comment on a single line (`# text`). `hash` is the position of `#`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub hash: Pos,
    pub text: String,
}

/// A shell source file: a list of statements plus any trailing comments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct File {
    pub name: Option<String>,
    pub stmts: Vec<Stmt>,
    pub last: Vec<Comment>,
}

/// A statement ("complete command"): a command with optional leading `!`,
/// trailing `&`/`|&`/`;`, redirections, and attached comments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stmt {
    pub comments: Vec<Comment>,
    pub cmd: Option<Command>,
    pub position: Pos,
    /// Position of the trailing `;`, `&`, or `|&`, if any.
    pub semicolon: Pos,
    pub negated: bool,    // `! stmt`
    pub background: bool, // `stmt &`
    pub coprocess: bool,  // mksh `stmt |&`
    pub redirs: Vec<Redirect>,
}

/// Every command / compound-command / function-declaration variant.
/// Corresponds to Go's `Command` interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Call(CallExpr),
    Binary(Box<BinaryCmd>),
    If(Box<IfClause>),
    While(Box<WhileClause>),
    For(Box<ForClause>),
    Case(CaseClause),
    Block(Block),
    Subshell(Subshell),
    Func(Box<FuncDecl>),
    Arithm(ArithmCmd),
    Test(Box<TestClause>),
    Decl(DeclClause),
    Let(LetClause),
    Time(Box<TimeClause>),
    Coproc(Box<CoprocClause>),
}

/// An assignment such as `a=x`, `arr[i]=x`, `a+=x`, or a naked decl argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assign {
    pub append: bool, // `+=`
    pub naked: bool,  // no `=` (declare arg / option)
    pub name: Option<Lit>,
    pub index: Option<ArithmExpr>, // `[i]`, `["k"]`
    pub value: Option<Word>,       // `=val`
    pub array: Option<ArrayExpr>,  // `=(arr)`
}

/// An input/output redirection (`>a`, `2>&1`, `<<EOF`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub op_pos: Pos,
    pub op: RedirOperator,
    pub n: Option<Lit>,    // fd number, or `{varname}` in Bash
    pub word: Option<Word>, // target word
    pub hdoc: Option<Word>, // here-document body
}

/// A simple command / function call ("simple command").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallExpr {
    pub assigns: Vec<Assign>,
    pub args: Vec<Word>,
}

/// `( ... )` — commands in a subshell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subshell {
    pub lparen: Pos,
    pub rparen: Pos,
    pub stmts: Vec<Stmt>,
    pub last: Vec<Comment>,
}

/// `{ ...; }` — commands in a new scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub lbrace: Pos,
    pub rbrace: Pos,
    pub stmts: Vec<Stmt>,
    pub last: Vec<Comment>,
}

/// An `if`/`elif`/`else` clause. `else_` chains for `elif`/`else`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfClause {
    pub position: Pos, // `if`, `elif`, or `else`
    pub then_pos: Pos, // `then` (invalid for an `else`)
    pub fi_pos: Pos,   // `fi` (shared with the tail `else_`)
    pub cond: Vec<Stmt>,
    pub cond_last: Vec<Comment>,
    pub then: Vec<Stmt>,
    pub then_last: Vec<Comment>,
    pub else_: Option<Box<IfClause>>,
    pub last: Vec<Comment>,
}

/// A `while` or `until` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhileClause {
    pub while_pos: Pos,
    pub do_pos: Pos,
    pub done_pos: Pos,
    pub until: bool,
    pub cond: Vec<Stmt>,
    pub cond_last: Vec<Comment>,
    pub do_: Vec<Stmt>,
    pub do_last: Vec<Comment>,
}

/// A `for` or `select` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForClause {
    pub for_pos: Pos,
    pub do_pos: Pos,
    pub done_pos: Pos,
    pub select: bool,
    pub braces: bool, // deprecated `{ }` form
    pub loop_: Loop,
    pub do_: Vec<Stmt>,
    pub do_last: Vec<Comment>,
}

/// The iteration head of a `for` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loop {
    Word(WordIter),
    CStyle(CStyleLoop),
}

/// `for name in items` (or, with `in_pos` invalid, over `"$@"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordIter {
    pub name: Lit,
    pub in_pos: Pos,
    pub items: Vec<Word>,
}

/// `for (( init; cond; post ))` — Bash only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CStyleLoop {
    pub lparen: Pos,
    pub rparen: Pos,
    pub init: Option<ArithmExpr>,
    pub cond: Option<ArithmExpr>,
    pub post: Option<ArithmExpr>,
}

/// A `&&` / `||` / `|` / `|&` expression between two statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryCmd {
    pub op_pos: Pos,
    pub op: BinCmdOperator,
    pub x: Stmt,
    pub y: Stmt,
}

/// A function declaration. `rsrv_word` is the `function f()` style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncDecl {
    pub position: Pos,
    pub rsrv_word: bool,
    pub name: Lit,
    pub body: Stmt,
}

/// A shell word: one or more contiguous parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub parts: Vec<WordPart>,
}

impl Word {
    /// Returns the word as a literal string if every part is a [`Lit`],
    /// otherwise an empty string (mirrors Go's `Word.Lit`).
    pub fn lit(&self) -> String {
        let mut s = String::new();
        for part in &self.parts {
            match part {
                WordPart::Lit(l) => s.push_str(&l.value),
                _ => return String::new(),
            }
        }
        s
    }
}

/// Every node that can form part of a word. Corresponds to Go's `WordPart`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WordPart {
    Lit(Lit),
    SglQuoted(SglQuoted),
    DblQuoted(DblQuoted),
    ParamExp(Box<ParamExp>),
    CmdSubst(Box<CmdSubst>),
    ArithmExp(Box<ArithmExp>),
    ProcSubst(Box<ProcSubst>),
    ExtGlob(ExtGlob),
    BraceExp(BraceExp), // only after brace splitting
}

/// A string literal. Note the source may have split it across escaped
/// newlines; that splitting is lost but the end position is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lit {
    pub value_pos: Pos,
    pub value_end: Pos,
    pub value: String,
}

/// A single-quoted string. `dollar` marks the `$'...'` form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SglQuoted {
    pub left: Pos,
    pub right: Pos,
    pub dollar: bool,
    pub value: String,
}

/// A double-quoted string: a list of parts. `dollar` marks `$"..."`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DblQuoted {
    pub left: Pos,
    pub right: Pos,
    pub dollar: bool,
    pub parts: Vec<WordPart>,
}

/// A command substitution `$(...)` (or backquotes / mksh `${ ;}` forms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdSubst {
    pub left: Pos,
    pub right: Pos,
    pub stmts: Vec<Stmt>,
    pub last: Vec<Comment>,
    pub backquotes: bool, // deprecated `` `foo` ``
    pub temp_file: bool,  // mksh `${ foo;}`
    pub reply_var: bool,  // mksh `${|foo;}`
}

/// A parameter expansion `${...}` (or short `$a`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamExp {
    pub dollar: Pos,
    pub rbrace: Pos,
    pub short: bool,  // `$a` instead of `${a}`
    pub excl: bool,   // `${!a}`
    pub length: bool, // `${#a}`
    pub width: bool,  // `${%a}`
    pub param: Lit,
    pub index: Option<ArithmExpr>,        // `${a[i]}`
    pub slice: Option<Slice>,             // `${a:x:y}`
    pub repl: Option<Replace>,            // `${a/x/y}`
    pub names: Option<ParNamesOperator>,  // `${!prefix*}` / `${!prefix@}`
    pub exp: Option<Expansion>,           // `${a:-b}`, `${a#b}`, ...
}

/// `${a:offset:length}` slicing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice {
    pub offset: Option<ArithmExpr>,
    pub length: Option<ArithmExpr>,
}

/// `${a/orig/with}` search-and-replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replace {
    pub all: bool,
    pub orig: Option<Word>,
    pub with: Option<Word>,
}

/// Other `${...}` manipulations such as `${a:-b}` or `${a#b}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub op: ParExpOperator,
    pub word: Option<Word>,
}

/// An arithmetic expansion `$(( ... ))` (or deprecated `$[ ... ]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArithmExp {
    pub left: Pos,
    pub right: Pos,
    pub bracket: bool,  // deprecated `$[expr]`
    pub unsigned: bool, // mksh `$((# expr))`
    pub x: ArithmExpr,
}

/// An arithmetic command `(( ... ))` — Bash only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArithmCmd {
    pub left: Pos,
    pub right: Pos,
    pub unsigned: bool,
    pub x: ArithmExpr,
}

/// Every node forming an arithmetic expression. Corresponds to `ArithmExpr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArithmExpr {
    Binary(Box<BinaryArithm>),
    Unary(Box<UnaryArithm>),
    Paren(Box<ParenArithm>),
    Word(Box<Word>),
}

/// A binary arithmetic expression. Ternaries `a ? b : c` nest as
/// `Binary{TernQuest, a, Binary{TernColon, b, c}}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryArithm {
    pub op_pos: Pos,
    pub op: BinAritOperator,
    pub x: ArithmExpr,
    pub y: ArithmExpr,
}

/// A unary arithmetic expression; `post` distinguishes `x++` from `++x`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnaryArithm {
    pub op_pos: Pos,
    pub op: UnAritOperator,
    pub post: bool,
    pub x: ArithmExpr,
}

/// `( expr )` inside arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParenArithm {
    pub lparen: Pos,
    pub rparen: Pos,
    pub x: ArithmExpr,
}

/// A `case` (switch) clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseClause {
    pub case: Pos,
    pub in_: Pos,
    pub esac: Pos,
    pub braces: bool, // deprecated mksh `{ }` form
    pub word: Word,
    pub items: Vec<CaseItem>,
    pub last: Vec<Comment>,
}

/// One pattern list within a `case`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseItem {
    pub op: CaseOperator,
    pub op_pos: Pos, // invalid if finished by `esac`
    pub comments: Vec<Comment>,
    pub patterns: Vec<Word>,
    pub stmts: Vec<Stmt>,
    pub last: Vec<Comment>,
}

/// A Bash extended test clause `[[ ... ]]` — Bash only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestClause {
    pub left: Pos,
    pub right: Pos,
    pub x: TestExpr,
}

/// Every node forming a test expression. Corresponds to `TestExpr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestExpr {
    Binary(Box<BinaryTest>),
    Unary(Box<UnaryTest>),
    Paren(Box<ParenTest>),
    Word(Box<Word>),
}

/// A binary test expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTest {
    pub op_pos: Pos,
    pub op: BinTestOperator,
    pub x: TestExpr,
    pub y: TestExpr,
}

/// A unary test expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnaryTest {
    pub op_pos: Pos,
    pub op: UnTestOperator,
    pub x: TestExpr,
}

/// `( expr )` inside `[[ ]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParenTest {
    pub lparen: Pos,
    pub rparen: Pos,
    pub x: TestExpr,
}

/// A Bash declare-family clause (`declare`/`local`/`export`/`readonly`/...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclClause {
    pub variant: Lit,
    pub args: Vec<Assign>,
}

/// A Bash array expression `=( ... )`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayExpr {
    pub lparen: Pos,
    pub rparen: Pos,
    pub elems: Vec<ArrayElem>,
    pub last: Vec<Comment>,
}

/// One element of an array expression. Either field may be absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayElem {
    pub index: Option<ArithmExpr>,
    pub value: Option<Word>,
    pub comments: Vec<Comment>,
}

/// A Bash extended-glob expression such as `@(foo|bar)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtGlob {
    pub op_pos: Pos,
    pub op: GlobOperator,
    pub pattern: Lit,
}

/// A Bash process substitution `<( ... )` / `>( ... )`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcSubst {
    pub op_pos: Pos,
    pub rparen: Pos,
    pub op: ProcOperator,
    pub stmts: Vec<Stmt>,
    pub last: Vec<Comment>,
}

/// A Bash `time` clause. `posix_format` is the `-p` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeClause {
    pub time: Pos,
    pub posix_format: bool,
    pub stmt: Option<Stmt>,
}

/// A Bash `coproc` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoprocClause {
    pub coproc: Pos,
    pub name: Option<Word>,
    pub stmt: Stmt,
}

/// A Bash `let` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LetClause {
    pub let_: Pos,
    pub exprs: Vec<ArithmExpr>,
}

/// A Bash brace expression `{a,b}` / `{1..10}` — only after brace splitting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BraceExp {
    pub sequence: bool, // `{x..y}` instead of `{x,y}`
    pub elems: Vec<Word>,
}

// ----------------------------------------------------------------------------
// Operators. Each maps to its source spelling via `as_str`, matching the
// `//line comment` spellings in mvdan/sh's tokens.go.
// ----------------------------------------------------------------------------

/// Generates a fieldless enum plus an `as_str` returning the source spelling,
/// and a `Display` impl forwarding to it.
macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $s),+ }
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

str_enum! {
    /// Redirection operators.
    RedirOperator {
        RdrOut => ">", AppOut => ">>", RdrIn => "<", RdrInOut => "<>",
        DplIn => "<&", DplOut => ">&", ClbOut => ">|", Hdoc => "<<",
        DashHdoc => "<<-", WordHdoc => "<<<", RdrAll => "&>", AppAll => "&>>",
    }
}

str_enum! {
    /// Process-substitution operators.
    ProcOperator { CmdIn => "<(", CmdOut => ">(" }
}

str_enum! {
    /// Extended-glob operators.
    GlobOperator {
        GlobZeroOrOne => "?(", GlobZeroOrMore => "*(", GlobOneOrMore => "+(",
        GlobOne => "@(", GlobExcept => "!(",
    }
}

str_enum! {
    /// Binary command operators: `&&`, `||`, `|`, `|&`.
    BinCmdOperator { AndStmt => "&&", OrStmt => "||", Pipe => "|", PipeAll => "|&" }
}

str_enum! {
    /// Case terminators: `;;`, `;&`, `;;&`, `;|`.
    CaseOperator { Break => ";;", Fallthrough => ";&", Resume => ";;&", ResumeKorn => ";|" }
}

str_enum! {
    /// `${!prefix*}` / `${!prefix@}` operators.
    ParNamesOperator { NamesPrefix => "*", NamesPrefixWords => "@" }
}

str_enum! {
    /// Parameter-expansion operators (`${a:-b}` etc).
    ParExpOperator {
        AlternateUnset => "+", AlternateUnsetOrNull => ":+",
        DefaultUnset => "-", DefaultUnsetOrNull => ":-",
        ErrorUnset => "?", ErrorUnsetOrNull => ":?",
        AssignUnset => "=", AssignUnsetOrNull => ":=",
        RemSmallSuffix => "%", RemLargeSuffix => "%%",
        RemSmallPrefix => "#", RemLargePrefix => "##",
        UpperFirst => "^", UpperAll => "^^",
        LowerFirst => ",", LowerAll => ",,",
        OtherParamOps => "@",
    }
}

str_enum! {
    /// Unary arithmetic operators.
    UnAritOperator {
        Not => "!", BitNegation => "~", Inc => "++", Dec => "--",
        Plus => "+", Minus => "-",
    }
}

str_enum! {
    /// Binary arithmetic operators (includes assignment and ternary pieces).
    BinAritOperator {
        Add => "+", Sub => "-", Mul => "*", Quo => "/", Rem => "%", Pow => "**",
        Eql => "==", Gtr => ">", Lss => "<", Neq => "!=", Leq => "<=", Geq => ">=",
        And => "&", Or => "|", Xor => "^", Shr => ">>", Shl => "<<",
        AndArit => "&&", OrArit => "||", Comma => ",",
        TernQuest => "?", TernColon => ":",
        Assgn => "=", AddAssgn => "+=", SubAssgn => "-=", MulAssgn => "*=",
        QuoAssgn => "/=", RemAssgn => "%=", AndAssgn => "&=", OrAssgn => "|=",
        XorAssgn => "^=", ShlAssgn => "<<=", ShrAssgn => ">>=",
    }
}

str_enum! {
    /// Unary test operators (`-e`, `-z`, `!`, ...).
    UnTestOperator {
        TsExists => "-e", TsRegFile => "-f", TsDirect => "-d", TsCharSp => "-c",
        TsBlckSp => "-b", TsNmPipe => "-p", TsSocket => "-S", TsSmbLink => "-L",
        TsSticky => "-k", TsGIDSet => "-g", TsUIDSet => "-u", TsGrpOwn => "-G",
        TsUsrOwn => "-O", TsModif => "-N", TsRead => "-r", TsWrite => "-w",
        TsExec => "-x", TsNoEmpty => "-s", TsFdTerm => "-t", TsEmpStr => "-z",
        TsNempStr => "-n", TsOptSet => "-o", TsVarSet => "-v", TsRefVar => "-R",
        TsNot => "!",
    }
}

str_enum! {
    /// Binary test operators (`-eq`, `=~`, `<`, ...).
    BinTestOperator {
        TsReMatch => "=~", TsNewer => "-nt", TsOlder => "-ot", TsDevIno => "-ef",
        TsEql => "-eq", TsNeq => "-ne", TsLeq => "-le", TsGeq => "-ge",
        TsLss => "-lt", TsGtr => "-gt", AndTest => "&&", OrTest => "||",
        TsMatchShort => "=", TsMatch => "==", TsNoMatch => "!=",
        TsBefore => "<", TsAfter => ">",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_spellings() {
        assert_eq!(BinCmdOperator::AndStmt.as_str(), "&&");
        assert_eq!(RedirOperator::DashHdoc.to_string(), "<<-");
        assert_eq!(BinTestOperator::TsReMatch.as_str(), "=~");
        assert_eq!(ParExpOperator::RemLargePrefix.as_str(), "##");
    }

    #[test]
    fn word_lit_joins_literals_only() {
        let p = Pos::new(0, 1, 1);
        let lit = |v: &str| Lit { value_pos: p, value_end: p, value: v.into() };
        let w = Word { parts: vec![WordPart::Lit(lit("foo")), WordPart::Lit(lit("bar"))] };
        assert_eq!(w.lit(), "foobar");

        let mixed = Word {
            parts: vec![
                WordPart::Lit(lit("foo")),
                WordPart::SglQuoted(SglQuoted { left: p, right: p, dollar: false, value: "x".into() }),
            ],
        };
        assert_eq!(mixed.lit(), "");
    }
}
