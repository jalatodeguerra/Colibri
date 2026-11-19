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
    /// The latest diagnostics published for each URI.
    diagnostics: HashMap<String, Vec<Value>>,
    /// `window/logMessage` texts.
    logs: Vec<String>,
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
            diagnostics: HashMap::new(),
            logs: Vec::new(),
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

fn uri(path: &Path) -> String {
    format!("file://{}", path.display())
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
    let err = c.request_raw("textDocument/rename", json!({})).unwrap_err();
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
    c.shutdown();
}
