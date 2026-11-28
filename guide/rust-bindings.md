# Rust bindings: building and running

A package can call Rust crates. It names them in `[rust-dependencies]` of its `colibri.toml`, one
key for each crate:

```toml
[rust-dependencies]
native = { path = "native/" }
```

Such a package needs a project runner: the `colibri` you installed plus your binding crates,
built by cargo once and kept in a cache. You do not call the runner yourself.

## Commands that build Rust

`colibri bindings` builds the runner and writes the interface files (`native.cbi`, one for each
key) next to the manifest. Commit them. `colibri bindings --check` fails if one is out of date. It
builds the runner to ask it for the interfaces, so it needs Rust (a CI job that runs it needs the
toolchain, unlike `colibri check`).

`colibri run`, `test` and `repl` (with `--leaks` and `--profile` as well) do the same when the
package declares bindings: they find the runner of the project, build it if it is missing or out
of date, and hand the command over to it, with the same arguments, standard input, output and
exit code as if `colibri` had run the program itself. The first time prints

```console
building the Rust bindings of this project (once, then cached)
```

and compiles Colibri's engine, which takes minutes. After that only a changed binding is rebuilt,
and an unchanged project does not call cargo at all: handing over costs a few milliseconds. You
need Rust (https://rustup.rs). `--offline` and `--locked` are passed on to cargo; the versions it
resolved are kept in `colibri.rust.lock`, which you commit like `colibri.lock`.

A script that has no `colibri.toml`, and a package without `[rust-dependencies]` (and without
dependencies that have them), is run by `colibri` itself and never touches the cache.

When a run had to build the runner, it also brings the interface files up to date: a `.cbi` whose
text differs from what the new runner says is rewritten (one line names it), and then `colibri
check` sees the new signatures. Files in a git dependency are never written. When the runner was
already there and a committed `.cbi` differs from what it says, the run goes on and prints one
warning that names `colibri bindings`, which brings the files up to date.

Two builds at once on one machine take turns: the second says "waiting for another Colibri build"
and goes on when the first is done.

## Watching

`colibri run --watch` works in a package with Rust bindings too. `colibri` stays in front and keeps
the project runner running; what you edit decides what happens.

- A Colibri edit is reloaded inside the running program exactly as without bindings: a change to
  function bodies is patched in place and the program's state is kept, other changes restart it
  (`save_state` and `restore_state` carry state).
- A Rust edit (a file of a binding crate, or of a crate it names with `path =`, or a change to
  `[rust-dependencies]`) builds the runner again. When the build works, the program is stopped and
  started again in the new runner, which prints "Rust code changed: rebuilt the runner and
  restarted" (state is not carried over a Rust change). When it fails, cargo's errors are printed
  and the old runner keeps running until the next edit.
- The command ends with the program's exit code. Ctrl-C ends the program and the command. If
  `colibri` itself is killed, the runner ends too.

## Shipping with `colibri build`

`colibri build` of a package with `[rust-dependencies]` writes one executable: the release runner
of the project (the one `colibri bindings --release` builds, once, then cached) with the program
after it. It runs like `colibri run` does, needs no Rust, no cargo and no cache where it runs, and
says how big the runner is. If a committed `.cbi` differs from the runner's own interface, the build
stops and names `colibri bindings`, since the program would be checked against the wrong interface:

```text
building the Rust bindings of this project (once, then cached)
  runner /home/me/.colibri/runners/.../runner-9f2c (SIZE bytes, with the Rust bindings of `native`)
built app (3 source files, SIZE bytes)
```

For another platform, build the runner on that platform with `colibri bindings --release -o FILE`
and name it: `colibri build --target windows-x86_64 --runner FILE`. The runner must be of the same
`colibri` version and hold exactly the bindings of the package; a plain `colibri`, or the runner of
another project, is refused; the check reads the crates and versions, not the code, so a runner
built from edited sources at the same crate version passes it and may still fail when the program
starts (build the runner from the sources the package builds from). A built program is not a
runner either (`--runner` of `run`, `test` and `repl` refuses it). Nothing is cross-compiled. `colibri build --web` refuses a package
with Rust bindings (they cannot run in a page yet).

## Commands that never call cargo

`colibri check`, `fmt`, `doc`, `new`, `update` (git dependencies only) and the language server
read the committed `.cbi` files, so an editor and a CI check need no Rust toolchain.
`colibri debug` does not use the runner yet.

## A runner built elsewhere

`colibri bindings -o FILE` copies the runner it built to FILE. On a machine without Rust (a CI
job that only runs programs), give it to `run`, `test` or `repl`:

```console
$ colibri run --runner ./app-runner main.colibri
```

`--runner` goes before the script (the folder of the script is the package): what follows the
script belongs to the program. `colibri` checks the file first. It must be a `colibri` of the same
version, and hold exactly the bindings the package declares (same keys, crates, versions,
features). A plain `colibri`, or a runner of other bindings, is refused with a message that names
the bindings it lacks.

## When the runner is rebuilt

`colibri` notices edits to your binding crates and the crates they name with `path =`, and to the
Colibri source it was built from. It does not notice what a build script reads elsewhere,
`RUSTFLAGS`, or an edit that leaves a file's size and time unchanged. To force a rebuild, delete
the project's folder under `runners/` in the cache (the cache is the one git dependencies use,
`COLIBRI_HOME` or the system cache; everything in it is safe to delete).
