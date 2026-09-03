/**
 * The VS Code client: one `ya-lsp` process per workspace folder.
 *
 * One per folder because the server takes a single `rootUri` and everything it does is scoped to
 * it — the index, the gem discovery, `ya-lsp.toml`, and the "is this the user's own code?" test
 * that three features turn on. A single process handed several roots would have to re-derive all
 * of that per request.
 *
 * A folder starts eagerly only when it is the workspace's only one. A monorepo with a dozen
 * folders would otherwise index a dozen bundles before the editor finishes opening, for folders
 * nobody has looked at; every folder of a multi-root workspace starts when a Ruby file inside it
 * is opened.
 */

import * as fs from 'node:fs';
import * as os from 'node:os';
import * as vscode from 'vscode';
import {
  DidChangeConfigurationNotification,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from 'vscode-languageclient/node';

import { RESTART_REQUIRED, Settings, serverEnvironment, serverOptions } from './config';
import { resolveServer } from './server';

const clients = new Map<string, LanguageClient>();
const channels = new Map<string, vscode.LogOutputChannel>();
/** Folders whose server could not be found, so the error is reported once and not per file. */
const reported = new Set<string>();

let context: vscode.ExtensionContext;

export async function activate(extensionContext: vscode.ExtensionContext): Promise<void> {
  context = extensionContext;

  context.subscriptions.push(
    vscode.commands.registerCommand('ya-lsp.restart', restartAll),
    vscode.commands.registerCommand('ya-lsp.showOutput', showOutput),
    vscode.workspace.onDidOpenTextDocument(startForDocument),
    vscode.workspace.onDidChangeWorkspaceFolders(onFoldersChanged),
    vscode.workspace.onDidChangeConfiguration(onConfigurationChanged)
  );

  // The one folder eagerly, so a single-folder project — which is nearly all of them — has a
  // server warming up before the user's first keystroke.
  //
  // Only when there is exactly one. `folders[0]` in a multi-root workspace is whichever folder
  // the `.code-workspace` happens to list first, which says nothing about whether it holds any
  // Ruby: activation is `onLanguage:ruby`, so opening a file in the *second* folder would start
  // a server on the first, index a folder nobody asked about, and warn that it found no Ruby
  // there. Multi-root falls through to the lazy path below, which is what the rest of this
  // module already documents.
  const folders = vscode.workspace.workspaceFolders;
  if (folders?.length === 1 && folders[0]) {
    await start(folders[0]);
  }
  // Activation is usually *caused* by opening a Ruby file, and it may be in a different folder
  // than the first one.
  for (const document of vscode.workspace.textDocuments) {
    await startForDocument(document);
  }
}

export async function deactivate(): Promise<void> {
  await Promise.all([...clients.keys()].map(stop));
}

async function startForDocument(document: vscode.TextDocument): Promise<void> {
  if (document.languageId !== 'ruby' || document.uri.scheme !== 'file') {
    return;
  }
  const folder = vscode.workspace.getWorkspaceFolder(document.uri);
  // A loose file with no folder has no root to index, and the server needs one. The editor's
  // own word-based suggestions still work, which is the right outcome for a scratch file.
  if (folder) {
    await start(folder);
  }
}

async function start(folder: vscode.WorkspaceFolder): Promise<void> {
  const key = folder.uri.toString();
  if (clients.has(key)) {
    return;
  }

  const settings = settingsFor(folder);
  const resolved = resolveServer({
    configured: settings.explicit<string>('serverPath') ?? '',
    extensionPath: context.extensionPath,
    workspaceFolder: folder.uri.fsPath,
    home: os.homedir(),
    platform: process.platform,
    isExecutable,
  });

  if (resolved.kind === 'missing') {
    if (!reported.has(key)) {
      reported.add(key);
      void vscode.window.showErrorMessage(resolved.message);
    }
    return;
  }
  reported.delete(key);

  const server: ServerOptions = {
    command: resolved.command,
    args: ['--stdio'],
    transport: TransportKind.stdio,
    options: { env: serverEnvironment(settings, process.env) },
  };

  const options: LanguageClientOptions = {
    // The pattern is the only thing that keeps this client from also claiming files in a sibling
    // folder — `workspaceFolder` below sets the `rootUri` and nothing else. It is written in the
    // protocol's own shape (a `baseUri` string, LSP 3.18's `RelativePattern`) rather than as a
    // `vscode.RelativePattern`: the client runs every selector through `asDocumentSelector`,
    // which recognises only this form and silently converts anything else to `undefined` — and
    // an undefined pattern matches on language and scheme alone, so every folder's client would
    // claim every folder's files. Given this form it builds the `vscode.RelativePattern` itself,
    // which is also what makes the path separator right on Windows by construction.
    documentSelector: [
      {
        scheme: 'file',
        language: 'ruby',
        pattern: { baseUri: folder.uri.toString(), pattern: '**/*' },
      },
    ],
    workspaceFolder: folder,
    outputChannel: channelFor(folder),
    initializationOptions: serverOptions(settings),
    // No `synchronize.fileEvents`. The server registers its own watchers through
    // `client/registerCapability` — `ya-lsp.toml` as of v0.2.0, and everything `index.include`
    // covers as of v0.3.0 — which is what makes reload and on-disk freshness work in editors
    // that have no extension to bring one, and the client installs that registration itself.
    // Passing one here as well would mean two watchers on every file, so two
    // `didChangeWatchedFiles` per save and every file indexed twice per `git checkout`.
  };

  // The id is also the settings prefix the client reads `trace.server` from.
  const client = new LanguageClient('ya-lsp', `ya-lsp (${folder.name})`, server, options);
  clients.set(key, client);
  try {
    await client.start();
  } catch (error) {
    clients.delete(key);
    void vscode.window.showErrorMessage(`ya-lsp failed to start: ${describe(error)}`);
  }
}

async function stop(key: string): Promise<void> {
  const client = clients.get(key);
  if (!client) {
    return;
  }
  clients.delete(key);
  try {
    await client.stop();
  } catch {
    // A server that has already died cannot be stopped politely, and there is nothing the user
    // would do with the news.
  }
}

async function restartAll(): Promise<void> {
  const folders = [...clients.keys()];
  await Promise.all(folders.map(stop));
  reported.clear();
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    if (folders.includes(folder.uri.toString())) {
      await start(folder);
    }
  }
}

async function onFoldersChanged(event: vscode.WorkspaceFoldersChangeEvent): Promise<void> {
  for (const folder of event.removed) {
    const key = folder.uri.toString();
    // `stop` takes the client with it; the channel is this function's to close.
    await stop(key);
    channels.get(key)?.dispose();
    channels.delete(key);
    reported.delete(key);
  }
  // Added folders start lazily, the same as the ones that were there at startup.
}

async function onConfigurationChanged(event: vscode.ConfigurationChangeEvent): Promise<void> {
  for (const [key, client] of [...clients]) {
    const folder = vscode.workspace.workspaceFolders?.find((f) => f.uri.toString() === key);
    if (!folder || !event.affectsConfiguration('ya-lsp', folder)) {
      continue;
    }

    // The server reads its log filter from the environment before it has a client to be
    // configured by, and the binary to run is decided when the process is spawned. Both are
    // settings the user can change, so the extension makes the change take rather than leaving
    // it quietly inert until the next window reload.
    if (RESTART_REQUIRED.some((setting) => event.affectsConfiguration(setting, folder))) {
      await stop(key);
      await start(folder);
      continue;
    }

    // `null`, not `undefined`: LSP's `settings` field is required, and the server reads a null
    // one as "the client has nothing to say", which is what an all-defaults workspace means.
    await client.sendNotification(DidChangeConfigurationNotification.type, {
      settings: serverOptions(settingsFor(folder)) ?? null,
    });
  }
}

function showOutput(): void {
  const active = vscode.window.activeTextEditor?.document.uri;
  const folder = active ? vscode.workspace.getWorkspaceFolder(active) : undefined;
  const channel =
    (folder && channels.get(folder.uri.toString())) ?? [...channels.values()][0];
  channel?.show();
}

function channelFor(folder: vscode.WorkspaceFolder): vscode.LogOutputChannel {
  const key = folder.uri.toString();
  let channel = channels.get(key);
  if (!channel) {
    // `{ log: true }` because `vscode-languageclient` 10 types `outputChannel` as a
    // `LogOutputChannel`. It also gives the channel its own level selector in the editor.
    channel = vscode.window.createOutputChannel(`ya-lsp (${folder.name})`, { log: true });
    channels.set(key, channel);
    context.subscriptions.push(channel);
  }
  return channel;
}

/**
 * Read settings through `inspect`, so that "the user set this" and "this is the package.json
 * default" stay distinguishable. See `Settings` for why that matters.
 */
function settingsFor(folder: vscode.WorkspaceFolder): Settings {
  const configuration = vscode.workspace.getConfiguration('ya-lsp', folder);
  return {
    explicit<T>(key: string): T | undefined {
      const values = configuration.inspect<T>(key);
      return (
        values?.workspaceFolderValue ??
        values?.workspaceValue ??
        values?.globalValue ??
        undefined
      );
    },
  };
}

function isExecutable(candidate: string): boolean {
  try {
    return fs.statSync(candidate).isFile();
  } catch {
    return false;
  }
}


function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
