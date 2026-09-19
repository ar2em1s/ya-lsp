import assert from 'node:assert/strict';
import { test } from 'node:test';

import { RESTART_REQUIRED, Settings, serverEnvironment, serverOptions } from './config';

/** Settings the user has explicitly set, and nothing else. */
function set(values: Record<string, unknown>): Settings {
  return { explicit: <T,>(key: string) => values[key] as T | undefined };
}

test('a workspace nobody has configured sends no layer at all', () => {
  // Not `{}`: the server layers this *under* `ya-lsp.toml` and over its own defaults, so a layer
  // with nothing to say must be absent, not empty.
  assert.equal(serverOptions(set({})), undefined);
});

test('only the settings the user actually set are sent', () => {
  // Sending everything `get` returns would make package.json a second source of truth for every
  // default in the Rust config, synced by hand.
  assert.deepEqual(serverOptions(set({ 'gems.enabled': false })), {
    gems: { enabled: false },
  });
});

test('setting names are translated into the server wire format', () => {
  // The server deserializes with `deny_unknown_fields`, so a camelCase key does not degrade: it
  // rejects the whole layer, and every other setting silently stops working.
  assert.deepEqual(
    serverOptions(
      set({
        'index.include': ['lib/**/*.rb'],
        'index.exclude': ['spec/**/*'],
        'index.loadPaths': ['lib', 'app'],
        'index.maxFiles': 1234,
        'gems.enabled': true,
        'gems.defaultGems': false,
        'gems.rubyVersion': '3.3.0',
        'gems.paths': ['/opt/gems'],
        'rbs.enabled': true,
        'rbs.stdlib': false,
        'rbs.path': '/opt/rbs',
        'types.guessFromNames': false,
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
      },
      gems: {
        enabled: true,
        default_gems: false,
        ruby_version: '3.3.0',
        paths: ['/opt/gems'],
      },
      rbs: { enabled: true, stdlib: false, path: '/opt/rbs' },
      types: { guess_from_names: false },
      diagnostics: { enabled: false, rules: { 'dynamic-ancestor': 'warning' } },
    }
  );
});

test('an empty list is a value, not a setting nobody set', () => {
  // Unlike the empty string on the two path settings, `[]` is a value: no excludes, no extra load
  // paths, no extra gem roots. `index.include = []` indexes nothing, and the server reports that
  // out loud; dropping it here would turn a reported mistake into a setting that quietly does
  // nothing.
  assert.deepEqual(serverOptions(set({ 'index.exclude': [] })), { index: { exclude: [] } });
  assert.deepEqual(serverOptions(set({ 'gems.paths': [] })), { gems: { paths: [] } });
});

test('a list the manifest could not have produced is not forwarded', () => {
  // `explicit` casts, it does not check, and settings.json is hand-edited. A number among the globs
  // is a type error the server answers by rejecting the whole layer, not just the key.
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

test('the rails switch is sent as the word it is, and a family at a time', () => {
  // `auto` has no boolean spelling, so the value travels as written. The server also takes `true`
  // and `false`, which is what someone hand-editing `ya-lsp.toml` types.
  assert.deepEqual(serverOptions(set({ 'rails.enabled': 'off' })), {
    rails: { enabled: 'off' },
  });
  assert.deepEqual(
    serverOptions(set({ 'rails.schema': false, 'rails.views': false })),
    { rails: { schema: false, views: false } }
  );
  assert.equal(serverOptions(set({ 'rails.enabled': '  ' })), undefined);
});

test('the two type families that are not rails are in the types table', () => {
  // `structs` and `annotations` have nothing to do with Rails, so they must not sit under a `rails`
  // table.
  assert.deepEqual(serverOptions(set({ 'types.structs': false })), {
    types: { structs: false },
  });
  assert.deepEqual(
    serverOptions(set({ 'types.annotations': false, 'types.guessFromNames': false })),
    { types: { guess_from_names: false, annotations: false } }
  );
});

test('an empty tree list is a value, not an unset setting', () => {
  // The opposite of `gems.rubyVersion` and `rbs.path`, where `""` means "work it out yourself".
  // `trees.test = []` turns the suite fence off and `trees.migration = []` the migration fence, and
  // a project may mean either.
  assert.deepEqual(serverOptions(set({ 'trees.test': [] })), { trees: { test: [] } });
  assert.deepEqual(serverOptions(set({ 'trees.migration': [] })), {
    trees: { migration: [] },
  });
  assert.deepEqual(
    serverOptions(set({ 'trees.test': ['qa'], 'trees.testSupport': ['fixtures'] })),
    { trees: { test: ['qa'], test_support: ['fixtures'] } }
  );
  // And a list whose entries are not strings is not one this file will vouch for.
  assert.equal(serverOptions(set({ 'trees.test': [1, 2] })), undefined);
});

test('an empty rule map is not a rule map', () => {
  assert.equal(serverOptions(set({ 'diagnostics.rules': {} })), undefined);
});

test('the log level travels in the settings layer rather than in the environment', () => {
  // The server re-points its own log when the layer arrives, so the level travels in the layer and
  // needs no restart. The key keeps its shipped spelling, `ya-lsp.logLevel`: a rename costs a
  // deprecation and a window where two keys can disagree.
  assert.deepEqual(serverOptions(set({ logLevel: 'debug' })), { log: { level: 'debug' } });
  // `off` too. Treating it as "the user said nothing" would fall through to the server's fallback,
  // `info`, which is louder than the `error` and `warn` above it in the same drop-down.
  assert.deepEqual(serverOptions(set({ logLevel: 'off' })), { log: { level: 'off' } });
});

test('an inherited YA_LSP_LOG is passed on untouched', () => {
  // Somebody debugging from a terminal set it on purpose, and the server lets it outrank both the
  // setting and `ya-lsp.toml`. Sending the setting here too would take that away from the one
  // person the variable exists for.
  assert.equal(
    serverEnvironment({ YA_LSP_LOG: 'ya_lsp=trace' }).YA_LSP_LOG,
    'ya_lsp=trace'
  );
  assert.equal(serverEnvironment({}).YA_LSP_LOG, undefined);
});

test('the file sink is three settings and each one is sent only when it is set', () => {
  assert.deepEqual(serverOptions(set({ 'log.file': true })), { log: { file: true } });
  assert.deepEqual(serverOptions(set({ 'log.file': false })), { log: { file: false } });
  assert.deepEqual(
    serverOptions(
      set({ 'log.file': true, 'log.filePath': '/var/log/ya.log', 'log.fileLevel': 'trace' })
    ),
    { log: { file: true, file_path: '/var/log/ya.log', file_level: 'trace' } }
  );
  // An empty path is not "work it out yourself" (there is nothing to work out), so it is dropped,
  // not sent as a directory called "".
  assert.equal(serverOptions(set({ 'log.filePath': '   ' })), undefined);
});

test('the settings that need a new process are the ones a running server cannot be told', () => {
  // `serverPath` decides which binary was spawned, which a running process cannot be told. Every
  // other setting, `logLevel` included, reaches the running server.
  assert.deepEqual(RESTART_REQUIRED, ['ya-lsp.serverPath']);
});
