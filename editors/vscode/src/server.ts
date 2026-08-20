/**
 * Which `ya-lsp` binary to run.
 *
 * No `vscode` import, for the same reason as `config.ts`: this is the part most likely to be
 * wrong on a platform nobody tested on, so it has to be testable without one.
 */

import * as path from 'node:path';

export interface Lookup {
  /** `ya-lsp.serverPath`, or empty when unset. */
  configured: string;
  /** The installed extension's directory, which holds the bundled binary. */
  extensionPath: string;
  /** The folder `${workspaceFolder}` stands for. */
  workspaceFolder?: string;
  /** `os.homedir()`. */
  home: string;
  /** `process.platform`. */
  platform: NodeJS.Platform;
  /** Whether a path exists and can be executed. */
  isExecutable(candidate: string): boolean;
}

export type Resolved =
  | { kind: 'configured' | 'bundled'; command: string }
  | { kind: 'missing'; message: string };

/** Where the VSIX puts the binary. CI writes it here before packaging. */
export function bundledPath(extensionPath: string, platform: NodeJS.Platform): string {
  const name = platform === 'win32' ? 'ya-lsp.exe' : 'ya-lsp';
  return path.join(extensionPath, 'server', name);
}

/**
 * The configured binary if there is one, otherwise the bundled one.
 *
 * A configured path that does not exist is an error rather than a silent fall back to the
 * bundle: someone who set the path is developing against a specific build, and running a
 * different one instead would produce results they would spend a long time not understanding.
 */
export function resolveServer(lookup: Lookup): Resolved {
  const configured = lookup.configured.trim();
  if (configured !== '') {
    const expanded = expand(configured, lookup);
    if (lookup.isExecutable(expanded)) {
      return { kind: 'configured', command: expanded };
    }
    return {
      kind: 'missing',
      message:
        `ya-lsp.serverPath points at ${expanded}, which is not an executable file. ` +
        `Fix the setting or clear it to use the bundled server.`,
    };
  }

  const bundled = bundledPath(lookup.extensionPath, lookup.platform);
  if (lookup.isExecutable(bundled)) {
    return { kind: 'bundled', command: bundled };
  }
  return {
    kind: 'missing',
    message:
      `This build of the ya-lsp extension has no server binary for ${lookup.platform}. ` +
      `Install the extension from the Marketplace, which ships one per platform, or set ` +
      `ya-lsp.serverPath to a binary you built yourself.`,
  };
}

/**
 * `~` and `${workspaceFolder}`, the two substitutions people expect a path setting to make.
 *
 * VS Code performs neither: `${workspaceFolder}` is resolved for launch configurations and task
 * definitions, not for settings, and a `~` reaches the process verbatim. Both are what somebody
 * types first, so both are handled here rather than left as a mystery.
 */
function expand(value: string, lookup: Lookup): string {
  let expanded = value;
  if (lookup.workspaceFolder !== undefined) {
    expanded = expanded.replaceAll('${workspaceFolder}', lookup.workspaceFolder);
  }
  if (expanded === '~') {
    expanded = lookup.home;
  } else if (expanded.startsWith('~/') || expanded.startsWith('~\\')) {
    expanded = path.join(lookup.home, expanded.slice(2));
  }
  return path.normalize(expanded);
}
