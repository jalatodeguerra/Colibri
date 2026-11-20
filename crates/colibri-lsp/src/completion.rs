//! `textDocument/completion`.
//!
//! Three kinds of place are completed:
//!
//! - after a dot, `receiver.|`: the fields and methods of the receiver's type, the items of
//!   an imported module, or the variants and associated functions of a type;
//! - a name, `pri|`: what is in scope at the cursor (locals, parameters, the module's items
//!   and imports, the prelude, keywords) and, where a `match` arm starts, the variants of the
//!   enum that is matched;
//! - `.|` with nothing in front of it: the variants of the enum that is matched.
//!
//! A file the user is typing in does not parse or check at the cursor (`foo.` followed by a
//! `}`). The text is therefore *repaired* first: the dot and the word being typed are taken
//! out, so that the receiver stands alone as an ordinary expression, and the repaired text is
//! checked in place of the buffer. Where the repaired program still cannot be checked (a
//! syntax error elsewhere in the file), the types come from the last analysis of the file
//! that had them, with the receiver's position carried over from the old text to the new by
//! the text the two share before and after the edit.

use std::collections::HashMap;
use std::path::Path;

use colibri_check::insight::{Member, MemberKind, PreludeKind, prelude_names};
use colibri_syntax::ast::{Ident as ast_ident, ItemKind, TopLevel};
use colibri_syntax::{FileId, Span, TokenKind, lex};
use lsp_types::{CompletionItem, CompletionItemKind, CompletionItemTag};

use crate::analysis::Analysis;
use crate::resolve::{DefKind, ModRef, Target, resolve};

/// What the cursor is in.
#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    /// `receiver.|`; the receiver is a byte range of the buffer.
    Member {
        receiver: (usize, usize),
        /// The dot is the first thing on its line: a method chain continued, or (in the
        /// arms of a `match`) the start of a variant pattern.
        line_start: bool,
    },
    /// `.|` with no receiver: a variant the type says.
    DotVariant,
    /// A name.
    Ident,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Ctx {
    pub kind: Kind,
    /// The word at the cursor (empty if none), as a byte range.
    pub word: (usize, usize),
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether the text of a string token ends with its own closing quote(s): `"`, `"""` or `"`
/// and the `#`s of a raw string, not an escaped quote.
fn string_terminated(token: &str) -> bool {
    let rest = token.strip_prefix('r').unwrap_or(token);
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    let quotes = if hashes == 0 && rest.starts_with("\"\"\"") {
        3
    } else {
        1
    };
    let closer = format!("{}{}", "\"".repeat(quotes), "#".repeat(hashes));
    let opener = token.len() - rest.len() + hashes + quotes;
    if token.len() < opener + closer.len() || !token.ends_with(&closer) {
        return false;
    }
    // A quote after an odd number of backslashes is escaped (in a string that has escapes).
    if token.starts_with('r') {
        return true;
    }
    let before = &token[..token.len() - closer.len()];
    (before.len() - before.trim_end_matches('\\').len()).is_multiple_of(2)
}

/// Whether the offset is inside a string (outside its interpolation holes) or a comment.
fn in_string_or_comment(text: &str, offset: usize) -> bool {
    let (tokens, _) = lex(FileId(0), text);
    let mut prev_end = 0;
    for t in &tokens {
        let (s, e) = (t.span.start as usize, t.span.end as usize);
        match &t.kind {
            TokenKind::Str(lit) => {
                let terminated = string_terminated(&text[s..e]);
                let inside = (s < offset && offset < e) || (offset == e && !terminated);
                if inside {
                    let in_hole = lit.parts.iter().any(|p| match p {
                        colibri_syntax::StrPart::Interp { expr, .. } => {
                            expr.start as usize <= offset && offset <= expr.end as usize
                        }
                        colibri_syntax::StrPart::Lit(_) => false,
                    });
                    if !in_hole {
                        return true;
                    }
                }
            }
            TokenKind::DocComment(_) if s <= offset && offset <= e => return true,
            _ => {}
        }
        if e <= offset {
            prev_end = e;
        }
    }
    // Plain comments are not tokens: look for one in the text since the last token.
    let gap = &text[prev_end.min(offset)..offset];
    let line_gap = gap.rsplit('\n').next().unwrap_or(gap);
    line_gap.contains("//") || gap.rfind("/*") > gap.rfind("*/")
}

/// Where the cursor is: `None` where nothing should be offered (in a string, a comment or a
/// number).
pub fn context(text: &str, offset: usize) -> Option<Ctx> {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    if in_string_or_comment(text, offset) {
        return None;
    }
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(offset, |(i, _)| i);
    let end = text[offset..]
        .char_indices()
        .find(|(_, c)| !is_word(*c))
        .map_or(text.len(), |(i, _)| offset + i);
    if text[start..end]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit())
    {
        return None;
    }
    let before = &text[..start];
    let kind = if before.ends_with('.') && !before.ends_with("..") {
        let dot = start - 1;
        let receiver_end = before[..dot].trim_end().len();
        let last = before[..receiver_end].chars().next_back();
        match last {
            Some(c) if is_word(c) || ")]}\"?'".contains(c) => {
                // `1.` is a number, not a receiver.
                let run = before[..receiver_end]
                    .chars()
                    .rev()
                    .take_while(|c| is_word(*c))
                    .collect::<String>();
                if !run.is_empty() && run.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                let line_start = text[..dot]
                    .rsplit('\n')
                    .next()
                    .is_some_and(|l| l.trim().is_empty());
                // A line that starts with `.` and a capitalised name starts an arm.
                let capitalised = text[start..end]
                    .chars()
                    .next()
                    .is_some_and(char::is_uppercase);
                if line_start && capitalised {
                    Kind::DotVariant
                } else {
                    let s = receiver_start(text, receiver_end)?;
                    Kind::Member {
                        receiver: (s, receiver_end),
                        line_start,
                    }
                }
            }
            _ => Kind::DotVariant,
        }
    } else {
        Kind::Ident
    };
    Some(Ctx {
        kind,
        word: (start, end),
    })
}

/// The start of the expression that ends at `end`, as far as a chain of calls, indexing,
/// fields and brackets goes: `a.b(c)[0]?` .
fn receiver_start(text: &str, end: usize) -> Option<usize> {
    let chars: Vec<(usize, char)> = text[..end].char_indices().collect();
    let mut i = chars.len();
    loop {
        while i > 0 && chars[i - 1].1 == '?' {
            i -= 1;
        }
        if i == 0 {
            return None;
        }
        let c = chars[i - 1].1;
        match c {
            ')' | ']' | '}' => {
                i = matching_open(&chars, i - 1)?;
                // A call or index: the name in front of the bracket belongs to it.
                let mut j = i;
                while j > 0 && is_word(chars[j - 1].1) {
                    j -= 1;
                }
                i = j;
            }
            '"' => {
                let mut j = i - 1;
                loop {
                    if j == 0 {
                        return None;
                    }
                    j -= 1;
                    if chars[j].1 == '"' && (j == 0 || chars[j - 1].1 != '\\') {
                        break;
                    }
                }
                i = j;
            }
            c if is_word(c) => {
                while i > 0 && is_word(chars[i - 1].1) {
                    i -= 1;
                }
            }
            _ => return None,
        }
        // A dot in front continues the chain, across white space and lines.
        let mut k = i;
        while k > 0 && chars[k - 1].1.is_whitespace() {
            k -= 1;
        }
        if k > 0 && chars[k - 1].1 == '.' && !(k >= 2 && chars[k - 2].1 == '.') {
            k -= 1;
            while k > 0 && chars[k - 1].1.is_whitespace() {
                k -= 1;
            }
            // `.map(..)` at the start of a line has the receiver on the line before.
            if k == 0 {
                return None;
            }
            i = k;
            continue;
        }
        return chars.get(i).map(|(b, _)| *b).or(Some(end));
    }
}

/// The index of the bracket that opens the one at `close`.
fn matching_open(chars: &[(usize, char)], close: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = close + 1;
    while i > 0 {
        i -= 1;
        match chars[i].1 {
            ')' | ']' | '}' => depth += 1,
            '(' | '[' | '{' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            '"' => {
                // Skip a string, backwards.
                while i > 0 {
                    i -= 1;
                    if chars[i].1 == '"' && (i == 0 || chars[i - 1].1 != '\\') {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// The text with the dot and the word at the cursor taken out, so that the receiver stands
/// alone; `None` unless the cursor follows a dot.
pub fn repair(text: &str, ctx: &Ctx) -> Option<String> {
    let Kind::Member { receiver, .. } = &ctx.kind else {
        return None;
    };
    Some(format!("{} {}", &text[..receiver.1], &text[ctx.word.1..]))
}

/// Carries a byte range of `new` over to `old`, when both share the text around it (the
/// text before the first and after the last place where they differ).
pub fn map_span(old: &str, new: &str, from: (usize, usize)) -> Option<(usize, usize)> {
    let prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = old.len().min(new.len()) - prefix.min(old.len().min(new.len()));
    let suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let mapped = if from.1 <= prefix {
        from
    } else if from.0 >= new.len() - suffix {
        let shift = |x: usize| (x + old.len()).checked_sub(new.len());
        (shift(from.0)?, shift(from.1)?)
    } else {
        return None;
    };
    (old.get(mapped.0..mapped.1)? == new.get(from.0..from.1)?).then_some(mapped)
}

/// Where an offset of `new` is in `old`: the same place when the text around it did not
/// change, else the point where the two texts start to differ.
fn old_position(old: &str, new: &str, at: usize) -> usize {
    let prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    if at <= prefix {
        return at;
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    if at >= new.len() - suffix {
        return (at + old.len()).saturating_sub(new.len());
    }
    prefix
}

/// Where an offset of `old` is in `new`, when the text around it did not change: before the
/// first place where the two differ, or after the last.
fn new_position(old: &str, new: &str, at: usize) -> Option<usize> {
    let prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    if at <= prefix {
        return Some(at);
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    if at >= old.len() - suffix {
        return (at + new.len()).checked_sub(old.len());
    }
    None
}

/// Whether `text` between the byte offsets `from` and `to` binds `name` again: in the pattern
/// of a `let`, `var` or `for`, or as a name matched in an arm (`x =>`). Meant to be sure
/// rather than precise: a hit means the name may no longer mean what it did.
fn redeclared(text: &str, from: usize, to: usize, name: &str) -> bool {
    let (tokens, _) = lex(FileId(0), text);
    let mut in_pattern = false;
    let mut prev_dot = false;
    for (i, t) in tokens.iter().enumerate() {
        if (t.span.end as usize) <= from {
            continue;
        }
        if (t.span.start as usize) >= to {
            break;
        }
        match &t.kind {
            TokenKind::Let | TokenKind::Var | TokenKind::For => in_pattern = true,
            TokenKind::Eq
            | TokenKind::In
            | TokenKind::Semi
            | TokenKind::LBrace
            | TokenKind::RBrace
            | TokenKind::FatArrow => in_pattern = false,
            TokenKind::Ident(n) if n == name && !prev_dot => {
                let arm = tokens
                    .get(i + 1)
                    .is_some_and(|n| n.kind == TokenKind::FatArrow);
                if in_pattern || arm {
                    return true;
                }
            }
            _ => {}
        }
        prev_dot = t.kind == TokenKind::Dot;
    }
    false
}

// ---- the items ----

/// What the completion functions read.
pub struct Env<'a> {
    pub path: &'a Path,
    /// The analysis of the text the cursor is in (repaired after a dot).
    pub analysis: &'a Analysis,
    pub text: &'a str,
    /// The last analysis of the file whose program had types, with its text.
    pub good: Option<(&'a str, &'a Analysis)>,
}

impl Env<'_> {
    fn file(&self) -> Option<FileId> {
        self.analysis.file_of(self.path)
    }

    /// The package of the module being edited.
    fn package(&self) -> Option<usize> {
        let m = self.analysis.module_of(self.file()?)?;
        self.analysis.program.metas.get(m).map(|m| m.package)
    }

    /// The analysis that knows the type of the expression at `range` of the text, and the
    /// type as written and its members: the analysis of the text itself, else the last good
    /// one.
    fn typed(&self, range: (usize, usize)) -> Option<Typed<'_>> {
        // `(p)` has no type of its own: the inside has.
        self.typed_exact(range).or_else(|| {
            let text = self.text.get(range.0..range.1)?;
            let inner = text.strip_prefix('(')?.strip_suffix(')')?;
            // The brackets must be one pair: `(a).f(b)` is not wrapped by them.
            let mut depth = 0i32;
            for c in inner.chars() {
                depth += match c {
                    '(' => 1,
                    ')' => -1,
                    _ => 0,
                };
                if depth < 0 {
                    return None;
                }
            }
            let lead = inner.len() - inner.trim_start().len();
            let start = range.0 + 1 + lead;
            self.typed((start, start + inner.trim().len()))
        })
    }

    fn typed_exact(&self, range: (usize, usize)) -> Option<Typed<'_>> {
        let known = |analysis: &'_ Analysis, r: (usize, usize)| -> Option<(String, Vec<Member>)> {
            let file = analysis.file_of(self.path)?;
            let span = Span::new(file, r.0 as u32, r.1 as u32);
            let ty = analysis.insight.type_at(span)?;
            Some((ty.to_string(), analysis.insight.members_at(span).to_vec()))
        };
        if let Some((ty, members)) = known(self.analysis, range) {
            return Some(Typed {
                analysis: self.analysis,
                ty,
                members,
            });
        }
        let (old, analysis) = self.good?;
        if let Some(mapped) = map_span(old, self.text, range)
            && let Some((ty, members)) = known(analysis, mapped)
        {
            return Some(Typed {
                analysis,
                ty,
                members,
            });
        }
        // The receiver is new text: a name in scope where the edit is has the type it had.
        let name = self.text.get(range.0..range.1)?;
        if name.is_empty() || !name.chars().all(is_word) {
            return None;
        }
        let file = analysis.file_of(self.path)?;
        let at = old_position(old, self.text, range.0);
        let res = resolve(analysis, Some((file, at as u32)));
        let decl = res
            .scope
            .iter()
            .find(|l| l.name == name)
            .map(|l| l.span)
            .or_else(|| {
                res.module_scope
                    .iter()
                    .find(|(n, k, _)| n == name && *k == DefKind::Global)
                    .map(|(_, _, s)| *s)
            })?;
        // Not when the current text declares the name again after that binding: the old
        // type is then very likely the wrong one.
        let declared_at = new_position(old, self.text, decl.end as usize)?;
        if redeclared(self.text, declared_at, range.0, name) {
            return None;
        }
        let ty = match analysis.insight.binding_type(decl) {
            Some(ty) => ty.to_string(),
            None => details(analysis)
                .remove(&decl)?
                .split_once(": ")?
                .1
                .to_string(),
        };
        Some(Typed {
            analysis,
            ty,
            members: analysis.insight.binding_members(decl).to_vec(),
        })
    }

    /// The members of the type of the expression at `range`.
    fn members_at(&self, range: (usize, usize)) -> Vec<Member> {
        self.typed(range).map(|t| t.members).unwrap_or_default()
    }
}

/// What is known of the type of an expression.
struct Typed<'a> {
    analysis: &'a Analysis,
    /// As written.
    ty: String,
    members: Vec<Member>,
}

/// The declaration texts the checker recorded, by the span of the declared name.
fn details(analysis: &Analysis) -> HashMap<Span, String> {
    analysis
        .insight
        .facts()
        .iter()
        .filter(|f| f.name.is_none() && f.def == Some(f.span))
        .map(|f| (f.span, unqualify(&f.text)))
        .collect()
}

/// `fn util.double(x: int)` as `fn double(x: int)`: the module is the one written in front.
fn unqualify(text: &str) -> String {
    let Some(rest) = text.strip_prefix("fn ") else {
        return text.to_string();
    };
    let head_end = rest.find(['(', '[']).unwrap_or(rest.len());
    let name = rest[..head_end]
        .rsplit('.')
        .next()
        .unwrap_or(&rest[..head_end]);
    format!("fn {name}{}", &rest[head_end..])
}

fn def_kind(kind: DefKind) -> CompletionItemKind {
    match kind {
        DefKind::Local | DefKind::Param | DefKind::Global | DefKind::ImportAlias => {
            CompletionItemKind::VARIABLE
        }
        DefKind::Function => CompletionItemKind::FUNCTION,
        DefKind::Method => CompletionItemKind::METHOD,
        DefKind::Type => CompletionItemKind::CLASS,
        DefKind::Field => CompletionItemKind::FIELD,
        DefKind::Variant => CompletionItemKind::ENUM_MEMBER,
        DefKind::Generic => CompletionItemKind::TYPE_PARAMETER,
        DefKind::Module => CompletionItemKind::MODULE,
    }
}

fn item(
    label: &str,
    kind: CompletionItemKind,
    detail: Option<String>,
    rank: &str,
) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        detail,
        sort_text: Some(format!("{rank}{label}")),
        ..CompletionItem::default()
    }
}

/// Marks `item` as deprecated (editors strike its name through).
fn tagged(mut item: CompletionItem, deprecated: bool) -> CompletionItem {
    if deprecated {
        item.tags = Some(vec![CompletionItemTag::DEPRECATED]);
    }
    item
}

/// Marks `item` as deprecated when the declaration at `def` is (editors strike its name through).
fn deprecated_if(mut item: CompletionItem, analysis: &Analysis, def: Span) -> CompletionItem {
    if analysis.insight.deprecation(def).is_some() {
        item.tags = Some(vec![CompletionItemTag::DEPRECATED]);
    }
    item
}

/// Completion after a dot.
pub fn member_items(
    env: &Env<'_>,
    receiver: (usize, usize),
    line_start: bool,
) -> Vec<CompletionItem> {
    let mut out = member_items_of(env, receiver);
    // `.` at the start of a line in the arms of a `match`: a chain, or a variant.
    if line_start {
        out.extend(arm_variants(env, receiver.1));
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|i| seen.insert(i.label.clone()));
    out
}

fn member_items_of(env: &Env<'_>, receiver: (usize, usize)) -> Vec<CompletionItem> {
    let Some(file) = env.file() else {
        return Vec::new();
    };
    let text = &env.text[receiver.0..receiver.1];
    let is_name = !text.is_empty() && text.chars().all(is_word);
    let in_type = in_type_position(env.text, receiver.0);
    if is_name {
        let res = resolve(env.analysis, Some((file, receiver.0 as u32)));
        let span = Span::new(file, receiver.0 as u32, receiver.1 as u32);
        let def = res
            .occs
            .iter()
            .find(|o| o.span == span)
            .and_then(|o| match &o.target {
                Target::Def(d) => Some(*d),
                Target::External(_) => None,
            })
            // Not recorded where the text does not check (a type position, an arm): the
            // imported modules in scope there.
            .or_else(|| {
                res.module_scope
                    .iter()
                    .find(|(n, k, _)| n == text && *k == DefKind::Module)
                    .map(|(_, _, s)| *s)
            });
        if let Some(d) = def
            && let Some(info) = res.defs.get(&d)
        {
            match info.kind {
                DefKind::Module => {
                    if let Some(target) = res.module_refs.get(&d) {
                        return module_items(env, target, in_type);
                    }
                    return Vec::new();
                }
                DefKind::Type => {
                    let detail = details(env.analysis);
                    return res
                        .type_members
                        .get(&d)
                        .into_iter()
                        .flatten()
                        .filter(|m| match m.kind {
                            DefKind::Variant => true,
                            DefKind::Method => !m.receiver,
                            _ => false,
                        })
                        .map(|m| {
                            deprecated_if(
                                item(&m.name, def_kind(m.kind), detail.get(&m.span).cloned(), "0"),
                                env.analysis,
                                m.span,
                            )
                        })
                        .collect();
                }
                _ => {}
            }
        }
    } else if let Some((module, name)) = text.split_once('.')
        && !name.contains('.')
        && !module.is_empty()
        && !name.is_empty()
        && module.chars().all(is_word)
        && name.chars().all(is_word)
    {
        // `module.Type.`: the variants and associated functions of a type of a module.
        let res = resolve(env.analysis, Some((file, receiver.0 as u32)));
        return qualified_type_items(env, &res, module, name);
    }
    let package = env.package();
    env.members_at(receiver)
        .into_iter()
        .filter(|m| m.public || m.package.is_none() || m.package == package)
        .map(|m| match m.kind {
            MemberKind::Field => tagged(
                item(&m.name, CompletionItemKind::FIELD, Some(m.detail), "0"),
                m.deprecated,
            ),
            MemberKind::Method => tagged(
                item(&m.name, CompletionItemKind::METHOD, Some(m.detail), "1"),
                m.deprecated,
            ),
        })
        .collect()
}

/// What `module.|` offers: the module's public items, and every item to code of its package;
/// only its types where a type is written (`types_only`).
fn module_items(env: &Env<'_>, target: &ModRef, types_only: bool) -> Vec<CompletionItem> {
    let mut out = Vec::new();
    match target {
        ModRef::Source(i) => {
            let a = env.analysis;
            let Some(file) = a.program.files.get(*i) else {
                return out;
            };
            let same_package = env
                .package()
                .is_some_and(|p| a.program.metas.get(*i).is_some_and(|m| m.package == p));
            let detail = details(a);
            for top in &file.items {
                let TopLevel::Item(it) = top else { continue };
                if !(it.public || same_package) {
                    continue;
                }
                let one = |name: &ast_ident, kind: CompletionItemKind| {
                    deprecated_if(
                        item(&name.name, kind, detail.get(&name.span).cloned(), "0"),
                        a,
                        name.span,
                    )
                };
                match &it.kind {
                    ItemKind::Fn(f) => out.push(one(&f.name, CompletionItemKind::FUNCTION)),
                    ItemKind::Struct(s) => out.push(one(&s.name, CompletionItemKind::STRUCT)),
                    ItemKind::Enum(e) => out.push(one(&e.name, CompletionItemKind::ENUM)),
                    ItemKind::Interface(x) => {
                        out.push(one(&x.name, CompletionItemKind::INTERFACE));
                    }
                    ItemKind::TypeAlias(t) => out.push(one(&t.name, CompletionItemKind::CLASS)),
                    ItemKind::Binding(b) => {
                        let mut names = Vec::new();
                        crate::symbols::pattern_names(&b.pat, &mut names);
                        if let colibri_syntax::ast::PatKind::Ident(id) = &b.pat.kind {
                            names = vec![(id.name.clone(), id.span)];
                        }
                        for (name, span) in names {
                            out.push(item(
                                &name,
                                CompletionItemKind::VARIABLE,
                                detail.get(&span).cloned(),
                                "0",
                            ));
                        }
                    }
                    ItemKind::Extern(block) => {
                        for m in &block.items {
                            if let ItemKind::Fn(f) = &m.kind {
                                out.push(one(&f.name, CompletionItemKind::FUNCTION));
                            }
                        }
                    }
                    ItemKind::Impl(_)
                    | ItemKind::Import(_)
                    | ItemKind::Test(_)
                    | ItemKind::Error => {}
                }
            }
        }
        ModRef::Host(name) => {
            for file in env.analysis.host.module_syntax(name) {
                host_items(file, &mut out);
            }
        }
    }
    if types_only {
        out.retain(|i| {
            matches!(
                i.kind,
                Some(
                    CompletionItemKind::STRUCT
                        | CompletionItemKind::ENUM
                        | CompletionItemKind::INTERFACE
                        | CompletionItemKind::CLASS
                )
            )
        });
    }
    out
}

/// What `module.Type.|` offers: the variants of an enum and the associated functions of a
/// type that `module` (a name in scope) declares or the host provides.
fn qualified_type_items(
    env: &Env<'_>,
    res: &crate::resolve::Resolution,
    module: &str,
    name: &str,
) -> Vec<CompletionItem> {
    let Some(target) = res
        .module_scope
        .iter()
        .find(|(n, k, _)| n == module && *k == DefKind::Module)
        .and_then(|(_, _, s)| res.module_refs.get(s))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match target {
        ModRef::Source(i) => {
            let a = env.analysis;
            let Some(file) = a.program.files.get(*i) else {
                return out;
            };
            let same_package = env
                .package()
                .is_some_and(|p| a.program.metas.get(*i).is_some_and(|m| m.package == p));
            let visible = file.items.iter().any(|top| {
                let TopLevel::Item(it) = top else {
                    return false;
                };
                let declared = match &it.kind {
                    ItemKind::Struct(s) => s.name.name == name,
                    ItemKind::Enum(e) => e.name.name == name,
                    ItemKind::Interface(x) => x.name.name == name,
                    ItemKind::TypeAlias(t) => t.name.name == name,
                    _ => false,
                };
                declared && (it.public || same_package)
            });
            let detail = details(a);
            let found = res
                .defs
                .iter()
                .filter(|(span, d)| {
                    d.kind == DefKind::Type
                        && d.module == *i
                        && d.name == name
                        && res.type_members.contains_key(*span)
                })
                .min_by_key(|(span, _)| (span.file, span.start));
            if let (true, Some((def, _))) = (visible, found) {
                for m in &res.type_members[def] {
                    let keep = match m.kind {
                        DefKind::Variant => true,
                        DefKind::Method => !m.receiver && (m.public || same_package),
                        _ => false,
                    };
                    if keep {
                        out.push(item(
                            &m.name,
                            def_kind(m.kind),
                            detail.get(&m.span).cloned(),
                            "0",
                        ));
                    }
                }
            }
        }
        ModRef::Host(host) => {
            for file in env.analysis.host.module_syntax(host) {
                for top in &file.items {
                    let TopLevel::Item(it) = top else { continue };
                    // The functions a host class has without a receiver (`json.Value.str`).
                    if let ItemKind::Impl(imp) = &it.kind
                        && imp.interface.is_none()
                        && matches!(&imp.target.kind, colibri_syntax::ast::TypeKind::Named { path, .. }
                            if path.len() == 1 && path[0].name == name)
                    {
                        for m in &imp.items {
                            if let ItemKind::Fn(f) = &m.kind
                                && f.receiver.is_none()
                            {
                                out.push(item(
                                    &f.name.name,
                                    CompletionItemKind::FUNCTION,
                                    Some(format!("{module}.{name}.{}", f.name.name)),
                                    "0",
                                ));
                            }
                        }
                    }
                    if let ItemKind::Enum(e) = &it.kind
                        && e.name.name == name
                    {
                        for v in &e.variants {
                            out.push(item(
                                &v.name.name,
                                CompletionItemKind::ENUM_MEMBER,
                                Some(format!("{module}.{name}.{}", v.name.name)),
                                "0",
                            ));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Whether a type is written at `at` (not an expression): after `->`, or after the `:` that
/// directly follows the name of a `let`, `var` or `const`, of a parameter of a `fn`, or of a
/// field of a `struct` or `class`. Decided from the syntax around the cursor: the name before
/// the colon and the bracket that is still open, never from the line's first word (so a
/// `:` inside a struct literal, a call's argument label or a map literal is an expression).
fn in_type_position(text: &str, at: usize) -> bool {
    let before = text[..at].trim_end();
    if before.ends_with("->") {
        return true;
    }
    if !before.ends_with(':') || before.ends_with("::") {
        return false;
    }
    let head = before[..before.len() - 1].trim_end();
    let name_start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(head.len(), |(i, _)| i);
    let name = &head[name_start..];
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    let pre = head[..name_start].trim_end();
    if pre.ends_with('.') {
        return false;
    }
    // `let name:`, `var name:`, `const name:` (after an optional `pub`).
    let last_word = pre
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or("", |(i, _)| &pre[i..]);
    if ["let", "var", "const"].contains(&last_word) {
        return true;
    }
    // The nearest bracket that is still open decides: `fn name(` or `struct Name {`.
    let mut depth = [0usize; 3];
    let bytes = before.as_bytes();
    for i in (0..bytes.len()).rev() {
        let (open, close) = match bytes[i] {
            b')' => (None, Some(0)),
            b']' => (None, Some(1)),
            b'}' => (None, Some(2)),
            b'(' => (Some(0), None),
            b'[' => (Some(1), None),
            b'{' => (Some(2), None),
            _ => continue,
        };
        if let Some(k) = close {
            depth[k] += 1;
        } else if let Some(k) = open {
            if depth[k] > 0 {
                depth[k] -= 1;
                continue;
            }
            if k == 1 {
                continue;
            }
            let head = before[..i].trim_end();
            // Skip the generics of `fn f[T](`.
            let head = if k == 0 && head.ends_with(']') {
                head.rfind('[').map_or(head, |j| head[..j].trim_end())
            } else {
                head
            };
            return match k {
                0 => {
                    // A parameter name follows `(` or `,`.
                    let words: Vec<&str> = head.split_whitespace().rev().take(3).collect();
                    words.get(1) == Some(&"fn") && (pre.ends_with('(') || pre.ends_with(','))
                }
                _ => {
                    // `struct Name {` or `class Name[T] {`.
                    let line_start = head.rfind('\n').map_or(0, |n| n + 1);
                    let header = head[line_start..].trim_start();
                    let header = header.strip_prefix("pub ").unwrap_or(header);
                    header.starts_with("struct ") || header.starts_with("class ")
                }
            };
        }
    }
    false
}

fn host_items(file: &colibri_syntax::ast::File, out: &mut Vec<CompletionItem>) {
    for top in &file.items {
        let TopLevel::Item(it) = top else { continue };
        let (name, kind) = match &it.kind {
            ItemKind::Fn(f) => (&f.name, CompletionItemKind::FUNCTION),
            ItemKind::Struct(s) => (&s.name, CompletionItemKind::STRUCT),
            ItemKind::Enum(e) => (&e.name, CompletionItemKind::ENUM),
            ItemKind::Interface(i) => (&i.name, CompletionItemKind::INTERFACE),
            ItemKind::TypeAlias(t) => (&t.name, CompletionItemKind::CLASS),
            ItemKind::Binding(b) => match &b.pat.kind {
                colibri_syntax::ast::PatKind::Ident(id) => (id, CompletionItemKind::VARIABLE),
                _ => continue,
            },
            _ => continue,
        };
        out.push(item(&name.name, kind, None, "0"));
    }
}

/// The scrutinee of the `match` whose braces enclose `at`, as a byte range, when `at` is
/// where an arm starts: right after the opening brace or after a finished arm, and not in an
/// arm's body.
fn match_scrutinee(text: &str, at: usize) -> Option<(usize, usize)> {
    let before = text[..at].trim_end();
    let last = before.chars().next_back()?;
    if !(is_word(last) || "{}),]\"?".contains(last)) {
        return None;
    }
    // The nearest brace that is still open.
    let chars: Vec<(usize, char)> = before.char_indices().collect();
    let mut depth = 0usize;
    let mut open = None;
    let mut i = chars.len();
    while i > 0 {
        i -= 1;
        match chars[i].1 {
            '}' => depth += 1,
            '{' if depth == 0 => {
                open = Some(chars[i].0);
                break;
            }
            '{' => depth -= 1,
            '"' => {
                while i > 0 {
                    i -= 1;
                    if chars[i].1 == '"' && (i == 0 || chars[i - 1].1 != '\\') {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    let open = open?;
    let head = &text[..open];
    let kw = head.rmatch_indices("match").find(|(i, _)| {
        let before_ok = head[..*i].chars().next_back().is_none_or(|c| !is_word(c));
        let after_ok = head[i + 5..].chars().next().is_none_or(|c| !is_word(c));
        before_ok && after_ok
    })?;
    let scrutinee = &head[kw.0 + 5..];
    if scrutinee.contains(['{', '}']) || scrutinee.trim().is_empty() {
        return None;
    }
    let start = kw.0 + 5 + (scrutinee.len() - scrutinee.trim_start().len());
    Some((start, kw.0 + 5 + scrutinee.trim_end().len()))
}

/// The enum variants a `match` arm can start with, when the cursor is where one starts.
fn arm_variants(env: &Env<'_>, word_start: usize) -> Vec<CompletionItem> {
    let Some(range) = match_scrutinee(env.text, word_start) else {
        return Vec::new();
    };
    let Some(Typed { analysis, ty, .. }) = env.typed(range) else {
        return Vec::new();
    };
    let name = ty.trim_end_matches('?');
    let name = name.split('[').next().unwrap_or(name);
    let res = resolve(analysis, None);
    let detail = details(analysis);
    let Some((&def, _)) = res
        .defs
        .iter()
        .filter(|(span, d)| {
            d.kind == DefKind::Type && d.name == name && res.type_members.contains_key(*span)
        })
        .min_by_key(|(span, _)| (span.file, span.start))
    else {
        return Vec::new();
    };
    res.type_members[&def]
        .iter()
        .filter(|v| v.kind == DefKind::Variant)
        .map(|v| {
            let shown = detail
                .get(&v.span)
                .cloned()
                .unwrap_or_else(|| format!("{name}.{}", v.name));
            item(&v.name, CompletionItemKind::ENUM_MEMBER, Some(shown), "0")
        })
        .collect()
}

/// Completion of a name, or of a variant after a bare dot.
pub fn ident_items(env: &Env<'_>, offset: usize, ctx: &Ctx) -> Vec<CompletionItem> {
    let Some(file) = env.file() else {
        return Vec::new();
    };
    let res = resolve(env.analysis, Some((file, offset as u32)));
    // After a bare dot the arm starts at the dot.
    let arm_at = if ctx.kind == Kind::DotVariant {
        ctx.word.0.saturating_sub(1)
    } else {
        ctx.word.0
    };
    let variants = arm_variants(env, arm_at);
    if ctx.kind == Kind::DotVariant {
        return variants;
    }
    let mut out = variants;
    let detail = details(env.analysis);
    let mut seen = std::collections::HashSet::new();
    for l in &res.scope {
        if seen.insert(l.name.clone()) {
            out.push(item(
                &l.name,
                def_kind(l.kind),
                detail.get(&l.span).cloned(),
                "1",
            ));
        }
    }
    for (name, kind, span) in &res.module_scope {
        if seen.insert(name.clone()) {
            out.push(deprecated_if(
                item(name, def_kind(*kind), detail.get(span).cloned(), "2"),
                env.analysis,
                *span,
            ));
        }
    }
    let prelude = prelude_names();
    for (name, kind) in &prelude {
        if seen.insert(name.clone()) {
            let k = match kind {
                PreludeKind::Function => CompletionItemKind::FUNCTION,
                PreludeKind::Type => CompletionItemKind::CLASS,
                PreludeKind::Interface => CompletionItemKind::INTERFACE,
            };
            out.push(item(name, k, None, "3"));
        }
    }
    let mut host = Vec::new();
    for file in env
        .analysis
        .host
        .module_syntax(colibri_check::host::DEFAULT_MODULE)
    {
        host_items(file, &mut host);
    }
    for mut it in host {
        if seen.insert(it.label.clone()) {
            it.sort_text = Some(format!("3{}", it.label));
            out.push(it);
        }
    }
    for kw in TokenKind::keywords() {
        if seen.insert(kw.to_string()) {
            out.push(item(kw, CompletionItemKind::KEYWORD, None, "4"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_at(text: &str) -> Option<Ctx> {
        let at = text.find('|').unwrap();
        let clean = text.replace('|', "");
        context(&clean, at)
    }

    #[test]
    fn the_cursor_after_a_dot_has_a_receiver() {
        let text = "let a = xs.map(f).fi|";
        let ctx = ctx_at(text).unwrap();
        let clean = text.replace('|', "");
        let Kind::Member { receiver, .. } = ctx.kind else {
            panic!("{ctx:?}")
        };
        assert_eq!(&clean[receiver.0..receiver.1], "xs.map(f)");
        assert_eq!(&clean[ctx.word.0..ctx.word.1], "fi");

        for (src, want) in [
            ("a.|", "a"),
            ("f(a, b.c[0]?.|)", "b.c[0]?"),
            ("(x + y).|", "(x + y)"),
            ("\"s\".|", "\"s\""),
            ("xs\n    .map(f)\n    .|", "xs\n    .map(f)"),
            ("p.pos.x.|", "p.pos.x"),
        ] {
            let clean = src.replace('|', "");
            let Kind::Member { receiver, .. } = ctx_at(src).unwrap().kind else {
                panic!("{src}")
            };
            assert_eq!(&clean[receiver.0..receiver.1], want, "{src}");
        }
    }

    #[test]
    fn places_without_a_receiver_or_completion() {
        assert_eq!(ctx_at("let a = .|").unwrap().kind, Kind::DotVariant);
        assert_eq!(ctx_at("match s { .Ci| }").unwrap().kind, Kind::DotVariant);
        // At the start of a line a lower-case name continues the chain; a capitalised one is a variant.
        assert!(matches!(
            ctx_at("a\n    .m|").unwrap().kind,
            Kind::Member {
                line_start: true,
                ..
            }
        ));
        assert_eq!(ctx_at("a\n    .M|").unwrap().kind, Kind::DotVariant);
        assert_eq!(ctx_at("for i in 0..|").unwrap().kind, Kind::Ident);
        assert_eq!(ctx_at("let a = pri|").unwrap().kind, Kind::Ident);
        // A number, a string and comments offer nothing.
        assert!(ctx_at("let a = 1.|").is_none());
        assert!(ctx_at("let a = \"hel|lo\"").is_none());
        assert!(ctx_at("let a = 1 // hel|lo").is_none());
        assert!(ctx_at("/* a\n b| */ let a = 1").is_none());
        assert!(ctx_at("/// doc| text\nfn f() {}").is_none());
        // The end of a raw string that is finished is code again; an unfinished one is not.
        assert!(ctx_at("let a = r#\"x\"#|").is_some());
        assert!(ctx_at("let a = r\"x\"|").is_some());
        assert!(ctx_at("let a = \"\"\"x\"\"\"|").is_some());
        assert!(ctx_at("let a = r#\"x\"|").is_none());
        assert!(ctx_at("let a = \"x\\\"|").is_none());
        // The hole of an interpolation is code.
        assert!(ctx_at("let a = \"x {b.|}\"").is_some());
        assert!(ctx_at("let a = \"x {b|}\"").is_some());
        assert!(ctx_at("let a = \"x\" + b|").is_some());
    }

    #[test]
    fn repairing_takes_out_the_dot_and_the_word() {
        let text = "fn f() {\n    foo.ba\n}\n";
        let at = text.find("ba").unwrap() + 2;
        let ctx = context(text, at).unwrap();
        assert_eq!(repair(text, &ctx).unwrap(), "fn f() {\n    foo \n}\n");
        let text = "foo.|bar(1)";
        let ctx = ctx_at(text).unwrap();
        assert_eq!(repair(&text.replace('|', ""), &ctx).unwrap(), "foo (1)");
        assert!(repair("x", &context("x", 1).unwrap()).is_none());
    }

    #[test]
    fn a_range_moves_to_the_old_text_around_an_edit() {
        let old = "let a = b\nlet c = d.len()\nlet e = 1\n";
        let new = "let a = b\nlet c = d.len() \nlet e = 1\n";
        let at = new.find("d.len()").unwrap();
        // Before the edit and after it.
        assert_eq!(map_span(old, new, (at, at + 7)), Some((at, at + 7)));
        let e = new.find("e =").unwrap();
        assert_eq!(map_span(old, new, (e, e + 1)), Some((e - 1, e)));
        // Over the edit itself.
        assert_eq!(map_span(old, new, (at, at + 8)), None);
    }
}
