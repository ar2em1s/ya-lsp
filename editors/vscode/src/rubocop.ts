/**
 * Whether a folder lints with RuboCop, and the extension that would do it.
 *
 * ya-lsp never runs Ruby, so it has no cop offences and no autocorrect. The gap is closed by
 * composition, not a proxy: RuboCop ships its own language server, the RuboCop team ships its
 * client, and LSP lets one language have several servers. Proxying `rubocop --lsp` through ya-lsp
 * would buy nothing (RuboCop is Ruby and must be installed either way) and would cost a supervised
 * child process, a diagnostics merge, and the first module CI could not cover without installing
 * Ruby.
 *
 * No `vscode` import, like `config.ts` and `server.ts`: this guesses at somebody else's toolchain,
 * and a guess that cannot be unit-tested ships wrong. `extension.ts` owns the notification; this
 * module owns only the question.
 */

/** The official client, published by the RuboCop team. It starts `rubocop --lsp` over stdio. */
export const RUBOCOP_EXTENSION = 'rubocop.vscode-rubocop';

/** The files this module needs, so the decision can be tested without a disk. */
export interface FolderFiles {
  /** The file's text, or `undefined` when it is absent or unreadable. */
  read(relative: string): string | undefined;
}

/**
 * RuboCop's own `ConfigFinder::DOTFILE`, plus the `.yaml` spelling people use anyway.
 *
 * Only the folder root is checked. RuboCop itself walks upwards, but a `.rubocop.yml` above the
 * workspace belongs to somebody else's project, and suggesting an install because of it is the kind
 * of confident wrong guess this project avoids.
 */
const CONFIG_FILES = ['.rubocop.yml', '.rubocop.yaml'];

/**
 * `rubocop` as a resolved *spec* in the lockfile, at four spaces of indent.
 *
 * Not a bare substring: `rubocop-ast` and `rubocop-rails` appear as dependency lines at six spaces
 * under other gems, and `DEPENDENCIES` lists names at two. Matching the spec line asks "is RuboCop
 * in the bundle at all, however it got there", the right question, since a transitive RuboCop still
 * lints.
 */
const SPEC_LINE = /^ {4}rubocop \(/m;

/** Whether this folder looks like it wants RuboCop's diagnostics. */
export function usesRubocop(files: FolderFiles): boolean {
  if (CONFIG_FILES.some((name) => files.read(name) !== undefined)) {
    return true;
  }
  const lockfile = files.read('Gemfile.lock');
  return lockfile !== undefined && SPEC_LINE.test(lockfile);
}
