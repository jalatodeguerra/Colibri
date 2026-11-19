//! `colibri repl`: an interactive session (ARCHITECTURE §21).
//!
//! The session is one growing program. `Session::items` holds the source of every declaration
//! and module-level binding entered so far; each entry is checked together with all of them
//! (`Vm::session_check`) and, if it checks, put to work in the running `Vm`
//! (`Vm::session_commit`), which keeps the module-level values and runs only the new code.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use colibri_runtime as colibri;
use colibri_syntax::ast::{BindKind, ItemKind, PatKind, StmtKind, TopLevel};
use colibri_syntax::{FileId, parse_file};

use crate::repl_input::is_complete;

const HELP: &str = "\
Type a statement, an expression (its value is printed with its type) or a declaration
(fn, struct, class, enum, interface, impl, import, let, var, const). Everything persists.
A line that leaves a bracket or an expression open continues on the next line; an empty
line submits what is there.

  :help           this text
  :type <expr>    the type of an expression, without running it
  :load <file>    load the declarations of a file (top-level statements are ignored)
  :run            run the event loop until every coroutine has finished
  :reset          forget everything and start a new session
  :quit           leave (Ctrl-D does too)
";

/// The name of the module-level value that holds the result of an expression entry.
const LAST: &str = "__last";

/// One declaration or module-level binding of the session.
#[derive(Clone)]
struct Item {
    /// What it defines (`fn:name`, `type:name`, `val:name`, `import:text`), if it can be
    /// defined again.
    key: Option<String>,
    text: String,
    /// Part of the current entry.
    fresh: bool,
}

/// An entry split into declarations and top-level statements.
struct Parts {
    items: Vec<Item>,
    script: String,
    /// The one statement of the entry when it is an expression.
    expr: Option<String>,
    statements: usize,
}

/// The source of a check, and how to show its diagnostics.
struct Built {
    source: String,
    /// `(start in source, end in source, start in shown)`.
    segments: Vec<(usize, usize, usize)>,
    shown: String,
}

struct Session {
    vm: colibri::Vm,
    items: Vec<Item>,
    /// Numbers the `__entry_N` functions that keep an entry's closures alive.
    entries: u32,
    color: bool,
    last_tick: Instant,
}

/// Runs the REPL on stdin and stdout.
pub fn run() -> ExitCode {
    let interactive = io::stdin().is_terminal();
    let mut session = match Session::new(io::stderr().is_terminal()) {
        Ok(s) => s,
        Err(e) => {
            eprint!("{}", e.render(false));
            return ExitCode::from(1);
        }
    };
    if interactive {
        println!("colibri repl (:help for help, :quit or Ctrl-D to leave)");
        interactive_loop(&mut session);
    } else {
        piped_loop(&mut session);
    }
    ExitCode::SUCCESS
}

fn piped_loop(session: &mut Session) {
    let stdin = io::stdin();
    let mut pending = String::new();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if pending.is_empty() && line.trim_start().starts_with(':') {
            if !session.command(line.trim()) {
                return;
            }
            continue;
        }
        pending.push_str(&line);
        pending.push('\n');
        if is_complete(&pending) || line.trim().is_empty() {
            session.entry(&pending);
            pending.clear();
        }
    }
    if !pending.trim().is_empty() {
        session.entry(&pending);
    }
}

fn interactive_loop(session: &mut Session) {
    let Ok(mut editor) = rustyline::DefaultEditor::new() else {
        piped_loop(session);
        return;
    };
    let mut pending = String::new();
    loop {
        let prompt = if pending.is_empty() {
            "colibri> "
        } else {
            "     ... "
        };
        match editor.readline(prompt) {
            Ok(line) => {
                if pending.is_empty() && line.trim_start().starts_with(':') {
                    let _ = editor.add_history_entry(line.as_str());
                    if !session.command(line.trim()) {
                        return;
                    }
                    continue;
                }
                pending.push_str(&line);
                pending.push('\n');
                if is_complete(&pending) || line.trim().is_empty() {
                    let _ = editor.add_history_entry(pending.trim_end());
                    session.entry(&pending);
                    pending.clear();
                }
            }
            // Ctrl-C drops what was being typed.
            Err(rustyline::error::ReadlineError::Interrupted) => pending.clear(),
            Err(_) => return,
        }
    }
}

impl Session {
    fn new(color: bool) -> Result<Session, colibri::Error> {
        let mut vm = crate::build_vm(crate::vm_builder().allow_all(), Vec::new());
        vm.session_start()?;
        Ok(Session {
            vm,
            items: Vec::new(),
            entries: 0,
            color,
            last_tick: Instant::now(),
        })
    }

    /// Runs a `:command`; false means leave.
    fn command(&mut self, line: &str) -> bool {
        let (name, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match name {
            ":quit" | ":q" | ":exit" => return false,
            ":help" | ":h" | ":?" => print!("{HELP}"),
            ":reset" => match self.vm.session_start() {
                Ok(()) => {
                    self.items.clear();
                    self.entries = 0;
                    println!("session reset");
                }
                Err(e) => eprint!("{}", e.render(self.color)),
            },
            ":type" | ":t" => self.type_of(rest),
            ":load" | ":l" => self.load(rest),
            ":run" => {
                let result = self.vm.run_event_loop();
                self.last_tick = Instant::now();
                if let Err(e) = result {
                    eprint!("{}", e.render(self.color));
                }
            }
            other => eprintln!("unknown command `{other}` (:help lists them)"),
        }
        let _ = io::stdout().flush();
        true
    }

    fn type_of(&mut self, expr: &str) {
        if expr.is_empty() {
            eprintln!("usage: :type <expression>");
            return;
        }
        let mut items = self.items.clone();
        upsert(&mut items, last_item(expr, "__type"));
        let built = build(&items, "");
        match self.vm.session_check(&built.source) {
            Ok(check) => match check.global_type("__type") {
                Some(ty) => println!("{ty}"),
                None => eprintln!("error: no type"),
            },
            Err(e) => eprint!("{}", self.render_error(&e, &built)),
        }
    }

    fn load(&mut self, path: &str) {
        if path.is_empty() {
            eprintln!("usage: :load <file>");
            return;
        }
        match std::fs::read_to_string(Path::new(path)) {
            Ok(text) => {
                self.eval(&text, true, false);
                let _ = io::stdout().flush();
            }
            Err(e) => eprintln!("error: cannot read {path}: {e}"),
        }
    }

    /// One entry. It runs top to bottom like a script (DESIGN §4.2): an entry where a `let`
    /// or `var` follows a statement runs in pieces, one after the other, so that the binding
    /// sees what the statement did. Only the last piece shows its value; a piece that fails
    /// stops the rest.
    fn entry(&mut self, text: &str) {
        if !text.trim().is_empty() {
            let pieces = pieces(text);
            let last = pieces.len() - 1;
            for (i, piece) in pieces.into_iter().enumerate() {
                if !self.eval(piece, false, i == last) {
                    break;
                }
            }
        }
        self.settle();
        let _ = io::stdout().flush();
    }

    /// Runs the coroutines that are due now (on the real time that passed since the last
    /// entry) without sleeping, until none is. A coroutine that keeps asking to run on the
    /// next tick (`wait_frames(1)` in a loop) is left for the next entry.
    fn settle(&mut self) {
        let dt = self.last_tick.elapsed().as_secs_f64();
        self.last_tick = Instant::now();
        let mut dt = dt;
        for _ in 0..100 {
            let before = self.vm.coroutine_resumptions();
            if let Err(e) = self.vm.tick(dt) {
                eprint!("{}", e.render(self.color));
            }
            dt = 0.0;
            if self.vm.coroutine_resumptions() == before {
                break;
            }
        }
    }

    /// Checks and runs `text`; whether it was taken. With `echo`, an entry that is one
    /// expression prints its value.
    fn eval(&mut self, text: &str, load: bool, echo: bool) -> bool {
        let mut parts = split(text);
        if !echo {
            parts.expr = None;
        }
        if load {
            if parts.statements > 0 {
                eprintln!(
                    "note: ignored {} top-level statement(s) of the file",
                    parts.statements
                );
            }
            parts.script.clear();
            parts.expr = None;
        }
        let mut items: Vec<Item> = self.items.clone();
        let mut replaced: Vec<String> = Vec::new();
        let mut reinit: Vec<String> = Vec::new();
        for item in parts.items {
            if let Some(key) = &item.key
                && let Some(pos) = items.iter().position(|i| i.key.as_ref() == Some(key))
            {
                if let Some(name) = key.strip_prefix("type:") {
                    eprintln!(
                        "error: `{name}` is already defined; types cannot be redefined (`:reset` starts over)"
                    );
                    return false;
                }
                if key.starts_with("import:") {
                    continue;
                }
                if let Some(name) = key.strip_prefix("fn:") {
                    replaced.push(name.to_string());
                } else if let Some(name) = key.strip_prefix("val:") {
                    reinit.push(name.to_string());
                }
                items[pos] = item;
                continue;
            }
            items.push(item);
        }
        // An expression entry: `let __last = (expr)` gives its type and value in one go.
        if let Some(expr) = &parts.expr {
            let mut with = items.clone();
            upsert(&mut with, last_item(expr, LAST));
            let built = build(&with, "");
            if let Ok(check) = self.vm.session_check(&built.source)
                && !matches!(check.global_type(LAST).as_deref(), Some("()" | "!") | None)
            {
                let mut reinit2 = reinit.clone();
                reinit2.push(LAST.to_string());
                if !self.commit(check, &reinit2, &replaced, &built) {
                    return false;
                }
                self.items = with;
                if let Some((value, ty)) = self.vm.session_display(LAST) {
                    println!("= {value} : {ty}");
                }
                return true;
            }
        }
        // Declarations and statements.
        let mut script = parts.script;
        let mut built = build(&items, &script);
        let mut check = match self.vm.session_check(&built.source) {
            Ok(c) => c,
            Err(e) => {
                eprint!("{}", self.render_error(&e, &built));
                return false;
            }
        };
        let mut entry_fn = false;
        if check.script_has_closures() {
            // The closures of the statements outlive the entry, so their code stays in the
            // program as a function that is never called again.
            let n = self.entries + 1;
            items.push(Item {
                key: None,
                text: format!("fn __entry_{n}() {{\n{script}\n}}"),
                fresh: true,
            });
            script = format!("__entry_{n}()");
            built = build(&items, &script);
            check = match self.vm.session_check(&built.source) {
                Ok(c) => c,
                Err(e) => {
                    eprint!("{}", self.render_error(&e, &built));
                    return false;
                }
            };
            entry_fn = true;
        }
        if !self.commit(check, &reinit, &replaced, &built) {
            return false;
        }
        self.items = items;
        if entry_fn {
            self.entries += 1;
        }
        true
    }

    /// Puts a checked entry to work; whether it was taken.
    fn commit(
        &mut self,
        check: colibri::SessionCheck,
        reinit: &[String],
        replaced: &[String],
        _built: &Built,
    ) -> bool {
        let reinit: Vec<&str> = reinit.iter().map(String::as_str).collect();
        let replaced: Vec<&str> = replaced.iter().map(String::as_str).collect();
        match self.vm.session_commit(check, &reinit, &replaced) {
            Ok(()) => true,
            Err(colibri::SessionError::Conflict(why)) => {
                eprintln!("error: {why}");
                false
            }
            Err(colibri::SessionError::Failed(e)) => {
                eprint!("{}", e.render(self.color));
                false
            }
        }
    }

    /// The diagnostics of a failed check, pointing into what the user typed.
    fn render_error(&self, error: &colibri::Error, built: &Built) -> String {
        let colibri::Error::Compile(report) = error else {
            return error.render(self.color);
        };
        let mut sources = report.sources.clone();
        let shown = sources.add_file("<repl>", built.shown.as_str());
        let all = sources.add_file("<session>", built.source.as_str());
        let session_file = (0..)
            .map(FileId)
            .take_while(|f| sources.contains(*f))
            .find(|f| sources.name(*f) == "<repl>" && *f != shown);
        let remap = |span: colibri_syntax::Span| {
            if Some(span.file) != session_file {
                return span;
            }
            let (s, e) = (span.start as usize, span.end as usize);
            for &(from, to, at) in &built.segments {
                if s >= from && s <= to {
                    let end = e.min(to) - from + at;
                    return colibri_syntax::Span::new(shown, (s - from + at) as u32, end as u32);
                }
            }
            colibri_syntax::Span::new(all, span.start, span.end)
        };
        let diagnostics = report
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(|d| {
                let mut d = d.clone();
                d.span = remap(d.span);
                for l in &mut d.labels {
                    l.0 = remap(l.0);
                }
                d
            })
            .collect();
        colibri::Report {
            sources,
            diagnostics,
        }
        .render(self.color)
    }
}

/// `let name = (expr)` as the item that holds an expression entry.
fn last_item(expr: &str, name: &str) -> Item {
    Item {
        key: Some(format!("val:{name}")),
        text: format!("let {name} = (\n{}\n)", expr.trim()),
        fresh: true,
    }
}

/// Replaces the item with the same key, or adds it.
fn upsert(items: &mut Vec<Item>, item: Item) {
    match items.iter().position(|i| i.key == item.key) {
        Some(pos) => items[pos] = item,
        None => items.push(item),
    }
}

/// The program text: the entry's statements first (so that their positions are the ones the
/// user typed), then the declarations.
fn build(items: &[Item], script: &str) -> Built {
    let mut source = String::new();
    let mut shown = String::new();
    let mut segments = Vec::new();
    if !script.is_empty() {
        segments.push((0, script.len(), 0));
        source.push_str(script);
        shown.push_str(script);
    }
    for item in items {
        source.push('\n');
        if item.fresh {
            shown.push('\n');
            segments.push((source.len(), source.len() + item.text.len(), shown.len()));
            shown.push_str(&item.text);
        }
        source.push_str(&item.text);
    }
    Built {
        source,
        segments,
        shown,
    }
}

/// An entry cut before every `let` or `var` that follows a statement, so that each binding
/// runs after the statements above it. Other declarations (`fn`, types, `const`, ..) stay
/// where they are and are visible to the whole of their piece. Text that does not parse is
/// one piece (the checker reports it).
fn pieces(text: &str) -> Vec<&str> {
    let (file, diagnostics) = parse_file(FileId(0), text);
    if diagnostics.iter().any(|d| d.is_error()) {
        return vec![text];
    }
    let mut out = Vec::new();
    let (mut start, mut prev_end, mut after_stmt) = (0usize, 0usize, false);
    for top in &file.items {
        let (end, binding, stmt) = match top {
            TopLevel::Item(i) => {
                let binding = matches!(&i.kind, ItemKind::Binding(b) if b.kind != BindKind::Const);
                (i.span.end as usize, binding, false)
            }
            TopLevel::Stmt(s) => (s.span.end as usize, false, true),
        };
        if binding && after_stmt {
            out.push(&text[start..prev_end]);
            // The next piece starts after the separator (a newline or `;`).
            start = prev_end + text[prev_end..].len()
                - text[prev_end..]
                    .trim_start_matches(|c: char| c == ';' || c.is_whitespace())
                    .len();
        }
        after_stmt = (after_stmt && !binding) || stmt;
        prev_end = end.min(text.len());
    }
    out.push(&text[start..]);
    out
}

/// Splits an entry into declarations, statements and the expression it may be.
fn split(text: &str) -> Parts {
    let (file, diagnostics) = parse_file(FileId(0), text);
    let whole = |text: &str| Parts {
        items: Vec::new(),
        script: text.trim().to_string(),
        expr: None,
        statements: 1,
    };
    if diagnostics.iter().any(|d| d.is_error()) {
        return whole(text);
    }
    let mut parts = Parts {
        items: Vec::new(),
        script: String::new(),
        expr: None,
        statements: 0,
    };
    let mut prev = 0usize;
    let mut last_expr = None;
    for top in &file.items {
        let (end, is_item) = match top {
            TopLevel::Item(i) => (i.span.end as usize, true),
            TopLevel::Stmt(s) => (s.span.end as usize, false),
        };
        let end = end.min(text.len());
        // A chunk starts after the separator of the one before (a newline or `;`).
        let chunk = text[prev.min(end)..end]
            .trim_start_matches(|c: char| c == ';' || c.is_whitespace())
            .trim_end()
            .to_string();
        prev = end;
        match top {
            TopLevel::Item(item) => {
                let key = match &item.kind {
                    ItemKind::Fn(f) if f.receiver.is_none() => Some(format!("fn:{}", f.name.name)),
                    ItemKind::Struct(s) => Some(format!("type:{}", s.name.name)),
                    ItemKind::Enum(e) => Some(format!("type:{}", e.name.name)),
                    ItemKind::Interface(i) => Some(format!("type:{}", i.name.name)),
                    ItemKind::TypeAlias(t) => Some(format!("type:{}", t.name.name)),
                    ItemKind::Binding(b) => match &b.pat.kind {
                        PatKind::Ident(id) => Some(format!("val:{}", id.name)),
                        _ => None,
                    },
                    ItemKind::Import(_) => Some(format!(
                        "import:{}",
                        chunk.split_whitespace().collect::<Vec<_>>().join(" ")
                    )),
                    _ => None,
                };
                parts.items.push(Item {
                    key,
                    text: chunk,
                    fresh: true,
                });
            }
            TopLevel::Stmt(stmt) => {
                parts.statements += 1;
                if !parts.script.is_empty() {
                    parts.script.push('\n');
                }
                parts.script.push_str(&chunk);
                last_expr = matches!(stmt.kind, StmtKind::Expr(_)).then(|| chunk.clone());
            }
        }
        let _ = is_item;
    }
    if parts.statements == 1 {
        parts.expr = last_expr;
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_declarations_from_statements() {
        let p = split("fn f() => 1\nlet x = 2\nprint(x)\n");
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].key.as_deref(), Some("fn:f"));
        assert_eq!(p.items[1].key.as_deref(), Some("val:x"));
        assert_eq!(p.script, "print(x)");
        assert_eq!(p.expr.as_deref(), Some("print(x)"));
    }

    #[test]
    fn an_entry_is_cut_where_a_declaration_follows_a_statement() {
        let p = pieces("var a: List[int] = []\na.push(1)\nlet n = a.len()\nprint(n)\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].trim(), "var a: List[int] = []\na.push(1)");
        assert_eq!(p[1].trim(), "let n = a.len()\nprint(n)");
        assert_eq!(pieces("let x = 1\nlet y = 2\nprint(x + y)\n").len(), 1);
        // Functions and types are not cut off from the statements that use them.
        assert_eq!(pieces("print(f())\nfn f() -> int => 1\n").len(), 1);
        let p = pieces("print(1); fn f() -> int => 2; let w = f(); print(w)");
        assert_eq!(p, ["print(1); fn f() -> int => 2", "let w = f(); print(w)"]);
    }

    #[test]
    fn assignments_are_statements_not_expressions() {
        let p = split("x = 1");
        assert!(p.items.is_empty());
        assert_eq!(p.script, "x = 1");
        assert!(p.expr.is_none());
    }

    #[test]
    fn a_syntax_error_is_left_to_the_checker() {
        let p = split("fn (");
        assert!(p.items.is_empty());
        assert_eq!(p.script, "fn (");
    }
}
