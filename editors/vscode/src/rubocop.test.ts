import assert from 'node:assert/strict';
import { test } from 'node:test';

import { FolderFiles, RUBOCOP_EXTENSION, usesRubocop } from './rubocop';

/** A folder holding exactly these files. */
function folder(files: Record<string, string>): FolderFiles {
  return { read: (relative) => files[relative] };
}

/** The shape a real lockfile has: specs at four spaces, their dependencies at six. */
function lockfile(specs: string): string {
  return `GEM\n  remote: https://rubygems.org/\n  specs:\n${specs}\nDEPENDENCIES\n  rails\n`;
}

test('a folder with neither a config nor a lockfile does not want RuboCop', () => {
  assert.equal(usesRubocop(folder({})), false);
});

test('a .rubocop.yml is enough on its own', () => {
  // A project can lint with a globally installed RuboCop and no Gemfile at all.
  assert.equal(usesRubocop(folder({ '.rubocop.yml': 'AllCops:\n' })), true);
  assert.equal(usesRubocop(folder({ '.rubocop.yaml': 'AllCops:\n' })), true);
});

test('an empty config file still counts', () => {
  // `read` returning `''` is a file that exists; only `undefined` is absence, and `''` is
  // falsy in a way that would silently reverse this answer if the check were on truthiness.
  assert.equal(usesRubocop(folder({ '.rubocop.yml': '' })), true);
});

test('rubocop in the bundle is enough on its own', () => {
  assert.equal(
    usesRubocop(folder({ 'Gemfile.lock': lockfile('    rubocop (1.87.0)\n      json (~> 2.3)') })),
    true
  );
});

test('and a transitive rubocop counts, because it still lints', () => {
  // Nobody wrote `gem "rubocop"`; `rubocop-rails` pulled it in. It is in the bundle, `bundle
  // exec rubocop` runs, and the official extension will find it.
  assert.equal(
    usesRubocop(
      folder({
        'Gemfile.lock': lockfile(
          '    rubocop (1.81.7)\n      json (~> 2.3)\n    rubocop-rails (2.34.2)\n      rubocop (>= 1.75.0, < 2.0)'
        ),
      })
    ),
    true
  );
});

test('a lockfile that only mentions rubocop as a dependency of something absent does not count', () => {
  // The guard the four-space anchor exists for: `rubocop-ast` is a spec, and the word `rubocop`
  // appears inside its name and inside dependency lines at six spaces. None of that is RuboCop
  // being installed, and a bare substring search would say it was.
  assert.equal(
    usesRubocop(
      folder({
        'Gemfile.lock': lockfile(
          '    rubocop-ast (1.48.0)\n      parser (>= 3.3.7.2)\n    parser (3.3.9.0)\n      racc'
        ),
      })
    ),
    false
  );
});

test('the name of the extension is the one the RuboCop team publishes', () => {
  // Spelled wrong, `getExtension` never matches and the hint shows to someone who already has
  // it — every session, forever.
  assert.equal(RUBOCOP_EXTENSION, 'rubocop.vscode-rubocop');
});
