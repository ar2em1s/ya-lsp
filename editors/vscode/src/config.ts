/**
 * VS Code settings, translated into what the server actually reads.
 *
 * No `vscode` import: everything here is a pure function of values the caller looked up, which
 * is the only way any of it can be tested without an extension host.
 */

/** The server's `PartialConfig`. Field names are its wire format, not VS Code's. */
export interface ServerOptions {
  log?: {
    level?: string;
    file?: boolean;
    file_path?: string;
    file_level?: string;
  };
  index?: {
    include?: string[];
    exclude?: string[];
    load_paths?: string[];
    max_files?: number;
    respect_gitignore?: boolean;
  };
  gems?: {
    enabled?: boolean;
    default_gems?: boolean;
    ruby_version?: string;
    paths?: string[];
  };
  rbs?: { enabled?: boolean; stdlib?: boolean; path?: string };
  rails?: {
    enabled?: string | boolean;
    schema?: boolean;
    models?: boolean;
    routes?: boolean;
    entrypoints?: boolean;
    views?: boolean;
  };
  trees?: { test?: string[]; test_support?: string[]; migration?: string[] };
  types?: { structs?: boolean; annotations?: boolean; guess_from_names?: boolean };
  hints?: { block_parameters?: boolean; locals?: boolean; returns?: boolean };
  diagnostics?: { enabled?: boolean; rules?: Record<string, string> };
}

/**
 * A source of *explicitly set* settings.
 *
 * "Explicitly set" is the whole point. `WorkspaceConfiguration.get` folds the default from
 * package.json in with the user's choice and returns one value, so a setting nobody touched is
 * indistinguishable from one deliberately set to the same value. Sending those defaults would
 * make package.json a second source of truth for every default in the Rust config, to be kept in
 * sync forever. Omitting what nobody set leaves exactly one.
 */
export interface Settings {
  /** The user's value for `ya-lsp.<key>`, or `undefined` if they never set one. */
  explicit<T>(key: string): T | undefined;
}

/**
 * A list setting, or `undefined` unless the user set one whose shape this file can vouch for.
 *
 * There is deliberately no empty-value case here, unlike `gems.rubyVersion` and `rbs.path`:
 * `""` is how those two spell "work it out yourself", but `[]` is a value — no excludes, no
 * extra load paths, no extra gem roots. `index.include = []` is the one that indexes nothing,
 * and the server already says so out loud; dropping it here would turn a reported mistake into
 * a setting that silently does nothing.
 */
function strings(settings: Settings, key: string): string[] | undefined {
  const value = settings.explicit<unknown>(key);
  return Array.isArray(value) && value.every((entry) => typeof entry === 'string')
    ? (value as string[])
    : undefined;
}

/**
 * The `initializationOptions` for a workspace folder, or `undefined` when there is nothing to
 * say.
 *
 * Sending `undefined` is meaningfully different from sending `{}`: the server layers this under
 * `ya-lsp.toml`, and a layer that says nothing lets its own defaults through.
 */
export function serverOptions(settings: Settings): ServerOptions | undefined {
  const options: ServerOptions = {};

  const index: NonNullable<ServerOptions['index']> = {};
  const include = strings(settings, 'index.include');
  if (include) {
    index.include = include;
  }
  const exclude = strings(settings, 'index.exclude');
  if (exclude) {
    index.exclude = exclude;
  }
  const loadPaths = strings(settings, 'index.loadPaths');
  if (loadPaths) {
    index.load_paths = loadPaths;
  }
  const maxFiles = settings.explicit<number>('index.maxFiles');
  if (typeof maxFiles === 'number') {
    index.max_files = maxFiles;
  }
  const respectGitignore = settings.explicit<boolean>('index.respectGitignore');
  if (typeof respectGitignore === 'boolean') {
    index.respect_gitignore = respectGitignore;
  }
  if (Object.keys(index).length > 0) {
    options.index = index;
  }

  // `logLevel` is the one setting whose editor name does not map onto its TOML name: it shipped
  // as `ya-lsp.logLevel` when it was an environment variable, and a rename would cost a
  // deprecation, a migration and a window where both keys are read. So the key stays and the
  // value now travels in the layer like every other setting — which is what took it off
  // `RESTART_REQUIRED`, because a level the server is *told* can change while it runs.
  const log: NonNullable<ServerOptions['log']> = {};
  const level = settings.explicit<string>('logLevel');
  if (typeof level === 'string' && level.trim() !== '') {
    log.level = level.trim();
  }
  const logFile = settings.explicit<boolean>('log.file');
  if (typeof logFile === 'boolean') {
    log.file = logFile;
  }
  // Unlike `gems.rubyVersion` and `rbs.path`, an empty string here is not "work it out
  // yourself" — there is no discovery to fall back on — so it is dropped as the mistake it is
  // rather than sent as a path called "".
  const logFilePath = settings.explicit<string>('log.filePath');
  if (typeof logFilePath === 'string' && logFilePath.trim() !== '') {
    log.file_path = logFilePath.trim();
  }
  const logFileLevel = settings.explicit<string>('log.fileLevel');
  if (typeof logFileLevel === 'string' && logFileLevel.trim() !== '') {
    log.file_level = logFileLevel.trim();
  }
  if (Object.keys(log).length > 0) {
    options.log = log;
  }

  const gems: NonNullable<ServerOptions['gems']> = {};
  const gemsEnabled = settings.explicit<boolean>('gems.enabled');
  if (typeof gemsEnabled === 'boolean') {
    gems.enabled = gemsEnabled;
  }
  const defaultGems = settings.explicit<boolean>('gems.defaultGems');
  if (typeof defaultGems === 'boolean') {
    gems.default_gems = defaultGems;
  }
  // An empty string is the "auto-detect" default written out, not a Ruby version. Sending it
  // would make the server look for gems belonging to a Ruby called "".
  const rubyVersion = settings.explicit<string>('gems.rubyVersion');
  if (typeof rubyVersion === 'string' && rubyVersion.trim() !== '') {
    gems.ruby_version = rubyVersion.trim();
  }
  const gemPaths = strings(settings, 'gems.paths');
  if (gemPaths) {
    gems.paths = gemPaths;
  }
  if (Object.keys(gems).length > 0) {
    options.gems = gems;
  }

  const rbs: NonNullable<ServerOptions['rbs']> = {};
  const rbsEnabled = settings.explicit<boolean>('rbs.enabled');
  if (typeof rbsEnabled === 'boolean') {
    rbs.enabled = rbsEnabled;
  }
  const rbsStdlib = settings.explicit<boolean>('rbs.stdlib');
  if (typeof rbsStdlib === 'boolean') {
    rbs.stdlib = rbsStdlib;
  }
  // As with `gems.rubyVersion`: the empty string is the "find one yourself" default written
  // out, and sending it would point the server at a directory called "".
  const rbsPath = settings.explicit<string>('rbs.path');
  if (typeof rbsPath === 'string' && rbsPath.trim() !== '') {
    rbs.path = rbsPath.trim();
  }
  if (Object.keys(rbs).length > 0) {
    options.rbs = rbs;
  }

  // `enabled` is a word rather than a boolean and the server takes both, so it is sent as
  // written: the drop-down offers `auto`, which has no boolean spelling at all.
  const rails: NonNullable<ServerOptions['rails']> = {};
  const railsEnabled = settings.explicit<string>('rails.enabled');
  if (typeof railsEnabled === 'string' && railsEnabled.trim() !== '') {
    rails.enabled = railsEnabled.trim();
  }
  for (const key of ['schema', 'models', 'routes', 'entrypoints', 'views'] as const) {
    const value = settings.explicit<boolean>(`rails.${key}`);
    if (typeof value === 'boolean') {
      rails[key] = value;
    }
  }
  if (Object.keys(rails).length > 0) {
    options.rails = rails;
  }

  // **An empty list is a value here and is sent as one**, unlike `gems.rubyVersion` and
  // `rbs.path` where `""` spells "work it out yourself". `trees.test = []` turns the suite
  // fence off and `trees.migration = []` turns the migration fence off; both are things a
  // project may legitimately mean, and dropping them would turn a deliberate setting into one
  // that silently does nothing.
  const trees: NonNullable<ServerOptions['trees']> = {};
  const testTrees = strings(settings, 'trees.test');
  if (testTrees) {
    trees.test = testTrees;
  }
  const testSupport = strings(settings, 'trees.testSupport');
  if (testSupport) {
    trees.test_support = testSupport;
  }
  const migration = strings(settings, 'trees.migration');
  if (migration) {
    trees.migration = migration;
  }
  if (Object.keys(trees).length > 0) {
    options.trees = trees;
  }

  const types: NonNullable<ServerOptions['types']> = {};
  const guessFromNames = settings.explicit<boolean>('types.guessFromNames');
  if (typeof guessFromNames === 'boolean') {
    types.guess_from_names = guessFromNames;
  }
  const structs = settings.explicit<boolean>('types.structs');
  if (typeof structs === 'boolean') {
    types.structs = structs;
  }
  const annotations = settings.explicit<boolean>('types.annotations');
  if (typeof annotations === 'boolean') {
    types.annotations = annotations;
  }
  if (Object.keys(types).length > 0) {
    options.types = types;
  }

  const hints: NonNullable<ServerOptions['hints']> = {};
  const blockParameters = settings.explicit<boolean>('hints.blockParameters');
  if (typeof blockParameters === 'boolean') {
    hints.block_parameters = blockParameters;
  }
  const locals = settings.explicit<boolean>('hints.locals');
  if (typeof locals === 'boolean') {
    hints.locals = locals;
  }
  const returns = settings.explicit<boolean>('hints.returns');
  if (typeof returns === 'boolean') {
    hints.returns = returns;
  }
  if (Object.keys(hints).length > 0) {
    options.hints = hints;
  }

  const diagnostics: NonNullable<ServerOptions['diagnostics']> = {};
  const diagnosticsEnabled = settings.explicit<boolean>('diagnostics.enabled');
  if (typeof diagnosticsEnabled === 'boolean') {
    diagnostics.enabled = diagnosticsEnabled;
  }
  const rules = settings.explicit<Record<string, string>>('diagnostics.rules');
  if (rules && Object.keys(rules).length > 0) {
    diagnostics.rules = rules;
  }
  if (Object.keys(diagnostics).length > 0) {
    options.diagnostics = diagnostics;
  }

  return Object.keys(options).length > 0 ? options : undefined;
}

/**
 * The environment the server process is started with.
 *
 * **`YA_LSP_LOG` is deliberately not set from `ya-lsp.logLevel` any more.** The level used to be
 * read once by `EnvFilter::try_from_env`, before the server had a client to be configured by,
 * which is why changing it restarted the process; it is a setting in the layer now and the
 * server re-points its own log when it arrives. What the variable is left to mean is what
 * somebody debugging from a terminal typed, and the server treats it as outranking both the
 * setting and `ya-lsp.toml` — so passing the setting through here as well would take that away
 * from the one person it exists for.
 *
 * The shell's own environment is passed on untouched.
 */
export function serverEnvironment(base: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  return { ...base };
}

/**
 * Settings whose value only reaches the server through a fresh process.
 *
 * `logLevel` was on this list for as long as it was an environment variable. It is one entry
 * and not a pair on purpose: a list of one still has to be a list, because `serverPath` is
 * genuinely one of these and the next setting like it should land beside it.
 */
export const RESTART_REQUIRED = ['ya-lsp.serverPath'];
