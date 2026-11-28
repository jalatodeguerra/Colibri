//! Building and caching the project runner (`bindings::build`, `colibri bindings`), with a fake
//! cargo that records its arguments and copies the fixture runner. No real cargo is run.

use std::fs;
use std::path::{Path, PathBuf};

use colibri_lang::bindings::build::{self, Env, Options, Profile};
use colibri_lang::bindings::command::{self, Args};
use colibri_lang::bindings::engine::Inputs;
use colibri_lang::depscmd::DepFlags;

const CRATE_TOML: &str = "[package]\nname = \"colibri-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ncolibri-runtime = \"0.1\"\n";

/// A project folder with a path binding `fixture` in `native/`, a cache folder and a cargo log.
struct Project {
    root: PathBuf,
}

impl Project {
    fn new(name: &str) -> Project {
        let root =
            std::env::temp_dir().join(format!("colibri-build-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let p = Project { root };
        p.write(
            "colibri.toml",
            "[package]\nname = \"app\"\n\n[rust-dependencies]\nfixture = { path = \"native/\" }\n",
        );
        p.write("main.colibri", "fn main() {\n}\n");
        p.write("native/Cargo.toml", CRATE_TOML);
        p.write("native/src/lib.rs", "// the binding\n");
        p
    }

    fn write(&self, rel: &str, text: &str) {
        let full = self.root.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, text).unwrap();
    }

    /// The root the loader reports (links resolved).
    fn real_root(&self) -> PathBuf {
        fs::canonicalize(&self.root).unwrap()
    }

    fn env(&self, extra: &[(&str, &str)]) -> Env {
        let mut cargo_env = vec![
            (
                "FAKE_CARGO_LOG".to_string(),
                self.root.join("cargo.log").display().to_string(),
            ),
            (
                "FAKE_CARGO_RUNNER".to_string(),
                env!("CARGO_BIN_EXE_colibri-runner-fixture").to_string(),
            ),
        ];
        cargo_env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        Env {
            runners: self.root.join("cache").join("runners"),
            cargo: Some(PathBuf::from(env!("CARGO_BIN_EXE_fake-cargo"))),
            cargo_env,
            engine: Inputs::current(),
        }
    }

    /// The lines cargo was called with so far.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("cargo.log"))
            .map(|t| t.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn ensure(&self, env: &Env, options: Options) -> Result<build::Runner, String> {
        let (root, found) = build::project(&self.root)?;
        build::ensure_runner(env, &root, &found, options)
    }

    fn lock(&self) -> PathBuf {
        self.real_root().join(build::LOCK_FILE)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const RUN: Options = Options {
    profile: Profile::Run,
    locked: false,
    offline: false,
};

fn bump_mtime(path: &Path) {
    // Same size, later time: only the modification time tells.
    let f = fs::OpenOptions::new().write(true).open(path).unwrap();
    let t = f.metadata().unwrap().modified().unwrap() + std::time::Duration::from_secs(5);
    f.set_modified(t).unwrap();
}

#[test]
fn an_unchanged_project_does_not_call_cargo_again() {
    let p = Project::new("unchanged");
    let env = p.env(&[]);
    let first = p.ensure(&env, RUN).unwrap();
    assert!(first.built);
    assert!(first.path.is_file());
    let second = p.ensure(&env, RUN).unwrap();
    assert!(!second.built);
    assert_eq!(first.path, second.path);
    assert_eq!(p.calls().len(), 1, "{:?}", p.calls());
    // The line cargo gets: the profile, the manifest, a shared target folder, the one binary.
    let call = &p.calls()[0];
    assert!(
        call.starts_with("build --profile colibri-run --manifest-path "),
        "{call}"
    );
    assert!(call.contains("target-0.1.0"), "{call}");
    assert!(
        !call.contains("--locked") && !call.contains("--offline"),
        "{call}"
    );
}

#[test]
fn touching_the_crate_the_manifest_or_the_lock_calls_cargo() {
    let p = Project::new("touch");
    let env = p.env(&[]);
    let first = p.ensure(&env, RUN).unwrap();

    // A path file with another size.
    p.write("native/src/lib.rs", "// the binding, edited\n");
    let second = p.ensure(&env, RUN).unwrap();
    assert!(second.built);
    assert_ne!(first.path, second.path);
    assert!(first.path.is_file(), "the older runner is kept");

    // The same size and another time.
    bump_mtime(&p.root.join("native/src/lib.rs"));
    let third = p.ensure(&env, RUN).unwrap();
    assert!(third.built);
    assert_eq!(p.calls().len(), 3);

    // A new file in the crate.
    p.write("native/src/more.rs", "\n");
    assert!(p.ensure(&env, RUN).unwrap().built);

    // The crate's own `target/` and `.git` do not count.
    p.write("native/target/debug/junk", "x");
    p.write("native/.git/HEAD", "x");
    assert!(!p.ensure(&env, RUN).unwrap().built);

    // The manifest (a feature of the dependency).
    p.write(
        "colibri.toml",
        "[package]\nname = \"app\"\n\n[rust-dependencies]\nfixture = { path = \"native/\", features = [\"fast\"] }\n",
    );
    assert!(p.ensure(&env, RUN).unwrap().built);

    // The lock.
    let lock = fs::read_to_string(p.lock()).unwrap();
    fs::write(p.lock(), format!("{lock}# edited\n")).unwrap();
    assert!(p.ensure(&env, RUN).unwrap().built);
    assert_eq!(p.calls().len(), 6, "{:?}", p.calls());
    // No change in between: no call.
    assert!(!p.ensure(&env, RUN).unwrap().built);
}

#[test]
fn locked_and_offline_reach_cargo() {
    let p = Project::new("flags");
    let env = p.env(&[]);
    let options = Options {
        profile: Profile::Run,
        locked: true,
        offline: true,
    };
    p.ensure(&env, options).unwrap();
    let call = &p.calls()[0];
    assert!(
        call.contains(" --locked") && call.contains(" --offline"),
        "{call}"
    );
}

#[test]
fn the_release_profile_is_a_runner_of_its_own() {
    let p = Project::new("profiles");
    let env = p.env(&[]);
    let run = p.ensure(&env, RUN).unwrap();
    let release = p
        .ensure(
            &env,
            Options {
                profile: Profile::Release,
                ..RUN
            },
        )
        .unwrap();
    assert!(release.built);
    assert_ne!(run.path, release.path);
    assert!(
        p.calls()[1].starts_with("build --profile release "),
        "{:?}",
        p.calls()
    );
    assert!(!p.ensure(&env, RUN).unwrap().built);
}

#[test]
fn the_lock_is_copied_back_once_and_crlf_is_lf() {
    let p = Project::new("lock");
    let env = p.env(&[]);
    assert!(!p.lock().exists());
    p.ensure(&env, RUN).unwrap();
    let text = fs::read_to_string(p.lock()).unwrap();
    assert!(text.contains("# resolved by fake cargo"), "{text}");
    // The same lock with Windows line ends is the same lock: no call, no rewrite.
    let crlf = text.replace('\n', "\r\n");
    fs::write(p.lock(), &crlf).unwrap();
    assert!(!p.ensure(&env, RUN).unwrap().built);
    assert_eq!(fs::read_to_string(p.lock()).unwrap(), crlf);
}

#[test]
fn a_failed_build_keeps_the_old_runner_and_the_old_interfaces() {
    let p = Project::new("fail");
    let env = p.env(&[]);
    let args = Args {
        path: &p.root,
        release: false,
        output: None,
        check: false,
        deps: DepFlags::default(),
    };
    assert!(command::bindings(&env, &args).unwrap());
    let cbi = p.real_root().join("fixture.cbi");
    let before = fs::read_to_string(&cbi).unwrap();
    assert!(before.contains("fn double"), "{before}");
    let old = p.ensure(&env, RUN).unwrap();
    // A stale-looking interface that a failed build must not touch.
    fs::write(&cbi, "stale\n").unwrap();

    p.write("native/src/lib.rs", "// edited, and it does not compile\n");
    let failing = p.env(&[("FAKE_CARGO_FAIL", "build")]);
    let err = command::bindings(&failing, &args).unwrap_err();
    assert!(err.contains("could not be built"), "{err}");
    assert!(!err.contains("cargo fetch"), "{err}");
    assert!(old.path.is_file());
    assert_eq!(fs::read_to_string(&cbi).unwrap(), "stale\n");
    // The old runner is still the one for the old sources.
    assert!(old.path.is_file());
}

#[test]
fn no_interface_is_written_when_the_first_build_fails() {
    let p = Project::new("firstfail");
    let failing = p.env(&[("FAKE_CARGO_FAIL", "build")]);
    let args = Args {
        path: &p.root,
        release: false,
        output: None,
        check: false,
        deps: DepFlags::default(),
    };
    assert!(command::bindings(&failing, &args).is_err());
    assert!(!p.real_root().join("fixture.cbi").exists());
    assert!(!p.lock().exists());
}

#[test]
fn a_network_failure_suggests_fetch_and_offline() {
    let p = Project::new("network");
    let env = p.env(&[("FAKE_CARGO_FAIL", "network")]);
    let err = p.ensure(&env, RUN).unwrap_err();
    assert!(
        err.contains("cargo fetch") && err.contains("`--offline`"),
        "{err}"
    );
}

#[test]
fn two_runtimes_in_the_resolved_lock_are_refused() {
    let p = Project::new("dup");
    let env = p.env(&[("FAKE_CARGO_DUP_RUNTIME", "1")]);
    let err = p.ensure(&env, RUN).unwrap_err();
    assert!(err.contains("two copies of `colibri-runtime`"), "{err}");
    assert!(
        !p.lock().exists(),
        "the lock is copied back only after a success"
    );
    let runners = p.root.join("cache/runners");
    let kept: Vec<_> = fs::read_dir(&runners)
        .unwrap()
        .flat_map(|d| fs::read_dir(d.unwrap().path()).into_iter().flatten())
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("runner-"))
        .collect();
    assert!(kept.is_empty(), "{kept:?}");
}

#[test]
fn two_builds_at_once_both_end_with_a_working_runner() {
    let p = Project::new("concurrent");
    let paths: Vec<PathBuf> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let env = p.env(&[("FAKE_CARGO_SLEEP_MS", "300")]);
                s.spawn(|| {
                    let env = env;
                    p.ensure(&env, RUN).unwrap().path
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(paths[0], paths[1]);
    let out = std::process::Command::new(&paths[0])
        .arg("__write-interfaces")
        .arg(p.root.join("cbi-out"))
        .status()
        .unwrap();
    assert!(out.success());
    assert!(p.root.join("cbi-out/fixture.cbi").is_file());
    // And the next call is a cache hit.
    assert!(!p.ensure(&p.env(&[]), RUN).unwrap().built);
}

#[test]
fn an_installed_runner_is_never_overwritten_not_even_while_it_runs() {
    let dir = std::env::temp_dir().join(format!("colibri-install-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // `to` is a real runner that is running (it waits for input); `from` holds other bytes.
    let to = dir.join(format!("runner-1{}", std::env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_colibri-runner-fixture"), &to).unwrap();
    let mut running = std::process::Command::new(&to)
        .arg("repl")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let from = dir.join("new-bytes");
    fs::write(&from, "other bytes").unwrap();
    let before = fs::read(&to).unwrap();
    build::install(&from, &to).unwrap();
    assert_eq!(fs::read(&to).unwrap(), before);
    drop(running.stdin.take());
    let _ = running.wait();
    // Running or not, a name that is taken keeps its bytes (a build that lost the race to make
    // it must not touch what the winner made: it may be running, and Windows would refuse).
    let idle = dir.join("runner-3");
    fs::write(&idle, "first bytes").unwrap();
    build::install(&from, &idle).unwrap();
    assert_eq!(fs::read(&idle).unwrap(), b"first bytes");
    // A free name gets the bytes.
    let free = dir.join("runner-2");
    build::install(&from, &free).unwrap();
    assert_eq!(fs::read(&free).unwrap(), b"other bytes");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bindings_writes_the_interface_and_check_compares() {
    let p = Project::new("check");
    let env = p.env(&[]);
    let args = |check| Args {
        path: &p.root,
        release: false,
        output: None,
        check,
        deps: DepFlags::default(),
    };
    let cbi = p.real_root().join("fixture.cbi");
    // Missing: stale. `--check` writes nothing.
    assert!(!command::bindings(&env, &args(true)).unwrap());
    assert!(!cbi.exists());
    // Written.
    assert!(command::bindings(&env, &args(false)).unwrap());
    let text = fs::read_to_string(&cbi).unwrap();
    assert!(
        text.contains("module = \"fixture\"") && text.contains("fn double"),
        "{text}"
    );
    // Fresh: passes and changes nothing.
    assert!(command::bindings(&env, &args(true)).unwrap());
    // Stale: fails and is not touched.
    fs::write(&cbi, "old\n").unwrap();
    assert!(!command::bindings(&env, &args(true)).unwrap());
    assert_eq!(fs::read_to_string(&cbi).unwrap(), "old\n");
    // Repaired.
    assert!(command::bindings(&env, &args(false)).unwrap());
    assert_eq!(fs::read_to_string(&cbi).unwrap(), text);
    // The cargo calls: one build, the rest were cache hits.
    assert_eq!(p.calls().len(), 1);
}

#[test]
fn a_dependency_package_gets_its_interface_in_its_own_folder() {
    let p = Project::new("dep");
    p.write(
        "colibri.toml",
        "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"dep\" }\n",
    );
    p.write("main.colibri", "import dep\nfn main() {\n}\n");
    p.write(
        "dep/colibri.toml",
        "[package]\nname = \"dep\"\n\n[rust-dependencies]\nfixture = { path = \"native/\" }\n",
    );
    p.write("dep/dep.colibri", "pub fn nothing() {}\n");
    p.write("dep/native/Cargo.toml", CRATE_TOML);
    p.write("dep/native/src/lib.rs", "\n");
    fs::remove_dir_all(p.root.join("native")).unwrap();
    let env = p.env(&[]);
    let args = Args {
        path: &p.root,
        release: false,
        output: None,
        check: false,
        deps: DepFlags::default(),
    };
    assert!(command::bindings(&env, &args).unwrap());
    assert!(p.real_root().join("dep/fixture.cbi").is_file());
    assert!(!p.real_root().join("fixture.cbi").exists());
}

#[test]
fn a_script_without_bindings_has_nothing_to_build() {
    let p = Project::new("none");
    p.write("colibri.toml", "[package]\nname = \"app\"\n");
    let err = p.ensure(&p.env(&[]), RUN).unwrap_err();
    assert!(err.contains("declares no `[rust-dependencies]`"), "{err}");
}
