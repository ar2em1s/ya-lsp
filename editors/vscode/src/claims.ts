/**
 * Which of several running servers answers about a file.
 *
 * Two questions, arriving from opposite directions. Both end the same way if unanswered: two
 * providers over one document, and the user reading every card twice.
 * - [`Claims`] decides a file **no** workspace folder holds (a gem, Ruby's own library). The server
 *   registers the roots it has answers about, because only it knows where they are, and two folders
 *   on one Ruby register the same roots.
 * - [`claimedByNestedFolder`] decides a file **two** folders hold: VS Code lets one folder contain
 *   another, and a selector cannot subtract the inner one.
 *
 * **Only the extension can decide this**, because only it sees every client at once. Deciding needs
 * no knowledge of gems, only of which strings were already handed out. First asker wins, so the
 * answer is stable while that client runs, and there is nothing to arbitrate when two bundles
 * really differ: a root only one of them resolved to is claimed by that one.
 *
 * No `vscode` import, for `config.ts`' reason: it keeps this testable without an extension host.
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
 * Fixed on both sides, because the file watcher's registration arrives on the same channel with no
 * document selector and must be forwarded exactly as sent. Recognising *these* by name keeps this
 * module from touching anything else.
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
   * - A registration not made under [`DOCUMENTS_ID_PREFIX`] passes through untouched, and so does
   *   one with no selector: the client falls back to its own selector, which the watcher's
   *   registration needs.
   * - A registration whose selector is emptied is **dropped, not forwarded empty**: an empty array
   *   is not nullish, so the client would keep it, match nothing, and hold a provider that can
   *   never answer.
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
        // A root inside *another* workspace folder belongs to that folder's client, by its
        // selector; taking it here would put two providers over the file again, from the
        // registration side. The server drops roots inside its own root before sending, the only
        // half it can see; this is the other half, and only the extension has it. A vendored bundle
        // in a sibling folder reaches this, and so does a shared tree another folder put on
        // `index.load_paths`.
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
   * Give up everything a client held, and return which other clients now have a reason to ask
   * again.
   *
   * A selector is fixed at construction, so a client that lost a root cannot pick it up without
   * being rebuilt, and a root nobody owns is a gem file that answers nothing: the silence this
   * mechanism exists to end. The caller decides whether to rebuild: a client being restarted anyway
   * claims its own roots back a moment later.
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
 * **The problem.** VS Code lets one workspace folder contain another, like a monorepo listing
 * `/repo` for shared code and `/repo/backend` for an app with its own `Gemfile.lock`.
 * `getWorkspaceFolder` resolves a file in the inner one to the **innermost** folder, but a document
 * selector cannot: LSP globs have `*`, `**`, `?`, `{}` and `[]` and no way to subtract a path. So
 * the outer client claims the inner folder's files too, both clients match, and every request is
 * answered twice: doubled hover cards, doubled completion items, stacked squiggles.
 *
 * **`index.exclude` does not help.** The outer server indexes an opened buffer whatever its walk
 * collected, so the narrowing must happen here, where the request would be sent.
 *
 * **Only a descendant folder wins.** A sibling's files are already refused by the selector, and a
 * document under no other folder stays this client's however deep it sits (the outer folder alone
 * holds `/repo/shared`).
 *
 * **Prefix comparison on the editor's own URI spelling**, which [`Claims`] already assumes of
 * `baseUri`: both strings are `Uri.toString()` from one process, so they cannot disagree about
 * encoding the way a client's URI and rubydex's can.
 */
export function claimedByNestedFolder(
  folder: string,
  folders: readonly string[],
  document: string
): boolean {
  // Not this client's folder at all, so there is nothing to give away. A root the server registered
  // (a gem, Ruby's own library) arrives looking exactly like this, and `Claims` decides those.
  if (!within(folder, document)) {
    return false;
  }
  const holder = innermostFolder(folders, document);
  return holder !== undefined && holder !== folder;
}

/**
 * The workspace folder that holds `path`, or `undefined` when none does.
 *
 * **Innermost wins**, which is what `getWorkspaceFolder` answers, so it agrees with the editor.
 * Longest match, not first: folders arrive in `.code-workspace` order, which says nothing about
 * containment, and first match would give a nested folder's files to whichever was listed first.
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
 * and so that `/repo` is not read as a prefix of `/repository`.
 */
function within(base: string, path: string): boolean {
  const prefix = base.replace(/\/+$/, '');
  return path === prefix || path.startsWith(`${prefix}/`);
}
