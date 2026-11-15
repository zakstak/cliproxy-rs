<script lang="ts">
  // Provider API keys: groups of keys per provider. A group is one endpoint with one or more
  // keys, used in turn by weight. Each key is named as Accounts names it (its shortened key)
  // and carries the server's auth index, which links it to its account row and activity.
  // Every write is a frozen operation (server.ts): compare-and-swap against what was shown.
  import { providerEndpoint } from "../../account";
  import { Effect } from "effect";
  import { onDestroy, untrack } from "svelte";
  import { label, mask, signIns, sum, type Data } from "../../core";
  import { app, asData, Res, server } from "../../app.svelte";
  import { configWrite, explain, type ConfigWrite, type WriteError } from "../../server";
  import { num, plural } from "../../fmt";
  import { focusAfter, find, retried } from "../../focus";

  const asked = app.route.query.get("provider");
  let family = $state(asked && signIns.includes(asked) ? asked : "claude");
  let reveal = $state(false);
  /** The key a link asked for (?key=<auth index>): shown highlighted and focused. */
  let target = $state(app.route.query.get("key") || "");
  const groups = $derived(app.setting<Data[]>(`api-keys/${family}`, []));
  const usage = new Res<Data>(server.read("/observability/usage/api-keys").pipe(Effect.map(asData)));
  usage.load();
  function traffic(g: Data, k: Data): number | null {
    const hit = usage.data?.[family]?.[`${String(k["base-url"] ?? g["base-url"] ?? "").trim()}|${String(k["api-key"]).trim()}`];
    return hit ? sum((hit.recent_requests || []).map((b: Data) => (b.success || 0) + (b.failed || 0))) : null;
  }
  const keyName = (k: Data) => mask(String(k["api-key"] || ""));
  const groupName = (g: Data, gi: number) => g.name || `Group ${gi + 1}`;
  const short = (index: unknown) => String(index || "").slice(0, 8);

  // A link from an account (#settings/keys?provider=claude&key=…) opens that provider at that key.
  $effect(() => {
    const q = app.route.query;
    untrack(() => {
      const p = q.get("provider");
      if (p && signIns.includes(p) && p !== family) select(p);
      target = q.get("key") || "";
      if (target) focusAfter(() => find(`[data-key="${CSS.escape(target)}"]`));
    });
  });

  /** The last failed write on this screen, with the operation itself so it can be retried as it was. */
  let failure = $state<{ op: ConfigWrite; done: string; error: WriteError } | null>(null);
  /** Moves when the provider changes: an answer for an earlier provider never lands in this one's view. */
  let context = 0;
  let gone = false;
  onDestroy(() => { gone = true; });
  function select(value: string) {
    context++;
    family = value;
    failure = null;
    confirm = "";
    addingTo = null;
  }
  const operation = (next: Data[], what: string) =>
    configWrite({ path: `api-keys/${family}`, before: groups, next, what: `${what} (${label(family)})`, href: `#settings/keys?provider=${family}` });
  async function write(op: ConfigWrite, done: string) {
    const started = context;
    failure = null;
    const error = await app.write(op, done);
    if (gone || started !== context) {
      // The user switched provider meanwhile: report it wherever they are, under its own name.
      if (error) app.failedAway(op, error, done);
      return false;
    }
    if (error) {
      failure = { op, done, error };
      focusAfter(() => find(".pk-failure"));
    }
    return !error;
  }
  // Confirmations: "g:<index>" for a group, "k:<group>:<key>" for one key.
  let confirm = $state("");
  function ask(id: string) {
    confirm = id;
    focusAfter(() => find(".pk-confirm .keep"));
  }
  function keep(id: string) {
    confirm = "";
    focusAfter(() => find(`[data-ask="${id}"]`));
  }
  async function removeKey(gi: number, ki: number) {
    const g = groups[gi];
    const k = g.keys[ki];
    const next = groups.map((x, i) => (i === gi ? { ...x, keys: x.keys.filter((_: Data, n: number) => n !== ki) } : x));
    if (await write(operation(next, `Removing key ${keyName(k)} from ${groupName(g, gi)}`), `Key ${keyName(k)} removed from ${groupName(g, gi)}.`)) {
      confirm = "";
      focusAfter(() => find(`[data-group="${gi}"] h4`));
    }
  }
  async function removeGroup(gi: number) {
    const g = groups[gi];
    if (await write(operation(groups.filter((_, i) => i !== gi), `Deleting group ${groupName(g, gi)}`), `Group ${groupName(g, gi)} deleted.`)) {
      confirm = "";
      focusAfter(() => find(".pk-add input"));
    }
  }
  let addingTo = $state<number | null>(null);
  let newKey = $state({ key: "", weight: "" });
  async function addKey(gi: number, e: SubmitEvent) {
    e.preventDefault();
    const g = groups[gi];
    const entry: Data = { "api-key": newKey.key.trim(), ...(newKey.weight.trim() ? { weight: Number(newKey.weight) } : {}) };
    const next = groups.map((x, i) => (i === gi ? { ...x, keys: [...(x.keys || []), entry] } : x));
    if (await write(operation(next, `Adding a key to ${groupName(g, gi)}`), `Key ${mask(entry["api-key"])} added to ${groupName(g, gi)}.`)) {
      newKey = { key: "", weight: "" };
      addingTo = null;
      focusAfter(() => find(`[data-group="${gi}"] .pk-keys li:last-child`));
    }
  }
  let form = $state({ name: "", url: "", key: "" });
  async function addGroup(e: SubmitEvent) {
    e.preventDefault();
    const group: Data = { name: form.name.trim(), keys: [{ "api-key": form.key.trim() }], "base-url": form.url.trim() || providerEndpoint(family) };
    if (await write(operation([...groups, group], `Adding group ${group.name}`), `Group ${group.name} added.`)) {
      form = { name: "", url: "", key: "" };
      focusAfter(() => find(`[data-group="${groups.length - 1}"] h4`));
    }
  }
</script>

<p class="small muted">API keys from a provider's developer platform, billed per request. Each key is also an account on Accounts, named by its group and shortened key.</p>
<div class="flex flex-wrap items-center gap-2">
  <div class="seg" role="group" aria-label="Provider">
    {#each signIns as f (f)}<button aria-pressed={family === f} onclick={() => select(f)}>{label(f)} <span class="small">{app.setting<Data[]>(`api-keys/${f}`, []).length || ""}</span></button>{/each}
  </div>
  <span class="grow"></span>
  {#if groups.length}<button class="btn small quiet" aria-pressed={reveal} onclick={() => (reveal = !reveal)}>{reveal ? "Hide full keys" : "Show full keys"}</button>{/if}
</div>

{#if failure}
  <div class="notice bad small pk-failure" role="alert" tabindex="-1">
    <span class="dot bad"></span>
    <div class="grid min-w-0 gap-2">
      <span>{failure.op.what} failed: {explain(failure.error)}</span>
      <div class="flex flex-wrap gap-2">
        {#if failure.error._tag !== "Changed"}<button class="btn small" aria-disabled={app.busy} onclick={() => failure && write(failure.op, failure.done)}>Try again</button>{/if}
        <button class="btn small quiet" onclick={() => (failure = null)}>Dismiss</button>
      </div>
    </div>
  </div>
{/if}
{#if usage.error}<p class="small muted">Per-key traffic could not be read: {usage.error.message} <button class="link" onclick={() => retried(usage.load().then((r) => r.ok), () => find(`[data-group="0"] h4`), () => app.notify("Per-key traffic read."))}>Try again</button></p>{/if}

{#if !groups.length}
  <p class="muted">No {label(family)} API keys. Add a group below.</p>
{:else}
  <ul class="grid gap-3">
    {#each groups as g, gi (gi)}
      <li class="card grid gap-2 px-3.5 py-3" data-group={gi}>
        <div class="flex flex-wrap items-center gap-2">
          <h4 class="min-w-0 grow text-[14px] wrap-anywhere focus:outline-none" tabindex="-1">{groupName(g, gi)}</h4>
          <button class="btn small quiet danger" data-ask={`g:${gi}`} onclick={() => ask(`g:${gi}`)}>Delete group…<span class="sr"> {groupName(g, gi)}</span></button>
        </div>
        <p class="small muted mono break-all">{g["base-url"] || "Provider default endpoint"}</p>
        {#if confirm === `g:${gi}`}
          <div class="pk-confirm notice warn small wrap-anywhere" role="group" aria-label={`Confirm deleting group ${groupName(g, gi)}`}>
            <span class="dot warn"></span>
            <div class="grid gap-2">
              <span>Delete <strong>{groupName(g, gi)}</strong> and its {plural((g.keys || []).length, "key")} ({(g.keys || []).map(keyName).join(", ")})? Requests stop using them at once.</span>
              <div class="flex flex-wrap gap-2"><button class="btn small danger" aria-disabled={app.busy} onclick={() => removeGroup(gi)}>Delete group<span class="sr"> {groupName(g, gi)}</span></button><button class="btn small keep" onclick={() => keep(`g:${gi}`)}>Keep it</button></div>
            </div>
          </div>
        {/if}
        <ul class="pk-keys">
          {#each g.keys || [] as k, ki (ki)}
            {@const t = traffic(g, k)}
            <li class="flex flex-wrap items-center gap-x-3 gap-y-1 py-1.5" class:target={!!k.auth_index && k.auth_index === target} data-key={k.auth_index || undefined} tabindex="-1">
              <span class="grid min-w-0 flex-[1_1_18ch] gap-0.5">
                <span class="mono break-all">{reveal ? k["api-key"] : keyName(k)}</span>
                <span class="small muted">{k.auth_index ? `ID ${short(k.auth_index)} · ` : ""}weight {k.weight ?? 1}{t !== null ? ` · ${plural(t, "request")} in 200 min` : ""}</span>
              </span>
              {#if confirm === `k:${gi}:${ki}`}
                <div class="pk-confirm grid basis-full gap-2" role="group" aria-label={`Remove ${keyName(k)}`}>
                  <p class="small wrap-anywhere">Remove <span class="mono">{keyName(k)}</span>{k.auth_index ? ` (ID ${short(k.auth_index)})` : ""} from {groupName(g, gi)}?{(g.keys || []).length === 1 ? " It is the group's only key; the group stays, with no key." : ""} Tools whose requests it serves move to other accounts.</p>
                  <div class="flex flex-wrap gap-2"><button class="btn small danger" aria-disabled={app.busy} onclick={() => removeKey(gi, ki)}>Remove {keyName(k)}</button><button class="btn small keep" onclick={() => keep(`k:${gi}:${ki}`)}>Keep it</button></div>
                </div>
              {:else}
                {#if k.auth_index}<a class="btn small quiet" href={`#accounts/${encodeURIComponent(k.auth_index)}`}>Account<span class="sr"> for {groupName(g, gi)} · {keyName(k)}</span></a>{/if}
                <button class="btn small quiet danger" data-ask={`k:${gi}:${ki}`} onclick={() => ask(`k:${gi}:${ki}`)}>Remove…<span class="sr"> {keyName(k)} from {groupName(g, gi)}</span></button>
              {/if}
            </li>
          {/each}
        </ul>
        {#if addingTo === gi}
          <form class="flex flex-wrap items-end gap-2" onsubmit={(e) => addKey(gi, e)}>
            <label class="field min-w-[12rem] grow">API key<input type="password" autocomplete="off" required bind:value={newKey.key} /></label>
            <label class="field w-28">Weight<input type="text" inputmode="numeric" pattern="[0-9]*" bind:value={newKey.weight} placeholder="1" /></label>
            <button class="btn small" aria-disabled={app.busy}>Add key</button>
            <button type="button" class="btn small quiet" onclick={() => ((addingTo = null), focusAfter(() => find(`[data-group="${gi}"] .add-key`)))}>Cancel</button>
          </form>
        {:else}
          <div><button class="btn small add-key" onclick={() => ((addingTo = gi), focusAfter(() => find(`[data-group="${gi}"] form input`)))}>Add a key to this group</button></div>
        {/if}
      </li>
    {/each}
  </ul>
{/if}

<form class="pk-add card grid gap-2 px-3.5 py-3" onsubmit={addGroup}>
  <h4 class="text-[14px]">Add a {label(family)} group</h4>
  <div class="grid gap-2 sm:grid-cols-3">
    <label class="field">Group name<input type="text" required bind:value={form.name} placeholder="For example, Console key" /></label>
    <label class="field">Base URL<input type="url" bind:value={form.url} placeholder="Provider default" /></label>
    <label class="field">API key<input type="password" autocomplete="off" required bind:value={form.key} /></label>
  </div>
  <div><button class="btn small primary" aria-disabled={app.busy}>Add group</button></div>
</form>

<style>
  .pk-keys li {
    border-top: 1px solid var(--line);
  }
  .pk-keys li:focus {
    outline: none;
  }
  .pk-keys li.target {
    margin-inline: -8px;
    padding-inline: 8px;
    border-radius: 6px;
    background: var(--sunk);
    box-shadow: inset 3px 0 0 var(--accent);
  }
  .pk-failure:focus {
    outline: none;
  }
</style>
