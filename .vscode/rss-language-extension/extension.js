const path = require("node:path");
const vscode = require("vscode");
const {
  LanguageClient,
  TransportKind
} = require("vscode-languageclient/node");

let client;

function resolveServerModule(context) {
  return context.asAbsolutePath(path.join("server", "server.js"));
}

async function activate(context) {
  const config = vscode.workspace.getConfiguration("rustscript");
  const wasmPath = config.get("languageServer.wasmPath", "").trim();

  const serverOptions = {
    run: {
      module: resolveServerModule(context),
      transport: TransportKind.ipc,
      options: {
        env: {
          ...process.env,
          RUSTSCRIPT_LINT_WASM: wasmPath
        }
      }
    },
    debug: {
      module: resolveServerModule(context),
      transport: TransportKind.ipc,
      options: {
        execArgv: ["--nolazy", "--inspect=6010"],
        env: {
          ...process.env,
          RUSTSCRIPT_LINT_WASM: wasmPath
        }
      }
    }
  };

  const clientOptions = {
    documentSelector: [{ language: "rustscript", scheme: "file" }],
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher("**/*.rss")
    },
    outputChannelName: "RustScript Language Server"
  };

  client = new LanguageClient(
    "rustscriptLanguageServer",
    "RustScript Language Server",
    serverOptions,
    clientOptions
  );

  context.subscriptions.push(client.start());
}

async function deactivate() {
  if (!client) {
    return;
  }
  await client.stop();
}

module.exports = {
  activate,
  deactivate
};
