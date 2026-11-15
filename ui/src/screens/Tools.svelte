<script lang="ts">
  // Use with tools: pick a tool, get configuration with the right address and a client key.
  // Client keys are created, chosen and removed here; nothing else in the app lists them.
  import { clientModels, base as served } from "../api";
  import { mask, newKey, snippet, tools } from "../core";
  import { app, message } from "../app.svelte";
  import { configWrite, explain } from "../server";
  import { plural } from "../fmt";
  import Copy from "../parts/Copy.svelte";
  import { focusAfter, find } from "../focus";

  type Tool = (typeof tools)[number];

  let tool = $state<Tool>("Claude Code");
  let chosen = $state("");
  let address = $state(served);
  let model = $state("");
  let models = $state<string[]>([]);
  type Check = { kind: "checking" } | { kind: "ok"; models: number } | { kind: "no-models" } | { kind: "failed"; why: string };
  let result = $state<Check | null>(null);

  const keys = $derived(app.setting<string[]>("access/api-keys", []));
  const key = $derived(keys.includes(chosen) ? chosen : keys[0] || "");
  const needsModel = $derived(tool === "OpenAI SDK" || tool === "Anthropic SDK");
  const base = $derived(address.trim().replace(/\/+$/, ""));
  const local = $derived(/^https?:\/\/(localhost|127\.|\[?::1\]?)/.test(base));
  const canServe = $derived((app.accounts.data || []).some((a) => !a.disabled && a.status !== "error"));
  const text = (secret: string) => snippet(tool, base, secret, model || "MODEL_ID");

  const notes: Record<Tool, string> = {
    "Claude Code": "Run these in the shell you start Claude Code from. To keep them, put both variables in the env block of ~/.claude/settings.json.",
    "Codex CLI": "Add the provider to ~/.codex/config.toml, then set the key in your shell.",
    Cursor: "Enter both values under Cursor Settings, Models.",
    "OpenAI SDK": "Python. The JavaScript SDK takes the same baseURL and apiKey.",
    "Anthropic SDK": "Python. The address has no /v1 here: the SDK adds it.",
    curl: "Lists the models this key can reach. It is the same check this page runs.",
  };

  /** Asks the proxy for its model list with the chosen key, as a tool would. No provider request. */
  async function check() {
    const k = key;
    if (!k) return void (result = null);
    result = { kind: "checking" };
    try {
      const ids = await clientModels(k);
      if (k !== key) return;
      models = ids;
      if (!ids.includes(model)) model = ids[0] || "";
      result = ids.length ? { kind: "ok", models: ids.length } : { kind: "no-models" };
    } catch (e) {
      if (k === key) result = { kind: "failed", why: message(e) };
    }
  }
  $effect(() => {
    key;
    check();
  });

  /** The last failed key write, with the key it was for, so the message says which. */
  let keyError = $state<{ what: string; key: string; why: string } | null>(null);
  let removing = $state("");
  let managing = $state(false);
  let reveal = $state(false);
  let own = $state("");
  let ownShown = $state(false);
  let ownError = $state("");
  async function add(fresh: string, what: string) {
    keyError = null;
    const first = !keys.length;
    const op = configWrite({ path: "access/api-keys", before: keys, next: [...keys, fresh], what: what === "create" ? "Creating a client key" : `Adding client key ${mask(fresh)}`, href: "#tools" });
    const error = await app.write(op, what === "create" ? `Client key ${mask(fresh)} created.` : `Client key ${mask(fresh)} added.`);
    if (error) keyError = { what: what === "create" ? "The new key was not created" : "Your key was not added", key: fresh, why: explain(error) };
    else {
      chosen = fresh;
      // The first key replaces the empty state, and the control that was used with it: continue at the key's setup.
      if (first) focusAfter(() => find("#client-key"));
    }
    return !error;
  }
  const create = () => add(newKey(), "create");
  async function addOwn(e: SubmitEvent) {
    e.preventDefault();
    const k = own.trim();
    ownError = "";
    if (k.length < 8) ownError = "Use at least 8 characters. Anyone who can reach the proxy can try to guess a short key.";
    else if (/\s/.test(k)) ownError = "A key cannot contain spaces: tools send it in a header.";
    else if (keys.includes(k)) ownError = `That key is already in the list as ${mask(k)}.`;
    if (ownError) return void focusAfter(() => find("#own-key"));
    if (await add(k, "own")) own = "";
  }
  function askRemove(k: string) {
    removing = k;
    focusAfter(() => find(".remove-confirm .keep"));
  }
  function keep(k: string) {
    removing = "";
    focusAfter(() => find(`[data-remove="${CSS.escape(k)}"]`));
  }
  async function remove(k: string) {
    const at = keys.indexOf(k);
    keyError = null;
    const error = await app.write(configWrite({ path: "access/api-keys", before: keys, next: keys.filter((x) => x !== k), what: `Removing client key ${mask(k)}`, href: "#tools" }), `Client key ${mask(k)} removed.`);
    if (error) {
      keyError = { what: `Key ${mask(k)} was not removed`, key: k, why: explain(error) };
      return void focusAfter(() => find(".remove-confirm .danger"));
    }
    removing = "";
    // Continue at the key that took its place, or the add form when the list is empty.
    focusAfter(() => {
      const next = keys[Math.min(at, keys.length - 1)];
      return next ? find(`[data-remove="${CSS.escape(next)}"]`) : find("#own-key");
    });
  }
</script>

{#snippet ownForm()}
  <form class="own" onsubmit={addOwn} novalidate>
    <label class="field" for="own-key">Add a key of your own</label>
    <div class="row">
      <input id="own-key" class="grow mono" type={ownShown ? "text" : "password"} autocomplete="off" spellcheck="false" bind:value={own} aria-invalid={!!ownError} aria-describedby="own-help" />
      <button type="button" class="btn small quiet" aria-pressed={ownShown} onclick={() => (ownShown = !ownShown)}>{ownShown ? "Hide" : "Show"}<span class="sr"> the key you are typing</span></button>
      <button class="btn small" aria-disabled={app.busy}>Add key</button>
    </div>
    <p class="small" class:muted={!ownError} class:code-status={!!ownError} class:bad={!!ownError} id="own-help" role={ownError ? "alert" : undefined}>
      {ownError || "For a key a tool already uses. Tools send it as a Bearer token or an x-api-key header."}
    </p>
  </form>
{/snippet}

<div class="page narrow">
  <div class="head"><h1>Use with tools</h1></div>

  {#if !app.config.data}
    <p class="empty muted">Reading client keys…</p>
  {:else if !keys.length}
    <div class="empty">
      <h2>Create a client key first</h2>
      <p class="muted">A tool proves itself to this proxy with a client key. It is separate from your management key and from your provider accounts.</p>
      {#if keyError}<p class="notice bad small" role="alert"><span class="dot bad"></span><span>{keyError.what}: {keyError.why}</span></p>{/if}
      <div class="row">
        <button class="btn primary" aria-disabled={app.busy} onclick={create}>{keyError ? "Try creating again" : "Create a client key"}</button>
        <button class="btn" onclick={() => ((managing = true), focusAfter(() => find("#own-key")))}>Use a key of my own</button>
      </div>
      {#if managing}{@render ownForm()}{/if}
    </div>
  {:else}
    {#if app.accounts.data && !canServe}
      <p class="notice warn small" style="margin-bottom:16px">
        <span class="dot warn"></span>
        <span>No account can serve requests right now, so a tool will connect and then get errors. <a href="#accounts">Open Accounts</a></span>
      </p>
    {/if}
    <div class="tool-layout">
      <div class="tool-list" role="group" aria-label="Tool">
        {#each tools as t}<button aria-pressed={tool === t} onclick={() => (tool = t)}>{t}</button>{/each}
      </div>

      <div class="steps">
        <div class="step">
          <h2>Client key</h2>
          <div class="row">
            <label>
              <span class="sr">Client key to use</span>
              <select id="client-key" value={key} onchange={(e) => (chosen = e.currentTarget.value)}>
                {#each keys as k (k)}<option value={k}>{mask(k)}</option>{/each}
              </select>
            </label>
            <button class="btn" aria-disabled={app.busy} onclick={create}>Create another key</button>
            <button class="btn quiet" aria-expanded={managing} aria-controls="key-list" onclick={() => (managing = !managing)}>{managing ? "Close key list" : `Manage ${plural(keys.length, "key")}`}</button>
          </div>
          {#if keyError}<p class="notice bad small" role="alert"><span class="dot bad"></span><span class="grow">{keyError.what}: {keyError.why}</span><button class="btn small quiet" onclick={() => (keyError = null)}>Dismiss</button></p>{/if}
          {#if managing}
            <div class="card key-manager" id="key-list">
              <div class="row key-head">
                <h3 class="grow">Client keys</h3>
                <button class="btn small quiet" aria-pressed={reveal} onclick={() => (reveal = !reveal)}>{reveal ? "Hide full keys" : "Show full keys"}</button>
              </div>
              <ul class="keys">
                {#each keys as k (k)}
                  <li class:confirming={removing === k}>
                    <span class="mono key-text">{reveal ? k : mask(k)}{#if k === key}<span class="faint small in-setup">(in the setup below)</span>{/if}</span>
                    {#if removing === k}
                      <div class="remove-confirm" role="group" aria-label={`Remove ${mask(k)}`}>
                        <p class="small">Remove <span class="mono">{mask(k)}</span>? Tools using it lose access at once. This cannot be undone.</p>
                        <div class="row">
                          <button class="btn small danger" aria-disabled={app.busy} onclick={() => remove(k)}>Remove {mask(k)}</button>
                          <button class="btn small keep" onclick={() => keep(k)}>Keep it</button>
                        </div>
                      </div>
                    {:else}
                      <div class="row key-actions">
                        <Copy text={k} label="Copy key" done="Full key copied" />
                        <button class="btn small quiet danger" data-remove={k} onclick={() => askRemove(k)}>Remove…<span class="sr"> {mask(k)}</span></button>
                      </div>
                    {/if}
                  </li>
                {/each}
              </ul>
              {@render ownForm()}
            </div>
          {/if}
          <p class="row small" role="status">
            {#if result?.kind === "checking"}<span class="dot live pulse"></span>Asking the proxy for its model list with this key…
            {:else if result?.kind === "ok"}<span class="dot ok"></span>The proxy accepts this key and offers it {plural(result.models, "model")}.
            {:else if result?.kind === "no-models"}<span class="dot warn"></span>The proxy accepts this key, but no account can serve a model yet.
            {:else if result?.kind === "failed"}<span class="dot bad"></span><span class="grow">Check failed: {result.why}</span>{/if}
            {#if result && result.kind !== "checking"}<button class="link" onclick={check}>Check again</button>{/if}
          </p>
          <p class="small muted">The check lists models on the proxy. It sends nothing to a provider.</p>
        </div>

        <div class="step">
          <h2><label for="address">Proxy address</label></h2>
          <div class="row">
            <input id="address" type="url" class="grow mono" style="max-width:420px" bind:value={address} spellcheck="false" />
            {#if base !== served}<button class="link small" onclick={() => (address = served)}>Use this page's address</button>{/if}
          </div>
          {#if tool === "Cursor" && local}
            <p class="notice warn small"><span class="dot warn"></span><span>Cursor sends requests from its own servers, so it cannot reach an address on this computer. Replace the address with the proxy's public HTTPS address, for example a tunnel.</span></p>
          {:else if local}
            <p class="small muted">This address works for tools on this computer. For a tool on another machine, replace it with an address that machine can reach.</p>
          {/if}
        </div>

        {#if needsModel}
          <div class="step">
            <h2><label for="model">Model</label></h2>
            {#if models.length}
              <select id="model" bind:value={model} style="max-width:420px">{#each models as m}<option>{m}</option>{/each}</select>
            {:else}
              <p class="small muted">No models to choose from yet, so the example below says MODEL_ID.</p>
            {/if}
          </div>
        {/if}

        <div class="step">
          <h2>{tool === "curl" ? "Command" : `${tool} configuration`}</h2>
          <p class="small muted">{notes[tool]}</p>
          <pre class="code tall" aria-label={`${tool} configuration, with the key shortened`}>{text(mask(key))}</pre>
          <div class="row">
            <Copy text={() => text(key)} label="Copy with the full key" done="Copied, including the full key" small={false} primary />
            <span class="small muted">Shown shortened here. The copy holds the full key: paste it only where it belongs.</span>
          </div>
        </div>
      </div>
    </div>
  {/if}
</div>

<style>
  .key-manager {
    display: grid;
    gap: 8px;
    padding: 8px 14px 14px;
  }
  .key-head h3 {
    color: var(--ink);
  }
  .keys li {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 10px;
  }
  .key-text {
    flex: 1 1 14ch;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .in-setup {
    margin-left: 0.6ch;
    font-family: var(--sans);
  }
  .keys li.confirming {
    display: grid;
    gap: 6px;
    padding: 10px 0;
  }
  .remove-confirm {
    display: grid;
    gap: 8px;
  }
  .own {
    display: grid;
    gap: 6px;
    margin-top: 6px;
  }
  .own input {
    min-width: 12ch;
  }
</style>
