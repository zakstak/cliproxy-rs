<script lang="ts">
  // First run: what to do before the proxy can serve anything, in order. Each step is done
  // when the server says so (an account exists, a client key exists, the proxy answered a
  // model list with that key), so the list never claims progress the server does not show.
  import { clientModels } from "../api";
  import { app, message } from "../app.svelte";
  import { plural } from "../fmt";
  import { focusAfter, find } from "../focus";

  let { ondone }: { ondone: () => void } = $props();

  const accounts = $derived(app.accounts.data || []);
  const keys = $derived(app.setting<string[]>("access/api-keys", []));
  type Probe = { kind: "idle" } | { kind: "sending" } | { kind: "ok"; models: number } | { kind: "empty" } | { kind: "failed"; why: string };
  let probe = $state<Probe>({ kind: "idle" });
  async function test() {
    probe = { kind: "sending" };
    try {
      const testedKey = keys[0];
      const ids = await clientModels(testedKey);
      probe = ids.length ? { kind: "ok", models: ids.length } : { kind: "empty" };
      if (ids.length) {
        app.setupTestKey = testedKey;
        app.setupVerified = true;
        // The step and its button are replaced by the verified message: continue there.
        focusAfter(() => find("#setup-verified"));
      }
    } catch (e) {
      probe = { kind: "failed", why: message(e) };
    }
  }
  const steps = $derived([
    { done: accounts.length > 0, title: "Add an account", text: "Sign in to a Claude, Codex, xAI or Meta subscription, or add a provider API key.", href: "#accounts?add=1", action: "Add an account" },
    { done: keys.length > 0, title: "Create a client key", text: "Tools prove themselves to this proxy with a client key. It is separate from your management key.", href: "#tools", action: "Create a client key" },
    { done: !!app.setupTestKey && keys.includes(app.setupTestKey), title: "Send a test request", text: "Asks this proxy for its model list with your client key, as a tool would. It uses none of your plan's limits.", href: "", action: "Send test request" },
  ]);
  const left = $derived(steps.filter((s) => !s.done).length);
  const next = $derived(steps.findIndex((s) => !s.done));
</script>

<section class="setup grid max-w-[760px] gap-2 px-4 py-4.5" aria-labelledby={app.setupVerified ? "setup-verified" : "setup-title"}>
  {#if app.setupVerified}
    <h2 id="setup-verified" tabindex="-1" class="flex items-center gap-2 text-base focus:outline-none"><span class="dot ok"></span>Setup verified</h2>
    <p>The proxy answered your client key with {probe.kind === "ok" ? plural(probe.models, "model") : "its model list"}. Your tools can use it now; their requests appear here as they finish.</p>
    <div><button class="btn primary" onclick={ondone}>Show live requests</button></div>
  {:else}
    <h2 id="setup-title" class="text-base">Set up the proxy <span class="muted small">{3 - left} of 3 done</span></h2>
    <p class="small muted">Complete these steps to connect a tool and verify that it can reach the proxy.</p>
    <ol class="mt-1.5 grid gap-0.5">
      {#each steps as s, i (s.title)}
        <li class="grid grid-cols-[24px_minmax(0,1fr)] items-start gap-x-3 gap-y-2 border-t border-line py-2.5 sm:grid-cols-[24px_minmax(0,1fr)_auto]" class:done={s.done} class:next={i === next}>
          <span class="mark" aria-hidden="true">{s.done ? "✓" : i + 1}</span>
          <div class="min-w-0">
            <strong>{s.title}</strong>{#if s.done}<span class="sr">, done</span>{/if}
            {#if !s.done}<p class="small muted">{s.text}{i === 2 && (!keys.length || !accounts.length) ? " Available once steps 1 and 2 are done." : ""}</p>{/if}
            {#if i === 2 && probe.kind !== "idle"}
              <p class="small" role="status">
                {#if probe.kind === "sending"}<span class="dot live pulse"></span> Asking the proxy…
                {:else if probe.kind === "ok"}<span class="dot ok"></span> The proxy answered with {plural(probe.models, "model")}.
                {:else if probe.kind === "empty"}<span class="dot warn"></span> The proxy accepted the key, but no account can serve a model yet.
                {:else if probe.kind === "failed"}<span class="dot bad"></span> {probe.why}{/if}
              </p>
            {/if}
          </div>
          {#if !s.done}
            <div class="col-start-2 sm:col-start-3">
              {#if s.href}<a class="btn small" class:primary={i === next} href={s.href}>{s.action}</a>
              {:else}<button class="btn small" class:primary={i === next} aria-disabled={!keys.length || !accounts.length || probe.kind === "sending"} onclick={test}>{probe.kind === "failed" || probe.kind === "empty" ? "Send again" : s.action}</button>{/if}
            </div>
          {/if}
        </li>
      {/each}
    </ol>
  {/if}
</section>

<style>
  .mark {
    display: grid;
    place-items: center;
    width: 24px;
    height: 24px;
    border: 1.5px solid var(--line-2);
    border-radius: 50%;
    font-size: 13px;
    font-weight: 600;
    color: var(--ink-2);
  }
  li.done .mark {
    border-color: var(--ok);
    color: var(--ok);
  }
  li.next .mark {
    border-color: var(--ink);
    color: var(--ink);
  }
  li.done strong {
    color: var(--ink-2);
  }
</style>
