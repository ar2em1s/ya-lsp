/**
 * Which one of several servers answers about a file that no workspace folder holds.
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
  narrow(client: string, registrations: readonly Registration[]): Registration[] {
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
