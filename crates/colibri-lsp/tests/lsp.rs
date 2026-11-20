//! Drives the server in-process through `lsp-server`'s in-memory connection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use colibri_lsp::{Config, serve};
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use serde_json::{Value, json};

struct Client {
    conn: Connection,
    next_id: i32,
    /// The result of `initialize`.
    init: Value,
    /// The latest diagnostics published for each URI.
    diagnostics: HashMap<String, Vec<Value>>,
    /// `window/logMessage` texts.
    logs: Vec<String>,
    /// `window/showMessage` texts.
    shown: Vec<String>,
    server: Option<JoinHandle<()>>,
}

impl Client {
    fn start(root: Option<&Path>, options: Value) -> Client {
        let (server_side, conn) = Connection::memory();
        let server = std::thread::spawn(move || {
            serve(server_side, Config::default()).expect("the server runs");
        });
        let mut client = Client {
            conn,
            next_id: 1,
            init: Value::Null,
            diagnostics: HashMap::new(),
            logs: Vec::new(),
            shown: Vec::new(),
            server: Some(server),
        };
        let root_uri = root.map(uri);
        let init = client.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "capabilities": {},
                "initializationOptions": options,
            }),
        );
        assert_eq!(init["serverInfo"]["name"], "colibri");
        assert_eq!(init["capabilities"]["hoverProvider"], true);
        assert_eq!(init["capabilities"]["definitionProvider"], true);
        client.init = init;
        client.notify("initialized", json!({}));
        client
    }

    fn send(&self, message: impl Into<Message>) {
        self.conn.sender.send(message.into()).expect("server is up");
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(Notification::new(method.to_string(), params));
    }

    fn absorb(&mut self, note: Notification) {
        match note.method.as_str() {
            "textDocument/publishDiagnostics" => {
                let uri = note.params["uri"].as_str().unwrap().to_string();
                let list = note.params["diagnostics"].as_array().unwrap().clone();
                self.diagnostics.insert(uri, list);
            }
            "window/showMessage" => self
                .shown
                .push(note.params["message"].as_str().unwrap().into()),
            "window/logMessage" => self
                .logs
                .push(note.params["message"].as_str().unwrap().into()),
            _ => {}
        }
    }

    /// Sends a request and waits for its response, taking notes of what arrives first.
    fn request_raw(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = RequestId::from(self.next_id);
        self.next_id += 1;
        self.send(Request::new(id.clone(), method.to_string(), params));
        loop {
            match self.conn.receiver.recv().expect("the server answers") {
                Message::Response(r) if r.id == id => {
                    return match r.error {
                        Some(e) => Err(json!({"code": e.code, "message": e.message})),
                        None => Ok(r.result.unwrap_or(Value::Null)),
                    };
                }
                Message::Notification(n) => self.absorb(n),
                other => panic!("unexpected message {other:?}"),
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.request_raw(method, params)
            .expect("the request succeeds")
    }

    /// Waits until everything sent so far was handled (a request is answered after the
    /// notifications before it).
    fn sync(&mut self) {
        let _ = self.request_raw("colibri/ping", Value::Null);
    }

    fn open(&mut self, path: &Path, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": uri(path), "languageId": "colibri", "version": 1, "text": text}}),
        );
        self.sync();
    }

    fn change(&mut self, path: &Path, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({"textDocument": {"uri": uri(path), "version": 2}, "contentChanges": [{"text": text}]}),
        );
        self.sync();
    }

    fn diags(&self, path: &Path) -> Vec<Value> {
        self.diagnostics
            .get(&uri(path))
            .cloned()
            .unwrap_or_default()
    }

    fn hover(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/hover",
            json!({"textDocument": {"uri": uri(path)}, "position": {"line": line, "character": character}}),
        )
    }

    fn definition(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/definition",
            json!({"textDocument": {"uri": uri(path)}, "position": {"line": line, "character": character}}),
        )
    }

    /// The completion items at the end of the first `needle` in `text`.
    fn complete_after(&mut self, path: &Path, text: &str, needle: &str) -> Vec<Value> {
        let (line, col) = pos_after(text, needle);
        let result = self.request(
            "textDocument/completion",
            json!({"textDocument": {"uri": uri(path)}, "position": {"line": line, "character": col}}),
        );
        result.as_array().expect("an array of items").clone()
    }

    fn references(&mut self, path: &Path, pos: (u32, u32), declaration: bool) -> Vec<Value> {
        let result = self.request(
            "textDocument/references",
            json!({"textDocument": {"uri": uri(path)}, "position": {"line": pos.0, "character": pos.1},
                   "context": {"includeDeclaration": declaration}}),
        );
        result.as_array().cloned().unwrap_or_default()
    }

    fn prepare_rename(&mut self, path: &Path, pos: (u32, u32)) -> Result<Value, Value> {
        self.request_raw(
            "textDocument/prepareRename",
            json!({"textDocument": {"uri": uri(path)}, "position": {"line": pos.0, "character": pos.1}}),
        )
    }

    fn rename(&mut self, path: &Path, pos: (u32, u32), new_name: &str) -> Result<Value, Value> {
        self.request_raw(
            "textDocument/rename",
            json!({"textDocument": {"uri": uri(path)}, "position": {"line": pos.0, "character": pos.1},
                   "newName": new_name}),
        )
    }

    fn document_symbols(&mut self, path: &Path) -> Value {
        self.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri(path)}}),
        )
    }

    fn shutdown(mut self) {
        assert_eq!(self.request("shutdown", Value::Null), Value::Null);
        self.notify("exit", Value::Null);
        self.server
            .take()
            .unwrap()
            .join()
            .expect("the server exits cleanly");
    }
}

/// The `file:` URI of a path, written the way editors write them (VS Code: `file:///c%3A/dir`):
/// `/` separators, a leading `/` before a Windows drive, everything but letters, digits and
/// `-._~/` percent-encoded. A native path pasted after `file://` (`file://C:\dir`) is not a
/// URI. This is a separate implementation from the server's own, so a mistake in one of them
/// shows as a difference.
fn uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !text.starts_with('/') {
        out.push('/');
    }
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `path` for a TOML basic string or a Colibri string: `/` separators, which Windows reads as
/// it reads `\\` (which would start an escape). Never paste `path.display()` into source text.
fn slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// The path a `file:` URI of this server names (the reverse of [`uri`]).
fn uri_path(uri: &str) -> PathBuf {
    let rest = uri.strip_prefix("file://").expect("a file: URI");
    let mut bytes = Vec::new();
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            bytes.push(u8::from_str_radix(&rest[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    let text = String::from_utf8(bytes).unwrap();
    // `/C:/dir` is the Windows path `C:/dir`.
    let b = text.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        PathBuf::from(&text[1..])
    } else {
        PathBuf::from(text)
    }
}

/// Makes `link` a link to `target`: a symbolic link on Unix; on Windows a junction for a folder
/// (no privilege needed) and a symbolic link for a file (which needs Developer Mode or an
/// elevated shell, as the CI machines have). A test that needs a link fails if it cannot be
/// made, it is never skipped.
fn make_link(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    if target.is_dir() {
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "mklink /J failed: {}",
            String::from_utf8_lossy(&made.stdout)
        );
    } else {
        std::os::windows::fs::symlink_file(target, link)
            .expect("a symbolic link needs Developer Mode or an elevated shell");
    }
}

/// A fresh folder for a test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("lsp-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

/// `(line, UTF-16 column)` of the first `needle` in `text`.
fn pos_of(text: &str, needle: &str) -> (u32, u32) {
    let at = text.find(needle).expect("the text has it");
    let line = text[..at].matches('\n').count() as u32;
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let col: usize = text[start..at].chars().map(char::len_utf16).sum();
    (line, col as u32)
}

/// `(line, UTF-16 column)` of the end of the first `needle` in `text`.
fn pos_after(text: &str, needle: &str) -> (u32, u32) {
    let (line, col) = pos_of(text, needle);
    let units: usize = needle.chars().map(char::len_utf16).sum();
    (line, col + units as u32)
}

/// The byte offset of an LSP position in `text`.
fn offset_of(text: &str, pos: &Value) -> usize {
    let line = pos["line"].as_u64().unwrap() as usize;
    let col = pos["character"].as_u64().unwrap() as usize;
    let start: usize = text.split_inclusive('\n').take(line).map(str::len).sum();
    let mut units = 0;
    for (i, c) in text[start..].char_indices() {
        if units >= col {
            return start + i;
        }
        units += c.len_utf16();
    }
    text.len()
}

/// The text with the edits of a `WorkspaceEdit` for `path` applied.
fn apply_edits(edit: &Value, path: &Path, text: &str) -> String {
    let mut edits: Vec<&Value> = edit["changes"][uri(path)]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let mut out = text.to_string();
    edits.sort_by_key(|e| std::cmp::Reverse(offset_of(text, &e["range"]["start"])));
    for e in edits {
        let (s, t) = (
            offset_of(text, &e["range"]["start"]),
            offset_of(text, &e["range"]["end"]),
        );
        out.replace_range(s..t, e["newText"].as_str().unwrap());
    }
    out
}

fn labels(items: &[Value]) -> Vec<&str> {
    items.iter().map(|i| i["label"].as_str().unwrap()).collect()
}

/// The item with this label, panicking with the labels there are.
fn item<'a>(items: &'a [Value], label: &str) -> &'a Value {
    items
        .iter()
        .find(|i| i["label"] == label)
        .unwrap_or_else(|| panic!("no `{label}` among {:?}", labels(items)))
}

fn range_of(d: &Value) -> ((u64, u64), (u64, u64)) {
    let r = &d["range"];
    (
        (
            r["start"]["line"].as_u64().unwrap(),
            r["start"]["character"].as_u64().unwrap(),
        ),
        (
            r["end"]["line"].as_u64().unwrap(),
            r["end"]["character"].as_u64().unwrap(),
        ),
    )
}

const BROKEN: &str = "fn f(a: str, b: int) {}\n\nfn main() {\n    f(\"é😀 ü\", \"x\")\n}\n";
const FIXED: &str = "fn f(a: str, b: int) {}\n\nfn main() {\n    f(\"é😀 ü\", 1)\n}\n";

#[test]
fn diagnostics_have_utf16_ranges_and_clear_when_fixed() {
    let dir = temp_dir("diag");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, BROKEN);
    let diags = c.diags(&file);
    assert_eq!(diags.len(), 1, "{diags:?}");
    let d = &diags[0];
    assert_eq!(d["severity"], 1);
    assert_eq!(d["source"], "colibri");
    assert!(
        d["message"]
            .as_str()
            .unwrap()
            .contains("expected `int`, found `str`")
    );
    // The literal `"x"` follows non-ASCII characters on its line: é (1 unit), the emoji (2)
    // and ü (1).
    let (line, col) = pos_of(BROKEN, "\"x\"");
    assert_eq!(
        range_of(d),
        ((line as u64, col as u64), (line as u64, col as u64 + 3))
    );
    assert_eq!(
        col, 15,
        "the line has 3 non-ASCII characters of 4 units, 7 bytes more"
    );

    c.change(&file, FIXED);
    assert!(
        c.diagnostics.contains_key(&uri(&file)),
        "an empty list was published"
    );
    assert!(c.diags(&file).is_empty());
    c.shutdown();
}

#[test]
fn syntax_errors_warnings_and_hints() {
    let dir = temp_dir("kinds");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, "fn main() {\n    let x = \n}\n");
    let diags = c.diags(&file);
    assert!(!diags.is_empty());
    assert_eq!(diags[0]["severity"], 1);

    // A shadowing warning has a label (related information) and a hint.
    let text = "fn main() {\n    let a = 1\n    print(a)\n    let a = 2\n    print(a)\n}\n";
    c.change(&file, text);
    let diags = c.diags(&file);
    assert_eq!(diags.len(), 1, "{diags:?}");
    let d = &diags[0];
    assert_eq!(d["severity"], 2);
    assert!(d["message"].as_str().unwrap().contains("\nhint: "), "{d}");
    let related = d["relatedInformation"].as_array().unwrap();
    assert_eq!(related[0]["message"], "the earlier binding");
    assert_eq!(related[0]["location"]["uri"], uri(&file));
    assert_eq!(related[0]["location"]["range"]["start"]["line"], 1);
    c.shutdown();
}

#[test]
fn an_error_in_another_file_of_the_package_is_published_there() {
    let dir = temp_dir("multi");
    let main = write(
        &dir,
        "main.colibri",
        "import lib\n\nfn main() {\n    print(\"{lib.f()}\")\n}\n",
    );
    let lib_text = "pub fn f() -> int {\n    return \"text\"\n}\n";
    let lib = write(&dir, "lib.colibri", lib_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, &std::fs::read_to_string(&main).unwrap());
    assert!(c.diags(&main).is_empty());
    let lib_diags = c.diags(&lib);
    assert_eq!(lib_diags.len(), 1, "{lib_diags:?}");
    assert_eq!(range_of(&lib_diags[0]).0.0, 1);

    // Unsaved edits of a second document override the file on disk.
    c.open(&lib, "pub fn f() -> int {\n    return 1\n}\n");
    assert!(c.diags(&lib).is_empty());
    assert!(c.diags(&main).is_empty());

    // Breaking `lib` in its buffer shows up in the package, not just the document.
    c.change(&lib, "pub fn f() -> int {\n    return true\n}\n");
    assert_eq!(c.diags(&lib).len(), 1);

    // Opening the folder's `main` and changing only the buffer of main also reaches lib.
    c.change(
        &main,
        "import lib\n\nfn main() {\n    print(\"{lib.f()}\")\n    print(nope)\n}\n",
    );
    assert_eq!(c.diags(&main).len(), 1);
    assert_eq!(c.diags(&lib).len(), 1);
    c.shutdown();
}

#[test]
fn a_package_with_a_manifest_is_checked_from_a_module() {
    let dir = temp_dir("manifest");
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    write(
        &dir,
        "main.colibri",
        "import util\n\nfn main() {\n    print(\"{util.g()}\")\n}\n",
    );
    let util = write(
        &dir,
        "util.colibri",
        "pub fn g() -> int {\n    return \"no\"\n}\n",
    );
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&util, &std::fs::read_to_string(&util).unwrap());
    assert_eq!(c.diags(&util).len(), 1);
    c.shutdown();
}

#[test]
fn closing_a_document_clears_its_diagnostics() {
    let dir = temp_dir("close");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, BROKEN);
    assert_eq!(c.diags(&file).len(), 1);
    c.notify(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&file)}}),
    );
    c.sync();
    assert!(c.diags(&file).is_empty());
    c.shutdown();
}

#[test]
fn formatting_returns_one_edit_and_leaves_broken_files_alone() {
    let dir = temp_dir("fmt");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    let messy = "fn main(){\nlet x=1+2\nprint(x)\n}\n";
    c.open(&file, messy);
    let edits = c.request(
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri(&file)}, "options": {"tabSize": 4, "insertSpaces": true}}),
    );
    let edits = edits.as_array().unwrap();
    assert_eq!(edits.len(), 1);
    let expected = colibri_syntax::format_source("a", messy).unwrap();
    assert_eq!(edits[0]["newText"], expected);
    assert_eq!(
        edits[0]["range"]["start"],
        json!({"line": 0, "character": 0})
    );
    assert_eq!(edits[0]["range"]["end"], json!({"line": 4, "character": 0}));

    // Already formatted: no edits.
    c.change(&file, &expected);
    let edits = c.request(
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri(&file)}, "options": {"tabSize": 4, "insertSpaces": true}}),
    );
    assert_eq!(edits, json!([]));

    // A syntax error: nothing to do.
    c.change(&file, "fn main( {\n");
    let edits = c.request(
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri(&file)}, "options": {"tabSize": 4, "insertSpaces": true}}),
    );
    assert_eq!(edits, Value::Null);
    c.shutdown();
}

const PROGRAM: &str = "\
struct Point { x: float, y: float }

fn length(p: Point) -> float {
    return p.x * p.x + p.y * p.y
}

impl Point {
    fn scaled(self, k: float) -> Point => Point { x: self.x * k, y: self.y * k }
}

fn main() {
    let origin = Point { x: 0.0, y: 1.5 }
    let n = length(origin)
    let big = origin.scaled(2.0)
    print(\"é {n} {big.y}\")
}
";

fn hover_text(h: &Value) -> String {
    h["contents"]["value"].as_str().unwrap().to_string()
}

#[test]
fn hover_shows_types_signatures_and_fields() {
    let dir = temp_dir("hover");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, PROGRAM);
    assert!(c.diags(&file).is_empty(), "{:?}", c.diags(&file));

    // A local at its use and its declaration.
    let (l, col) = pos_of(PROGRAM, "origin)\n    let big");
    let h = c.hover(&file, l, col + 1);
    assert_eq!(hover_text(&h), "```colibri\norigin: Point\n```");
    assert_eq!(h["range"]["start"], json!({"line": l, "character": col}));
    let (l, col) = pos_of(PROGRAM, "n = length");
    assert_eq!(
        hover_text(&c.hover(&file, l, col)),
        "```colibri\nn: float\n```"
    );

    // A function at a call, and at its declaration.
    let (l, col) = pos_of(PROGRAM, "length(origin)");
    assert_eq!(
        hover_text(&c.hover(&file, l, col + 2)),
        "```colibri\nfn length(p: Point) -> float\n```"
    );
    let (l, col) = pos_of(PROGRAM, "length(p");
    assert_eq!(
        hover_text(&c.hover(&file, l, col)),
        "```colibri\nfn length(p: Point) -> float\n```"
    );

    // A method call.
    let (l, col) = pos_of(PROGRAM, "scaled(2.0)");
    assert_eq!(
        hover_text(&c.hover(&file, l, col)),
        "```colibri\nfn Point.scaled(self, k: float) -> Point\n```"
    );

    // A field in `p.x`, and the variable in front of it.
    let (l, col) = pos_of(PROGRAM, "x * p.x");
    assert_eq!(
        hover_text(&c.hover(&file, l, col + 6)),
        "```colibri\nx: float\n```"
    );
    assert_eq!(
        hover_text(&c.hover(&file, l, col + 4)),
        "```colibri\np: Point\n```"
    );

    // An expression: the product `p.x * p.x`.
    let (l, col) = pos_of(PROGRAM, "* p.x");
    assert_eq!(
        hover_text(&c.hover(&file, l, col)),
        "```colibri\nfloat\n```"
    );

    // After the non-ASCII `é` of the string: the interpolated `n`.
    let (l, col) = pos_of(PROGRAM, "n} {big");
    assert_eq!(
        hover_text(&c.hover(&file, l, col)),
        "```colibri\nn: float\n```"
    );

    // Nothing to say about whitespace outside any expression.
    assert_eq!(c.hover(&file, 1, 0), Value::Null);
    c.shutdown();
}

#[test]
fn hover_still_works_for_a_program_with_a_type_error_elsewhere() {
    let dir = temp_dir("hover_err");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    let text = "fn main() {\n    let a = 1\n    let b: str = a\n    print(b)\n}\n";
    c.open(&file, text);
    assert_eq!(c.diags(&file).len(), 1);
    let (l, col) = pos_of(text, "a\n    print(b)");
    assert_eq!(
        hover_text(&c.hover(&file, l, col)),
        "```colibri\na: int\n```"
    );
    c.shutdown();
}

fn location(v: &Value) -> (String, u64, u64, u64) {
    (
        v["uri"].as_str().unwrap().to_string(),
        v["range"]["start"]["line"].as_u64().unwrap(),
        v["range"]["start"]["character"].as_u64().unwrap(),
        v["range"]["end"]["character"].as_u64().unwrap(),
    )
}

#[test]
fn definition_finds_locals_functions_methods_fields_and_types() {
    let dir = temp_dir("def");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, PROGRAM);
    let me = uri(&file);
    let at = |needle: &str, delta: u32| {
        let (l, col) = pos_of(PROGRAM, needle);
        (l, col + delta)
    };

    // local
    let (l, col) = at("origin)\n    let big", 1);
    let (l0, c0) = pos_of(PROGRAM, "origin = Point");
    assert_eq!(
        location(&c.definition(&file, l, col)),
        (me.clone(), l0 as u64, c0 as u64, c0 as u64 + 6)
    );
    // function
    let (l, col) = at("length(origin)", 1);
    let (l0, c0) = pos_of(PROGRAM, "length(p");
    assert_eq!(
        location(&c.definition(&file, l, col)),
        (me.clone(), l0 as u64, c0 as u64, c0 as u64 + 6)
    );
    // method
    let (l, col) = at("scaled(2.0)", 1);
    let (l0, c0) = pos_of(PROGRAM, "scaled(self");
    assert_eq!(
        location(&c.definition(&file, l, col)),
        (me.clone(), l0 as u64, c0 as u64, c0 as u64 + 6)
    );
    // field of `p.x`
    let (l, col) = at("x * p.x", 6);
    let (l0, c0) = pos_of(PROGRAM, "x: float");
    assert_eq!(
        location(&c.definition(&file, l, col)),
        (me.clone(), l0 as u64, c0 as u64, c0 as u64 + 1)
    );
    // the type of a struct literal
    let (l, col) = at("Point { x: 0.0", 1);
    let (l0, c0) = pos_of(PROGRAM, "Point {");
    assert_eq!(
        location(&c.definition(&file, l, col)),
        (me.clone(), l0 as u64, c0 as u64, c0 as u64 + 5)
    );
    // a name with no declaration in source
    let (l, col) = at("print(", 1);
    assert_eq!(c.definition(&file, l, col), Value::Null);
    c.shutdown();
}

#[test]
fn definition_and_hover_cross_files() {
    let dir = temp_dir("cross");
    let main_text = "import shapes.{Circle, area}\n\nfn main() {\n    let c = Circle { r: 2 }\n    print(\"{area(c)}\")\n}\n";
    let main = write(&dir, "main.colibri", main_text);
    let shapes = write(
        &dir,
        "shapes.colibri",
        "pub struct Circle { pub r: int }\n\npub fn area(c: Circle) -> int {\n    return c.r * c.r * 3\n}\n",
    );
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, main_text);
    assert!(c.diags(&main).is_empty() && c.diags(&shapes).is_empty());
    let (l, col) = pos_of(main_text, "area(c)");
    assert_eq!(
        hover_text(&c.hover(&main, l, col)),
        "```colibri\nfn shapes.area(c: Circle) -> int\n```"
    );
    let def = c.definition(&main, l, col);
    assert_eq!(location(&def), (uri(&shapes), 2, 7, 11));
    c.shutdown();
}

#[test]
fn interface_files_make_host_functions_check() {
    let dir = temp_dir("iface");
    let mut vm = colibri_runtime::Vm::builder().build();
    vm.register_fn("engine_add", |a: i64, b: i64| a + b)
        .unwrap();
    let cbi = write(&dir, "engine.cbi", &vm.interface_text());
    let file = dir.join("a.colibri");
    let text = "fn main() {\n    print(engine_add(1, 2))\n}\n";

    // Without the interface the host function is unknown.
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, text);
    assert_eq!(c.diags(&file).len(), 1);
    c.shutdown();

    // Named in the initialization options (relative to the workspace folder).
    let mut c = Client::start(Some(&dir), json!({"interfaces": ["engine.cbi"]}));
    c.open(&file, text);
    assert!(c.diags(&file).is_empty(), "{:?}", c.diags(&file));
    let (l, col) = pos_of(text, "engine_add");
    assert!(hover_text(&c.hover(&file, l, col)).contains("fn engine_add(a: int, b: int) -> int"));
    c.shutdown();

    // Found next to a `colibri.toml`.
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    write(&dir, "main.colibri", text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&dir.join("main.colibri"), text);
    assert!(c.diags(&dir.join("main.colibri")).is_empty());
    c.shutdown();

    // A broken interface file is reported in the log, and checking goes on.
    std::fs::write(&cbi, "this is not an interface").unwrap();
    let mut c = Client::start(Some(&dir), json!({"interfaces": ["engine.cbi"]}));
    c.open(&file, text);
    assert!(
        c.logs.iter().any(|l| l.contains("engine.cbi")),
        "{:?}",
        c.logs
    );
    assert_eq!(c.diags(&file).len(), 1);
    c.shutdown();
}

#[test]
fn unknown_requests_get_an_error_and_the_server_survives() {
    let mut c = Client::start(None, Value::Null);
    let err = c
        .request_raw("textDocument/codeAction", json!({}))
        .unwrap_err();
    assert_eq!(err["code"], -32601);
    // Hover in a document that was never opened is just empty.
    let r = c.request(
        "textDocument/hover",
        json!({"textDocument": {"uri": "file:///nowhere/a.colibri"}, "position": {"line": 0, "character": 0}}),
    );
    assert_eq!(r, Value::Null);
    c.shutdown();
}

#[test]
fn the_standard_library_is_checked_and_hovered_like_any_module() {
    let dir = temp_dir("std");
    let file = dir.join("a.colibri");
    let good = "import math\nimport std.json\n\nfn main() {\n    print(math.sqrt(4.0))\n    print(json.encode(1).expect(\"ok\"))\n}\n";
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, good);
    assert!(c.diags(&file).is_empty(), "{:?}", c.diags(&file));
    // A wrong argument type is a diagnostic about the std signature.
    let bad = good.replace("math.sqrt(4.0)", "math.sqrt(\"4\")");
    c.change(&file, &bad);
    let diags = c.diags(&file);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("expected `float`, found `str`"),
        "{diags:?}"
    );
    // A module the library does not have is reported as such.
    c.change(&file, "import std.nothing\n\nfn main() {}\n");
    let diags = c.diags(&file);
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("the standard library has no module"),
        "{diags:?}"
    );
    c.shutdown();
}

#[test]
fn a_type_error_inside_a_test_block_is_a_diagnostic() {
    let dir = temp_dir("test_block");
    let file = dir.join("a.colibri");
    let text = "fn double(n: int) -> int => n * 2\n\ntest \"doubles\" {\n    let x: str = double(2)\n    assert_eq(x, \"4\")\n}\n";
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, text);
    let diags = c.diags(&file);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("expected `str`, found `int`"),
        "{diags:?}"
    );
    let (line, col) = pos_of(text, "double(2)");
    assert_eq!(range_of(&diags[0]).0, (line as u64, col as u64));

    // Hover inside the test body sees the types, and the fix clears the diagnostic.
    let hover = c.hover(&file, line, col);
    assert!(hover.to_string().contains("int"), "{hover}");
    c.change(
        &file,
        &text
            .replace("let x: str", "let x: int")
            .replace("\"4\"", "4"),
    );
    assert!(c.diags(&file).is_empty());
    // An assertion misuse is reported in the test too.
    c.change(&file, "test \"t\" {\n    assert_eq(1, \"one\")\n}\n");
    assert_eq!(c.diags(&file).len(), 1);
    c.shutdown();
}

const FFI_PROGRAM: &str = "\
@repr(C)
struct Point { x: i32, y: i32 }

@link(\"c\")
extern \"C\" {
    fn malloc(n: u64) -> ptr[Point]
    fn free(p: ptr[Point])
}

fn main() {
    let p = malloc(u64(size_of[Point]()))
    unsafe { p.write(Point { x: 1, y: 2 }) }
    free(p)
}
";

#[test]
fn ffi_declarations_check_hover_and_format_in_the_editor() {
    let dir = temp_dir("ffi");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, FFI_PROGRAM);
    assert!(c.diags(&file).is_empty(), "{:?}", c.diags(&file));
    let (l, col) = pos_of(FFI_PROGRAM, "malloc(u64");
    assert_eq!(
        hover_text(&c.hover(&file, l, col + 2)),
        "```colibri\nfn malloc(n: u64) -> ptr[Point]\n```"
    );
    // Already formatted: the formatter keeps attributes, `extern` blocks and `unsafe`.
    let edits = c.request(
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri(&file)}, "options": {"tabSize": 4, "insertSpaces": true}}),
    );
    assert_eq!(edits, json!([]));
    // Reading through a pointer outside `unsafe` is a diagnostic at the call.
    let unsafe_less = FFI_PROGRAM.replace(
        "unsafe { p.write(Point { x: 1, y: 2 }) }",
        "p.write(Point { x: 1, y: 2 })",
    );
    c.change(&file, &unsafe_less);
    let diags = c.diags(&file);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("needs an `unsafe` block")
    );
    c.shutdown();
}

#[test]
fn unused_variables_are_published_as_warnings() {
    let dir = temp_dir("unused");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, "fn main() {\n    let spare = 1\n}\n");
    let diags = c.diags(&file);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0]["severity"], 2);
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("unused variable `spare`")
    );
    // Editors fade unused code (`DiagnosticTag::Unnecessary` = 1).
    assert_eq!(diags[0]["tags"], serde_json::json!([1]));
    c.shutdown();
}

const OUTLINE: &str = "\
/// A point.
pub struct Point { x: float, pub y: float }
class Node { next: Node?, parent: weak Node }
enum Shape {
    Circle(radius: float)
    Rect(float, float)
    Empty
}
interface Area {
    fn area(self) -> float
}
impl Point {
    fn new(x: float, y: float) -> Point => Point { x, y }
    fn norm(self) -> float => self.x
}
impl Area for Shape {
    fn area(self) -> float => 0.0
}
type Pair = (int, int)
let limit = 10
var counter: int = 0
const NAME = \"colibri\"
let (lo, hi) = (1, 2)
fn main() {
    let local = 1
}
test \"adds up\" {
    assert_eq(1 + 1, 2)
}
";

/// `(name, kind, detail, children)` of a symbol, children recursively.
fn outline(v: &Value) -> Vec<(String, u64, Option<String>, Vec<Value>)> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap().to_string(),
                s["kind"].as_u64().unwrap(),
                s["detail"].as_str().map(str::to_string),
                s["children"].as_array().cloned().unwrap_or_default(),
            )
        })
        .collect()
}

#[test]
fn the_outline_lists_declarations_with_their_members() {
    let dir = temp_dir("outline");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    assert_eq!(c.init["capabilities"]["documentSymbolProvider"], true);
    c.open(&file, OUTLINE);
    let symbols = c.document_symbols(&file);
    let top = outline(&symbols);
    let names: Vec<(&str, u64)> = top.iter().map(|(n, k, _, _)| (n.as_str(), *k)).collect();
    // LSP kinds: 5 class, 6 method, 8 field, 10 enum, 11 interface, 12 function, 13 variable,
    // 14 constant, 22 enum member, 23 struct, 19 object.
    assert_eq!(
        names,
        [
            ("Point", 23),
            ("Node", 5),
            ("Shape", 10),
            ("Area", 11),
            ("impl Point", 19),
            ("impl Area for Shape", 19),
            ("Pair", 5),
            ("limit", 13),
            ("counter", 13),
            ("NAME", 14),
            ("lo", 13),
            ("hi", 13),
            ("main", 12),
            ("test \"adds up\"", 12),
        ]
    );
    let children = |i: usize| -> Vec<(String, u64, Option<String>)> {
        top[i]
            .3
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().to_string(),
                    s["kind"].as_u64().unwrap(),
                    s["detail"].as_str().map(str::to_string),
                )
            })
            .collect()
    };
    assert_eq!(
        children(0),
        [
            ("x".to_string(), 8, Some("float".to_string())),
            ("y".to_string(), 8, Some("float".to_string()))
        ]
    );
    assert_eq!(children(1).len(), 2);
    assert_eq!(
        children(2),
        [
            (
                "Circle".to_string(),
                22,
                Some("(radius: float)".to_string())
            ),
            ("Rect".to_string(), 22, Some("(float, float)".to_string())),
            ("Empty".to_string(), 22, None),
        ]
    );
    assert_eq!(
        children(3),
        [("area".to_string(), 6, Some("(self) -> float".to_string()))]
    );
    assert_eq!(
        children(4),
        [
            (
                "new".to_string(),
                12,
                Some("(x: float, y: float) -> Point".to_string())
            ),
            ("norm".to_string(), 6, Some("(self) -> float".to_string())),
        ]
    );
    assert_eq!(children(5).len(), 1);
    assert_eq!(top[8].2.as_deref(), Some("int"));

    // Ranges: a symbol's selection is its name, inside its range.
    let list = symbols.as_array().unwrap();
    let main = &list[12];
    let (l, col) = pos_of(OUTLINE, "fn main");
    assert_eq!(main["range"]["start"], json!({"line": l, "character": col}));
    assert_eq!(main["range"]["end"]["line"], l + 2);
    assert_eq!(
        main["selectionRange"]["start"],
        json!({"line": l, "character": col + 3})
    );
    assert_eq!(
        main["selectionRange"]["end"],
        json!({"line": l, "character": col + 7})
    );
    let test = &list[13];
    let (l, _) = pos_of(OUTLINE, "test \"adds");
    assert_eq!(test["range"]["start"]["line"], l);
    c.shutdown();
}

#[test]
fn the_outline_works_while_the_file_does_not_parse_or_check() {
    let dir = temp_dir("outline_broken");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    // A type error: the outline is the same.
    c.open(&file, "fn a() -> int { return \"x\" }\nfn b() {}\n");
    assert!(!c.diags(&file).is_empty());
    let top = outline(&c.document_symbols(&file));
    assert_eq!(top.len(), 2);
    // A syntax error inside a body: declarations before and after it are listed.
    c.change(
        &file,
        "fn a() {}\nfn broken() {\n    let x = \n}\nstruct S { f: int }\nfn z() {}\n",
    );
    let names: Vec<String> = outline(&c.document_symbols(&file))
        .into_iter()
        .map(|s| s.0)
        .collect();
    assert!(names.contains(&"a".to_string()), "{names:?}");
    assert!(names.contains(&"z".to_string()), "{names:?}");
    assert!(names.contains(&"S".to_string()), "{names:?}");
    // An empty file has an empty outline.
    c.change(&file, "");
    assert_eq!(c.document_symbols(&file), json!([]));
    c.shutdown();
}

const SHAPES: &str = "\
struct Point { x: float, pub y: float }

impl Point {
    fn scaled(self, k: float) -> Point => Point { x: self.x * k, y: self.y * k }
    fn origin() -> Point => Point { x: 0.0, y: 0.0 }
}

enum Shape {
    Circle(radius: float)
    Square(float)
    Empty
}

interface Named {
    fn name(self) -> str
}

impl Named for Point {
    fn name(self) -> str => \"point\"
}

fn describe(s: Shape) -> int {
    return 1
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
    let words = [\"a\", \"b\"]
    let name = \"colibri\"
    print(\"{p.x} {words.len()} {name.len()}\")
}
";

#[test]
fn completion_after_a_dot_lists_the_members_of_the_receiver() {
    let dir = temp_dir("complete_members");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    assert_eq!(
        c.init["capabilities"]["completionProvider"]["triggerCharacters"],
        json!(["."])
    );
    c.open(&file, SHAPES);
    assert!(c.diags(&file).is_empty(), "{:?}", c.diags(&file));

    // The user types `p.` in the middle of `main`: the line does not parse.
    let typing = SHAPES.replace("    print(", "    p.\n    print(");
    c.change(&file, &typing);
    assert!(!c.diags(&file).is_empty(), "the buffer does not parse");
    let items = c.complete_after(&file, &typing, "    p.");
    let names = labels(&items);
    for want in ["x", "y", "scaled", "name", "clone", "eq"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    // `origin` has no receiver: not a method of a value.
    assert!(!names.contains(&"origin"), "{names:?}");
    assert_eq!(item(&items, "x")["detail"], "x: float");
    assert_eq!(item(&items, "x")["kind"], 5, "a field");
    assert_eq!(
        item(&items, "scaled")["detail"],
        "fn scaled(k: float) -> Point"
    );
    assert_eq!(item(&items, "scaled")["kind"], 2, "a method");
    assert_eq!(item(&items, "name")["detail"], "fn name() -> str");

    // With part of the member written, and the cursor in the middle of a word.
    let typing = SHAPES.replace("    print(", "    p.sca\n    print(");
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "    p.sca");
    assert!(labels(&items).contains(&"scaled"));
    let items = c.complete_after(&file, &typing, "    p.s");
    assert!(labels(&items).contains(&"scaled"));

    // After a call and a field: `words.len().` is an int, `p.scaled(2.0).` a Point.
    let typing = SHAPES.replace("    print(", "    p.scaled(2.0).\n    print(");
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "p.scaled(2.0).");
    assert!(labels(&items).contains(&"scaled"));
    let typing = SHAPES.replace("    print(", "    words.len().\n    print(");
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "words.len().");
    let names = labels(&items);
    assert!(
        names.contains(&"pow") && names.contains(&"checked_add"),
        "{names:?}"
    );
    // Inside an interpolation, in the middle of a line.
    let typing = SHAPES.replace("{p.x}", "{p.}");
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "{p.");
    assert!(labels(&items).contains(&"y"));
    c.shutdown();
}

#[test]
fn completion_knows_the_built_in_types() {
    let dir = temp_dir("complete_builtin");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    let program = "\
fn parse(s: str) -> Result[int] {
    return ok(1)
}

fn main() {
    let s = \"abc\"
    let xs = [1, 2, 3]
    var m = {\"a\": 1}
    var set = Set[int]()
    let o = some(1)
    let r = parse(\"1\")
    let mut_buf = StrBuilder()
    let f = 1.5
    let i = 3
    print(\"{s} {xs} {m} {set} {o} {r} {mut_buf} {f} {i}\")
}
";
    c.open(&file, program);
    assert!(c.diags(&file).is_empty(), "{:?}", c.diags(&file));
    let cases: [(&str, &[&str]); 8] = [
        ("s", &["len", "split", "trim", "contains", "to_upper"]),
        ("xs", &["push", "pop", "map", "filter", "len", "sort"]),
        ("m", &["get", "insert", "keys", "contains_key"]),
        ("set", &["add", "union", "contains"]),
        ("o", &["is_some", "is_none", "map", "expect"]),
        ("r", &["is_ok", "ok", "err", "expect"]),
        ("mut_buf", &["push", "to_str", "len"]),
        ("f", &["sqrt", "floor", "abs", "is_nan"]),
    ];
    for (name, expected) in cases {
        let line = format!("    {name}.\n    print(");
        let typing = program.replace("    print(", &line);
        c.change(&file, &typing);
        let items = c.complete_after(&file, &typing, &format!("    {name}."));
        let names = labels(&items);
        for want in expected {
            assert!(
                names.contains(want),
                "{name}: {want} missing from {names:?}"
            );
        }
        assert!(names.contains(&"clone"), "{name}: {names:?}");
    }
    // The signature is the detail, with the element type filled in.
    let typing = program.replace("    print(", "    xs.\n    print(");
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "    xs.");
    assert_eq!(item(&items, "push")["detail"], "fn push(item: int)");
    assert_eq!(item(&items, "len")["detail"], "fn len() -> int");
    assert_eq!(
        item(&items, "map")["detail"],
        "fn map(f: fn(int) -> U) -> List[U]"
    );
    let typing = program.replace("    print(", "    s.\n    print(");
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "    s.");
    assert_eq!(
        item(&items, "split")["detail"],
        "fn split(sep: str) -> List[str]"
    );
    // An integer has integer methods, not float ones.
    let typing = program.replace("    print(", "    i.\n    print(");
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "    i.");
    assert!(labels(&items).contains(&"pow"));
    assert!(!labels(&items).contains(&"sqrt"));
    c.shutdown();
}

#[test]
fn completion_after_a_module_or_a_type_name() {
    let dir = temp_dir("complete_paths");
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    write(
        &dir,
        "util.colibri",
        "pub fn double(x: int) -> int => x * 2\nfn secret() {}\npub struct Box { v: int }\npub const LIMIT = 10\n",
    );
    let main_text = "import util\nimport math\n\nenum Shape {\n    Circle(float)\n    Empty\n}\n\nimpl Shape {\n    fn unit() -> Shape => Shape.Empty\n    fn area(self) -> float => 0.0\n}\n\nfn main() {\n    HERE\n    print(1)\n}\n";
    let main = write(&dir, "main.colibri", main_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    let typing = |what: &str| main_text.replace("HERE", what);
    c.open(&main, &typing("let a = 1"));

    // An imported module of the package: everything the package can see.
    let main_text = typing("util.");
    c.change(&main, &main_text);
    let items = c.complete_after(&main, &main_text, "    util.");
    let names = labels(&items);
    for want in ["double", "secret", "Box", "LIMIT"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    assert_eq!(item(&items, "double")["detail"], "fn double(x: int) -> int");
    assert_eq!(item(&items, "double")["kind"], 3, "a function");
    assert_eq!(item(&items, "Box")["kind"], 22, "a struct");

    // A standard library module.
    let main_text = typing("math.");
    c.change(&main, &main_text);
    let items = c.complete_after(&main, &main_text, "    math.");
    let names = labels(&items);
    assert!(
        names.contains(&"sqrt") && names.contains(&"floor"),
        "{names:?}"
    );
    assert!(!names.contains(&"double"));

    // A type: its variants and associated functions, not its methods.
    let main_text = typing("Shape.");
    c.change(&main, &main_text);
    let items = c.complete_after(&main, &main_text, "    Shape.");
    let names = labels(&items);
    assert_eq!(names.len(), 3, "{names:?}");
    for want in ["Circle", "Empty", "unit"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    assert_eq!(item(&items, "Circle")["kind"], 20, "an enum member");
    c.shutdown();
}

#[test]
fn completion_of_names_follows_the_scope_at_the_cursor() {
    let dir = temp_dir("complete_names");
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    write(
        &dir,
        "util.colibri",
        "pub fn double(x: int) -> int => x * 2\npub struct Box { v: int }\n",
    );
    let text = "import util.{double, Box}\nimport math\n\nstruct Point { x: int }\nlet LIMIT = 3\n\nfn helper(count: int) -> int {\n    let before = 1\n    if count > 0 {\n        let inner = 2\n    }\n    pri\n    let after = 3\n    return before\n}\n\nfn other() {}\n";
    let main = write(&dir, "main.colibri", text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, text);
    let items = c.complete_after(&main, text, "    pri");
    let names = labels(&items);
    // Parameters and the locals declared before the cursor, in this scope.
    for want in ["count", "before"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    for not in ["inner", "after"] {
        assert!(!names.contains(&not), "{not} should not be in {names:?}");
    }
    // Items of the module, imports (names and modules), the prelude and keywords.
    for want in [
        "helper",
        "other",
        "Point",
        "LIMIT",
        "double",
        "Box",
        "math",
        "print",
        "assert_eq",
        "List",
        "Option",
        "Show",
        "int",
        "match",
        "return",
        "while",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    assert_eq!(item(&items, "count")["kind"], 6, "a variable");
    assert_eq!(item(&items, "count")["detail"], "count: int");
    assert_eq!(item(&items, "helper")["kind"], 3);
    assert_eq!(
        item(&items, "helper")["detail"],
        "fn helper(count: int) -> int"
    );
    assert_eq!(item(&items, "match")["kind"], 14, "a keyword");
    // A name is listed once; locals sort before the rest.
    assert_eq!(names.iter().filter(|n| **n == "print").count(), 1);
    assert!(
        item(&items, "count")["sortText"].as_str().unwrap()
            < item(&items, "helper")["sortText"].as_str().unwrap()
    );
    // Not offered from the other function: nothing of `helper`'s scope.
    let in_other = text.replace("fn other() {}", "fn other() {\n    pri\n}");
    c.change(&main, &in_other);
    let items = c.complete_after(&main, &in_other, "    pri\n}");
    let names = labels(&items);
    assert!(
        !names.contains(&"count") && !names.contains(&"before"),
        "{names:?}"
    );
    assert!(names.contains(&"helper"));
    c.shutdown();
}

#[test]
fn completion_offers_nothing_in_strings_and_comments() {
    let dir = temp_dir("complete_none");
    let file = dir.join("a.colibri");
    let text = "fn main() {\n    let s = \"hello wor\"\n    // a comm\n    let n = 1.5\n}\n";
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, text);
    assert!(c.complete_after(&file, text, "hello wor").is_empty());
    assert!(c.complete_after(&file, text, "a comm").is_empty());
    assert!(c.complete_after(&file, text, "1.").is_empty());
    assert!(!c.complete_after(&file, text, "let n").is_empty());
    // A document that is not open has nothing, and no error.
    let r = c.request(
        "textDocument/completion",
        json!({"textDocument": {"uri": "file:///nowhere/a.colibri"}, "position": {"line": 0, "character": 0}}),
    );
    assert_eq!(r, Value::Null);
    c.shutdown();
}

#[test]
fn completion_falls_back_to_the_last_good_program() {
    let dir = temp_dir("complete_good");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, SHAPES);
    assert!(c.diags(&file).is_empty());
    // A syntax error elsewhere keeps the repaired program from loading, as does the typing
    // itself: the receiver, a local, still has the type it had.
    let typing = SHAPES
        .replace("    let name = ", "    let = 4\n    let name = ")
        .replace("    print(", "    p.\n    print(");
    c.change(&file, &typing);
    let errors = c.diags(&file);
    assert!(!errors.is_empty());
    let items = c.complete_after(&file, &typing, "    p.");
    let names = labels(&items);
    assert!(
        names.contains(&"scaled") && names.contains(&"x"),
        "{names:?}"
    );
    // The same for a parameter, in another function.
    let typing = SHAPES
        .replace("    let name = ", "    let = 4\n    let name = ")
        .replace("    return 1", "    s.\n    return 1");
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "    s.");
    // `Shape` has no fields or methods of its own, but it has `clone` and `eq`.
    assert!(labels(&items).contains(&"clone"), "{:?}", labels(&items));
    // A receiver that is not a name and is new text has no known type.
    let typing = SHAPES
        .replace("    let name = ", "    let = 4\n    let name = ")
        .replace("    print(", "    words.len().\n    print(");
    c.change(&file, &typing);
    assert!(c.complete_after(&file, &typing, "words.len().").is_empty());
    c.shutdown();
}

#[test]
fn completion_offers_the_variants_of_the_matched_enum() {
    let dir = temp_dir("complete_variants");
    let file = dir.join("a.colibri");
    let program = "\
enum Shape {
    Circle(float)
    Square(float)
    Empty
}

fn area(s: Shape) -> float {
    return match s {
        Circle(r) => r
        Squ
        Empty => 0.0
    }
}
";
    let good = program.replace("        Squ\n", "        Square(w) => w\n");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, &good);
    assert!(c.diags(&file).is_empty(), "{:?}", c.diags(&file));
    // The first arm is being typed: `S|` between the braces.
    let typing = good.replace(
        "        Circle(r) => r\n",
        "        S\n        Circle(r) => r\n",
    );
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "        S");
    let names = labels(&items);
    for want in ["Circle", "Square", "Empty"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    assert_eq!(item(&items, "Circle")["kind"], 20);
    // After a finished arm, and after a dot.
    let typing = good.replace(
        "        Square(w) => w\n",
        "        .\n        Square(w) => w\n",
    );
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "        .");
    let names = labels(&items);
    for want in ["Circle", "Square", "Empty", "sqrt"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    // A capitalised name after the dot can only be a variant.
    let typing = good.replace(
        "        Square(w) => w\n",
        "        .S\n        Square(w) => w\n",
    );
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "        .S");
    assert_eq!(labels(&items).len(), 3, "{:?}", labels(&items));
    // In the body of an arm it is an ordinary name.
    let typing = good.replace("Square(w) => w", "Square(w) => w + S");
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "w + S");
    let names = labels(&items);
    assert!(
        !names.contains(&"Circle") && names.contains(&"w"),
        "{names:?}"
    );
    c.shutdown();
}

const RENAMING: &str = "\
import util
import util.{helper}

struct Point { x: float, y: float }

impl Point {
    fn scaled(self, k: float) -> Point => Point { x: self.x * k, y: self.y * k }
    fn len2(self) -> float => self.x * self.x + self.y * self.y
}

enum Shape {
    Circle(float)
    Empty
}

interface Named {
    fn name(self) -> str
}

impl Named for Point {
    fn name(self) -> str => \"name of a point\"
}

let SCALE = 2.0

fn area(s: Shape, factor: float) -> float {
    // the factor of the shape
    return match s {
        .Circle(r) => r * factor * SCALE
        Empty => 0.0
    }
}

fn main() {
    let count = 3
    var p = Point { x: 1.0, y: 2.0 }
    p.x = 4.0
    let x = 5.0
    let q = Point { x, y: 1.0 }
    print(\"count is {count} and {p.scaled(2.0).len2()} {q.x}\")
    print(area(.Circle(1.0), factor: 2.0))
    print(\"{util.double(count)} {helper(1)} {p.name()}\")
    let Point { x: px, y } = q
    print(\"{px} {y} {x}\")
}
";
const UTIL: &str = "pub fn double(x: int) -> int => x * 2\npub fn helper(n: int) -> int => n + 1\n";

fn renaming_project(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = temp_dir(name);
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    let util = write(&dir, "util.colibri", UTIL);
    let main = write(&dir, "main.colibri", RENAMING);
    (dir, main, util)
}

/// The renamed text of `main` for a rename at the first `needle` (plus `into` characters).
fn renamed(
    c: &mut Client,
    main: &Path,
    needle: &str,
    into: u32,
    new: &str,
) -> Result<String, String> {
    let (l, col) = pos_of(RENAMING, needle);
    let edit = c
        .rename(main, (l, col + into), new)
        .map_err(|e| e["message"].as_str().unwrap().to_string())?;
    Ok(apply_edits(&edit, main, RENAMING))
}

#[test]
fn rename_and_references_of_locals_and_parameters() {
    let (dir, main, _) = renaming_project("rename_locals");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, RENAMING);
    assert!(
        c.diags(&main).iter().all(|d| d["severity"] != 1),
        "{:?}",
        c.diags(&main)
    );
    assert_eq!(c.init["capabilities"]["referencesProvider"], true);
    assert_eq!(
        c.init["capabilities"]["renameProvider"]["prepareProvider"],
        true
    );

    // `count`: its declaration and the two uses, one inside a string's interpolation.
    let at = pos_of(RENAMING, "count = 3");
    let refs = c.references(&main, at, true);
    assert_eq!(refs.len(), 3, "{refs:?}");
    assert_eq!(c.references(&main, at, false).len(), 2);
    // From a use, the same.
    let use_at = pos_of(RENAMING, "count}");
    assert_eq!(c.references(&main, use_at, true), refs);
    let text = renamed(&mut c, &main, "count = 3", 0, "total").unwrap();
    assert_eq!(
        text,
        RENAMING
            .replace("let count = 3", "let total = 3")
            .replace("count is {count}", "count is {total}")
            .replace("double(count)", "double(total)")
    );
    // The word in the string and in the comment is not touched.
    assert!(text.contains("count is {total}"));

    // A parameter, with the named argument of a call.
    let text = renamed(&mut c, &main, "factor: float", 0, "scale").unwrap();
    assert_eq!(
        text,
        RENAMING
            .replace("factor: float", "scale: float")
            .replace("r * factor *", "r * scale *")
            .replace("factor: 2.0", "scale: 2.0")
    );
    assert!(text.contains("// the factor of the shape"));
    // And from the named argument.
    let text2 = renamed(&mut c, &main, "factor: 2.0", 0, "scale").unwrap();
    assert_eq!(text, text2);

    // prepareRename gives the range of the name and the name as the placeholder.
    let (l, col) = pos_of(RENAMING, "count = 3");
    let prepared = c.prepare_rename(&main, (l, col + 2)).unwrap();
    assert_eq!(prepared["placeholder"], "count");
    assert_eq!(
        prepared["range"],
        json!({"start": {"line": l, "character": col}, "end": {"line": l, "character": col + 5}})
    );
    c.shutdown();
}

#[test]
fn rename_functions_types_and_module_values_across_files() {
    let (dir, main, util) = renaming_project("rename_items");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, RENAMING);
    // `helper` is declared in util and used through an import of the name.
    let refs = c.references(&main, pos_of(RENAMING, "helper(1)"), true);
    assert_eq!(refs.len(), 3, "{refs:?}");
    let in_util: Vec<&Value> = refs.iter().filter(|r| r["uri"] == uri(&util)).collect();
    assert_eq!(in_util.len(), 1);
    let edit = c
        .rename(&main, pos_of(RENAMING, "helper(1)"), "assist")
        .unwrap();
    assert_eq!(
        apply_edits(&edit, &main, RENAMING),
        RENAMING.replace("helper", "assist")
    );
    assert_eq!(
        apply_edits(&edit, &util, UTIL),
        UTIL.replace("helper", "assist")
    );
    // `double`, used as `util.double`.
    let edit = c
        .rename(&main, pos_of(RENAMING, "double(count"), "twice")
        .unwrap();
    assert_eq!(
        apply_edits(&edit, &main, RENAMING),
        RENAMING.replace("util.double", "util.twice")
    );
    assert_eq!(
        apply_edits(&edit, &util, UTIL),
        UTIL.replace("double", "twice")
    );
    // From its declaration in the other file, with only that file open.
    c.open(&util, UTIL);
    let edit = c.rename(&util, pos_of(UTIL, "double"), "twice").unwrap();
    assert_eq!(
        apply_edits(&edit, &main, RENAMING),
        RENAMING.replace("util.double", "util.twice")
    );

    // A type: annotations, literals, impl blocks and constructors all change.
    let text = renamed(&mut c, &main, "Point {", 0, "Vec").unwrap();
    assert_eq!(
        text,
        RENAMING
            .replace("Point", "Vec")
            .replace("name of a vec", "name of a point")
    );
    // A module-level binding, with the same capital letter.
    let text = renamed(&mut c, &main, "SCALE", 0, "FACTOR").unwrap();
    assert_eq!(text, RENAMING.replace("SCALE", "FACTOR"));
    let err = renamed(&mut c, &main, "SCALE", 0, "factor").unwrap_err();
    assert!(err.contains("capital"), "{err}");
    c.shutdown();
}

#[test]
fn rename_fields_methods_and_variants() {
    let (dir, main, _) = renaming_project("rename_members");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, RENAMING);
    // A field: the declaration, `self.x`, a write `p.x = ..`, `q.x`, the literal fields,
    // the shorthand `Point { x, y }` and the pattern `Point { x: px }`.
    let text = renamed(&mut c, &main, "x: float, y: float", 0, "left").unwrap();
    let expected = RENAMING
        .replace("Point { x: float", "Point { left: float")
        .replace("Point { x: self.x * k", "Point { left: self.left * k")
        .replace("self.x * self.x", "self.left * self.left")
        .replace("Point { x: 1.0", "Point { left: 1.0")
        .replace("p.x = 4.0", "p.left = 4.0")
        .replace("Point { x, y: 1.0 }", "Point { left: x, y: 1.0 }")
        .replace("{q.x}", "{q.left}")
        .replace("Point { x: px, y }", "Point { left: px, y }");
    assert_eq!(text, expected);
    // A field from a use.
    let again = renamed(&mut c, &main, "x * k", 0, "left").unwrap();
    assert_eq!(again, expected);

    // A local that is also written as a shorthand field keeps the field's name.
    let text = renamed(&mut c, &main, "x = 5.0", 0, "wide").unwrap();
    assert_eq!(
        text,
        RENAMING
            .replace("let x = 5.0", "let wide = 5.0")
            .replace("Point { x, y: 1.0 }", "Point { x: wide, y: 1.0 }")
            .replace("{px} {y} {x}", "{px} {y} {wide}")
    );

    // A method: the declaration and the call in an interpolation.
    let text = renamed(&mut c, &main, "scaled(self", 0, "stretched").unwrap();
    assert_eq!(text, RENAMING.replace("scaled", "stretched"));
    // An interface method: the declaration, the implementation and the call.
    let text = renamed(&mut c, &main, "name(self) -> str\n}", 0, "title").unwrap();
    assert_eq!(
        text,
        RENAMING
            .replace("fn name(self)", "fn title(self)")
            .replace("p.name()", "p.title()")
    );
    // Starting from the call.
    let from_call = renamed(&mut c, &main, "name()", 0, "title").unwrap();
    assert_eq!(from_call, text);

    // A variant: the declaration, `.Circle(..)` in a pattern and in an expression, and
    // the bare `Empty` pattern.
    let text = renamed(&mut c, &main, "Circle(float)", 0, "Disc").unwrap();
    assert_eq!(text, RENAMING.replace("Circle", "Disc"));
    let text = renamed(&mut c, &main, "Empty", 0, "Nothing").unwrap();
    assert_eq!(text, RENAMING.replace("Empty", "Nothing"));
    c.shutdown();
}

#[test]
fn rename_refuses_bad_names_and_names_outside_the_package() {
    let (dir, main, _) = renaming_project("rename_refuse");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, RENAMING);
    let count = pos_of(RENAMING, "count = 3");
    for (bad, why) in [
        ("fn", "keyword"),
        ("match", "keyword"),
        ("true", "keyword"),
        ("self", "keyword"),
        ("", "empty"),
        ("1x", "not a valid name"),
        ("a b", "not a valid name"),
        ("x-y", "not a valid name"),
        ("x.y", "not a valid name"),
        ("\"x\"", "not a valid name"),
        ("_", "not a valid name"),
        ("Count", "capital"),
    ] {
        let err = c.rename(&main, count, bad).unwrap_err();
        let message = err["message"].as_str().unwrap();
        assert!(message.contains(why), "{bad:?}: {message}");
        assert_eq!(err["code"], -32803, "a refusal, not an invalid request");
    }
    // A type cannot lose its capital letter.
    let err = c
        .rename(&main, pos_of(RENAMING, "Point {"), "point")
        .unwrap_err();
    assert!(err["message"].as_str().unwrap().contains("capital"));
    // Nothing is edited when the name does not change.
    let same = c.rename(&main, count, "count").unwrap();
    assert!(same["changes"].is_null() || same["changes"] == json!({}));

    // Nowhere to rename: white space and a string.
    assert!(c.prepare_rename(&main, (3, 0)).is_err());
    let (l, col) = pos_of(RENAMING, "name of a point");
    assert!(c.prepare_rename(&main, (l, col + 2)).is_err());
    assert!(c.rename(&main, (l, col + 2), "other").is_err());
    c.shutdown();
}

#[test]
fn rename_refuses_the_prelude_the_standard_library_and_dependencies() {
    let dir = temp_dir("rename_outside");
    let dep = dir.join("dep");
    write(&dep, "colibri.toml", "[package]\nname = \"dep\"\n");
    write(
        &dep,
        "lib.colibri",
        "pub fn shout(s: str) -> str => s.to_upper()\n",
    );
    let app = dir.join("app");
    write(
        &app,
        "colibri.toml",
        "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
    );
    let text = "import math\nimport dep\n\nfn main() {\n    let xs: List[int] = [1, 2]\n    let word = \"a\"\n    print(\"{math.sqrt(4.0)} {xs.len()} {dep.shout(word)}\")\n    let n = xs.len() + 1\n    print(n)\n}\n";
    let main = write(&app, "main.colibri", text);
    let mut c = Client::start(Some(&app), Value::Null);
    c.open(&main, text);
    assert!(
        c.diags(&main).iter().all(|d| d["severity"] != 1),
        "{:?}",
        c.diags(&main)
    );
    for (needle, what) in [
        ("print(\"{math", "the prelude"),
        ("sqrt(4.0)", "the standard library"),
        ("List[int]", "the prelude type"),
        ("shout(", "a dependency"),
    ] {
        let at = pos_of(text, needle);
        let err = c.rename(&main, at, "other").unwrap_err();
        let message = err["message"].as_str().unwrap();
        assert!(
            message.contains("outside") || message.contains("not declared in this package"),
            "{what}: {message}"
        );
        assert!(c.prepare_rename(&main, at).is_err(), "{what}");
    }
    // A local of the same program still renames.
    let at = pos_of(text, "n = xs");
    assert!(c.rename(&main, at, "total").is_ok());
    c.shutdown();
}

#[test]
fn rename_refuses_a_name_that_clashes_and_a_program_it_cannot_follow() {
    let (dir, main, _) = renaming_project("rename_clash");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, RENAMING);
    // `q` is declared after `count` and before its uses in the string: `count` would be
    // captured by it.
    let err = renamed(&mut c, &main, "count = 3", 0, "q").unwrap_err();
    assert!(err.contains("break the program"), "{err}");
    // A clash that still type checks: the new name would capture the other variable's use.
    let text = "fn main() {\n    let a = 1\n    let b = 2\n    print(\"{a} {b}\")\n}\n";
    let small = dir.join("small.colibri");
    std::fs::write(&small, text).unwrap();
    c.open(&small, text);
    let err = c.rename(&small, pos_of(text, "a = 1"), "b").unwrap_err();
    let message = err["message"].as_str().unwrap();
    assert!(message.contains("clashes with another name"), "{message}");
    assert!(c.rename(&small, pos_of(text, "a = 1"), "c").is_ok());
    // Two functions of one name is an error.
    let err = renamed(&mut c, &main, "area(s", 0, "main").unwrap_err();
    assert!(
        err.contains("break") || err.contains("change what"),
        "{err}"
    );

    // A field cannot be renamed where a use of one could not be resolved.
    let text = "struct P { x: int }\n\nfn main() {\n    let p = P { x: 1 }\n    let w = unknown()\n    print(\"{p.x} {w.x}\")\n}\n";
    let file = dir.join("broken.colibri");
    std::fs::write(&file, text).unwrap();
    c.open(&file, text);
    let err = c.rename(&file, pos_of(text, "x: int"), "left").unwrap_err();
    let message = err["message"].as_str().unwrap();
    assert!(message.contains("could not be resolved"), "{message}");
    // Fix the program, and it renames.
    let fixed = text.replace("w.x", "p.x").replace("let w = unknown()", "");
    c.change(&file, &fixed);
    assert!(c.rename(&file, pos_of(&fixed, "x: int"), "left").is_ok());
    c.shutdown();
}

#[test]
fn rename_is_refused_while_a_file_of_the_package_has_a_syntax_error() {
    let dir = temp_dir("rename_syntax_error");
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    let broken = "pub fn helper(x: int) -> int => x + 1\npub fn other() -> int {\n    let a = helper(2) +\n    let b = helper(3)\n    return a + b + helper(4)\n}\nfn third() -> int {\n    return helper(5) )\n}\n";
    let util = write(&dir, "util.colibri", broken);
    let main_text = "import util\nfn main() {\n    print(util.helper(1))\n}\n";
    let main = write(&dir, "main.colibri", main_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, main_text);
    let at = pos_of(main_text, "helper(1)");
    for err in [
        c.rename(&main, at, "assist").unwrap_err(),
        c.prepare_rename(&main, at).unwrap_err(),
    ] {
        let message = err["message"].as_str().unwrap();
        assert!(message.contains("util.colibri"), "{message}");
    }
    // References stay best effort.
    assert!(!c.references(&main, at, true).is_empty());
    // Fixing the file (an open buffer) makes it work again.
    let fixed = "pub fn helper(x: int) -> int => x + 1\npub fn other() -> int {\n    let a = helper(2)\n    let b = helper(3)\n    return a + b + helper(4)\n}\nfn third() -> int {\n    return helper(5)\n}\n";
    c.open(&util, fixed);
    let edit = c.rename(&main, at, "assist").expect("renames");
    let renamed = apply_edits(&edit, &util, fixed);
    assert_eq!(renamed.matches("assist").count(), 5, "{renamed}");
    assert!(!renamed.contains("helper"));
    c.shutdown();
}

#[test]
fn references_and_rename_cover_every_module_of_the_package() {
    let dir = temp_dir("package_wide");
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    let main_text = "import util\nfn main() {\n    print(util.f(1))\n}\n";
    let util_text = "pub fn f(x: int) -> int => x\n";
    let extra_text = "import util\npub fn g() -> int => util.f(2)\n";
    let deep_text = "import util\npub fn h() -> int => util.f(3)\n";
    // A script: top-level statements, imported by nothing.
    let script_text = "import util\nprint(util.f(4))\n";
    let main = write(&dir, "main.colibri", main_text);
    let util = write(&dir, "util.colibri", util_text);
    let extra = write(&dir, "extra.colibri", extra_text);
    let deep = write(&dir, "sub/deep.colibri", deep_text);
    let script = write(&dir, "tool.colibri", script_text);
    let all = [
        (&main, main_text),
        (&util, util_text),
        (&extra, extra_text),
        (&deep, deep_text),
        (&script, script_text),
    ];
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, main_text);
    c.open(&extra, extra_text);
    // From the entry, and from a module the entry does not import: the same answers.
    let want = 5;
    assert_eq!(
        c.references(&main, pos_of(main_text, "f(1)"), true).len(),
        want
    );
    assert_eq!(
        c.references(&extra, pos_of(extra_text, "f(2)"), true).len(),
        want
    );
    for (from, text, needle) in [(&main, main_text, "f(1)"), (&extra, extra_text, "f(2)")] {
        let edit = c.rename(from, pos_of(text, needle), "g2").expect("renames");
        for (path, old) in all {
            let new = apply_edits(&edit, path, old);
            assert!(!new.contains("f("), "{}: {new}", path.display());
            assert_eq!(new.matches("g2").count(), 1, "{}: {new}", path.display());
        }
    }
    // A syntax error in a module the entry does not import refuses the rename too.
    c.change(&extra, "import util\npub fn g() -> int => util.f(2) )\n");
    let err = c
        .rename(&main, pos_of(main_text, "f(1)"), "g2")
        .unwrap_err();
    assert!(err["message"].as_str().unwrap().contains("extra.colibri"));
    c.shutdown();
}

#[test]
fn rename_is_refused_when_it_fixes_one_error_and_adds_another() {
    let dir = temp_dir("rename_error_swap");
    // One type error. Renaming `a` to `b` makes `b` mean the integer from then on: the error
    // at `d` goes, and `g` gets one. The number of errors is the same.
    let text = "fn main() {\n    let b = \"x\"\n    let a = 1\n    let d: int = b\n    let g: str = b\n    print(\"{a} {d} {g}\")\n}\n";
    let file = write(&dir, "swap.colibri", text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, text);
    assert_eq!(
        c.diags(&file).iter().filter(|d| d["severity"] == 1).count(),
        1
    );
    let err = c.rename(&file, pos_of(text, "a = 1"), "b").unwrap_err();
    let message = err["message"].as_str().unwrap();
    assert!(message.contains("break the program"), "{message}");
    c.shutdown();
}

#[test]
fn completion_tells_two_types_that_print_alike_apart() {
    let dir = temp_dir("complete_alike");
    write(
        &dir,
        "other.colibri",
        "pub struct P { pub theirs: int }\npub fn make() -> P => P { theirs: 1 }\n",
    );
    let text = "import other\nstruct P { mine: int }\nfn main() {\n    let t = other.make()\n    let a = P { mine: 1 }\n    print(\"{t.theirs} {a.mine}\")\n    a.\n}\n";
    let file = write(&dir, "main.colibri", text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, text);
    let items = c.complete_after(&file, text, "    a.");
    let names = labels(&items);
    assert!(names.contains(&"mine"), "{names:?}");
    assert!(!names.contains(&"theirs"), "{names:?}");
    // The other type's own value still has the other type's members.
    let text = text.replace("    a.\n", "    t.\n");
    c.change(&file, &text);
    let items = c.complete_after(&file, &text, "    t.");
    let names = labels(&items);
    assert!(names.contains(&"theirs") && !names.contains(&"mine"));

    // Two type parameters named `T`: the impl's has no bounds, the function's is a `Shape`.
    let text = "interface Shape { fn area(self) -> float }\nstruct Box[T] { v: T }\nimpl[T] Box[T] {\n    fn get(self) -> T => self.v\n}\nfn one[T: Shape](t: T) -> float {\n    t.\n    return 0.0\n}\n";
    let generic = write(&dir, "generic.colibri", text);
    c.open(&generic, text);
    let items = c.complete_after(&generic, text, "    t.");
    let names = labels(&items);
    assert!(names.contains(&"area"), "{names:?}");
    c.shutdown();
}

#[test]
fn completion_from_a_stale_analysis_ignores_a_shadowed_binding() {
    let dir = temp_dir("complete_shadow");
    let file = dir.join("a.colibri");
    let good = "struct A { ax: int }\nstruct B { by: int }\nfn main() {\n    let p = A { ax: 1 }\n    print(\"{p.ax}\")\n}\n";
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, good);
    assert!(c.diags(&file).is_empty());
    // Still the binding it was, while the file does not check: `A`'s members.
    let typing = good.replace("\")\n}\n", "\")\n    p.\n    let = (\n}\n");
    assert_ne!(typing, good);
    c.change(&file, &typing);
    let items = c.complete_after(&file, &typing, "    p.");
    assert!(labels(&items).contains(&"ax"), "{:?}", labels(&items));
    // A newer `let p` in between: nothing, rather than `A`'s members.
    let typing = good.replace(
        "\")\n}\n",
        "\")\n    let p = B { by: 2 }\n    p.\n    let = (\n}\n",
    );
    c.change(&file, &typing);
    assert!(!c.diags(&file).is_empty());
    let items = c.complete_after(&file, &typing, "    p.");
    assert!(items.is_empty(), "{:?}", labels(&items));
    c.shutdown();
}

#[test]
fn completion_after_a_parenthesised_receiver_and_a_raw_string() {
    let dir = temp_dir("complete_paren");
    let file = dir.join("a.colibri");
    let text = "struct P { x: int }\nfn main() {\n    let p = P { x: 1 }\n    print(\"{p.x}\")\n    (p).\n    let r = r#\"a\"#\n}\n";
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, text);
    let items = c.complete_after(&file, text, "(p).");
    assert!(labels(&items).contains(&"x"), "{:?}", labels(&items));
    // Right after a finished raw string, names are offered again.
    let text = "fn main() {\n    let r = r#\"a\"#\n    let s = r#\"b\"#\n}\n";
    c.change(&file, text);
    let items = c.complete_after(&file, text, "r#\"b\"#");
    assert!(labels(&items).contains(&"print"), "{:?}", labels(&items));
    c.shutdown();
}

#[test]
fn a_folder_of_scripts_is_one_package_for_references_and_rename() {
    let dir = temp_dir("implicit_package");
    let helpers_text = "pub fn f(x: int) -> int => x\n";
    let a_text = "import helpers\nprint(helpers.f(1))\n";
    let b_text = "import helpers\nprint(helpers.f(2))\n";
    let helpers = write(&dir, "helpers.colibri", helpers_text);
    let a = write(&dir, "a.colibri", a_text);
    let b = write(&dir, "b.colibri", b_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&a, a_text);
    let at = pos_of(a_text, "f(1)");
    assert_eq!(c.references(&a, at, true).len(), 3);
    assert!(c.prepare_rename(&a, at).is_ok());
    let edit = c.rename(&a, at, "g").expect("renames");
    for (path, old) in [(&helpers, helpers_text), (&a, a_text), (&b, b_text)] {
        let new = apply_edits(&edit, path, old);
        assert!(!new.contains("f("), "{}: {new}", path.display());
        assert_eq!(new.matches('g').count(), 1, "{}: {new}", path.display());
    }
    // A syntax error in the other script refuses it.
    c.change(&b, "import helpers\nprint(helpers.f(2) ))\n");
    for err in [
        c.rename(&a, at, "g").unwrap_err(),
        c.prepare_rename(&a, at).unwrap_err(),
    ] {
        assert!(err["message"].as_str().unwrap().contains("b.colibri"));
    }
    // A file in a subfolder that nothing imports is a script of its own, with its own folder
    // as the root: it is read, and nothing there uses the name.
    c.change(&b, b_text);
    let lost_text = "pub fn h() -> int => 1\n";
    let lost = write(&dir, "sub/lost.colibri", lost_text);
    let edit = c.rename(&a, at, "g").expect("renames");
    assert!(edit["changes"][uri(&lost)].is_null(), "{edit}");
    assert!(!apply_edits(&edit, &helpers, helpers_text).contains("f("));
    // A syntax error in it refuses the rename, naming the file.
    write(&dir, "sub/lost.colibri", "pub fn h( -> int => 1\n");
    let err = c.rename(&a, at, "g").unwrap_err();
    assert!(err["message"].as_str().unwrap().contains("lost.colibri"));
    c.shutdown();
}

#[test]
fn hover_names_the_module_of_a_module_enum() {
    let src = "import fs\n\nfn main() {\n    let kind: fs.Error = fs.Error.NotFound\n    print(kind)\n}\n";
    let dir = temp_dir("hover_module_enum");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, src);
    assert!(c.diags(&file).is_empty(), "{:?}", c.diags(&file));
    // `kind: fs.Error`, not the bare `Error` that `http.Error` and the prelude share.
    let (l, col) = pos_of(src, "kind)");
    assert_eq!(
        hover_text(&c.hover(&file, l, col)),
        "```colibri\nkind: fs.Error\n```"
    );
    c.shutdown();
}

#[test]
fn hover_names_the_module_of_a_host_class_and_a_host_struct() {
    let src = "import process\nimport fs\n\nfn main() {\n    let stat: fs.Stat = fs.stat(\".\").expect(\"stat\")\n    let child: process.Child? = none\n    print(stat)\n    print(child)\n}\n";
    let dir = temp_dir("hover_module_types");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, src);
    assert!(c.diags(&file).is_empty(), "{:?}", c.diags(&file));
    let (l, col) = pos_of(src, "stat)");
    assert_eq!(
        hover_text(&c.hover(&file, l, col)),
        "```colibri\nstat: fs.Stat\n```"
    );
    let (l, col) = pos_of(src, "child)");
    assert_eq!(
        hover_text(&c.hover(&file, l, col)),
        "```colibri\nchild: process.Child?\n```"
    );
    c.shutdown();
}

// ---- references and rename across folders ----

/// The text of `path` after the edit, or the old text if the edit does not touch it.
fn edited(edit: &Value, path: &Path, old: &str) -> String {
    apply_edits(edit, path, old)
}

fn message(err: &Value) -> String {
    err["message"].as_str().unwrap_or_default().to_string()
}

#[test]
fn a_rename_from_a_subfolder_module_reaches_the_importer_in_the_parent_folder() {
    let dir = temp_dir("rename_from_subfolder");
    let main_text = "import sub.foo\nprint(foo.f(1))\n";
    let foo_text = "pub fn f(x: int) -> int => x\n";
    let main = write(&dir, "main.colibri", main_text);
    let foo = write(&dir, "sub/foo.colibri", foo_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&foo, foo_text);
    let at = pos_of(foo_text, "f(x");
    let refs = c.references(&foo, at, true);
    assert_eq!(refs.len(), 2, "{refs:?}");
    assert!(c.prepare_rename(&foo, at).is_ok());
    let edit = c.rename(&foo, at, "g").expect("renames");
    assert_eq!(
        edited(&edit, &foo, foo_text),
        "pub fn g(x: int) -> int => x\n"
    );
    assert_eq!(
        edited(&edit, &main, main_text),
        "import sub.foo\nprint(foo.g(1))\n"
    );
    // Applied to the files, every way of running them still checks.
    assert_eq!(check_reading(&main), Ok(()));
    apply_to_disk(&edit);
    assert_eq!(check_reading(&main), Ok(()));
    assert_eq!(check_reading(&foo), Ok(()));
    std::fs::write(&main, main_text).unwrap();
    std::fs::write(&foo, foo_text).unwrap();
    // The same from the importer.
    c.open(&main, main_text);
    let edit = c
        .rename(&main, pos_of(main_text, "f(1"), "g")
        .expect("renames");
    assert_eq!(
        edited(&edit, &main, main_text),
        "import sub.foo\nprint(foo.g(1))\n"
    );
    assert_eq!(
        edited(&edit, &foo, foo_text),
        "pub fn g(x: int) -> int => x\n"
    );
    c.shutdown();
}

#[test]
fn a_subfolder_with_its_own_main_does_not_hide_the_importer_above_it() {
    let dir = temp_dir("rename_from_subfolder_main");
    let main_text = "import sub.foo\nprint(foo.f(1))\n";
    let foo_text = "pub fn f(x: int) -> int => x\n";
    let inner_text = "import foo\nprint(foo.f(2))\n";
    let main = write(&dir, "main.colibri", main_text);
    let foo = write(&dir, "sub/foo.colibri", foo_text);
    let inner = write(&dir, "sub/main.colibri", inner_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&foo, foo_text);
    let edit = c
        .rename(&foo, pos_of(foo_text, "f(x"), "g")
        .expect("renames");
    assert_eq!(
        edited(&edit, &foo, foo_text),
        "pub fn g(x: int) -> int => x\n"
    );
    assert_eq!(
        edited(&edit, &inner, inner_text),
        "import foo\nprint(foo.g(2))\n"
    );
    assert_eq!(
        edited(&edit, &main, main_text),
        "import sub.foo\nprint(foo.g(1))\n"
    );
    c.shutdown();
}

#[test]
fn without_a_workspace_folder_the_scripts_above_are_found_too() {
    let dir = temp_dir("rename_no_workspace");
    let main_text = "import sub.foo\nprint(foo.f(1))\n";
    let foo_text = "pub fn f(x: int) -> int => x\n";
    let main = write(&dir, "main.colibri", main_text);
    let foo = write(&dir, "sub/foo.colibri", foo_text);
    let mut c = Client::start(None, Value::Null);
    c.open(&foo, foo_text);
    let edit = c
        .rename(&foo, pos_of(foo_text, "f(x"), "g")
        .expect("renames");
    assert_eq!(
        edited(&edit, &main, main_text),
        "import sub.foo\nprint(foo.g(1))\n"
    );
    c.shutdown();
}

#[test]
fn an_unrelated_script_in_a_subfolder_does_not_block_a_rename() {
    let dir = temp_dir("rename_unrelated_sub");
    let a_text = "fn helper() -> int => 1\nfn main() {\n    let v = helper()\n    print(v)\n}\n";
    let a = write(&dir, "a.colibri", a_text);
    let old = write(&dir, "old/x.colibri", "print(1)\n");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&a, a_text);
    let refs = c.references(&a, pos_of(a_text, "v ="), true);
    assert_eq!(refs.len(), 2, "{refs:?}");
    let edit = c
        .rename(&a, pos_of(a_text, "v ="), "w")
        .expect("renames a local");
    assert_eq!(
        edited(&edit, &a, a_text),
        "fn helper() -> int => 1\nfn main() {\n    let w = helper()\n    print(w)\n}\n"
    );
    let edit = c
        .rename(&a, pos_of(a_text, "helper"), "h")
        .expect("renames");
    assert!(edited(&edit, &a, a_text).contains("fn h()"));
    assert!(edit["changes"][uri(&old)].is_null());
    // A syntax error in another file does not matter to a local, only to what others can use.
    write(&dir, "old/x.colibri", "print((\n");
    let edit = c
        .rename(&a, pos_of(a_text, "v ="), "w")
        .expect("renames a local");
    assert!(edited(&edit, &a, a_text).contains("let w"));
    let err = c.rename(&a, pos_of(a_text, "helper"), "h").unwrap_err();
    assert!(message(&err).contains("x.colibri"), "{err}");
    // A syntax error in the file itself does.
    let broken = format!("{a_text}fn other() {{\n    print(1 ))\n}}\n");
    c.change(&a, &broken);
    let err = c.rename(&a, pos_of(&broken, "v ="), "w").unwrap_err();
    assert!(message(&err).contains("a.colibri"), "{err}");
    c.shutdown();
}

#[test]
fn a_library_with_tests_and_examples_can_be_renamed() {
    let dir = temp_dir("rename_library");
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    let lib_text = "pub fn f(x: int) -> int => x\n";
    let lib = write(&dir, "lib.colibri", lib_text);
    write(&dir, "inner/x.colibri", "pub fn g() -> int => 1\n");
    let y_text = "import x\nprint(x.g())\n";
    let y = write(&dir, "inner/y.colibri", y_text);
    let t_text = "import lib\nprint(lib.f(1))\n";
    let t = write(&dir, "tools/t.colibri", t_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&lib, lib_text);
    let at = pos_of(lib_text, "f(x");
    // `tools/t.colibri` is run with `tools` as its root (`colibri run tools/t.colibri`): its
    // `import lib` finds no module, so it is no use of `lib.f` in any way the CLI reads it.
    assert_eq!(c.references(&lib, at, true).len(), 1);
    let edit = c.rename(&lib, at, "h").expect("renames");
    assert_eq!(
        edited(&edit, &lib, lib_text),
        "pub fn h(x: int) -> int => x\n"
    );
    assert_eq!(edited(&edit, &t, t_text), t_text);
    // A file that imports a sibling of its own folder is read with that folder as the root.
    let x = dir.join("inner/x.colibri");
    let x_text = "pub fn g() -> int => 1\n";
    c.open(&x, x_text);
    let edit = c.rename(&x, pos_of(x_text, "g()"), "k").expect("renames");
    assert_eq!(edited(&edit, &x, x_text), "pub fn k() -> int => 1\n");
    assert_eq!(edited(&edit, &y, y_text), "import x\nprint(x.k())\n");
    c.shutdown();
}

#[test]
fn locals_do_not_need_the_package_and_ignore_the_file_cap() {
    let dir = temp_dir("rename_file_cap");
    let a_text = "fn f() -> int => 1\nfn main() {\n    let v = f()\n    print(v)\n}\n";
    let a = write(&dir, "a.colibri", a_text);
    for i in 0..2001 {
        write(&dir, &format!("many/f{i}.colibri"), "print(1)\n");
    }
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&a, a_text);
    assert_eq!(c.references(&a, pos_of(a_text, "v ="), true).len(), 2);
    assert!(c.rename(&a, pos_of(a_text, "v ="), "w").is_ok());
    let err = c.rename(&a, pos_of(a_text, "f() ->"), "g").unwrap_err();
    assert!(message(&err).contains("2000"), "{err}");
    c.shutdown();
}

#[test]
fn errors_that_drop_no_code_do_not_block_a_rename() {
    let dir = temp_dir("rename_load_errors");
    let text = "import nonexist\nfn mine() -> int => 1\nfn main() {\n    let v = mine()\n    print(v)\n}\n";
    let a = write(&dir, "a.colibri", text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&a, text);
    let edit = c
        .rename(&a, pos_of(text, "v ="), "w")
        .expect("renames a local");
    assert!(edited(&edit, &a, text).contains("let w = mine()"));
    let edit = c
        .rename(&a, pos_of(text, "mine() ->"), "yours")
        .expect("renames");
    assert_eq!(
        edited(&edit, &a, text),
        "import nonexist\nfn yours() -> int => 1\nfn main() {\n    let v = yours()\n    print(v)\n}\n"
    );
    c.shutdown();

    // A git dependency that is not fetched.
    let dir = temp_dir("rename_missing_git");
    write(
        &dir,
        "colibri.toml",
        "[package]\nname = \"demo\"\n\n[dependencies]\na = { git = \"https://example.invalid/a.git\", rev = \"abc\" }\n",
    );
    let main_text = "import a\nfn mine() -> int => 1\nfn main() {\n    print(mine())\n}\n";
    let main = write(&dir, "main.colibri", main_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, main_text);
    let edit = c
        .rename(&main, pos_of(main_text, "mine()"), "yours")
        .expect("renames");
    assert_eq!(
        edited(&edit, &main, main_text),
        "import a\nfn yours() -> int => 1\nfn main() {\n    print(yours())\n}\n"
    );
    c.shutdown();
}

#[test]
fn an_import_that_matches_two_modules_refuses_a_rename() {
    let dir = temp_dir("rename_ambiguous");
    let dep = temp_dir("rename_ambiguous_dep");
    write(&dep, "colibri.toml", "[package]\nname = \"foo\"\n");
    write(&dep, "lib.colibri", "pub fn z() -> int => 1\n");
    write(
        &dir,
        "colibri.toml",
        &format!(
            "[package]\nname = \"demo\"\n\n[dependencies]\nfoo = {{ path = \"{}\" }}\n",
            slashes(&dep)
        ),
    );
    write(&dir, "foo.colibri", "pub fn mine() -> int => 1\n");
    let main_text = "import foo\nprint(foo.mine())\n";
    let main = write(&dir, "main.colibri", main_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, main_text);
    let err = c
        .rename(&main, pos_of(main_text, "mine"), "yours")
        .unwrap_err();
    assert!(message(&err).contains("main.colibri"), "{err}");
    assert!(message(&err).contains("more than one module"), "{err}");
    c.shutdown();
}

#[test]
fn a_rename_reaches_the_packages_of_the_workspace_that_depend_on_it() {
    let dir = temp_dir("rename_dependents");
    write(
        &dir,
        "app/colibri.toml",
        "[package]\nname = \"app\"\n\n[dependencies]\nlibr = { path = \"../libr\" }\n",
    );
    let use_text = "import libr\nfn main() {\n    print(libr.f(1))\n}\n";
    let app = write(&dir, "app/main.colibri", use_text);
    write(&dir, "libr/colibri.toml", "[package]\nname = \"libr\"\n");
    let lib_text =
        "pub fn f(x: int) -> int => x\nfn private() -> int => 1\nfn user() -> int => private()\n";
    let lib = write(&dir, "libr/lib.colibri", lib_text);
    // A third package that depends on the second one only through the first.
    write(
        &dir,
        "top/colibri.toml",
        "[package]\nname = \"top\"\n\n[dependencies]\napp = { path = \"../app\" }\n",
    );
    write(&dir, "top/main.colibri", "import app\nfn main() {\n}\n");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&lib, lib_text);
    let at = pos_of(lib_text, "f(x");
    let refs = c.references(&lib, at, true);
    assert_eq!(refs.len(), 2, "{refs:?}");
    let edit = c.rename(&lib, at, "g").expect("renames");
    assert!(edited(&edit, &lib, lib_text).starts_with("pub fn g("));
    assert_eq!(
        edited(&edit, &app, use_text),
        "import libr\nfn main() {\n    print(libr.g(1))\n}\n"
    );
    // From the package that uses it, the item is a dependency's: not renamed there.
    c.open(&app, use_text);
    let err = c.rename(&app, pos_of(use_text, "f(1"), "g").unwrap_err();
    assert!(message(&err).contains("dependency"), "{err}");
    // A syntax error in a package that depends on this one refuses the rename.
    c.change(
        &app,
        "import libr\nfn main() {\n    print(libr.f(1) ))\n}\n",
    );
    let err = c.rename(&lib, at, "g").unwrap_err();
    assert!(message(&err).contains("main.colibri"), "{err}");
    c.shutdown();
}

#[test]
fn a_workspace_too_big_to_search_refuses_to_rename_what_other_packages_can_name() {
    let dir = temp_dir("rename_big_workspace");
    write(&dir, "p/colibri.toml", "[package]\nname = \"p\"\n");
    let lib_text = "pub fn f(x: int) -> int => x\nfn hidden() -> int => 1\nprint(hidden())\n";
    let lib = write(&dir, "p/lib.colibri", lib_text);
    for i in 0..5100 {
        std::fs::create_dir_all(dir.join(format!("many/d{i}"))).unwrap();
    }
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&lib, lib_text);
    let err = c.rename(&lib, pos_of(lib_text, "f(x"), "g").unwrap_err();
    assert!(message(&err).contains("depend on this one"), "{err}");
    // What no other package can name is still renamed.
    assert!(
        c.rename(&lib, pos_of(lib_text, "hidden()"), "secret")
            .is_ok()
    );
    c.shutdown();
}

// ---- completion and hover on qualified type paths ----

#[test]
fn completion_after_a_qualified_type_path_and_in_a_type_position() {
    let dir = temp_dir("complete_qualified");
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    write(
        &dir,
        "util.colibri",
        "pub enum Color {\n    Red\n    Green\n}\n\nimpl Color {\n    pub fn parse(s: str) -> Color => Color.Red\n}\n\npub struct Box { v: int }\npub fn double(x: int) -> int => x * 2\nenum Hidden {\n    A\n}\n",
    );
    let template =
        "import fs\nimport util\n\nfn show(kind: fs.Error, c: util.Color) {\n    HERE\n}\n";
    let main = write(&dir, "main.colibri", template);
    let mut c = Client::start(Some(&dir), Value::Null);
    let typing = |what: &str| template.replace("HERE", what);
    c.open(&main, &typing("print(1)"));

    // The variants of a module's enum after `module.Type.`.
    let text = typing("let k = fs.Error.");
    c.change(&main, &text);
    let items = c.complete_after(&main, &text, "fs.Error.");
    let names = labels(&items);
    assert!(names.contains(&"NotFound"), "{names:?}");
    assert!(names.len() > 1, "{names:?}");

    // And of a user module's enum, with its associated functions but not its methods.
    let text = typing("let k = util.Color.");
    c.change(&main, &text);
    let items = c.complete_after(&main, &text, "util.Color.");
    let names = labels(&items);
    for want in ["Red", "Green", "parse"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    assert_eq!(names.len(), 3, "{names:?}");

    // Inside a `match`, at the start of an arm.
    let text = typing("match kind {\n        fs.Error.\n    }");
    c.change(&main, &text);
    let items = c.complete_after(&main, &text, "fs.Error.");
    assert!(labels(&items).contains(&"NotFound"), "{:?}", labels(&items));
    let text = typing("match c {\n        util.Color.\n    }");
    c.change(&main, &text);
    let items = c.complete_after(&main, &text, "util.Color.");
    let names = labels(&items);
    assert!(
        names.contains(&"Red") && names.contains(&"Green"),
        "{names:?}"
    );
    assert_eq!(
        names.iter().filter(|n| **n == "Red").count(),
        1,
        "no duplicates: {names:?}"
    );

    // A type that is not `pub` is offered inside its package, and not from another one.
    let text = typing("let k = util.Hidden.");
    c.change(&main, &text);
    let items = c.complete_after(&main, &text, "util.Hidden.");
    assert_eq!(labels(&items), ["A"]);
    write(
        &dir,
        "other/colibri.toml",
        "[package]\nname = \"other\"\n\n[dependencies]\ndemo = { path = \"..\" }\n",
    );
    let other_text = "import demo.util\n\nfn f() {\n    let k = util.Hidden.\n}\n";
    let other = write(&dir, "other/main.colibri", other_text);
    c.open(&other, other_text);
    let items = c.complete_after(&other, other_text, "util.Hidden.");
    assert!(items.is_empty(), "{:?}", labels(&items));

    // A type position offers the module's types, not its functions.
    let text = typing("let e: util.");
    c.change(&main, &text);
    let items = c.complete_after(&main, &text, "let e: util.");
    let names = labels(&items);
    assert!(
        names.contains(&"Color") && names.contains(&"Box"),
        "{names:?}"
    );
    assert!(!names.contains(&"double"), "{names:?}");
    let text = typing("let e: fs.");
    c.change(&main, &text);
    let items = c.complete_after(&main, &text, "let e: fs.");
    let names = labels(&items);
    assert!(
        names.contains(&"Error") && names.contains(&"Stat"),
        "{names:?}"
    );
    assert!(!names.contains(&"stat"), "{names:?}");
    // An expression position still offers both.
    let text = typing("let e = util.");
    c.change(&main, &text);
    let items = c.complete_after(&main, &text, "let e = util.");
    assert!(labels(&items).contains(&"double"));
    c.shutdown();
}

#[test]
fn hover_on_a_qualified_type_name_shows_the_enum() {
    let src = "import fs\nimport util\n\nfn main() {\n    let kind: fs.Error = fs.Error.NotFound\n    let c: util.Color = util.Color.Red\n    print(kind)\n    print(c)\n}\n";
    let dir = temp_dir("hover_qualified_type");
    write(&dir, "util.colibri", "pub enum Color {\n    Red\n}\n");
    let file = dir.join("a.colibri");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&file, src);
    assert!(c.diags(&file).is_empty(), "{:?}", c.diags(&file));
    let shown = |c: &mut Client, needle: &str| {
        let (l, col) = pos_of(src, needle);
        hover_text(&c.hover(&file, l, col))
    };
    // In an annotation and in an expression.
    assert_eq!(shown(&mut c, "Error ="), "```colibri\nfs.Error\n```");
    assert_eq!(shown(&mut c, "Error.NotFound"), "```colibri\nfs.Error\n```");
    assert_eq!(shown(&mut c, "Color ="), "```colibri\nenum Color\n```");
    assert_eq!(shown(&mut c, "Color.Red"), "```colibri\nColor\n```");
    c.shutdown();
}

#[test]
fn completion_decides_a_type_position_from_the_syntax_around_the_cursor() {
    let dir = temp_dir("complete_type_position");
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    write(
        &dir,
        "util.colibri",
        "pub struct Q { a: int }\npub fn f(level: int) -> int => level\npub fn make() -> int => 1\n",
    );
    let template = "import util\n\nstruct S {\n    a: int\n}\n\nfn show(p: int) {\n    HERE\n}\n";
    let main = write(&dir, "main.colibri", template);
    let mut c = Client::start(Some(&dir), Value::Null);
    let typing = |what: &str| template.replace("HERE", what);
    c.open(&main, &typing("print(1)"));
    // An expression after a `:` that is not a binding's own annotation: functions are offered.
    for (what, at) in [
        ("let q = util.Q { a: util.", "a: util."),
        ("let r = util.f(level: util.", "level: util."),
        ("let m = {\"a\": util.", "\"a\": util."),
    ] {
        let text = typing(what);
        c.change(&main, &text);
        let items = c.complete_after(&main, &text, at);
        let names = labels(&items);
        assert!(names.contains(&"make"), "{what}: {names:?}");
        assert!(names.contains(&"Q"), "{what}: {names:?}");
    }
    // A type position: only types.
    for what in [
        "let x: util.",
        "var x : util.",
        "fn inner(a: int, b: util.",
        "let r = 1 -> util.",
    ] {
        let text = typing(what);
        c.change(&main, &text);
        let at = what.rsplit_once(' ').map_or(what, |(_, last)| last);
        let items = c.complete_after(&main, &text, at);
        let names = labels(&items);
        if what.contains("let r") {
            continue;
        }
        assert!(names.contains(&"Q"), "{what}: {names:?}");
        assert!(!names.contains(&"make"), "{what}: {names:?}");
    }
    // A field of a struct declaration.
    let text = "import util\n\nstruct T {\n    a: int\n    b: util.\n}\n";
    c.change(&main, text);
    let items = c.complete_after(&main, text, "b: util.");
    assert!(!labels(&items).contains(&"make"), "{:?}", labels(&items));
    assert!(labels(&items).contains(&"Q"), "{:?}", labels(&items));
    c.shutdown();
}

#[test]
fn completion_after_a_type_hides_what_another_package_may_not_call() {
    let dir = temp_dir("complete_private_members");
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    write(
        &dir,
        "util.colibri",
        "pub struct P { v: int }\n\nimpl P {\n    pub fn new() -> P => P { v: 1 }\n    fn secret() -> P => P { v: 2 }\n}\n",
    );
    let main_text = "import util\n\nfn f() {\n    let k = util.P.\n}\n";
    let main = write(&dir, "main.colibri", main_text);
    write(
        &dir,
        "other/colibri.toml",
        "[package]\nname = \"other\"\n\n[dependencies]\ndemo = { path = \"..\" }\n",
    );
    let other = write(
        &dir,
        "other/main.colibri",
        &main_text
            .replace("util", "demo.util")
            .replace("demo.util.P", "util.P"),
    );
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, main_text);
    let items = c.complete_after(&main, main_text, "util.P.");
    let names = labels(&items);
    assert!(
        names.contains(&"new") && names.contains(&"secret"),
        "{names:?}"
    );
    let other_text = "import demo.util\n\nfn f() {\n    let k = util.P.\n}\n";
    c.open(&other, other_text);
    let items = c.complete_after(&other, other_text, "util.P.");
    let names = labels(&items);
    assert_eq!(names, ["new"], "{names:?}");
    c.shutdown();
}

#[test]
fn completion_after_a_host_class_offers_its_functions_without_a_receiver() {
    let dir = temp_dir("complete_host_class");
    let text = "import json\n\nfn f() {\n    let v = json.Value.\n}\n";
    let main = write(&dir, "a.colibri", text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, text);
    let items = c.complete_after(&main, text, "json.Value.");
    let names = labels(&items);
    for want in ["str", "int", "null"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    assert!(
        !names.contains(&"kind"),
        "methods need a receiver: {names:?}"
    );
    c.shutdown();
}

// ---- every way a file is read ----

/// `Ok` if the program at `path` (a script file, or a package folder with all its modules)
/// checks, else the first error.
fn check_reading(path: &Path) -> Result<(), String> {
    let config = colibri_runtime::CheckConfig::default();
    let report = if path.is_dir() {
        colibri_runtime::check_package_all_with(path, &config)
    } else {
        colibri_runtime::check_file_with(path, &config)
    };
    match report {
        Ok(r) => match r.diagnostics.iter().find(|d| d.is_error()) {
            Some(d) => Err(d.message.clone()),
            None => Ok(()),
        },
        Err(e) => Err(e.to_string()),
    }
}

/// Writes the edits of a `WorkspaceEdit` to the files they name.
fn apply_to_disk(edit: &Value) {
    let Some(changes) = edit["changes"].as_object() else {
        return;
    };
    for (uri, _) in changes {
        let path = uri_path(uri);
        let text = std::fs::read_to_string(&path).unwrap();
        let new = apply_edits(edit, &path, &text);
        std::fs::write(&path, new).unwrap();
    }
}

/// Renames at `at` in `doc`; a refusal is returned as its message. A rename that is answered
/// is applied to the files, and every reading that checked before must still check: the safety
/// property of a rename. `readings` are the ways the files are run (script files, package
/// folders).
fn rename_keeping_every_reading(
    c: &mut Client,
    doc: &Path,
    text: &str,
    needle: &str,
    new: &str,
    readings: &[&Path],
) -> Result<Value, String> {
    let before: Vec<Result<(), String>> = readings.iter().map(|r| check_reading(r)).collect();
    let at = pos_of(text, needle);
    let edit = c.rename(doc, at, new).map_err(|e| message(&e))?;
    apply_to_disk(&edit);
    for (r, was) in readings.iter().zip(before) {
        if was.is_ok() {
            assert_eq!(
                check_reading(r),
                Ok(()),
                "{} no longer checks after the rename",
                r.display()
            );
        }
    }
    Ok(edit)
}

#[test]
fn a_file_read_under_two_roots_is_edited_in_the_reading_that_works() {
    // `main` reads `sub/x` with `util` meaning the root's, which is an import cycle between
    // packages, so the only reading of `sub/x` that checks is the script one (`util` is
    // `sub/util`): the rename must follow it, as `colibri check sub/x.colibri` passes.
    let dir = temp_dir("rename_two_roots");
    write(
        &dir,
        "main.colibri",
        "import sub.x\nfn main() {\n    print(x.g())\n}\n",
    );
    let util = write(&dir, "util.colibri", "pub fn f() -> int => 1\n");
    let x_text = "import util\npub fn g() -> int => util.f()\nfn main() {\n    print(g())\n}\n";
    let x = write(&dir, "sub/x.colibri", x_text);
    let sub_util_text = "pub fn f() -> int => 2\n";
    let sub_util = write(&dir, "sub/util.colibri", sub_util_text);
    let main = dir.join("main.colibri");
    assert!(check_reading(&main).is_err(), "the root reading is a cycle");
    assert_eq!(check_reading(&x), Ok(()));
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&sub_util, sub_util_text);
    let refs = c.references(&sub_util, pos_of(sub_util_text, "f()"), true);
    assert_eq!(refs.len(), 2, "{refs:?}");
    let edit =
        rename_keeping_every_reading(&mut c, &sub_util, sub_util_text, "f()", "h", &[&main, &x])
            .expect("renames");
    assert!(edit["changes"][uri(&x)].is_array(), "x is edited: {edit}");
    assert!(
        !edit["changes"]
            .as_object()
            .unwrap()
            .contains_key(&uri(&util))
    );
    c.shutdown();
}

#[test]
fn a_script_in_a_subfolder_of_a_package_with_a_main_reads_its_own_folder() {
    let dir = temp_dir("rename_subfolder_script");
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    write(
        &dir,
        "main.colibri",
        "import helper\nfn main() {\n    print(helper.f())\n}\n",
    );
    write(&dir, "helper.colibri", "pub fn f() -> int => 1\n");
    write(&dir, "tools/helper.colibri", "pub fn f() -> int => 2\n");
    let t = write(
        &dir,
        "tools/t.colibri",
        "import helper\nfn main() {\n    print(helper.f())\n}\n",
    );
    assert_eq!(check_reading(&dir), Ok(()));
    assert_eq!(check_reading(&t), Ok(()));
    let mut c = Client::start(Some(&dir), Value::Null);
    for target in ["helper.colibri", "tools/helper.colibri"] {
        let path = dir.join(target);
        let text = std::fs::read_to_string(&path).unwrap();
        c.open(&path, &text);
        let err = rename_keeping_every_reading(&mut c, &path, &text, "f()", "h", &[&dir, &t])
            .unwrap_err();
        assert!(err.contains("different things"), "{target}: {err}");
        assert!(err.contains("tools/t.colibri"), "{target}: {err}");
        assert!(err.contains("colibri run"), "{target}: {err}");
        assert!(err.contains("package"), "{target}: {err}");
    }
    // Nothing was edited.
    assert_eq!(check_reading(&dir), Ok(()));
    c.shutdown();
}

#[test]
fn a_folder_reached_through_a_link_is_one_file_with_two_names() {
    let w = temp_dir("rename_symlink");
    let m_text = "pub fn f() -> int => 1\n";
    let m = write(&w, "shared/m.colibri", m_text);
    let a_text = "import link.m\nfn main() {\n    print(m.f())\n}\n";
    let a = write(&w, "proj/a.colibri", a_text);
    make_link(&w.join("shared"), &w.join("proj/link"));
    assert_eq!(check_reading(&a), Ok(()));
    // The folder opened is the project, the one beside it, or both; the file is opened by
    // either of its names.
    // With the project as the folder, `shared` is outside it (the link only leads there): the
    // item is `pub`, other packages may use it, and they are not looked for outside the folder,
    // so the rename is refused; references are still answered.
    for doc in [w.join("proj/link/m.colibri"), m.clone()] {
        let mut c = Client::start(Some(&w.join("proj")), Value::Null);
        c.open(&doc, m_text);
        let refs = c.references(&doc, pos_of(m_text, "f()"), true);
        assert_eq!(refs.len(), 2, "{} {refs:?}", doc.display());
        let r = c.rename(&doc, pos_of(m_text, "f()"), "g");
        let why = message(&r.unwrap_err());
        assert!(why.contains("outside the open folder"), "{why}");
        c.shutdown();
    }
    let cases = [(w.clone(), m.clone())];
    for (root, doc) in cases {
        std::fs::write(&m, m_text).unwrap();
        let mut c = Client::start(Some(&root), Value::Null);
        c.open(&doc, m_text);
        let refs = c.references(&doc, pos_of(m_text, "f()"), true);
        assert_eq!(refs.len(), 2, "{} {refs:?}", doc.display());
        let edit =
            rename_keeping_every_reading(&mut c, &doc, m_text, "f()", "g", &[&a, &m]).unwrap();
        // The edit is addressed to the document as the client opened it.
        assert!(edit["changes"][uri(&doc)].is_array(), "{edit}");
        assert_eq!(
            std::fs::read_to_string(&a).unwrap(),
            a_text.replace("m.f()", "m.g()")
        );
        std::fs::write(&a, a_text).unwrap();
        c.shutdown();
    }
}

#[test]
fn a_rename_is_not_refused_for_an_error_that_was_there_before() {
    let dir = temp_dir("rename_same_error");
    write(&dir, "colibri.toml", "[package]\nname = \"demo\"\n");
    let lib_text = "pub fn x() -> int => 1\n";
    let lib = write(&dir, "lib.colibri", lib_text);
    let main_text = "import lib\nfn bad() -> int => \"s\"\nfn main() {\n    print(lib.x())\n}\n";
    let main = write(&dir, "main.colibri", main_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&lib, lib_text);
    c.open(&main, main_text);
    let d = c.diags(&main);
    assert!(
        d.iter()
            .any(|d| d["message"].as_str().unwrap().contains("expected")),
        "{d:?}"
    );
    // `x` becomes `y`: the old rewrite of messages turned `expected` into `eypected`.
    let edit = c
        .rename(&lib, pos_of(lib_text, "x()"), "y")
        .expect("renames");
    assert_eq!(edited(&edit, &lib, lib_text), "pub fn y() -> int => 1\n");
    assert_eq!(
        edited(&edit, &main, main_text),
        main_text.replace("lib.x()", "lib.y()")
    );
    c.shutdown();
}

#[test]
fn cache_folders_are_not_walked_and_a_cut_walk_is_said() {
    let dir = temp_dir("folder_cap");
    write(&dir, "p/colibri.toml", "[package]\nname = \"p\"\n");
    let lib_text = "pub fn f(x: int) -> int => x\n";
    let lib = write(&dir, "p/lib.colibri", lib_text);
    // A big cache (a folder with a `CACHEDIR.TAG`) is not walked; `target` and `node_modules`
    // are ordinary folders (here small ones) and are.
    for i in 0..100 {
        std::fs::create_dir_all(dir.join(format!("target/d{i}"))).unwrap();
        std::fs::create_dir_all(dir.join(format!("node_modules/d{i}"))).unwrap();
    }
    for i in 0..6000 {
        std::fs::create_dir_all(dir.join(format!("cache/d{i}"))).unwrap();
    }
    write(
        &dir,
        "cache/CACHEDIR.TAG",
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    );
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&lib, lib_text);
    let at = pos_of(lib_text, "f(x");
    assert!(c.rename(&lib, at, "g").is_ok(), "pub items still rename");
    assert!(c.shown.is_empty(), "{:?}", c.shown);
    c.shutdown();

    // A tree that is too big says that its references may be incomplete.
    let dir = temp_dir("folder_cap_big");
    write(&dir, "p/colibri.toml", "[package]\nname = \"p\"\n");
    let lib = write(&dir, "p/lib.colibri", lib_text);
    for i in 0..5100 {
        std::fs::create_dir_all(dir.join(format!("many/d{i}"))).unwrap();
    }
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&lib, lib_text);
    let refs = c.references(&lib, at, true);
    assert_eq!(refs.len(), 1);
    assert!(
        c.shown.iter().any(|m| m.contains("may be incomplete")),
        "{:?}",
        c.shown
    );
    c.shutdown();
}

// ---- the fourth review: conflicts, one skip rule, links, speed, scope ----

const PKG: &str = "[package]\nname = \"demo\"\n";

/// A package whose root `helper.f` is also a different `helper.f` for `tools/*` read as scripts.
fn helper_tree(name: &str, t_text: &str) -> (PathBuf, PathBuf, String, PathBuf) {
    let dir = temp_dir(name);
    write(&dir, "colibri.toml", PKG);
    write(
        &dir,
        "main.colibri",
        "import helper\nfn main() {\n    print(helper.f())\n}\n",
    );
    let h_text = "pub fn f() -> int => 1\n";
    let h = write(&dir, "helper.colibri", h_text);
    write(
        &dir,
        "tools/helper.colibri",
        "pub fn f() -> int => 2\npub fn h() -> int => 3\n",
    );
    let t = write(&dir, "tools/t.colibri", t_text);
    (dir, h, h_text.to_string(), t)
}

const T_USES: &str = "import helper\nfn main() {\n    print(helper.f())\n}\n";

#[test]
fn a_type_error_elsewhere_in_another_reading_does_not_hide_a_conflict() {
    let t_text = format!("{T_USES}fn wip() -> int => \"not yet\"\n");
    let (dir, h, h_text, t) = helper_tree("rv4_err_conflict", &t_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&h, &h_text);
    // `tools/t.colibri` as a script has a type error of its own, but its `helper.f` is still
    // the one of `tools/helper.colibri`, which `h` would turn into another function.
    let r = c.rename(&h, pos_of(&h_text, "f()"), "h");
    let m = message(&r.expect_err("a conflict is refused"));
    assert!(m.contains("different things"), "{m}");
    assert!(m.contains("tools/t.colibri"), "{m}");
    // The package reading is the workspace root itself: shown as `.`, not as nothing.
    assert!(m.contains("package in `.`"), "{m}");
    assert!(!m.contains("``"), "{m}");
    assert_eq!(std::fs::read_to_string(&t).unwrap(), t_text);
    // Without the type error it was refused already.
    std::fs::write(&t, T_USES).unwrap();
    let r = c.rename(&h, pos_of(&h_text, "f()"), "h");
    assert!(message(&r.unwrap_err()).contains("different things"));
    c.shutdown();
}

#[test]
fn a_reading_with_errors_is_still_compared_for_a_name_that_is_not_in_a_conflict() {
    // The type error does not stop an ordinary rename, and the new name is checked there.
    let t_text = format!("{T_USES}fn wip() -> int => \"not yet\"\n");
    let (dir, h, h_text, t) = helper_tree("rv4_err_ok", &t_text);
    std::fs::remove_file(dir.join("tools/helper.colibri")).unwrap();
    write(&dir, "tools/helper.colibri", "pub fn f() -> int => 2\n");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&h, &h_text);
    // `f` -> `g` means the same in every reading: `tools/helper.f` is renamed with it? No:
    // that is another declaration, so the script reading of `t` disagrees and it is refused.
    let r = c.rename(&h, pos_of(&h_text, "f()"), "g");
    assert!(message(&r.unwrap_err()).contains("different things"));
    let _ = t;
    c.shutdown();
}

#[test]
fn an_erroneous_script_reading_still_guards_the_meaning() {
    let dir = temp_dir("rv4_err_reading");
    write(&dir, "colibri.toml", PKG);
    write(&dir, "main.colibri", "import sub.x\nprint(x.run())\n");
    let util_text = "pub fn f() -> int => 1\n";
    let util = write(&dir, "util.colibri", util_text);
    let x_text = "import util\npub fn run() -> int => util.f()\n";
    let x = write(&dir, "sub/x.colibri", x_text);
    write(
        &dir,
        "sub/util.colibri",
        "pub fn f() -> int => 2\npub fn g() -> int => 3\npub fn bad() -> int => \"s\"\n",
    );
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&util, util_text);
    let r = c.rename(&util, pos_of(util_text, "f()"), "g");
    let m = message(&r.expect_err("refused"));
    assert!(m.contains("different things"), "{m}");
    assert_eq!(std::fs::read_to_string(&x).unwrap(), x_text);
    c.shutdown();
}

#[test]
fn a_target_folder_is_an_ordinary_folder_with_its_own_script_reading() {
    let dir = temp_dir("rv4_target");
    write(&dir, "colibri.toml", PKG);
    write(&dir, "main.colibri", "import util\nprint(util.f())\n");
    let util_text = "pub fn f() -> int => 1\n";
    let util = write(&dir, "util.colibri", util_text);
    let a_text = "import util\nprint(util.f())\n";
    let a = write(&dir, "target/a.colibri", a_text);
    write(&dir, "target/util.colibri", "pub fn f() -> int => 5\n");
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&util, util_text);
    // `target/a.colibri` is a module of the package (as `colibri check <folder>` reads it) and
    // a script (`colibri run`): `util.f` is not the same function in the two.
    let r = c.rename(&util, pos_of(util_text, "f()"), "g");
    let m = message(&r.expect_err("refused"));
    assert!(m.contains("different things"), "{m}");
    assert!(m.contains("target/a.colibri"), "{m}");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), a_text);
    c.shutdown();
}

#[test]
fn a_folder_with_a_cache_tag_is_not_source() {
    let dir = temp_dir("rv4_cache_tag");
    write(&dir, "colibri.toml", PKG);
    write(&dir, "main.colibri", "import util\nprint(util.f())\n");
    let util_text = "pub fn f() -> int => 1\n";
    let util = write(&dir, "util.colibri", util_text);
    let a_text = "import util\nprint(util.f())\n";
    let a = write(&dir, "cache/a.colibri", a_text);
    write(&dir, "cache/util.colibri", "pub fn f() -> int => 5\n");
    write(
        &dir,
        "cache/CACHEDIR.TAG",
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    );
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&util, util_text);
    let edit = c
        .rename(&util, pos_of(util_text, "f()"), "g")
        .expect("the cache is not looked at");
    assert!(edit["changes"][uri(&a)].is_null(), "{edit}");
    assert_eq!(
        edited(
            &edit,
            &dir.join("main.colibri"),
            "import util\nprint(util.f())\n"
        ),
        "import util\nprint(util.g())\n"
    );
    c.shutdown();
}

#[test]
fn a_file_reached_by_a_link_with_another_name_reads_the_unsaved_buffer() {
    let dir = temp_dir("rv4_symfile");
    write(&dir, "colibri.toml", PKG);
    let main_text = "import alias\nprint(alias.f())\n";
    let main = write(&dir, "main.colibri", main_text);
    let util = write(&dir, "util.colibri", "pub fn f() -> int => 1\n");
    make_link(&util, &dir.join("alias.colibri"));
    let mut c = Client::start(Some(&dir), Value::Null);
    // The buffer has two lines more than the file and a second use.
    let buffer = "// a new comment line\n// another\npub fn f() -> int => 1\npub fn k() -> int => f() + f()\n";
    c.open(&util, buffer);
    let refs = c.references(&util, pos_of(buffer, "f()"), true);
    assert_eq!(refs.len(), 4, "{refs:?}");
    let edit = c
        .rename(&util, pos_of(buffer, "f()"), "g")
        .expect("renames");
    assert_eq!(edited(&edit, &util, buffer), buffer.replace("f()", "g()"));
    assert_eq!(
        edited(&edit, &main, main_text),
        "import alias\nprint(alias.g())\n"
    );
    c.shutdown();
}

#[test]
fn nested_and_dependent_packages_are_edited_within_the_workspace_only() {
    let ws = temp_dir("rv4_deps");
    write(&ws, "lib/colibri.toml", "[package]\nname = \"lib\"\n");
    let u_text = "pub fn f() -> int => 1\n";
    let u = write(&ws, "lib/util.colibri", u_text);
    let use_text = "import lib.util\nprint(util.f())\n";
    let own_text = "import util\nprint(util.f())\n";
    let t = write(&ws, "lib/tests/t.colibri", own_text);
    write(&ws, "lib/main.colibri", own_text);
    let dep = |p: &str| {
        format!("[package]\nname = \"x\"\n\n[dependencies]\nlib = {{ path = \"{p}\" }}\n")
    };
    write(&ws, "app/colibri.toml", &dep("../lib"));
    let app = write(&ws, "app/main.colibri", use_text);
    write(&ws, "app2/colibri.toml", &dep("vendor/lib"));
    let app2 = write(&ws, "app2/main.colibri", use_text);
    std::fs::create_dir_all(ws.join("app2/vendor")).unwrap();
    make_link(&ws.join("lib"), &ws.join("app2/vendor/lib"));
    write(&ws, "mid/colibri.toml", &dep("../lib"));
    let mid = write(&ws, "mid/s.colibri", use_text);
    write(&ws, "lib/ex/colibri.toml", &dep(".."));
    let ex = write(&ws, "lib/ex/main.colibri", use_text);

    // Open at the folder that holds everything: every dependent is edited.
    let mut c = Client::start(Some(&ws), Value::Null);
    c.open(&u, u_text);
    let edit = c.rename(&u, pos_of(u_text, "f()"), "g").expect("renames");
    assert_eq!(
        edited(&edit, &t, own_text),
        "import util\nprint(util.g())\n",
        "a module of the package itself"
    );
    for file in [&app, &app2, &mid, &ex] {
        assert_eq!(
            edited(&edit, file, use_text),
            "import lib.util\nprint(util.g())\n",
            "{}",
            file.display()
        );
    }
    c.shutdown();

    // Open at `lib`: the package and the package nested in it, nothing outside.
    let mut c = Client::start(Some(&ws.join("lib")), Value::Null);
    c.open(&u, u_text);
    let edit = c.rename(&u, pos_of(u_text, "f()"), "g").expect("renames");
    assert_eq!(
        edited(&edit, &t, own_text),
        "import util\nprint(util.g())\n"
    );
    assert_eq!(
        edited(&edit, &ex, use_text),
        "import lib.util\nprint(util.g())\n"
    );
    assert!(edit["changes"][uri(&app)].is_null());
    c.shutdown();

    // Open at `app`: `lib` is outside the open folder and its `pub fn f` is used by packages
    // nobody looked for, so the rename is refused and says to open the folder.
    let mut c = Client::start(Some(&ws.join("app")), Value::Null);
    c.open(&u, u_text);
    let m = message(&c.rename(&u, pos_of(u_text, "f()"), "g").unwrap_err());
    assert!(m.contains("outside the open folder"), "{m}");
    assert!(m.contains("open"), "{m}");
    // Names that nothing outside the package can use still rename.
    let private = "fn p() -> int => 1\npub fn q() -> int => p()\n";
    let pu = write(&ws, "lib/priv.colibri", private);
    c.open(&pu, private);
    assert!(c.rename(&pu, pos_of(private, "p()"), "r").is_ok());
    c.shutdown();

    // No workspace folder: the document's own package.
    let mut c = Client::start(None, Value::Null);
    c.open(&u, u_text);
    let edit = c.rename(&u, pos_of(u_text, "f()"), "g").expect("renames");
    assert_eq!(
        edited(&edit, &t, own_text),
        "import util\nprint(util.g())\n"
    );
    // The package nested in the document's own folder is still found.
    assert_eq!(
        edited(&edit, &ex, use_text),
        "import lib.util\nprint(util.g())\n"
    );
    assert!(edit["changes"][uri(&app)].is_null());
    c.shutdown();
}

/// A package with `n` modules (in subfolders, or all in the root), each using `util.f`;
/// renames `f` and returns how long that took.
fn big_tree(n: usize, flat: bool, name: &str) -> std::time::Duration {
    let dir = temp_dir(name);
    write(&dir, "colibri.toml", PKG);
    write(&dir, "main.colibri", "import util\nprint(util.f())\n");
    let util_text = "pub fn f() -> int => 1\n";
    let util = write(&dir, "util.colibri", util_text);
    for i in 0..n {
        write(
            &dir,
            &if flat {
                format!("s{i}.colibri")
            } else {
                format!("sub{}/s{i}.colibri", i % 10)
            },
            &format!("import util\npub fn s{i}() -> int => util.f()\n"),
        );
    }
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&util, util_text);
    let t = std::time::Instant::now();
    let edit = c.rename(&util, pos_of(util_text, "f()"), "g");
    let took = t.elapsed();
    eprintln!("rename in {n} files: {took:?}");
    assert!(edit.is_ok(), "{edit:?}");
    c.shutdown();
    took
}

#[test]
fn rename_in_a_big_tree_takes_seconds_not_minutes() {
    // It took 11.5 s in subfolders and 4 s flat when every file lookup scanned the buffers.
    for (flat, name) in [(false, "rv4_big_sub"), (true, "rv4_big_flat")] {
        let took = big_tree(1900, flat, name);
        assert!(took < std::time::Duration::from_secs(5), "{name}: {took:?}");
    }
}

#[test]
fn a_masked_non_ascii_credential_keeps_the_columns_after_it() {
    // The manifest text the loader keeps for the error printer has the credential masked; the
    // masks must be as long as the characters in UTF-16, or the end column of a diagnostic on
    // the value is wrong (`éé` is two units, a 4-byte character two for itself).
    let dir = temp_dir("masked_columns");
    let manifest =
        "[package]\nname = \"demo\"\n\n[dependencies]\nfoo = { git = \"https://éé😀:p@h/r\" }\n";
    let toml = write(&dir, "colibri.toml", manifest);
    let main_text = "fn main() {}\n";
    let main = write(&dir, "main.colibri", main_text);
    let mut c = Client::start(Some(&dir), Value::Null);
    c.open(&main, main_text);
    let d = c.diags(&toml);
    assert_eq!(d.len(), 1, "{d:?}");
    let value = "\"https://éé😀:p@h/r\"";
    let (line, col) = pos_of(manifest, value);
    let units: u32 = value.encode_utf16().count() as u32;
    assert_eq!(
        d[0]["range"]["start"],
        json!({"line": line, "character": col})
    );
    assert_eq!(
        d[0]["range"]["end"],
        json!({"line": line, "character": col + units}),
        "{d:?}"
    );
    let shown = d[0].to_string();
    assert!(!shown.contains(":p@"), "{shown}");
    c.shutdown();
}
