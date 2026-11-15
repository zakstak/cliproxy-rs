import type { Answer, Request, Transport } from "./server.ts";

export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly method: string;
  readonly path: string;
  constructor(message: string, status: number, code: string, method: string, path: string) {
    super(message);
    this.status = status;
    this.code = code;
    this.method = method;
    this.path = path;
  }
  get missing() {
    return this.status === 501 || this.status === 405 || (this.status === 404 && !this.code);
  }
}
export type Hooks = {
  unauthorized(): void;
  missing(method: string, path: string): void;
  meta(headers: Headers | null): void;
};
const emptyHooks: Hooks = { unauthorized() {}, missing() {}, meta() {} };
const object = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);

/** A tab's authenticated session. Keys stay in memory; ending it aborts requests and
 * rejects late bodies, including responses that arrived before sign-out. */
export class ManagementSession {
  #key = "";
  #revision = 0;
  #lifetime = new AbortController();
  #hooks = emptyHooks;
  readonly endpoint: string;
  private readonly fetcher: typeof fetch;
  constructor(base: string, fetcher: typeof fetch = fetch) {
    this.fetcher = (input, init) => fetcher(input, init);
    this.endpoint = `${base.replace(/\/$/, "")}/v8/management`;
  }
  connect(secret: string, hooks: Hooks) {
    this.disconnect();
    this.#key = secret;
    this.#hooks = hooks;
  }
  disconnect() {
    this.#key = "";
    this.#revision++;
    this.#lifetime.abort();
    this.#lifetime = new AbortController();
    this.#hooks = emptyHooks;
  }
  readonly transport: Transport = async (path: string, request: Request): Promise<Answer> => {
    if (!this.#key) throw new Error("Sign in with your management key.");
    const url = new URL(this.endpoint + path);
    // Management credentials never leave this server or its management path prefix.
    if (!path.startsWith("/") || url.origin !== new URL(this.endpoint).origin || !url.pathname.startsWith(new URL(this.endpoint).pathname + "/"))
      throw new Error("Invalid management API path.");
    const at = this.#revision;
    const hooks = this.#hooks;
    const lifetime = this.#lifetime.signal;
    const signal = AbortSignal.any([lifetime, ...(request.signal ? [request.signal] : []), AbortSignal.timeout(path.includes("/refresh") ? 300_000 : 30_000)]);
    const headers = new Headers({ Authorization: `Bearer ${this.#key}` });
    if (request.ifMatch) headers.set("If-Match", request.ifMatch);
    const yaml = path.split("?")[0] === "/config.yaml";
    let body: BodyInit | undefined;
    if (request.body instanceof FormData) body = request.body;
    else if (request.body !== undefined) {
      headers.set("Content-Type", yaml ? "application/yaml" : "application/json");
      if (yaml) {
        if (typeof request.body !== "string") throw new Error("Configuration YAML must be text.");
        body = request.body;
      } else body = JSON.stringify(request.body);
    }
    try {
      const response = await this.fetcher(url, { method: request.method, headers, body, signal, cache: "no-store", redirect: "error" });
      const value: unknown = !response.ok
        ? await response.json().catch(() => ({}))
        : request.format === "blob" ? await response.blob()
        : request.format === "text" ? await response.text()
        : response.status === 204 ? {} : await response.json();
      if (at !== this.#revision) throw new Error("Connection changed. Try again.");
      signal.throwIfAborted();
      hooks.meta(response.headers);
      if (response.ok) return { body: value, etag: response.headers.get("ETag") };
      const details = object(value) ? value : {};
      const error = new ApiError(String(details.message || details.error || `HTTP ${response.status}`), response.status, String(details.code || details.error || ""), request.method, path.split("?")[0]);
      if (error.missing) {
        error.message = `Not available on this server (${error.method} ${error.path} → ${error.status}).`;
        hooks.missing(error.method, error.path);
      }
      if (response.status === 401) hooks.unauthorized();
      throw error;
    } catch (e) {
      if (e instanceof ApiError) throw e;
      if (at !== this.#revision) throw new Error("Connection changed. Try again.");
      // Interrupting a superseded read says nothing about the server's health.
      if (lifetime.aborted || request.signal?.aborted) throw e;
      hooks.meta(null);
      throw new Error(signal.reason instanceof DOMException && signal.reason.name === "TimeoutError"
        ? "The server did not answer in time."
        : "Cannot reach the server. Check that cliproxy is still running.");
    }
  };
}
