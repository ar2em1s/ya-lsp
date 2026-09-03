import assert from 'node:assert/strict';
import { test } from 'node:test';

import { RESTART_REQUIRED, Settings, serverEnvironment, serverOptions } from './config';

/** Settings the user has explicitly set, and nothing else. */
function set(values: Record<string, unknown>): Settings {
  return { explicit: <T,>(key: string) => values[key] as T | undefined };
}

test('a workspace nobody has configured sends no layer at all', () => {
  // Not `{}`: the server layers this *under* `ya-lsp.toml` and over its own defaults, so a
  // layer that says nothing has to be absent rather than empty.
  assert.equal(serverOptions(set({})), undefined);
});

test('only the settings the user actually set are sent', () => {
  // The alternative — sending everything `get` returns — would make package.json a second
  // source of truth for every default in the Rust config, to be kept in sync by hand.
  assert.deepEqual(serverOptions(set({ 'gems.enabled': false })), {
    gems: { enabled: false },
  });
});

test('setting names are translated into the server wire format', () => {
  // The server deserializes with `deny_unknown_fields`, so a camelCase key here does not
  // degrade — it rejects the entire layer and every other setting silently stops working.
  assert.deepEqual(
    serverOptions(
      set({
        'index.include': ['lib/**/*.rb'],
        'index.exclude': ['spec/**/*'],
        'index.loadPaths': ['lib', 'app'],
        'index.maxFiles': 1234,
        'index.respectGitignore': false,
        'gems.enabled': true,
        'gems.defaultGems': false,
        'gems.rubyVersion': '3.3.0',
        'gems.paths': ['/opt/gems'],
        'rbs.enabled': true,
        'rbs.stdlib': false,
        'rbs.path': '/opt/rbs',
        'diagnostics.enabled': false,
        'diagnostics.rules': { 'dynamic-ancestor': 'warning' },
      })
    ),
    {
      index: {
        include: ['lib/**/*.rb'],
        exclude: ['spec/**/*'],
        load_paths: ['lib', 'app'],
        max_files: 1234,
        respect_gitignore: false,
      },
      gems: {
        enabled: true,
        default_gems: false,
        ruby_version: '3.3.0',
        paths: ['/opt/gems'],
      },
      rbs: { enabled: true, stdlib: false, path: '/opt/rbs' },
      diagnostics: { enabled: false, rules: { 'dynamic-ancestor': 'warning' } },
    }
  );
});

test('an empty list is a value, not a setting nobody set', () => {
  // Unlike the empty string on the two path settings: `[]` means no excludes, no extra load
  // paths, no extra gem roots. `index.include = []` is the one that indexes nothing, and the
  // server already reports it out loud — dropping it here would turn a reported mistake into a
  // setting that quietly does nothing.
  assert.deepEqual(serverOptions(set({ 'index.exclude': [] })), { index: { exclude: [] } });
  assert.deepEqual(serverOptions(set({ 'gems.paths': [] })), { gems: { paths: [] } });
});

test('a list the manifest could not have produced is not forwarded', () => {
  // `explicit` casts rather than checks, and settings.json is hand-edited. A number among the
  // globs is a type error the server answers by rejecting the whole layer, not just the key.
  assert.equal(serverOptions(set({ 'index.include': 'lib/**/*.rb' })), undefined);
  assert.equal(serverOptions(set({ 'gems.paths': ['/opt/gems', 7] })), undefined);
});

test('an empty ruby version means detect it, not a Ruby called ""', () => {
  assert.equal(serverOptions(set({ 'gems.rubyVersion': '   ' })), undefined);
  assert.deepEqual(serverOptions(set({ 'gems.rubyVersion': ' 3.3.0 ' })), {
    gems: { ruby_version: '3.3.0' },
  });
});

test('an empty rbs path means find one, not a directory called ""', () => {
  assert.equal(serverOptions(set({ 'rbs.path': '  ' })), undefined);
  assert.deepEqual(serverOptions(set({ 'rbs.path': ' /opt/rbs ' })), {
    rbs: { path: '/opt/rbs' },
  });
});

test('an empty rule map is not a rule map', () => {
  assert.equal(serverOptions(set({ 'diagnostics.rules': {} })), undefined);
});

test('the log level becomes the environment variable the server reads', () => {
  assert.equal(
    serverEnvironment(set({ logLevel: 'debug' }), {}).YA_LSP_LOG,
    'ya_lsp=debug'
  );
});

test('an unset log level leaves an inherited one alone', () => {
  // Somebody debugging from a terminal set `YA_LSP_LOG` on purpose; a default that overrode it
  // would be maddening and invisible.
  assert.equal(
    serverEnvironment(set({}), { YA_LSP_LOG: 'ya_lsp=trace' }).YA_LSP_LOG,
    'ya_lsp=trace'
  );
});

test('turning the log off turns it off, rather than to the loudest value in the list', () => {
  // `off` used to be treated as "the user said nothing", which fell through to the server's own
  // fallback — `info`, louder than the `error` and `warn` sitting above it in the same drop-down.
  // It is a perfectly good `EnvFilter` directive; the fix is to send it.
  assert.equal(serverEnvironment(set({ logLevel: 'off' }), {}).YA_LSP_LOG, 'ya_lsp=off');
  assert.equal(
    serverEnvironment(set({ logLevel: 'off' }), { YA_LSP_LOG: 'ya_lsp=trace' }).YA_LSP_LOG,
    'ya_lsp=off'
  );
});

test('the settings that need a new process are the ones a running server cannot be told', () => {
  // `logLevel` is read by `EnvFilter::try_from_env` at startup and `serverPath` decides which
  // binary was spawned. Neither can be pushed to a process that is already running.
  assert.deepEqual(RESTART_REQUIRED, ['ya-lsp.serverPath', 'ya-lsp.logLevel']);
});
