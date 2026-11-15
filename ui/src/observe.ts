// What this tab actually saw of a time window. The live feed reports only requests that
// finish while the tab watches, so a window can hold time before watching started, gaps
// the server reported, and time after watching stopped. Counts over such a window are
// lower bounds, and a rate must divide by the time watched, not by the window.
// Pure: covered by observe.test.ts.

export type Span = { from: number; to: number };
export type Blind = Span & { why: "before" | "gap" | "stopped" };

/** Time in [from, to] this tab did not see, merged where reasons overlap, oldest first. */
export function blindSpans(from: number, to: number, w: { since: number; gaps: Span[]; stoppedAt: number }): Blind[] {
  const raw: Blind[] = [];
  if (!w.since) raw.push({ from, to, why: "before" });
  else if (w.since > from) raw.push({ from, to: Math.min(w.since, to), why: "before" });
  for (const g of w.gaps) raw.push({ from: g.from, to: g.to, why: "gap" });
  if (w.since && w.stoppedAt) raw.push({ from: w.stoppedAt, to, why: "stopped" });
  return raw
    .map((b) => ({ ...b, from: Math.max(from, b.from), to: Math.min(to, b.to) }))
    .filter((b) => b.to > b.from)
    .sort((a, b) => a.from - b.from);
}

/** Milliseconds of [from, to] covered by none of `blind`. */
export function watchedMs(from: number, to: number, blind: Span[]): number {
  let seen = 0;
  let at = from;
  for (const b of [...blind].sort((x, y) => x.from - y.from)) {
    if (b.from > at) seen += Math.min(b.from, to) - at;
    at = Math.max(at, b.to);
    if (at >= to) break;
  }
  if (at < to) seen += to - at;
  return Math.max(0, seen);
}

/**
 * Requests a minute over the watched time. Null while too little was watched to say: under
 * 30 seconds one request reads as a burst rate that means nothing.
 */
export function perMinute(count: number, watched: number): number | null {
  return watched < 30_000 ? null : count / (watched / 60_000);
}

/** "4.2 a minute", or why there is no rate yet. */
export function rateText(count: number, watched: number): string {
  const r = perMinute(count, watched);
  if (r === null) return watched > 0 ? "rate after 30 s of watching" : "nothing watched in this window";
  return `${r < 10 ? r.toFixed(1) : Math.round(r)} a minute`;
}

/** The middle value of the positive ones; null when there are none. */
export function median(values: number[]): number | null {
  const v = values.filter((n) => n > 0).sort((a, b) => a - b);
  return v.length ? v[Math.floor((v.length - 1) / 2)] : null;
}
