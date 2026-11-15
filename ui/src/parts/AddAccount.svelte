<script lang="ts">
  // Adding an account, or signing in again to one. One dialog for the whole flow, opened from
  // Accounts, so a failure keeps the provider and the account it was for and can be retried.
  import { onMount, onDestroy, tick, untrack } from "svelte";
  import { api } from "../api";
  import { authUrl, label, mask, provider, signIns, span, type Data } from "../core";
  import { accountName as credName, providerEndpoint } from "../account";
  import { app, message } from "../app.svelte";
  import { configWrite, explain, type ConfigWrite } from "../server";
  import { canCheck, check, key, limits, wait } from "../limits";
  import Copy from "./Copy.svelte";
  import Meter from "./Meter.svelte";

  const props: { target?: Data | null; onclose: (opened?: Data) => void } = $props();
  // The dialog is for one account (or a new one) for its whole life; the parent remounts it for another.
  const target = untrack(() => props.target ?? null);
  const onclose = (opened?: Data) => props.onclose(opened);

  type Session = { state: string; url: string; code: string; device: boolean; expires: number };
  type Step =
    | { kind: "choose" }
    | { kind: "key" }
    | { kind: "starting" }
    | { kind: "waiting"; session: Session }
    | { kind: "verifying" }
    | { kind: "done"; account: Data | null }
    | { kind: "failed"; why: string };

  let dialog = $state<HTMLDialogElement>();
  let step = $state<Step>(target ? { kind: "starting" } : { kind: "choose" });
  let p = $state(target ? provider(target) : "claude");
  let method = $state<"signin" | "key">("signin");
  let callback = $state("");
  let callbackError = $state("");
  let form = $state({ name: "", key: "", url: "" });
  let formError = $state("");
  // Whose page the user approves on.
  const owner = $derived(p === "codex" ? "OpenAI" : label(p));
  const P = $derived(label(p));
  let before = new Set<string>();
  let alive = true;
  let pollingError = $state("");
  onDestroy(() => {
    alive = false;
    if (step.kind === "waiting") api(`/oauth/session?state=${encodeURIComponent(step.session.state)}`, "DELETE").catch(() => {});
  });

  onMount(() => {
    dialog?.showModal();
    // Start at the first choice, not at the header's Close.
    if (target) start();
    else dialog?.querySelector<HTMLElement>('input[name="provider"]:checked')?.focus();
  });

  // Each step replaces the last one's controls, including the one that had focus. After the
  // new step renders, focus its lead line, which also names the step to a screen reader.
  let shownStep = "";
  $effect(() => {
    const kind = step.kind;
    if (kind === shownStep) return;
    const first = !shownStep;
    shownStep = kind;
    if (first && kind === "choose") return;
    tick().then(() => {
      const lead = kind === "key" ? "form input" : kind === "choose" ? 'input[name="provider"]:checked' : "[data-step-lead]";
      dialog?.querySelector<HTMLElement>(lead)?.focus();
    });
  });
  /** A server message as a sentence: capital first letter, full stop at the end. */
  const sentence = (text: string) => {
    const t = text.trim();
    return t ? `${t[0].toUpperCase()}${t.slice(1)}${/[.!?]$/.test(t) ? "" : "."}` : "";
  };
  /** One line per step for screen readers; the step's own content is not a live region. */
  const announce = $derived(
    step.kind === "starting"
      ? `Asking the server for a ${P} sign-in address.`
      : step.kind === "waiting"
        ? `Waiting for you to approve on ${owner}'s page.`
        : step.kind === "verifying"
          ? "Approved. Reading the account's limits."
          : step.kind === "done"
            ? target
              ? "Signed in again."
              : `${P} account connected.`
            : step.kind === "failed"
              ? `${target ? "Not signed in" : "No account was added"}. ${sentence(step.why)}`
              : "",
  );


  async function start() {
    step = { kind: "starting" };
    callback = callbackError = "";
    try {
      before = new Set((app.accounts.data || []).map((a) => a.name));
      const r = await api(authUrl(p, target ?? undefined));
      if (!alive) {
        await api(`/oauth/session?state=${encodeURIComponent(String(r.state))}`, "DELETE").catch(() => {});
        return;
      }
      const url = new URL(String(r.url), location.href);
      if (!/^https?:$/.test(url.protocol)) throw new Error("The server returned an unsafe sign-in address.");
      step = {
        kind: "waiting",
        session: { state: String(r.state), url: url.href, code: typeof r.user_code === "string" ? r.user_code : "", device: r.flow === "device", expires: r.expires_in ? Date.now() + r.expires_in * 1000 : 0 },
      };
    } catch (e) {
      step = { kind: "failed", why: message(e) };
    }
  }
  // While waiting, ask the server every 2 seconds how the sign-in ended.
  $effect(() => {
    if (step.kind !== "waiting") return;
    const s = step.session;
    let busy = false;
    const timer = setInterval(async () => {
      if (busy || document.hidden) return;
      busy = true;
      const r = await api(`/oauth/status?state=${encodeURIComponent(s.state)}`).catch((e) => ({ status: "unreachable", error: message(e) }));
      busy = false;
      // An answer for a sign-in that was cancelled or replaced meanwhile is ignored.
      if (!alive || step.kind !== "waiting" || step.session !== s) return;
      pollingError = r.status === "unreachable" ? String(r.error) : "";
      if (r.status === "wait" || r.status === "unreachable") return;
      if (r.status !== "ok") return void (step = { kind: "failed", why: String(r.error || "The sign-in did not finish.") });
      finish();
    }, 2000);
    return () => clearInterval(timer);
  });
  /** After the server confirms: find the account and read its limits once. */
  async function finish() {
    step = { kind: "verifying" };
    await app.accounts.load(true);
    if (!alive) return;
    const list = app.accounts.data || [];
    const fresh = list.filter((a) => !before.has(a.name) && provider(a) === p);
    const account = target ? list.find((a) => a.name === target.name && a.auth_index === target.auth_index) : fresh.length === 1 ? fresh[0] : undefined;
    if (account && canCheck(account) && !wait(account)) await check(account);
    if (alive) step = { kind: "done", account: account ?? null };
  }
  async function cancel() {
    if (step.kind !== "waiting") return true;
    try {
      const r = await api(`/oauth/session?state=${encodeURIComponent(step.session.state)}`, "DELETE");
      if (r.cancelled === false) {
        const ended = await api(`/oauth/status?state=${encodeURIComponent(step.session.state)}`);
        if (ended.status === "ok") await finish();
        else if (ended.status !== "wait") step = { kind: "failed", why: String(ended.error || "The sign-in ended without adding an account.") };
        return false;
      }
      step = { kind: "choose" };
      return true;
    } catch (e) {
      pollingError = `Sign-in was not cancelled: ${message(e)}`;
      return false;
    }
  }
  async function close(opened?: Data) {
    if (!(await cancel())) return;
    dialog?.close();
    onclose(opened);
  }
  async function sendCallback(e: SubmitEvent) {
    e.preventDefault();
    callbackError = await app.act(() => api("/oauth/callback", "POST", { provider: p, redirect_url: callback.trim() }));
  }
  let keyAttempt: { snapshot: string; op: ConfigWrite; done: string } | null = null;
  async function addKey(e: SubmitEvent) {
    e.preventDefault();
    const secret = form.key.trim();
    const selected = p;
    const snapshot = JSON.stringify([selected, form.name, form.key, form.url]);
    if (!keyAttempt || keyAttempt.snapshot !== snapshot) {
      const groups = app.setting<Data[]>(`api-keys/${selected}`, []);
      const group: Data = { name: form.name.trim(), keys: [{ "api-key": secret }], "base-url": form.url.trim() || providerEndpoint(selected) };
      keyAttempt = {
        snapshot,
        op: configWrite({ path: `api-keys/${selected}`, before: groups, next: [...groups, group], what: `Adding ${P} API key ${mask(secret)}`, href: "#accounts" }),
        done: `${P} API key ${group.name} · ${mask(secret)} added.`,
      };
    }
    const error = await app.write(keyAttempt.op, keyAttempt.done);
    formError = error ? explain(error) : "";
    if (error) return void tick().then(() => dialog?.querySelector<HTMLElement>(".key-error")?.focus());
    await app.accounts.load(true);
    // Open the new key's account, so the user sees it under the name it has everywhere.
    close((app.accounts.data || []).find((a) => a.account === secret && provider(a) === selected));
  }
  async function upload(input: HTMLInputElement) {
    const files = [...(input.files || [])];
    input.value = "";
    if (!files.length) return;
    const body = new FormData();
    files.forEach((f) => body.append("file", f));
    formError = await app.act(async () => {
      const r = await api("/credentials", "POST", body);
      await app.accounts.load(true);
      if (Array.isArray(r.failed) && r.failed.length) throw new Error(`${r.uploaded || 0} uploaded. Failed: ${r.failed.map((f: Data) => `${f.name} (${f.error})`).join(", ")}`);
    }, files.length > 1 ? `${files.length} files uploaded.` : "Credential file uploaded.");
    if (!formError) close();
  }
  const reading = $derived(step.kind === "done" && step.account ? limits(step.account) : null);
  const fresh = $derived(step.kind === "done" && step.account ? app.readings[key(step.account)] : undefined);
</script>

<dialog bind:this={dialog} oncancel={(e) => (e.preventDefault(), close())} onclick={(e) => e.target === e.currentTarget && close()} aria-labelledby="add-title">
  <header>
    <h2 id="add-title">{target ? `Sign in again: ${credName(target)}` : "Add an account"}</h2>
    <button class="btn small quiet" onclick={() => close()}>Close</button>
  </header>
  <p class="sr" role="status">{announce}</p>
  <div class="body">
    {#if step.kind === "choose"}
      <fieldset class="plain">
        <legend>Provider</legend>
        <div class="choices">
          {#each signIns as id}<label class="choice"><input type="radio" name="provider" value={id} bind:group={p} />{label(id)}</label>{/each}
        </div>
      </fieldset>
      <fieldset class="plain">
        <legend>How</legend>
        <div class="stack" style="gap:6px">
          <label class="setting"><input type="radio" name="method" value="signin" bind:group={method} /><span>Sign in with a {P} subscription</span><small>You approve on {owner}'s own page. The proxy keeps the token and renews it; it never sees your password.</small></label>
          <label class="setting"><input type="radio" name="method" value="key" bind:group={method} /><span>Use a {P} API key</span><small>A key from the provider's developer platform, billed per request.</small></label>
        </div>
      </fieldset>
      {#if formError}<p class="notice bad small" role="alert"><span class="dot bad"></span><span>{formError}</span></p>{/if}
      <div class="row">
        <button class="btn primary" onclick={() => (method === "key" ? (step = { kind: "key" }) : start())}>Continue</button>
        <span class="grow"></span>
        <label class="btn quiet small">
          Upload credential files instead
          <input class="sr" type="file" accept=".json,application/json" multiple onchange={(e) => upload(e.currentTarget)} />
        </label>
      </div>
    {:else if step.kind === "key"}
      <form class="stack" onsubmit={addKey} oninput={() => { keyAttempt = null; formError = ""; }}>
        <label class="field">Name<input type="text" required bind:value={form.name} placeholder="For example, Console key" /></label>
        <label class="field">{P} API key<input type="password" autocomplete="off" required bind:value={form.key} /></label>
        <details>
          <summary class="small">Use a different endpoint</summary>
          <label class="field">Base URL<input type="url" bind:value={form.url} placeholder="Provider default" /></label>
        </details>
        {#if formError}<p class="notice bad small key-error" role="alert" tabindex="-1"><span class="dot bad"></span><span>Could not confirm the key was added: {formError}</span></p>{/if}
        <div class="row">
          <button class="btn primary" aria-disabled={app.busy}>{formError ? "Try again" : "Add API key"}</button>
          <button type="button" class="btn" onclick={() => ((step = { kind: "choose" }), (formError = ""))}>Back</button>
        </div>
      </form>
    {:else if step.kind === "starting"}
      <p class="row" data-step-lead tabindex="-1"><span class="dot live pulse"></span>Asking the server for a {P} sign-in address…</p>
    {:else if step.kind === "waiting"}
      {@const s = step.session}
      <p class="row" data-step-lead tabindex="-1"><span class="dot live pulse"></span><strong>Waiting for you to approve on {owner}'s page</strong></p>
      {#if pollingError}<p class="notice bad small" role="alert">{pollingError} The proxy will check again while this dialog is open.</p>{/if}
      {#if target}<p class="small">Sign in as <strong>{credName(target)}</strong>. A sign-in to any other account is refused and saves nothing.</p>{/if}
      {#if s.code}
        <div class="stack" style="gap:6px">
          <span class="small muted">Enter this code on the sign-in page{s.expires ? (s.expires > app.now ? `. It expires in ${span(s.expires - app.now)}` : ". It has expired; start again") : ""}.</span>
          <div class="row"><span class="usercode">{s.code}</span><Copy text={s.code} label="Copy code" /></div>
        </div>
      {/if}
      <div class="row">
        <a class="btn primary" href={s.url} target="_blank" rel="noopener noreferrer">Open sign-in page</a>
        <Copy text={s.url} label="Copy link" small={false} />
      </div>
      {#if s.device}
        <p class="small muted">No browser on this machine? Copy the link and open it on any device.</p>
      {:else}
        <details>
          <summary class="small">The browser ended on a page that won't load</summary>
          <form class="stack" style="gap:8px" onsubmit={sendCallback}>
            <label class="field">Paste that page's full address<input type="url" required bind:value={callback} placeholder="http://localhost:…/callback?code=…" /></label>
            {#if callbackError}<p class="notice bad small" role="alert"><span class="dot bad"></span><span>{callbackError}</span></p>{/if}
            <div class="row"><button class="btn small" aria-disabled={app.busy}>Send address</button></div>
          </form>
        </details>
      {/if}
      <div class="row"><button class="btn quiet" onclick={() => close()}>Cancel sign-in</button></div>
    {:else if step.kind === "verifying"}
      <p class="row" data-step-lead tabindex="-1"><span class="dot ok"></span>Approved on {owner}'s page, and the token is saved.</p>
      <p class="row"><span class="dot live pulse"></span>Reading the account's limits. This is a usage read, not a model request.</p>
    {:else if step.kind === "done"}
      <p class="row" data-step-lead tabindex="-1"><span class="dot ok"></span><strong>{target ? "Signed in again" : `${P} account connected`}{step.account ? `: ${credName(step.account)}` : ""}</strong></p>
      {#if target}<p class="small muted">The account's file holds the new sign-in. Its settings are unchanged.</p>{/if}
      {#if reading && "windows" in reading}
        <div class="stack" style="gap:8px">
          {#each reading.windows as w}<Meter name={w.label} used={w.used} note={Date.parse(w.reset) > app.now ? `resets in ${span(Date.parse(w.reset) - app.now)}` : ""} why={w.detail} />{/each}
        </div>
      {/if}
      {#if fresh?.error}
        <p class="notice warn small"><span class="dot warn"></span><span>The sign-in worked, but its limits could not be read: {fresh.error}</span></p>
      {:else if step.account && !canCheck(step.account)}
        <p class="small muted">{P} has no usage endpoint the proxy can read, so this account shows no limits.</p>
      {/if}
      <div class="row">
        <button class="btn primary" onclick={() => close(step.kind === "done" ? (step.account ?? undefined) : undefined)}>Done</button>
        {#if !target}<button class="btn" onclick={() => ((step = { kind: "choose" }), (method = "signin"))}>Add another</button>{/if}
      </div>
    {:else}
      <p class="notice bad" data-step-lead tabindex="-1"><span class="dot bad"></span><span><strong>{target ? "Not signed in" : "No account was added"}.</strong> {sentence(step.why)}</span></p>
      <p class="small muted">{target ? `${credName(target)} is unchanged.` : "Nothing was saved."} Starting again asks {owner} for a new sign-in{target ? " for the same account" : ""}.</p>
      <div class="row">
        <button class="btn primary" onclick={start}>Start again</button>
        {#if !target}<button class="btn" onclick={() => (step = { kind: "choose" })}>Choose another way</button>{/if}
        <button class="btn quiet" onclick={() => close()}>Close</button>
      </div>
    {/if}
  </div>
</dialog>

<style>
  [data-step-lead]:focus,
  .key-error:focus {
    outline: none;
  }
  fieldset.plain {
    display: grid;
    gap: 8px;
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
  }
  legend {
    padding: 0;
    margin-bottom: 8px;
    font-size: 13px;
    font-weight: 520;
    color: var(--ink-2);
  }
</style>
