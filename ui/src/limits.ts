// Limit checks: a check asks
// the provider through the server (POST /requests/api-call with the account's own token), so
// it runs only on a button, one at a time, and at most once per account every 2 minutes.
import { api } from "./api";
import { app, message, type Reading } from "./app.svelte";
import { provider, quotaWindows, signalWindows, type Data, type Window } from "./core";
import { isApiKey } from "./account";

const usageURL: Record<string, string> = {
  claude: "https://api.anthropic.com/api/oauth/usage",
  codex: "https://chatgpt.com/backend-api/wham/usage",
};
export const WAIT = 120_000;
export const key = (a: Data): string => a.auth_index || a.name;
/** The provider has a usage endpoint the server can call for this account. */
export const hasSource = (a: Data) => !!usageURL[provider(a)] && !isApiKey(a);
export const canCheck = (a: Data) => !a.disabled && hasSource(a);

export type Limits = {
  windows: Window[];
  /** When the windows were read; 0 when unknown. */
  at: number;
  /** "check": read by a check in this tab. "seen": the server saw them in a response. */
  from: "check" | "seen";
  /** The last check failed; the windows are the last good reading. */
  error?: string;
};

/** The limits to show: the last good check, else what the server saw, with any check error. */
export function limits(a: Data): Limits | { error: string } | null {
  const reading: Reading | undefined = app.readings[key(a)];
  if (reading && "windows" in reading) return { windows: reading.windows, at: reading.at, from: "check", error: reading.error };
  const windows = signalWindows(provider(a), a.quota);
  if (windows.length) return { windows, at: Date.parse(a.quota?.observed_at) || 0, from: "seen", error: reading?.error };
  return reading ?? null;
}

const tried = new Map<string, number>();
let queue: Promise<unknown> = Promise.resolve();
const gap = (k: string, now: number) => Math.max(0, (tried.get(k) ?? -Infinity) + WAIT - now);
/** Milliseconds until this account may be checked again; a queued check counts as a full wait. */
export const wait = (a: Data, now = Date.now()) => (app.pending[key(a)] ? WAIT : gap(key(a), now));

export function check(a: Data) {
  const k = key(a);
  if (wait(a)) return queue;
  app.pending[k] = "Queued";
  return (queue = queue.then(async () => {
    try {
      // The account as it is when its turn comes: it must still exist, be checkable and be due.
      const now = (app.accounts.data || []).find((x) => key(x) === k);
      if (!now || !canCheck(now) || gap(k, Date.now())) return;
      tried.set(k, Date.now());
      app.pending[k] = "Checking…";
      await read(now);
    } finally {
      delete app.pending[k];
    }
  }));
}

async function read(a: Data) {
  const p = provider(a);
  const k = key(a);
  const generation = app.generation;
  try {
    const header: Data = { Authorization: "Bearer $TOKEN$", "Content-Type": "application/json" };
    if (p === "claude") Object.assign(header, { "anthropic-beta": "oauth-2025-04-20", "User-Agent": "claude-cli/2.1.280 (external, cli)" });
    if (p === "codex") {
      header["User-Agent"] = "codex-tui/0.149.1";
      const account = a.account_id || a.id_token?.chatgpt_account_id;
      if (account) header["Chatgpt-Account-Id"] = account;
    }
    const r = await api("/requests/api-call", "POST", { authIndex: a.auth_index, method: "GET", url: usageURL[p], header });
    if (r.status_code < 200 || r.status_code >= 300)
      throw Object.assign(
        new Error(
          p === "claude" && r.status_code === 403
            ? "Claude denied access to usage (HTTP 403). Setup tokens cannot read usage; sign in to this account from Accounts to check its limits."
            : r.status_code === 401 || r.status_code === 403
              ? `The provider rejected this account's token (HTTP ${r.status_code}). Sign in to the account again.`
              : `The provider answered HTTP ${r.status_code}. Try again later.`,
        ),
        { code: r.status_code },
      );
    const raw: Data = typeof r.body === "string" ? JSON.parse(r.body) : r.body;
    if (generation !== app.generation) return;
    app.readings[k] = { at: Date.now(), windows: quotaWindows(p, raw) };
  } catch (e) {
    if (generation !== app.generation) return;
    // A failed check keeps the last good windows and adds why it failed.
    app.readings[k] = { ...app.readings[k], error: message(e), code: (e as { code?: number }).code };
  }
}

/** One check per account that has a source and is due, one after another. */
export async function checkAll(list: Data[]) {
  for (const a of list) if (canCheck(a)) check(a);
  await queue;
}
