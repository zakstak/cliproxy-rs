import { test } from "node:test";
import assert from "node:assert/strict";
import { describe, runBatch } from "./batch.ts";

test("one failing unit does not stop the others, and is named with its reason", async () => {
  const ran: string[] = [];
  const o = await runBatch(["a", "b", "c"], async (u) => {
    ran.push(u);
    if (u === "b") throw new Error("HTTP 500");
  });
  assert.deepEqual(ran, ["a", "b", "c"]);
  assert.deepEqual(o.ok, ["a", "c"]);
  assert.deepEqual(o.failed, [{ unit: "b", why: "HTTP 500" }]);
  assert.equal(describe(o, (u) => u, "disabled"), "2 accounts disabled. 1 failed: b (HTTP 500).");
});

test("with a readback, the server's state decides the outcome, not the answers", async () => {
  const state = new Map([["a", false], ["b", false], ["c", true]]);
  const o = await runBatch(
    ["a", "b", "c"],
    async (u) => {
      // "b" answers success but the change does not stick.
      if (u !== "b") state.set(u, true);
    },
    { done: (u) => state.get(u) === true, reread: async () => {} },
  );
  assert.deepEqual(o.ok, ["a", "c"]);
  assert.equal(o.failed[0].unit, "b");
  assert.match(o.failed[0].why, /did not report the change/);
});

test("units already done are skipped", async () => {
  const ran: string[] = [];
  await runBatch(["a", "b"], async (u) => void ran.push(u), { done: (u) => u === "a" });
  assert.deepEqual(ran, ["b"]);
});

test("a reread that fails stops the batch with its error", async () => {
  await assert.rejects(
    runBatch(["a", "b"], async () => {}, {
      reread: async () => {
        throw new Error("list did not reload");
      },
    }),
    /list did not reload/,
  );
});
