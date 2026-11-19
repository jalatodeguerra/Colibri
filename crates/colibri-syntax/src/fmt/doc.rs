//! A small Wadler/Prettier-style document model and its printer.
//!
//! The formatter builds a [`Doc`] for the whole file and [`render`] lays it out in a given
//! width. A [`Doc::Group`] is printed flat when it fits in the rest of the line and broken
//! (every direct line break becomes a newline) otherwise.

use std::cell::Cell;
use std::rc::Rc;

/// Spaces per indentation level.
pub(crate) const INDENT: usize = 4;

#[derive(Clone, Debug)]
pub(crate) enum Doc {
    Nil,
    Text(String),
    Concat(Vec<Doc>),
    /// Indents the line breaks inside by one level.
    Indent(Box<Doc>),
    /// A space when flat, a newline when broken.
    Line,
    /// Nothing when flat, a newline when broken.
    SoftLine,
    /// Always a newline; makes every enclosing group break.
    HardLine,
    /// `.1` is whether the group must break (it contains a forced break).
    Group(Box<Doc>, bool),
    /// `.0` printed flat if it fits (up to its first forced newline), else `.1` in break mode.
    Either(Box<Doc>, Box<Doc>),
    /// `.0` in break mode, `.1` in flat mode.
    IfBreak(Box<Doc>, Box<Doc>),
    /// Text deferred to the end of the current line (trailing comments).
    LineSuffix(String),
    /// Makes every enclosing group break.
    BreakParent,
    /// Spaces that align what follows with the widest of a set of lines: `.1` columns are
    /// already used, `.0` holds the widest (filled in while the document is built).
    Pad(Rc<Cell<usize>>, usize),
    /// A document that appears in more than one alternative: cloning it is cheap. `.1` is
    /// whether it is forced.
    Shared(Rc<Doc>, bool),
    /// A document that the enclosing groups measure as if it were flat, whatever mode they
    /// are in (a function's return type: the parameters break before it does). Printed like a
    /// group: flat if it fits, broken otherwise.
    Unit(Box<Doc>),
    /// The separator of a filled list: a space if the next item (`.0` columns wide, with the
    /// comma that follows it) still fits on the line, a line break otherwise. Flat: a space.
    FillSep(usize),
}

pub(crate) fn text(s: impl Into<String>) -> Doc {
    Doc::Text(s.into())
}

pub(crate) fn concat(parts: Vec<Doc>) -> Doc {
    Doc::Concat(parts)
}

/// Makes `doc` cheap to clone.
pub(crate) fn shared(doc: Doc) -> Doc {
    let forced = doc.forced();
    Doc::Shared(Rc::new(doc), forced)
}

pub(crate) fn indent(doc: Doc) -> Doc {
    Doc::Indent(Box::new(doc))
}

pub(crate) fn group(doc: Doc) -> Doc {
    let broken = doc.forced();
    Doc::Group(Box::new(doc), broken)
}

/// A group that always breaks.
pub(crate) fn broken_group(doc: Doc) -> Doc {
    Doc::Group(Box::new(doc), true)
}

/// A document the enclosing groups measure flat (see [`Doc::Unit`]).
pub(crate) fn unit(doc: Doc) -> Doc {
    Doc::Unit(Box::new(doc))
}

pub(crate) fn if_break(broken: Doc, flat: Doc) -> Doc {
    Doc::IfBreak(Box::new(broken), Box::new(flat))
}

/// Parts separated by `sep`.
pub(crate) fn join(sep: &Doc, parts: Vec<Doc>) -> Doc {
    let mut out = Vec::with_capacity(parts.len() * 2);
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            out.push(sep.clone());
        }
        out.push(part);
    }
    Doc::Concat(out)
}

impl Doc {
    /// Whether printing this document must contain a line break, which forces the groups
    /// around it to break.
    pub(crate) fn forced(&self) -> bool {
        match self {
            Doc::HardLine | Doc::BreakParent => true,
            Doc::Concat(parts) => parts.iter().any(Doc::forced),
            Doc::Indent(d) => d.forced(),
            Doc::Group(_, broken) => *broken,
            Doc::Shared(_, forced) => *forced,
            Doc::Unit(d) => d.forced(),
            Doc::Either(first, fallback) => first.forced() && fallback.forced(),
            Doc::Nil
            | Doc::Text(_)
            | Doc::Line
            | Doc::SoftLine
            | Doc::IfBreak(..)
            | Doc::LineSuffix(_)
            | Doc::FillSep(_)
            | Doc::Pad(..) => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
}

#[derive(Clone, Copy)]
struct Cmd<'a> {
    indent: usize,
    mode: Mode,
    doc: &'a Doc,
}

/// The number of columns of `s`.
fn width_of(s: &str) -> usize {
    s.chars().count()
}

/// Whether `first` (and the rest of its line, taken from `rest`) fits in `remaining` columns.
fn fits(first: Cmd<'_>, rest: &[Cmd<'_>], mut remaining: isize) -> bool {
    let mut stack: Vec<(Mode, &Doc)> = vec![(first.mode, first.doc)];
    let mut rest_index = rest.len();
    loop {
        let (mode, doc) = match stack.pop() {
            Some(item) => item,
            None => {
                if rest_index == 0 {
                    return true;
                }
                rest_index -= 1;
                (rest[rest_index].mode, rest[rest_index].doc)
            }
        };
        match doc {
            Doc::Nil | Doc::BreakParent | Doc::LineSuffix(_) => {}
            Doc::Pad(widest, used) => {
                remaining -= widest.get().saturating_sub(*used) as isize;
                if remaining < 0 {
                    return false;
                }
            }
            Doc::Text(s) => {
                if let Some(i) = s.find('\n') {
                    remaining -= width_of(&s[..i]) as isize;
                    return remaining >= 0;
                }
                remaining -= width_of(s) as isize;
                if remaining < 0 {
                    return false;
                }
            }
            Doc::Concat(parts) => stack.extend(parts.iter().rev().map(|d| (mode, d))),
            Doc::Shared(d, _) => stack.push((mode, d)),
            Doc::Unit(d) => stack.push((Mode::Flat, d)),
            Doc::Indent(d) => stack.push((mode, d)),
            Doc::FillSep(_) => {
                if mode == Mode::Break {
                    return true;
                }
                remaining -= 1;
                if remaining < 0 {
                    return false;
                }
            }
            Doc::Line => {
                if mode == Mode::Break {
                    return true;
                }
                remaining -= 1;
                if remaining < 0 {
                    return false;
                }
            }
            Doc::SoftLine => {
                if mode == Mode::Break {
                    return true;
                }
            }
            Doc::HardLine => return true,
            Doc::Group(d, broken) => {
                let mode = if *broken { Mode::Break } else { mode };
                stack.push((mode, d));
            }
            Doc::Either(first, fallback) => {
                stack.push((mode, if mode == Mode::Break { fallback } else { first }));
            }
            Doc::IfBreak(broken, flat) => {
                stack.push((mode, if mode == Mode::Break { broken } else { flat }));
            }
        }
    }
}

/// Lays `doc` out in `width` columns.
pub(crate) fn render(doc: &Doc, width: usize) -> String {
    let mut out = String::new();
    let mut column = 0usize;
    let mut suffix = String::new();
    let mut suffix_has_line_comment = false;
    let mut cmds: Vec<Cmd<'_>> = vec![Cmd {
        indent: 0,
        mode: Mode::Break,
        doc,
    }];
    while let Some(Cmd { indent, mode, doc }) = cmds.pop() {
        match doc {
            Doc::Nil | Doc::BreakParent => {}
            Doc::Text(s) => {
                out.push_str(s);
                match s.rfind('\n') {
                    Some(i) => column = width_of(&s[i + 1..]),
                    None => column += width_of(s),
                }
            }
            Doc::Concat(parts) => {
                for part in parts.iter().rev() {
                    cmds.push(Cmd {
                        indent,
                        mode,
                        doc: part,
                    });
                }
            }
            Doc::Shared(d, _) => cmds.push(Cmd {
                indent,
                mode,
                doc: d,
            }),
            Doc::Unit(d) => {
                let flat = Cmd {
                    indent,
                    mode: Mode::Flat,
                    doc: d,
                };
                if mode == Mode::Flat
                    || (!d.forced() && fits(flat, &cmds, width as isize - column as isize))
                {
                    cmds.push(flat);
                } else {
                    cmds.push(Cmd {
                        indent,
                        mode: Mode::Break,
                        doc: d,
                    });
                }
            }
            Doc::FillSep(next) => {
                if mode == Mode::Flat || column + 1 + next <= width {
                    out.push(' ');
                    column += 1;
                } else {
                    out.push_str(&suffix);
                    suffix.clear();
                    suffix_has_line_comment = false;
                    let kept = out.trim_end_matches(' ').len();
                    out.truncate(kept);
                    out.push('\n');
                    out.extend(std::iter::repeat_n(' ', indent));
                    column = indent;
                }
            }
            Doc::Indent(d) => cmds.push(Cmd {
                indent: indent + INDENT,
                mode,
                doc: d,
            }),
            Doc::Line | Doc::SoftLine | Doc::HardLine => {
                let flat = mode == Mode::Flat && !matches!(doc, Doc::HardLine);
                if flat {
                    if matches!(doc, Doc::Line) {
                        out.push(' ');
                        column += 1;
                    }
                } else {
                    out.push_str(&suffix);
                    suffix.clear();
                    suffix_has_line_comment = false;
                    let kept = out.trim_end_matches(' ').len();
                    out.truncate(kept);
                    out.push('\n');
                    for _ in 0..indent {
                        out.push(' ');
                    }
                    column = indent;
                }
            }
            Doc::Group(d, broken) => {
                let flat = Cmd {
                    indent,
                    mode: Mode::Flat,
                    doc: d,
                };
                if mode == Mode::Flat {
                    cmds.push(Cmd {
                        indent,
                        mode: if *broken { Mode::Break } else { Mode::Flat },
                        doc: d,
                    });
                } else if !*broken && fits(flat, &cmds, width as isize - column as isize) {
                    cmds.push(flat);
                } else {
                    cmds.push(Cmd {
                        indent,
                        mode: Mode::Break,
                        doc: d,
                    });
                }
            }
            Doc::Either(first, fallback) => {
                let flat = Cmd {
                    indent,
                    mode: Mode::Flat,
                    doc: first,
                };
                if mode == Mode::Flat || fits(flat, &cmds, width as isize - column as isize) {
                    cmds.push(flat);
                } else {
                    cmds.push(Cmd {
                        indent,
                        mode: Mode::Break,
                        doc: fallback,
                    });
                }
            }
            Doc::IfBreak(broken, flat) => cmds.push(Cmd {
                indent,
                mode,
                doc: if mode == Mode::Break { broken } else { flat },
            }),
            Doc::LineSuffix(s) => {
                // Two line comments cannot share a line: the second would become part of
                // the first.
                if suffix_has_line_comment {
                    out.push_str(&suffix);
                    suffix.clear();
                    out.push('\n');
                    out.extend(std::iter::repeat_n(' ', indent));
                    column = indent;
                    suffix_has_line_comment = false;
                    suffix.push_str(s.trim_start());
                } else {
                    suffix.push_str(s);
                }
                suffix_has_line_comment |= s.trim_start().starts_with("//");
            }
            Doc::Pad(widest, used) => {
                let n = widest.get().saturating_sub(*used);
                out.extend(std::iter::repeat_n(' ', n));
                column += n;
            }
        }
    }
    out.push_str(&suffix);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Doc {
        group(concat(vec![
            text("["),
            indent(concat(vec![
                Doc::SoftLine,
                join(
                    &concat(vec![text(","), Doc::Line]),
                    items.iter().map(|s| text(*s)).collect(),
                ),
                if_break(text(","), Doc::Nil),
            ])),
            Doc::SoftLine,
            text("]"),
        ]))
    }

    #[test]
    fn group_fits_or_breaks() {
        let doc = list(&["aaa", "bbb"]);
        assert_eq!(render(&doc, 20), "[aaa, bbb]");
        assert_eq!(render(&doc, 8), "[\n    aaa,\n    bbb,\n]");
    }

    #[test]
    fn line_suffix_goes_to_the_end_of_the_line() {
        let doc = concat(vec![
            text("a"),
            Doc::LineSuffix(" // c".into()),
            text(" b"),
            Doc::HardLine,
            text("c"),
        ]);
        assert_eq!(render(&doc, 80), "a b // c\nc");
    }

    #[test]
    fn hard_line_breaks_the_group() {
        let doc = group(concat(vec![
            text("("),
            indent(concat(vec![
                Doc::SoftLine,
                text("x"),
                Doc::HardLine,
                text("y"),
            ])),
            Doc::SoftLine,
            text(")"),
        ]));
        assert_eq!(render(&doc, 80), "(\n    x\n    y\n)");
    }
}
