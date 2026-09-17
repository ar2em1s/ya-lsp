/**
 * Which server answers about a gem, when several are running.
 *
 * The failure this pins is the one the narrow per-folder selector used to prevent by accident: two
 * folders on one Ruby resolve to the same gem roots, both servers register them, and the user reads
 * the same hover card twice with no way to tell which server wrote either half. The opposite
 * mistake is just as silent — a root dropped from every client is a gem file that answers nothing,
 * which is indistinguishable from the server not knowing the answer.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { Claims, DOCUMENTS_ID_PREFIX, Registration, claimedByNestedFolder } from './claims';

const APP = 'file:///work/app';
const API = 'file:///work/api';
/** Where a real bundle puts a gem: outside every workspace folder, which is the whole point. */
const ACTIVERECORD = 'file:///gems/activerecord-8.1.3.1';
const RUBY = 'file:///ruby/4.0.0';

/** One document registration, as the server sends it: one filter per root per language. */
function documents(method: string, bases: string[]): Registration {
  return {
    id: `${DOCUMENTS_ID_PREFIX}${method}`,
    method,
    registerOptions: {
      documentSelector: bases.flatMap((baseUri) =>
        ['ruby', 'erb'].map((language) => ({
          scheme: 'file',
          language,
          pattern: { baseUri, pattern: '**/*' },
        }))
      ),
    },
  };
}

/** Every root a narrowed batch still claims, deduplicated across the two languages. */
function claimed(registrations: Registration[]): string[] {
  const bases = new Set<string>();
  for (const registration of registrations) {
    for (const filter of registration.registerOptions?.documentSelector ?? []) {
      if (filter.pattern) {
        bases.add(filter.pattern.baseUri);
      }
    }
  }
  return [...bases].sort();
}

test('the first server to ask gets every root it asked for', () => {
  const claims = new Claims();

  const narrowed = claims.narrow(APP, [
    documents('textDocument/hover', [ACTIVERECORD, RUBY]),
    documents('textDocument/didOpen', [ACTIVERECORD, RUBY]),
  ]);

  assert.equal(narrowed.length, 2, 'both registrations survive');
  assert.deepEqual(claimed(narrowed), [ACTIVERECORD, RUBY].sort());
});

test('a second server asking for the same roots is dropped, not forwarded empty', () => {
  const claims = new Claims();
  claims.narrow(APP, [documents('textDocument/hover', [ACTIVERECORD, RUBY])]);

  const second = claims.narrow(API, [documents('textDocument/hover', [ACTIVERECORD, RUBY])]);

  assert.deepEqual(second, [], 'an empty selector is kept by the client and matches nothing');
});

test('a root only one bundle resolved to stays with that bundle', () => {
  const claims = new Claims();
  const mine = 'file:///gems/solidus_core-4.6.0';
  claims.narrow(APP, [documents('textDocument/hover', [ACTIVERECORD, RUBY])]);

  const second = claims.narrow(API, [
    documents('textDocument/hover', [ACTIVERECORD, RUBY, mine]),
  ]);

  assert.deepEqual(claimed(second), [mine], 'two bundles differing is not a conflict');
});

test('asking twice is not a conflict with itself', () => {
  const claims = new Claims();
  claims.narrow(APP, [documents('textDocument/hover', [ACTIVERECORD])]);

  const again = claims.narrow(APP, [documents('textDocument/hover', [ACTIVERECORD])]);

  assert.deepEqual(claimed(again), [ACTIVERECORD], 'a reload re-registers the same roots');
});

/**
 * The file watcher's registration arrives on the same channel and must be forwarded exactly as
 * sent: it carries no document selector, and the client falls back to its own for one that does not.
 */
test('a registration this module does not recognise passes through untouched', () => {
  const claims = new Claims();
  const watcher: Registration = {
    id: 'ya-lsp-watched-files',
    method: 'workspace/didChangeWatchedFiles',
    registerOptions: { watchers: [{ globPattern: '**/*.rb' }] },
  };

  const narrowed = claims.narrow(APP, [watcher, documents('textDocument/hover', [ACTIVERECORD])]);

  assert.equal(narrowed.length, 2);
  assert.deepEqual(narrowed[0], watcher, 'the same object, options and all');
  // The guard, so the test above cannot pass because everything passes through: a document
  // registration a second client asks for really is dropped.
  claims.narrow(API, [watcher]);
  assert.deepEqual(claims.narrow(API, [documents('textDocument/hover', [ACTIVERECORD])]), []);
});

test('releasing an owner names the servers that were waiting on what it held', () => {
  const claims = new Claims();
  claims.narrow(APP, [documents('textDocument/hover', [ACTIVERECORD, RUBY])]);
  claims.narrow(API, [documents('textDocument/hover', [ACTIVERECORD, RUBY])]);

  assert.deepEqual(claims.release(APP), [API], 'a rebuilt API client can pick the roots up');
  assert.deepEqual(
    claims.narrow(API, [documents('textDocument/hover', [ACTIVERECORD, RUBY])]).length,
    1,
    'and does, because nobody owns them any more'
  );
});

test('releasing a server nobody was waiting on rebuilds nothing', () => {
  const claims = new Claims();
  claims.narrow(APP, [documents('textDocument/hover', [ACTIVERECORD])]);
  claims.narrow(API, [documents('textDocument/hover', ['file:///gems/other-1.0.0'])]);

  assert.deepEqual(claims.release(APP), [], 'API never asked for what APP was holding');
});

/**
 * A folder inside another folder, which is the one containment case the selector cannot express.
 *
 * `/repo` holds the code beside the apps; `/repo/backend` and `/repo/frontend` are applications
 * with their own `Gemfile.lock`, so all three want a server and the nesting cannot be flattened
 * away. Without this, `/repo`'s client claims every file in both apps as well as its own and the
 * user reads every hover, every completion item and every squiggle twice.
 */
const REPO = 'file:///repo';
const BACKEND = 'file:///repo/backend';
const FRONTEND = 'file:///repo/frontend';
const NESTED = [REPO, BACKEND, FRONTEND];

test('an outer folder gives up the files a nested folder holds', () => {
  assert.equal(
    claimedByNestedFolder(REPO, NESTED, 'file:///repo/backend/app/models/story.rb'),
    true,
    'the backend client answers about its own files, and it is the only one that should'
  );
  assert.equal(
    claimedByNestedFolder(REPO, NESTED, 'file:///repo/frontend/app/helpers/tag.rb'),
    true
  );
});

test('an outer folder keeps everything no nested folder holds', () => {
  // The guard that stops the test above from passing by refusing everything: `/repo` still has
  // files of its own, at any depth, and giving those up would leave them answered by nobody.
  assert.equal(claimedByNestedFolder(REPO, NESTED, 'file:///repo/shared/models/user.rb'), false);
  assert.equal(claimedByNestedFolder(REPO, NESTED, 'file:///repo/Rakefile'), false);
  assert.equal(
    claimedByNestedFolder(REPO, NESTED, 'file:///repo/lib/tasks/deep/down/here.rb'),
    false,
    'depth is not what decides it — which folder holds the file is'
  );
});

test('a nested folder gives up nothing to the folder above it', () => {
  // The direction matters, and getting it backwards silently swaps which server answers: the
  // inner folder is the one `getWorkspaceFolder` resolves to, so it keeps its own files.
  assert.equal(claimedByNestedFolder(BACKEND, NESTED, 'file:///repo/backend/app/models/story.rb'), false);
  // And it never had a claim on its parent's files or its sibling's — the selector refused those.
  assert.equal(claimedByNestedFolder(BACKEND, NESTED, 'file:///repo/shared/models/user.rb'), false);
  assert.equal(claimedByNestedFolder(BACKEND, NESTED, 'file:///repo/frontend/app/helpers/tag.rb'), false);
});

test('a sibling folder is left to the selector, and a gem to `Claims`', () => {
  assert.equal(claimedByNestedFolder(APP, [APP, API], 'file:///work/api/app/models/story.rb'), false);
  assert.equal(
    claimedByNestedFolder(APP, [APP, API], `${ACTIVERECORD}/lib/active_record.rb`),
    false,
    'a root outside every folder is claimed by registration, and must not be dropped here'
  );
});

test('a folder whose name merely prefixes another keeps its files', () => {
  // `/repo` against `/repository`: a prefix test without the separator hands one folder's files
  // to a server that was never told about them, and nothing anywhere reports it.
  assert.equal(
    claimedByNestedFolder('file:///repo', ['file:///repo', 'file:///repository'], 'file:///repository/app.rb'),
    false
  );
  assert.equal(
    claimedByNestedFolder('file:///repo/', ['file:///repo/', BACKEND], 'file:///repo/backend/app.rb'),
    true,
    'a folder URI carrying a trailing slash decides the same way'
  );
});

test('a root inside another folder is that folder`s, whoever registered it', () => {
  // The registration side of the nesting problem. `repo/backend`'s server resolves the shared
  // tree its `[index] load_paths` names and asks for it — but that tree is inside `repo`, whose
  // client already claims it by selector. Granting it would put two providers over the file
  // again, from the one direction `claimedByNestedFolder` cannot see.
  const claims = new Claims();
  const shared = 'file:///repo/shared';
  assert.deepEqual(
    claims.narrow(BACKEND, [documents('textDocument/hover', [shared])], NESTED),
    [],
    'the repository folder holds it, and its selector already claims every file in it'
  );
  // The guard: a root inside *no* folder is exactly what this class exists to hand out, and
  // passing the folder list must not have broken that.
  assert.deepEqual(
    claimed(claims.narrow(BACKEND, [documents('textDocument/hover', [ACTIVERECORD])], NESTED)),
    [ACTIVERECORD]
  );
});

test('a server may still claim a root inside its own folder', () => {
  // A vendored bundle at `vendor/bundle` is inside the folder that locked it. The server filters
  // those out before sending — they are already claimed by its own selector — but the rule here
  // is "another folder's", not "any folder's", and getting that backwards would drop a root the
  // server does send: `.gem_rbs_collection/` in a folder that is itself nested.
  const claims = new Claims();
  const vendored = `${BACKEND}/vendor/bundle/ruby/4.0.0/gems/nokogiri-1.19.0`;
  assert.deepEqual(
    claimed(claims.narrow(BACKEND, [documents('textDocument/hover', [vendored])], NESTED)),
    [vendored]
  );
});
