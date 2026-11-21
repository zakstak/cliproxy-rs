<script lang="ts">
  // The history store, the logging switches, the routing strategy and the appearance.
  import { strategies, strategy } from "../../core";
  import { api } from "../../api";
  import { num } from "../../fmt";
  import { app, message } from "../../app.svelte";
  import { configWrite, explain, type ConfigWrite, type WriteError } from "../../server";

  const switches = [
    ["observability/logs/request-log", "Save every request and response", "Lets Activity open the full request log for any request. Files hold prompts and grow quickly. Off still saves failed requests."],
    ["observability/logs/debug", "Debug logging", "Adds detail lines, such as which account a request was given."],
  ] as const;
  const names: Record<string, string> = Object.fromEntries([...switches.map(([p, n]) => [p, n]), ["routing/strategy", "Routing strategy"], ["storage/retention-days", "History retention"]]);

  type History = { enabled: boolean; backend?: string; retention_days?: number; written?: number; dropped?: number };
  let history = $state<History | null>(null);
  let historyError = $state("");
  $effect(() => {
    api("/history/status")
      .then((r) => ((history = { enabled: r.enabled === true, backend: r.backend, retention_days: r.retention_days, written: r.written, dropped: r.dropped }), (historyError = "")))
      .catch((e: unknown) => (historyError = message(e)));
  });
  const retention = $derived(Number(app.setting("storage/retention-days", 30)));
  let days = $state("");
  $effect(() => {
    days = String(retention);
  });
  function saveRetention() {
    const next = Number(days);
    if (!Number.isInteger(next) || next < 0) return;
    run(configWrite({ path: "storage/retention-days", before: app.setting("storage/retention-days", null), next, what: "History retention: changing", href: "#settings" }), "History retention: saved.");
  }

  /** The last failed write, as the operation itself, so a retry writes the same change or refuses. */
  let failed = $state<{ op: ConfigWrite; done: string; error: WriteError } | null>(null);
  async function run(op: ConfigWrite, done: string) {
    failed = null;
    const error = await app.write(op, done);
    if (error) failed = { op, done, error };
  }
  const set = (path: string, before: unknown, next: unknown) =>
    run(configWrite({ path, before, next, what: `${names[path]}: ${typeof next === "boolean" ? (next ? "turning on" : "turning off") : "changing"}`, href: "#settings" }), `${names[path]}: saved.`);
  const current = $derived(strategy(app.setting("routing/strategy", ""), true));
</script>

<section class="group">
  <h3>History</h3>
  <p class="small muted max-w-[72ch]">Activity, Usage and the server log read the server's history store: every upstream request and every log line, kept across restarts. It never holds keys or prompts.</p>
  {#if historyError}
    <p class="notice bad small" role="alert"><span class="dot bad"></span><span>Could not read the history store's state: {historyError}</span></p>
  {:else if history && !history.enabled}
    <p class="notice warn small"><span class="dot warn"></span><span>Not recording: <span class="mono">storage.backend</span> is <span class="mono">none</span>, or the store failed to open (the server log says which). Changing the backend needs a restart.</span></p>
  {:else if history}
    <p class="small">Recording to <span class="mono">{history.backend}</span>. {num(history.written ?? 0)} entries written since the server started{history.dropped ? `, ${num(history.dropped)} dropped because the store fell behind` : ""}.</p>
  {/if}
  <div class="row items-end">
    <label class="grid gap-1">
      <span>Keep history for (days)</span>
      <input class="max-w-32" type="number" min="0" step="1" inputmode="numeric" bind:value={days} />
    </label>
    <button class="btn" disabled={days === String(retention) || !/^\d+$/.test(days)} aria-disabled={app.busy} onclick={() => !app.busy && saveRetention()}>Save</button>
  </div>
  <p class="small muted">0 keeps everything. Older entries are deleted hourly. Applies without a restart.</p>
  {#if failed?.op.path === "storage/retention-days"}<p class="notice bad small" role="alert"><span class="dot bad"></span><span class="grow">Retention not changed: {explain(failed.error)}</span>{#if failed.error._tag !== "Changed"}<button class="btn small" aria-disabled={app.busy} onclick={() => failed && run(failed.op, failed.done)}>Try again</button>{/if}</p>{/if}
</section>

<section class="group">
  <h3>Logging</h3>
  {#each switches as [path, name, help]}
    {@const on = app.setting(path, false)}
    <div class="setting-row">
      <label class="setting">
        <input type="checkbox" checked={on} aria-disabled={app.busy} onchange={(e) => ((e.currentTarget.checked = on), !app.busy && set(path, on, !on))} />
        <span>{name}</span>
        <small>{help}</small>
      </label>
      {#if failed?.op.path === path}
        <p class="notice bad small" role="alert"><span class="dot bad"></span><span class="grow">{on ? "Not turned off" : "Not turned on"}: {explain(failed.error)}</span>{#if failed.error._tag !== "Changed"}<button class="btn small" aria-disabled={app.busy} onclick={() => failed && run(failed.op, failed.done)}>Try again</button>{/if}</p>
      {/if}
    </div>
  {/each}
</section>

<section class="group">
  <h3><label for="strategy">Which account takes the next request</label></h3>
  <select id="strategy" class="max-w-80" value={current.value} onchange={(e) => set("routing/strategy", app.setting("routing/strategy", ""), e.currentTarget.value)}>
    {#each strategies as s}<option value={s.value}>{s.name}</option>{/each}
  </select>
  <p class="small muted max-w-[72ch]">{current.help}</p>
  {#if failed?.op.path === "routing/strategy"}<p class="notice bad small" role="alert"><span class="dot bad"></span><span class="grow">Strategy not changed: {explain(failed.error)}</span>{#if failed.error._tag !== "Changed"}<button class="btn small" aria-disabled={app.busy} onclick={() => failed && run(failed.op, failed.done)}>Try again</button>{/if}</p>{/if}
</section>

<section class="group">
  <h3 id="theme-label">Appearance</h3>
  <div class="seg w-fit" role="group" aria-labelledby="theme-label">
    {#each [["dark", "Dark"], ["light", "Light"], ["system", "Match system"]] as const as [id, name]}
      <button aria-pressed={app.theme === id} onclick={() => app.setTheme(id)}>{name}</button>
    {/each}
  </div>
  <p class="small muted">Kept for this browser. Match system follows your computer's light or dark setting.</p>
</section>
