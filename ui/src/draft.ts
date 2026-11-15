// An unsaved edit of config.yaml or one config section, and what happens to it when the server
// copy changes underneath it. Every earlier text the user typed is kept as a numbered version
// until the user removes it or writes with that removal stated; nothing here drops one.
// Pure; covered by draft.test.ts. The app keeps drafts by path (app.svelte.ts).
import { comparable, merge3, type Conflict } from "./diff.ts";

export type Version = {
  readonly n: number;
  /** What the user had typed when the server copy changed. */
  readonly text: string;
  /** The server copy that text was based on. */
  readonly base: string;
  readonly keptAt: number;
};

/** A change of the user's that was not applied for them, with the version it came from. */
export type Unapplied = Conflict & { readonly version: number };

export type Draft = {
  readonly path: string;
  readonly title: string;
  readonly yaml: boolean;
  /** The server copy `text` started from. A write rereads the server and refuses unless it still matches. */
  readonly base: string;
  readonly text: string;
  /** Earlier texts, oldest first: one for each time the server copy changed under this draft. */
  readonly versions: readonly Version[];
  /** How many of the user's changes the last reconciliation applied. */
  readonly applied: number;
  /** Version used by the last reconciliation, even if its saved text was removed. Zero before reconciliation. */
  readonly appliedVersion: number;
  /**
   * Every change that could not be applied for the user, from any version, oldest first. One
   * leaves the list only when the user says it is handled or removes its version: a later
   * conflict never makes an earlier one look applied.
   */
  readonly unapplied: readonly Unapplied[];
};

export function startDraft(o: { path: string; title: string; yaml: boolean; server: string }): Draft {
  return { path: o.path, title: o.title, yaml: o.yaml, base: o.server, text: o.server, versions: [], applied: 0, appliedVersion: 0, unapplied: [] };
}

/** The draft's text and base as a review compares them (indented JSON for a section). */
export const shown = (d: Draft, text: string) => comparable(text, !d.yaml);
/** The draft would change what the server has. */
export const changesServer = (d: Draft) => shown(d, d.text) !== shown(d, d.base);
/** Something in the draft is not on the server: typed changes, or kept earlier versions. */
export const isUnsaved = (d: Draft) => d.text !== d.base || d.versions.length > 0;

/**
 * The server copy changed to `latest` while this draft was open. The current text becomes
 * the next kept version, and the editor continues from `latest` with the user's changes
 * applied where they do not touch the server's. Changes that do touch them are listed, not
 * applied, so the server's newer values are never overwritten without the user seeing it.
 */
export function reconcile(d: Draft, latest: string, now: number): Draft {
  const m = merge3(shown(d, d.base), shown(d, d.text), shown(d, latest));
  const version: Version = { n: d.appliedVersion + 1, text: d.text, base: d.base, keptAt: now };
  return {
    ...d,
    base: latest,
    text: m.text,
    versions: [...d.versions, version],
    applied: m.applied.length,
    appliedVersion: version.n,
    unapplied: [...d.unapplied, ...m.conflicts.map((c) => ({ ...c, version: version.n }))],
  };
}

/** The user removed one kept version, and with it the changes of it that were not applied. */
export const dropVersion = (d: Draft, n: number): Draft => ({ ...d, versions: d.versions.filter((v) => v.n !== n), unapplied: d.unapplied.filter((u) => u.version !== n) });

/** The user says one change that was not applied is handled: redone in the text, or not wanted. */
export const handled = (d: Draft, change: Unapplied): Draft => ({ ...d, unapplied: d.unapplied.filter((u) => u !== change) });
