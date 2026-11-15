// Line diffs for reviewing configuration edits: a shortest edit script (Myers), the changed
// places with a few lines of context, and a three-way merge that puts the user's changes on
// top of a newer server copy where they do not touch the server's own changes.
// Pure; covered by diff.test.ts.

export type Line =
  | { readonly kind: "same"; readonly text: string; readonly a: number; readonly b: number }
  | { readonly kind: "removed"; readonly text: string; readonly a: number }
  | { readonly kind: "added"; readonly text: string; readonly b: number };

/**
 * Text split into lines; "\r\n" counts as "\n". A line break ends a line, so a text ending in
 * one has no empty last line: appending after it is one added line, not a changed blank one.
 * An empty text has no lines.
 */
export function lines(text: string): string[] {
  const t = text.replace(/\r\n/g, "\n");
  if (t === "") return [];
  const out = t.split("\n");
  if (t.endsWith("\n")) out.pop();
  return out;
}
/** The text ends with a line break (which `lines` does not show as a line). */
export const endsLine = (text: string) => text.endsWith("\n");

/**
 * The text a review compares. JSON is shown as indented JSON, so a draft typed compactly or
 * with keys on other lines differs only where a value differs. What is written is still the
 * parsed draft; this changes only the display. Text that is not JSON stays as it is.
 */
export function comparable(text: string, json: boolean): string {
  if (!json) return text.replace(/\r\n/g, "\n");
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text.replace(/\r\n/g, "\n");
  }
}

/** A shortest edit script from `a` to `b`, as same, removed and added lines in order. */
export function diffLines(a: readonly string[], b: readonly string[]): Line[] {
  let start = 0;
  while (start < a.length && start < b.length && a[start] === b[start]) start++;
  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA--;
    endB--;
  }
  const out: Line[] = [];
  for (let i = 0; i < start; i++) out.push({ kind: "same", text: a[i], a: i, b: i });
  out.push(...middle(a, b, start, endA, start, endB));
  for (let i = endA, j = endB; i < a.length; i++, j++) out.push({ kind: "same", text: a[i], a: i, b: j });
  return out;
}

/** Myers' O((N+M)D) search over a[a0,a1) and b[b0,b1), keeping each round to walk back. */
function middle(a: readonly string[], b: readonly string[], a0: number, a1: number, b0: number, b1: number): Line[] {
  const n = a1 - a0;
  const m = b1 - b0;
  if (!n) return b.slice(b0, b1).map((text, i) => ({ kind: "added", text, b: b0 + i }));
  if (!m) return a.slice(a0, a1).map((text, i) => ({ kind: "removed", text, a: a0 + i }));
  const max = n + m;
  const offset = max + 1;
  let v = new Int32Array(2 * max + 3);
  const trace: Int32Array[] = [];
  let found = -1;
  for (let d = 0; d <= max && found < 0; d++) {
    trace.push(v.slice());
    const next = v.slice();
    for (let k = -d; k <= d; k += 2) {
      let x = k === -d || (k !== d && v[offset + k - 1] < v[offset + k + 1]) ? v[offset + k + 1] : v[offset + k - 1] + 1;
      let y = x - k;
      while (x < n && y < m && a[a0 + x] === b[b0 + y]) {
        x++;
        y++;
      }
      next[offset + k] = x;
      if (x >= n && y >= m) {
        found = d;
        break;
      }
    }
    v = next;
  }
  // Walk back from (n, m) through the saved rounds.
  const rev: Line[] = [];
  let x = n;
  let y = m;
  for (let d = found; d > 0; d--) {
    const before = trace[d];
    const k = x - y;
    const down = k === -d || (k !== d && before[offset + k - 1] < before[offset + k + 1]);
    const pk = down ? k + 1 : k - 1;
    const px = before[offset + pk];
    const py = px - pk;
    while (x > px + (down ? 0 : 1) && y > py + (down ? 1 : 0)) {
      x--;
      y--;
      rev.push({ kind: "same", text: a[a0 + x], a: a0 + x, b: b0 + y });
    }
    if (down) {
      y--;
      rev.push({ kind: "added", text: b[b0 + y], b: b0 + y });
    } else {
      x--;
      rev.push({ kind: "removed", text: a[a0 + x], a: a0 + x });
    }
  }
  while (x > 0 && y > 0) {
    x--;
    y--;
    rev.push({ kind: "same", text: a[a0 + x], a: a0 + x, b: b0 + y });
  }
  return rev.reverse();
}

/** One changed place with context. Line numbers count from 1, as an editor shows them. */
export type Hunk = {
  readonly fromLine: number;
  readonly toLine: number;
  readonly lines: readonly Line[];
  readonly added: number;
  readonly removed: number;
};

/** The changed places of a diff, each with `context` unchanged lines around it; nearby places merge. */
export function hunks(diff: readonly Line[], context = 3): Hunk[] {
  const changed = diff.flatMap((l, i) => (l.kind === "same" ? [] : [i]));
  const out: Hunk[] = [];
  let i = 0;
  while (i < changed.length) {
    let first = changed[i];
    let last = first;
    while (i + 1 < changed.length && changed[i + 1] - last <= context * 2 + 1) last = changed[++i];
    i++;
    first = Math.max(0, first - context);
    last = Math.min(diff.length - 1, last + context);
    const part = diff.slice(first, last + 1);
    const fromLine = lineIn(diff, first, "a");
    const toLine = lineIn(diff, first, "b");
    out.push({
      fromLine,
      toLine,
      lines: part,
      added: part.filter((l) => l.kind === "added").length,
      removed: part.filter((l) => l.kind === "removed").length,
    });
  }
  return out;
}

/** The 1-based line number on side `side` where diff[index] sits (or would sit). */
function lineIn(diff: readonly Line[], index: number, side: "a" | "b"): number {
  for (let j = index; j < diff.length; j++) {
    const l = diff[j];
    if (side === "a" && l.kind !== "added") return l.a + 1;
    if (side === "b" && l.kind !== "removed") return l.b + 1;
  }
  return diff.filter((l) => (side === "a" ? l.kind !== "added" : l.kind !== "removed")).length + 1;
}

export type Summary = { readonly added: number; readonly removed: number; readonly places: number };
export function summarize(diff: readonly Line[], context = 3): Summary {
  return {
    added: diff.filter((l) => l.kind === "added").length,
    removed: diff.filter((l) => l.kind === "removed").length,
    places: hunks(diff, context).length,
  };
}

/** "2 lines added and 1 removed, in 2 places" */
export function describeSummary(s: Summary): string {
  if (!s.added && !s.removed) return "No lines changed";
  const n = (k: number, what: string) => `${k} ${k === 1 ? "line" : "lines"} ${what}`;
  const parts = [s.added && n(s.added, "added"), s.removed && (s.added ? `${s.removed} removed` : n(s.removed, "removed"))].filter(Boolean);
  return `${parts.join(" and ")}${s.places > 1 ? `, in ${s.places} places` : ""}`;
}

// ---- three-way merge ----------------------------------------------------------------------

/** base[from, to) replaced by `lines`. An insertion has from === to. */
export type Change = { readonly from: number; readonly to: number; readonly removed: readonly string[]; readonly lines: readonly string[] };

/** The changes of a diff, in base order. */
export function changes(diff: readonly Line[]): Change[] {
  const out: Change[] = [];
  let at = 0;
  let open: { from: number; to: number; removed: string[]; lines: string[] } | null = null;
  for (const l of diff) {
    if (l.kind === "same") {
      if (open) out.push(open);
      open = null;
      at = l.a + 1;
      continue;
    }
    open ??= { from: at, to: at, removed: [], lines: [] };
    if (l.kind === "removed") {
      open.removed.push(l.text);
      open.to = l.a + 1;
      at = l.a + 1;
    } else open.lines.push(l.text);
  }
  if (open) out.push(open);
  return out;
}

/** A change of the user's that touches a change on the server, so it was not applied. */
export type Conflict = { readonly mine: Change; readonly theirs: readonly Change[]; readonly line: number };
export type Merge = { readonly text: string; readonly applied: readonly Change[]; readonly conflicts: readonly Conflict[] };

const same = (x: Change, y: Change) =>
  x.from === y.from && x.to === y.to && x.lines.length === y.lines.length && x.lines.every((l, i) => l === y.lines[i]);
/** Overlapping or adjacent: applying both would need a human to decide the order or content. */
const touch = (x: Change, y: Change) => x.from <= y.to && y.from <= x.to;

/**
 * Applies the changes from `base` to `mine` onto `theirs` (a newer copy of `base`). A change
 * of mine that overlaps or adjoins a change of theirs is not applied: the result keeps the
 * server's lines there and reports the conflict, so nothing on the server is overwritten
 * without the user seeing it. A change both made identically is applied once.
 */
export function merge3(base: string, mine: string, theirs: string): Merge {
  const b = lines(base);
  const ours = changes(diffLines(b, lines(mine)));
  const server = changes(diffLines(b, lines(theirs)));
  const applied: Change[] = [];
  const conflicts: Conflict[] = [];
  const take: Change[] = [...server];
  for (const c of ours) {
    if (server.some((s) => same(s, c))) {
      applied.push(c);
      continue;
    }
    const hit = server.filter((s) => touch(s, c));
    if (hit.length) conflicts.push({ mine: c, theirs: hit, line: 0 });
    else {
      applied.push(c);
      take.push(c);
    }
  }
  take.sort((x, y) => x.from - y.from || x.to - y.to);
  const out: string[] = [];
  let at = 0;
  const lineOf = new Map<Change, number>();
  for (const c of take) {
    out.push(...b.slice(at, c.from));
    for (const s of server) if (s === c) lineOf.set(c, out.length + 1);
    out.push(...c.lines);
    at = c.to;
  }
  out.push(...b.slice(at));
  // The final line break: the server's, unless only the user changed it.
  const eol = endsLine(mine) !== endsLine(base) && endsLine(theirs) === endsLine(base) ? endsLine(mine) : endsLine(theirs);
  return {
    text: out.join("\n") + (eol && out.length ? "\n" : ""),
    applied,
    conflicts: conflicts.map((k) => ({ ...k, line: lineOf.get(k.theirs[0]) ?? 1 })),
  };
}
