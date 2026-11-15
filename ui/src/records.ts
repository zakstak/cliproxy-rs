// Parsing at the boundary: the two things Logs reads are a text line and a usage record.
// Both come from the server as loose data and leave here as named types.

/** `[2026-11-15 14:02:11] [a1b2c3d4] [info ] message` (crates/cpa-server/src/logging.rs). */
export type LogLine = {
  n: number;
  raw: string;
  /** Milliseconds, or 0 when the line has no timestamp (a continuation line). */
  at: number;
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
  if (!m) return { n, raw, at: 0, id: "", level: "", message: raw, access: null, own: false };
  const level = levels.has(m[3]) ? (m[3] as LogLine["level"]) : "";
  const a = m[4].match(/^\s*(\d{3}) \|\s*(\S+) \|\s*(\S+) \| (\w+)\s+"?(.*?)"?$/);
  return {
    n,
    raw,
    at: Date.parse(m[1].replace(" ", "T")) || 0,
    id: /[^-]/.test(m[2]) ? m[2] : "",
    level,
    message: m[4],
    access: a ? { status: Number(a[1]), duration: a[2], client: a[3], method: a[4], path: a[5] } : null,
    own: m[4].includes("/management/"),
  };
}

/** One request the proxy sent upstream, from `GET /observability/usage/live`. */
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
  /** Input splits into uncached, cache read and cache write; output includes reasoning. */
  tokens: { input: number; uncached: number; cacheRead: number; cacheWrite: number; output: number; reasoning: number };
  /** Milliseconds from the start of the request, in order, for the stages the server timed. */
  stages: { label: string; ms: number }[];
  /** The record as the server sent it, without the client key. */
  raw: Record<string, unknown>;
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

export function parseRequest(record: unknown, n: number): Request | null {
  if (!isObject(record)) return null;
  const tokens = record.tokens;
  const input = part(record.token_breakdown, "input");
  const output = part(record.token_breakdown, "output");
  // The v2 breakdown separates cache reads from uncached input. Without it, the flat counters
  // are all there is: input is what the provider called input, cache reads are separate.
  const cacheRead = count(part(input, "cache_read_tokens")) || count(part(tokens, "cache_read_tokens"));
  const cacheWrite = count(part(input, "cache_write_tokens")) || count(part(tokens, "cache_creation_tokens"));
  const uncached = isObject(input) ? count(input.uncached_tokens) : count(part(tokens, "input_tokens"));
  const failed = record.failed === true;
  const at = Date.parse(text(record.timestamp));
  const { api_key: _key, ...raw } = record;
  return {
    n,
    at: Number.isNaN(at) ? Date.now() : at,
    id: text(record.request_id),
    failed,
    status: count(part(record.fail, "status_code")) || (failed ? 0 : 200),
    failure: failed ? { code: text(part(record.fail, "code")), body: text(part(record.fail, "body")), message: failureMessage(text(part(record.fail, "body"))) } : null,
    model: text(record.alias) || text(record.model) || "unknown",
    provider: text(record.provider) || "unknown",
    auth: text(record.auth_index),
    endpoint: text(record.endpoint),
    client: text(record.user_agent),
    stream: record.stream === true,
    effort: text(record.reasoning_effort),
    latency: count(record.latency_ms),
    ttft: count(record.ttft_ms),
    tokens: {
      input: isObject(input) ? count(input.total_tokens) : uncached + cacheRead + cacheWrite,
      uncached,
      cacheRead,
      cacheWrite,
      output: isObject(output) ? count(output.total_tokens) : count(part(tokens, "output_tokens")),
      reasoning: count(part(output, "reasoning_tokens")) || count(part(tokens, "reasoning_tokens")),
    },
    stages: stageNames.flatMap(([key, label]) => {
      const ms = part(record.timings, key);
      return typeof ms === "number" ? [{ label, ms }] : [];
    }),
    raw,
  };
}

/** One answer of the live feed. Anything else throws, so a wrong server is not an empty feed. */
export function parseLivePage(raw: unknown, next: () => number) {
  if (!isObject(raw) || typeof raw.cursor !== "string" || !/^[0-9a-f]{16}\.\d+$/.test(raw.cursor) || !Array.isArray(raw.records))
    throw new Error("Unexpected live feed format.");
  return {
    cursor: raw.cursor,
    gap: raw.gap === true,
    statistics: raw.usage_statistics_enabled !== false,
    requests: raw.records.flatMap((r) => parseRequest(r, next()) ?? []),
  };
}
