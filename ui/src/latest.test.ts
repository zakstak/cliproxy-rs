import { test } from "node:test";
import assert from "node:assert/strict";
import { Effect } from "effect";
import { LatestRead } from "./latest.ts";
import { client, type CallError, type Transport } from "./server.ts";
import { confirmingReread, describe, runBatch } from "./batch.ts";

type Account = { key: string; disabled: boolean };

/**
 * A server holding accounts. A list read takes its copy when the request arrives and answers
 * only when the test releases it, so the test decides the order in which reads finish. An
 * aborted read rejects, as fetch does.
 */
function accountsServer(accounts: Account[]) {
  const reads: { copy: Account[]; release: () => void; fail: () => void; aborted: boolean }[] = [];
  const transport: Transport = (path, req) =>
    new Promise((resolve, reject) => {
      assert.equal(path, "/credentials");
      const copy = structuredClone(accounts);
      const read = {
        copy,
        aborted: false,
        release: () => resolve({ body: { files: copy }, etag: null }),
        fail: () => reject(Object.assign(new Error("internal error"), { status: 500, code: "internal" })),
      };
      req.signal?.addEventListener("abort", () => {
        read.aborted = true;
        reject(new Error("aborted"));
      });
      reads.push(read);
    });
  return { accounts, reads, server: client(transport) };
}

/** What a screen holds: the latest stored list, as Res keeps it. */
function list(server: ReturnType<typeof client>) {
  const shown = { data: [] as Account[], stored: 0 };
  const read = new LatestRead<Account[], CallError>(
    server.read("/credentials").pipe(Effect.map((b) => (b as { files: Account[] }).files)),
    { success: (d) => ((shown.data = d), shown.stored++), failure: () => {}, settled: () => {} },
  );
  return { shown, read };
}

const settle = () => new Promise((r) => setTimeout(r, 0));
/** Runs `work` after everything already queued: a batch that does not wait for it judges first. */
const afterQueued = (work: () => void) => void setTimeout(work, 0);
/** Waits until the server has received `n` reads in total. */
async function received(reads: unknown[], n: number) {
  for (let i = 0; i < 50 && reads.length < n; i++) await settle();
  assert.equal(reads.length, n);
}

test("a read replaced by a newer one settles with the newer answer, never the older one", async () => {
  const s = accountsServer([{ key: "a", disabled: false }]);
  const { shown, read } = list(s.server);
  const first = read.start();
  await received(s.reads, 1);
  s.accounts[0].disabled = true;
  const second = read.start();
  await received(s.reads, 2);
  assert.ok(s.reads[0].aborted, "the older request is aborted");
  // The older request answering late is ignored.
  s.reads[0].release();
  s.reads[1].release();
  const [a, b] = await Promise.all([first, second]);
  assert.deepEqual(a, b);
  assert.ok(a.ok && a.data[0].disabled);
  assert.equal(shown.stored, 1);
  assert.equal(shown.data[0].disabled, true);
});

test("cancelling settles waiters as failed, with nothing stored", async () => {
  const s = accountsServer([{ key: "a", disabled: false }]);
  const { shown, read } = list(s.server);
  const pending = read.start();
  await received(s.reads, 1);
  read.cancel();
  const r = await pending;
  assert.ok(!r.ok && r.error._tag === "Stopped");
  s.reads[0].release();
  await settle();
  assert.equal(shown.stored, 0);
});

/**
 * The account page's batch: each unit is a PATCH, then a confirming reread. While each
 * confirming read is in flight a newer read of the same list starts (a Try again, a sign-in
 * finishing, or the refresh as it worked before it learned to join), replacing it.
 */
async function batchWithPoll(s: ReturnType<typeof accountsServer>, units: string[], applies: (key: string) => boolean, onPoll: (n: number) => void) {
  const { shown, read } = list(s.server);
  shown.data = structuredClone(s.accounts);
  let polls = 0;
  const confirm = confirmingReread(async () => {
    const before = s.reads.length;
    const mine = read.start();
    await received(s.reads, before + 1);
    // A newer read starts while the confirming read is still waiting.
    read.start();
    polls++;
    await received(s.reads, before + 2);
    onPoll(polls);
    return mine;
  });
  return runBatch(
    units,
    async (key) => {
      if (applies(key)) s.accounts.find((a) => a.key === key)!.disabled = true;
    },
    { reread: confirm, done: (key) => shown.data.find((a) => a.key === key)?.disabled === true },
  );
}

test("a newer read replacing a confirming read cannot make the batch report a false failure", async () => {
  const s = accountsServer([
    { key: "codex", disabled: false },
    { key: "xai", disabled: false },
  ]);
  const o = await batchWithPoll(s, ["codex", "xai"], () => true, () => {
    const [confirming, poll] = s.reads.slice(-2);
    assert.ok(confirming.aborted, "the confirming request was replaced");
    afterQueued(poll.release);
  });
  assert.deepEqual(o.ok, ["codex", "xai"]);
  assert.deepEqual(o.failed, []);
});

test("with newer reads overlapping, a change that did not stick is still the only failure", async () => {
  const s = accountsServer([
    { key: "codex", disabled: false },
    { key: "xai", disabled: false },
    { key: "meta", disabled: false },
  ]);
  const o = await batchWithPoll(s, ["codex", "xai", "meta"], (k) => k !== "xai", () => afterQueued(s.reads.at(-1)!.release));
  assert.deepEqual(o.ok, ["codex", "meta"]);
  assert.deepEqual(o.failed.map((f) => f.unit), ["xai"]);
  assert.equal(describe(o, (k) => `zak@example.invalid (${k})`, "disabled"), "2 accounts disabled. 1 failed: zak@example.invalid (xai) (the server did not report the change afterwards).");
});

test("when the read that replaced the confirming read fails, the batch stops instead of judging the old list", async () => {
  const s = accountsServer([
    { key: "codex", disabled: false },
    { key: "xai", disabled: false },
  ]);
  await assert.rejects(
    batchWithPoll(s, ["codex", "xai"], () => true, () => {
      // The read retries an unreachable server; a refusal fails at once.
      afterQueued(s.reads.at(-1)!.fail);
    }),
    /did not reload \(internal error\), so the result could not be confirmed/,
  );
});

test("a background refresh joins a running read instead of aborting it", async () => {
  const s = accountsServer([{ key: "a", disabled: false }]);
  const { shown, read } = list(s.server);
  const confirming = read.start();
  await received(s.reads, 1);
  const refreshed = read.refresh();
  await settle();
  assert.equal(s.reads.length, 1, "no second request");
  assert.ok(!s.reads[0].aborted);
  s.reads[0].release();
  assert.deepEqual(await confirming, await refreshed);
  assert.equal(shown.stored, 1);
  // With nothing running, a refresh reads.
  const next = read.refresh();
  await received(s.reads, 2);
  s.reads[1].release();
  assert.ok((await next).ok);
});
