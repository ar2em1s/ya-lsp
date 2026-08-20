import assert from 'node:assert/strict';
import * as path from 'node:path';
import { test } from 'node:test';

import { Lookup, bundledPath, resolveServer } from './server';

function lookup(overrides: Partial<Lookup> & { present?: string[] } = {}): Lookup {
  const present = new Set((overrides.present ?? []).map((p) => path.normalize(p)));
  return {
    configured: '',
    extensionPath: '/ext',
    workspaceFolder: '/work',
    home: '/home/me',
    platform: 'linux',
    isExecutable: (candidate) => present.has(path.normalize(candidate)),
    ...overrides,
  };
}

test('the bundled binary is used when nothing is configured', () => {
  const resolved = resolveServer(lookup({ present: ['/ext/server/ya-lsp'] }));
  assert.deepEqual(resolved, { kind: 'bundled', command: path.normalize('/ext/server/ya-lsp') });
});

test('windows gets the .exe', () => {
  assert.equal(bundledPath('/ext', 'win32'), path.join('/ext', 'server', 'ya-lsp.exe'));
  assert.equal(bundledPath('/ext', 'darwin'), path.join('/ext', 'server', 'ya-lsp'));
});

test('a configured path wins over the bundled one', () => {
  const resolved = resolveServer(
    lookup({
      configured: '/build/ya-lsp',
      present: ['/build/ya-lsp', '/ext/server/ya-lsp'],
    })
  );
  assert.deepEqual(resolved, { kind: 'configured', command: path.normalize('/build/ya-lsp') });
});

test('a configured path that is not there is an error, not a silent fall back', () => {
  // Falling back would run a different binary than the one asked for, and the difference would
  // show up as behaviour nobody could account for.
  const resolved = resolveServer(
    lookup({ configured: '/build/ya-lsp', present: ['/ext/server/ya-lsp'] })
  );
  assert.equal(resolved.kind, 'missing');
  assert.match((resolved as { message: string }).message, /ya-lsp\.serverPath/);
});

test('a VSIX with no binary for this platform says so', () => {
  const resolved = resolveServer(lookup({ platform: 'freebsd' }));
  assert.equal(resolved.kind, 'missing');
  assert.match((resolved as { message: string }).message, /freebsd/);
  assert.match((resolved as { message: string }).message, /serverPath/);
});

test('a tilde is expanded, because VS Code does not expand it in settings', () => {
  const resolved = resolveServer(
    lookup({ configured: '~/src/ya-lsp/target/release/ya-lsp', present: ['/home/me/src/ya-lsp/target/release/ya-lsp'] })
  );
  assert.equal(resolved.kind, 'configured');
});

test('${workspaceFolder} is expanded, because settings are not launch configurations', () => {
  const resolved = resolveServer(
    lookup({
      configured: '${workspaceFolder}/target/release/ya-lsp',
      present: ['/work/target/release/ya-lsp'],
    })
  );
  assert.deepEqual(resolved, {
    kind: 'configured',
    command: path.normalize('/work/target/release/ya-lsp'),
  });
});

test('a bare tilde is the home directory', () => {
  const resolved = resolveServer(lookup({ configured: '~', present: ['/home/me'] }));
  assert.deepEqual(resolved, { kind: 'configured', command: path.normalize('/home/me') });
});

test('whitespace around a path is not a path', () => {
  // The setting is a text box; a stray space would otherwise become a directory name.
  const resolved = resolveServer(
    lookup({ configured: '  /build/ya-lsp  ', present: ['/build/ya-lsp'] })
  );
  assert.equal(resolved.kind, 'configured');
});

test('an all-whitespace setting is an unset setting', () => {
  const resolved = resolveServer(lookup({ configured: '   ', present: ['/ext/server/ya-lsp'] }));
  assert.equal(resolved.kind, 'bundled');
});
