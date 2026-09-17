/**
 * Which one of several servers answers about a file, when several are running.
 *
 * Two questions, and they arrive from opposite directions. [`Claims`] decides a file that **no**
 * workspace folder holds — a gem, Ruby's own library — which two bundles can resolve to alike.
 * [`claimedByNestedFolder`] decides a file that **two** folders hold, because VS Code lets one
 * workspace folder contain another and a selector cannot subtract the inner one. Both end in the
 * same place: two providers over one document, and the user reading the same card twice.
 *
 * No `vscode` import, for `config.ts`'s reason and one more. The server registers the roots it has
 * answers about — a gem's source, Ruby's own library, the RBS beside them — because it is the only
 * side that knows where they are. In a multi-root workspace every folder's server registers *its*
 * roots, and two folders on one Ruby resolve to the same ones: without this, two providers answer
 * one hover and the user reads the same card twice, which is the failure the narrow per-folder
 * selector was protecting against arriving from the other direction.
 *
 * **The extension is the only place that can decide this**, because it is the only place that sees
 * every client at once — and deciding it needs no knowledge of gems at all, only of which strings
 * have already been handed out. First asker wins, so the answer is stable for as long as that
 * client is running and there is nothing to arbitrate when two bundles genuinely differ: a root
 * only one of them resolved to is claimed by that one.
 */

/** LSP 3.18's relative pattern: the only narrowing shape `vscode-languageclient` understands. */
export interface RelativePattern {
  readonly baseUri: string;
  readonly pattern: string;
}

/** One entry of a document selector, in the protocol's own shape. */
export interface DocumentFilter {
  readonly scheme: string;
  readonly language: string;
  readonly pattern?: RelativePattern;
}

/** One registration as the server sends it, narrowed to the fields this module reads. */
export interface Registration {
  readonly id: string;
  readonly method: string;
  registerOptions?: { documentSelector?: DocumentFilter[] } & Record<string, unknown>;
}

/**
 * The prefix the server makes its document registrations under.
 *
 * Fixed on both sides, and the reason it has to be: the file watcher's registration arrives on the
 * same channel and carries no document selector, so it must be forwarded exactly as sent.
 * Recognising *these* by name is what keeps this module from touching anything else.
 */
export const DOCUMENTS_ID_PREFIX = 'ya-lsp-documents/';

/** Which client answers about each root outside the workspace folders. */
export class Claims {
  /** Every root a client asked for, whether or not it won it. Read when an owner goes away. */
  private readonly asked = new Map<string, Set<string>>();
  /** Which client owns each root. */
  private readonly owner = new Map<string, string>();

  /**
   * Narrow one batch of registrations to the roots no other client already holds.
   *
   * Registrations the server did not make under [`DOCUMENTS_ID_PREFIX`] pass through untouched, and
   * so does one carrying no selector — the client falls back to its own for those, which is the
   * behaviour the watcher's registration needs. A registration whose selector is emptied is
   * **dropped rather than forwarded empty**: an empty array is not nullish, so the client would
   * keep it, match nothing with it, and hold a provider that can never answer.
   */
  narrow(
    client: string,
    registrations: readonly Registration[],
    folders: readonly string[] = []
  ): Registration[] {
    const mine = this.asked.get(client) ?? new Set<string>();
    this.asked.set(client, mine);

    const kept: Registration[] = [];
    for (const registration of registrations) {
      const selector = registration.registerOptions?.documentSelector;
      if (!registration.id.startsWith(DOCUMENTS_ID_PREFIX) || !selector) {
        kept.push(registration);
        continue;
      }
      const narrowed = selector.filter((filter) => {
        const base = filter.pattern?.baseUri;
        if (base === undefined) {
          return true;
        }
        // A root that lies inside *another* workspace folder is that folder's client's, by its
        // selector, and no arbitration here can change that — taking it would put two providers
        // over the file again from the registration side. The server drops the roots inside its
        // own root before sending, which is the only half it can see; this is the other half, and
        // only the extension has it. A vendored bundle in a sibling folder reaches this, and so
        // does a shared tree a second folder put on `[index] load_paths`.
        const holder = innermostFolder(folders, base);
        if (holder !== undefined && holder !== client) {
          return false;
        }
        mine.add(base);
        const owner = this.owner.get(base);
        if (owner !== undefined && owner !== client) {
          return false;
        }
        this.owner.set(base, client);
        return true;
      });
      if (narrowed.length > 0) {
        kept.push({
          ...registration,
          registerOptions: { ...registration.registerOptions, documentSelector: narrowed },
        });
      }
    }
    return kept;
  }

  /**
   * Give up everything a client held, and say which other clients now have a reason to ask again.
   *
   * A selector cannot be changed after construction, so a client that lost a root the first time
   * cannot pick it up later without being rebuilt — and a root nobody owns is a gem file that
   * answers nothing, which is the silence this whole mechanism exists to end. The caller decides
   * whether a rebuild is right: a client stopping on its way to being restarted anyway will claim
   * its own roots back a moment later.
   */
  release(client: string): string[] {
    const released = new Set<string>();
    for (const [base, owner] of [...this.owner]) {
      if (owner === client) {
        this.owner.delete(base);
        released.add(base);
      }
    }
    this.asked.delete(client);

    const orphaned = new Set<string>();
    for (const [other, bases] of this.asked) {
      if ([...bases].some((base) => released.has(base))) {
        orphaned.add(other);
      }
    }
    return [...orphaned];
  }
}

/**
 * Whether a document inside this client's folder is really a nested folder's to answer.
 *
 * VS Code lets one workspace folder contain another — a monorepo listing `/repo` for the code
 * beside the apps and `/repo/backend` for an application with its own `Gemfile.lock` — and
 * `getWorkspaceFolder` resolves a file in the inner one to the **innermost** folder. A document
 * selector cannot say that. LSP's glob syntax has `*`, `**`, `?`, `{}` and `[]` and no way to
 * subtract a path, so "under `/repo` but not under `/repo/backend`" is unsayable, and the outer
 * folder's client claims the inner folder's files along with its own. Both clients then match,
 * `languages.match` scores both, and every request is answered twice: a hover card printed twice,
 * a completion list with every item doubled, one set of squiggles on top of another.
 *
 * `index.exclude` does not reach it. The outer server indexes an opened buffer whatever its walk
 * collected, so it answers about a file it was told to ignore — which is why the narrowing has to
 * happen on this side, at the point the request would be sent.
 *
 * Only a **descendant** folder wins. A sibling's files are already refused by the selector, and a
 * document under no other folder stays this client's however deep it sits — the outer folder is
 * still the only one that holds `/repo/shared`.
 *
 * Prefix comparison on the editor's own URI spelling, which is what [`Claims`] already assumes of
 * `baseUri`: both strings are `Uri.toString()` from the same process, so they cannot disagree
 * about encoding the way a client's URI and rubydex's can.
 */
export function claimedByNestedFolder(
  folder: string,
  folders: readonly string[],
  document: string
): boolean {
  // Not this client's folder at all, so there is nothing here to give away. A root the server
  // registered — a gem, Ruby's own library — arrives looking exactly like this, and `Claims` is
  // what decides those.
  if (!within(folder, document)) {
    return false;
  }
  const holder = innermostFolder(folders, document);
  return holder !== undefined && holder !== folder;
}

/**
 * The workspace folder that holds `path`, or `undefined` when none does.
 *
 * **Innermost wins**, which is what `getWorkspaceFolder` answers and therefore the only answer
 * that agrees with the editor. Longest match rather than first: folders arrive in the order the
 * `.code-workspace` lists them, which says nothing about which contains which, and taking the
 * first would hand a nested folder's files to whichever of the two happened to be written down
 * first.
 */
function innermostFolder(folders: readonly string[], path: string): string | undefined {
  let held: string | undefined;
  for (const folder of folders) {
    if (within(folder, path) && (held === undefined || folder.length > held.length)) {
      held = folder;
    }
  }
  return held;
}

/**
 * Whether `path` is `base` or sits under it.
 *
 * The trailing slash is trimmed because a folder URI may carry one and a document URI never does,
 * and because `/repo` must not be read as a prefix of `/repository`.
 */
function within(base: string, path: string): boolean {
  const prefix = base.replace(/\/+$/, '');
  return path === prefix || path.startsWith(`${prefix}/`);
}
