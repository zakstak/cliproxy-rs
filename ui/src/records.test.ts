import { test } from "node:test";
import assert from "node:assert/strict";
import { parseEvent, parseHistoryPage, parseLine, parseRequest, requestTotals, retainRequests, sumByKey } from "./records.ts";
import { blindSpans, perMinute, watchedMs } from "./observe.ts";
import { providerEndpoint } from "./account.ts";

function request(completed: number, n: number, extra: Record<string, unknown> = {}) {
  const r = parseRequest({ id: n + 1, at: completed, latency_ms: 100, ...extra }, n);
  assert.ok(r);
  return r;
}

test("local capacity eviction marks the lost interval unknown and fixes the rate denominator", () => {
  const requests = Array.from({ length: 6000 }, (_, n) => request((n + 1) * 600, n));
  const retained = retainRequests(requests, { cutoff: 0, retainedAfter: 0 });
  assert.equal(retained.requests.length, 2000);
  assert.equal(retained.retainedAfter, 2_400_000);
  const blind = blindSpans(1, 3_600_000, { since: 1, retainedAfter: retained.retainedAfter, gaps: [], stoppedAt: 0 });
  assert.deepEqual(blind, [{ from: 1, to: 2_400_000, why: "retained" }]);
  assert.equal(watchedMs(1, 3_600_000, blind), 1_200_000);
  assert.equal(perMinute(retained.requests.length, watchedMs(1, 3_600_000, blind)), 100);
});

test("eviction keeps no partial completion instant or older out-of-order records", () => {
  const retained = retainRequests([request(500, 1), request(400, 2), request(500, 3), request(600, 4)], { cutoff: 0, retainedAfter: 0, limit: 3 });
  assert.equal(retained.retainedAfter, 500);
  assert.deepEqual(retained.requests.map(r => r.n), [4]);
  const next = retainRequests([...retained.requests, request(450, 5), request(700, 6)], { cutoff: 0, retainedAfter: retained.retainedAfter });
  assert.equal(next.retainedAfter, 500);
  assert.deepEqual(next.requests.map(r => r.n), [4, 6]);
});

test("retention is by completion, including a long-running request that started before the hour", () => {
  const r = request(5000, 1, { latency_ms: 4900 });
  assert.deepEqual(retainRequests([r, request(2999, 2)], { cutoff: 3000, retainedAfter: 0 }).requests, [r]);
});

test("failed streams contribute reported tokens while timing remains success-only", () => {
  const failed = request(1000, 1, { failed: true, usage_reported: true, ttft_ms: 70, input_tokens: 12, output_tokens: 6, cache_read_tokens: 3, reasoning_tokens: 2 });
  const totals = requestTotals([failed]);
  assert.deepEqual(totals, { requests: 1, failed: 1, usageReports: 1, input: 12, cached: 3, output: 6, reasoning: 2, ttft: null, slowest: 0 });
  assert.equal(failed.tokens.uncached, 9, "input counts cache reads; uncached is the rest");
  const success = request(1100, 2, { usage_reported: true, ttft_ms: 20, input_tokens: 4, output_tokens: 2 });
  assert.deepEqual(requestTotals([failed, success]), { requests: 2, failed: 1, usageReports: 2, input: 16, cached: 3, output: 8, reasoning: 2, ttft: 20, slowest: 20 });
});

test("a row without reported usage counts no tokens", () => {
  const unknown = request(1000, 1, { failed: true, usage_reported: false, input_tokens: 7 });
  assert.equal(requestTotals([unknown]).usageReports, 0);
  assert.equal(requestTotals([unknown]).input, 0);
  assert.equal(request(1000, 2, { input_tokens: 1 }).usageReported, true);
});

test("history rows start at their finish minus latency, and pages come out oldest first", () => {
  let n = 0;
  const page = parseHistoryPage(
    { requests: [{ id: 9, at: 5000, latency_ms: 1000, request_id: "b", timings: { upstream_headers_ms: 40 } }, { id: 7, at: 4000, latency_ms: 0, request_id: "a" }, "junk"], more: true },
    () => ++n,
  );
  assert.deepEqual(page.requests.map((r) => [r.row, r.n, r.at, r.id]), [[7, 1, 4000, "a"], [9, 2, 4000, "b"]]);
  assert.deepEqual(page.requests[1].stages, [{ label: "Provider answered", ms: 40 }]);
  assert.equal(page.more, true);
  assert.throws(() => parseHistoryPage({ records: [] }, () => 0), /Unexpected history format/);
});

test("server log events keep their row and parse as log lines", () => {
  const e = parseEvent({ id: 3, at: 1, level: "warn", request_id: "", line: "[2026-11-15 14:02:11] [--------] [warn ] slow" }, 1);
  assert.equal(e?.row, 3);
  assert.equal(e?.level, "warn");
  assert.equal(e?.message, "slow");
  assert.equal(parseEvent({ line: "x" }, 1), null);
});

test("timezone-free server timestamps stay literal in browsers in different timezones", () => {
  const previous = process.env.TZ;
  try {
    for (const tz of ["America/New_York", "Asia/Tokyo", "UTC"]) {
      process.env.TZ = tz;
      const line = parseLine('[2026-11-15 14:02:11] [a1b2c3d4] [info ] 500 | 1ms | 127.0.0.1 | POST "/v1/messages"', 1);
      assert.equal(line.timestamp, "2026-11-15 14:02:11");
      assert.equal(line.access?.status, 500);
      assert.equal("at" in line, false);
    }
  } finally {
    if (previous === undefined) delete process.env.TZ;
    else process.env.TZ = previous;
  }
  assert.equal(parseLine("continuation", 2).timestamp, "");
});

test("developer Codex keys default to the OpenAI API profile", () => {
  assert.equal(providerEndpoint("codex"), "https://api.openai.com/v1");
});

test("grouped totals that straddle an epoch-aligned bucket merge into one row per key", () => {
  const row = (start: number, key: string, requests: number) => ({ start, key, requests, failed: 1, input_tokens: 10, output_tokens: 2, reasoning_tokens: 0, cache_read_tokens: 5, cache_creation_tokens: 0, total_tokens: 12 });
  const merged = sumByKey([row(0, "a", 2), row(0, "b", 1), row(86_400_000, "a", 3)]);
  assert.deepEqual(merged.map((r) => [r.key, r.requests, r.failed, r.input_tokens, r.cache_read_tokens]), [["a", 5, 2, 20, 10], ["b", 1, 1, 10, 5]]);
});
