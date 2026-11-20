import { credName, label, mask, provider, type Data } from "./core.ts";

/**
 * A provider API key as the server lists it among credentials: account_type "api_key", the
 * key as `account`, its group's name as `label` (crates/cliproxy-internal/src/gateway/management/auth_files.rs).
 */
export const isApiKey = (a: Data) => a.account_type === "api_key" || (a.runtime_only && typeof a.account === "string");

/** The key, shortened as Provider keys shows it: "sk-…dd29". Never the whole key. */
export const keyHint = (a: Data) => (typeof a.account === "string" && a.account ? mask(a.account) : `ID ${String(a.auth_index || "").slice(0, 8)}`);
/** keyHint that never breaks across lines (word joiners follow its hyphen and ellipsis). */
const unbroken = (hint: string) => hint.replace("-", "-\u2060").replace("…", "…\u2060");

/**
 * How an account is named everywhere: filters, lanes, rows, details and confirmations.
 * API keys of one group share a label, so a key is its group and its shortened key, which is
 * also how Provider keys lists it.
 */
export function accountName(a: Data): string {
  return isApiKey(a) ? `${a.label || "API key"} · ${unbroken(keyHint(a))}` : credName(a);
}

/** The name with its provider, where accounts of several providers can share a name. */
export const accountWithProvider = (a: Data) => `${accountName(a)} (${label(provider(a))})`;

/** Provider keys, opened at this key. */
export const keyLink = (a: Data) => `#settings/keys?provider=${encodeURIComponent(provider(a))}&key=${encodeURIComponent(String(a.auth_index || ""))}`;

/** Developer-key endpoints, including the API-key profile in docs/codex/CONTRACT.md.
 * An explicit endpoint also
 * makes Codex and xAI keys active: their config sanitizer requires a nonempty base URL. */
export const providerEndpoint = (provider: string): string => ({
  claude: "https://api.anthropic.com",
  codex: "https://api.openai.com/v1",
  xai: "https://api.x.ai/v1",
  meta: "https://api.meta.ai/v1",
}[provider] || "");
