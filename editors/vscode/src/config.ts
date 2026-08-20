/**
 * VS Code settings, translated into what the server actually reads.
 *
 * No `vscode` import: everything here is a pure function of values the caller looked up, which
 * is the only way any of it can be tested without an extension host.
 */

/** The server's `PartialConfig`. Field names are its wire format, not VS Code's. */
export interface ServerOptions {
  index?: { max_files?: number };
  gems?: { enabled?: boolean; default_gems?: boolean; ruby_version?: string };
  rbs?: { enabled?: boolean; stdlib?: boolean; path?: string };
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
 * The `initializationOptions` for a workspace folder, or `undefined` when there is nothing to
 * say.
 *
 * Sending `undefined` is meaningfully different from sending `{}`: the server layers this under
 * `ya-lsp.toml`, and a layer that says nothing lets its own defaults through.
 */
export function serverOptions(settings: Settings): ServerOptions | undefined {
  const options: ServerOptions = {};

  const maxFiles = settings.explicit<number>('index.maxFiles');
  if (typeof maxFiles === 'number') {
    options.index = { max_files: maxFiles };
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
 * The log filter is read once, by `EnvFilter::try_from_env`, before the server has a client to
 * be configured by — so it is an environment variable rather than a setting, and changing it
 * means starting a new process. The extension does that restart rather than leaving the setting
 * quietly inert.
 */
export function serverEnvironment(
  settings: Settings,
  base: NodeJS.ProcessEnv
): NodeJS.ProcessEnv {
  const level = settings.explicit<string>('logLevel');
  if (!level || level === 'off') {
    // Leave an inherited `YA_LSP_LOG` alone when the setting says nothing; someone debugging
    // from a terminal should not have it silently overridden by a default.
    return { ...base };
  }
  return { ...base, YA_LSP_LOG: `ya_lsp=${level}` };
}

/** Settings whose value only reaches the server through a fresh process. */
export const RESTART_REQUIRED = ['ya-lsp.serverPath', 'ya-lsp.logLevel'];
