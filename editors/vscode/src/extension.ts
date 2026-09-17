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
 * `erb` is contributed by the manifest, with the same id and the same extensions ruby-lsp uses, so
 * a workspace that has an ERB grammar installed keeps it: a grammar binds to the id, and two
 * contributions of one id merge. ya-lsp ships no grammar of its own — it is a language client, not
 * a syntax — and semantic tokens colour the Ruby either way.
 */
export const LANGUAGES = ['ruby', 'erb'];

/**
 * Which documents a folder's client claims: that folder's, and nothing else.
 *
 * The selector is the only gate on what the client sends. `LanguageClientOptions.workspaceFolder`
 * sets the `rootUri` and does not filter documents, so whatever this returns is exactly the set of
 * files the client will `didOpen` and answer requests for; everything else scores 0 in
 * `languages.match` and the server is never told the document exists.
 *
 * **Nothing here names a file outside the folder, and that is deliberate.** A gem's source, Ruby's
 * stdlib and the RBS beside them live outside every workspace folder, and the server indexes and
 * answers about all three — but which directories those are is `workspace/gems.rs`: the bundle
 * parse, `require_paths`, the vendored-versus-installed choice, an engine's `app/`. A second copy
 * of that in TypeScript would drift in the one direction nothing reports, since a file the client
 * does not claim produces silence rather than an error. So the server names its own roots after the
 * handshake, over `client/registerCapability`, and `claims.ts` decides which client takes each one.
 *
 * The pattern is the protocol's own shape — a `baseUri` **string** — rather than a
 * `vscode.RelativePattern`: the client runs every selector through `asDocumentSelector`, which
 * recognises only this form and silently converts anything else to `undefined`, and an undefined
 * pattern does not narrow, it *widens* to language and scheme alone. Given this shape the client
 * builds the `vscode.RelativePattern` itself, which is what makes the path separator right on
 * Windows by construction.
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
/** Folders whose server could not be found, so the error is reported once and not per file. */
const reported = new Set<string>();
/** Folders already asked about RuboCop, so the hint is once per session and not per Ruby file. */
const suggested = new Set<string>();
/**
 * Which folder's server answers about each root outside every folder.
 *
 * One ledger for the window, because the question only exists between clients: two folders on one
 * Ruby register the same gem roots, and two providers over one file is the same server answering
 * the same hover twice.
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
  if (!LANGUAGES.includes(document.languageId) || document.uri.scheme !== 'file') {
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
    options: { env: serverEnvironment(process.env) },
  };

  const options: LanguageClientOptions = {
    documentSelector: documentSelector(key),
    workspaceFolder: folder,
    outputChannel: channelFor(folder),
    initializationOptions: serverOptions(settings),
    middleware: {
      ...narrowing(key),
      // The one place every client is visible at once, which is what this decision needs. The
      // server registers the roots it has answers about; `claims` drops the ones another folder's
      // server got to first, and forwards everything it does not recognise — the file watcher's
      // registration carries no selector and has to arrive exactly as sent.
      handleRegisterCapability: (params, next): Promise<void> => {
        const narrowed = claims.narrow(key, params.registrations as Registration[], folderUris());
        // `next` is typed as the protocol's `RequestHandler`, which takes a cancellation token as
        // its second argument — but the client builds it as `nextParams =>
        // this.doRegisterCapability(nextParams)` and there is no token anywhere to pass. Narrowed
        // to the shape it actually has rather than handed an invented one.
        const forward = next as unknown as (
          forwarded: RegistrationParams
        ) => void | Promise<void>;
        return Promise.resolve(forward({ registrations: narrowed }));
      },
    },
    // No `synchronize.fileEvents`. The server registers its own watchers through
    // `client/registerCapability` — `ya-lsp.toml` and everything `index.include` covers —
    // which is what makes reload and on-disk freshness work in editors
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
    claims.release(key);
    void vscode.window.showErrorMessage(`ya-lsp failed to start: ${describe(error)}`);
    return;
  }
  // After the server is up, not before: a folder whose server could not start has a worse
  // problem than its choice of linter, and two notifications about one folder is one too many.
  void suggestRubocop(folder, settings);
}

/**
 * Offer RuboCop's own extension to a project that lints with RuboCop.
 *
 * ya-lsp reports parse errors and Prism's warnings and stops there, because every cop is Ruby.
 * The protocol's own answer to that is a second server, not a proxy inside this one — so the
 * extension says so once, where the user is, rather than only in a README they have no reason
 * to open. `rubocop.hint` turns it off, and choosing "Don't show again" is what writes it.
 */
async function suggestRubocop(
  folder: vscode.WorkspaceFolder,
  settings: Settings
): Promise<void> {
  const key = folder.uri.toString();
  if (suggested.has(key) || settings.explicit<boolean>('rubocop.hint') === false) {
    return;
  }
  // Marked before the checks, not after: this is "we have considered this folder", and a
  // second `start` for the same folder must not re-ask while the first is still awaiting.
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
    // Global, not folder: the answer is about this user's taste, and being asked again in the
    // next project is the thing they just declined.
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
        // Absent, a directory, or unreadable — all of which mean the same thing here.
        return undefined;
      }
    },
  };
}

/**
 * Stop one folder's client and give up the roots it had claimed.
 *
 * The roots are released here rather than by the caller because every route out of a running client
 * comes through this function, and a root still marked as owned by a process that has gone is a gem
 * file nobody answers about. Whether anyone should be rebuilt to pick it up is the caller's
 * question: a client on its way to being restarted claims its own roots back a moment later, and
 * `onFoldersChanged` is the one place where the loss is permanent.
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
    // A server that has already died cannot be stopped politely, and there is nothing the user
    // would do with the news.
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
    // `stop` takes the client with it, and hands back whoever else wanted what it was holding;
    // the channel is this function's to close.
    for (const waiting of await stop(key)) {
      orphaned.add(waiting);
    }
    channels.get(key)?.dispose();
    channels.delete(key);
    reported.delete(key);
    suggested.delete(key);
  }

  // Nothing here varies with the number of folders — the selector is the same shape for one folder
  // and for twelve — so the only thing a removal can invalidate is a *claim*. The folder that went
  // may have been the one answering about the gems, and a client that lost a root the first time
  // cannot pick it up later: a selector is fixed at construction. So the clients that asked for a
  // root nobody owns any more are rebuilt, and only those.
  for (const key of orphaned) {
    const folder = vscode.workspace.workspaceFolders?.find((f) => f.uri.toString() === key);
    if (folder) {
      await stop(key);
      await start(folder);
    }
  }
  // Added folders otherwise start lazily, the same as the ones that were there at startup.
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


/**
 * Whether this client is the one to send about `document`.
 *
 * A request naming no document — `initialize`, `workspace/symbol`, `shutdown` — is always this
 * client's to send: it is about the folder rather than about a file, and the folder is its own.
 */
/**
 * The middleware that keeps one folder's client out of a nested folder's files.
 *
 * Exported because this is the half of the nesting fix that cannot be reasoned about from
 * `claims.ts`: the predicate there is pure and tested directly, and everything that can still go
 * wrong is *wiring* — reading the document from the wrong place in the parameters, or passing the
 * folder and the document in the wrong order. Both fail silently and in opposite directions. One
 * leaves every answer doubled, exactly as before; the other makes a client answer nothing at all,
 * with no error anywhere. `activation.test.ts` drives this against the stubbed editor.
 */
export function narrowing(folder: string): Middleware {
  return {
    // Every request this client would send about a document a *nested* workspace folder holds,
    // dropped before it goes. `getWorkspaceFolder` resolves such a file to the innermost folder
    // and `documentSelector` cannot: an LSP glob has no way to subtract a path, so this folder's
    // client claims a nested folder's files too and the user reads every answer twice. The
    // folder list is read per call rather than captured, because `onDidChangeWorkspaceFolders`
    // can add a nested folder under a client that is already running.
    //
    // `textDocument.uri` is the only shape read, and it is the only one that has to be: every
    // request carrying a document somewhere else — `completionItem/resolve`, a hierarchy item, an
    // inlay hint being resolved — follows one that carries it here, so blocking the entry point
    // blocks the rest. `null` is the empty answer for all of them.
    sendRequest: (type, param, token, next) => {
      if (mine(folder, documentOf(param))) {
        return next(type, param, token);
      }
      // `sendRequest`'s `R` is the response type of whichever request this is, and there is no
      // way to name it from here. `null` is a legal response to every one of them — the
      // protocol's own "no answer" — so the cast asserts what the protocol already guarantees.
      return Promise.resolve(null) as never;
    },
    // The same decision for the text sync, so the server is never told the document exists. It
    // saves the outer server indexing and publishing diagnostics about a buffer that is not its
    // to answer about — `didOpen` indexes whatever it is handed, whatever `index.exclude` said —
    // and the four must agree with each other or the server sees an edit to a file it never
    // opened. They do, because the predicate is the same one and it reads the same list.
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
 * Every workspace folder's URI, read at the call rather than captured at construction.
 *
 * A nested folder can be added under a client that is already running, and unlike the selector —
 * which is fixed once the client is built — this decision can follow.
 */
function folderUris(): string[] {
  return (vscode.workspace.workspaceFolders ?? []).map((folder) => folder.uri.toString());
}

/** The document a request names, in the one place every request that starts a chain names it. */
function documentOf(param: unknown): string | undefined {
  const uri = (param as { textDocument?: { uri?: unknown } } | undefined)?.textDocument?.uri;
  return typeof uri === 'string' ? uri : undefined;
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
