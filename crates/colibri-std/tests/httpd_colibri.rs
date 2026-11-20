//! The tests of the `httpd` source module that are written in Colibri (`tests/httpd/*.colibri`).
//!
//! `colibri test` does not collect `test` blocks from a standard source module, and the parser's
//! functions are private to the module, so each file is run in a package of its own: a temporary
//! folder with a `colibri.toml`, an empty `main.colibri`, the text of the `httpd` module (from
//! `colibri_std::sources()`, so it is the very text that ships) under the name `httpd_impl`, the
//! helpers of `common.colibri` and the test file. Package-private items are visible across the
//! files of a package, so the tests call the module's own functions. `Vm::load_tests` and
//! `Vm::run_test` run them. Nothing here writes a path into Colibri source: the folder is only
//! ever handed to the loader as a path.
//!
//! One Rust test runs the tests of one file. A second kind of entry, `expect_failure`, runs the
//! files of `tests/httpd/failing/`: there every `test` must panic, with the text its name gives
//! after `=>`, because `colibri test` has no way to say that a test should panic (the router
//! refuses a malformed pattern by panicking).

use std::path::PathBuf;
use std::time::Duration;

use colibri_runtime as colibri;

/// A folder of its own, removed afterwards.
struct Package(PathBuf);

impl Package {
    /// `name` is unique among the tests that run together (one folder for each Rust test).
    fn new(name: &str, files: &[(&str, &str)]) -> Package {
        let dir = std::env::temp_dir().join(format!("colibri-httpd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let httpd = colibri_std::sources()
            .into_iter()
            .find(|s| s.name == "httpd")
            .expect("httpd is a standard source module");
        let mut all = vec![
            ("colibri.toml", "[package]\nname = \"httpd_tests\"\n"),
            ("main.colibri", "fn main() {}\n"),
            ("httpd_impl.colibri", httpd.text),
            ("common.colibri", include_str!("httpd/common.colibri")),
        ];
        all.extend_from_slice(files);
        for (file, text) in all {
            std::fs::write(dir.join(file), text).unwrap();
        }
        Package(dir)
    }

    /// A `Vm` with the standard library and the package loaded for its tests.
    fn load(&self) -> colibri::Vm {
        let mut vm = colibri::Vm::builder()
            .stdout(Box::new(colibri::SharedBuffer::new()))
            .allow(colibri::Cap::Net)
            .allow(colibri::Cap::Os)
            .build();
        colibri_std::install(&mut vm).expect("the standard library installs");
        if let Err(e) = vm.load_tests(&self.0) {
            panic!("the package does not load:\n{}", e.render(false));
        }
        vm
    }
}

impl Drop for Package {
    fn drop(&mut self) {
        // Windows refuses to remove a folder with a file that is still open; try again briefly.
        for _ in 0..50 {
            if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The number of `test "..." {` blocks the text of a file has, to see that every one was found.
fn declared_tests(text: &str) -> usize {
    text.lines().filter(|l| l.starts_with("test \"")).count()
}

/// Runs every test of a file, which must all pass.
fn run_file(name: &str, text: &str) {
    let package = Package::new(name, &[(&format!("{name}.colibri"), text)]);
    let mut vm = package.load();
    let tests: Vec<colibri::TestInfo> = vm.tests().into_iter().filter(|t| t.root).collect();
    assert_eq!(
        tests.len(),
        declared_tests(text),
        "{name}.colibri: a test was not found"
    );
    assert!(!tests.is_empty(), "{name}.colibri has no tests");
    let mut failures = Vec::new();
    for test in &tests {
        if let Err(e) = vm.run_test(test.id) {
            failures.push(format!(
                "---- {} ({}:{}) ----\n{}",
                test.name,
                test.file,
                test.line,
                e.render(false)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} tests of {name}.colibri failed:\n{}",
        failures.len(),
        tests.len(),
        failures.join("\n")
    );
}

/// Runs the tests of a file of `tests/httpd/failing/`: each must panic, and the panic's message
/// must hold the text after ` => ` in the test's name.
fn expect_failure(name: &str, text: &str) {
    let package = Package::new(
        &format!("{name}-failing"),
        &[(&format!("{name}.colibri"), text)],
    );
    let mut vm = package.load();
    let tests: Vec<colibri::TestInfo> = vm.tests().into_iter().filter(|t| t.root).collect();
    assert_eq!(
        tests.len(),
        declared_tests(text),
        "{name}.colibri: a test was not found"
    );
    assert!(!tests.is_empty(), "{name}.colibri has no tests");
    let mut wrong = Vec::new();
    for test in &tests {
        let (title, expected) = test
            .name
            .split_once(" => ")
            .unwrap_or_else(|| panic!("the test `{}` does not say `=> text`", test.name));
        match vm.run_test(test.id) {
            Ok(()) => wrong.push(format!("{title}: it did not panic")),
            Err(colibri::Error::Panic { message, .. }) if message.contains(expected) => {}
            Err(colibri::Error::Panic { message, .. }) => wrong.push(format!(
                "{title}: it panicked with `{message}`, not with `{expected}`"
            )),
            Err(other) => wrong.push(format!(
                "{title}: it failed, but not by a panic:\n{}",
                other.render(false)
            )),
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

macro_rules! suite {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                run_file(
                    stringify!($name),
                    include_str!(concat!("httpd/", stringify!($name), ".colibri")),
                );
            }
        )*
    };
}

suite!(
    request_line,
    fields,
    host,
    framing,
    chunk,
    percent,
    headers,
    router,
    response,
    body,
);

macro_rules! failing {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                expect_failure(
                    stringify!($name),
                    include_str!(concat!("httpd/failing/", stringify!($name), ".colibri")),
                );
            }
        )*
    };
}

failing!(router_registration, request_misuse);

/// The module as a program sees it: `import httpd` (the source module itself, not the copy the
/// tests above use), the handlers of the plan's JSON example, and `test_request` to call them.
#[test]
fn a_program_imports_the_module_and_tests_its_handlers_without_sockets() {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .allow(colibri::Cap::Net)
        .allow(colibri::Cap::Os)
        .build();
    colibri_std::install(&mut vm).expect("the standard library installs");
    vm.load_source(
        "app/main.colibri",
        r##"
import httpd
import json
import str

struct Item {
    id: int
    name: str
}

struct NewItem {
    name: str
}

class Store {
    items: Map[int, Item] = {:}
    next_id: int = 1
}

fn item_id(req: httpd.Request) -> Result[int] {
    let text = req.param("id") or ""
    let id = str.parse_int(text) or |e| {
        return err(httpd.fail(400, "`{text}` is not an item id"))
    }
    return ok(id)
}

fn show(result: Result[httpd.Response]) -> str {
    return match result {
        ok(r) => {
            let body = match r.body {
                Text(s) => s
                _ => ""
            }
            "{r.status} {body}"
        }
        err(e) => "error {e}"
    }
}

fn main() -> Result[()] {
    let store = Store {}
    let r = httpd.Router()
    r.get("/items", fn(req) => httpd.json(200, store.items.values()))
    r.get("/items/:id", fn(req: httpd.Request) -> Result[httpd.Response] {
        let id = item_id(req)?
        let item = store.items.get(id) or { return err(httpd.fail(404, "no item {id}")) }
        return httpd.json(200, item)
    })
    r.post("/items", fn(req: httpd.Request) -> Result[httpd.Response] {
        let new = json.decode[NewItem](req.text()?) or |e| { return err(httpd.fail(400, e.message())) }
        let item = Item { id: store.next_id, name: new.name }
        store.next_id += 1
        store.items[item.id] = item
        return ok(httpd.json(201, item)?.header("Location", "/items/{item.id}"))
    })
    let handle = r.handler()
    let created = handle(httpd.test_request("POST", "/items", [("Content-Type", "application/json")], r#"{"name": "pen"}"#.to_bytes()))
    print(show(created))
    print(show(handle(httpd.test_request("GET", "/items/1"))))
    print(show(handle(httpd.test_request("GET", "/items/9"))))
    print(show(handle(httpd.test_request("GET", "/items/x"))))
    print(show(handle(httpd.test_request("POST", "/items", [], "not json".to_bytes()))))
    print(show(handle(httpd.test_request("GET", "/items"))))
    print(show(handle(httpd.test_request("DELETE", "/items"))))
    return ok(())
}
"##,
    )
    .unwrap_or_else(|e| panic!("{}", e.render(false)));
    vm.run_main()
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
    let printed = out.contents();
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(lines[0], r#"201 {"id":1,"name":"pen"}"#);
    assert_eq!(lines[1], r#"200 {"id":1,"name":"pen"}"#);
    assert_eq!(lines[2], "error 404: no item 9");
    assert_eq!(lines[3], "error 400: `x` is not an item id");
    assert!(lines[4].starts_with("error 400: "), "{}", lines[4]);
    // the message of a JSON error may take more than one line, so the last ones are counted from
    // the end; the body of the 405 ends with a newline of its own
    assert_eq!(lines[lines.len() - 3], r#"200 [{"id":1,"name":"pen"}]"#);
    assert_eq!(lines[lines.len() - 2], "405 405 Method Not Allowed");
    assert_eq!(lines[lines.len() - 1], "");
}
