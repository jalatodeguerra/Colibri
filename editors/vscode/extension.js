// The Colibri VS Code client: starts `colibri lsp` for the language server and `colibri debug` for
// the debugger. Syntax highlighting needs no code (see package.json "contributes").
const vscode = require("vscode");

let client;

// The `colibri` executable: the one setting both tools use.
function colibriPath() {
  return vscode.workspace.getConfiguration("colibri").get("server.path") || "colibri";
}

// Starts `colibri debug`, which speaks the Debug Adapter Protocol on standard input and output.
class DebugAdapterFactory {
  createDebugAdapterDescriptor() {
    return new vscode.DebugAdapterExecutable(colibriPath(), ["debug"]);
  }
}

// Pressing F5 with no `launch.json` (or an empty configuration) debugs the file in the editor.
class DebugConfigurationProvider {
  resolveDebugConfiguration(folder, config) {
    // `null` makes VS Code open launch.json; `undefined` would abort without a word.
    if (!config.type && !config.request && !config.name) {
      const editor = vscode.window.activeTextEditor;
      if (!editor || editor.document.languageId !== "colibri") {
        return null;
      }
      return {
        type: "colibri",
        request: "launch",
        name: "Colibri: launch the current file",
        program: "${file}",
        // With no folder open `${workspaceFolder}` cannot be resolved.
        cwd: folder ? "${workspaceFolder}" : "${fileDirname}",
      };
    }
    if (!config.program) {
      return vscode.window
        .showInformationMessage("A Colibri launch configuration needs `program`.")
        .then(() => undefined);
    }
    return config;
  }
}

function activate(context) {
  // The debugger first: it does not need the language client's npm dependency.
  context.subscriptions.push(
    vscode.debug.registerDebugAdapterDescriptorFactory("colibri", new DebugAdapterFactory()),
    vscode.debug.registerDebugConfigurationProvider("colibri", new DebugConfigurationProvider()),
  );
  try {
    startLanguageClient(context);
  } catch (e) {
    vscode.window.showWarningMessage(
      `Colibri: the language server did not start (${e.message}). Run \`npm install\` in the extension folder.`,
    );
  }
}

function startLanguageClient(context) {
  const { LanguageClient } = require("vscode-languageclient/node");
  const config = vscode.workspace.getConfiguration("colibri");
  const serverOptions = { command: colibriPath(), args: ["lsp"] };
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
