# Colibri for VS Code

Syntax highlighting for `.colibri` files, and a client for the Colibri language server
(`colibri lsp`): diagnostics as you type (the whole package is checked, with unsaved buffers),
format document, hover (types and signatures) and go to definition. It also contributes a debugger
(`colibri debug`): breakpoints, stepping, the call stack, variables and coroutines as threads.

## Install

The extension needs the `colibri` executable on your `PATH` (or set `colibri.server.path`):
`cargo install --path crates/colibri-cli` from the repo root.

The client uses `vscode-languageclient`, so install its dependency once:

```sh
cd editors/vscode
npm install
```

Then symlink or copy this folder to `~/.vscode/extensions/colibri` (on Windows
`%USERPROFILE%\.vscode\extensions\colibri`) and reload VS Code. Or run
`code --extensionDevelopmentPath=editors/vscode` from the repo root to try it without installing.
Without `npm install` the extension fails to activate (highlighting needs no code, but VS Code
loads `extension.js` for the language server).

## Debugging

Press F5 in a `.colibri` file to debug it, or add a configuration to `.vscode/launch.json` (the
snippets "Colibri: Launch ..." write one):

```json
{
  "type": "colibri",
  "request": "launch",
  "name": "Colibri: launch the current file",
  "program": "${file}",
  "cwd": "${workspaceFolder}"
}
```

Attributes: `program` (a script or a package folder; required), `args`, `cwd`, `features`
(fixed by the first launch of a session), `stopOnEntry`, `noDebug` (what "Run Without
Debugging" sets) and `watch` (reload the program when its files change while its coroutines run;
breakpoints stay where they were set). The debugger needs only the `colibri` executable, not
`npm install`. See `guide/debugging.md` for what it can and cannot do.

`node editors/vscode/check-package.mjs` checks the manifest (CI runs it).

## Settings

- `colibri.server.path`: the executable (default `colibri`), for the language server and the
  debugger.
- `colibri.interfaces`: `.cbi` files of the host program, relative to the workspace folder, so
  host functions and types check. `.cbi` files at the top of a folder with a `colibri.toml`
  are picked up without listing them. Restart the server (reload the window) after changing.

## Other editors

`colibri lsp` speaks plain LSP over stdio; point any client at it. Pass the interface files as
`initializationOptions`: `{"interfaces": ["engine.cbi"]}`.
