/**
 * The manifest, against the settings this extension actually reads.
 *
 * `package.json` is a JSON document no compiler looks at, and two of its fields decide whether a
 * setting works at all. A property `config.ts` reads and the manifest never declares cannot be
 * set from the settings UI. And a `scope` VS Code will not honour inside a workspace folder is a
 * setting a multi-root workspace cannot vary — which is what shipped for two releases, while
 * `extension.ts` read every one of them per folder because it runs one server per folder. Neither
 * defect is visible to review, so neither is left to it.
 *
 * `tests/vscode_manifest.rs` holds the half this cannot see: the defaults, and the rule names.
 */

import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { test } from 'node:test';

import { RESTART_REQUIRED, serverOptions } from './config';

interface Property {
  scope?: string;
  description?: string;
  markdownDescription?: string;
  enum?: string[];
  enumDescriptions?: string[];
  properties?: Record<string, Property>;
  additionalProperties?: Property;
}

interface Category {
  title?: string;
  properties: Record<string, Property>;
}

const categories: Category[] = JSON.parse(
  fs.readFileSync(path.resolve(__dirname, '..', 'package.json'), 'utf8')
).contributes.configuration;

/** Every `ya-lsp.*` property, flattened out of the categories it is rendered in. */
const properties: Record<string, Property> = Object.assign(
  {},
  ...categories.map((category) => category.properties)
);

/**
 * The settings `serverOptions` reads, taken from `serverOptions` itself.
 *
 * Writing the list out here would be a third copy to keep in step with the other two. Asking for
 * every setting and answering `undefined` to all of them produces the same list by construction,
 * which is the only version that cannot drift.
 */
const READ_BY_CONFIG: string[] = [];
serverOptions({
  explicit<T>(key: string): T | undefined {
    READ_BY_CONFIG.push(`ya-lsp.${key}`);
    return undefined;
  },
});

/** Every property a user picks by name, including the rules declared inside the rule map. */
function* named(): Generator<[string, Property]> {
  for (const [name, property] of Object.entries(properties)) {
    yield [name, property];
    for (const [key, declared] of Object.entries(property.properties ?? {})) {
      yield [`${name}.${key}`, declared];
    }
  }
}

test('every setting the extension sends is one the manifest declares', () => {
  // The failure this catches is total and silent: a setting read here and declared nowhere has
  // no UI, no completion in settings.json, and no way for anyone to discover it exists.
  assert.ok(READ_BY_CONFIG.length > 0, 'serverOptions must read something');
  for (const setting of READ_BY_CONFIG) {
    assert.ok(properties[setting], `${setting} is read by config.ts and declared nowhere`);
  }
});

test('and every setting the manifest declares reaches somebody', () => {
  // The other direction, which is how five settings sat in `ya-lsp.toml` and nowhere else: a
  // property nothing reads is a promise the extension does not keep. These three never reach the
  // server — two the extension acts on itself, and one `vscode-languageclient` reads for its own
  // tracing — so they are named rather than matched by pattern.
  assert.deepEqual(
    Object.keys(properties)
      .filter((setting) => !READ_BY_CONFIG.includes(setting))
      .sort(),
    ['ya-lsp.logLevel', 'ya-lsp.serverPath', 'ya-lsp.trace.server']
  );
});

test('every setting the server reads is honoured inside a workspace folder', () => {
  // A property with no `scope` at all is `window`-scoped, and VS Code does not read a
  // `window`-scoped setting from a folder's `.vscode/settings.json`. The per-folder read in
  // `extension.ts` is real; without this the value it reads could not vary.
  for (const setting of READ_BY_CONFIG) {
    assert.equal(
      properties[setting].scope,
      'resource',
      `${setting} cannot be set per workspace folder`
    );
  }
});

test('and the two that decide which process is spawned can also be set per machine', () => {
  // `machine-overridable` is honoured in a folder too, and additionally lets a remote or a
  // container carry its own path to the binary and its own log level.
  assert.deepEqual(RESTART_REQUIRED, ['ya-lsp.serverPath', 'ya-lsp.logLevel']);
  for (const setting of RESTART_REQUIRED) {
    assert.equal(properties[setting].scope, 'machine-overridable', `${setting} scope`);
  }
  // The client reads this one once for the window rather than per folder, so `window` is not an
  // oversight here — it is the scope that matches who reads it.
  assert.equal(properties['ya-lsp.trace.server'].scope, 'window');
});

test('every setting says what it does', () => {
  for (const [name, property] of named()) {
    assert.ok(
      property.markdownDescription ?? property.description,
      `${name} is offered with no explanation at all`
    );
  }
});

test('and every choice says what choosing it does', () => {
  // An enum with no `enumDescriptions` is a drop-down of bare words — `hint` against `warning`,
  // `messages` against `verbose` — where the difference is the entire decision being made.
  const open = properties['ya-lsp.diagnostics.rules'].additionalProperties;
  assert.ok(open, 'an eleventh rule from rubydex still has to validate');
  for (const [name, property] of [...named(), ['ya-lsp.diagnostics.rules.*', open] as const]) {
    if (property.enum) {
      assert.equal(
        property.enumDescriptions?.length,
        property.enum.length,
        `${name} explains a different number of choices than it offers`
      );
    }
  }
});

test('the settings are grouped, and no setting is declared in two groups', () => {
  // `properties` above is a merge, so a duplicate would be silently swallowed by the last
  // category to declare it while the settings UI showed it twice.
  const declared = categories.flatMap((category) => Object.keys(category.properties));
  assert.equal(declared.length, new Set(declared).size, 'a setting is declared twice');
  assert.equal(declared.length, Object.keys(properties).length);
  for (const category of categories) {
    assert.ok(category.title, 'a category with no title is a heading VS Code cannot render');
  }
});
