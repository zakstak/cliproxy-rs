// Parsing at the boundary: the two things Logs reads are a text line and a usage record.
// Both come from the server as loose data and leave here as named types.
import { median } from "./observe.ts";

/** `[2026-11-15 14:02:11] [a1b2c3d4] [info ] message` (crates/cliproxy-internal/src/gateway/logging.rs). */
export type LogLine = {
  n: number;
  raw: string;
  /** Server-local wall clock, without a reported timezone; never an absolute instant. */
  timestamp: string;
  /** The last 8 characters of the request ID; empty for lines outside a request. */
  id: string;
  level: "error" | "warn" | "info" | "debug" | "trace" | "";
  message: string;
  /** Set when the line is an access line: one finished HTTP request. */
  access: { status: number; duration: string; client: string; method: string; path: string } | null;
  /** The dashboard's own Management API call. */
  own: boolean;
};

const levels = new Set(["error", "warn", "info", "debug", "trace"]);

export function parseLine(raw: string, n: number): LogLine {
  const m = raw.match(/^\[(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d)\] \[([^\]]*)\] \[(\w+)\s*\] (?:\[[^\]]*:\d+\] )?(.*)$/s);
  if (!m) return { n, raw, timestamp: "", id: "", level: "", message: raw, access: null, own: false };
  const level = levels.has(m[3]) ? (m[3] as LogLine["level"]) : "";
  const a = m[4].match(/^\s*(\d{3}) \|\s*(\S+) \|\s*(\S+) \| (\w+)\s+"?(.*?)"?$/);
  return {
    n,
    raw,
    timestamp: m[1],
    id: /[^-]/.test(m[2]) ? m[2] : "",
    level,
    message: m[4],
    access: a ? { status: Number(a[1]), duration: a[2], client: a[3], method: a[4], path: a[5] } : null,
    own: m[4].includes("/management/"),
  };
}

/** One request the proxy sent upstream, from the history store (`GET /history/requests`). */
export type Request = {
  n: number;
  at: number;
  id: string;
  failed: boolean;
  status: number;
  /** `message` is the provider's own wording when the body is JSON that carries one. */
  failure: { code: string; body: string; message: string } | null;
  model: string;
  provider: string;
  /** The credential's auth index; Accounts and Usage join on it. */
  auth: string;
  endpoint: string;
  client: string;
  stream: boolean;
  effort: string;
  latency: number;
  ttft: number;
  usageReported: boolean;
  /** Input splits into uncached, cache read and cache write; output includes reasoning. */
  tokens: { input: number; uncached: number; cacheRead: number; cacheWrite: number; output: number; reasoning: number };
  /** Milliseconds from the start of the request, in order, for the stages the server timed. */
  stages: { label: string; ms: number }[];
  /** The row as the server stored it. It never holds a key. */
  raw: Record<string, unknown>;
  /** The history row id: the cursor for the next read. */
  row: number;
};

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const text = (v: unknown) => (typeof v === "string" ? v : "");
const count = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : 0);
const part = (v: unknown, key: string) => (isObject(v) ? v[key] : undefined);

const stageNames: [string, string][] = [
  ["client_body_received_ms", "Request received"],
  ["upstream_started_ms", "Upstream request started"],
  ["upstream_connected_ms", "Connected to provider"],
  ["upstream_body_sent_ms", "Request sent"],
  ["upstream_headers_ms", "Provider answered"],
  ["upstream_first_byte_ms", "First byte from provider"],
  ["client_first_byte_ms", "First byte to client"],
];

/** `error.message`, or `message`, from a JSON error body; empty when the body has neither. */
function failureMessage(body: string): string {
  try {
    const json: unknown = JSON.parse(body);
    return text(part(part(json, "error"), "message")) || text(part(json, "message"));
  } catch {
    return "";
  }
}

/** One history row (`crates/cliproxy-internal/src/persistence/history.rs`, `RequestEntry`).
 * `at` there is when the attempt finished; here it is when it started. Input counts every
 * input token, cache reads and writes included. */
export function parseRequest(row: unknown, n: number): Request | null {
  if (!isObject(row) || typeof row.id !== "number" || typeof row.at !== "number") return null;
  const failed = row.failed === true;
  const latency = count(row.latency_ms);
  const input = count(row.input_tokens);
  const cacheRead = count(row.cache_read_tokens);
  const cacheWrite = count(row.cache_creation_tokens);
  const body = text(row.fail_body);
  return {
    n,
    row: row.id,
    at: row.at - latency,
    id: text(row.request_id),
    failed,
    status: count(row.status) || (failed ? 0 : 200),
    failure: failed ? { code: "", body, message: failureMessage(body) } : null,
    model: text(row.model) || "unknown",
    provider: text(row.provider) || "unknown",
    auth: text(row.auth_index),
    endpoint: text(row.endpoint),
    client: text(row.user_agent),
    stream: row.stream === true,
    effort: text(row.reasoning_effort),
    latency,
    ttft: count(row.ttft_ms),
    usageReported: row.usage_reported !== false,
    tokens: {
      input,
      uncached: Math.max(0, input - cacheRead - cacheWrite),
      cacheRead,
      cacheWrite,
      output: count(row.output_tokens),
      reasoning: count(row.reasoning_tokens),
    },
    stages: stageNames.flatMap(([key, label]) => {
      const ms = part(row.timings, key);
      return typeof ms === "number" ? [{ label, ms }] : [];
    }),
    raw: row,
  };
}

/** Counters include reported usage from failed streams; timing describes successful requests. */
export function requestTotals(requests: Request[]) {
  const ok = requests.filter((r) => !r.failed);
  const reported = requests.filter((r) => r.usageReported);
  const sum = (pick: (r: Request) => number) => reported.reduce((n, r) => n + pick(r), 0);
  return {
    requests: requests.length, failed: requests.length - ok.length, usageReports: reported.length,
    input: sum((r) => r.tokens.input), cached: sum((r) => r.tokens.cacheRead),
    output: sum((r) => r.tokens.output), reasoning: sum((r) => r.tokens.reasoning),
    ttft: median(ok.map((r) => r.ttft)), slowest: Math.max(0, ...ok.map((r) => r.ttft)),
  };
}

/** Keep a complete suffix of completion times. Dropped records and timestamp ties make
 * everything through their latest completion unobserved, including out-of-order records. */
export function retainRequests(requests: Request[], options: { cutoff: number; retainedAfter: number; limit?: number }) {
  const { cutoff, retainedAfter, limit = 2000 } = options;
  const after = Math.max(cutoff, retainedAfter);
  const recent = requests.filter((r) => r.at + r.latency > after);
  const evicted = recent.slice(0, Math.max(0, recent.length - limit));
  const boundary = Math.max(after, ...evicted.map((r) => r.at + r.latency));
  return { retainedAfter: boundary, requests: recent.slice(-limit).filter((r) => r.at + r.latency > boundary) };
}

/** One page of `GET /history/requests`, oldest first. Anything else throws, so a wrong server is not an empty history. */
export function parseHistoryPage(raw: unknown, next: () => number) {
  if (!isObject(raw) || !Array.isArray(raw.requests) || typeof raw.more !== "boolean") throw new Error("Unexpected history format.");
  const requests = raw.requests.flatMap((r) => parseRequest(r, 0) ?? []).sort((a, b) => a.row - b.row);
  for (const r of requests) r.n = next();
  return { requests, more: raw.more };
}

/** One server log event from `GET /history/events`. */
export function parseEvent(raw: unknown, n: number): (LogLine & { row: number }) | null {
  if (!isObject(raw) || typeof raw.id !== "number" || typeof raw.line !== "string") return null;
  return { ...parseLine(raw.line, n), row: raw.id };
}
