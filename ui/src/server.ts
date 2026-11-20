// The management API as Effect programs: each call has typed failures, reads can be
// interrupted (the transport receives the fiber's AbortSignal), and only reads are repeated
// automatically. A configuration write is an immutable value that holds its target, the
// value it was shown and the value it sets, so running it again after a failure, or after
// the screen changed, writes exactly what the user confirmed or refuses.
//
// Config writes are compare-and-swap. A config read carries an ETag (the SHA-256 of
// config.yaml), and a write with If-Match fails with 412 when the file changed since that
// read (crates/cliproxy-internal/src/gateway/management.rs `config_sync`, under the server's disk lock).
// A write rereads its value, compares it with what the user was shown and sends the ETag of
// that read; a 412 caused by a change elsewhere in the file repeats the read and comparison.
// Covered by server.test.ts with an in-memory transport.
import { Data, Effect, Schedule } from "effect";
import { equal, fieldPath } from "./core.ts";
import { comparable } from "./diff.ts";

export type Format = "json" | "text" | "blob";
export type Request = { method: string; body?: unknown; format: Format; signal?: AbortSignal; ifMatch?: string };
/** An answer's body and, for config reads, the version of config.yaml it came from. */
export type Answer = { body: unknown; etag: string | null };
/** Authenticated browser transport in production, an in-memory transport in unit tests. */
export type Transport = (path: string, req: Request) => Promise<Answer>;

/** No answer came back: the server is down, or the connection failed. */
export class Unreachable extends Data.TaggedError("Unreachable")<{ readonly message: string }> {}
/** A write was sent and no answer came back, so it may or may not have been applied. */
export class Unanswered extends Data.TaggedError("Unanswered")<{ readonly message: string }> {}
/** The server answered with an error status. */
export class Refused extends Data.TaggedError("Refused")<{ readonly status: number; readonly code: string; readonly message: string }> {}
/** The server copy no longer matches the one the user was shown. Nothing was written. */
export class Changed extends Data.TaggedError("Changed")<{ readonly path: string; readonly latest: unknown }> {}
/** The text cannot be written as it is. */
export class Invalid extends Data.TaggedError("Invalid")<{ readonly message: string }> {}

export type CallError = Unreachable | Refused;
export type WriteError = Unreachable | Unanswered | Refused | Changed | Invalid;

/** An error the server sent: ApiError in the browser, any Error with a numeric status in tests. */
function isRefusal(e: unknown): e is Error & { status: number; code?: unknown } {
  return e instanceof Error && "status" in e && typeof e.status === "number";
}

/** One write to one config path, frozen when the user confirms it. */
export type ConfigWrite = {
  readonly path: string;
  /** The value the user was shown. The write is refused unless the server still has it. */
  readonly before: unknown;
  readonly next: unknown;
  /** What the server's "not found" means for this path: an unset list is [], anything else `before`. */
  readonly absent: unknown;
  /** What the write does, for messages: "Disabling group Team gateway (Claude)". */
  readonly what: string;
  /** Where the write was made, so a message seen elsewhere can lead back. */
  readonly href: string;
};

const freeze = <T>(value: T): T => {
  if (value && typeof value === "object") {
    Object.values(value).forEach(freeze);
    Object.freeze(value);
  }
  return value;
};
const copy = <T>(value: T): T => (value === undefined ? value : freeze(structuredClone(value)));

export function configWrite(o: { path: string; before: unknown; next: unknown; what: string; href: string }): ConfigWrite {
  const before = copy(o.before);
  return freeze({ path: o.path, before, next: copy(o.next), absent: Array.isArray(before) ? freeze([]) : before, what: o.what, href: o.href });
}

/** A whole-file or whole-section text write: the reviewed text and the server copy it was based on. */
export type TextWrite = { readonly path: string; readonly yaml: boolean; readonly base: string; readonly text: string };

export type Applied = "written" | "already";

/** Match management/auth_index.rs::strip_update: metadata belongs only to groups
 * and their immediate keys. Arbitrary payloads and nested unknown fields stay intact. */
function withoutIndexes(path: string, value: unknown): unknown {
  const map = (v: unknown, change: (key: string, child: unknown) => unknown): unknown =>
    v && typeof v === "object" && !Array.isArray(v)
      ? Object.fromEntries(Object.entries(v).map(([key, child]) => [key, change(key, child)]))
      : v;
  const strip = (v: unknown): unknown =>
    v && typeof v === "object" && !Array.isArray(v)
      ? Object.fromEntries(Object.entries(v).filter(([key]) => key !== "auth_index" && key !== "auth-index"))
      : v;
  const group = (v: unknown): unknown => map(strip(v), (key, child) => key === "keys" && Array.isArray(child) ? child.map(strip) : child);
  const groups = (v: unknown): unknown => Array.isArray(v) ? v.map(group) : group(v);
  const providers = (v: unknown): unknown => map(v, (_key, child) => groups(child));
  if (!path) return map(value, (key, child) => key === "api-keys" ? providers(child) : child);
  if (path === "api-keys") return providers(value);
  return path.startsWith("api-keys/") ? groups(value) : value;
}

export function client(transport: Transport) {
  const answer = (path: string, req: Partial<Request> = {}): Effect.Effect<Answer, CallError> =>
    Effect.tryPromise({
      try: (signal) => transport(path, { method: req.method ?? "GET", body: req.body, format: req.format ?? "json", ifMatch: req.ifMatch, signal }),
      catch: (e) =>
        isRefusal(e)
          ? new Refused({ status: e.status, code: typeof e.code === "string" ? e.code : "", message: e.message })
          : new Unreachable({ message: e instanceof Error ? e.message : String(e) }),
    });

  const call = (path: string, req: Partial<Request> = {}) => answer(path, req).pipe(Effect.map((a) => a.body));

  /** A read is safe to repeat: a server that did not answer is asked twice more, with backoff. */
  const readAnswer = (path: string, format: Format = "json") =>
    answer(path, { format }).pipe(Effect.retry({ while: (e: CallError) => e._tag === "Unreachable", times: 2, schedule: Schedule.exponential("150 millis") }));
  const read = (path: string, format: Format = "json") => readAnswer(path, format).pipe(Effect.map((a) => a.body));

  /** A write is sent once. No answer means it may have landed, which the caller must hear. */
  const send = (path: string, method: string, body: unknown, ifMatch?: string) =>
    call(path, { method, body, ifMatch }).pipe(Effect.catchTag("Unreachable", (e) => Effect.fail(new Unanswered({ message: e.message }))));

  const notFound = (e: Refused) => e.status === 404 && e.code === "not_found";
  /**
   * A config value with the file version it was read at. An unset value answers 404 without
   * an ETag, so the file's version is read first and the value read again: if it is still
   * unset, a write with that version fails should anything change in between.
   */
  const versioned = (url: string, format: Format = "json"): Effect.Effect<{ found: boolean; body: unknown; etag: string | null }, CallError> =>
    readAnswer(url, format).pipe(
      Effect.map((a) => ({ found: true, ...a })),
      Effect.catchTag("Refused", (e) =>
        notFound(e)
          ? readAnswer("/config.yaml", "text").pipe(
              Effect.flatMap((file) =>
                readAnswer(url, format).pipe(
                  Effect.map((a) => ({ found: true, ...a })),
                  Effect.catchTag("Refused", (again) => (notFound(again) ? Effect.succeed({ found: false, body: undefined, etag: file.etag }) : Effect.fail(again))),
                ),
              ),
            )
          : Effect.fail(e),
      ),
    );
  /** A 412 means config.yaml changed between this read and the write: read and compare again. */
  const swap = <A>(attempt: Effect.Effect<A, WriteError>, path: string): Effect.Effect<A, WriteError> =>
    Effect.retry(attempt, { while: (e: WriteError) => e._tag === "Refused" && e.status === 412, times: 2 }).pipe(
      // Still changing after three tries: report it as a change, with nothing written.
      Effect.catchTag("Refused", (e): Effect.Effect<never, Refused | Changed> => (e.status === 412 ? Effect.fail(new Changed({ path, latest: undefined })) : Effect.fail(e))),
    );

  /**
   * Reread, compare, write with that read's version. When the server already holds the new
   * value (a retry after an unanswered write that did land, or another writer setting the
   * same value), nothing is sent.
   */
  const replace = (w: ConfigWrite): Effect.Effect<Applied, WriteError> =>
    swap(
      Effect.gen(function* () {
        const url = fieldPath(w.path);
        const read = yield* versioned(url);
        // The server adds each API key's auth_index to reads and drops it from writes.
        const latest = withoutIndexes(w.path, read.found ? read.body : w.absent);
        if (equal(latest, withoutIndexes(w.path, w.next))) return "already" as const;
        if (!equal(latest, withoutIndexes(w.path, w.before))) return yield* new Changed({ path: w.path, latest: read.body });
        yield* send(url, "PUT", w.next, read.etag ?? undefined);
        return "written" as const;
      }),
      w.path,
    );

  const copyAnswer = (path: string, yaml: boolean): Effect.Effect<{ text: string; etag: string | null }, CallError> =>
    yaml
      ? readAnswer(path, "text").pipe(Effect.map((a) => ({ text: String(a.body), etag: a.etag })))
      : versioned(path).pipe(Effect.map((a) => ({ text: a.found ? JSON.stringify(a.body, null, 2) : "{}", etag: a.etag })));
  /** The server copy of the file (YAML) or of one section (indented JSON; "{}" when unset). */
  const copyOf = (path: string, yaml: boolean) => copyAnswer(path, yaml).pipe(Effect.map((c) => c.text));

  /** Writes reviewed text if the server copy is still the one it was based on. */
  const saveText = (w: TextWrite): Effect.Effect<Applied, WriteError> =>
    swap(
      Effect.gen(function* () {
        const body = w.yaml ? w.text : yield* Effect.try({ try: (): unknown => JSON.parse(w.text), catch: (e) => new Invalid({ message: e instanceof Error ? e.message : String(e) }) });
        const latest = yield* copyAnswer(w.path, w.yaml);
        const json = !w.yaml;
        if (comparable(latest.text, json) === comparable(w.text, json)) return "already" as const;
        if (comparable(latest.text, json) !== comparable(w.base, json)) return yield* new Changed({ path: w.path, latest: latest.text });
        yield* send(w.path, "PUT", body, latest.etag ?? undefined);
        return "written" as const;
      }),
      w.path,
    );

  return { call, read, send, replace, copyOf, saveText };
}
export type Server = ReturnType<typeof client>;

/** What went wrong, in words for the person who asked for it. */
export function explain(e: WriteError): string {
  switch (e._tag) {
    case "Unreachable":
      return `${e.message} Nothing was written.`;
    case "Unanswered":
      return "The server did not answer, so the change may or may not have been saved. Trying again checks the server first and does not apply it twice.";
    case "Refused":
      return `The server refused it (HTTP ${e.status}): ${e.message}.`;
    case "Changed":
      return "It changed on the server since this page showed it. Nothing was written; the page now shows the server's copy.";
    case "Invalid":
      return `It is not valid JSON, so it cannot be written: ${e.message}`;
    default: {
      const unknown: never = e;
      return String(unknown);
    }
  }
}
