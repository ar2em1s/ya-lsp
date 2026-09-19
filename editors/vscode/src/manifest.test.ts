/**
 * The manifest, against the settings this extension actually reads.
 *
 * `package.json` is JSON no compiler looks at, and two of its fields decide whether a setting works
 * at all:
 * - a property `config.ts` reads that the manifest never declares cannot be set from the settings
 *   UI;
 * - a `scope` VS Code will not honour inside a workspace folder makes a setting a multi-root
 *   workspace cannot vary, while `extension.ts` reads every setting per folder.
 * Neither defect is visible in review, so neither is left to review.
 *
 * `tests/vscode_manifest.rs` holds the half this cannot see: the defaults and the rule names.
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
 * Writing the list out would be a third copy to keep in step. Asking for every setting and
 * answering `undefined` to all of them produces the list by construction, the only version that
 * cannot drift.
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
  // The failure this catches is total and silent: a setting read here and declared nowhere has no
  // UI, no completion in settings.json, and no way for anyone to discover it.
  assert.ok(READ_BY_CONFIG.length > 0, 'serverOptions must read something');
  for (const setting of READ_BY_CONFIG) {
    assert.ok(properties[setting], `${setting} is read by config.ts and declared nowhere`);
  }
});

test('and every setting the manifest declares reaches somebody', () => {
  // The other direction: a declared property nothing reads is a promise the extension does not
  // keep. These four never reach the server (the extension acts on three itself, and
  // `vscode-languageclient` reads one for its own tracing), so they are named, not matched by
  // pattern.
  //
  // `rubocop.hint` especially: the server deserializes `initializationOptions` with
  // `deny_unknown_fields`, so a client-only setting leaking into `serverOptions` would not be
  // ignored. It would reject the whole layer and silently switch every other setting off.
  assert.deepEqual(
    Object.keys(properties)
      .filter((setting) => !READ_BY_CONFIG.includes(setting))
      .sort(),
    ['ya-lsp.rubocop.hint', 'ya-lsp.serverPath', 'ya-lsp.trace.server']
  );
});

test('every setting the server reads is honoured inside a workspace folder', () => {
  // A property with no `scope` is `window`-scoped, and VS Code does not read a `window` setting
  // from a folder's `.vscode/settings.json`, so the per-folder read in `extension.ts` could not
  // vary.
  //
  // `machine-overridable` is honoured in a folder too, and also lets a container carry its own
  // value. `logLevel` has it because a remote with its own log level is exactly what that scope is
  // for.
  for (const setting of READ_BY_CONFIG) {
    assert.ok(
      ['resource', 'machine-overridable'].includes(properties[setting].scope ?? ''),
      `${setting} cannot be set per workspace folder`
    );
  }
  assert.equal(properties['ya-lsp.logLevel'].scope, 'machine-overridable');
});

test('and the two that decide which process is spawned can also be set per machine', () => {
  // `machine-overridable` is honoured in a folder too, and also lets a remote or a container carry
  // its own path to the binary and its own log level.
  assert.deepEqual(RESTART_REQUIRED, ['ya-lsp.serverPath']);
  for (const setting of RESTART_REQUIRED) {
    assert.equal(properties[setting].scope, 'machine-overridable', `${setting} scope`);
  }
  // The client reads this once for the window, not per folder, so `window` is the scope that
  // matches its reader.
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
  // An enum with no `enumDescriptions` is a drop-down of bare words (`hint` against `warning`,
  // `messages` against `verbose`), where the difference is the whole decision.
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
  // `properties` above is a merge, so a duplicate would be silently swallowed by the last category
  // to declare it, while the settings UI showed it twice.
  const declared = categories.flatMap((category) => Object.keys(category.properties));
  assert.equal(declared.length, new Set(declared).size, 'a setting is declared twice');
  assert.equal(declared.length, Object.keys(properties).length);
  for (const category of categories) {
    assert.ok(category.title, 'a category with no title is a heading VS Code cannot render');
  }
});
