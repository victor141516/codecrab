const vscode = require("vscode");

const documents = new Map();
let changedEmitter;
let stopped = false;

function virtualUri(commandId, index, side, path) {
  const query = new URLSearchParams({ commandId, index: String(index), side });
  return vscode.Uri.parse(
    `codecrab-diff:/${encodeURIComponent(path.replaceAll("\\", "/"))}?${query}`
  );
}

async function openDiff(command) {
  const files = command.files || [];
  if (!files.length) return;
  const selected = Math.min(Math.max(command.focus || 0, 0), files.length - 1);
  for (const [index, file] of files.entries()) {
    const before = virtualUri(command.id, index, "before", file.path);
    const after = virtualUri(command.id, index, "after", file.path);
    documents.set(before.toString(), file.before || "");
    documents.set(after.toString(), file.after || "");
    changedEmitter.fire(before);
    changedEmitter.fire(after);
    if (index === selected) continue;
    await vscode.commands.executeCommand(
      "vscode.diff",
      before,
      after,
      `${file.path} — ${command.title}`,
      { preview: false, preserveFocus: true }
    );
  }
  const file = files[selected];
  await vscode.commands.executeCommand(
    "vscode.diff",
    virtualUri(command.id, selected, "before", file.path),
    virtualUri(command.id, selected, "after", file.path),
    `${file.path} — ${command.title}`,
    { preview: false, preserveFocus: false }
  );
  const editor = vscode.window.activeTextEditor;
  if (editor) {
    const line = Math.max((file.focus_line || 1) - 1, 0);
    const position = new vscode.Position(line, 0);
    editor.revealRange(
      new vscode.Range(position, position),
      vscode.TextEditorRevealType.InCenterIfOutsideViewport
    );
  }
}

async function request(path, options = {}) {
  const origin = process.env.CODECRAB_CONTROL_ORIGIN;
  const instance = process.env.CODECRAB_INSTANCE_ID;
  const token = process.env.CODECRAB_EXTENSION_TOKEN;
  if (!origin || !instance || !token) {
    throw new Error("CodeCrab integration environment is incomplete");
  }
  const response = await fetch(`${origin}/api/code-server/extension/${instance}${path}`, {
    ...options,
    headers: {
      "X-CodeCrab-Extension-Token": token,
      "Content-Type": "application/json",
      ...(options.headers || {})
    }
  });
  if (!response.ok) throw new Error(`CodeCrab bridge returned ${response.status}`);
  return response.status === 204 ? null : response.json();
}

async function poll() {
  while (!stopped) {
    try {
      const commands = await request("/commands");
      for (const command of commands || []) {
        if (command.action === "open_diff") await openDiff(command);
      }
    } catch (error) {
      console.error("CodeCrab extension poll failed", error);
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
}

async function activate(context) {
  changedEmitter = new vscode.EventEmitter();
  context.subscriptions.push(changedEmitter);
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider("codecrab-diff", {
      onDidChange: changedEmitter.event,
      provideTextDocumentContent(uri) {
        return documents.get(uri.toString()) || "";
      }
    })
  );
  context.subscriptions.push(
    vscode.workspace.onDidCloseTextDocument((document) => {
      if (document.uri.scheme === "codecrab-diff") {
        documents.delete(document.uri.toString());
      }
    })
  );
  const files = vscode.workspace.getConfiguration("files");
  await files.update(
    "readonlyInclude",
    { "**": true },
    vscode.ConfigurationTarget.Global
  );
  await files.update(
    "exclude",
    {
      "**/.git": false,
      "**/.svn": false,
      "**/.hg": false,
      "**/CVS": false,
      "**/.DS_Store": false,
      "**/Thumbs.db": false
    },
    vscode.ConfigurationTarget.Global
  );
  await vscode.workspace
    .getConfiguration("explorer")
    .update("excludeGitIgnore", false, vscode.ConfigurationTarget.Global);
  await request("/handshake", { method: "POST", body: "{}" });
  void poll();
}

function deactivate() {
  stopped = true;
  documents.clear();
}

module.exports = { activate, deactivate };
