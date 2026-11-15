import type { Data } from "./core";
import type { Format } from "./server";
import { ApiError, ManagementSession, type Hooks } from "./api-session";
export { ApiError } from "./api-session";

// /management.html and reverse-proxy prefixes resolve to the same API neighbour.
export const base = new URL(".", location.href).href.replace(/\/$/, "");
const session = new ManagementSession(base);
export const connect = (secret: string, hooks: Hooks) => session.connect(secret, hooks);
export const disconnect = () => session.disconnect();
export const endpoint = () => session.endpoint;
export const transport = session.transport;
export const route = (path: string) => path.split("?")[0];

export function api(path: string, method?: string, body?: unknown, format?: "json"): Promise<Data>;
export function api(path: string, method: string, body: unknown, format: "text"): Promise<string>;
export function api(path: string, method: string, body: unknown, format: "blob"): Promise<Blob>;
export async function api(path: string, method = "GET", body?: unknown, format: Format = "json"): Promise<unknown> {
  return (await transport(path, { method, body, format })).body;
}
export const missing = (e: unknown) => e instanceof ApiError && e.missing;
export const text = (e: unknown) => e instanceof Error ? e.message : String(e);

/** This proxy's public model list, authenticated with a client key, never a management key.
 * Redirects are refused so client credentials cannot follow a redirect to another host. */
export async function clientModels(clientKey: string): Promise<string[]> {
  const r = await fetch(`${base}/v1/models`, { headers: { Authorization: `Bearer ${clientKey}` }, cache: "no-store", redirect: "error", signal: AbortSignal.timeout(30_000) });
  if (r.status === 401) throw new Error("The proxy rejected this key. Check that it is listed under Client keys.");
  if (!r.ok) throw new Error(`The proxy answered HTTP ${r.status}. Check Activity for the reason.`);
  const body: unknown = await r.json();
  if (!body || typeof body !== "object" || !("data" in body) || !Array.isArray(body.data)) throw new Error("The server returned an unexpected model list.");
  return body.data.flatMap((m: unknown) => m && typeof m === "object" && "id" in m && typeof m.id === "string" ? [m.id] : []);
}
export function download(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob), a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** Actionable sign-in errors, including a disabled management API. */
export function signInHelp(e: unknown): string {
  if (!(e instanceof ApiError)) return text(e);
  if (e.status === 404 || /key not set/.test(e.message)) return "The management API is off on this server. Set management.secret-key in config.yaml (or start it with MANAGEMENT_PASSWORD), then restart it.";
  if (/remote management disabled/.test(e.message)) return "This server accepts the dashboard only from its own machine. Open it there, or set management.allow-remote: true in config.yaml with a strong key.";
  if (/banned/i.test(e.message)) return `${e.message}. Too many wrong keys came from this address.`;
  if (e.status === 401) return "That key is not right. Use the plain text you set as management.secret-key: the server replaces it in config.yaml with a hash on first start.";
  return e.message;
}
