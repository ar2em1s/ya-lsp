/**
 * The VS Code client: one `ya-lsp` process per workspace folder.
 *
 * **One per folder** because the server takes a single `rootUri`, and everything it does is scoped
 * to it: the index, gem discovery, `ya-lsp.toml`, and the "is this the user's own code?" test three
 * features depend on. One process with several roots would re-derive all of that per request.
 *
 * **Eager only for a single-folder workspace.** In a multi-root workspace each folder's server
 * starts when a Ruby file inside it is opened; otherwise a monorepo with a dozen folders would
 * index a dozen bundles, for folders nobody has looked at, before the editor finishes opening.
 */

import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import * as vscode from 'vscode';
import {
  DidChangeConfigurationNotification,
  LanguageClient,
  LanguageClientOptions,
  Middleware,
  RegistrationParams,
  ServerOptions,
  TransportKind,
} from 'vscode-languageclient/node';

import { Claims, DocumentFilter, Registration, claimedByNestedFolder } from './claims';
import { RESTART_REQUIRED, Settings, serverEnvironment, serverOptions } from './config';
import { FolderFiles, RUBOCOP_EXTENSION, usesRubocop } from './rubocop';
import { resolveServer } from './server';

/**
 * The language ids this extension serves.
 *
 * - **`erb`** is contributed by the manifest with ruby-lsp's id and extensions, so an installed ERB
 *   grammar keeps working: a grammar binds to the id, and two contributions of one id merge. ya-lsp
 *   ships no grammar (it is a language client, not a syntax); semantic tokens colour the Ruby
 *   either way.
 * - **`ruby`** is contributed only to add `.jbuilder`, `.builder` and `.ruby`: Rails template
 *   handlers that are plain Ruby. VS Code opens an unclaimed extension as plain text, which
 *   activates nothing. Adding extensions to the existing id merges the contributions, so the
 *   built-in Ruby grammar still colours them.
 *
 * There is no third language: the server claims `ruby` and `erb`, and `tests/vscode_manifest.rs`
 * fails on an activation event for anything else.
 */
export const LANGUAGES = ['ruby', 'erb'];

/**
 * Which documents a folder's client claims: that folder's, and nothing else.
 *
 * **The selector is the only gate on what the client sends.**
 * `LanguageClientOptions.workspaceFolder` sets `rootUri` and filters nothing. So this returns
 * exactly the files the client will `didOpen` and answer for; everything else scores 0 in
 * `languages.match`, and the server never hears of it.
 *
 * **Nothing here names a file outside the folder, on purpose.** Gem sources, Ruby's stdlib and
 * their RBS live outside every folder, and the server answers about all three. But which
 * directories those are is `workspace/gems.rs`' business (the bundle parse, `require_paths`,
 * vendored versus installed, an engine's `app/`). A TypeScript copy would drift silently, because
 * an unclaimed file produces silence, not an error. So the server names its own roots after the
 * handshake, over `client/registerCapability`, and `claims.ts` decides which client takes each.
 *
 * **The pattern is the protocol's shape, a `baseUri` string, never a `vscode.RelativePattern`.**
 * The client runs every selector through `asDocumentSelector`, which recognises only this form and
 * silently turns anything else into `undefined`, and an undefined pattern does not narrow: it
 * *widens* to language and scheme alone. Given this shape, the client builds the
 * `vscode.RelativePattern` itself, which gets Windows path separators right by construction.
 */
export function documentSelector(folderUri: string): DocumentFilter[] {
  return LANGUAGES.map((language) => ({
    scheme: 'file',
    language,
    pattern: { baseUri: folderUri, pattern: '**/*' },
  }));
}

const clients = new Map<string, LanguageClient>();
const channels = new Map<string, vscode.LogOutputChannel>();
/** Folders whose server could not be found, so the error is reported once, not per file. */
const reported = new Set<string>();
/** Folders already asked about RuboCop, so the hint appears once per session, not per Ruby file. */
const suggested = new Set<string>();
/**
 * Which folder's server answers about each root outside every folder.
 *
 * One ledger for the window, because the question only exists between clients: two folders on one
 * Ruby register the same gem roots, and two providers over one file means the same hover twice.
 */
const claims = new Claims();

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

  // Start the one folder eagerly, so a single-folder project (nearly all of them) has a server
  // warming up before the first keystroke.
  //
  // Only when there is exactly one. `folders[0]` in a multi-root workspace is whichever folder the
  // `.code-workspace` lists first, which says nothing about Ruby. Activation is `onLanguage:ruby`,
  // so opening a file in the *second* folder would start a server on the first, index a folder
  // nobody asked about, and warn that it has no Ruby. Multi-root takes the lazy path below.
  const folders = vscode.workspace.workspaceFolders;
  if (folders?.length === 1 && folders[0]) {
    await start(folders[0]);
  }
  // Activation is usually *caused* by opening a Ruby file, which may be in any folder.
  for (const document of vscode.workspace.textDocuments) {
    await startForDocument(document);
  }
}

export async function deactivate(): Promise<void> {
  await Promise.all([...clients.keys()].map(stop));
}

async function startForDocument(document: vscode.TextDocument): Promise<void> {
  if (!LANGUAGES.includes(document.languageId) || document.uri.scheme !== 'file') {
    return;
  }
  const folder = vscode.workspace.getWorkspaceFolder(document.uri);
  // A loose file with no folder has no root to index, and the server needs one. The editor's
  // word-based suggestions still work, which is right for a scratch file.
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
    options: { env: serverEnvironment(process.env) },
  };

  const options: LanguageClientOptions = {
    documentSelector: documentSelector(key),
    workspaceFolder: folder,
    outputChannel: channelFor(folder),
    initializationOptions: serverOptions(settings),
    middleware: {
      ...narrowing(key),
      // The one place every client is visible at once, which this decision needs. The server
      // registers the roots it has answers about; `claims` drops the ones another folder's server
      // claimed first, and forwards everything it does not recognise. The file watcher's
      // registration carries no selector and must arrive exactly as sent.
      handleRegisterCapability: (params, next): Promise<void> => {
        const narrowed = claims.narrow(key, params.registrations as Registration[], folderUris());
        // `next` is typed as the protocol's `RequestHandler`, whose second argument is a
        // cancellation token, but the client builds it as
        // `nextParams => this.doRegisterCapability(nextParams)` and has no token to pass. Narrowed
        // to its real shape instead of handing it an invented token.
        const forward = next as unknown as (
          forwarded: RegistrationParams
        ) => void | Promise<void>;
        return Promise.resolve(forward({ registrations: narrowed }));
      },
      // A generated document belongs to exactly one server in this window, but the client registers
      // a content provider per server for the one scheme they all serve. VS Code tries the
      // providers in turn and takes the first answer, but a rejection stops the loop, and a server
      // that did not write the document answers an error by design. So a failure becomes "nothing
      // here", and the next provider (the one that has it) gets asked.
      provideTextDocumentContent: async (uri, token, next) => {
        try {
          return await next(uri, token);
        } catch {
          return null;
        }
      },
    },
    // No `synchronize.fileEvents`. The server registers its own watchers through
    // `client/registerCapability` (`ya-lsp.toml` and everything `index.include` covers), which
    // makes reloads work even in editors with no extension, and the client installs that
    // registration itself. A second watcher here would mean two `didChangeWatchedFiles` per save,
    // and every file indexed twice per `git checkout`.
  };

  // The id is also the settings prefix the client reads `trace.server` from.
  const client = new LanguageClient('ya-lsp', `ya-lsp (${folder.name})`, server, options);
  clients.set(key, client);
  try {
    await client.start();
  } catch (error) {
    clients.delete(key);
    claims.release(key);
    void vscode.window.showErrorMessage(`ya-lsp failed to start: ${describe(error)}`);
    return;
  }
  // After the server is up, not before: a folder whose server failed has a worse problem than its
  // linter, and two notifications about one folder is one too many.
  void suggestRubocop(folder, settings);
}

/**
 * Offer RuboCop's own extension to a project that lints with RuboCop.
 *
 * ya-lsp reports parse errors and Prism's warnings and stops there, because every cop is Ruby. The
 * protocol's answer is a second server, not a proxy inside this one. So the extension says so once,
 * where the user is, not only in a README. `rubocop.hint` turns it off; "Don't show again" writes
 * it.
 */
async function suggestRubocop(
  folder: vscode.WorkspaceFolder,
  settings: Settings
): Promise<void> {
  const key = folder.uri.toString();
  if (suggested.has(key) || settings.explicit<boolean>('rubocop.hint') === false) {
    return;
  }
  // Marked before the checks: this means "we have considered this folder", and a second `start` for
  // the same folder must not re-ask while the first is still waiting.
  suggested.add(key);
  if (vscode.extensions.getExtension(RUBOCOP_EXTENSION) || !usesRubocop(filesIn(folder))) {
    return;
  }

  const install = 'Install RuboCop';
  const never = "Don't show again";
  const chosen = await vscode.window.showInformationMessage(
    'This project lints with RuboCop, and ya-lsp does not run Ruby — it reports parse errors ' +
      "and warnings only. RuboCop's own language server can run alongside ya-lsp for the cop " +
      'offences and for formatting.',
    install,
    never
  );
  if (chosen === install) {
    await vscode.commands.executeCommand(
      'workbench.extensions.installExtension',
      RUBOCOP_EXTENSION
    );
  } else if (chosen === never) {
    // Global, not per folder: the answer is about this user's taste, and being asked again in the
    // next project is what they just declined.
    await vscode.workspace
      .getConfiguration('ya-lsp')
      .update('rubocop.hint', false, vscode.ConfigurationTarget.Global);
  }
}

/** The folder's files, read from disk, for the one question `rubocop.ts` asks of them. */
function filesIn(folder: vscode.WorkspaceFolder): FolderFiles {
  return {
    read(relative: string): string | undefined {
      try {
        return fs.readFileSync(path.join(folder.uri.fsPath, relative), 'utf8');
      } catch {
        // Absent, a directory, or unreadable: all mean the same here.
        return undefined;
      }
    },
  };
}

/**
 * Stop one folder's client and give up the roots it had claimed.
 *
 * **Roots are released here, not by the caller**, because every way out of a running client passes
 * through this function, and a root still owned by a dead process is a gem file nobody answers
 * about.
 *
 * Whether to rebuild anyone to pick it up is the caller's question: a client being restarted claims
 * its own roots back a moment later, and `onFoldersChanged` is the one place the loss is permanent.
 */
async function stop(key: string): Promise<string[]> {
  const client = clients.get(key);
  if (!client) {
    return [];
  }
  clients.delete(key);
  const orphaned = claims.release(key);
  try {
    await client.stop();
  } catch {
    // A server that already died cannot be stopped politely, and the user could do nothing with the
    // news.
  }
  return orphaned;
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
  const orphaned = new Set<string>();
  for (const folder of event.removed) {
    const key = folder.uri.toString();
    // `stop` takes the client with it and returns whoever else wanted its roots; closing the
    // channel is this function's job.
    for (const waiting of await stop(key)) {
      orphaned.add(waiting);
    }
    channels.get(key)?.dispose();
    channels.delete(key);
    reported.delete(key);
    suggested.delete(key);
  }

  // Only a *claim* can be invalidated by a removal: the selector has the same shape for one folder
  // or twelve. The removed folder may have been answering about the gems, and a client that lost a
  // root cannot pick it up later, because its selector is fixed at construction. So only the
  // clients that asked for a now-unowned root are rebuilt.
  for (const key of orphaned) {
    const folder = vscode.workspace.workspaceFolders?.find((f) => f.uri.toString() === key);
    if (folder) {
      await stop(key);
      await start(folder);
    }
  }
  // Added folders otherwise start lazily, like the ones present at startup.
}

async function onConfigurationChanged(event: vscode.ConfigurationChangeEvent): Promise<void> {
  for (const [key, client] of [...clients]) {
    const folder = vscode.workspace.workspaceFolders?.find((f) => f.uri.toString() === key);
    if (!folder || !event.affectsConfiguration('ya-lsp', folder)) {
      continue;
    }

    // The binary is chosen when the process spawns, so a `RESTART_REQUIRED` setting only takes
    // effect in a fresh process. Restart it now instead of leaving the change inert until the next
    // window reload. Every other setting reaches the running server below.
    if (RESTART_REQUIRED.some((setting) => event.affectsConfiguration(setting, folder))) {
      await stop(key);
      await start(folder);
      continue;
    }

    // `null`, not `undefined`: LSP's `settings` field is required, and the server reads null as
    // "the client has nothing to say", which is what an all-defaults workspace means.
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
    // `{ log: true }`, because `vscode-languageclient` 10 types `outputChannel` as a
    // `LogOutputChannel`. It also gives the channel its own level selector.
    channel = vscode.window.createOutputChannel(`ya-lsp (${folder.name})`, { log: true });
    channels.set(key, channel);
    context.subscriptions.push(channel);
  }
  return channel;
}

/**
 * Read settings through `inspect`, so "the user set this" stays distinct from "this is the
 * package.json default". `Settings` says why that matters.
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


/**
 * Whether this client is the one to send about `document`.
 *
 * A request naming no document (`initialize`, `workspace/symbol`, `shutdown`) is always this
 * client's: it is about the folder, not a file, and the folder is its own.
 */
/**
 * The middleware that keeps one folder's client out of a nested folder's files.
 *
 * Exported because this half of the nesting fix is *wiring*, which `claims.ts` cannot test: the
 * predicate there is pure and tested directly. Two wiring mistakes fail silently, in opposite
 * directions:
 * - reading the document from the wrong place in the parameters leaves every answer doubled;
 * - passing the folder and the document in the wrong order makes the client answer nothing, with no
 *   error.
 * `activation.test.ts` drives this against the stubbed editor.
 */
export function narrowing(folder: string): Middleware {
  return {
    // Drop every request about a document a *nested* workspace folder holds, before it goes.
    // - `getWorkspaceFolder` resolves such a file to the innermost folder, but `documentSelector`
    //   cannot: an LSP glob cannot subtract a path, so this client claims a nested folder's files
    //   too, and every answer shows twice.
    // - The folder list is read per call, not captured, because `onDidChangeWorkspaceFolders` can
    //   add a nested folder under a running client.
    //
    // Only `textDocument.uri` is read, and that is enough: every request carrying a document
    // elsewhere (`completionItem/resolve`, a hierarchy item, an inlay hint being resolved) follows
    // one that carries it here, so blocking the entry point blocks the rest. `null` is the empty
    // answer for all of them.
    sendRequest: (type, param, token, next) => {
      if (mine(folder, documentOf(param))) {
        return next(type, param, token);
      }
      // `sendRequest`'s `R` is the response type of whichever request this is, which cannot be
      // named here. `null` is a legal response to all of them (the protocol's "no answer"), so the
      // cast asserts what the protocol guarantees.
      return Promise.resolve(null) as never;
    },
    // The same decision for text sync, so the server never learns the document exists. It saves the
    // outer server indexing and publishing diagnostics for a buffer it should not answer about
    // (`didOpen` indexes whatever it is handed, whatever `index.exclude` says). The four hooks must
    // agree, or the server sees an edit to a file it never opened; they do, because they share the
    // predicate and the folder list.
    didOpen: (document, next) => (mine(folder, document.uri.toString()) ? next(document) : Promise.resolve()),
    didChange: (event, next) =>
      mine(folder, event.document.uri.toString()) ? next(event) : Promise.resolve(),
    didSave: (document, next) => (mine(folder, document.uri.toString()) ? next(document) : Promise.resolve()),
    didClose: (document, next) => (mine(folder, document.uri.toString()) ? next(document) : Promise.resolve()),
  };
}

function mine(folder: string, document: string | undefined): boolean {
  return document === undefined || !claimedByNestedFolder(folder, folderUris(), document);
}

/**
 * Every workspace folder's URI, read at the call, not captured at construction.
 *
 * A nested folder can be added under a running client, and unlike the selector (fixed once the
 * client is built), this decision can follow.
 */
function folderUris(): string[] {
  return (vscode.workspace.workspaceFolders ?? []).map((folder) => folder.uri.toString());
}

/**
 * The document a request names, read from the one place every request that starts a chain names it.
 */
function documentOf(param: unknown): string | undefined {
  const uri = (param as { textDocument?: { uri?: unknown } } | undefined)?.textDocument?.uri;
  return typeof uri === 'string' ? uri : undefined;
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
