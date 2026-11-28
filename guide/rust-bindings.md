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
key) next to the manifest. Commit them. `colibri bindings --check` fails if one is out of date,
which is what a CI job wants.

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

Two builds at once on one machine take turns: the second says "waiting for another Colibri build"
and goes on when the first is done.

## Commands that never call cargo

`colibri check`, `fmt`, `doc`, `new`, `update` (git dependencies only) and the language server
read the committed `.cbi` files, so an editor and a CI check need no Rust toolchain.
`colibri debug` does not use the runner yet.

## A runner built elsewhere

`colibri bindings -o FILE` copies the runner it built to FILE. On a machine without Rust (a CI
job that only runs programs), give it to `run`, `test` or `repl`:

```console
$ colibri run --runner ./app-runner src/main.colibri
```

`--runner` goes before the script: what follows the script belongs to the program. `colibri`
checks the file first. It must be a `colibri` of the same version, and hold exactly the bindings
the package declares (same keys, crates, versions, features). A plain `colibri`, or a runner of
other bindings, is refused with a message that names the bindings it lacks.

## When the runner is rebuilt

`colibri` notices edits to your binding crates and the crates they name with `path =`, and to the
Colibri source it was built from. It does not notice what a build script reads elsewhere,
`RUSTFLAGS`, or an edit that leaves a file's size and time unchanged. To force a rebuild, delete
the project's folder under `runners/` in the cache (the cache is the one git dependencies use,
`COLIBRI_HOME` or the system cache; everything in it is safe to delete).
