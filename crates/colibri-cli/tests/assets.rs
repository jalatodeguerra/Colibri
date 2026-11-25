//! `import assets` and `[build] assets`, end to end: the same program reads the same assets under
//! `colibri run`, under `colibri test` and in a built executable that has neither its sources nor
//! its assets on disk; what is not an asset, and what leaves the package, is an error that the
//! program can handle.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

mod common;

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

/// A fresh folder, removed at the end, with a private `COLIBRI_HOME` inside.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir =
            std::env::temp_dir().join(format!("colibri-assets-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home")).unwrap();
        Dir(dir)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn write_bytes(&self, rel: &str, data: &[u8]) {
        let full = self.0.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, data).unwrap();
    }

    fn write(&self, rel: &str, text: &str) {
        self.write_bytes(rel, text.as_bytes());
    }

    fn colibri(&self, args: &[&str]) -> Out {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_colibri"));
        cmd.args(args);
        self.exec(cmd)
    }

    fn built(&self, exe: &str) -> Out {
        self.exec(Command::new(self.path(exe)))
    }

    fn exec(&self, mut cmd: Command) -> Out {
        cmd.current_dir(&self.0)
            .env("COLIBRI_HOME", self.path("home"));
        let out = cmd.output().expect("the program runs");
        Out {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().unwrap_or(-1),
        }
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn build(dir: &Dir, target: &str, name: &str) -> Out {
    let out = dir.colibri(&["build", target, "-o", &exe(name)]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(dir.path(&exe(name)).is_file());
    out
}

const MANIFEST: &str =
    "[package]\nname = \"app\"\n\n[build]\nassets = [\"greeting.txt\", \"data/\", \"img/*.png\"]\n";

/// The bytes of the logo: not text, with a first byte of 0.
const LOGO: &[u8] = &[0x00, 0xff, 0x10, 0xc0, 0x7f, 0x80];

const MAIN: &str = r#"import assets

fn kind(e: Error) -> str {
    match e.as[assets.Error]() {
        some(k) => return "{k}"
        none => return "other"
    }
}

fn show(path: str) {
    match assets.read_bytes(path) {
        ok(b) => print("{path}: {b.len()} bytes")
        err(e) => print("{path}: {kind(e)}")
    }
}

fn main() {
    print(assets.read("greeting.txt").expect("greeting").trim())
    let logo = assets.read_bytes("img/logo.png").expect("logo")
    print("logo {logo.len()} first {logo[0]} last {logo[logo.len() - 1]}")
    print(assets.read("data/deep/note.txt").expect("note").trim())
    print(assets.list("").expect("top"))
    print(assets.list("img").expect("img"))
    print(assets.list("data").expect("data"))
    print(assets.list("data/").expect("data slash"))
    show("secret.txt")
    show("img/photo.jpg")
    show("nope.txt")
    show("data")
    show("../outside.txt")
    show("data/../greeting.txt")
    show("/etc/hostname")
    show("")
    show("data//deep/note.txt")
    show("img\\logo.png")
    match assets.read("img/logo.png") {
        ok(_) => print("logo as text: read")
        err(e) => print("logo as text: {kind(e)}")
    }
    match assets.list("nope") {
        ok(_) => print("list nope: listed")
        err(e) => print("list nope: {kind(e)}")
    }
    match assets.list("../app") {
        ok(_) => print("list ../app: listed")
        err(e) => print("list ../app: {kind(e)}")
    }
}

test "assets under test" {
    assert_eq(assets.read("greeting.txt").expect("g").trim(), "hello assets")
    assert_eq(assets.read_bytes("img/logo.png").expect("l").len(), 6)
    assert_eq(assets.list("data").expect("d"), ["deep/", "table.csv"])
    assert(assets.read_bytes("secret.txt").err().is_some())
    assert(assets.read_bytes("../outside.txt").err().is_some())
}
"#;

/// What `main` prints, whichever way it runs.
const EXPECTED: &str = r#"hello assets
logo 6 first 0 last 128
note
["data/", "greeting.txt", "img/"]
["logo.png"]
["deep/", "table.csv"]
["deep/", "table.csv"]
secret.txt: NotFound
img/photo.jpg: NotFound
nope.txt: NotFound
data: NotFound
../outside.txt: Invalid
data/../greeting.txt: Invalid
/etc/hostname: Invalid
: Invalid
data//deep/note.txt: Invalid
img\logo.png: Invalid
logo as text: InvalidData
list nope: NotFound
list ../app: Invalid
"#;

/// A package `app/` with assets, a file the manifest does not list, a file the patterns do not
/// reach, and a file beside the package that nothing may read.
fn package(dir: &Dir) {
    dir.write("app/colibri.toml", MANIFEST);
    dir.write("app/main.colibri", MAIN);
    dir.write("app/greeting.txt", "hello assets\n");
    dir.write("app/data/deep/note.txt", "note\n");
    dir.write("app/data/table.csv", "a,b\n");
    dir.write_bytes("app/img/logo.png", LOGO);
    dir.write("app/img/photo.jpg", "PHOTO-NOT-LISTED");
    dir.write("app/secret.txt", "SECRET-UNLISTED");
    dir.write("outside.txt", "OUTSIDE-OF-THE-PACKAGE");
}

#[test]
fn run_test_and_a_built_program_read_the_same_assets() {
    let dir = Dir::new("parity");
    package(&dir);
    let run = dir.colibri(&["run", "app"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.stdout, EXPECTED);

    let test = dir.colibri(&["test", "app"]);
    assert_eq!(test.code, 0, "{}{}", test.stdout, test.stderr);
    assert!(
        test.stdout.contains("assets under test ... ok"),
        "{}",
        test.stdout
    );

    let made = build(&dir, "app", "appbin");
    // The build says what it packed.
    for line in [
        "  asset data/deep/note.txt (5 bytes)",
        "  asset data/table.csv (4 bytes)",
        "  asset greeting.txt (13 bytes)",
        "  asset img/logo.png (6 bytes)",
    ] {
        assert!(made.stderr.contains(line), "{line}\n{}", made.stderr);
    }
    assert!(
        made.stderr.contains("(1 source file, 4 assets,"),
        "{}",
        made.stderr
    );
    // The files the patterns do not select are not in the executable.
    let bytes = fs::read(dir.path(&exe("appbin"))).unwrap();
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    assert!(has(b"hello assets"));
    assert!(!has(b"SECRET-UNLISTED"));
    assert!(!has(b"OUTSIDE-OF-THE-PACKAGE"));
    assert!(!has(b"PHOTO-NOT-LISTED"));
    // Neither the sources nor the assets are left: the executable has them.
    fs::remove_dir_all(dir.path("app")).unwrap();
    fs::remove_file(dir.path("outside.txt")).unwrap();
    let built = dir.built(&exe("appbin"));
    assert_eq!(built.code, 0, "{}", built.stderr);
    assert_eq!(built.stdout, run.stdout);
    assert_eq!(built.stderr, run.stderr);
}

#[test]
fn the_assets_of_a_package_do_not_depend_on_the_current_folder() {
    let dir = Dir::new("cwd");
    package(&dir);
    let near = dir.colibri(&["run", "app"]);
    assert_eq!(near.stdout, EXPECTED);
    // From inside the package, and from a folder beside it, the program is the same.
    let inside = {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_colibri"));
        cmd.args(["run", "."]);
        let mut c = cmd;
        c.current_dir(dir.path("app"))
            .env("COLIBRI_HOME", dir.path("home"));
        let o = c.output().unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    assert_eq!(inside, EXPECTED);
}

#[test]
fn a_script_beside_a_manifest_has_its_assets_and_a_lone_script_has_none() {
    let dir = Dir::new("script");
    let prog = "import assets\n\nfn main() {\n    match assets.read_bytes(\"data.txt\") {\n        ok(b) => print(\"read {b.len()}\")\n        err(e) => print(\"error {e.as[assets.Error]()}\")\n    }\n    print(assets.list(\"\").expect(\"top\"))\n}\n";
    // Beside a manifest: the folder of the script is the package.
    dir.write(
        "pkg/colibri.toml",
        "[package]\nname = \"pkg\"\n\n[build]\nassets = [\"data.txt\"]\n",
    );
    dir.write("pkg/data.txt", "12345");
    dir.write("pkg/tool.colibri", prog);
    // No manifest: nothing is an asset, though the file is there.
    dir.write("lone/data.txt", "12345");
    dir.write("lone/tool.colibri", prog);

    let run = dir.colibri(&["run", "pkg/tool.colibri"]);
    assert_eq!(run.stdout, "read 5\n[\"data.txt\"]\n", "{}", run.stderr);
    build(&dir, "pkg/tool.colibri", "tool");
    fs::remove_dir_all(dir.path("pkg")).unwrap();
    let built = dir.built(&exe("tool"));
    assert_eq!(
        (built.stdout.as_str(), built.code),
        (run.stdout.as_str(), 0)
    );

    let run = dir.colibri(&["run", "lone/tool.colibri"]);
    assert_eq!(run.stdout, "error some(NotFound)\n[]\n", "{}", run.stderr);
    let made = build(&dir, "lone/tool.colibri", "lonetool");
    assert!(!made.stderr.contains("asset"), "{}", made.stderr);
    fs::remove_dir_all(dir.path("lone")).unwrap();
    let built = dir.built(&exe("lonetool"));
    assert_eq!(
        (built.stdout.as_str(), built.code),
        (run.stdout.as_str(), 0)
    );
}

#[test]
fn a_package_that_lists_no_assets_reads_none() {
    let dir = Dir::new("none");
    dir.write("app/colibri.toml", "[package]\nname = \"app\"\n");
    dir.write("app/data.txt", "x");
    dir.write(
        "app/main.colibri",
        "import assets\n\nfn main() {\n    print(assets.read_bytes(\"data.txt\").err().is_some())\n}\n",
    );
    assert_eq!(dir.colibri(&["run", "app"]).stdout, "true\n");
    let made = build(&dir, "app", "appbin");
    assert!(!made.stderr.contains("asset"), "{}", made.stderr);
    assert_eq!(dir.built(&exe("appbin")).stdout, "true\n");
}

#[test]
fn a_pattern_that_selects_nothing_stops_the_build() {
    let dir = Dir::new("typo");
    package(&dir);
    dir.write(
        "app/colibri.toml",
        "[package]\nname = \"app\"\n\n[build]\nassets = [\"data/\", \"imgs/*.png\"]\n",
    );
    let out = dir.colibri(&["build", "app", "-o", &exe("appbin")]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("the asset pattern `imgs/*.png` of `[build] assets` selects no file"),
        "{}",
        out.stderr
    );
    assert!(!dir.path(&exe("appbin")).exists());
}

#[test]
fn a_bad_pattern_in_the_manifest_is_an_error_of_every_command() {
    let dir = Dir::new("badpattern");
    package(&dir);
    for pattern in ["../outside.txt", "/etc/passwd", "a/**b", ""] {
        dir.write(
            "app/colibri.toml",
            &format!("[package]\nname = \"app\"\n\n[build]\nassets = [\"{pattern}\"]\n"),
        );
        for cmd in ["run", "test", "build", "check"] {
            let out = dir.colibri(&[cmd, "app"]);
            assert_eq!(out.code, 1, "{cmd} {pattern}: {}{}", out.stdout, out.stderr);
            assert!(
                out.stderr.contains("not a valid asset pattern"),
                "{cmd} {pattern}: {}",
                out.stderr
            );
        }
    }
}

#[test]
fn the_output_is_never_an_asset() {
    let dir = Dir::new("clobber");
    package(&dir);
    let out = dir.colibri(&["build", "app", "-o", "app/greeting.txt"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("the output is the asset `greeting.txt`"),
        "{}",
        out.stderr
    );
    assert_eq!(
        fs::read_to_string(dir.path("app/greeting.txt")).unwrap(),
        "hello assets\n"
    );
}

#[cfg(unix)]
#[test]
fn a_link_out_of_the_package_is_never_read_and_a_build_refuses_it() {
    use std::os::unix::fs::symlink;
    let dir = Dir::new("links");
    package(&dir);
    dir.write("app/data/ok.txt", "ok");
    symlink(dir.path("outside.txt"), dir.path("app/data/leak.txt")).unwrap();
    symlink(dir.0.clone(), dir.path("app/data/up")).unwrap();
    let prog = "import assets\n\nfn main() {\n    for p in [\"data/ok.txt\", \"data/leak.txt\", \"data/up/outside.txt\"] {\n        match assets.read(p) {\n            ok(t) => print(\"{p}: {t}\")\n            err(e) => print(\"{p}: {e.as[assets.Error]()}\")\n        }\n    }\n    print(assets.list(\"data\").expect(\"data\"))\n}\n";
    dir.write("app/main.colibri", prog);
    let run = dir.colibri(&["run", "app"]);
    assert_eq!(
        run.stdout,
        "data/ok.txt: ok\ndata/leak.txt: some(Invalid)\ndata/up/outside.txt: some(Invalid)\n[\"deep/\", \"ok.txt\", \"table.csv\"]\n",
        "{}",
        run.stderr
    );
    assert!(!run.stdout.contains("OUTSIDE"));
    let out = dir.colibri(&["build", "app", "-o", &exe("appbin")]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("is a symbolic link"), "{}", out.stderr);
    assert!(!dir.path(&exe("appbin")).exists());
}

/// The executable `built` made, with a mistake `damage` makes in its bytes.
fn damaged(dir: &Dir, name: &str, damage: impl FnOnce(&mut Vec<u8>)) -> Out {
    let mut bytes = common::plain(fs::read(dir.path(&exe("good"))).unwrap());
    let unsigned_len = bytes.len() as u64;
    damage(&mut bytes);
    common::fit(&mut bytes, unsigned_len);
    let target = dir.path(&exe(name));
    fs::write(&target, &bytes).unwrap();
    common::sign(&target);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir.built(&exe(name))
}

fn damaged_error(out: &Out) {
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(
        out.stderr.starts_with("error: ") && out.stderr.contains("program's data"),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains("panicked"), "{}", out.stderr);
}

fn find(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("the bytes are in the file")
}

#[test]
fn a_damaged_program_with_assets_is_a_clean_error() {
    let dir = Dir::new("damaged");
    dir.write(
        "app/colibri.toml",
        "[package]\nname = \"app\"\n\n[build]\nassets = [\"blob.bin\", \"other.bin\"]\n",
    );
    dir.write(
        "app/main.colibri",
        "import assets\n\nfn main() {\n    match assets.read_bytes(\"other.bin\") {\n        ok(b) => print(\"other {b.len()}\")\n        err(e) => print(\"other failed\")\n    }\n    match assets.read_bytes(\"blob.bin\") {\n        ok(b) => print(\"blob {b.len()}\")\n        err(e) => print(\"blob failed: {e.as[assets.Error]()} {e.message()}\")\n    }\n}\n",
    );
    dir.write_bytes("app/blob.bin", b"ASSET-MARKER-0123456789");
    dir.write_bytes("app/other.bin", b"fine");
    build(&dir, "app", "good");
    assert_eq!(dir.built(&exe("good")).stdout, "other 4\nblob 23\n");
    // A flipped byte inside the asset's data: the program starts, and the read of that asset
    // (and only that one) says the data is damaged, as an error the program can handle.
    let out = damaged(&dir, "flip", |b| {
        let at = find(b, b"ASSET-MARKER");
        b[at + 3] ^= 0x20;
    });
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stdout.starts_with("other 4\nblob failed: some(Other)"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("the check sum of this asset does not match"),
        "{}",
        out.stdout
    );
    assert!(!out.stderr.contains("panicked"), "{}", out.stderr);
    // The executable cut in the middle of the asset, with its trailer put back after the cut.
    damaged_error(&damaged(&dir, "cut", |b| {
        let at = find(b, b"ASSET-MARKER") + 5;
        let trailer: Vec<u8> = b[b.len() - 32..].to_vec();
        b.truncate(at);
        b.extend(trailer);
    }));
    // The executable cut and its trailer made to fit: the assets do not fill what is left.
    damaged_error(&damaged(&dir, "short", |b| {
        let n = b.len();
        let len = u64::from_le_bytes(b[n - 12..n - 4].try_into().unwrap());
        let cut = 3usize;
        b.drain(n - 32 - cut..n - 32);
        let n = b.len();
        b[n - 12..n - 4].copy_from_slice(&(len - cut as u64).to_le_bytes());
    }));
    // The head damaged: the path of an asset, and the length of an asset.
    damaged_error(&damaged(&dir, "path", |b| {
        let at = find(b, b"blob.bin");
        b[at] ^= 0x01;
    }));
    damaged_error(&damaged(&dir, "long", |b| {
        let at = find(b, b"blob.bin") + 8;
        b[at + 1] = 0x7f;
    }));
}

/// A pack whose check sum is right but whose content is not what a build makes.
fn forged(dir: &Dir, name: &str, edit: impl FnOnce(&mut colibri_lang::pack::Pack)) -> Out {
    use colibri_lang::pack::{AssetData, PackAsset};
    let mut bytes = common::plain(fs::read(dir.path(&exe("good"))).unwrap());
    let unsigned_len = bytes.len() as u64;
    let mut pack = colibri_lang::pack::read_from(&mut std::io::Cursor::new(bytes.clone()))
        .unwrap()
        .expect("a built program");
    let trailer = &bytes[bytes.len() - 32..];
    let offset = u64::from_le_bytes(trailer[12..20].try_into().unwrap());
    // The assets as bytes again, to write them anew.
    for asset in &mut pack.assets {
        if let AssetData::Packed { offset: at } = asset.data {
            let from = (pack.assets_at + at) as usize;
            *asset = PackAsset::held(
                asset.path.clone(),
                bytes[from..from + asset.len as usize].to_vec(),
            );
        }
    }
    edit(&mut pack);
    bytes.truncate(offset as usize);
    bytes.extend(pack.append_to(offset).unwrap());
    common::fit(&mut bytes, unsigned_len);
    let target = dir.path(&exe(name));
    fs::write(&target, &bytes).unwrap();
    common::sign(&target);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir.built(&exe(name))
}

#[test]
fn a_pack_with_asset_paths_that_no_build_makes_is_refused_and_a_missing_asset_is_an_error() {
    use colibri_lang::pack::PackAsset;
    let dir = Dir::new("forged");
    dir.write(
        "app/colibri.toml",
        "[package]\nname = \"app\"\n\n[build]\nassets = [\"blob.bin\"]\n",
    );
    dir.write(
        "app/main.colibri",
        "import assets\n\nfn main() {\n    match assets.read_bytes(\"blob.bin\") {\n        ok(b) => print(\"read {b.len()}\")\n        err(e) => print(\"error {e.as[assets.Error]()}\")\n    }\n}\n",
    );
    dir.write_bytes("app/blob.bin", b"12345");
    build(&dir, "app", "good");
    assert_eq!(dir.built(&exe("good")).stdout, "read 5\n");
    let asset = |path: &str| PackAsset::held(path, b"x".to_vec());
    for (name, path) in [
        ("up", "../outside"),
        ("abs", "/etc/hostname"),
        ("empty", ""),
        ("dots", "a/./b"),
        ("back", "a\\b"),
    ] {
        let out = forged(&dir, name, |p| p.assets.push(asset(path)));
        damaged_error(&out);
        assert!(
            out.stderr.contains("an asset has a path that is not valid"),
            "{path}: {}",
            out.stderr
        );
    }
    let out = forged(&dir, "twice", |p| p.assets.push(asset("blob.bin")));
    damaged_error(&out);
    assert!(
        out.stderr.contains("an asset is there twice"),
        "{}",
        out.stderr
    );
    // A pack that lost its asset runs, and the program is told there is none.
    let out = forged(&dir, "lost", |p| p.assets.clear());
    assert_eq!(
        (out.stdout.as_str(), out.code),
        ("error some(NotFound)\n", 0),
        "{}",
        out.stderr
    );
}

#[test]
fn double_star_selects_through_folders_in_run_and_in_a_build() {
    let dir = Dir::new("dstar");
    dir.write(
        "app/colibri.toml",
        "[package]\nname = \"app\"\n\n[build]\nassets = [\"tpl/**/*.html\", \"**/*.css\"]\n",
    );
    dir.write(
        "app/main.colibri",
        "import assets\n\nfn main() {\n    for p in [\"tpl/a.html\", \"tpl/x/y/b.html\", \"tpl/x/c.txt\", \"other/d.html\", \"site.css\", \"deep/er/site.css\", \"deep/er/x.js\"] {\n        match assets.read(p) {\n            ok(t) => print(\"{p}: {t}\")\n            err(e) => print(\"{p}: {e.as[assets.Error]()}\")\n        }\n    }\n    print(assets.list(\"tpl\").expect(\"tpl\"))\n    print(assets.list(\"tpl/x\").expect(\"x\"))\n}\n",
    );
    dir.write("app/tpl/a.html", "A");
    dir.write("app/tpl/x/y/b.html", "B");
    dir.write("app/tpl/x/c.txt", "C");
    dir.write("app/other/d.html", "D");
    dir.write("app/site.css", "S");
    dir.write("app/deep/er/site.css", "SD");
    dir.write("app/deep/er/x.js", "J");
    let want = "tpl/a.html: A\ntpl/x/y/b.html: B\ntpl/x/c.txt: some(NotFound)\nother/d.html: some(NotFound)\nsite.css: S\ndeep/er/site.css: SD\ndeep/er/x.js: some(NotFound)\n[\"a.html\", \"x/\"]\n[\"y/\"]\n";
    let run = dir.colibri(&["run", "app"]);
    assert_eq!(run.stdout, want, "{}", run.stderr);
    let made = build(&dir, "app", "appbin");
    assert!(
        made.stderr.contains("1 source file, 4 assets,"),
        "{}",
        made.stderr
    );
    fs::remove_dir_all(dir.path("app")).unwrap();
    let built = dir.built(&exe("appbin"));
    assert_eq!(
        (built.stdout.as_str(), built.code),
        (want, 0),
        "{}",
        built.stderr
    );
}

#[test]
fn a_name_in_another_case_is_not_found_in_run_and_in_a_build_alike() {
    let dir = Dir::new("case");
    dir.write(
        "app/colibri.toml",
        "[package]\nname = \"app\"\n\n[build]\nassets = [\"data/\"]\n",
    );
    dir.write(
        "app/main.colibri",
        "import assets\n\nfn main() {\n    for p in [\"data/A.TXT\", \"data/a.txt\", \"DATA/A.TXT\", \"data/A.TXT.\", \"data/A.TXT \", \"data/CON\"] {\n        match assets.read(p) {\n            ok(t) => print(\"{p}: {t}\")\n            err(e) => print(\"{p}: {e.as[assets.Error]()}\")\n        }\n    }\n}\n",
    );
    dir.write("app/data/A.TXT", "upper");
    let want = "data/A.TXT: upper\ndata/a.txt: some(NotFound)\nDATA/A.TXT: some(NotFound)\ndata/A.TXT.: some(Invalid)\ndata/A.TXT : some(Invalid)\ndata/CON: some(Invalid)\n";
    let run = dir.colibri(&["run", "app"]);
    assert_eq!(run.stdout, want, "{}", run.stderr);
    build(&dir, "app", "appbin");
    fs::remove_dir_all(dir.path("app")).unwrap();
    let built = dir.built(&exe("appbin"));
    assert_eq!(
        (built.stdout.as_str(), built.code),
        (want, 0),
        "{}",
        built.stderr
    );
}

#[test]
fn a_large_asset_is_packed_and_read_back_whole_and_a_program_that_skips_it_runs() {
    const BIG: usize = 6 * 1024 * 1024;
    let dir = Dir::new("big");
    dir.write(
        "app/colibri.toml",
        "[package]\nname = \"app\"\n\n[build]\nassets = [\"big.bin\", \"small.txt\"]\n",
    );
    let big: Vec<u8> = (0..BIG).map(|i| (i % 251) as u8).collect();
    dir.write_bytes("app/big.bin", &big);
    dir.write("app/small.txt", "small");
    // One program reads only the small asset, the other reads the big one as well.
    dir.write(
        "app/main.colibri",
        "import assets\nimport os\n\nfn main() {\n    print(assets.read(\"small.txt\").expect(\"small\"))\n    if os.args().len() > 0 {\n        let b = assets.read_bytes(\"big.bin\").expect(\"big\")\n        print(\"{b.len()} {b[0]} {b[250]} {b[251]} {b[b.len() - 1]}\")\n    }\n}\n",
    );
    let made = build(&dir, "app", "appbin");
    assert!(
        made.stderr
            .contains(&format!("asset big.bin ({BIG} bytes)")),
        "{}",
        made.stderr
    );
    fs::remove_dir_all(dir.path("app")).unwrap();
    let size = fs::metadata(dir.path(&exe("appbin"))).unwrap().len();
    assert!(size > BIG as u64);
    let skipped = dir.built(&exe("appbin"));
    assert_eq!(
        (skipped.stdout.as_str(), skipped.code),
        ("small\n", 0),
        "{}",
        skipped.stderr
    );
    let mut cmd = Command::new(dir.path(&exe("appbin")));
    cmd.arg("go");
    let whole = dir.exec(cmd);
    let last = big[BIG - 1];
    assert_eq!(
        whole.stdout,
        format!("small\n{BIG} 0 250 0 {last}\n"),
        "{}",
        whole.stderr
    );
}
