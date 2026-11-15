import { test } from "node:test";
import assert from "node:assert/strict";
import { blindSpans, median, perMinute, rateText, watchedMs } from "./observe.ts";

const MIN = 60_000;

test("a window before watching started is entirely unknown", () => {
  assert.deepEqual(blindSpans(0, 15 * MIN, { since: 0, gaps: [], stoppedAt: 0 }), [{ from: 0, to: 15 * MIN, why: "before" }]);
});

test("watching that started inside the window leaves the earlier part blind", () => {
  const blind = blindSpans(0, 15 * MIN, { since: 11 * MIN, gaps: [], stoppedAt: 0 });
  assert.deepEqual(blind, [{ from: 0, to: 11 * MIN, why: "before" }]);
  assert.equal(watchedMs(0, 15 * MIN, blind), 4 * MIN);
});

test("gaps and time after stopping are blind; overlapping spans are not counted twice", () => {
  const w = { since: 1 * MIN, gaps: [{ from: 5 * MIN, to: 6 * MIN }, { from: 5.5 * MIN, to: 7 * MIN }], stoppedAt: 12 * MIN };
  const blind = blindSpans(0, 15 * MIN, w);
  assert.deepEqual(
    blind.map((b) => b.why),
    ["before", "gap", "gap", "stopped"],
  );
  // Watched: 1–5 and 7–12 minutes.
  assert.equal(watchedMs(0, 15 * MIN, blind), 9 * MIN);
});

test("gaps outside the window are clipped away", () => {
  const blind = blindSpans(10 * MIN, 20 * MIN, { since: 1, gaps: [{ from: 0, to: 2 * MIN }], stoppedAt: 0 });
  assert.deepEqual(blind, []);
  assert.equal(watchedMs(10 * MIN, 20 * MIN, blind), 10 * MIN);
});

test("the rate divides by watched time, not by the window", () => {
  // 102 requests in 4 watched minutes of a 15-minute window is 25.5 a minute, not 6.8.
  assert.equal(perMinute(102, 4 * MIN), 25.5);
  assert.equal(rateText(102, 4 * MIN), "26 a minute");
  assert.equal(rateText(4, 2 * MIN), "2.0 a minute");
});

test("too little watched time gives no rate instead of a burst figure", () => {
  assert.equal(perMinute(4, 20_000), null);
  assert.equal(rateText(4, 20_000), "rate after 30 s of watching");
  assert.equal(rateText(0, 0), "nothing watched in this window");
});

test("median ignores missing values and is null without any", () => {
  assert.equal(median([0, 0]), null);
  assert.equal(median([300, 0, 100, 200]), 200);
});
