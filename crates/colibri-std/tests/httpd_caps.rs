//! What `import httpd` asks the host for, which the reference (`guide/std/httpd.md`) states: a
//! module needs `net` (it listens) and `os` (the `Date` header reads the clock), and nothing else.

use colibri_runtime as colibri;

fn check(source: &str, caps: colibri::CapSet) -> String {
    let config = colibri::CheckConfig {
        interfaces: colibri_std::interfaces(),
        sources: colibri_std::sources(),
        caps,
        ..colibri::CheckConfig::default()
    };
    colibri::check_source_with("a.colibri", source, &config).render(false)
}

#[test]
fn httpd_needs_net_and_os_and_nothing_else() {
    let serve = "import httpd\nfn main() -> Result[()] {\n    return httpd.serve(\"127.0.0.1:0\", fn(r) => ok(httpd.text(200, \"x\")))\n}\n";
    let text = check(serve, colibri::CapSet::none());
    assert!(
        text.contains("`net.listen` needs the `net` capability"),
        "{text}"
    );
    assert!(
        text.contains("`time.unix` needs the `os` capability"),
        "{text}"
    );
    assert_eq!(text.matches("Error:").count(), 2, "{text}");
    assert_eq!(check(serve, colibri::CapSet::all()), "");

    // The module is checked as a whole, so a program that only builds values asks for the same.
    let build = "import httpd\nfn main() -> Result[()] {\n    let router = httpd.Router()\n    router.get(\"/\", fn(r) => httpd.json(200, [1, 2]))\n    let r = httpd.test_request(\"GET\", \"/\")\n    let _ = router.handler()(r)?\n    let _ = httpd.text(200, \"x\").header(\"A\", \"b\")\n}\n";
    let text = check(build, colibri::CapSet::none());
    assert_eq!(text.matches("Error:").count(), 2, "{text}");
    let mut both = colibri::CapSet::none();
    both.insert(colibri::Cap::Net);
    both.insert(colibri::Cap::Os);
    assert_eq!(check(build, both), "");
}
