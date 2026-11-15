<script lang="ts">
  // Activity: what the proxy is doing now, and what happened to a request.
  //   Requests   — every request this tab has watched finish, drawn live: a timeline per
  //                account, trends for requests, tokens, cache and first-token time, and the
  //                list itself. Kept in this tab only, for the last hour.
  //   Server log — the log file the server writes. Durable text, one line per event.
  // Each links to the other by request ID.
  import { Effect } from "effect";
  import { accountName as credName } from "../account";
  import { tick, untrack } from "svelte";
  import { focusAfter, find, retried } from "../focus";
  import { blindSpans, median, rateText, watchedMs } from "../observe";
  import Detail from "../parts/Detail.svelte";
  import Setup from "../parts/Setup.svelte";
  import { api } from "../api";
  import { ApiError, download } from "../api";
  import { credState, label, provider, type Data } from "../core";
  import { app, asData, watch, message, Res, server } from "../app.svelte";
  import { configWrite, explain } from "../server";
  import { parseLine, type LogLine, type Request } from "../records";
  import { compact, duration, num, percent, plural, pretty, time, when } from "../fmt";
  import { span as ago } from "../core";
  import Copy from "../parts/Copy.svelte";
  import RequestLog from "../parts/RequestLog.svelte";
  import Timeline from "../parts/Timeline.svelte";
  import Trend from "../parts/Trend.svelte";
  import type { Lane, Step } from "../parts/types";
  import { currentIssues, ISSUE_WINDOW } from "../issues";

  type View = "log" | "requests";
  // #logs opens the live requests; #logs/server the server log.
  const view = $derived<View>(app.route.rest[0] === "server" ? "log" : "requests");

  // ---- server log -----------------------------------------------------------------------
  const KEEP = 2000;
  let lines = $state.raw<LogLine[]>([]);
  let logState = $state<"loading" | "ok" | "off">("loading");
  let logError = $state("");
  let logAt = $state(0);
  let cursor = "";
  let n = 0;
  /** The read in progress; a second ask (the timer, Try again) waits for it instead of starting another. */
  let reading: Promise<boolean> | null = null;
  /** Reads the newest lines. True when the server answered (with lines, or with logging off). */
  function readLog(): Promise<boolean> {
    return (reading ??= readOnce().finally(() => (reading = null)));
  }
  async function readOnce(): Promise<boolean> {
    try {
      const r = await api(`/observability/logs?limit=300${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ""}`);
      const fresh: LogLine[] = (Array.isArray(r.lines) ? r.lines : []).map((l: unknown) => parseLine(String(l), ++n));
      lines = (cursor && !r["cursor-reset"] ? [...lines, ...fresh] : fresh).slice(-KEEP);
      cursor = typeof r["next-cursor"] === "string" ? r["next-cursor"] : "";
      logState = "ok";
      logError = "";
      logAt = Date.now();
      return true;
    } catch (e) {
      // The one failure that is a setting, not a fault: the server writes no log file.
      if (e instanceof ApiError && e.status === 400 && /logging to file disabled/.test(e.message)) {
        logState = "off";
        logError = "";
        lines = [];
        cursor = "";
        return true;
      }
      logError = message(e);
      if (logState === "loading") logState = "ok";
      return false;
    }
  }
  const retryLog = () => retried(readLog(), () => scroller ?? find(".empty h2"), () => app.notify("Server log read again."));
  // Reads stop when the screen closes.
  $effect(() => {
    readLog();
    const timer = setInterval(() => document.hidden || readLog(), 3000);
    return () => clearInterval(timer);
  });

  // ---- filters --------------------------------------------------------------------------
  let query = $state(app.route.query.get("q") || "");
  let kind = $state<"all" | "requests" | "problems">("all");
  let own = $state(false);
  let failedOnly = $state(app.route.query.get("failed") === "1");
  let account = $state(app.route.query.get("account") || "");
  // A link from Usage or Accounts carries its filter in the address.
  $effect(() => {
    const q = app.route.query;
    untrack(() => {
      if (q.has("account")) account = q.get("account") || "";
      if (q.has("failed")) failedOnly = q.get("failed") === "1";
      if (q.has("q")) query = q.get("q") || "";
      // An issue counts failures over 15 minutes; a shorter window would hide some of them.
      if (q.has("failed") && span < 15) span = 15;
    });
  });
  const needle = $derived(query.trim().toLowerCase());
  const accounts = $derived(app.accounts.data || []);
  const nameOf = (auth: string) => {
    const a = accounts.find((x) => x.auth_index === auth);
    return a ? credName(a) : auth ? `Removed account ${auth.slice(0, 6)}` : "Unknown";
  };
  const lineMatches = (l: LogLine) =>
    (own || !l.own) &&
    (kind === "all" || (kind === "requests" ? !!l.access : l.level === "warn" || l.level === "error")) &&
    (!needle || l.raw.toLowerCase().includes(needle));
  const requestMatches = (r: Request) =>
    (!failedOnly || r.failed) &&
    (!account || r.auth === account) &&
    (!needle || [r.model, r.id, String(r.status), r.endpoint, r.provider, nameOf(r.auth)].some((v) => v.toLowerCase().includes(needle)));
  const filtered = $derived(kind !== "all" || own || !!needle);
  const requestsFiltered = $derived(failedOnly || !!account || !!needle);
  function clearFilters() {
    query = "";
    kind = "all";
    failedOnly = false;
    account = "";
    own = false;
    if (app.route.query.size) app.go(view === "requests" ? "logs" : "logs/server");
    focusAfter(() => searchBox);
  }
  let searchBox = $state<HTMLInputElement>();
  /** Filters beyond the search sit behind one button on a phone, so the list starts higher. */
  let filtersOpen = $state(false);

  // ---- holding still --------------------------------------------------------------------
  // New entries join the list only while the user is at its newest end with nothing open and
  // nothing selected. Otherwise they wait, and a button says how many.
  let paused = $state(false);
  let away = $state(false);
  let selecting = $state(false);
  let scroller = $state<HTMLElement>();
  let selLine = $state<number | null>(null);
  let selRequest = $state<number | null>(null);
  let files = $state(false);
  const line = $derived(view === "log" ? lines.find((l) => l.n === selLine) : undefined);
  const request = $derived(view === "requests" ? watch.requests.find((r) => r.n === selRequest) : undefined);
  const picked = $derived(!!line || !!request || (view === "log" && files));
  const modal = $derived(!app.inline);
  const held = $derived(paused || away || selecting || !!line || !!request);
  const latest = $derived(view === "log" ? (lines.at(-1)?.n ?? 0) : (watch.requests.at(-1)?.n ?? 0));
  const limit = $state({ log: 0, requests: 0 });
  $effect(() => {
    if (!held) limit[view] = latest;
  });
  // Changing view starts at that view's newest entry.
  $effect(() => {
    const v = view;
    untrack(() => {
      limit[v] = latest;
      away = false;
    });
  });
  $effect(() => {
    const onSelect = () => {
      const s = document.getSelection();
      selecting = !!s && !s.isCollapsed && !!scroller && scroller.contains(s.anchorNode);
    };
    document.addEventListener("selectionchange", onSelect);
    return () => document.removeEventListener("selectionchange", onSelect);
  });

  const shownLines = $derived(lines.filter((l) => l.n <= limit.log && lineMatches(l)).reverse());
  const shownRequests = $derived(watch.requests.filter((r) => r.n <= limit.requests && requestMatches(r)).reverse());
  const waiting = $derived(
    view === "log" ? lines.filter((l) => l.n > limit.log && lineMatches(l)).length : watch.requests.filter((r) => r.n > limit.requests && requestMatches(r)).length,
  );

  // ---- the live window ------------------------------------------------------------------
  // One window scopes the trends, the timeline and the list. It ends now and slides every
  // second; while the list is held still it stops too, so nothing moves under the pointer.
  let span = $state<5 | 15 | 60>(15);
  let frozen = $state(0);
  $effect(() => {
    if (!held) frozen = 0;
    else if (!untrack(() => frozen)) frozen = Date.now();
  });
  const to = $derived(held && frozen ? frozen : app.now);
  const from = $derived(to - span * 60_000);
  const size = $derived((span * 60_000) / 30);
  let hoverT = $state<number | null>(null);
  const ended = (r: Request) => r.at + r.latency;
  const inWindow = $derived(shownRequests.filter((r) => ended(r) > from && r.at < to));
  const total = $derived.by(() => {
    const ok = inWindow.filter((r) => !r.failed);
    const sum = (pick: (r: Request) => number) => ok.reduce((n, r) => n + pick(r), 0);
    return {
      requests: inWindow.length,
      failed: inWindow.length - ok.length,
      input: sum((r) => r.tokens.input),
      cached: sum((r) => r.tokens.cacheRead),
      output: sum((r) => r.tokens.output),
      reasoning: sum((r) => r.tokens.reasoning),
      succeeded: ok.length,
      ttft: median(ok.map((r) => r.ttft)),
      slowest: Math.max(0, ...ok.map((r) => r.ttft)),
    };
  });
  /** What of the window this tab saw; counts over the rest are unknown, not zero. */
  const blind = $derived(blindSpans(from, to, { since: watch.since, gaps: watch.gaps, stoppedAt: watch.state === "off" ? watch.stoppedAt : 0 }));
  const watched = $derived(watchedMs(from, to, blind));
  const windowName = $derived(span === 60 ? "hour" : `${span} minutes`);
  /** The window cut into 30 steps on clock boundaries; a request counts where it finished. */
  function steps(pick: (r: Request) => [number, number]): Step[] {
    const first = Math.floor(from / size) * size;
    const out: Step[] = [];
    for (let t = first; t < to; t += size) out.push({ t, a: 0, b: 0 });
    for (const r of inWindow) {
      const s = out[Math.floor((ended(r) - first) / size)];
      if (!s) continue;
      const [a, b] = pick(r);
      s.a += a;
      s.b += b;
    }
    return out;
  }
  const requestSteps = $derived(steps((r) => (r.failed ? [0, 1] : [1, 0])));
  const inputSteps = $derived(steps((r) => [r.tokens.cacheRead, r.tokens.input - r.tokens.cacheRead]));
  const outputSteps = $derived(steps((r) => [r.tokens.output, 0]));
  const firstTokens = $derived(inWindow.filter((r) => !r.failed && r.ttft > 0).map((r) => ({ t: r.at + r.ttft, v: r.ttft, r })));
  const laneState = (a: Data): Lane["state"] => (a.disabled || a.configured_only ? "off" : credState(a, to).lamp);
  /** A lane for every account that can take requests, in the server's order so lanes stay put. */
  const lanes = $derived<Lane[]>([
    ...accounts.filter((a) => (!account || a.auth_index === account) && (!a.disabled || inWindow.some((r) => r.auth === a.auth_index))).map((a) => ({ key: String(a.auth_index), name: credName(a), sub: label(provider(a)), state: laneState(a) })),
    ...[...new Set(inWindow.map((r) => r.auth))].filter((k) => !accounts.some((a) => a.auth_index === k)).map((k) => ({ key: k, name: nameOf(k), sub: "", state: "off" as const })),
  ]);
  const windowGaps = $derived(watch.gaps.filter((g) => g.to > from && g.from < to));
  /** On first run there is nothing to watch: the screen offers setup instead of empty charts. */
  $effect(() => {
    if (app.accounts.data && !app.accounts.data.length) app.setupStarted = true;
  });
  const firstRun = $derived(
    !!app.accounts.data &&
      (!app.accounts.data.length || app.setupVerified || (app.setupStarted && (!app.setting<string[]>("access/api-keys", []).length || !app.setting<string[]>("access/api-keys", []).includes(app.setupTestKey)))),
  );
  // Leaving Activity ends the "setup verified" message; the setup stays done.
  $effect(() => () => void (app.setupVerified = false));
  function setupDone() {
    app.setupVerified = false;
    focusAfter(() => find(".live-sum button") ?? find(".start-watching") ?? searchBox);
  }
  /** Trends and timeline in full, or one line of figures so the requests start near the top. Kept across screens. */
  const fullCharts = $derived(app.charts === "full");

  // ---- issues ---------------------------------------------------------------------------
  const issues = $derived(currentIssues(Math.ceil(to / 15_000) * 15_000));
  let allIssues = $state(false);
  /** Issues shown before "Show all": enough to see what is wrong, few enough that the list stays in view. */
  const ISSUES_SHOWN = $derived(app.wide ? 3 : app.short ? 0 : 1);
  const servingCount = $derived(accounts.filter((a) => !a.disabled && credState(a, to).lamp === "ok").length);
  const unknownKeys = $derived(accounts.filter((a) => a.configured_only && !a.disabled).length);
  /** Failed requests the server log remembers from before this tab watched, by status. */
  const earlier = $derived.by(() => {
    const failed = lines.filter((l) => l.access && !l.own && l.access.status >= 400 && l.at < (watch.since || Infinity));
    const by = new Map<number, number>();
    for (const l of failed) by.set(l.access!.status, (by.get(l.access!.status) || 0) + 1);
    return { count: failed.length, since: lines.find((l) => l.at)?.at ?? 0, by: [...by.entries()].sort((a, b) => b[1] - a[1]) };
  });

  type Row = { kind: "request"; at: number; r: Request } | { kind: "gap"; at: number; from: number };
  const rows = $derived<Row[]>(
    [...inWindow.map((r): Row => ({ kind: "request", at: ended(r), r })), ...windowGaps.map((g): Row => ({ kind: "gap", at: g.to, from: g.from }))].sort((a, b) => b.at - a.at),
  );
  function showWaiting() {
    document.getSelection()?.removeAllRanges();
    limit[view] = latest;
    paused = false;
    scroller?.scrollTo({ top: 0 });
    // The button is gone: continue at the newest entry, the first row of the list.
    focusAfter(() => find(view === "log" ? ".logline .t" : "tbody .cellbtn", scroller), () => scroller);
  }

  // ---- opening details ------------------------------------------------------------------
  let opener: HTMLElement | null = null;
  // A drag that selected text is not a click on the row.
  const clicked = () => document.getSelection()?.isCollapsed !== false;
  // The detail takes focus itself when it opens (parts/Detail.svelte).
  async function reveal() {
    await tick();
    scroller?.querySelector(".sel")?.scrollIntoView({ block: "nearest" });
  }
  function openLine(l: LogLine, from?: EventTarget | null) {
    if (!clicked()) return;
    if (from instanceof HTMLElement) opener = from;
    files = false;
    selLine = selLine === l.n ? null : l.n;
    if (selLine !== null) reveal();
  }
  function openRequest(r: Request, from?: EventTarget | null) {
    if (!clicked()) return;
    if (from instanceof HTMLElement) opener = from;
    selRequest = selRequest === r.n ? null : r.n;
    if (selRequest !== null) reveal();
  }
  function close() {
    selLine = selRequest = null;
    files = false;
    opener = null;
  }
  /** Where focus goes when a detail closes and its opener is gone. */
  /** Where a closing detail sends focus when what opened it is gone: its row, or the list. */
  const returnTo = $derived([request ? `[data-req="${request.n}"]` : "", line ? `[data-line="${line.n}"]` : "", files ? ".files-btn" : "", ".loglines"].filter(Boolean));
  /** From a log line to the watched request with the same ID, and back. */
  function jumpToRequest(r: Request) {
    selRequest = r.n;
    // Its row must be in the window to be seen: the hour is everything this tab kept.
    span = 60;
    app.go("logs");
    reveal();
  }
  function jumpToLine(l: LogLine) {
    selLine = l.n;
    files = false;
    if (l.own) own = true;
    app.go("logs/server");
    reveal();
  }
  const day = (t: number) => new Date(t).toLocaleDateString("en-US", { weekday: "long", day: "numeric", month: "long" });
  const linesFor = (id: string) => (id ? lines.filter((l) => l.id && id.endsWith(l.id)) : []);
  const watchedFor = (id: string) => (id ? watch.requests.find((r) => r.id.endsWith(id)) : undefined);

  // ---- log files ------------------------------------------------------------------------
  const errorLogs = new Res<Data[]>(server.read("/observability/logs/errors").pipe(Effect.map((b) => (asData(b).files || []).filter((f: unknown): f is Data => !!f && typeof f === "object"))));
  let confirmClear = $state(false);
  let clearError = $state("");
  function openFiles() {
    selLine = null;
    files = !files;
    confirmClear = false;
    clearError = "";
    if (files) {
      errorLogs.load();
      reveal();
    }
  }
  async function clearLog() {
    clearError = await app.act(async () => {
      await api("/observability/logs", "DELETE");
      cursor = "";
      lines = [];
      await readLog();
    }, "Server log cleared.");
    if (!clearError) confirmClear = false;
    focusAfter(() => find(confirmClear ? ".clear-confirm .danger" : ".clear-start"));
  }
  function askClear(ask: boolean) {
    confirmClear = ask;
    focusAfter(() => find(ask ? ".clear-confirm .keep" : ".clear-start"));
  }
  function stopWatching() {
    watch.stop();
    paused = false;
    focusAfter(() => find(".start-watching"));
  }
  async function startWatching() {
    await watch.start();
    focusAfter(() => (watch.state === "on" ? find(".stop-watching") : find(".start-watching")));
  }
  let turningOn = $state("");
  async function turnOnLogging() {
    const error = await app.write(configWrite({ path: "observability/logs/logging-to-file", before: false, next: true, what: "Turning on file logging", href: "#logs/server" }), "File logging is on.");
    turningOn = error ? explain(error) : "";
    if (!turningOn) {
      logState = "loading";
      readLog();
    }
  }
</script>


{#snippet body(l: LogLine)}
  {#if l.access}
    <span class="msg"><span class="code-status" class:bad={l.access.status >= 400}>{l.access.status}</span> {l.access.method} {l.access.path}  <span class="faint">{l.access.duration}</span></span>
  {:else}<span class="msg">{l.message}</span>{/if}
{/snippet}

<div class="page fill">
  <div class="toolbar">
    <h1 class="sr">Activity</h1>
    <div class="seg max-md:order-1" role="group" aria-label="Source">
      <button aria-pressed={view === "requests"} onclick={() => app.go("logs")}>Requests</button>
      <button aria-pressed={view === "log"} onclick={() => app.go("logs/server")}>Server log</button>
    </div>
    <label class="search max-md:order-3">
      <span class="sr">Search {view === "log" ? "log lines" : "requests"}</span>
      <input type="search" bind:this={searchBox} bind:value={query} placeholder={view === "log" ? "Search lines, a path or a request ID" : "Search model, account, status or ID"} />
    </label>
    <button class="btn filters-toggle max-md:order-4" aria-expanded={filtersOpen} aria-controls="activity-filters" onclick={() => (filtersOpen = !filtersOpen)}>
      Filters{#if view === "log" ? filtered && (kind !== "all" || own) : failedOnly || account}<span class="dot live" aria-hidden="true"></span><span class="sr"> (on)</span>{/if}
    </button>
    <div class="filters" class:open={filtersOpen} id="activity-filters">
      {#if view === "requests"}
        <div class="seg" role="group" aria-label="Time window">
          {#each [[5, "5 min"], [15, "15 min"], [60, "1 hour"]] as const as [m, name]}<button aria-pressed={span === m} onclick={() => (span = m)}>{name}</button>{/each}
        </div>
      {/if}
      {#if view === "log"}
        <div class="seg" role="group" aria-label="Show">
          <button aria-pressed={kind === "all"} onclick={() => (kind = "all")}>All</button>
          <button aria-pressed={kind === "requests"} onclick={() => (kind = "requests")}>Requests</button>
          <button aria-pressed={kind === "problems"} onclick={() => (kind = "problems")}>Warnings and errors</button>
        </div>
        <label class="check small"><input type="checkbox" bind:checked={own} />Dashboard's own calls</label>
      {:else}
        <div class="seg" role="group" aria-label="Show">
          <button aria-pressed={!failedOnly} onclick={() => (failedOnly = false)}>All</button>
          <button aria-pressed={failedOnly} onclick={() => (failedOnly = true)}>Failed</button>
        </div>
        <label class="block min-w-0 max-w-full">
          <span class="sr">Account</span>
          <select class="max-w-full" bind:value={account}>
            <option value="">All accounts</option>
            {#each accounts as a (a.auth_index)}<option value={a.auth_index}>{label(provider(a))}: {credName(a)}</option>{/each}
          </select>
        </label>
      {/if}
    </div>
    <!-- On a phone the keys regroup into two rows: source and live state, then search, filters and trends. -->
    <div class="end max-md:contents">
      {#if view === "requests" && watch.state === "off"}
        <button class="btn start-watching max-md:order-2" onclick={startWatching}><span class="dot off"></span>{watch.since ? "Stopped: start again" : "Start watching"}</button>
      {:else}
        <button class="btn max-md:order-2" aria-pressed={!paused} onclick={() => (paused = !paused)} title={paused ? "Show new entries as they arrive" : "Hold the list still; new entries wait"}>
          <span class="dot {paused ? 'off' : 'live pulse'}"></span>{paused ? "Paused" : "Live"}
        </button>
      {/if}
      {#if view === "requests" && watch.since && !firstRun}
        <button class="btn quiet max-md:order-5" aria-pressed={fullCharts} onclick={() => app.setCharts(fullCharts ? "compact" : "full")}>Trends<span class="hidden sm:inline">&nbsp;and timeline</span></button>
      {/if}
      {#if view === "log"}<button class="btn files-btn max-md:order-5" aria-pressed={files} onclick={(e) => ((opener = e.currentTarget), openFiles())}>Log files</button>{/if}
    </div>
  </div>

  <div class="split" class:open={picked && !modal}>
    <div class="list">
      {#if waiting > 0 && held}
        <button class="btn primary small newer" onclick={showWaiting}>Show {plural(waiting, "new entry", "new entries")}</button>
      {/if}

      {#if view === "log"}
        {#if logError}
          <p class="notice bad small" role="alert" style="margin:8px">
            <span class="dot bad"></span>
            <span class="grow">Could not read the server log: {logError}{lines.length ? ` The lines below were read at ${time(logAt)}.` : ""}</span>
            <button class="btn small" onclick={retryLog}>Try again</button>
          </p>
        {/if}
        {#if logState === "loading"}
          <p class="empty muted">Reading the server log…</p>
        {:else if logState === "off"}
          <div class="empty">
            <h2>The server is not writing a log file</h2>
            <p class="muted">This list reads the server's log file, and file logging is off, so there is nothing to read. Turning it on takes effect right away and does not restart the server.</p>
            {#if turningOn}<p class="notice bad small" role="alert"><span class="dot bad"></span><span>File logging was not turned on: {turningOn}</span></p>{/if}
            <div class="row">
              <button class="btn primary" aria-disabled={app.busy} onclick={turnOnLogging}>{turningOn ? "Try again" : "Turn on file logging"}</button>
              <a class="btn" href="#logs">See live requests instead</a>
            </div>
          </div>
        {:else if !shownLines.length}
          <div class="empty">
            {#if filtered && lines.length}
              <h2>No lines match</h2>
              <p class="muted">{num(lines.length)} lines are loaded; none match the search and filters.</p>
              <button class="btn" onclick={clearFilters}>Clear search and filters</button>
            {:else if !logError}
              <h2>The log is empty</h2>
              <p class="muted">Lines appear here as the server writes them.</p>
            {/if}
          </div>
        {:else}
          <!-- svelte-ignore a11y_no_noninteractive_tabindex: the scrolling log must be reachable by keyboard -->
          <div class="scroll loglines" bind:this={scroller} onscroll={() => (away = (scroller?.scrollTop ?? 0) > 24)} role="log" aria-label="Server log, newest first" aria-live="off" tabindex="0">
            {#each shownLines as l, i (l.n)}
              {#if i > 0 && l.at && shownLines[i - 1].at && day(l.at) !== day(shownLines[i - 1].at)}<div class="logday">{day(l.at)}</div>{/if}
              <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions: the time is the row's button; the row click is a larger target for the same action -->
              <div class="logline {l.level}" class:sel={selLine === l.n} onclick={(e) => openLine(l, e.currentTarget.querySelector("button"))}>
                <button class="t" data-line={l.n} aria-expanded={selLine === l.n} aria-label={`${l.at ? time(l.at) : "No time"}, ${l.level || "line"}: open this line`} onclick={(e) => (e.stopPropagation(), openLine(l, e.currentTarget))}
                  >{l.at ? time(l.at) : "—"}</button
                >
                <span class="lvl">{l.level}</span>
                {@render body(l)}
              </div>
            {/each}
          </div>
        {/if}
        <div class="status-line">
          <span role="status">
            {#if logState !== "ok"}&nbsp;
            {:else if paused}Paused. {waiting ? `${plural(waiting, "new line")} waiting.` : "No new lines yet."}
            {:else if held && waiting}{plural(waiting, "new line")} waiting while you read.
            {:else}Newest first. New lines join as the server writes them.{/if}
          </span>
          {#if logState === "ok"}<span class="wide-only">{num(shownLines.length)} of {num(lines.length)} loaded lines; this tab keeps the newest {num(KEEP)}</span>{/if}
        </div>
      {:else}
        {#if watch.problem}
          <p class="notice bad small" role="alert" style="margin:8px">
            <span class="dot bad"></span><span class="grow">Could not read live requests: {watch.problem}{watch.state === "on" ? " Trying again every few seconds." : ""}</span>
            {#if watch.state === "off"}<button class="btn small" onclick={startWatching}>Try again</button>{/if}
          </p>
        {/if}
        {#if !watch.statistics}
          <p class="notice warn small" style="margin:8px">
            <span class="dot warn"></span><span>Usage statistics are off on this server, so it reports no requests here. <a href="#settings">Turn them on in Settings</a></span>
          </p>
        {/if}
        {#if firstRun}
          <div class="scroll"><Setup ondone={setupDone} /></div>
        {:else if !watch.since}
          <div class="empty">
            <h2>Not watching requests</h2>
            <p class="muted">The live view draws each request as it finishes: which account took it, how long it ran, its tokens and cache use. It lasts as long as this tab watches.</p>
            <button class="btn primary start-watching" onclick={startWatching}>Start watching</button>
          </div>
        {:else}
          <div class="scroll" bind:this={scroller} onscroll={() => (away = (scroller?.scrollTop ?? 0) > 24)}>
            <section class="issues" aria-labelledby="issues-title">
              {#if !issues.length}
                <p class="row small muted" id="issues-title">
                  <span class="dot ok"></span>No reported issues: {plural(servingCount, "account")} reported ready{unknownKeys ? `; ${plural(unknownKeys, "configured key")} with unknown health` : ""}. No watched request failed in the last {ISSUE_WINDOW / 60_000} minutes.
                </p>
              {:else}
                <h2 id="issues-title">{plural(issues.length, "issue")} now</h2>
                {#if allIssues || ISSUES_SHOWN}<ul>
                  {#each allIssues ? issues : issues.slice(0, ISSUES_SHOWN) as i (i.key)}
                    <li class={i.level}>
                      <span class="dot {i.level}"></span>
                      <div class="what">
                        <strong>{i.title}</strong>
                        <span class="small muted">{i.detail} <span class="meta">· {i.kind}{i.last ? `, last ${time(i.last)}` : ""}</span></span>
                      </div>
                      {#if i.action}<a class="btn small" href={i.action.href}>{i.action.label}</a>{/if}
                    </li>
                  {/each}
                </ul>{/if}
                {#if issues.length > ISSUES_SHOWN}
                  <button class="link small justify-self-start" aria-expanded={allIssues} onclick={() => (allIssues = !allIssues)}>{allIssues ? (ISSUES_SHOWN ? `Show the first ${ISSUES_SHOWN}` : "Hide them") : ISSUES_SHOWN ? `Show ${issues.length - ISSUES_SHOWN} more` : `Show ${issues.length === 1 ? "it" : "them"}`}</button>
                {/if}
              {/if}
              {#if earlier.count && (app.wide || allIssues || !issues.length)}
                <p class="small muted">
                  Before this tab was watching, the server log has {plural(earlier.count, "failed request")} since {when(earlier.since)}:
                  {earlier.by.map(([status, n]) => `${n} with ${status}`).join(", ")}. <a href="#logs/server" onclick={() => (kind = "problems")}>Open the server log</a>
                </p>
              {/if}
            </section>
            <section class="live-sum grid gap-2 border-b border-line px-3.5 py-2.5" aria-label={`Live requests, last ${windowName}`}>
              {#if !fullCharts}
              <div class="flex flex-wrap items-center gap-x-6 gap-y-1">
                  <dl class="grid grow basis-full grid-cols-2 gap-x-4 gap-y-1 sm:flex sm:basis-auto sm:flex-wrap sm:gap-x-7">
                    <div class="flex min-w-0 flex-wrap items-baseline gap-x-1.5"><dt class="small muted">Requests</dt><dd class="font-semibold">{num(total.requests)}</dd><dd class="small {total.failed ? 'code-status bad' : 'muted'}">{total.failed} failed</dd></div>
                    <div class="flex min-w-0 flex-wrap items-baseline gap-x-1.5"><dt class="small muted">Input</dt><dd class="font-semibold">{total.succeeded ? compact(total.input) : "—"}</dd><dd class="small muted">{total.succeeded ? `${percent(total.cached, total.input)} cached` : ""}</dd></div>
                    <div class="flex min-w-0 flex-wrap items-baseline gap-x-1.5"><dt class="small muted">Output</dt><dd class="font-semibold">{total.succeeded ? compact(total.output) : "—"}</dd></div>
                    <div class="flex min-w-0 flex-wrap items-baseline gap-x-1.5"><dt class="small muted">First token</dt><dd class="font-semibold">{total.ttft === null ? "—" : duration(total.ttft)}</dd><dd class="small muted">{total.ttft === null ? "" : "median"}</dd></div>
                  </dl>
              </div>
              {/if}
              {#if blind.length}
                <p class="small muted watched">
                  <span class="hatch-key" aria-hidden="true"></span>
                  <span>{watched ? `Watched ${ago(watched)} of the last ${windowName}` : `Nothing watched in the last ${windowName}`}{watch.state === "off" ? ", not watching now" : ""}, so these counts are lower bounds.</span>
                </p>
              {/if}
            </section>
            {#if fullCharts}
              <section class="activity" id="live-charts" aria-label={`Trends and timeline, last ${windowName}`}>
                <div class="trends">
                  <Trend
                    title="Requests"
                    value={num(total.requests)}
                    note={`${rateText(total.requests, watched)}, ${total.failed} failed`}
                    names={["Succeeded", "Failed"]}
                    tone="bad"
                    steps={requestSteps}
                    fmt={compact}
                    read={(s) => `${num(s.a + s.b)}${s.b ? `, ${num(s.b)} failed` : ""}`}
                    {from} {to} {size} hover={hoverT} onhover={(t) => (hoverT = t)}
                  />
                  <Trend
                    title="Input tokens"
                    value={total.succeeded ? compact(total.input) : "—"}
                    note={total.succeeded ? `${percent(total.cached, total.input)} read from cache` : "no finished requests"}
                    names={["From cache", "Not cached"]}
                    tone="light"
                    steps={inputSteps}
                    fmt={compact}
                    read={(s) => `${compact(s.a + s.b)}, ${percent(s.a, s.a + s.b)} cached`}
                    {from} {to} {size} hover={hoverT} onhover={(t) => (hoverT = t)}
                  />
                  <Trend
                    title="Output tokens"
                    value={total.succeeded ? compact(total.output) : "—"}
                    note={!total.succeeded ? "no finished requests" : total.reasoning ? `${compact(total.reasoning)} reasoning` : "no reasoning tokens"}
                    steps={outputSteps}
                    fmt={compact}
                    read={(s) => compact(s.a)}
                    {from} {to} {size} hover={hoverT} onhover={(t) => (hoverT = t)}
                  />
                  <Trend
                    title="Time to first token"
                    value={total.ttft === null ? "—" : duration(total.ttft)}
                    note={total.ttft === null ? "no finished requests" : `median; slowest ${duration(total.slowest)}`}
                    dots={firstTokens}
                    fmt={duration}
                    empty="No finished requests in this window"
                    {from} {to} {size} hover={hoverT} onhover={(t) => (hoverT = t)} onpick={(r) => openRequest(r, document.activeElement)}
                  />
                </div>
                <Timeline {lanes} requests={inWindow} {from} {to} {blind} selected={selRequest} hover={hoverT} onhover={(t) => (hoverT = t)} onpick={(r) => openRequest(r, document.activeElement)} />
              </section>
            {/if}
            {#if !rows.length}
              <div class="empty">
                {#if requestsFiltered && watch.requests.length}
                  <h2>No requests match</h2>
                  <p class="muted">{plural(watch.requests.length, "request")} watched; none match the search and filters in this window.</p>
                  <button class="btn" onclick={clearFilters}>Clear search and filters</button>
                {:else if watch.requests.length}
                  <h2>Nothing in the last {windowName}</h2>
                  <p class="muted">{plural(watch.requests.length, "request")} finished earlier. <button class="link" onclick={() => (span = 60)}>Show the last hour</button></p>
                {:else}
                  <h2>No requests since {time(watch.since, false)}</h2>
                  <p class="muted">
                    Each request shows up here when it finishes. The server keeps no history of them: for anything earlier, the
                    <a href="#logs/server">server log</a> has one line per request, without model or token detail.
                  </p>
                {/if}
              </div>
            {:else}
              <table class="requests">
                <caption class="sr">Requests that finished in the last {windowName}, newest first</caption>
                <thead>
                  <tr>
                    <th scope="col">Finished</th><th scope="col">Status</th><th scope="col">Model</th><th scope="col" class="c-acct">Account</th><th scope="col" class="num c-dur">Duration</th>
                    <th scope="col" class="num c-wide">First token</th><th scope="col" class="num c-mid">Input</th><th scope="col" class="num c-wide">From cache</th><th scope="col" class="num c-mid">Output</th>
                  </tr>
                </thead>
                <tbody>
                  {#each rows as row (row.kind === "request" ? row.r.n : `gap-${row.at}`)}
                    {#if row.kind === "gap"}
                      <tr class="gap-row"><td colspan="9"><span class="dot warn"></span> Not captured: requests between {time(row.from)} and {time(row.at)}. This tab was hidden, behind or not watching.</td></tr>
                    {:else}
                      {@const r = row.r}
                      <tr class="clickable" class:failed={r.failed} class:sel={selRequest === r.n} onclick={(e) => openRequest(r, e.currentTarget.querySelector("button"))}>
                        <td><button class="cellbtn" data-req={r.n} aria-expanded={selRequest === r.n} aria-label={`${time(ended(r))}, ${r.failed ? `failed ${r.status || ""}` : r.status}, ${r.model}: open this request`} onclick={(e) => (e.stopPropagation(), openRequest(r, e.currentTarget))}>{time(ended(r))}</button></td>
                        <td><span class="code-status" class:bad={r.failed}>{r.status || "Failed"}</span></td>
                        <td><span class="mono model">{r.model}</span><div class="acct-under small muted">{nameOf(r.auth)} · {label(r.provider)}<span class="dur-under">&nbsp;· {duration(r.latency)}</span></div></td>
                        <td class="c-acct">{nameOf(r.auth)} <span class="faint">{label(r.provider)}</span></td>
                        <td class="num c-dur">{duration(r.latency)}</td>
                        <td class="num c-wide">{r.ttft ? duration(r.ttft) : "—"}</td>
                        <td class="num c-mid">{r.failed ? "—" : num(r.tokens.input)}</td>
                        <td class="num c-wide">{r.failed ? "—" : percent(r.tokens.cacheRead, r.tokens.input)}</td>
                        <td class="num c-mid">{r.failed ? "—" : num(r.tokens.output)}</td>
                      </tr>
                    {/if}
                  {/each}
                </tbody>
              </table>
            {/if}
          </div>
        {/if}
        <div class="status-line">
          <span role="status">
            {#if firstRun}{!accounts.length ? "No accounts yet, so nothing is sent through the proxy." : app.setupVerified ? "Setup verified." : "Finish setup to verify your tool can reach the proxy."}
            {:else if watch.state === "off"}{watch.since ? `Stopped watching at ${time(watch.stoppedAt || Date.now(), false)}. Requests finishing now are not captured.` : "Not watching."}
            {:else if paused}Paused. {waiting ? `${plural(waiting, "new request")} waiting.` : "Still watching; nothing new yet."}
            {:else if held && waiting}{plural(waiting, "new request")} waiting while you read.
            {:else if watch.resumedAt}Watching again since {time(watch.resumedAt, false)}; first started at {time(watch.since, false)}.
            {:else}Watching since {time(watch.since, false)}.{/if}
          </span>
          {#if watch.state === "on" && !firstRun}<button class="link stop-watching" onclick={stopWatching}>Stop watching</button>{/if}
          <span class="wide-only grow end-note">A request appears when it finishes. Kept in this tab only: the last hour, at most 2,000</span>
        </div>
      {/if}
    </div>

    {#if line}
      {@const related = linesFor(line.id).filter((l) => l.n !== line.n)}
      {@const seen = watchedFor(line.id)}
      <Detail label="Log line details" {modal} item={`line-${line.n}`} onclose={close} {returnTo}>
        {#snippet head()}
          <h2 data-detail-title tabindex="-1">{line.access ? `${line.access.status} ${line.access.method} ${line.access.path}` : "Log line"}</h2>
          <p class="small muted">{line.at ? `${when(line.at)}:${String(new Date(line.at).getSeconds()).padStart(2, "0")}` : "No timestamp"}{line.level ? `, ${line.level}` : ""}</p>
        {/snippet}
        <section>
          <pre class="code tall">{line.raw}</pre>
          <div class="row"><Copy text={line.raw} label="Copy line" /></div>
        </section>
        {#if line.access}
          <section>
            <dl class="facts">
              <dt>Status</dt><dd><span class="code-status" class:bad={line.access.status >= 400}>{line.access.status}</span></dd>
              <dt>Took</dt><dd>{line.access.duration}</dd>
              <dt>Request</dt><dd class="mono">{line.access.method} {line.access.path}</dd>
              <dt>From</dt><dd class="mono">{line.access.client}</dd>
            </dl>
          </section>
        {/if}
        {#if line.id}
          <section>
            <h3>Same request <span class="mono faint">{line.id}</span></h3>
            {#if seen}
              <p class="small">
                This tab watched it: <span class="mono">{seen.model}</span> on {nameOf(seen.auth)}{seen.failed ? "" : `, ${num(seen.tokens.input + seen.tokens.output)} tokens`}.
                <button class="link" onclick={() => jumpToRequest(seen)}>Model, timing and tokens</button>
              </p>
            {/if}
            {#if related.length}
              <div class="loglines card">
                {#each related as l (l.n)}
                  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions: the time is the row's button -->
                  <div class="logline {l.level}" onclick={() => openLine(l)}>
                    <button class="t" onclick={(e) => (e.stopPropagation(), openLine(l))}>{time(l.at)}</button><span class="lvl">{l.level}</span>{@render body(l)}
                  </div>
                {/each}
              </div>
            {:else if !seen}<p class="small muted">No other loaded line has this request ID.</p>{/if}
          </section>
          {#if !line.own}<RequestLog id={line.id} />{/if}
        {/if}
      </Detail>
    {:else if request}
      {@const r = request}
      {@const related = linesFor(r.id)}
      <Detail label={`Request ${r.id.slice(-8)} details`} {modal} item={`req-${r.n}`} onclose={close} {returnTo}>
        {#snippet head()}
          <h2 data-detail-title tabindex="-1"><span class="code-status" class:bad={r.failed}>{r.status || "Failed"}</span> <span class="mono">{r.model}</span></h2>
          <p class="small muted">Started {time(r.at)}, finished {time(ended(r))}, took {duration(r.latency)}</p>
        {/snippet}
        {#if r.failure}
          <section>
            <h3>Failure</h3>
            <p class="notice bad small"><span class="dot bad"></span><span>The provider answered HTTP {r.status}{r.failure.code ? ` (${r.failure.code})` : ""}{r.failure.message ? `: ${r.failure.message}` : "."}</span></p>
            {#if r.failure.body}
              <pre class="code">{pretty(r.failure.body)}</pre>
              <div class="row"><Copy text={pretty(r.failure.body)} label="Copy response" /></div>
            {:else}<p class="small muted">The server recorded no response body.</p>{/if}
          </section>
        {/if}
        <section>
          <dl class="facts">
            <dt>Account</dt>
            <dd>
              {#if accounts.some((a) => a.auth_index === r.auth)}<a class="inline-flex min-h-11 items-center min-[861px]:min-h-0" href={`#accounts/${encodeURIComponent(r.auth)}`}>{nameOf(r.auth)}</a>{:else}{nameOf(r.auth)}{/if}
              <span class="faint">{label(r.provider)}</span>
            </dd>
            <dt>Endpoint</dt><dd class="mono">{r.endpoint || "Not reported"}</dd>
            <dt>Client</dt><dd>{r.client || "Not reported"}</dd>
            <dt>Response</dt><dd>{r.stream ? "Streamed" : "Sent whole"}</dd>
            {#if r.effort}<dt>Reasoning effort</dt><dd>{r.effort}</dd>{/if}
            <dt>Request ID</dt><dd class="row"><span class="mono" style="overflow-wrap:anywhere">{r.id}</span><Copy text={r.id} label="Copy ID" /></dd>
          </dl>
        </section>
        <section>
          <h3>Timing</h3>
          {#if r.stages.length}
            <ol class="stages">
              {#each [...r.stages, { label: "Finished", ms: r.latency }] as s}
                <li>
                  <span>{s.label}</span>
                  <span class="track" aria-hidden="true"><i style:width={`${Math.min(100, (s.ms / Math.max(1, r.latency)) * 100)}%`}></i></span>
                  <span class="num">{duration(s.ms)}</span>
                </li>
              {/each}
            </ol>
            <p class="small muted">Time since the request arrived. The server timed only the stages listed.</p>
          {:else}<p class="small muted">Took {duration(r.latency)}{r.ttft ? `, first token after ${duration(r.ttft)}` : ""}. The server reported no stage timings for this request.</p>{/if}
        </section>
        {#if !r.failed}
          <section>
            <h3>Tokens</h3>
            <dl class="facts">
              <dt>Input</dt><dd>{num(r.tokens.input)}</dd>
              <dt class="sub">Read from cache</dt><dd>{num(r.tokens.cacheRead)} <span class="faint">{percent(r.tokens.cacheRead, r.tokens.input)}</span></dd>
              <dt class="sub">Written to cache</dt><dd>{num(r.tokens.cacheWrite)}</dd>
              <dt class="sub">Not cached</dt><dd>{num(r.tokens.uncached)}</dd>
              <dt>Output</dt><dd>{num(r.tokens.output)}</dd>
              {#if r.tokens.reasoning}<dt class="sub">Reasoning</dt><dd>{num(r.tokens.reasoning)}</dd>{/if}
            </dl>
          </section>
        {:else}
          <section>
            <h3>Tokens</h3>
            <p class="small muted">Not reported: the request failed before the provider counted tokens.</p>
          </section>
        {/if}
        <section>
          <h3>Server log lines</h3>
          {#if related.length}
            <div class="loglines card">
              {#each related as l (l.n)}
                <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions: the time is the row's button -->
                <div class="logline {l.level}" onclick={() => jumpToLine(l)}>
                  <button class="t" onclick={(e) => (e.stopPropagation(), jumpToLine(l))}>{time(l.at)}</button><span class="lvl">{l.level}</span>{@render body(l)}
                </div>
              {/each}
            </div>
          {:else}<p class="small muted">{logState === "off" ? "File logging is off, so the server wrote no lines for this request." : "No loaded log line has this request ID."}</p>{/if}
        </section>
        <RequestLog id={r.id} />
        <details>
          <summary class="small">Record as the server sent it</summary>
          <pre class="code">{JSON.stringify(r.raw, null, 2)}</pre>
          <div class="row" style="margin-top:8px"><Copy text={JSON.stringify(r.raw, null, 2)} label="Copy record" /><span class="small muted">The client key is left out.</span></div>
        </details>
      </Detail>
    {:else if picked}
      <Detail label="Log files" {modal} item="files" onclose={close} {returnTo}>
        {#snippet head()}<h2 data-detail-title tabindex="-1">Log files</h2>{/snippet}
        <section>
          <h3>Saved failed requests</h3>
          <p class="small muted">With request logging off, the server saves the full request and response of each failed request.</p>
          {#if errorLogs.error}
            <p class="notice bad small" role="alert"><span class="dot bad"></span><span class="grow">Could not list the files: {message(errorLogs.error)}</span><button class="btn small" onclick={() => retried(errorLogs.load().then((r) => r.ok), () => find("[data-detail-title]"), () => app.notify("Log files listed."))}>Try again</button></p>
          {:else if !errorLogs.data}
            <p class="small muted">Reading…</p>
          {:else if !errorLogs.data.length}
            <p class="small">No files. {app.setting("observability/logs/request-log", false) ? "Request logging is on, so failed requests are in the request logs instead." : "No request has failed since the log was last cleared."}</p>
          {:else}
            <ul class="keys">
              {#each errorLogs.data as f (f.name)}
                <li>
                  <span class="grow">
                    <span class="mono" style="overflow-wrap:anywhere">{f.name}</span>
                    <span class="small muted" style="display:block">{f.modified ? when(f.modified * 1000) : ""}{f.size ? `, ${num(Math.ceil(f.size / 1024))} KB` : ""}</span>
                  </span>
                  <button
                    class="btn small"
                    onclick={() => app.run(async () => download(await api(`/observability/logs/errors/${encodeURIComponent(f.name)}`, "GET", undefined, "blob"), f.name))}>Download<span class="sr"> {f.name}</span></button
                  >
                </li>
              {/each}
            </ul>
          {/if}
        </section>
        <section>
          <h3>Clear the server log</h3>
          <p class="small muted">Empties the server's log file and deletes its rotated copies. Saved request logs stay.</p>
          {#if clearError}<p class="notice bad small" role="alert"><span class="dot bad"></span><span>The log was not cleared: {clearError}</span></p>{/if}
          {#if confirmClear}
            <div class="row clear-confirm">
              <button class="btn danger small" aria-disabled={app.busy} onclick={clearLog}>{clearError ? "Try clearing again" : "Clear the log for everyone"}</button>
              <button class="btn small keep" onclick={() => askClear(false)}>Keep it</button>
            </div>
          {:else}
            <div class="row"><button class="btn danger small clear-start" disabled={logState !== "ok"} onclick={() => askClear(true)}>Clear server log…</button></div>
          {/if}
        </section>
      </Detail>
    {/if}
  </div>
</div>

<style>
  .issues {
    display: grid;
    gap: 8px;
    padding: 12px 14px;
    border-bottom: 1px solid var(--line);
  }
  .issues ul {
    display: grid;
    gap: 6px;
  }
  .issues li {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    align-items: center;
    gap: 4px 12px;
    padding: 5px 10px 5px 12px;
    border-left: 3px solid var(--warn);
    background: var(--warn-wash);
  }
  .issues li.bad {
    border-left-color: var(--bad);
    background: var(--bad-wash);
  }
  .issues .what {
    display: grid;
    min-width: 0;
  }
  /* The reason wraps instead of being cut: it is read here, not in a tooltip. */
  .issues .what > span {
    overflow-wrap: anywhere;
  }
  .issues .what .meta {
    color: var(--ink-2);
  }
  @container (max-width: 560px) {
    /* The action goes under the words instead of beside them, so a long title never runs into it. */
    .issues li {
      grid-template-columns: auto minmax(0, 1fr);
      align-items: start;
    }
    .issues li > .dot {
      margin-top: 6px;
    }
    .issues li > .btn {
      grid-column: 2;
      justify-self: start;
    }
  }
  .activity {
    display: grid;
    gap: 16px;
    padding: 14px 14px 10px;
    border-bottom: 1px solid var(--line);
    min-width: 0;
  }
  .watched {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .hatch-key {
    flex: none;
    width: 14px;
    height: 10px;
    border: 1px solid var(--line-2);
    background: repeating-linear-gradient(135deg, var(--line-2) 0 2px, transparent 2px 5px);
  }
  .trends {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    gap: 14px 24px;
    min-width: 0;
  }
  @container (max-width: 820px) {
    .trends {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }
  @container (max-width: 420px) {
    .trends {
      gap: 12px 14px;
    }
  }
  /* The request table drops its least needed columns as its frame narrows, so it never scrolls sideways. */
  .requests .acct-under {
    display: none;
  }
  @container (max-width: 1000px) {
    .requests .c-wide {
      display: none;
    }
  }
  @container (max-width: 760px) {
    .requests .c-mid {
      display: none;
    }
  }
  @container (max-width: 600px) {
    .requests .c-acct {
      display: none;
    }
    .requests .acct-under {
      display: block;
    }
  }
  /* The narrowest phones: duration joins the account line under the model. */
  .requests .dur-under {
    display: none;
  }
  @container (max-width: 340px) {
    .requests .c-dur {
      display: none;
    }
    .requests .dur-under {
      display: inline;
    }
  }
  /* A model or account name, however long, wraps instead of widening every column. */
  .requests .model,
  .requests .acct-under,
  .requests .c-acct {
    overflow-wrap: anywhere;
    white-space: normal;
  }
  .stages {
    display: grid;
    gap: 4px;
  }
  .stages li {
    display: grid;
    grid-template-columns: minmax(0, 13.5em) minmax(40px, 1fr) 6.5em;
    gap: 10px;
    align-items: center;
    font-size: 13px;
  }
  .stages .track {
    height: 4px;
    border-radius: 2px;
    background: var(--sunk);
  }
  .stages .track i {
    display: block;
    height: 100%;
    min-width: 2px;
    border-radius: 2px;
    background: var(--ink-2);
  }
  .facts dt.sub {
    padding-left: 12px;
  }
  :global(.panel) .loglines .logline {
    grid-template-columns: 9ch minmax(0, 1fr);
    padding: 3px 8px;
  }
  :global(.panel) .loglines .lvl {
    display: none;
  }
  :global(.panel) .loglines .msg {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .end-note {
    text-align: right;
  }
</style>
