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

import { Claims, DOCUMENTS_ID_PREFIX, Registration } from './claims';

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
