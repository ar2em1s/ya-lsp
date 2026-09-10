/**
 * Whether a folder lints with RuboCop, and the extension that would do it.
 *
 * ya-lsp never runs Ruby, so it has no cop offences and no autocorrect. That gap is closed by
 * composition rather than by a proxy: RuboCop ships its own language server, the RuboCop team
 * ships the client for it, and LSP allows one language to be served by more than one server. Proxying `rubocop --lsp` through ya-lsp would buy nothing — RuboCop is Ruby and
 * has to be installed either way, so there is no "one thing to install" to win — while costing
 * a supervised child process, a diagnostics merge, and the first module in this repository that
 * CI could not cover without installing Ruby.
 *
 * No `vscode` import, for the reason `config.ts` and `server.ts` have none: this is a guess
 * about somebody else's toolchain, and a guess that cannot be unit-tested is one that ships
 * wrong. `extension.ts` owns the notification; this module owns only the question.
 */

/** The official client, published by the RuboCop team. It starts `rubocop --lsp` over stdio. */
export const RUBOCOP_EXTENSION = 'rubocop.vscode-rubocop';

/** The files this module needs, so the decision can be tested without a disk. */
export interface FolderFiles {
  /** The file's text, or `undefined` when it is absent or unreadable. */
  read(relative: string): string | undefined;
}

/**
 * RuboCop's own `ConfigFinder::DOTFILE`, plus the spelling YAML users reach for anyway.
 *
 * Only the folder root is checked. RuboCop itself walks upwards, but a `.rubocop.yml` in a
 * parent of the workspace is somebody else's project, and suggesting an install on the strength
 * of it is exactly the kind of confident wrong guess this project is organised against.
 */
const CONFIG_FILES = ['.rubocop.yml', '.rubocop.yaml'];

/**
 * `rubocop` as a resolved *spec* in the lockfile, which is four spaces of indent.
 *
 * Not a bare substring search: `rubocop-ast` and `rubocop-rails` appear as dependency lines at
 * six spaces under gems that are not RuboCop, and `DEPENDENCIES` lists names at two. Matching
 * the spec line is what makes "in the bundle at all, however it got there" the question —
 * which is the right one, since a transitive RuboCop still lints.
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
