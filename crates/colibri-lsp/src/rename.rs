//! References and rename, over the occurrences that [`crate::resolve`] finds.
//!
//! A rename edits identifiers only: every edit replaces the span of a name that the syntax
//! tree holds, and the text there is checked to be the old name, so strings, comments and
//! other text never change. It is refused for names that are not valid identifiers or are
//! keywords, for things the program does not declare or declares in a dependency, and where the
//! checker did not resolve a name that could be the one renamed (the program has errors
//! there). The server then checks the result: the renamed program must resolve every name to
//! the declaration it resolved to before ([`compare`]), which catches a new name that clashes
//! with another one in scope.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use colibri_syntax::{FileId, Span, TokenKind, lex};

use crate::analysis::{Analysis, Spec};
use crate::resolve::{DefInfo, DefKind, Occ, Resolution, Target};

/// The occurrence a cursor at `offset` is on. Where two names share their text (`P { x }`
/// names a field and a variable), the variable.
pub fn occ_at(res: &Resolution, file: FileId, offset: u32) -> Option<usize> {
    let is_field = |o: &Occ| {
        matches!(&o.target, Target::Def(d)
            if res.defs.get(d).is_some_and(|i| i.kind == DefKind::Field))
    };
    let all: Vec<usize> = (0..res.occs.len())
        .filter(|&i| {
            let s = res.occs[i].span;
            s.file == file && s.start <= offset && offset <= s.end
        })
        .collect();
    all.iter()
        .copied()
        .filter(|&i| !(res.occs[i].shorthand && is_field(&res.occs[i])))
        .min_by_key(|&i| res.occs[i].span.len())
        .or_else(|| all.first().copied())
}

/// Whether `name` can be written as a name: one identifier token, not a keyword.
fn check_identifier(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the new name is empty".to_string());
    }
    if TokenKind::keyword(name).is_some() {
        return Err(format!("`{name}` is a keyword, not a name"));
    }
    let (tokens, diags) = lex(FileId(0), name);
    let one_ident = diags.is_empty()
        && tokens.len() == 2
        && matches!(&tokens[0].kind, TokenKind::Ident(n) if n == name)
        && matches!(tokens[1].kind, TokenKind::Eof);
    if one_ident {
        Ok(())
    } else {
        Err(format!("`{name}` is not a valid name"))
    }
}

/// Checks the new name against the kind of thing renamed. A capitalised name in a pattern is
/// a variant or a constant (DESIGN §8.2), so a variable cannot become one.
pub fn check_new_name(info: &DefInfo, new: &str) -> Result<(), String> {
    check_identifier(new)?;
    let capitalised = |s: &str| s.chars().next().is_some_and(char::is_uppercase);
    match info.kind {
        DefKind::Local | DefKind::Param if capitalised(new) => Err(format!(
            "`{new}` starts with a capital letter: in a pattern that names a variant or a constant, not a variable"
        )),
        DefKind::Type | DefKind::Variant if !capitalised(new) => Err(format!(
            "`{new}` must start with a capital letter, like other types and variants"
        )),
        DefKind::Global if capitalised(&info.name) != capitalised(new) => Err(format!(
            "`{new}` would change whether the name starts with a capital letter, which patterns read as a constant"
        )),
        _ => Ok(()),
    }
}

/// What a rename is about to do.
pub struct Plan<'a> {
    pub info: &'a DefInfo,
    /// The occurrences (indices into [`Resolution::occs`]) of what is renamed.
    pub occs: Vec<usize>,
}

/// Why the thing the occurrence names cannot be renamed, or what renaming it would touch.
///
/// A module of a dependency cannot be renamed, unless the file is under one of the `editable`
/// folders (a package of the workspace that the edited package is a path dependency of).
/// With a workspace folder `root`, an item that other packages can name is renamed only if
/// its file is inside the folder: the packages that depend on it are looked for there, and
/// a rename does not edit what it did not look for. (The server also refuses any rename that
/// would edit a file whose canonical path is outside the folder, however the file is named or
/// reached: a link inside the folder to a folder outside it does not make that one inside.)
pub fn plan<'a>(
    analysis: &Analysis,
    res: &'a Resolution,
    occ: usize,
    editable: &[PathBuf],
    root: Option<&Path>,
) -> Result<Plan<'a>, String> {
    let target = res.occs[occ].target.clone();
    let def = match &target {
        Target::Def(d) => *d,
        Target::External(name) => {
            let name = name.rsplit('.').next().unwrap_or(name);
            return Err(format!(
                "`{name}` is not declared in this package (it comes from the prelude, the standard library or a dependency)"
            ));
        }
    };
    let Some(info) = res.defs.get(&def) else {
        return Err("this name has no declaration to rename".to_string());
    };
    if let Some(reason) = info.fixed {
        return Err(format!("`{}` cannot be renamed: {reason}", info.name));
    }
    let dependency = analysis
        .program
        .metas
        .get(info.module)
        .is_none_or(|m| m.dependency)
        && !analysis.program.files.get(info.module).is_some_and(|f| {
            let path = path_of(analysis, f.span.file);
            editable.iter().any(|dir| path.starts_with(dir))
        });
    if dependency {
        return Err(format!(
            "`{}` is declared in a dependency, outside this package",
            info.name
        ));
    }
    if info.visible
        && let Some(root) = root
        && let Some(f) = analysis.program.files.get(info.module)
    {
        let file = path_of(analysis, f.span.file);
        if !file.starts_with(root) {
            return Err(declared_outside_message(
                &info.name,
                &shown(None, &file),
                &shown(None, root),
            ));
        }
    }
    // Names that only a type (or a callee) can resolve: refuse where that did not work.
    let type_directed = matches!(
        info.kind,
        DefKind::Field | DefKind::Method | DefKind::Variant | DefKind::Param
    );
    if type_directed {
        let unresolved = res
            .unresolved
            .iter()
            .filter(|(_, name)| *name == info.name)
            .count();
        if unresolved > 0 {
            return Err(format!(
                "{unresolved} use{} of a `{}` could not be resolved (the program has errors there): fix them first, or the rename could miss them",
                if unresolved == 1 { "" } else { "s" },
                info.name
            ));
        }
    }
    let occs = (0..res.occs.len())
        .filter(|&i| res.occs[i].target == target)
        .collect();
    Ok(Plan { info, occs })
}

/// One replacement of the source text.
#[derive(Clone, Debug)]
pub struct Edit {
    pub span: Span,
    pub text: String,
    /// Where each occurrence the edit covers is in the replacement: its index, offset and
    /// length.
    pub pieces: Vec<(usize, usize, usize)>,
}

/// The edits that rename the planned occurrences to `new`. The text at each is checked to be
/// the old name.
pub fn edits(
    analysis: &Analysis,
    res: &Resolution,
    plan: &Plan<'_>,
    new: &str,
) -> Result<Vec<Edit>, String> {
    let old = &plan.info.name;
    let mut out: Vec<Edit> = Vec::new();
    for &i in &plan.occs {
        let o = &res.occs[i];
        let sources = &analysis.sources;
        let text_here = sources.contains(o.span.file).then(|| {
            sources
                .text(o.span.file)
                .get(o.span.start as usize..o.span.end as usize)
        });
        if text_here != Some(Some(old.as_str())) {
            return Err(format!(
                "the text at one use of `{old}` is not the name: not renamed"
            ));
        }
        if o.shorthand {
            // `P { x }` stands for `P { x: x }`: spell out the name that is not renamed.
            let twin = (0..res.occs.len())
                .find(|&t| res.occs[t].span == o.span && res.occs[t].target != o.target);
            let renamed_is_field = matches!(&o.target, Target::Def(d)
                if res.defs.get(d).is_some_and(|i| i.kind == DefKind::Field));
            let mut pieces = Vec::new();
            let text = if renamed_is_field {
                pieces.push((i, 0, new.len()));
                pieces.extend(twin.map(|t| (t, new.len() + 2, old.len())));
                format!("{new}: {old}")
            } else {
                pieces.extend(twin.map(|t| (t, 0, old.len())));
                pieces.push((i, old.len() + 2, new.len()));
                format!("{old}: {new}")
            };
            out.push(Edit {
                span: o.span,
                text,
                pieces,
            });
        } else {
            out.push(Edit {
                span: o.span,
                text: new.to_string(),
                pieces: vec![(i, 0, new.len())],
            });
        }
    }
    out.sort_by_key(|e| (e.span.file.0, e.span.start));
    out.dedup_by_key(|e| e.span);
    Ok(out)
}

/// Where the spans of the old text are once the edits are applied.
pub struct Shift {
    /// By file path: the old range of each edit and its change in length.
    by_file: HashMap<PathBuf, Vec<(u32, u32, i64)>>,
    /// The new range of the occurrences the edits cover, by occurrence.
    pieces: HashMap<usize, (u32, u32)>,
}

/// The total change in length of the edits that end at or before `at`.
fn shift_before(edits: &[(u32, u32, i64)], at: u32) -> i64 {
    edits
        .iter()
        .filter(|(_, end, _)| *end <= at)
        .map(|(_, _, d)| d)
        .sum()
}

pub fn path_of(analysis: &Analysis, file: FileId) -> PathBuf {
    analysis.path_of(file)
}

impl Shift {
    pub fn new(analysis: &Analysis, edits: &[Edit]) -> Shift {
        let mut by_file: HashMap<PathBuf, Vec<(u32, u32, i64)>> = HashMap::new();
        for e in edits {
            let delta = e.text.len() as i64 - i64::from(e.span.end - e.span.start);
            by_file
                .entry(path_of(analysis, e.span.file))
                .or_default()
                .push((e.span.start, e.span.end, delta));
        }
        let mut pieces = HashMap::new();
        for e in edits {
            let list = &by_file[&path_of(analysis, e.span.file)];
            let start = (i64::from(e.span.start) + shift_before(list, e.span.start)) as u32;
            for &(occ, rel, len) in &e.pieces {
                let at = start + rel as u32;
                pieces.insert(occ, (at, at + len as u32));
            }
        }
        Shift { by_file, pieces }
    }

    /// The new range of occurrence `i`, which is at `span`.
    pub fn occ(&self, analysis: &Analysis, i: usize, span: Span) -> (u32, u32) {
        if let Some(&r) = self.pieces.get(&i) {
            return r;
        }
        let path = path_of(analysis, span.file);
        let edits = self.by_file.get(&path).map_or(&[][..], Vec::as_slice);
        let start = (i64::from(span.start) + shift_before(edits, span.start)) as u32;
        (start, start + span.len())
    }
}

/// What a name stands for, independent of the analysis it came from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Meaning {
    Def(PathBuf, u32, u32),
    External(String),
}

/// What the target of an occurrence stands for, in terms that hold across analyses of the
/// same texts.
pub fn meaning(analysis: &Analysis, target: &Target) -> Meaning {
    match target {
        Target::Def(d) => Meaning::Def(path_of(analysis, d.file), d.start, d.end),
        Target::External(n) => Meaning::External(n.clone()),
    }
}

/// The first occurrence that stands for `what`, if the analysis has one.
pub fn occ_of_meaning(analysis: &Analysis, res: &Resolution, what: &Meaning) -> Option<usize> {
    res.occs
        .iter()
        .position(|o| meaning(analysis, &o.target) == *what)
}

/// Checks that the program after the edits resolves every name to the declaration it had
/// before. `before` and `after` are the program and its resolution, before and after the
/// edits. Returns what went wrong.
pub fn compare(
    before: (&Analysis, &Resolution),
    after: (&Analysis, &Resolution),
    shift: &Shift,
    root: Option<&Path>,
) -> Result<(), String> {
    let (a0, r0) = before;
    let (a1, r1) = after;
    let mut now: HashMap<(PathBuf, u32, u32), Vec<Meaning>> = HashMap::new();
    for o in &r1.occs {
        let meaning = match &o.target {
            Target::Def(d) => Meaning::Def(path_of(a1, d.file), d.start, d.end),
            Target::External(n) => Meaning::External(n.clone()),
        };
        now.entry((path_of(a1, o.span.file), o.span.start, o.span.end))
            .or_default()
            .push(meaning);
    }
    // Where each declaration's own identifier is afterwards.
    let mut decl_at: HashMap<Span, usize> = HashMap::new();
    for (i, o) in r0.occs.iter().enumerate() {
        if let Target::Def(d) = &o.target
            && o.decl
            && o.span == *d
        {
            decl_at.entry(*d).or_insert(i);
        }
    }
    let mut wrong = 0usize;
    let mut first: Option<PathBuf> = None;
    for (i, o) in r0.occs.iter().enumerate() {
        let (s, e) = shift.occ(a0, i, o.span);
        let path = path_of(a0, o.span.file);
        let expected = match &o.target {
            Target::Def(d) => decl_at.get(d).map(|&k| {
                let (ds, de) = shift.occ(a0, k, *d);
                Meaning::Def(path_of(a0, d.file), ds, de)
            }),
            Target::External(n) => Some(Meaning::External(n.clone())),
        };
        let ok = expected.as_ref().is_some_and(|m| {
            now.get(&(path.clone(), s, e))
                .is_some_and(|got| got.contains(m))
        });
        if !ok {
            wrong += 1;
            first.get_or_insert(path);
        }
    }
    if wrong == 0 && r0.occs.len() == r1.occs.len() {
        return Ok(());
    }
    Err(clash_message(wrong, first.map(|p| shown(root, &p))))
}

/// The message for a rename that would edit `outside`, a file outside the open folder `root`
/// (both as [`shown`] writes them).
pub fn outside_folder_message(outside: &str, root: &str) -> String {
    format!(
        "renaming would edit `{outside}`, which is outside the open folder (`{root}`): not renamed"
    )
}

/// The message for an item of `file` that packages outside the open folder `root` can use.
fn declared_outside_message(name: &str, file: &str, root: &str) -> String {
    format!(
        "`{name}` is declared in `{file}`, which belongs to a package outside the open folder (`{root}`), and packages that use it cannot all be found from here: open the folder that holds it to rename it"
    )
}

/// The message for a rename whose new name clashes, `first` being the first place it did.
fn clash_message(wrong: usize, first: Option<String>) -> String {
    let place = first.map_or(String::new(), |p| format!(", first in {p}"));
    format!(
        "the new name would change what {} name{} mean{place}: it clashes with another name in scope, or the program no longer reads as before",
        wrong.max(1),
        if wrong == 1 { "" } else { "s" },
    )
}

/// The message for a file that has no address to edit it by.
pub fn no_address_message(file: &str) -> String {
    format!("cannot address `{file}` to edit it: not renamed")
}

/// `text` with each whole identifier equal to `old` replaced by `new` (an identifier is a
/// maximal run of letters, digits and `_`), so that renaming `x` leaves `expected` alone.
pub fn replace_ident(text: &str, old: &str, new: &str) -> String {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word == old {
            out.push_str(new);
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in text.chars() {
        if is_word(c) {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The errors of a program as `(file, message)`, each message with `old` replaced by `new`
/// as whole identifiers when given.
pub fn error_list(analysis: &Analysis, rename: Option<(&str, &str)>) -> Vec<(PathBuf, String)> {
    analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == colibri_syntax::Severity::Error)
        .map(|d| {
            let message = match rename {
                Some((old, new)) => replace_ident(&d.message, old, new),
                None => d.message.clone(),
            };
            (path_of(analysis, d.span.file), message)
        })
        .collect()
}

/// The first error of `after` that `before` did not already have (the names that were
/// renamed read as the new ones), comparing the errors of each file as a multiset.
pub fn added_error(
    before: &Analysis,
    after: &Analysis,
    old: &str,
    new: &str,
) -> Option<(PathBuf, String)> {
    let mut had: HashMap<(PathBuf, String), usize> = HashMap::new();
    for e in error_list(before, Some((old, new))) {
        *had.entry(e).or_default() += 1;
    }
    for e in error_list(after, None) {
        match had.get_mut(&e) {
            Some(n) if *n > 0 => *n -= 1,
            _ => return Some(e),
        }
    }
    None
}

/// A path for a message: relative to `root` when it is under it, written with `/` on every
/// system (the messages tell the user to run `colibri run tools/t.colibri`, which Windows
/// takes as well as `\`).
pub fn shown(root: Option<&Path>, path: &Path) -> String {
    shown_as(root, path, cfg!(windows))
}

/// [`shown`] for a system where `\` separates (`windows`) or does not.
fn shown_as(root: Option<&Path>, path: &Path, windows: bool) -> String {
    let rel = root.and_then(|r| path.strip_prefix(r).ok()).unwrap_or(path);
    if rel.as_os_str().is_empty() {
        ".".to_string()
    } else {
        with_slashes(&rel.to_string_lossy(), windows)
    }
}

/// `text` with `/` for the separators of a Windows path (on Unix a `\\` is part of a name).
fn with_slashes(text: &str, windows: bool) -> String {
    if windows {
        text.replace('\\', "/")
    } else {
        text.to_string()
    }
}

/// How a reading of the program is told to the user.
pub fn describe(spec: &Spec, root: Option<&Path>) -> String {
    match spec {
        Spec::Package(d) => format!("as a module of the package in `{}`", shown(root, d)),
        Spec::File(f) => format!(
            "as a script (`colibri run {}`, imports read from `{}`)",
            shown(root, f),
            shown(root, f.parent().unwrap_or(f))
        ),
    }
}

/// Looks for a use of `what` that means something else in another reading of the same file.
/// A rename edits the text, which every reading shares, so it cannot be right when the use
/// is `what` in one reading and another declaration, or nothing, in another reading that
/// loaded (the checker ran: errors elsewhere in it do not matter). Returns the message that says so.
pub fn conflicting_reading(
    parts: &[(&Analysis, &Resolution)],
    what: &Meaning,
    name: &str,
    root: Option<&Path>,
) -> Option<String> {
    type Site = (PathBuf, u32, u32);
    let index: Vec<HashMap<Site, Vec<&Target>>> = parts
        .iter()
        .map(|(a, r)| {
            let mut m: HashMap<Site, Vec<&Target>> = HashMap::new();
            for o in &r.occs {
                m.entry((path_of(a, o.span.file), o.span.start, o.span.end))
                    .or_default()
                    .push(&o.target);
            }
            m
        })
        .collect();
    // The readings whose program loaded hold each file: with a load error the checker did not
    // run and nothing is resolved (a stated limit); otherwise every use it resolved counts,
    // whether or not the program has errors elsewhere.
    let mut holders: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (j, (b, _)) in parts.iter().enumerate() {
        if b.program.has_errors() {
            continue;
        }
        for (_, path) in b.files() {
            holders.entry(path).or_default().push(j);
        }
    }
    for (i, (a, r)) in parts.iter().enumerate() {
        for o in r.occs.iter().filter(|o| meaning(a, &o.target) == *what) {
            let site = (path_of(a, o.span.file), o.span.start, o.span.end);
            for &j in holders.get(&site.0).into_iter().flatten() {
                let (b, rb) = parts[j];
                if i == j {
                    continue;
                }
                let here = index[j].get(&site).map_or(&[][..], Vec::as_slice);
                if here.iter().any(|t| meaning(b, t) == *what) {
                    continue;
                }
                let line = a.sources.text(o.span.file)[..o.span.start as usize]
                    .matches('\n')
                    .count()
                    + 1;
                let decl = |m: &Meaning| match m {
                    Meaning::Def(p, ..) => format!("declared in `{}`", shown(root, p)),
                    Meaning::External(n) => format!("`{n}`"),
                };
                let other = match here.iter().find_map(|t| match t {
                    Target::Def(d) => rb.defs.get(d).map(|info| (info, *d)),
                    Target::External(_) => None,
                }) {
                    Some((info, d)) => format!(
                        "`{}` {}",
                        info.name,
                        decl(&Meaning::Def(path_of(b, d.file), d.start, d.end))
                    ),
                    None => "nothing that resolves".to_string(),
                };
                return Some(format!(
                    "cannot rename `{name}`: `{}` (line {line}) means different things depending on how it is run. Read {}, the name is `{name}` {}; read {}, it is {other}. A rename cannot be right for both",
                    shown(root, &site.0),
                    describe(&a.spec, root),
                    decl(what),
                    describe(&b.spec, root),
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        clash_message, declared_outside_message, no_address_message, outside_folder_message,
        replace_ident, shown, shown_as, with_slashes,
    };
    use std::path::Path;

    #[test]
    fn messages_show_paths_with_slashes_on_windows_too() {
        assert_eq!(with_slashes(r"tools\t.colibri", true), "tools/t.colibri");
        assert_eq!(
            with_slashes(r"C:\ws\tools\t.colibri", true),
            "C:/ws/tools/t.colibri"
        );
        assert_eq!(with_slashes("tools/t.colibri", false), "tools/t.colibri");
        let root = Path::new("/ws");
        assert_eq!(
            shown(Some(root), &root.join("tools").join("t.colibri")),
            "tools/t.colibri"
        );
    }

    #[test]
    fn every_rename_message_shows_a_windows_path_with_slashes() {
        // The paths a Windows system hands over, built into each message the way the server
        // does: a `\` anywhere in one would show.
        let root = Path::new(r"C:\ws");
        let outside = Path::new(r"D:\other\out\x.colibri");
        let inside = Path::new(r"C:\ws\sub\y.colibri");
        let s = |p: &Path, r: Option<&Path>| shown_as(r, p, true);
        let messages = [
            outside_folder_message(&s(outside, None), &s(root, None)),
            declared_outside_message("f", &s(outside, None), &s(root, None)),
            no_address_message(&s(inside, Some(root))),
            clash_message(2, Some(s(inside, Some(root)))),
            clash_message(1, Some(s(outside, Some(root)))),
        ];
        for m in &messages {
            assert!(!m.contains('\\'), "{m}");
        }
        assert!(messages[0].contains("`D:/other/out/x.colibri`"));
        assert!(messages[0].contains("`C:/ws`"));
        assert!(messages[3].contains("sub/y.colibri"), "{}", messages[3]);
    }

    #[test]
    fn identifiers_are_replaced_whole() {
        assert_eq!(
            replace_ident("expected `x`, found x_1 and xx", "x", "y"),
            "expected `y`, found x_1 and xx"
        );
        assert_eq!(replace_ident("util.f(f)", "f", "h"), "util.h(h)");
        assert_eq!(replace_ident("é x é", "x", "yy"), "é yy é");
    }
}
