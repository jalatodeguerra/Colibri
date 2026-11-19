// The Colibri VS Code client: starts `colibri lsp` and lets vscode-languageclient talk to it.
// Syntax highlighting needs no code (see package.json "contributes").
const { workspace } = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

let client;

function activate(context) {
  const config = workspace.getConfiguration("colibri");
  const command = config.get("server.path") || "colibri";
  const serverOptions = { command, args: ["lsp"] };
  const clientOptions = {
    documentSelector: [{ scheme: "file", language: "colibri" }],
    // `.cbi` files describing what the host program provides, so host functions check.
    initializationOptions: { interfaces: config.get("interfaces") || [] },
  };
  client = new LanguageClient("colibri", "Colibri", serverOptions, clientOptions);
  context.subscriptions.push(client);
  client.start();
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
