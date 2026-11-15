<script lang="ts">
  // The switches the four screens depend on, the routing strategy and the appearance.
  import { strategies, strategy } from "../../core";
  import { app } from "../../app.svelte";
  import { configWrite, explain, type ConfigWrite, type WriteError } from "../../server";

  const switches = [
    ["observability/logs/logging-to-file", "Write the server log to a file", "Activity's server log reads this file. Off means the server log has nothing to show."],
    ["observability/logs/request-log", "Save every request and response", "Lets Activity open the full request log for any request. Files hold prompts and grow quickly. Off still saves failed requests."],
    ["observability/logs/debug", "Debug logging", "Adds detail lines, such as which account a request was given."],
    ["observability/usage/usage-statistics-enabled", "Report requests as they happen", "Live requests in Activity come from this."],
  ] as const;
  const names: Record<string, string> = Object.fromEntries([...switches.map(([p, n]) => [p, n]), ["routing/strategy", "Routing strategy"]]);

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
  <h3>Logging and usage</h3>
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
