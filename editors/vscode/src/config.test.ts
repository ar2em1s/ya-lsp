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
        respect_gitignore: false,
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

test('the rails switch is sent as the word it is, and a family at a time', () => {
  // `auto` has no boolean spelling, so the value travels as written; the server takes `true`
  // and `false` as well, which is what somebody editing a `ya-lsp.toml` by hand will type.
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
  // Putting `structs` or `annotations` under a `rails` table would be the first Rails word to
  // leak somewhere it does not belong: neither has anything to do with Rails.
  assert.deepEqual(serverOptions(set({ 'types.structs': false })), {
    types: { structs: false },
  });
  assert.deepEqual(
    serverOptions(set({ 'types.annotations': false, 'types.guessFromNames': false })),
    { types: { guess_from_names: false, annotations: false } }
  );
});

test('an empty tree list is a value, not an unset setting', () => {
  // The opposite of `gems.rubyVersion` and `rbs.path`, where `""` spells "work it out
  // yourself". `trees.test = []` turns the suite fence off and `trees.migration = []` turns the
  // migration fence off, and both are things a project may legitimately mean.
  assert.deepEqual(serverOptions(set({ 'trees.test': [] })), { trees: { test: [] } });
  assert.deepEqual(serverOptions(set({ 'trees.migration': [] })), {
    trees: { migration: [] },
  });
  assert.deepEqual(
    serverOptions(set({ 'trees.test': ['qa'], 'trees.testSupport': ['fixtures'] })),
    { trees: { test: ['qa'], test_support: ['fixtures'] } }
  );
  // And a list whose entries are not strings is not a list this file will vouch for.
  assert.equal(serverOptions(set({ 'trees.test': [1, 2] })), undefined);
});

test('an empty rule map is not a rule map', () => {
  assert.equal(serverOptions(set({ 'diagnostics.rules': {} })), undefined);
});

test('the log level travels in the settings layer rather than in the environment', () => {
  // It was an environment variable for as long as the server read it once at startup. Now the
  // server re-points its own log when the layer arrives, which is what took the setting off
  // `RESTART_REQUIRED` — and the key keeps its shipped spelling, `ya-lsp.logLevel`, because a
  // rename costs a deprecation and a window where two keys can disagree.
  assert.deepEqual(serverOptions(set({ logLevel: 'debug' })), { log: { level: 'debug' } });
  // `off` included, and that is the fix rather than an oversight: treating it as "the user said
  // nothing" fell through to the server's own fallback, `info`, which is louder than the `error`
  // or `warn` sitting above it in the same drop-down.
  assert.deepEqual(serverOptions(set({ logLevel: 'off' })), { log: { level: 'off' } });
});

test('an inherited YA_LSP_LOG is passed on untouched', () => {
  // Somebody debugging from a terminal set it on purpose, and the server treats it as
  // outranking both the setting and `ya-lsp.toml`. Sending the setting here as well would take
  // that away from the one person the variable exists for.
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
  // An empty path is not "work it out yourself" — there is nothing to work out — so it is
  // dropped rather than sent as a directory called "".
  assert.equal(serverOptions(set({ 'log.filePath': '   ' })), undefined);
});

test('the settings that need a new process are the ones a running server cannot be told', () => {
  // `serverPath` decides which binary was spawned, which is not something a running process can
  // be told. `logLevel` used to be the other one and no longer is: the server re-points its own
  // log when the settings layer arrives.
  assert.deepEqual(RESTART_REQUIRED, ['ya-lsp.serverPath']);
});
