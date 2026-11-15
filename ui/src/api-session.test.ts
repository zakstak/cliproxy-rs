import { test } from "node:test";
import assert from "node:assert/strict";
import { ApiError, ManagementSession } from "./api-session.ts";
const hooks = () => ({ unauthorized() {}, missing() {}, meta(_headers: Headers | null) {} });
const get = { method: "GET", format: "json" } as const;

test("transport preserves proxy prefix, credentials, ETag, JSON, YAML and multipart", async () => {
  const calls: { url: string; init: RequestInit }[] = [];
  const session = new ManagementSession("https://proxy.invalid/team", async (url, init = {}) => {
    calls.push({ url: String(url), init });
    return new Response('{"ok":true}', { headers: { ETag: '"revision-1"' } });
  });
  session.connect("management-test-only", hooks());
  assert.equal((await session.transport("/config", get)).etag, '"revision-1"');
  await session.transport("/config/routing/strategy", { method: "PUT", format: "json", body: "fill-first", ifMatch: '"revision-1"' });
  await session.transport("/config.yaml", { method: "PUT", format: "json", body: "port: 8320\n" });
  const form = new FormData(); form.append("file", new Blob(['{}']), "test.json");
  await session.transport("/credentials", { method: "POST", format: "json", body: form });
  assert.equal(calls[0].url, "https://proxy.invalid/team/v8/management/config");
  assert.equal(new Headers(calls[0].init.headers).get("Authorization"), "Bearer management-test-only");
  assert.equal(new Headers(calls[1].init.headers).get("If-Match"), '"revision-1"');
  assert.equal(calls[1].init.body, '"fill-first"');
  assert.equal(calls[2].init.body, "port: 8320\n");
  assert.equal(new Headers(calls[2].init.headers).get("Content-Type"), "application/yaml");
  assert.equal(calls[3].init.body, form);
  assert.equal(new Headers(calls[3].init.headers).has("Content-Type"), false);
  assert.ok(calls.every(c => c.init.redirect === "error" && c.init.cache === "no-store"));
  await assert.rejects(session.transport("/../../outside", get), /Invalid management API path/);
});

test("HTTP conflicts remain typed and a rejected key ends the current session", async () => {
  let status = 412, unauthorized = 0;
  const session = new ManagementSession("http://127.0.0.1", async () => new Response('{"code":"config_changed","message":"changed"}', { status }));
  session.connect("test-only", { ...hooks(), unauthorized() { unauthorized++; session.disconnect(); } });
  await assert.rejects(session.transport("/config", get), e => e instanceof ApiError && e.status === 412 && e.code === "config_changed");
  status = 401;
  await assert.rejects(session.transport("/config", get), e => e instanceof ApiError && e.status === 401);
  assert.equal(unauthorized, 1);
  await assert.rejects(session.transport("/config", get), /Sign in/);
});

test("reconnect aborts old requests and rejects late bodies without running new hooks", async () => {
  let finish: (response: Response) => void = () => { throw new Error("No request started"); };
  let signal: AbortSignal | null | undefined;
  const session = new ManagementSession("http://127.0.0.1", async (_url, init) => {
    signal = init?.signal;
    return new Promise(resolve => { finish = resolve; });
  });
  session.connect("old-test-only", hooks());
  const pending = session.transport("/config", get);
  let called = false;
  session.connect("new-test-only", { ...hooks(), meta() { called = true; }, unauthorized() { called = true; } });
  assert.equal(signal?.aborted, true);
  finish(new Response('{"message":"old failure"}', { status: 401 }));
  await assert.rejects(pending, /Connection changed/);
  assert.equal(called, false);
});

test("interrupting a superseded read does not mark the server offline", async () => {
  let offline = 0;
  const session = new ManagementSession("http://127.0.0.1", async (_url, init) => new Promise((_resolve, reject) => {
    init?.signal?.addEventListener("abort", () => reject(init.signal?.reason), { once: true });
  }));
  session.connect("test-only", { ...hooks(), meta(h) { if (!h) offline++; } });
  const cancel = new AbortController();
  const pending = session.transport("/credentials", { ...get, signal: cancel.signal });
  cancel.abort();
  await assert.rejects(pending);
  assert.equal(offline, 0);
});

test("native fetch is invoked without binding it to the session object", async () => {
  const session = new ManagementSession("http://127.0.0.1", async function(this: unknown) {
    assert.equal(this, undefined);
    return new Response("{}");
  });
  session.connect("test-only", hooks());
  await session.transport("/config", get);
});
