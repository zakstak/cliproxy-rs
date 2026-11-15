<script lang="ts">
  // Accounts: which ones work, which need attention, and when limits reset. Limits live here
  // and nowhere else; what an account consumed is on Usage, one link away.
  // Filters, search and sort narrow a long list; selected accounts take batch actions.
  import { Effect } from "effect";
  import { accountName as credName, accountWithProvider, isApiKey, keyHint, keyLink } from "../account";
  import { api } from "../api";
  import { download } from "../api";
  import { accountRow, canSignInAgain, label, provider, signIns, span, type Bar, type Data } from "../core";
  import { app, asData, message, Res, server } from "../app.svelte";
  import { canCheck, check, checkAll, hasSource, key, limits, wait } from "../limits";
  import { plural, when } from "../fmt";
  import { focusAfter, find, retried } from "../focus";
  import { confirmingReread, describe, runBatch } from "../batch";
  import Meter from "../parts/Meter.svelte";
  import AddAccount from "../parts/AddAccount.svelte";
  import Detail from "../parts/Detail.svelte";

  type State = "bad" | "warn" | "ok" | "off" | "idle";
  const rank: Record<State, number> = { bad: 0, warn: 1, ok: 2, off: 3, idle: 4 };
  const stateName: Record<State, string> = { ok: "Can serve now", warn: "Limited or cooling down", bad: "Needs attention", off: "Disabled", idle: "Configured (health unknown)" };
  const view = app.accountView;

  const all = $derived(
    (app.accounts.data || []).map((a) => {
      const l = limits(a);
      const v = accountRow(a, l, !!l && "from" in l && l.from === "check", hasSource(a), app.now);
      if (a.configured_only) v.word = "Configured";
      const state: State = a.configured_only ? "idle" : a.disabled ? "off" : v.attention ? "bad" : v.ready ? "ok" : "warn";
      return { a, v, l, state, k: key(a) };
    }),
  );
  type Row = (typeof all)[number];
  const providers = $derived(signIns.filter((p) => all.some((r) => provider(r.a) === p)));
  const needle = $derived(view.q.trim().toLowerCase());
  const matches = (r: Row) =>
    !needle || [credName(r.a), r.a.name, r.a.note, r.a.label, r.a.id_token?.plan_type, label(provider(r.a))].some((v) => typeof v === "string" && v.toLowerCase().includes(needle));
  const inProvider = $derived(all.filter((r) => view.provider === "all" || provider(r.a) === view.provider));
  const rows = $derived(
    inProvider
      .filter((r) => (view.status === "all" || r.state === view.status) && matches(r))
      .sort((x, y) => {
        const name = credName(x.a).localeCompare(credName(y.a));
        if (view.sort === "name") return name;
        if (view.sort === "provider") return provider(x.a).localeCompare(provider(y.a)) || name;
        if (view.sort === "reset") return x.v.next - y.v.next || name;
        return rank[x.state] - rank[y.state] || x.v.next - y.v.next || name;
      }),
  );
  const filtered = $derived(view.provider !== "all" || view.status !== "all" || !!needle);
  const count = (s: State) => all.filter((r) => r.state === s).length;
  const summary = $derived(
    (["ok", "warn", "bad", "off", "idle"] as State[])
      .filter((s) => count(s))
      .map((s) => (s === "bad" ? `${plural(count(s), "needs", "need")} attention` : `${count(s)} ${stateName[s].toLowerCase()}`))
      .join(", "),
  );
  let search = $state<HTMLInputElement>();
  function clearFilters() {
    view.provider = "all";
    view.status = "all";
    view.q = "";
    focusAfter(() => search);
  }

  // ---- the open account -------------------------------------------------------------------
  const openKey = $derived(app.route.rest[0] || "");
  const open = $derived(all.find((r) => r.k === openKey || r.a.name === openKey));
  const modal = $derived(!app.inline);
  const toggle = (a: Data) => app.go(openKey === key(a) ? "accounts" : `accounts/${encodeURIComponent(key(a))}`);
  const close = () => app.go("accounts");

  // Add, and sign in again, share one dialog. #accounts?add=1 (from first-run setup) opens it once.
  let adding = $state<{ target: Data | null } | null>(null);
  $effect(() => {
    if (app.route.query.get("add") === "1") {
      adding = { target: null };
      app.redirect("accounts");
    }
  });
  const subtitle = (a: Data) => [isApiKey(a) ? `${label(provider(a))} API key` : label(provider(a)), a.id_token?.plan_type && `${a.id_token.plan_type} plan`, a.note].filter(Boolean).join(", ");
  const meter = (b: Bar) => ("used" in b ? { used: b.used, note: b.reset, why: "" } : { used: null, note: "", why: b.why });

  // One failed action at a time is kept with what it was and whom it was for, so it can be read
  // and retried. A validation problem has no retry: the value must change first.
  let failure = $state<{ for: string; what: string; why: string; retry: (() => void) | null; field?: string } | null>(null);
  async function act(a: Data, what: string, work: () => Promise<unknown>, done: string) {
    failure = null;
    const why = await app.act(async () => {
      await work();
      await app.accounts.load(true);
    }, done);
    if (why) failure = { for: key(a), what, why, retry: () => act(a, what, work, done) };
    return !why;
  }
  const lookup = (a: Data) => ({ name: a.name, ...(a.auth_index ? { auth_index: a.auth_index } : {}) });
  const setDisabled = (a: Data, disabled: boolean) =>
    act(a, `${disabled ? "Disabling" : "Enabling"} ${accountWithProvider(a)}`, () => api("/credentials/status", "PATCH", { ...lookup(a), disabled }), `${accountWithProvider(a)} ${disabled ? "disabled" : "enabled"}.`);

  let checkingAll = $state(false);
  async function checkEvery() {
    if (checkingAll) return;
    checkingAll = true;
    await checkAll(app.accounts.data || []);
    checkingAll = false;
    app.notify("Limit checks finished.");
  }
  const checkLabel = (a: Data) => app.pending[key(a)] || (wait(a, app.now) ? `Check again in ${span(wait(a, app.now))}` : "Check limits");
  const checkError = (l: Row["l"]) => (l && "error" in l && l.error ? l.error : "");
  const shortError = (l: Row["l"]) => {
    const code = l && "code" in l && l.code ? `HTTP ${l.code}` : "";
    return code ? `Last check failed (${code})` : "Last check failed";
  };
  /** The reason in its first sentence, for the row; the detail has all of it. */
  const firstSentence = (text: string) => text.replace(/\s*\(HTTP \d+\)/, "").split(/(?<=\.)\s/)[0];
  /** A check runs one at a time and at most every 2 minutes; a button that cannot run says why, and keeps focus. */
  const checkBlocked = (a: Data, k: string) => !!app.pending[k] || !!wait(a, app.now);
  async function checkOne(a: Data) {
    const k = key(a);
    if (checkBlocked(a, k)) return;
    await check(a);
    const r = app.readings[k];
    app.notify(r?.error ? `Limit check for ${accountWithProvider(a)} failed: ${firstSentence(r.error)}` : `Limits read for ${accountWithProvider(a)}.`);
  }

  // Detail state for the open account.
  let fields = $state({ note: "", priority: "", weight: "", request_retry: "" });
  let confirmDelete = $state(false);
  let filledFor = "";
  let models = $state.raw<Res<string[]> | null>(null);
  // Fill the form when an account is opened, not on every refresh, so typing is not overwritten.
  $effect(() => {
    const a = open?.a;
    const k = a ? key(a) : "";
    if (k === filledFor) return;
    filledFor = k;
    confirmDelete = false;
    if (failure?.field) failure = null;
    if (!a || isApiKey(a)) return void (models = null);
    fields = { note: a.note || "", priority: String(a.priority ?? ""), weight: String(a.weight ?? ""), request_retry: String(a.request_retry ?? "") };
    const name = a.name;
    const res = new Res<string[]>(
      server.read(`/credentials/models?name=${encodeURIComponent(name)}`).pipe(Effect.map((b) => (asData(b).models || []).map((m: Data) => String(m.id)))),
    );
    models = res;
    res.load();
  });
  const fieldName: Record<string, string> = { priority: "Priority", weight: "Weight", request_retry: "Retries" };
  function saveFields(a: Data, e: SubmitEvent) {
    e.preventDefault();
    const patch: Data = { name: a.id || a.name, note: fields.note.trim() };
    for (const k of ["priority", "weight", "request_retry"] as const) {
      const v = fields[k].trim();
      if (v === "") {
        // An emptied field clears the override.
        if (a[k] !== undefined) patch[k] = null;
      } else if (!/^-?\d+$/.test(v)) {
        failure = { for: key(a), what: "Saving routing settings", why: `${fieldName[k]} must be a whole number, such as 2. Nothing was saved.`, retry: null, field: k };
        return void focusAfter(() => find(`#field-${k}`));
      } else patch[k] = Number(v);
    }
    act(a, "Saving routing settings", () => api("/credentials/fields", "PATCH", patch), "Routing settings saved.");
  }
  function askDelete(ask: boolean) {
    confirmDelete = ask;
    focusAfter(() => find(ask ? ".delete-confirm .keep" : ".delete-start"));
  }
  async function remove(a: Data) {
    const ok = await act(a, `Deleting ${accountWithProvider(a)}`, () => api("/credentials", "DELETE", { names: [a.name] }), `${accountWithProvider(a)} deleted.`);
    if (ok) {
      app.go("accounts");
      focusAfter(() => find(".acct-heading"));
    }
  }
  // ---- selection and batch actions -------------------------------------------------------
  // Selection is by account and survives filters; the bar says how many the filters hide.
  let sel = $state<string[]>([]);
  const picked = $derived(all.filter((r) => sel.includes(r.k)));
  const hiddenPicked = $derived(picked.filter((r) => !rows.includes(r)));
  const files = $derived(picked.filter((r) => !r.a.runtime_only));
  const keysPicked = $derived(picked.length - files.length);
  const allShown = $derived(rows.length > 0 && rows.every((r) => sel.includes(r.k)));
  const someShown = $derived(!allShown && rows.some((r) => sel.includes(r.k)));
  const pick = (k: string) => (sel = sel.includes(k) ? sel.filter((x) => x !== k) : [...sel, k]);
  const toggleAll = () => (sel = allShown ? sel.filter((k) => !rows.some((r) => r.k === k)) : [...new Set([...sel, ...rows.map((r) => r.k)])]);
  type Batch = { kind: "idle" } | { kind: "confirm-delete" } | { kind: "running"; what: string } | { kind: "done"; text: string; bad: boolean; failed: string[]; retry: (() => void) | null };
  let batch = $state<Batch>({ kind: "idle" });
  const latest = (k: string) => (app.accounts.data || []).find((a) => key(a) === k);
  async function runOn(what: string, units: Row[], run: (a: Data) => Promise<unknown>, verb: string, done?: (k: string) => boolean) {
    if (app.busy || !units.length) return;
    batch = { kind: "running", what };
    app.busy = true;
    let outcome: Awaited<ReturnType<typeof runBatch<string>>>;
    try {
      // With a readback, only a read that started after the change and answered may judge it.
      const reread = done ? confirmingReread(() => app.accounts.load(true)) : async () => void (await app.accounts.load(true));
      outcome = await runBatch(units.map((u) => u.k), (k) => run(latest(k) ?? units.find((u) => u.k === k)!.a), { reread, done });
    } catch (e) {
      app.busy = false;
      batch = { kind: "done", text: `${what} stopped: ${message(e)}`, bad: true, failed: units.map((u) => u.k), retry: null };
      return;
    }
    app.busy = false;
    const name = (k: string) => accountWithProvider(units.find((u) => u.k === k)!.a);
    // Failed accounts stay selected, so a retry acts on exactly them.
    sel = sel.filter((k) => !outcome.ok.includes(k) && !!latest(k));
    const failedUnits = units.filter((u) => outcome.failed.some((f) => f.unit === u.k));
    batch = {
      kind: "done",
      text: describe(outcome, name, verb),
      bad: outcome.failed.length > 0,
      failed: outcome.failed.map((f) => f.unit),
      retry: outcome.failed.length ? () => runOn(what, failedUnits, run, verb, done) : null,
    };
    if (!outcome.failed.length) app.notify(batch.text);
    focusAfter(() => find(".batch-result"));
  }
  const toEnable = $derived(files.filter((r) => r.a.disabled));
  const toDisable = $derived(files.filter((r) => !r.a.disabled));
  const enableAll = () => runOn("Enabling", toEnable, (a) => api("/credentials/status", "PATCH", { ...lookup(a), disabled: false }), "enabled", (k) => latest(k)?.disabled === false);
  const disableAll = () => runOn("Disabling", toDisable, (a) => api("/credentials/status", "PATCH", { ...lookup(a), disabled: true }), "disabled", (k) => latest(k)?.disabled === true);
  const renewAll = () => runOn("Renewing tokens", files, (a) => api("/credentials/refresh", "POST", lookup(a)), "renewed");
  const deleteAll = () => runOn("Deleting", files, (a) => api("/credentials", "DELETE", { names: [a.name] }), "deleted", (k) => !latest(k));
  function askBatchDelete(ask: boolean) {
    batch = ask ? { kind: "confirm-delete" } : { kind: "idle" };
    focusAfter(() => find(ask ? ".batch .keep" : ".batch .delete-many"));
  }
  function clearSelection() {
    sel = [];
    batch = { kind: "idle" };
    focusAfter(() => find(".select-all"), () => search);
  }
  const cards = $derived(!app.wide);
  /** On a phone, state and sort sit behind one button so the list starts higher. */
  let moreOpen = $state(false);
</script>

{#snippet status(r: Row)}
  <span class="state-line"><span class="dot {r.state}"></span><span>{r.v.word}</span></span>
  {#if r.v.sub && r.state !== "ok"}<span class="small muted reason">{r.v.sub}</span>{/if}
  {#if checkError(r.l)}<span class="small check-failed"><span class="code-status bad">{shortError(r.l)}:</span> {firstSentence(checkError(r.l))}</span>{/if}
{/snippet}

{#snippet action(r: Row)}
  {@const a = r.a}
  {#if r.state === "bad" && canSignInAgain(a)}
    <button class="btn small primary" onclick={() => (adding = { target: a })}>Sign in again<span class="sr"> to {accountWithProvider(a)}</span></button>
  {:else if a.disabled && !a.runtime_only}
    <button class="btn small" aria-disabled={app.busy} onclick={() => setDisabled(a, false)}>Enable<span class="sr"> {accountWithProvider(a)}</span></button>
  {:else if canCheck(a)}
    <button class="btn small" aria-disabled={checkBlocked(a, r.k)} onclick={() => checkOne(a)}>{checkLabel(a)}<span class="sr"> for {accountWithProvider(a)}</span></button>
  {/if}
{/snippet}

<div class="page fill accounts-page">
  <div class="head">
    <h1 class="acct-heading" tabindex="-1">Accounts</h1>
    <button class="btn" aria-disabled={checkingAll || !all.some((r) => canCheck(r.a) && !wait(r.a, app.now))} aria-describedby="check-all-help" onclick={checkEvery}>
      {checkingAll ? "Checking limits…" : "Check all limits"}
    </button>
    <span class="sr" id="check-all-help">Asks each provider once, one account at a time. Each account at most once every 2 minutes.</span>
    <button class="btn primary add-account" onclick={() => (adding = { target: null })}>Add account</button>
  </div>

  {#if app.accounts.error}
    <p class="notice bad small" role="alert" style="margin-bottom:10px">
      <span class="dot bad"></span>
      <span class="grow">Could not refresh accounts: {message(app.accounts.error)}{app.accounts.at ? ` This list is from ${when(app.accounts.at)}.` : ""}</span>
      <button class="btn small" aria-disabled={app.accounts.loading} onclick={() => retried(app.accounts.load().then((r) => r.ok), () => find(".acct-heading"), () => app.notify("Accounts read again."))}>{app.accounts.loading ? "Reading…" : "Try again"}</button>
    </p>
  {/if}
  {#if failure && (failure.for !== openKey || !open)}
    {@const who = all.find((r) => r.k === failure?.for)}
    <p class="notice bad small" role="alert" style="margin-bottom:10px">
      <span class="dot bad"></span><span class="grow">{failure.what} failed{who && !failure.what.includes(accountWithProvider(who.a)) ? ` for ${accountWithProvider(who.a)}` : ""}: {failure.why}</span>
      {#if failure.retry}<button class="btn small" aria-disabled={app.busy} onclick={failure.retry}>Try again</button>{/if}<button class="btn small quiet" onclick={() => (failure = null)}>Dismiss</button>
    </p>
  {/if}

  {#if !app.accounts.data}
    <p class="empty muted">{app.accounts.error ? "" : "Reading accounts…"}</p>
  {:else if !all.length}
    <div class="empty">
      <h2>No accounts yet</h2>
      <p class="muted">The proxy answers your tools with the accounts you add here: a Claude, Codex, xAI or Meta subscription you sign in to, or a provider API key.</p>
      <div class="row">
        <button class="btn primary" onclick={() => (adding = { target: null })}>Add an account</button>
        <a class="btn" href="#tools">Create a client key</a>
      </div>
    </div>
  {:else}
    <div class="acct-tools">
      <div class="seg" role="group" aria-label="Provider">
        <button aria-pressed={view.provider === "all"} onclick={() => (view.provider = "all")}>All <span class="n">{all.length}</span></button>
        {#each providers as p (p)}<button aria-pressed={view.provider === p} onclick={() => (view.provider = p)}>{label(p)} <span class="n">{all.filter((r) => provider(r.a) === p).length}</span></button>{/each}
      </div>
      <label class="search">
        <span class="sr">Search accounts</span>
        <input type="search" bind:this={search} bind:value={view.q} placeholder="Search name, note or file" />
      </label>
      <button class="btn more-toggle" aria-expanded={moreOpen} aria-controls="acct-more" onclick={() => (moreOpen = !moreOpen)}>
        Filter and sort{#if view.status !== "all" || view.sort !== "attention"}<span class="dot live" aria-hidden="true"></span><span class="sr"> (changed)</span>{/if}
      </button>
      <div class="acct-more" class:open={moreOpen} id="acct-more">
      <label class="inline-field">
        <span>Show</span>
        <select bind:value={view.status}>
          <option value="all">Any state ({inProvider.length})</option>
          {#each ["bad", "warn", "ok", "off", "idle"] as const as s}<option value={s}>{stateName[s]} ({inProvider.filter((r) => r.state === s).length})</option>{/each}
        </select>
      </label>
      <label class="inline-field">
        <span>Sort</span>
        <select bind:value={view.sort}>
          <option value="attention">Needs attention first</option>
          <option value="reset">Soonest weekly reset</option>
          <option value="name">Name</option>
          <option value="provider">Provider</option>
        </select>
      </label>
      </div>
    </div>
    <p class="small muted summary" role="status">
      {plural(all.length, "account")}: {summary}.{filtered ? ` Showing ${rows.length}.` : ""}
      {#if filtered}<button class="link" onclick={clearFilters}>Clear filters</button>{/if}
    </p>

    {#if picked.length || batch.kind === "done"}
      <div class="batch" role="region" aria-label="Selected accounts">
        {#if picked.length}
          <p class="grow">
            <strong>{picked.length} selected.</strong>
            {#if hiddenPicked.length}<span class="muted">{hiddenPicked.length} {hiddenPicked.length === 1 ? "is" : "are"} hidden by the filters.</span>{/if}
            {#if keysPicked}<span class="muted">{plural(keysPicked, "API key")} {keysPicked === 1 ? "is" : "are"} left out: manage {keysPicked === 1 ? "it" : "them"} in <a href="#settings/keys">Provider keys</a>.</span>{/if}
          </p>
          {#if batch.kind === "confirm-delete"}
            <div class="confirm-many" role="group" aria-label="Confirm delete">
              <p class="small">Delete {plural(files.length, "account")} from the server: {files.map((r) => accountWithProvider(r.a)).join(", ")}? Sign-ins at the providers are not revoked.</p>
              <div class="row">
                <button class="btn small danger" aria-disabled={app.busy} onclick={deleteAll}>Delete {files.length}</button>
                <button class="btn small keep" onclick={() => askBatchDelete(false)}>Keep them</button>
              </div>
            </div>
          {:else}
            <div class="row">
              {#if toEnable.length}<button class="btn small" aria-disabled={app.busy} onclick={enableAll}>Enable {toEnable.length}</button>{/if}
              {#if toDisable.length}<button class="btn small" aria-disabled={app.busy} onclick={disableAll}>Disable {toDisable.length}</button>{/if}
              {#if files.length}<button class="btn small" aria-disabled={app.busy} onclick={renewAll}>Renew tokens for {files.length}</button>{/if}
              {#if files.length}<button class="btn small danger delete-many" aria-disabled={app.busy} onclick={() => askBatchDelete(true)}>Delete {files.length}…</button>{/if}
              <button class="btn small quiet" onclick={clearSelection}>Clear selection</button>
            </div>
          {/if}
        {/if}
        {#if batch.kind === "running"}<p class="small" role="status"><span class="dot live pulse"></span> {batch.what}…</p>{/if}
        {#if batch.kind === "done"}
          <p class="small batch-result" class:code-status={batch.bad} class:bad={batch.bad} role={batch.bad ? "alert" : "status"} tabindex="-1">
            {batch.text}{batch.bad && batch.failed.length ? " The failed accounts are still selected." : ""}
          </p>
          <div class="row">
            {#if batch.retry}<button class="btn small" aria-disabled={app.busy} onclick={batch.retry}>Try the failed ones again</button>{/if}
            {#if !picked.length}<button class="btn small quiet" onclick={() => (batch = { kind: "idle" })}>Dismiss</button>{/if}
          </div>
        {/if}
      </div>
    {/if}

    <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions: only the backdrop lands here -->
    <div class="split accounts" class:open={!!open && !modal}>
      <div class="list">
        {#if !rows.length}
          <div class="empty">
            <h2>No accounts match</h2>
            <p class="muted">{plural(all.length, "account")} in total; none match the provider, state and search chosen.</p>
            <button class="btn" onclick={clearFilters}>Clear filters</button>
          </div>
        {:else if cards}
          <div class="scroll">
            <div class="select-row">
              <label class="check"><input type="checkbox" class="select-all" checked={allShown} indeterminate={someShown} onchange={toggleAll} />Select all {rows.length} shown</label>
            </div>
            <ul class="acct-cards" aria-label="Accounts">
              {#each rows as r (r.k)}
                {@const a = r.a}
                <li class="acct-card" class:sel={openKey === r.k} class:dim={a.disabled} class:checked={sel.includes(r.k)}>
                  <label class="pick-hit" style="grid-area: pick"><input type="checkbox" aria-label={`Select ${accountWithProvider(a)}`} checked={sel.includes(r.k)} onchange={() => pick(r.k)} /></label>
                  <button class="card-open" data-acct={r.k} aria-expanded={openKey === r.k} onclick={() => toggle(a)}>
                    <strong>{credName(a)}</strong>
                    <span class="small muted">{subtitle(a)}</span>
                  </button>
                  <div class="card-status">{@render status(r)}</div>
                  {#if "why" in r.v.session && "why" in r.v.weekly && r.v.session.why === r.v.weekly.why}
                    <p class="card-limits small muted">Limits: {r.v.session.why}{r.v.session.why === "no usage source" ? `. ${label(provider(a))}${isApiKey(a) ? " API keys report" : " reports"} no usage the proxy can read.` : "."}</p>
                  {:else}
                    <div class="card-limits">
                      <Meter name="Session" {...meter(r.v.session)} stale={r.v.stale} />
                      <Meter name="Weekly" {...meter(r.v.weekly)} stale={r.v.stale} />
                    </div>
                  {/if}
                  <div class="card-foot">
                    <span class="small muted">{hasSource(a) || (r.l && "windows" in r.l) ? r.v.age : ""}</span>
                    {@render action(r)}
                  </div>
                </li>
              {/each}
            </ul>
          </div>
        {:else}
          <div class="scroll">
            <table class="acct-table">
              <caption class="sr">Accounts, {view.sort === "attention" ? "accounts that need you first" : `sorted by ${view.sort}`}</caption>
              <thead>
                <tr>
                  <th scope="col" class="pickcol"><label class="pick-hit"><input type="checkbox" class="select-all" aria-label={`Select all ${rows.length} shown`} checked={allShown} indeterminate={someShown} onchange={toggleAll} /></label></th>
                  <th scope="col">Account</th><th scope="col">Status</th><th scope="col" class="c-lim">Session limit</th><th scope="col" class="c-lim">Weekly limit</th><th scope="col" class="c-read">Limits read</th><th scope="col" class="num"><span class="sr">Action</span></th>
                </tr>
              </thead>
              <tbody>
                {#each rows as r (r.k)}
                  {@const a = r.a}
                  <!-- The name is the row's button; a click elsewhere on the row, outside its controls, does the same. -->
                  <tr class="clickable" class:sel={openKey === r.k} class:dim={a.disabled} onclick={(e) => e.target instanceof Element && !e.target.closest("button, a, input, label") && toggle(a)}>
                    <td class="pickcol"><label class="pick-hit"><input type="checkbox" aria-label={`Select ${accountWithProvider(a)}`} checked={sel.includes(r.k)} onchange={() => pick(r.k)} /></label></td>
                    <td>
                      <button class="cellbtn name" data-acct={r.k} aria-expanded={openKey === r.k} onclick={() => toggle(a)}><strong>{credName(a)}</strong></button>
                      <div class="small muted">{subtitle(a)}</div>
                    </td>
                    <td>
                      {@render status(r)}
                      <span class="small muted lim-under">{["used" in r.v.session && `Session ${Math.round(r.v.session.used)}%`, "used" in r.v.weekly && `weekly ${Math.round(r.v.weekly.used)}%`].filter(Boolean).join(", ") || ("why" in r.v.weekly ? `Limits: ${r.v.weekly.why}` : "")}</span>
                      {#if hasSource(a) || (r.l && "windows" in r.l)}<span class="small muted read-under">{r.v.age}</span>{/if}
                    </td>
                    <td class="c-lim"><Meter {...meter(r.v.session)} stale={r.v.stale} /></td>
                    <td class="c-lim"><Meter {...meter(r.v.weekly)} stale={r.v.stale} /></td>
                    <td class="c-read small muted">{hasSource(a) || (r.l && "windows" in r.l) ? r.v.age : "no limit source"}</td>
                    <td class="num">{@render action(r)}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        {/if}
      </div>

      {#if open}
        {@const a = open.a}
        {@const v = open.v}
        {@const l = open.l}
        <Detail label={`${credName(a)} details`} {modal} item={open.k} onclose={close} returnTo={[`[data-acct="${CSS.escape(open.k)}"]`, ".acct-heading"]}>
          {#snippet head()}
            <h2 data-detail-title tabindex="-1">{credName(a)}</h2>
            <p class="small muted">{subtitle(a)}</p>
          {/snippet}
          {#if failure && failure.for === openKey}
            <p class="notice bad small" role="alert">
              <span class="dot bad"></span><span class="grow">{failure.what} failed: {failure.why}</span>
              {#if failure.retry}<button class="btn small" aria-disabled={app.busy} onclick={failure.retry}>Try again</button>{/if}
            </p>
          {/if}
          {#if isApiKey(a)}
            <section>
              <dl class="facts">
                <dt>Key</dt><dd class="mono">{keyHint(a)}</dd>
                <dt>Group</dt><dd>{a.label || "No name"}</dd>
                <dt>ID</dt><dd class="mono">{String(a.auth_index).slice(0, 8)}</dd>
              </dl>
              <p><a class="btn small" href={keyLink(a)}>Manage this key in Provider keys</a></p>
              <p class="small muted wrap-anywhere">Provider keys lists it as <span class="mono">{keyHint(a)}</span> in {a.label || "its group"}; removing or disabling it is done there.</p>
            </section>
          {/if}
          <section>
            <p class="row"><span class="dot {open.state}"></span><strong>{v.word}</strong></p>
            {#if a.status_message}<p class="small">{a.status_message}</p>{/if}
            {#if open.state === "bad" && canSignInAgain(a)}
              <p class="small muted">The provider no longer accepts this account's token. Signing in again replaces it in the same file and keeps the account's settings.</p>
            {/if}
            {#each a.cooldowns || [] as c}
              <p class="small muted">
                {c.model_key ? `Model ${c.model_key}` : "The whole account"} is waiting{c.http_status ? ` after HTTP ${c.http_status}` : ""}: next try {Date.parse(c.retry_at) > app.now ? `in ${span(Date.parse(c.retry_at) - app.now)}` : "on the next request"}{c.recover_at ? `, limit resets ${when(Date.parse(c.recover_at))}` : ""}.
              </p>
            {/each}
            <div class="row">
              {#if canSignInAgain(a)}<button class="btn small" class:primary={open.state === "bad"} onclick={() => (adding = { target: a })}>Sign in again</button>{/if}
              {#if a.cooldowns?.length}
                <button class="btn small" aria-disabled={app.busy} title="Clears the proxy's own wait. It does not restore quota at the provider." onclick={() => act(a, "Clearing the cooldown", () => api("/routing/cooldown/reset", "POST", { auth_index: a.auth_index }), "Cooldown cleared.")}>Try this account again now</button>
              {/if}
              {#if !a.runtime_only}<button class="btn small" aria-disabled={app.busy} onclick={() => setDisabled(a, !a.disabled)}>{a.disabled ? "Enable" : "Disable"}</button>{/if}
            </div>
          </section>

          <section>
            <h3>Limits</h3>
            {#if l && "windows" in l}
              {#each l.windows as w}
                <Meter name={w.label} used={w.used} stale={v.stale} why={w.detail} note={Date.parse(w.reset) > app.now ? `resets ${when(Date.parse(w.reset))}, in ${span(Date.parse(w.reset) - app.now)}` : w.detail || ""} />
              {/each}
              <p class="small muted">
                {l.from === "check" ? "Read from the provider" : "Seen by the server in a response"}{l.at ? ` ${when(l.at)}` : ""}{v.stale ? ". The account has not worked since, so this may be out of date" : ""}.
              </p>
            {:else if a.disabled}
              <p class="small muted">Not read: the account is disabled.</p>
            {:else if !hasSource(a)}
              <p class="small muted">{label(provider(a))}{a.account_type === "apikey" ? " API keys have" : " has"} no usage endpoint the proxy can read, so limits are unknown. A limit shows up as a status when the provider refuses a request.</p>
            {:else if !checkError(l)}
              <p class="small muted">Not read yet. The server also picks limits up from response headers once the account serves a request.</p>
            {/if}
            {#if checkError(l)}
              <p class="notice bad small" role="alert"><span class="dot bad"></span><span>The last check failed: {checkError(l)}{l && "windows" in l ? " The limits above are the last good reading." : ""}</span></p>
            {/if}
            {#if canCheck(a)}
              <div class="row">
                <button class="btn small" aria-disabled={checkBlocked(a, open.k)} onclick={() => checkOne(a)}>{checkLabel(a)}</button>
                {#if wait(a, app.now) && !app.pending[open.k]}<span class="small muted">One check per account every 2 minutes, so the provider is not asked too often.</span>{/if}
              </div>
            {/if}
          </section>

          <section>
            <h3 class="acct-models" tabindex="-1">Models</h3>
            {#if isApiKey(a)}<p class="small muted">Model routing for this key is configured under <a href={keyLink(a)}>Provider keys</a>. The server does not expose a per-key model list here.</p>
            {:else if !models || models.loading && !models.data}<p class="small muted">Reading the models this account offers…</p>
            {:else if models.error}<p class="notice bad small" role="alert"><span class="dot bad"></span><span class="grow">Could not read models: {message(models.error)}</span><button class="btn small" onclick={() => models && retried(models.load().then((r) => r.ok), () => find(".acct-models"), () => app.notify("Models read."))}>Try again</button></p>
            {:else if models.data?.length}<ul class="chips">{#each models.data as m}<li class="chip mono">{m}</li>{/each}</ul>
            {:else}<p class="small muted">The server reports no models for this account.</p>{/if}
          </section>

          <section>
            <h3>Activity</h3>
            <ul class="grid">
              <li><a class="flex min-h-11 items-center min-[861px]:min-h-7" href={`#logs/requests?account=${encodeURIComponent(a.auth_index)}`}>Live requests from this account</a></li>
              <li><a class="flex min-h-11 items-center min-[861px]:min-h-7" href={`#usage?account=${encodeURIComponent(a.auth_index)}`}>Usage for this account</a></li>
            </ul>
          </section>

          <details>
            <summary class="small">{a.runtime_only ? "Limit headers and where to manage this key" : "Routing, file and removal"}</summary>
            <div class="stack" style="gap:18px">
              {#if !a.runtime_only}
                <form class="stack" style="gap:8px" onsubmit={(e) => saveFields(a, e)} novalidate>
                  <h3>Routing</h3>
                  <label class="field">Note<input type="text" bind:value={fields.note} /></label>
                  <div class="three">
                    {#each ["priority", "weight", "request_retry"] as const as k}
                      <label class="field">{fieldName[k]}<input
                          id={`field-${k}`}
                          type="text"
                          inputmode="numeric"
                          bind:value={fields[k]}
                          placeholder={k === "priority" ? "0" : k === "weight" ? "1" : "Global"}
                          aria-invalid={failure?.field === k}
                          aria-describedby={failure?.field === k ? "field-error" : undefined}
                        /></label>
                    {/each}
                  </div>
                  {#if failure?.field && failure.for === openKey}<p class="small code-status bad" id="field-error">{failure.why}</p>{/if}
                  <div class="row"><button class="btn small" aria-disabled={app.busy}>Save routing settings</button></div>
                </form>
                <section>
                  <h3>Credential file</h3>
                  <dl class="facts small">
                    <dt>File</dt><dd class="mono">{a.name}</dd>
                    <dt>Auth index</dt><dd class="mono">{a.auth_index}</dd>
                    {#if a.last_refresh}<dt>Token renewed</dt><dd>{when(Date.parse(a.last_refresh))}</dd>{/if}
                    <dt>Lifetime</dt><dd>{a.success === undefined ? "Not reported" : `${a.success} succeeded, ${a.failed || 0} failed`}</dd>
                  </dl>
                  <div class="row">
                    <button class="btn small" aria-disabled={app.busy} onclick={() => act(a, "Renewing the token", () => api("/credentials/refresh", "POST", lookup(a)), "Token renewed.")}>Renew token now</button>
                    <button class="btn small" onclick={() => act(a, "Downloading the file", async () => download(await api(`/credentials/download?name=${encodeURIComponent(a.name)}`, "GET", undefined, "blob"), a.name), "")} title="The file holds the account's tokens.">Download file</button>
                  </div>
                </section>
              {:else}
                <p class="small muted">This API key is part of the configuration. Its endpoint, weight and the other keys of its group are in Provider keys.</p>
              {/if}
              {#if Object.keys(a.quota?.signals || {}).length}
                <section>
                  <h3>Limit headers the server saw</h3>
                  <pre class="code">{Object.entries(a.quota.signals).map(([k, val]) => `${k}: ${val}`).join("\n")}</pre>
                </section>
              {/if}
              {#if !a.runtime_only}<section>
                <h3>Delete this account</h3>
                <p class="small muted">Deletes {a.name} from the server. The sign-in at the provider is not revoked.</p>
                {#if confirmDelete}
                  <div class="row delete-confirm">
                    <button class="btn danger small" aria-disabled={app.busy} onclick={() => remove(a)}>Delete {credName(a)}</button>
                    <button class="btn small keep" onclick={() => askDelete(false)}>Keep it</button>
                  </div>
                {:else}
                  <div class="row"><button class="btn danger small delete-start" onclick={() => askDelete(true)}>Delete account…</button></div>
                {/if}
              </section>{/if}
            </div>
          </details>
        </Detail>
      {/if}
    </div>
  {/if}
</div>

{#if adding}
  {#key adding}
    <AddAccount
      target={adding.target}
      onclose={(opened) => {
        adding = null;
        if (opened) app.go(`accounts/${encodeURIComponent(key(opened))}`);
        else focusAfter(() => find(".add-account"));
      }}
    />
  {/key}
{/if}

<style>
  .acct-tools {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 10px;
    margin-bottom: 8px;
  }
  .acct-tools .search {
    flex: 1 1 200px;
    max-width: 320px;
  }
  .acct-tools .search input {
    width: 100%;
  }
  .acct-more {
    display: contents;
  }
  .more-toggle {
    display: none;
  }
  @media (max-width: 760px) {
    .more-toggle {
      display: inline-flex;
    }
    .acct-more {
      display: none;
    }
    .acct-more.open {
      display: flex;
      flex-wrap: wrap;
      gap: 8px;
      flex-basis: 100%;
    }
    .acct-tools .search {
      flex: 1 1 150px;
      max-width: none;
    }
    /* The page scrolls as a whole on a phone; a list in its own frame would leave a sliver. */
    .accounts-page {
      height: auto !important;
      padding-bottom: calc(40px + var(--bottom)) !important;
    }
    .accounts-page .split,
    .accounts-page .scroll {
      overflow: visible;
    }
  }
  .inline-field {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    color: var(--ink-2);
  }
  .inline-field select {
    color: var(--ink);
  }
  .seg .n {
    margin-left: 4px;
    font-size: 12px;
    opacity: 0.8;
  }
  .summary {
    margin-bottom: 10px;
  }
  .batch {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 12px;
    margin-bottom: 10px;
    padding: 10px 12px;
    border: 1px solid var(--line-2);
    border-radius: 8px;
    background: var(--surface);
  }
  .batch > p {
    min-width: 0;
  }
  .confirm-many {
    display: grid;
    gap: 8px;
    flex-basis: 100%;
  }
  .batch-result {
    flex-basis: 100%;
  }
  .batch-result:focus {
    outline: none;
  }
  td {
    padding-top: 9px;
    padding-bottom: 9px;
  }
  .pickcol {
    width: 40px;
    padding-right: 0;
  }
  /* A long account name wraps in its cell instead of widening the table. */
  .acct-table .name {
    min-width: 0;
    text-align: left;
  }
  .acct-table .name strong {
    overflow-wrap: anywhere;
  }
  /* A disabled account reads quieter in --ink-2, which keeps 4.5:1 on every row background. */
  tr.dim td:nth-child(2) strong {
    color: var(--ink-2);
    font-weight: 560;
  }
  td .state-line {
    display: flex;
    align-items: center;
    gap: 7px;
    white-space: nowrap;
  }
  td .reason,
  td .check-failed,
  .lim-under,
  .read-under {
    display: block;
    max-width: 28ch;
  }
  .lim-under,
  .read-under {
    display: none;
  }
  /* As the table's frame narrows, the limit and read-time columns fold into the status cell. */
  @container (max-width: 1040px) {
    .acct-table .c-read {
      display: none;
    }
    .acct-table .read-under {
      display: block;
    }
  }
  @container (max-width: 760px) {
    .acct-table .c-lim {
      display: none;
    }
    .acct-table .lim-under {
      display: block;
    }
  }
  .three {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 8px;
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .chip {
    padding: 2px 8px;
    border: 1px solid var(--line-2);
    border-radius: 999px;
  }

  /* Cards: phones in every option, and Console at every width. A real list, each card one account. */
  .select-row {
    padding: 8px 12px;
    border-bottom: 1px solid var(--line);
  }
  .acct-cards {
    display: grid;
  }
  .acct-card {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    grid-template-areas:
      "pick open open"
      ". status status"
      ". limits limits"
      ". foot foot";
    gap: 6px 10px;
    padding: 10px 12px;
    border-bottom: 1px solid var(--line);
  }
  .acct-card.sel {
    background: var(--sunk);
    box-shadow: inset 3px 0 0 var(--accent);
  }
  .acct-card.dim .card-open strong {
    color: var(--ink-2);
  }
  /* The checkbox's whole cell is its target: 44px on a phone or tablet, 32px with a mouse. */
  .pick-hit {
    display: grid;
    place-items: center;
    width: 32px;
    height: 32px;
    margin: -4px;
    cursor: pointer;
  }
  @media (max-width: 860px), (pointer: coarse) {
    .pick-hit {
      width: 44px;
      height: 44px;
      margin: -10px -10px -10px -12px;
    }
  }
  .card-open {
    grid-area: open;
    display: grid;
    align-content: center;
    gap: 1px;
    min-width: 0;
    min-height: 44px;
    padding: 0;
    border: 0;
    background: none;
    text-align: left;
    cursor: pointer;
  }
  .card-open strong {
    overflow-wrap: anywhere;
  }
  .check-failed {
    display: block;
  }
  .card-status {
    grid-area: status;
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 2px 10px;
  }
  .card-status .state-line {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .card-limits {
    grid-area: limits;
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 10px;
  }
  p.card-limits {
    display: block;
  }
  .card-limits :global(.meter) {
    min-width: 0;
  }
  .card-foot:has(> :only-child:empty) {
    display: none;
  }
  .card-foot {
    grid-area: foot;
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 6px 10px;
  }
</style>
