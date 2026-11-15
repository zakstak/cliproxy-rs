import { test } from "node:test";
import assert from "node:assert/strict";
import { comparable, describeSummary, diffLines, hunks, lines, merge3, summarize } from "./diff.ts";

const file = Array.from({ length: 40 }, (_, i) => `key${i}: ${i}`);
const text = (l: string[]) => l.join("\n");

test("two separate edits count only their own lines, in two places", () => {
  const after = [...file];
  after[3] = "key3: changed";
  after.splice(30, 0, "added: true");
  const d = diffLines(file, after);
  assert.deepEqual(summarize(d), { added: 2, removed: 1, places: 2 });
  assert.equal(describeSummary(summarize(d)), "2 lines added and 1 removed, in 2 places");
  // Every unchanged line is still "same", in order: the script rebuilds both sides.
  assert.deepEqual(d.filter((l) => l.kind !== "added").map((l) => l.text), file);
  assert.deepEqual(d.filter((l) => l.kind !== "removed").map((l) => l.text), after);
});

test("hunks show the changed lines with context and their line numbers", () => {
  const after = [...file];
  after[20] = "key20: x";
  const [h, ...rest] = hunks(diffLines(file, after));
  assert.equal(rest.length, 0);
  assert.equal(h.fromLine, 18);
  assert.equal(h.lines.length, 8);
  assert.deepEqual(h.lines.filter((l) => l.kind !== "same").map((l) => `${l.kind} ${l.text}`), ["removed key20: 20", "added key20: x"]);
});

test("a shortest script for interleaved edits", () => {
  const d = diffLines(lines("a\nb\nc\nd\ne"), lines("a\nc\nd\nx\ne"));
  assert.deepEqual(summarize(d), { added: 1, removed: 1, places: 1 });
  assert.deepEqual(d.map((l) => l.kind[0] + l.text), ["sa", "rb", "sc", "sd", "ax", "se"]);
});

test("compact JSON with the same values is not a difference", () => {
  const server = JSON.stringify({ strategy: "round-robin", "request-retry": 2 }, null, 2);
  const typed = '{"request-retry":2,"strategy":"round-robin"}';
  // Key order is part of JSON text but not of the values; the display uses the parsed draft's order.
  assert.equal(comparable(typed, true), JSON.stringify({ "request-retry": 2, strategy: "round-robin" }, null, 2));
  const same = '{ "strategy": "round-robin",  "request-retry": 2 }';
  assert.deepEqual(summarize(diffLines(lines(comparable(server, true)), lines(comparable(same, true)))), { added: 0, removed: 0, places: 0 });
  // Not JSON: compared as typed.
  assert.equal(comparable("a: 1\r\nb: 2", true), "a: 1\nb: 2");
});

test("merge applies my change on top of a separate server change", () => {
  const mine = [...file];
  mine[5] = "key5: mine";
  const theirs = [...file];
  theirs.splice(35, 0, "request-retry: 1");
  const m = merge3(text(file), text(mine), text(theirs));
  assert.equal(m.conflicts.length, 0);
  assert.equal(m.applied.length, 1);
  const out = lines(m.text);
  assert.equal(out[5], "key5: mine");
  assert.ok(out.includes("request-retry: 1"));
  assert.equal(out.length, file.length + 1);
});

test("merge keeps the server's lines and reports a change that touches theirs", () => {
  const mine = [...file];
  mine[10] = "key10: mine";
  mine[30] = "key30: mine";
  const theirs = [...file];
  theirs[11] = "key11: theirs";
  const m = merge3(text(file), text(mine), text(theirs));
  assert.equal(m.conflicts.length, 1);
  assert.deepEqual(m.conflicts[0].mine.lines, ["key10: mine"]);
  assert.deepEqual(m.conflicts[0].mine.removed, ["key10: 10"]);
  assert.equal(m.conflicts[0].line, 12);
  const out = lines(m.text);
  assert.equal(out[10], "key10: 10", "the server's copy of the contested place is kept");
  assert.equal(out[11], "key11: theirs");
  assert.equal(out[30], "key30: mine", "the separate change is applied");
});

test("the same change on both sides is applied once", () => {
  const both = [...file];
  both[2] = "key2: both";
  const m = merge3(text(file), text(both), text(both));
  assert.equal(m.conflicts.length, 0);
  assert.equal(m.text, text(both));
});

test("any two texts: the script rebuilds both sides and keeps a longest common subsequence", () => {
  let seed = 7;
  const rnd = () => ((seed = (seed * 1103515245 + 12345) % 2 ** 31), seed / 2 ** 31);
  for (let round = 0; round < 300; round++) {
    const pick = () => Array.from({ length: Math.floor(rnd() * 12) }, () => "abcd"[Math.floor(rnd() * 4)]);
    const a = pick();
    const b = pick();
    const d = diffLines(a, b);
    assert.deepEqual(d.filter((l) => l.kind !== "added").map((l) => l.text), a);
    assert.deepEqual(d.filter((l) => l.kind !== "removed").map((l) => l.text), b);
    // A shortest script keeps a longest common subsequence: compare with the DP length.
    const lcs = Array.from({ length: a.length + 1 }, () => new Array<number>(b.length + 1).fill(0));
    for (let i = a.length - 1; i >= 0; i--)
      for (let j = b.length - 1; j >= 0; j--) lcs[i][j] = a[i] === b[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    assert.equal(d.filter((l) => l.kind === "same").length, lcs[0][0]);
  }
});

test("a line appended after the final line break is one added line", () => {
  const base = "a: 1\nb: 2\n";
  const d = diffLines(lines(base), lines("a: 1\nb: 2\n# appended\n"));
  assert.deepEqual(summarize(d), { added: 1, removed: 0, places: 1 });
  assert.deepEqual(d.filter((l) => l.kind !== "same").map((l) => `${l.kind} ${l.text}`), ["added # appended"]);
  // Without a final line break the append is still one line, not a changed blank one.
  assert.deepEqual(summarize(diffLines(lines(base), lines("a: 1\nb: 2\n# appended"))), { added: 1, removed: 0, places: 1 });
  assert.deepEqual(lines("a\n"), ["a"]);
  assert.deepEqual(lines("a\n\n"), ["a", ""]);
  assert.deepEqual(lines(""), []);
});

test("a merge keeps the final line break, and an append that collides is an insertion", () => {
  const base = text(file) + "\n";
  const theirs = text([...file, "request-retry: 1"]) + "\n";
  const mine = text([...file.slice(0, 5), "key5: mine", ...file.slice(6), "# mine"]) + "\n";
  const m = merge3(base, mine, theirs);
  assert.ok(m.text.endsWith("request-retry: 1\n"), "the server's line and the final line break are kept");
  assert.equal(lines(m.text)[5], "key5: mine");
  assert.equal(m.conflicts.length, 1);
  assert.deepEqual(m.conflicts[0].mine.removed, [], "nothing of the server's copy is shown as removed");
  assert.deepEqual(m.conflicts[0].mine.lines, ["# mine"]);
  // Only the user removed the final line break: that change is kept.
  assert.ok(!merge3("a\nb\n", "a\nb", "a\nb\n").text.endsWith("\n"));
  assert.equal(merge3("a\nb\n", "a\nb\n", "a\nc\n").text, "a\nc\n");
});
