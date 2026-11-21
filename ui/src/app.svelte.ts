// Shared state: the session, the route, the two reads every screen needs (config and
// accounts), the one live request watch that Logs and Usage both read, unsaved config
// drafts, and the one queue every configuration write goes through.
import { Effect, Result } from "effect";
import { LatestRead, type ReadResult } from "./latest";
import { api, ApiError, connect, disconnect, transport } from "./api";
import { reconcile, type Data, type Window } from "./core";
import { signInHelp } from "./api";
import { parseHistoryPage, retainRequests, type Request } from "./records";
import { client, explain, type Applied, type CallError, type ConfigWrite, type WriteError } from "./server";
import { configuredAccounts } from "./configured-accounts";
import { isUnsaved, type Draft } from "./draft";

export const server = client(transport);
/** Below this width a detail covers the page as a modal sheet instead of sitting beside the list. */
export const INLINE_FROM = 1100;

export const pages = [
  ["logs", "Activity", "Activity"],
  ["usage", "Usage", "Usage"],
  ["accounts", "Accounts", "Accounts"],
  ["tools", "Use with tools", "Tools"],
] as const;
type Page = (typeof pages)[number][0] | "settings";
/** Settings is one screen with sections; each section has its own address. */
export const settingsPages = [
  ["", "General"],
  ["keys", "Provider keys"],
  ["models", "Models"],
  ["payload", "Payload rules"],
  ["config", "Configuration"],
  ["system", "System"],
] as const;
export type SettingsPage = (typeof settingsPages)[number][0];
export type Route = { page: Page; rest: string[]; query: URLSearchParams };

function parseRoute(hash: string): Route {
  const [path, search = ""] = hash.replace(/^#/, "").split("?");
  const decode = (value: string) => { try { return decodeURIComponent(value); } catch { return value; } };
  let [page = "", ...rest] = path.split("/").map(decode);
  const aliases: Record<string, string> = { overview: "logs", use: "tools", keys: "tools", credentials: "accounts", quotas: "accounts", oauth: "accounts", connect: "accounts", providers: "settings/keys", configuration: "settings/config", config: "settings/config", models: "settings/models", payload: "settings/payload", system: "settings/system" };
  if (aliases[page]) [page, ...rest] = aliases[page].split("/");
  const known = page === "settings" || pages.some((p) => p[0] === page);
  return {
    page: known ? (page as Page) : "logs",
    rest: known ? rest : [],
    query: new URLSearchParams(search),
  };
}

export const message = (e: unknown) =>
  e instanceof Error ? e.message : String(e);

/**
 * One server read that keeps its last good data through a failed refresh. The latest read
 * always wins (latest.ts): starting one aborts the one still running, and `load()` settles
 * with a read that started at or after the call, never with the data that was already here.
 */
export class Res<T> {
  data = $state.raw<T>();
  error = $state.raw<CallError | null>(null);
  loading = $state(false);
  /** When `data` was last read successfully. */
  at = $state(0);
  #read: LatestRead<T, CallError>;
  constructor(read: Effect.Effect<T, CallError>) {
    this.#read = new LatestRead(read, {
      success: (data) => {
        this.data = data;
        this.error = null;
        this.at = Date.now();
      },
      failure: (e) => void (this.error = e),
      settled: () => void (this.loading = false),
    });
  }
  load(quiet = false): Promise<ReadResult<T, CallError>> {
    if (!quiet) this.loading = true;
    return this.#read.start();
  }
  /** A background refresh: joins a read already running rather than aborting it. */
  refresh(): Promise<ReadResult<T, CallError>> {
    return this.#read.refresh();
  }
  clear() {
    this.#read.cancel();
    this.data = undefined;
    this.error = null;
    this.loading = false;
    this.at = 0;
  }
}

const HISTORY = "/history/requests";
const HOUR = 60 * 60_000;
/** Rows one history read returns at most (the server's page limit). */
const PAGE = 1000;

/**
 * Requests from the server's history store. Starting reads the last hour back from the
 * store, so a new tab or a restarted server loses nothing the store kept; polling then
 * reads rows appended since, by row id. The tab keeps the last hour, at most 2,000
 * requests.
 */
class Watch {
  state = $state<"off" | "on">("off");
  /** Why the last read failed; empty after a good read. */
  problem = $state("");
  /** The start of what this tab holds: an hour before it first started watching; 0 before that. */
  since = $state(0);
  /** When watching started again after a stop; 0 before any restart. */
  resumedAt = $state(0);
  /** When watching last stopped; 0 while watching. */
  stoppedAt = $state(0);
  requests = $state.raw<Request[]>([]);
  /** Requests through this completion time are not loaded here and are unknown. */
  retainedAfter = $state(0);
  /** The store keeps every request, so a stop leaves no hole: restarting reads it back. */
  gaps = $state.raw<{ from: number; to: number }[]>([]);
  /** False when the server records no history (`storage.backend: none`, or the store failed to open). */
  recording = $state(true);
  #cursor = 0;
  #run = 0;
  #n = 0;
  #polling = false;

  /** A read the store could not answer because it is off says so instead of failing. */
  #off(e: unknown) {
    if (e instanceof ApiError && e.status === 503) {
      this.recording = false;
      this.problem = "";
      return true;
    }
    return false;
  }

  async start() {
    const mine = ++this.#run;
    try {
      const now = Date.now();
      const page = parseHistoryPage(await api(`${HISTORY}?since=${now - HOUR}&limit=${PAGE}`), () => ++this.#n);
      // An empty hour still needs a cursor: the newest row of all, if any.
      const cursor = page.requests.at(-1)?.row ?? parseHistoryPage(await api(`${HISTORY}?limit=1`), () => ++this.#n).requests.at(-1)?.row ?? 0;
      if (mine !== this.#run) return;
      if (this.since) this.resumedAt = now;
      this.since = now - HOUR;
      // More than a page in the hour: what is older than the page is unknown here.
      const oldest = page.requests[0];
      this.retainedAfter = page.more && oldest ? oldest.at + oldest.latency - 1 : 0;
      this.requests = retainRequests(page.requests, { cutoff: now - HOUR, retainedAfter: this.retainedAfter }).requests;
      this.stoppedAt = 0;
      this.#cursor = cursor;
      this.recording = true;
      this.problem = "";
      this.state = "on";
    } catch (e) {
      if (mine === this.#run && !this.#off(e)) this.problem = message(e);
    }
  }
  stop() {
    this.#run++;
    if (this.state === "on") this.stoppedAt = Date.now();
    this.state = "off";
  }
  async poll() {
    if (this.state !== "on" || this.#polling) return;
    this.#polling = true;
    const mine = this.#run;
    try {
      let more = true;
      // Reads until caught up; each page is at most PAGE rows.
      for (let pages = 0; more && pages < 5; pages++) {
        const page = parseHistoryPage(await api(`${HISTORY}?after=${this.#cursor}&limit=${PAGE}`), () => ++this.#n);
        if (mine !== this.#run) return;
        more = page.more;
        const last = page.requests.at(-1);
        if (!last) break;
        const now = Date.now();
        const retained = retainRequests([...this.requests, ...page.requests], { cutoff: now - HOUR, retainedAfter: this.retainedAfter });
        this.requests = retained.requests;
        this.retainedAfter = retained.retainedAfter;
        this.#cursor = last.row;
      }
      this.recording = true;
      this.problem = "";
    } catch (e) {
      if (mine === this.#run && !this.#off(e)) this.problem = message(e);
    } finally {
      this.#polling = false;
    }
  }
  clear() {
    this.stop();
    this.requests = [];
    this.retainedAfter = 0;
    this.gaps = [];
    this.since = 0;
    this.resumedAt = 0;
    this.stoppedAt = 0;
    this.#cursor = 0;
    this.problem = "";
  }
}
export const watch = new Watch();

export type Reading =
  | { at: number; windows: Window[]; error?: string; code?: number }
  | { error: string; code?: number };
/** Dark unless the user picks otherwise. "system" follows the operating system. */
type Theme = "dark" | "light" | "system";
export type Toast = {
  text: string;
  bad: boolean;
  /** "Try again" for a write that failed after its screen was left, "Show it" to go back there. */
  actions?: { label: string; run?: () => Promise<unknown>; href?: string }[];
};

const THEME_KEY = "cliproxy-theme";
const CHARTS_KEY = "cliproxy-activity-charts";
const remembered = (k: string) => {
  try {
    return sessionStorage.getItem(k);
  } catch {
    return null;
  }
};
const remember = (k: string, v: string) => {
  try {
    sessionStorage.setItem(k, v);
  } catch {}
};

class App {
  logged = $state(false);
  generation = 0;
  meta = $state({ version: "", commit: "", built: "" });
  setupStarted = $state(false);
  setupTestKey = $state("");
  /** The test just passed: the checklist stays, saying so, until the user moves on. */
  setupVerified = $state(false);
  /** False after a call could not reach the server; true again after the next answer. */
  online = $state(true);
  busy = $state(false);
  toast = $state<Toast | null>(null);
  route = $state.raw(parseRoute(location.hash));
  theme = $state<Theme>("dark");
  /** Wider than a phone. */
  wide = $state(matchMedia("(min-width: 761px)").matches);
  /** A short screen (under 760 px tall), where Activity lists its issues behind one line. */
  short = $state(matchMedia("(max-height: 759px)").matches);
  /** Wide enough for a detail to sit inline, without covering the page. */
  inline = $state(matchMedia(`(min-width: ${INLINE_FROM}px)`).matches);
  /** Activity's trends and timeline: drawn in full, or one compact line so requests start higher. Kept for this tab. */
  charts = $state<"compact" | "full">(remembered(CHARTS_KEY) === "full" ? "full" : "compact");
  /** Unsaved configuration edits, by path. */
  drafts = $state<Record<string, Draft>>({});
  /** Sign out was asked for while drafts are unsaved; the shell asks before discarding them. */
  confirmingLogout = $state(false);
  /** A clock for countdowns and ages, ticked while signed in. */
  now = $state(Date.now());
  config = new Res<Data>(server.read("/config").pipe(Effect.map(asData)));
  keyUsage = new Res<Data>(server.read("/observability/usage/api-keys").pipe(Effect.map(asData)));
  files: Res<Data[]> = new Res(
    server.read("/credentials").pipe(
      Effect.map((body) =>
        reconcile(
          this.files.data || [],
          (asData(body).files || []).filter(isData),
          (a) => `${a.name}\u0000${a.auth_index}`,
        ),
      ),
    ),
  );
  accounts = {
    get data(): Data[] | undefined {
      if (!app.files.data) return undefined;
      const files = app.files.data;
      const seen = new Set(files.map(a => a.auth_index));
      return [...files, ...configuredAccounts(app.config.data || {}, app.keyUsage.data || {}).filter(a => !seen.has(a.auth_index))];
    },
    get error() { return app.files.error || app.keyUsage.error; },
    get loading() { return app.files.loading || app.keyUsage.loading; },
    get at() { return app.files.at; },
    async load(quiet = false) {
      const [files] = await Promise.all([app.files.load(quiet), app.keyUsage.load(quiet)]);
      return files.ok ? { ok: true as const, data: app.accounts.data || [] } : files;
    },
    refresh() { return Promise.all([app.files.refresh(), app.keyUsage.refresh()]); },
    clear() { app.files.clear(); app.keyUsage.clear(); },
  };
  /** Limit checks made in this tab, by auth index. */
  readings = $state<Record<string, Reading>>({});
  /** Limit checks waiting or on their way, by auth index. */
  pending = $state<Record<string, "Queued" | "Checking…">>({});
  #timers: ReturnType<typeof setInterval>[] = [];
  #toastTimer: ReturnType<typeof setTimeout> | undefined;

  constructor() {
    matchMedia("(min-width: 761px)").addEventListener(
      "change",
      (e) => (this.wide = e.matches),
    );
    matchMedia("(max-height: 759px)").addEventListener("change", (e) => (this.short = e.matches));
    matchMedia(`(min-width: ${INLINE_FROM}px)`).addEventListener(
      "change",
      (e) => (this.inline = e.matches),
    );
    const asked = new URLSearchParams(location.search).get("theme");
    const pick = asked || (() => { try { return localStorage.getItem(THEME_KEY); } catch { return null; } })();
    if (pick === "light" || pick === "system" || pick === "dark")
      this.setTheme(pick, !asked);
  }

  async login(secret: string) {
    this.generation++;
    this.meta = { version: "", commit: "", built: "" };
    connect(secret, {
      unauthorized: () => {
        if (!this.logged) return;
        this.logout();
        this.notify("The management key was rejected. Sign in again.", true);
      },
      missing: () => {},
      meta: (h) => {
        this.online = !!h;
        if (!h) return;
        this.meta.version = h.get("x-cpa-version") || h.get("x-server-version") || this.meta.version;
        this.meta.commit = h.get("x-cpa-commit") || this.meta.commit;
        this.meta.built = h.get("x-cpa-build-date") || this.meta.built;
      },
    });
    try {
      this.config.data = asData(await api("/config"));
      this.config.at = Date.now();
    } catch (e) {
      disconnect();
      throw new Error(signInHelp(e));
    }
    this.logged = true;
    this.accounts.load();
    watch.start();
    const visible = (work: () => unknown) => () => document.hidden || work();
    this.#timers = [
      setInterval(
        visible(() => (this.now = Date.now())),
        1000,
      ),
      setInterval(
        visible(() => watch.poll()),
        3000,
      ),
      setInterval(
        visible(() => this.accounts.refresh()),
        10_000,
      ),
    ];
  }
  /** Drafts holding anything the server does not have: typed changes or kept earlier versions. */
  get unsaved(): Draft[] {
    return Object.values(this.drafts).filter(isUnsaved);
  }
  setCharts(mode: "compact" | "full") {
    this.charts = mode;
    remember(CHARTS_KEY, mode);
  }
  /** Sign out, asking first when an unsaved draft would be lost. */
  requestLogout() {
    if (this.unsaved.length) this.confirmingLogout = true;
    else this.logout();
  }
  /** Ends the session and every read it started. The key and any drafts are dropped. */
  logout() {
    this.generation++;
    this.setupStarted = false;
    this.setupTestKey = "";
    this.setupVerified = false;
    this.drafts = {};
    this.confirmingLogout = false;
    disconnect();
    this.#timers.forEach(clearInterval);
    this.#timers = [];
    watch.clear();
    this.config.clear();
    this.accounts.clear();
    this.readings = {};
    this.pending = {};
    this.toast = null;
    this.online = true;
    this.busy = false;
    this.logged = false;
  }

  go(hash: string) {
    if (location.hash.slice(1) !== hash) location.hash = hash;
  }
  /** Change the address without a history entry, for a one-shot link such as #accounts?add=1. */
  redirect(hash: string) {
    history.replaceState(null, "", `#${hash}`);
    this.sync();
  }
  /** How Accounts is filtered and sorted; kept here so leaving the screen and coming back keeps it. */
  accountView = $state({
    provider: "all",
    status: "all" as "all" | "ok" | "warn" | "bad" | "off" | "idle",
    q: "",
    sort: "attention" as "attention" | "reset" | "name" | "provider",
  });
  sync() {
    this.route = parseRoute(location.hash);
  }
  setTheme(theme: Theme, keep = true) {
    this.theme = theme;
    if (theme === "dark") delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = theme;
    if (keep) { try { localStorage.setItem(THEME_KEY, theme); } catch {} }
  }
  notify(text: string, bad = false, actions?: Toast["actions"]) {
    this.toast = { text, bad, actions };
    clearTimeout(this.#toastTimer);
    if (!bad) this.#toastTimer = setTimeout(() => (this.toast = null), 4000);
  }
  /** One write at a time. Returns the error text, or "" when it worked. */
  async act(work: () => Promise<unknown>, done = ""): Promise<string> {
    if (this.busy) return "Another change is still being saved.";
    const generation = this.generation;
    this.busy = true;
    try {
      await work();
      if (generation !== this.generation) return "";
      if (done) this.notify(done);
      return "";
    } catch (e) {
      return generation === this.generation ? message(e) : "";
    } finally {
      this.busy = false;
    }
  }
  /** `act`, with the failure shown as a toast that stays until dismissed. */
  async run(work: () => Promise<unknown>, done = "") {
    const error = await this.act(work, done);
    if (error) this.notify(error, true);
    return !error;
  }
  setting<T>(path: string, fallback: T): T {
    let v: unknown = this.config.data;
    for (const k of path.split("/")) v = v && typeof v === "object" ? (v as Data)[k] : undefined;
    return v === undefined || v === null ? fallback : (v as T);
  }

  #writes: Promise<unknown> = Promise.resolve();
  /**
   * Runs one configuration write after any write still in progress, then rereads the
   * configuration so the page shows the server's copy whatever happened. Returns the error,
   * or null when the server has the new value.
   */
  perform(work: Effect.Effect<Applied, WriteError>, done: string): Promise<WriteError | null> {
    const generation = this.generation;
    const run = async (): Promise<WriteError | null> => {
      if (generation !== this.generation) return null;
      this.busy = true;
      try {
        const r = await Effect.runPromise(Effect.result(work));
        if (generation !== this.generation) return null;
        await this.config.load(true);
        if (generation !== this.generation) return null;
        this.accounts.refresh();
        if (Result.isFailure(r)) return r.failure;
        this.notify(r.success === "already" ? `${done} The server already had it, so nothing more was written.` : done);
        return null;
      } finally {
        this.busy = false;
      }
    };
    const p = this.#writes.then(run);
    this.#writes = p.catch(() => {});
    return p;
  }
  /**
   * One configuration value replaced. The write is the frozen value the user confirmed:
   * retrying it after a failure writes the same thing, or refuses if the server copy changed.
   */
  write(op: ConfigWrite, done: string): Promise<WriteError | null> {
    return this.perform(server.replace(op), done);
  }
  /**
   * A write failed after the user left the screen it was made on: say so wherever they are,
   * naming what it was for, with a way back and, unless the server copy changed, a retry of
   * the same write. The screen they are on now is never given this error.
   */
  failedAway(op: ConfigWrite, error: WriteError, done: string, again = false) {
    const retry =
      error._tag === "Changed" || error._tag === "Invalid"
        ? []
        : [{ label: "Try again", run: () => this.write(op, done).then((e) => e && this.failedAway(op, e, done, true)) }];
    // "failed again" after a retry, so the alert reads out a second failure with the same reason.
    this.notify(`${op.what} failed${again ? " again" : ""}: ${explain(error)}`, true, [...retry, { label: "Show it", href: op.href }]);
  }
}
const isData = (v: unknown): v is Data => !!v && typeof v === "object" && !Array.isArray(v);
/** A JSON object from the server; anything else reads as empty. */
export const asData = (v: unknown): Data => (isData(v) ? v : {});
export const app = new App();
