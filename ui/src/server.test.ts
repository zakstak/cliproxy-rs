import { test } from "node:test";
import assert from "node:assert/strict";
import { Effect, Exit, Fiber, Result } from "effect";
import { client, configWrite, explain, type Transport } from "./server.ts";

class HttpError extends Error {
  status: number;
  code: string;
  constructor(message: string, status: number, code: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

/** An in-memory server: config values by path, with switches for failures. */
function memory(values: Record<string, unknown>) {
  const log: string[] = [];
  /** The file's version; every write moves it, as the server's ETag of config.yaml does. */
  const state = { down: 0, dropWrite: false, failWrite: false, version: 1, beforeWrite: null as null | (() => void) };
  const transport: Transport = async (path, req) => {
    log.push(`${req.method} ${path}${req.ifMatch ? ` if ${req.ifMatch}` : ""}`);
    if (state.down > 0) {
      state.down--;
      throw new Error("Cannot reach the server.");
    }
    const etag = `"v${state.version}"`;
    if (req.method === "PUT") {
      const race = state.beforeWrite;
      state.beforeWrite = null;
      race?.();
      if (req.ifMatch && req.ifMatch !== `"v${state.version}"`) throw new HttpError("precondition failed", 412, "precondition_failed");
      if (state.failWrite) throw new HttpError("failed to write: disk is full", 500, "write_failed");
      values[path] = structuredClone(req.body);
      state.version++;
      // The write lands, but the answer is lost on the way back.
      if (state.dropWrite) throw new Error("connection reset");
      return { body: {}, etag: null };
    }
    if (path === "/config.yaml") return { body: "", etag };
    if (!(path in values)) throw new HttpError("not found", 404, "not_found");
    return { body: structuredClone(values[path]), etag };
  };
  return { log, state, values, server: client(transport) };
}
const run = <A, E>(e: Effect.Effect<A, E>) => Effect.runPromise(Effect.result(e));

test("a read that gets no answer is asked again; a write is sent once", async () => {
  const m = memory({ "/config/routing/strategy": "fill-first" });
  m.state.down = 2;
  const r = await run(m.server.read("/config/routing/strategy"));
  assert.ok(Result.isSuccess(r) && r.success === "fill-first");
  assert.equal(m.log.length, 3);

  m.log.length = 0;
  m.state.down = 0;
  const w = configWrite({ path: "routing/strategy", before: "fill-first", next: "round-robin", what: "Changing the strategy", href: "#settings" });
  m.state.failWrite = true;
  const failed = await run(m.server.replace(w));
  assert.ok(Result.isFailure(failed) && failed.failure._tag === "Refused");
  assert.deepEqual(m.log, ["GET /config/routing/strategy", 'PUT /config/routing/strategy if "v1"']);
});

test("a retry writes the value captured at confirmation, and refuses after another change", async () => {
  const groups = [{ name: "Team gateway", keys: [{ "api-key": "sk-a" }, { "api-key": "sk-b" }] }];
  const m = memory({ "/config/api-keys/claude": structuredClone(groups) });
  const w = configWrite({ path: "api-keys/claude", before: groups, next: [{ ...groups[0], disabled: true }], what: "Disabling group Team gateway (Claude)", href: "#settings/keys" });
  // The screen moves on and its own data changes; the operation does not.
  groups[0].name = "Something else";
  groups.length = 0;
  assert.equal((w.before as { name: string }[])[0].name, "Team gateway");
  assert.throws(() => ((w.next as unknown[]).length = 0));

  m.state.failWrite = true;
  assert.ok(Result.isFailure(await run(m.server.replace(w))));
  m.state.failWrite = false;
  const ok = await run(m.server.replace(w));
  assert.ok(Result.isSuccess(ok) && ok.success === "written");
  assert.deepEqual(m.values["/config/api-keys/claude"], w.next);

  // Another writer changes the value: the same operation now refuses and writes nothing.
  m.values["/config/api-keys/claude"] = [{ name: "Renamed", keys: [] }];
  m.log.length = 0;
  const changed = await run(m.server.replace(configWrite({ ...w, before: w.before, next: [] })));
  assert.ok(Result.isFailure(changed) && changed.failure._tag === "Changed");
  assert.deepEqual(m.log, ["GET /config/api-keys/claude"]);
  assert.match(explain(changed.failure), /Nothing was written/);
});

test("an unanswered write that landed is reported as unknown, and its retry does not apply it twice", async () => {
  const m = memory({ "/config/access/api-keys": ["sk-1"] });
  const w = configWrite({ path: "access/api-keys", before: ["sk-1"], next: ["sk-1", "sk-2"], what: "Adding a client key", href: "#tools" });
  m.state.dropWrite = true;
  const first = await run(m.server.replace(w));
  assert.ok(Result.isFailure(first) && first.failure._tag === "Unanswered");
  m.state.dropWrite = false;
  m.log.length = 0;
  const again = await run(m.server.replace(w));
  assert.ok(Result.isSuccess(again) && again.success === "already");
  assert.deepEqual(m.log, ["GET /config/access/api-keys"]);
  assert.deepEqual(m.values["/config/access/api-keys"], ["sk-1", "sk-2"]);
});

test("an unset list compares as empty and is still written with the file's version", async () => {
  const m = memory({});
  const w = configWrite({ path: "oauth/excluded-models/codex", before: [], next: ["*-preview"], what: "Excluding *-preview", href: "#settings/models" });
  const r = await run(m.server.replace(w));
  assert.ok(Result.isSuccess(r) && r.success === "written");
  assert.deepEqual(m.log, ["GET /config/oauth/excluded-models/codex", "GET /config.yaml", "GET /config/oauth/excluded-models/codex", 'PUT /config/oauth/excluded-models/codex if "v1"']);
});

test("interrupting a read aborts the transport's signal", async () => {
  let started!: () => void;
  const begun = new Promise<void>((r) => (started = r));
  let seen: AbortSignal | undefined;
  const server = client((_path, req) => {
    seen = req.signal;
    started();
    return new Promise<never>((_, reject) => req.signal?.addEventListener("abort", () => reject(new Error("aborted"))));
  });
  const fiber = Effect.runFork(server.read("/credentials"));
  await begun;
  const exit = await Effect.runPromise(Fiber.interrupt(fiber).pipe(Effect.andThen(Fiber.await(fiber))));
  assert.equal(seen?.aborted, true);
  assert.ok(Exit.isFailure(exit));
});

test("a section write compares values, not layout, and returns the newer copy on a change", async () => {
  const m = memory({ "/config/routing": { strategy: "fill-first", "request-retry": 1 } });
  const base = JSON.stringify({ strategy: "fill-first", "request-retry": 1 }, null, 2);
  const same = await run(m.server.saveText({ path: "/config/routing", yaml: false, base, text: '{"strategy":"fill-first","request-retry":1}' }));
  assert.ok(Result.isSuccess(same) && same.success === "already");

  m.values["/config/routing"] = { strategy: "fill-first", "request-retry": 2 };
  const r = await run(m.server.saveText({ path: "/config/routing", yaml: false, base, text: '{"strategy":"round-robin","request-retry":1}' }));
  assert.ok(Result.isFailure(r) && r.failure._tag === "Changed");
  assert.equal(r.failure._tag === "Changed" && r.failure.latest, JSON.stringify({ strategy: "fill-first", "request-retry": 2 }, null, 2));

  const bad = await run(m.server.saveText({ path: "/config/routing", yaml: false, base, text: "{oops" }));
  assert.ok(Result.isFailure(bad) && bad.failure._tag === "Invalid");
});

test("a write races another writer: the server's version check refuses it and nothing is overwritten", async () => {
  const m = memory({ "/config/routing/strategy": "fill-first", "/config/routing/request-retry": 1 });
  const w = configWrite({ path: "routing/strategy", before: "fill-first", next: "round-robin", what: "Changing the strategy", href: "#settings" });
  // Between this tab's read and its write, someone else changes the same value.
  m.state.beforeWrite = () => {
    m.values["/config/routing/strategy"] = "sequential-fill";
    m.state.version++;
  };
  const r = await run(m.server.replace(w));
  assert.ok(Result.isFailure(r) && r.failure._tag === "Changed");
  assert.equal(m.values["/config/routing/strategy"], "sequential-fill");
  assert.deepEqual(m.log, ["GET /config/routing/strategy", 'PUT /config/routing/strategy if "v1"', "GET /config/routing/strategy"]);
});

test("a change elsewhere in the file between read and write is checked again, then written", async () => {
  const m = memory({ "/config/routing/strategy": "fill-first", "/config/routing/request-retry": 1 });
  m.state.beforeWrite = () => {
    m.values["/config/routing/request-retry"] = 2;
    m.state.version++;
  };
  const r = await run(m.server.replace(configWrite({ path: "routing/strategy", before: "fill-first", next: "round-robin", what: "Changing the strategy", href: "#settings" })));
  assert.ok(Result.isSuccess(r) && r.success === "written");
  assert.equal(m.values["/config/routing/strategy"], "round-robin");
  assert.equal(m.values["/config/routing/request-retry"], 2);
  assert.deepEqual(m.log.slice(-2), ["GET /config/routing/strategy", 'PUT /config/routing/strategy if "v2"']);
});

test("API key reads carry auth indexes; comparisons ignore them", async () => {
  const m = memory({ "/config/api-keys/claude": [{ name: "G", keys: [{ "api-key": "sk-a", auth_index: "0123456789abcdef" }] }] });
  const w = configWrite({ path: "api-keys/claude", before: [{ name: "G", keys: [{ "api-key": "sk-a" }] }], next: [], what: "Deleting group G (Claude)", href: "#settings/keys" });
  const r = await run(m.server.replace(w));
  assert.ok(Result.isSuccess(r) && r.success === "written");
});

test("payload auth_index values are written and concurrent changes are refused", async () => {
  const before = [{ params: { auth_index: "original" } }];
  const next = [{ params: { auth_index: "edited" } }];
  const path = "requests/payload/override";
  const m = memory({ [`/config/${path}`]: before });
  const w = configWrite({ path, before, next, what: "Editing a payload rule", href: "#settings/payload" });
  const r = await run(m.server.replace(w));
  assert.ok(Result.isSuccess(r) && r.success === "written");
  assert.deepEqual(m.values[`/config/${path}`], next);

  m.values[`/config/${path}`] = [{ params: { auth_index: "other writer" } }];
  m.log.length = 0;
  const changed = await run(m.server.replace(w));
  assert.ok(Result.isFailure(changed) && changed.failure._tag === "Changed");
  assert.deepEqual(m.log, [`GET /config/${path}`]);
});

test("API key metadata normalization preserves nested unknown fields", async () => {
  const before = [{ auth_index: "group-index", keys: [{ "api-key": "sk-a", "auth-index": "key-index", headers: { auth_index: "one" } }] }];
  const m = memory({ "/config/api-keys/claude": before });
  const next = [{ keys: [{ "api-key": "sk-a", headers: { auth_index: "two" } }] }];
  const r = await run(m.server.replace(configWrite({ path: "api-keys/claude", before, next, what: "Editing key headers", href: "#settings/keys" })));
  assert.ok(Result.isSuccess(r) && r.success === "written");
  assert.deepEqual(m.values["/config/api-keys/claude"], next);
});
