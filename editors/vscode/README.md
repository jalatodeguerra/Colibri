# Colibri for VS Code

Syntax highlighting for `.colibri` files, and a client for the Colibri language server
(`colibri lsp`): diagnostics as you type (the whole package is checked, with unsaved buffers),
format document, hover (types and signatures) and go to definition.

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

## Settings

- `colibri.server.path`: the executable (default `colibri`).
- `colibri.interfaces`: `.cbi` files of the host program, relative to the workspace folder, so
  host functions and types check. `.cbi` files at the top of a folder with a `colibri.toml`
  are picked up without listing them. Restart the server (reload the window) after changing.

## Other editors

`colibri lsp` speaks plain LSP over stdio; point any client at it. Pass the interface files as
`initializationOptions`: `{"interfaces": ["engine.cbi"]}`.
