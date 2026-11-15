import { test } from "node:test";
import assert from "node:assert/strict";
import { changesServer, dropVersion, handled, isUnsaved, reconcile, startDraft } from "./draft.ts";
import { lines } from "./diff.ts";

const yaml = (extra: Record<number, string> = {}, add: string[] = []) =>
  [...Array.from({ length: 30 }, (_, i) => extra[i] ?? `k${i}: ${i}`), ...add].join("\n");

test("two conflicts keep the original draft and the partial reconciliation", () => {
  const server1 = yaml();
  let d = startDraft({ path: "/config.yaml", title: "config.yaml", yaml: true, server: server1 });
  // The user's original draft: two changes far apart.
  d = { ...d, text: yaml({ 3: "k3: original", 20: "k20: original" }) };
  const original = d.text;

  // Someone changes line 4 on the server: the change at line 3 touches it, the one at 20 does not.
  const server2 = yaml({ 4: "k4: server" });
  d = reconcile(d, server2, 1);
  assert.equal(d.base, server2);
  assert.equal(d.applied, 1);
  assert.equal(d.unapplied.length, 1);
  assert.deepEqual(d.unapplied[0].mine.lines, ["k3: original"]);
  assert.equal(lines(d.text)[20], "k20: original");
  assert.equal(lines(d.text)[4], "k4: server");
  assert.deepEqual(d.versions.map((v) => v.text), [original]);

  // The user partly reconciles by hand, then the server changes again.
  d = { ...d, text: d.text.replace("k3: 3", "k3: by hand") };
  const partial = d.text;
  const server3 = yaml({ 4: "k4: server" }, ["request-retry: 1"]);
  d = reconcile(d, server3, 2);

  // Both earlier texts are still there, in order: the first original is not replaced.
  assert.deepEqual(d.versions.map((v) => [v.n, v.text]), [[1, original], [2, partial]]);
  assert.deepEqual(d.versions.map((v) => v.base), [server1, server2]);
  assert.ok(d.text.includes("k3: by hand") && d.text.includes("k20: original") && d.text.includes("request-retry: 1"));
  // Version 2 collided with nothing, but version 1's change was never applied for the user;
  // it stays listed until they say it is handled (here they redid it by hand).
  assert.deepEqual(d.unapplied.map((u) => [u.version, u.mine.lines]), [[1, ["k3: original"]]]);
  assert.equal(handled(d, d.unapplied[0]).unapplied.length, 0);
});

test("a draft with kept versions stays unsaved even when its text matches the server", () => {
  let d = startDraft({ path: "/config/routing", title: "routing", yaml: false, server: '{\n  "strategy": "a"\n}' });
  d = { ...d, text: '{\n  "strategy": "b"\n}' };
  d = reconcile(d, '{\n  "strategy": "c"\n}', 1);
  // Both changed the same line: the server's value stays and the user's is listed.
  assert.equal(d.text, '{\n  "strategy": "c"\n}');
  assert.equal(changesServer(d), false);
  assert.equal(isUnsaved(d), true);
  assert.equal(isUnsaved(dropVersion(d, 1)), false);
});

test("a compact JSON draft changes the server only where a value differs", () => {
  const d = startDraft({ path: "/config/routing", title: "routing", yaml: false, server: '{\n  "strategy": "a",\n  "retry": 1\n}' });
  assert.equal(changesServer({ ...d, text: '{"strategy":"a","retry":1}' }), false);
  assert.equal(changesServer({ ...d, text: '{"strategy":"b","retry":1}' }), true);
});

test("a change that was never applied stays listed, with its version, through a later conflict", () => {
  let d = startDraft({ path: "/config.yaml", title: "config.yaml", yaml: true, server: yaml() });
  d = { ...d, text: yaml({ 3: "k3: mine", 20: "k20: mine" }) };
  // The server changes line 3 too: k3 collides, k20 is applied.
  d = reconcile(d, yaml({ 3: "k3: server" }), 1);
  assert.deepEqual(d.unapplied.map((u) => [u.version, u.mine.lines]), [[1, ["k3: mine"]]]);

  // A second edit collides again, on another line.
  d = { ...d, text: d.text.replace("k25: 25", "k25: mine") };
  d = reconcile(d, yaml({ 3: "k3: server", 25: "k25: server" }), 2);
  assert.equal(d.applied, 1, "version 2's other change (k20) is applied");
  assert.deepEqual(
    d.unapplied.map((u) => [u.version, u.mine.lines]),
    [
      [1, ["k3: mine"]],
      [2, ["k25: mine"]],
    ],
    "version 1's change is still reported as not applied",
  );
  assert.ok(!d.text.includes("k3: mine"));

  // Saying one is handled removes only that one; removing a version removes only its own.
  assert.deepEqual(handled(d, d.unapplied[1]).unapplied.map((u) => u.version), [1]);
  const without1 = dropVersion(d, 1);
  assert.deepEqual(without1.unapplied.map((u) => u.version), [2]);
  assert.deepEqual(without1.versions.map((v) => v.n), [2]);
});


test("handling or removing a version preserves reconciliation provenance and never reuses its number", () => {
  let d = startDraft({ path: "/config.yaml", title: "config.yaml", yaml: true, server: yaml() });
  d = { ...d, text: yaml({ 3: "k3: unwanted", 20: "k20: mine" }) };
  d = reconcile(d, yaml({ 3: "k3: server" }), 1);
  d = { ...d, text: d.text.replace("k25: 25", "k25: mine") };
  d = reconcile(d, yaml({ 3: "k3: server", 25: "k25: server" }), 2);
  const applied = d.applied;
  d = handled(d, d.unapplied[0]);
  d = dropVersion(d, 2);
  assert.equal(d.appliedVersion, 2, "the count still belongs to version 2, not retained version 1");
  assert.equal(d.applied, applied);
  assert.equal(d.unapplied.length, 0, "handled can mean unwanted, not applied");
  assert.ok(!d.text.includes("k3: unwanted"));
  assert.ok(d.versions[0].text.includes("k3: unwanted"));
  d = reconcile(d, d.base + "\nnew: value", 3);
  assert.equal(d.appliedVersion, 3);
  assert.deepEqual(d.versions.map(v => v.n), [1, 3]);
});
