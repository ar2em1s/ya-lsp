/**
 * Load the *bundled* extension against a stubbed editor and activate it.
 *
 * This is as close to "the extension works" as anything can get without an extension host, and
 * it catches the class of failure the other tests cannot: an import esbuild dropped, a value
 * read at module load that only exists inside VS Code, a command declared in package.json that
 * nothing registers. Those all present identically to a user — the extension simply does
 * nothing — and none of them show up in a type check.
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
 * Exactly, and not permissively: a typo in one of these has to fail the test rather than
 * resolve to something harmless.
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
    // `{ log: true }` is what the extension asks for, so the stub answers with the extra methods
    // a `LogOutputChannel` carries — the client calls them as soon as it starts.
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
  // The client's protocol converter builds both of these when it turns a protocol relative
  // pattern into an editor one, and the document-selector test below reads back what it built.
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
 * It subclasses `vscode.CompletionItem` and friends the moment it is imported, and enumerating
 * which ones would tie this test to the client's internals — a version bump would break it for
 * no reason that concerns anybody. So unknown names answer with something that can be extended,
 * called, and read from.
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

// `out/` next to `dist/`, because this runs against what ships rather than what compiles.
const bundle = path.resolve(__dirname, '..', 'dist', 'extension.js');

test('the bundle loads and exposes the extension host contract', () => {
  const extension = require(bundle) as { activate?: unknown; deactivate?: unknown };
  assert.equal(typeof extension.activate, 'function', 'VS Code calls activate() by name');
  assert.equal(typeof extension.deactivate, 'function');
});

test('activating with no workspace folders registers every declared command', async () => {
  // A window with no folder open is a normal state, not an edge case — and it is also the state
  // in which a crash during activation disables the extension for the whole session.
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
 * `folders[0]` is whichever folder the `.code-workspace` lists first. Activation is
 * `onLanguage:ruby`, so opening a Ruby file in *any* folder used to start a server on that one —
 * and a workspace whose first folder holds no Ruby (infrastructure, docs, a sibling service in
 * another language) got an index of nothing plus a warning telling it to widen `index.include`,
 * for a folder the user had not opened. Counted through `showErrorMessage`, which `start` raises
 * once per folder when it cannot find a server binary.
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
 * The single-folder case still starts eagerly, so the fix above cannot be "never start".
 *
 * Nearly every project is one folder, and for those the eager start is the difference between a
 * warm server and one that begins indexing at the first keystroke. A guard that turned it off
 * everywhere would fix the multi-root warning by making the common case slower, which is why
 * both halves are pinned.
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

test('the language client turns a protocol relative pattern into an editor one', () => {
  // The one link in the multi-root chain that cannot be reasoned about from the types. Client 10
  // runs every selector through `asDocumentSelector`, which recognises only the protocol's
  // `{ baseUri, pattern }` shape and converts anything else to `undefined`. An undefined pattern
  // is not an inert one — `languages.match` then scores on language and scheme alone, so every
  // folder's client would claim every folder's Ruby files. This is where that shows up.
  //
  // `$test/common/*` is the subpath the client's own `exports` map publishes for reaching into
  // `lib/common`; the bare path was sealed off when 10 added that map.
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

  // The guard, so this cannot pass vacuously: an *editor* `RelativePattern` — whose `baseUri` is
  // a Uri object rather than a string — is precisely the shape that vanishes. It is what this
  // extension passed under client 9, where it was copied through untouched.
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
