//! A width-driven pretty-printing engine (Wadler/Lindig "Strictly Pretty").
//!
//! This is the layout core fmt-rs uses for M2 (length-driven wrapping). The
//! shell printer builds a [`Doc`] tree from the AST and hands it to [`pretty`],
//! which decides where to break based on a target line width.
//!
//! Why this and not shfmt's imperative, position-driven printer? shfmt
//! preserves the *user's* original line breaks because it formats
//! human-authored scripts. fmt-rs formats machine-generated one-liners that
//! carry no layout intent — we want canonical, width-aware reflow, which is
//! exactly what `group` gives us: a group lays out flat if it fits the
//! remaining width, otherwise every soft break inside it turns into a newline.
//!
//! The algebra:
//! - [`Doc::text`] — literal text (must not contain `\n`).
//! - [`Doc::line`] — a space when flat, a newline when broken.
//! - [`Doc::soft_line`] — nothing when flat, a newline when broken.
//! - [`Doc::hard_line`] — always a newline; forces every enclosing group to break.
//! - [`Doc::concat`] — sequence.
//! - [`Doc::nest`] — indent everything inside by N columns after each newline.
//! - [`Doc::group`] — try flat; fall back to broken if it doesn't fit.

use std::borrow::Cow;
use std::rc::Rc;

/// A document to be laid out. Cheap to clone (`Rc`-shared children).
#[derive(Debug, Clone)]
pub enum Doc {
    Nil,
    Text(Cow<'static, str>),
    /// Flat: a space. Broken: a newline.
    Line,
    /// Flat: nothing. Broken: a newline.
    SoftLine,
    /// Always a newline; forces enclosing groups to break.
    HardLine,
    Concat(Rc<[Doc]>),
    Nest(isize, Rc<Doc>),
    Group(Rc<Doc>),
    /// Renders `flat` when the enclosing group is laid out flat, and `broken`
    /// when it is broken. Useful for separators that exist only on one line,
    /// e.g. a `;` between inline statements that vanishes once they wrap.
    IfBreak {
        flat: Rc<Doc>,
        broken: Rc<Doc>,
    },
}

impl Doc {
    pub fn text(s: impl Into<Cow<'static, str>>) -> Doc {
        Doc::Text(s.into())
    }

    pub const fn line() -> Doc {
        Doc::Line
    }

    pub const fn soft_line() -> Doc {
        Doc::SoftLine
    }

    pub const fn hard_line() -> Doc {
        Doc::HardLine
    }

    pub fn concat<I: IntoIterator<Item = Doc>>(docs: I) -> Doc {
        let v: Vec<Doc> = docs.into_iter().filter(|d| !matches!(d, Doc::Nil)).collect();
        match v.len() {
            0 => Doc::Nil,
            1 => v.into_iter().next().unwrap(),
            _ => Doc::Concat(v.into()),
        }
    }

    pub fn nest(indent: isize, doc: Doc) -> Doc {
        Doc::Nest(indent, Rc::new(doc))
    }

    pub fn group(doc: Doc) -> Doc {
        Doc::Group(Rc::new(doc))
    }

    /// `flat` when the enclosing group is flat, `broken` when it breaks.
    pub fn if_break(broken: Doc, flat: Doc) -> Doc {
        Doc::IfBreak {
            flat: Rc::new(flat),
            broken: Rc::new(broken),
        }
    }

    /// Join `docs` with `sep` between each pair.
    pub fn join<I: IntoIterator<Item = Doc>>(sep: Doc, docs: I) -> Doc {
        let mut out = Vec::new();
        for (i, d) in docs.into_iter().enumerate() {
            if i > 0 {
                out.push(sep.clone());
            }
            out.push(d);
        }
        Doc::concat(out)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
}

/// Whether `doc` contains a hard line anywhere in its subtree. Such a doc can
/// never lie flat, so any enclosing group is forced to break (this is Prettier's
/// "break propagation"). We recurse through groups too, since a hard line inside
/// a nested group still breaks every ancestor.
fn contains_hard(doc: &Doc) -> bool {
    match doc {
        Doc::HardLine => true,
        Doc::Concat(items) => items.iter().any(contains_hard),
        Doc::Nest(_, inner) | Doc::Group(inner) => contains_hard(inner),
        _ => false,
    }
}

/// Whether the flat layout of `items` fits within `remaining` columns, stopping
/// at the first newline that ends the current line.
fn fits(mut remaining: isize, indent: isize, first: &Doc, rest: &[(isize, Mode, Doc)]) -> bool {
    // A small explicit stack so deep docs don't blow the call stack.
    let mut stack: Vec<(isize, Mode, Doc)> = Vec::new();
    stack.push((indent, Mode::Flat, first.clone()));
    // We also need to look past the current item into the rest of the work, but
    // only as much as fits on this line. `rest` is the layout work stack, which
    // is processed LIFO, so we look ahead from its top (back) to its front.
    let mut rest_iter = rest.iter().rev();

    loop {
        if remaining < 0 {
            return false;
        }
        let (ind, mode, doc) = match stack.pop() {
            Some(x) => x,
            None => match rest_iter.next() {
                Some((i, m, d)) => (*i, *m, d.clone()),
                None => return true,
            },
        };
        match doc {
            Doc::Nil => {}
            Doc::Text(s) => remaining -= s.chars().count() as isize,
            Doc::Line => match mode {
                Mode::Flat => remaining -= 1, // a space
                Mode::Break => return true,   // newline ends the line: it fits
            },
            Doc::SoftLine => match mode {
                Mode::Flat => {} // nothing
                Mode::Break => return true,
            },
            // A hard line ends the current line, so everything measured so far
            // fits. (Groups that *contain* a hard line are never flat-tested —
            // break propagation forces them to break before we get here — so a
            // hard line reached during measurement always comes from the work
            // queued after the group under test.)
            Doc::HardLine => return true,
            Doc::Concat(items) => {
                for it in items.iter().rev() {
                    stack.push((ind, mode, it.clone()));
                }
            }
            Doc::Nest(j, inner) => stack.push((ind + j, mode, (*inner).clone())),
            // Groups inside a flat context stay flat.
            Doc::Group(inner) => stack.push((ind, Mode::Flat, (*inner).clone())),
            Doc::IfBreak { flat, broken } => {
                let chosen = match mode {
                    Mode::Flat => flat,
                    Mode::Break => broken,
                };
                stack.push((ind, mode, (*chosen).clone()));
            }
        }
    }
}

/// Lay out `doc` targeting `width` columns. Returns the formatted string.
pub fn pretty(width: isize, doc: &Doc) -> String {
    let mut out = String::new();
    let mut col: isize = 0;
    // Work stack of (indent, mode, doc); processed LIFO, so children are pushed
    // in reverse order.
    let mut stack: Vec<(isize, Mode, Doc)> = vec![(0, Mode::Break, doc.clone())];

    while let Some((indent, mode, doc)) = stack.pop() {
        match doc {
            Doc::Nil => {}
            Doc::Text(s) => {
                out.push_str(&s);
                col += s.chars().count() as isize;
            }
            Doc::Line => match mode {
                Mode::Flat => {
                    out.push(' ');
                    col += 1;
                }
                Mode::Break => {
                    out.push('\n');
                    for _ in 0..indent {
                        out.push(' ');
                    }
                    col = indent;
                }
            },
            Doc::SoftLine => match mode {
                Mode::Flat => {}
                Mode::Break => {
                    out.push('\n');
                    for _ in 0..indent {
                        out.push(' ');
                    }
                    col = indent;
                }
            },
            Doc::HardLine => {
                out.push('\n');
                for _ in 0..indent {
                    out.push(' ');
                }
                col = indent;
            }
            Doc::Concat(items) => {
                for it in items.iter().rev() {
                    stack.push((indent, mode, it.clone()));
                }
            }
            Doc::Nest(j, inner) => stack.push((indent + j, mode, (*inner).clone())),
            Doc::Group(inner) => {
                // A group that contains a hard line must break (break
                // propagation); otherwise try it flat and fall back to broken if
                // it doesn't fit the remaining width (looking ahead too).
                let group_mode = if contains_hard(&inner) {
                    Mode::Break
                } else if fits(width - col, indent, &inner, &stack) {
                    Mode::Flat
                } else {
                    Mode::Break
                };
                stack.push((indent, group_mode, (*inner).clone()));
            }
            Doc::IfBreak { flat, broken } => {
                let chosen = match mode {
                    Mode::Flat => flat,
                    Mode::Break => broken,
                };
                stack.push((indent, mode, (*chosen).clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d_text(s: &'static str) -> Doc {
        Doc::text(s)
    }

    #[test]
    fn flat_when_it_fits() {
        // `a && b && c` fits in 80 cols, so it stays on one line.
        let chain = Doc::group(Doc::nest(
            2,
            Doc::concat([
                d_text("a"),
                Doc::line(),
                d_text("&& b"),
                Doc::line(),
                d_text("&& c"),
            ]),
        ));
        assert_eq!(pretty(80, &chain), "a && b && c");
    }

    #[test]
    fn breaks_when_too_wide() {
        // The same chain at width 6 must break at each `line`, indenting by 2.
        let chain = Doc::group(Doc::nest(
            2,
            Doc::concat([
                d_text("a"),
                Doc::line(),
                d_text("&& b"),
                Doc::line(),
                d_text("&& c"),
            ]),
        ));
        assert_eq!(pretty(6, &chain), "a\n  && b\n  && c");
    }

    #[test]
    fn hard_line_forces_break_even_when_short() {
        let doc = Doc::group(Doc::concat([
            d_text("do"),
            Doc::hard_line(),
            d_text("done"),
        ]));
        assert_eq!(pretty(80, &doc), "do\ndone");
    }

    #[test]
    fn soft_line_disappears_when_flat() {
        let doc = Doc::group(Doc::concat([d_text("$("), Doc::soft_line(), d_text("cmd)")]));
        assert_eq!(pretty(80, &doc), "$(cmd)");
    }

    #[test]
    fn if_break_picks_branch_by_mode() {
        // A `;` that appears only when the group is flat.
        let body = |open: &'static str| {
            Doc::group(Doc::concat([
                d_text("do"),
                Doc::nest(2, Doc::concat([Doc::line(), d_text(open)])),
                Doc::if_break(Doc::Nil, d_text(";")),
                Doc::line(),
                d_text("done"),
            ]))
        };
        assert_eq!(pretty(80, &body("echo x")), "do echo x; done");
        assert_eq!(pretty(8, &body("echo x")), "do\n  echo x\ndone");
    }

    #[test]
    fn nested_groups_break_independently() {
        // Outer breaks, but the inner group still fits flat on its own line.
        let inner = Doc::group(Doc::concat([d_text("x"), Doc::line(), d_text("y")]));
        let outer = Doc::group(Doc::nest(
            2,
            Doc::concat([d_text("outer:"), Doc::line(), inner, Doc::line(), d_text("tail-is-long")]),
        ));
        // width 12 forces the outer to break; "x y" (inner) fits flat.
        assert_eq!(pretty(12, &outer), "outer:\n  x y\n  tail-is-long");
    }
}
