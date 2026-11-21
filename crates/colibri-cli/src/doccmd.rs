//! `colibri doc --std DIR`: writes the standard library reference as Markdown, one page for
//! each module and an index, from what the toolchain itself knows about the modules.
//!
//! The modules written in Rust are described by their interface (`.cbi`) text, the same text
//! the checker reads, so a signature on a page can never differ from what a program is checked
//! against. The modules written in Colibri (`vec`, `httpd`) are parsed from their source, and
//! only their `pub` items are shown. A `///` comment before an item is its documentation and
//! lands on the page; the interface text has no comments yet, so the pages of the modules
//! written in Rust show signatures only, next to a one-line summary kept in [`SUMMARIES`].
//!
//! [`std_pages`] is the whole generator and has no side effects. The `guide` tests compare
//! it with the committed pages, so a change to a signature fails the build until the pages are
//! regenerated.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use colibri_syntax::ast::{FnBody, Item, ItemKind, StructDecl, TopLevel, TypeKind};
use colibri_syntax::{Diagnostic, FileId, parse_file, parse_interface_file};

/// The name of the index page.
pub const INDEX: &str = "README.md";

/// What each module is for, in a sentence: the interface text carries no description, so this
/// table does. `std_pages` panics for a module that is missing here, so a new module cannot be
/// added without a line.
const SUMMARIES: &[(&str, &str)] = &[
    ("args", "Command-line argument parsing."),
    (
        "base64",
        "Base64 encoding and decoding of text and of `Bytes`.",
    ),
    (
        "csv",
        "CSV text to records or to rows with a header, and back.",
    ),
    ("datetime", "Dates, times and their formatting."),
    (
        "fs",
        "Files and folders: whole-file reads and writes, listing, metadata, and `File` handles that read or write piece by piece.",
    ),
    ("hash", "SHA-256 of text and of `Bytes`."),
    (
        "hex",
        "Hexadecimal encoding and decoding of text and of `Bytes`.",
    ),
    (
        "http",
        "A blocking HTTP client that does not block other coroutines.",
    ),
    (
        "httpd",
        "An HTTP/1.1 server written in Colibri on top of `net`: a router, request bodies, streamed responses, limits and timeouts, and a graceful stop.",
    ),
    ("io", "Standard input and output."),
    (
        "json",
        "JSON: `encode`, typed `decode[T]`, and `Value` for a document of unknown shape.",
    ),
    ("math", "Math functions and constants."),
    (
        "net",
        "TCP sockets over IPv4 and IPv6 that read and write lines, bytes and text without blocking other coroutines.",
    ),
    (
        "os",
        "Environment variables, the program's arguments and `exit`.",
    ),
    (
        "path",
        "Path manipulation. Paths are `/`-separated text on every system.",
    ),
    ("process", "Running subprocesses, with pipes."),
    ("rand", "Random numbers."),
    ("regex", "Regular expressions."),
    ("str", "String helpers and parsing."),
    ("time", "Clocks and sleeping."),
    (
        "toml",
        "`toml.parse` into a `json.Value`, and typed `toml.decode[T]`.",
    ),
    (
        "vec",
        "Vectors of 2, 3 and 4 components of `f32` and `i32`, written in Colibri on top of the operator interfaces.",
    ),
];

/// Where a module comes from, which decides what is shown of it.
enum Origin {
    /// A host module: its interface text, every item of which is part of the module.
    Interface(String),
    /// A module written in Colibri: its source, of which only `pub` items are part of the module.
    Source(String),
}

/// One standard module: its name, its capabilities and where its declarations are.
struct Module {
    name: String,
    caps: Vec<String>,
    origin: Origin,
}

fn modules() -> Vec<Module> {
    let mut out = Vec::new();
    for name in colibri_std::MODULES {
        let interface = colibri_std::interface(name).expect("a listed module has an interface");
        let (caps, text) = split_header(&interface.canonical_text());
        out.push(Module {
            name: name.to_string(),
            caps,
            origin: Origin::Interface(text),
        });
    }
    // A module written in Colibri needs what the modules it imports need.
    let host_caps: Vec<(String, Vec<String>)> = out
        .iter()
        .map(|m| (m.name.clone(), m.caps.clone()))
        .collect();
    for source in colibri_std::sources() {
        let mut caps: Vec<String> = Vec::new();
        for line in source.text.lines().filter(|l| l.starts_with("import std.")) {
            let named = line["import std.".len()..]
                .split([' ', '.'])
                .next()
                .unwrap_or_default();
            for (name, c) in &host_caps {
                if name == named {
                    for cap in c {
                        if !caps.contains(cap) {
                            caps.push(cap.clone());
                        }
                    }
                }
            }
        }
        caps.sort();
        out.push(Module {
            name: source.name.to_string(),
            caps,
            origin: Origin::Source(source.text.to_string()),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The capabilities named by the `@interface(..)` header line, and the text with that line
/// blanked out (so that offsets stay where they were).
fn split_header(text: &str) -> (Vec<String>, String) {
    let mut caps = Vec::new();
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("@interface(") {
            if let Some(start) = line.find("caps = [") {
                let rest = &line[start + "caps = [".len()..];
                let list = rest.split(']').next().unwrap_or("");
                caps = list
                    .split(',')
                    .map(|c| c.trim().trim_matches('"').to_string())
                    .filter(|c| !c.is_empty())
                    .collect();
            }
            out.extend(line.chars().map(|c| if c == '\n' { '\n' } else { ' ' }));
        } else {
            out.push_str(line);
        }
    }
    (caps, out)
}

/// Every page of the reference: the file name (inside the folder) and the text, the index
/// first.
pub fn std_pages() -> Vec<(String, String)> {
    let modules = modules();
    for m in &modules {
        assert!(
            SUMMARIES.iter().any(|(n, _)| *n == m.name),
            "the standard module `{}` has no entry in doccmd::SUMMARIES",
            m.name
        );
    }
    let mut pages = vec![(INDEX.to_string(), index(&modules))];
    for m in &modules {
        pages.push((format!("{}.md", m.name), module_page(m)));
    }
    pages
}

fn summary(name: &str) -> &'static str {
    SUMMARIES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
        .unwrap_or("")
}

const GENERATED: &str = "<!-- Generated by `colibri doc --std guide/std`: do not edit. -->";

fn index(modules: &[Module]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{GENERATED}\n");
    out.push_str("# Standard library reference\n\n");
    out.push_str(
        "One page for each module of the standard library. A page lists the constants, types and \
         functions of the module as they are declared, with the doc comment of each where the \
         module has one. These pages are generated from the interfaces the checker reads, so a \
         signature here is what `colibri check` holds your program to.\n\n",
    );
    out.push_str("| Module | What it is for | Written in | Needs |\n");
    out.push_str("|--------|----------------|------------|-------|\n");
    for m in modules {
        let kind = match m.origin {
            Origin::Interface(_) => "Rust",
            Origin::Source(_) => "Colibri",
        };
        let caps = if m.caps.is_empty() {
            "nothing".to_string()
        } else {
            m.caps
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let _ = writeln!(
            out,
            "| [`{0}`]({0}.md) | {1} | {2} | {3} |",
            m.name,
            summary(&m.name),
            kind,
            caps
        );
    }
    out.push_str(
        "\n## Reading a page\n\n\
         - `import fs` makes the module's names available as `fs.read`, `fs.Error` and so on.\n\
         - `Option[T]` in a signature is the optional type, written `T?` in your own code.\n\
         - `Result[T]` is a result whose error is the language's `Error`. A module's own error \
           type is `Error` on its page, and `fs.Error` in your code.\n\
         - **Needs** is the capability a program must be granted to use the module. `colibri run` \
           grants all of them; a Rust program that embeds Colibri chooses which its scripts get.\n\
         - `List`, `Map`, `Set`, `str`, `Result` and the functions that need no `import` are not \
           modules: see [built-in types and functions](../builtins.md).\n\
         - Methods of a type are listed under the type. `self` is the value the method is called on.\n\
         - The modules written in Rust have signatures only for now; their doc comments are \
           still to be written.\n\n\
         ## Regenerating\n\n\
         ```sh\n\
         colibri doc --std guide/std\n\
         ```\n\n\
         The test suite fails when these pages differ from what the command writes.\n",
    );
    out
}

/// What of a module is documented: its declarations, gathered from the syntax tree.
struct Page<'a> {
    text: &'a str,
    host: bool,
}

impl Page<'_> {
    fn slice(&self, start: u32, end: u32) -> &str {
        &self.text[start as usize..end as usize]
    }

    /// Whether the item is part of the module's surface.
    fn visible(&self, item: &Item) -> bool {
        self.host || item.public
    }

    /// The text of an item from the keyword (after attributes and `pub`) up to `end`, with the
    /// indentation of the item removed from its continuation lines.
    fn head(&self, item: &Item, end: u32) -> String {
        let mut start = item.span.start;
        for attr in &item.attrs {
            start = start.max(attr.span.end);
        }
        let column = {
            let before = &self.text[..start as usize];
            let line_start = before.rfind('\n').map_or(0, |i| i + 1);
            self.text[line_start..item.span.start as usize]
                .chars()
                .take_while(|c| *c == ' ')
                .count()
        };
        let raw = self.slice(start, end.max(start)).trim();
        let raw = raw.strip_prefix("pub ").unwrap_or(raw);
        let mut out = String::new();
        for (i, line) in raw.lines().enumerate() {
            if i > 0 {
                out.push('\n');
                let strip = line.len() - line.trim_start_matches(' ').len();
                out.push_str(&line[strip.min(column)..]);
            } else {
                out.push_str(line);
            }
        }
        out.trim_end().to_string()
    }

    /// The signature of a function: everything before its body.
    fn fn_head(&self, item: &Item) -> String {
        let ItemKind::Fn(decl) = &item.kind else {
            return String::new();
        };
        let end = match &decl.body {
            Some(FnBody::Block(b)) => b.span.start,
            Some(FnBody::Expr(e)) => e.span.start,
            None => item.span.end,
        };
        let head = self.head(item, end);
        let head = head.trim_end();
        head.strip_suffix("=>")
            .unwrap_or(head)
            .trim_end()
            .to_string()
    }
}

/// The text of an attribute of the item, `requires(fs)` -> `fs`.
fn attr_text(page: &Page, item: &Item, name: &str) -> Option<String> {
    item.attrs.iter().find(|a| a.name.name == name).map(|a| {
        let s = page.slice(a.span.start, a.span.end);
        let open = s.find('(').map_or(s.len(), |i| i + 1);
        s[open..]
            .trim_end_matches(')')
            .trim()
            .trim_matches('"')
            .to_string()
    })
}

/// A doc comment as Markdown: the common indentation of its lines is removed.
fn doc_text(doc: &Option<String>) -> String {
    let Some(doc) = doc else {
        return String::new();
    };
    let indent = doc
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out = String::new();
    for line in doc.lines() {
        out.push_str(line.get(indent..).unwrap_or("").trim_end());
        out.push('\n');
    }
    out
}

/// The part of the page for one type: its declaration, what it implements and its methods.
#[derive(Default)]
struct TypeEntry<'a> {
    implements: Vec<String>,
    methods: Vec<&'a Item>,
}

fn module_page(m: &Module) -> String {
    let (text, host) = match &m.origin {
        Origin::Interface(t) => (t.as_str(), true),
        Origin::Source(t) => (t.as_str(), false),
    };
    let (file, diags) = if host {
        parse_interface_file(FileId(0), text)
    } else {
        parse_file(FileId(0), text)
    };
    assert!(
        !diags.iter().any(Diagnostic::is_error),
        "the standard module `{}` does not parse",
        m.name
    );
    let page = Page { text, host };
    let items: Vec<&Item> = file
        .items
        .iter()
        .filter_map(|t| match t {
            TopLevel::Item(i) => Some(i),
            TopLevel::Stmt(_) => None,
        })
        .collect();

    // The impl blocks, gathered under the type they are for.
    let mut by_type: BTreeMap<String, TypeEntry> = BTreeMap::new();
    for item in &items {
        let ItemKind::Impl(decl) = &item.kind else {
            continue;
        };
        let TypeKind::Named { path, .. } = &decl.target.kind else {
            continue;
        };
        let Some(last) = path.last() else { continue };
        let entry = by_type.entry(last.name.clone()).or_default();
        match &decl.interface {
            Some(iface) => {
                let name = page.slice(iface.span.start, iface.span.end).to_string();
                if !entry.implements.contains(&name) {
                    entry.implements.push(name);
                }
            }
            None => entry
                .methods
                .extend(decl.items.iter().filter(|i| page.visible(i))),
        }
    }

    let mut out = String::new();
    let _ = writeln!(out, "{GENERATED}\n");
    let _ = writeln!(out, "# `{}`\n", m.name);
    let _ = writeln!(out, "{}\n", summary(&m.name));
    let _ = writeln!(out, "```colibri\nimport {}\n```\n", m.name);
    if !m.caps.is_empty() {
        let caps: Vec<String> = m.caps.iter().map(|c| format!("`{c}`")).collect();
        let _ = writeln!(
            out,
            "Needs the {} {}.\n",
            caps.join(" and "),
            if caps.len() > 1 {
                "capabilities"
            } else {
                "capability"
            }
        );
    }
    let _ = writeln!(out, "Back to the [standard library index]({INDEX}).\n");

    let section = |out: &mut String, title: &str, kinds: &dyn Fn(&Item) -> bool| {
        let chosen: Vec<&&Item> = items
            .iter()
            .filter(|i| page.visible(i) && kinds(i))
            .collect();
        if chosen.is_empty() {
            return;
        }
        let _ = writeln!(out, "## {title}\n");
        for item in chosen {
            render_item(out, &page, item, &by_type, m);
        }
    };
    section(&mut out, "Constants", &|i| {
        matches!(i.kind, ItemKind::Binding(_))
    });
    section(&mut out, "Types", &|i| {
        matches!(
            i.kind,
            ItemKind::Struct(_)
                | ItemKind::Enum(_)
                | ItemKind::Interface(_)
                | ItemKind::TypeAlias(_)
        )
    });
    section(&mut out, "Functions", &|i| {
        matches!(i.kind, ItemKind::Fn(_))
    });
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

fn name_of(item: &Item) -> String {
    match &item.kind {
        ItemKind::Fn(d) => d.name.name.clone(),
        ItemKind::Struct(d) => d.name.name.clone(),
        ItemKind::Enum(d) => d.name.name.clone(),
        ItemKind::Interface(d) => d.name.name.clone(),
        ItemKind::TypeAlias(d) => d.name.name.clone(),
        _ => String::new(),
    }
}

fn render_item(
    out: &mut String,
    page: &Page,
    item: &Item,
    by_type: &BTreeMap<String, TypeEntry>,
    m: &Module,
) {
    match &item.kind {
        ItemKind::Binding(_) => {
            let head = page.head(item, item.span.end);
            // `const PI: float = 3.14` -> the name is the word after the keyword.
            let name = head
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .filter(|w| !w.is_empty())
                .nth(1)
                .unwrap_or("")
                .to_string();
            let _ = writeln!(out, "### `{name}`\n");
            code(out, &head);
            notes(out, page, item, m);
        }
        ItemKind::Fn(decl) => {
            let _ = writeln!(out, "### `{}`\n", decl.name.name);
            code(out, &page.fn_head(item));
            notes(out, page, item, m);
        }
        ItemKind::TypeAlias(_) => {
            let _ = writeln!(out, "### `{}`\n", name_of(item));
            code(out, &page.head(item, item.span.end));
            notes(out, page, item, m);
        }
        ItemKind::Struct(decl) => {
            let _ = writeln!(out, "### `{}`\n", decl.name.name);
            code(out, &struct_head(page, item, decl));
            notes(out, page, item, m);
            for field in decl.fields.iter().filter(|f| page.host || f.public) {
                if let Some(doc) = &field.doc {
                    let _ = writeln!(
                        out,
                        "- `{}`: {}",
                        field.name.name,
                        doc_text(&Some(doc.clone())).trim().replace('\n', " ")
                    );
                }
            }
            if decl
                .fields
                .iter()
                .any(|f| (page.host || f.public) && f.doc.is_some())
            {
                out.push('\n');
            }
            type_members(out, page, &decl.name.name, by_type, m);
        }
        ItemKind::Enum(decl) => {
            let _ = writeln!(out, "### `{}`\n", decl.name.name);
            let mut text = decl_head(page, item);
            text.push_str(" {\n");
            for v in &decl.variants {
                let _ = writeln!(text, "    {}", page.slice(v.span.start, v.span.end).trim());
            }
            text.push('}');
            code(out, &text);
            notes(out, page, item, m);
            let documented: Vec<_> = decl.variants.iter().filter(|v| v.doc.is_some()).collect();
            for v in &documented {
                let _ = writeln!(
                    out,
                    "- `{}`: {}",
                    v.name.name,
                    doc_text(&v.doc).trim().replace('\n', " ")
                );
            }
            if !documented.is_empty() {
                out.push('\n');
            }
            type_members(out, page, &decl.name.name, by_type, m);
        }
        ItemKind::Interface(decl) => {
            let _ = writeln!(out, "### `{}`\n", decl.name.name);
            let mut text = decl_head(page, item);
            text.push_str(" {\n");
            for method in &decl.items {
                if let ItemKind::Fn(_) = method.kind {
                    for line in page.fn_head(method).lines() {
                        let _ = writeln!(text, "    {line}");
                    }
                }
            }
            text.push('}');
            code(out, &text);
            notes(out, page, item, m);
            for method in decl.items.iter().filter(|i| i.doc.is_some()) {
                let _ = writeln!(
                    out,
                    "- `{}`: {}",
                    name_of(method),
                    doc_text(&method.doc).trim().replace('\n', " ")
                );
            }
        }
        _ => {}
    }
}

/// The head of a type declaration: its keyword, name and generic parameters.
fn decl_head(page: &Page, item: &Item) -> String {
    let raw = page.head(item, item.span.end);
    let end = raw.find('{').unwrap_or(raw.len());
    raw[..end].trim_end().to_string()
}

/// The declaration of a struct or class: its head and its fields (the `pub` ones of a module
/// written in Colibri).
fn struct_head(page: &Page, item: &Item, decl: &StructDecl) -> String {
    let mut text = decl_head(page, item);
    let fields: Vec<_> = decl
        .fields
        .iter()
        .filter(|f| page.host || f.public)
        .collect();
    let hidden = decl.fields.len() - fields.len();
    if fields.is_empty() && hidden == 0 {
        text.push_str(" {}");
        return text;
    }
    text.push_str(" {\n");
    for f in fields {
        let mut line = format!(
            "    {}: {}",
            f.name.name,
            page.slice(f.ty.span.start, f.ty.span.end)
        );
        if let Some(d) = &f.default {
            let _ = write!(line, " = {}", page.slice(d.span.start, d.span.end));
        }
        text.push_str(&line);
        text.push('\n');
    }
    if hidden > 0 {
        text.push_str("    // and private fields\n");
    }
    text.push('}');
    text
}

/// What a type implements, and its methods.
fn type_members(
    out: &mut String,
    page: &Page,
    name: &str,
    by_type: &BTreeMap<String, TypeEntry>,
    m: &Module,
) {
    let Some(entry) = by_type.get(name) else {
        return;
    };
    if !entry.implements.is_empty() {
        let list: Vec<String> = entry.implements.iter().map(|i| format!("`{i}`")).collect();
        let _ = writeln!(out, "Implements {}.\n", list.join(", "));
    }
    for method in &entry.methods {
        let ItemKind::Fn(decl) = &method.kind else {
            continue;
        };
        let _ = writeln!(out, "#### `{name}.{}`\n", decl.name.name);
        code(out, &page.fn_head(method));
        notes(out, page, method, m);
    }
}

/// The doc comment of an item, then its deprecation and capability notes.
fn notes(out: &mut String, page: &Page, item: &Item, m: &Module) {
    let doc = doc_text(&item.doc);
    if !doc.is_empty() {
        out.push_str(&doc);
        out.push('\n');
    }
    if let Some(message) = attr_text(page, item, "deprecated") {
        let _ = writeln!(out, "**Deprecated:** {message}\n");
    }
    // A module that needs several capabilities says which one each item needs.
    if m.caps.len() > 1
        && let Some(cap) = attr_text(page, item, "requires")
    {
        let _ = writeln!(out, "Needs the `{cap}` capability.\n");
    }
}

fn code(out: &mut String, text: &str) {
    let _ = writeln!(out, "```text\n{text}\n```\n");
}

/// `colibri doc --std DIR`: writes the pages into `dir`, creating it. With `check`, writes
/// nothing and fails when a page differs, is missing or is not one the generator makes.
pub fn run(dir: &Path, check: bool) -> ExitCode {
    let pages = std_pages();
    if check {
        let mut stale = Vec::new();
        for (name, text) in &pages {
            if read_page(&dir.join(name)).as_deref() != Some(text.as_str()) {
                stale.push(name.clone());
            }
        }
        for extra in extra_files(dir, &pages) {
            stale.push(format!("{extra} (not a page of the reference)"));
        }
        if stale.is_empty() {
            return ExitCode::SUCCESS;
        }
        eprintln!(
            "the reference in `{}` is out of date: {}\nrun `colibri doc --std {}`",
            dir.display(),
            stale.join(", "),
            dir.display()
        );
        return ExitCode::from(1);
    }
    if let Err(e) = fs::create_dir_all(dir) {
        eprintln!("error: cannot create `{}`: {e}", dir.display());
        return ExitCode::from(1);
    }
    for (name, text) in &pages {
        let path = dir.join(name);
        if read_page(&path).as_deref() == Some(text.as_str()) {
            continue;
        }
        if let Err(e) = fs::write(&path, text) {
            eprintln!("error: cannot write `{}`: {e}", path.display());
            return ExitCode::from(1);
        }
    }
    // A page of a module that no longer exists is removed.
    for extra in extra_files(dir, &pages) {
        if extra.ends_with(".md") {
            let _ = fs::remove_file(dir.join(extra));
        }
    }
    ExitCode::SUCCESS
}

/// A page on disk, with the line ends of a checkout on Windows turned back into `\n`.
fn read_page(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|t| t.replace("\r\n", "\n"))
}

/// The files of `dir` that are not among `pages`.
pub fn extra_files(dir: &Path, pages: &[(String, String)]) -> Vec<String> {
    let mut extra = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !pages.iter().any(|(n, _)| *n == name) {
                extra.push(name);
            }
        }
    }
    extra.sort();
    extra
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_for_every_module_and_an_index() {
        let pages = std_pages();
        assert_eq!(pages.len(), 1 + colibri_std::MODULES.len() + 2);
        assert_eq!(pages[0].0, INDEX);
    }

    #[test]
    fn only_pub_items_of_a_source_module_are_shown() {
        let pages = std_pages();
        let httpd = &pages.iter().find(|(n, _)| n == "httpd.md").unwrap().1;
        assert!(httpd.contains("### `fail`"));
        assert!(
            !httpd.contains("HeadParser"),
            "a private type is not documented"
        );
        assert!(
            !httpd.contains("`Headers.lines`"),
            "a private method is not documented"
        );
    }
}
