/**
 * Load the *bundled* extension against a stubbed editor and activate it.
 *
 * The closest thing to "the extension works" without an extension host. It catches what the other
 * tests cannot, all of which look the same to a user (the extension silently does nothing) and none
 * of which a type check sees:
 * - an import esbuild dropped;
 * - a value read at module load that exists only inside VS Code;
 * - a command declared in package.json that nothing registers.
 */

import assert from 'node:assert/strict';
import Module from 'node:module';
import * as path from 'node:path';
import { after, test } from 'node:test';

const registered: string[] = [];
/** One entry per `showErrorMessage`, which `start` raises once per folder it tries to serve. */
const errors: true[] = [];
const disposable = { dispose(): void {} };
const emitter = () => disposable;

/**
 * The namespaces this extension uses, stubbed exactly.
 *
 * Exactly, not permissively: a typo in one of these must fail the test, not resolve to something
 * harmless.
 */
const strict: Record<string, unknown> = {
  commands: {
    registerCommand(name: string): unknown {
      registered.push(name);
      return disposable;
    },
    executeCommand: () => Promise.resolve(),
  },
  workspace: {
    workspaceFolders: undefined,
    textDocuments: [],
    onDidOpenTextDocument: emitter,
    onDidChangeWorkspaceFolders: emitter,
    onDidChangeConfiguration: emitter,
    getConfiguration: () => ({ inspect: () => undefined }),
    getWorkspaceFolder: () => undefined,
  },
  window: {
    // The extension asks for `{ log: true }`, so the stub carries the extra methods of a
    // `LogOutputChannel`; the client calls them as soon as it starts.
    createOutputChannel: () => ({
      ...disposable,
      appendLine() {},
      show() {},
      trace() {},
      debug() {},
      info() {},
      warn() {},
      error() {},
      logLevel: 0,
      onDidChangeLogLevel: emitter,
    }),
    showErrorMessage: (): Promise<undefined> => {
      errors.push(true);
      return Promise.resolve(undefined);
    },
    activeTextEditor: undefined,
  },
  // The client's protocol converter builds both of these when it turns a protocol relative pattern
  // into an editor one, and the document-selector test below reads back what it built.
  Uri: { parse: (value: string) => ({ toString: () => value }) },
  RelativePattern: class {
    constructor(
      readonly baseUri: unknown,
      readonly pattern: string
    ) {}
  },
  version: '1.108.0',
};

/**
 * Everything else `vscode` exports, for the language client rather than for us.
 *
 * The client subclasses `vscode.CompletionItem` and friends as soon as it is imported. Listing
 * which would tie this test to the client's internals, so a version bump would break it for no
 * reason that matters. So unknown names answer with something that can be extended, called and read
 * from.
 */
function permissive(): unknown {
  const shape = function (): undefined {
    return undefined;
  };
  shape.prototype = {};
  return new Proxy(shape, {
    get: (target, key) =>
      key === 'prototype' || typeof key === 'symbol'
        ? (target as unknown as Record<string | symbol, unknown>)[key]
        : permissive(),
    construct: () => ({}),
    apply: () => permissive(),
  });
}

const stub = new Proxy(strict, {
  get: (target, key) =>
    typeof key === 'string' && key in target ? target[key] : permissive(),
  has: () => true,
});

const load = (Module as unknown as { _load(...args: unknown[]): unknown })._load;
(Module as unknown as { _load(...args: unknown[]): unknown })._load = function (
  this: unknown,
  request: unknown,
  ...rest: unknown[]
): unknown {
  if (request === 'vscode') {
    return stub;
  }
  return (load as (...args: unknown[]) => unknown).apply(this, [request, ...rest]);
};
after(() => {
  (Module as unknown as { _load: unknown })._load = load;
});

// `out/` sits next to `dist/`, and this runs against what ships, not what compiles.
const bundle = path.resolve(__dirname, '..', 'dist', 'extension.js');

test('the bundle loads and exposes the extension host contract', () => {
  const extension = require(bundle) as { activate?: unknown; deactivate?: unknown };
  assert.equal(typeof extension.activate, 'function', 'VS Code calls activate() by name');
  assert.equal(typeof extension.deactivate, 'function');
});

test('activating with no workspace folders registers every declared command', async () => {
  // A window with no folder open is a normal state, not an edge case, and it is also the state in
  // which an activation crash disables the extension for the whole session.
  const extension = require(bundle) as {
    activate(context: unknown): Promise<void>;
  };
  registered.length = 0;
  await extension.activate({ subscriptions: [], extensionPath: '/nonexistent' });

  const declared = (
    require(path.resolve(__dirname, '..', 'package.json')) as {
      contributes: { commands: { command: string }[] };
    }
  ).contributes.commands.map((entry) => entry.command);

  assert.deepEqual(registered.sort(), declared.sort());
});

/**
 * A multi-root workspace starts no server until a Ruby file asks for one.
 *
 * `folders[0]` is whichever folder the `.code-workspace` lists first, and activation is
 * `onLanguage:ruby`. Starting a server there because a Ruby file opened in *another* folder would
 * give a first folder with no Ruby (infrastructure, docs, a sibling service) an index of nothing,
 * plus a warning to widen `index.include`, for a folder the user never opened. Counted through
 * `showErrorMessage`, which `start` raises once per folder when it cannot find a server binary.
 */
test('a multi-root workspace starts no server before a Ruby file is opened', async () => {
  const extension = require(bundle) as {
    activate(context: unknown): Promise<void>;
    deactivate(): Promise<void>;
  };
  const folder = (name: string): unknown => ({
    name,
    uri: { toString: () => `file:///multi/${name}`, fsPath: `/multi/${name}` },
  });

  strict.workspace = {
    ...(strict.workspace as Record<string, unknown>),
    workspaceFolders: [folder('infrastructure'), folder('app')],
  };
  errors.length = 0;
  await extension.activate({ subscriptions: [], extensionPath: '/nonexistent' });
  await extension.deactivate();

  assert.equal(
    errors.length,
    0,
    'a folder nobody opened a Ruby file in must not get a server'
  );
});

/**
 * A single-folder workspace still starts eagerly, so the fix above cannot be "never start".
 *
 * Nearly every project is one folder, and for those the eager start is the difference between a
 * warm server and one that starts indexing at the first keystroke. Turning it off everywhere would
 * fix the multi-root warning by slowing the common case, which is why both halves are pinned.
 */
test('a single-folder workspace still starts its server eagerly', async () => {
  const extension = require(bundle) as {
    activate(context: unknown): Promise<void>;
    deactivate(): Promise<void>;
  };

  strict.workspace = {
    ...(strict.workspace as Record<string, unknown>),
    workspaceFolders: [
      {
        name: 'solo',
        uri: { toString: () => 'file:///solo', fsPath: '/solo' },
      },
    ],
  };
  errors.length = 0;
  await extension.activate({ subscriptions: [], extensionPath: '/nonexistent' });
  await extension.deactivate();

  assert.equal(errors.length, 1, 'the only folder must be served without waiting for a file');
});

/**
 * A template opens a server on its folder, exactly as a Ruby file does.
 *
 * ya-lsp indexes `.erb`, and the extension decides whether that reaches a user: `startForDocument`
 * filters on `languageId`, so a template in a folder whose Ruby nobody has opened would activate
 * nothing, and the feature would be invisible in the editor it was built for. Counted through
 * `showErrorMessage`, which `start` raises once per folder it tries to serve.
 */
test('a template opens a server on its folder, the way a Ruby file does', async () => {
  const extension = require(bundle) as {
    activate(context: unknown): Promise<void>;
    deactivate(): Promise<void>;
  };
  const folder = {
    name: 'app',
    uri: { toString: () => 'file:///multi/app', fsPath: '/multi/app' },
  };
  const document = (languageId: string): unknown => ({
    languageId,
    uri: { scheme: 'file', toString: () => `file:///multi/app/index.html.${languageId}` },
  });

  strict.workspace = {
    ...(strict.workspace as Record<string, unknown>),
    workspaceFolders: [
      { name: 'infrastructure', uri: { toString: () => 'file:///multi/infra', fsPath: '/i' } },
      folder,
    ],
    textDocuments: [document('erb')],
    getWorkspaceFolder: () => folder,
  };
  errors.length = 0;
  await extension.activate({ subscriptions: [], extensionPath: '/nonexistent' });
  await extension.deactivate();

  assert.equal(errors.length, 1, 'an open template must start its folder`s server');

  // The guard, so this cannot pass because everything starts a server: a language this extension
  // does not serve still starts nothing.
  strict.workspace = {
    ...(strict.workspace as Record<string, unknown>),
    textDocuments: [document('markdown')],
  };
  errors.length = 0;
  await extension.activate({ subscriptions: [], extensionPath: '/nonexistent' });
  await extension.deactivate();

  assert.equal(errors.length, 0, 'a Markdown file is not this extension`s business');
});

/**
 * Every folder's client is narrowed to that folder, and the shape survives the real conversion.
 *
 * There is one selector shape: the folder. What it must get right is containment: its own files
 * claimed, a sibling's refused, and nothing outside either. What lies outside is the server's to
 * ask for, and two clients claiming one gem file is two servers answering one hover.
 *
 * Run through the *real* converter, not asserted as a string, because the conversion is the step
 * that can silently drop the pattern, and a dropped pattern does not narrow: it widens to language
 * and scheme alone.
 */
test('a folder`s client claims its own folder and refuses a sibling`s', () => {
  const { LANGUAGES, documentSelector } = require(bundle) as {
    LANGUAGES: string[];
    documentSelector(folderUri: string): { scheme: string; language: string }[];
  };
  const { createConverter } = require('vscode-languageclient/$test/common/protocolConverter') as {
    createConverter(...args: unknown[]): {
      asDocumentSelector(selector: unknown[]): {
        language?: string;
        scheme?: string;
        pattern?: { baseUri?: { toString(): string }; pattern?: string };
      }[];
    };
  };

  const folder = 'file:///work/app';
  const selector = documentSelector(folder);
  assert.deepEqual(
    selector.map((filter) => filter.language).sort(),
    [...LANGUAGES].sort(),
    'both languages this extension serves, or one of them is dead in every file'
  );

  const converted = createConverter(undefined, true, true).asDocumentSelector(selector);
  assert.equal(converted.length, LANGUAGES.length, 'every filter must survive the conversion');

  /**
   * A `vscode.RelativePattern` matches a path only under its base, modelled here because the stub
   * above is not minimatch. The guard below keeps this from passing vacuously.
   */
  const underBase = (
    filter: { pattern?: { baseUri?: { toString(): string } } },
    fsPath: string
  ): boolean => {
    const base = filter.pattern?.baseUri?.toString();
    assert.ok(base, 'a dropped pattern claims every Ruby file the editor has open anywhere');
    const prefix = base.replace(/^file:\/\//, '');
    return fsPath === prefix || fsPath.startsWith(`${prefix}/`);
  };

  for (const filter of converted) {
    assert.equal(filter.scheme, 'file');
    assert.equal(filter.pattern?.pattern, '**/*');
  }
  const ruby = converted.find((filter) => filter.language === 'ruby');
  assert.ok(ruby, 'Ruby must be claimed at all');
  assert.ok(underBase(ruby, '/work/app/app/models/story.rb'), 'its own folder is claimed');
  assert.equal(
    underBase(ruby, '/work/api/app/models/story.rb'),
    false,
    'a sibling folder`s file is what the pattern exists to refuse'
  );
  assert.equal(
    underBase(ruby, '/gems/activerecord-8.1.3.1/lib/active_record.rb'),
    false,
    'a gem is claimed by the registration the server sends, never by a guess made here'
  );
});

test('the language client turns a protocol relative pattern into an editor one', () => {
  // The one link in the multi-root chain the types cannot vouch for. Client 10 runs every selector
  // through `asDocumentSelector`, which recognises only the protocol's `{ baseUri, pattern }` shape
  // and turns anything else into `undefined`. An undefined pattern is not inert: `languages.match`
  // then scores on language and scheme alone, so every folder's client would claim every folder's
  // Ruby files. This is where that would show.
  //
  // `$test/common/*` is the subpath the client's own `exports` map publishes for `lib/common`; that
  // map seals off the bare path.
  const { createConverter } = require('vscode-languageclient/$test/common/protocolConverter') as {
    createConverter(...args: unknown[]): {
      asDocumentSelector(selector: unknown[]): { pattern?: unknown }[];
    };
  };
  const converter = createConverter(undefined, true, true);

  const converted = converter.asDocumentSelector([
    { scheme: 'file', language: 'ruby', pattern: { baseUri: 'file:///w/proj', pattern: '**/*' } },
  ]);
  assert.equal(converted.length, 1, 'the filter must survive at all');
  const pattern = converted[0].pattern as { baseUri?: { toString(): string }; pattern?: string };
  assert.ok(pattern, 'a dropped pattern silently widens this client to every folder');
  assert.equal(pattern.pattern, '**/*');
  assert.equal(
    pattern.baseUri?.toString(),
    'file:///w/proj',
    'scoped to this folder, not to the whole workspace'
  );

  // The guard, so this cannot pass vacuously: an *editor* `RelativePattern`, whose `baseUri` is a
  // Uri object, not a string, is exactly the shape that vanishes.
  const dropped = converter.asDocumentSelector([
    {
      scheme: 'file',
      language: 'ruby',
      pattern: { base: '/w/proj', baseUri: { toString: () => 'file:///w/proj' }, pattern: '**/*' },
    },
  ]);
  assert.equal(
    dropped[0].pattern,
    undefined,
    'an editor RelativePattern is dropped, which is why the protocol shape is used'
  );
});

/**
 * The wiring between `claims.ts`' predicate and the requests this client actually sends.
 *
 * `claimedByNestedFolder` is pure and pinned in `claims.test.ts`. What is left can only go wrong in
 * `extension.ts`, silently, in opposite directions:
 * - reading the document from the wrong place in the parameters leaves every answer in a nested
 *   folder doubled (the bug this exists to fix);
 * - handing the predicate its two strings the wrong way round makes a client answer nothing
 *   anywhere.
 *
 * Driven through the bundle, not the sources, like every other test here.
 */
test('a parent folder`s client sends nothing about a nested folder`s files', async () => {
  const { narrowing } = require(bundle) as {
    narrowing(folder: string): {
      sendRequest(
        type: string,
        param: unknown,
        token: undefined,
        next: (type: string, param: unknown, token: undefined) => Promise<unknown>
      ): Promise<unknown>;
      didOpen(document: unknown, next: (document: unknown) => Promise<void>): Promise<void>;
    };
  };

  const folder = (uri: string): unknown => ({ name: uri, uri: { toString: () => uri } });
  strict.workspace = {
    ...(strict.workspace as Record<string, unknown>),
    workspaceFolders: [folder('file:///repo'), folder('file:///repo/backend')],
  };

  const middleware = narrowing('file:///repo');
  const forwarded: string[] = [];
  const next = (_type: string, param: unknown): Promise<unknown> => {
    forwarded.push(String((param as { textDocument: { uri: string } }).textDocument.uri));
    return Promise.resolve('a card');
  };
  const hover = (uri: string): unknown => ({
    textDocument: { uri },
    position: { line: 0, character: 0 },
  });

  assert.equal(
    await middleware.sendRequest('textDocument/hover', hover('file:///repo/backend/app.rb'), undefined, next),
    null,
    'the backend client is the one that answers here, and it is the only one that should'
  );
  // The guard, in the direction that would otherwise pass by refusing everything.
  assert.equal(
    await middleware.sendRequest('textDocument/hover', hover('file:///repo/shared/user.rb'), undefined, next),
    'a card',
    'the parent folder still answers about its own files, or nothing answers about them at all'
  );
  assert.deepEqual(forwarded, ['file:///repo/shared/user.rb'], 'and only that one was sent');

  // A request naming no document is about the folder, not a file, and must still go.
  const symbols: string[] = [];
  assert.deepEqual(
    await middleware.sendRequest('workspace/symbol', { query: 'User' }, undefined, (type) => {
      symbols.push(type);
      return Promise.resolve(['a symbol']);
    }),
    ['a symbol'],
    'workspace/symbol carries no textDocument and must not be mistaken for a blocked one'
  );
  assert.deepEqual(symbols, ['workspace/symbol']);

  // And the text sync, so the outer server is never handed the buffer at all.
  const opened: string[] = [];
  const document = (uri: string): unknown => ({ uri: { toString: () => uri } });
  const open = (document: unknown): Promise<void> => {
    opened.push(String((document as { uri: { toString(): string } }).uri.toString()));
    return Promise.resolve();
  };
  await middleware.didOpen(document('file:///repo/backend/app.rb'), open);
  await middleware.didOpen(document('file:///repo/shared/user.rb'), open);
  assert.deepEqual(opened, ['file:///repo/shared/user.rb']);
});
