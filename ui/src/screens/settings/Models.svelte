<script lang="ts">
  // Model aliases, exclusions and the static catalog, per sign-in channel. Writes are frozen
  // operations (server.ts); removing an alias asks first, as other removals do.
  import { Effect } from "effect";
  import { onDestroy } from "svelte";
  import { label, signIns, type Data } from "../../core";
  import { app, Res, server } from "../../app.svelte";
  import { configWrite, explain, type ConfigWrite, type WriteError } from "../../server";
  import { focusAfter, find, retried } from "../../focus";

  let channel = $state("claude");
  const aliases = $derived(app.setting<Data[]>(`oauth/model-alias/${channel}`, []));
  const excluded = $derived(app.setting<string[]>(`oauth/excluded-models/${channel}`, []));
  let catalog = $state.raw<Res<Data[]>>();
  $effect(() => {
    const c = channel;
    const r = new Res<Data[]>(
      server.read(`/routing/model-definitions/${encodeURIComponent(c)}`).pipe(
        Effect.map((b) => (b && typeof b === "object" && "models" in b && Array.isArray(b.models) ? b.models.filter((m): m is Data => !!m && typeof m === "object") : [])),
      ),
    );
    catalog = r;
    r.load();
  });
  const k = (n: number) => (n >= 1_000_000 ? `${n / 1_000_000}M` : n >= 1000 ? `${Math.round(n / 1000)}k` : n ? String(n) : "—");

  let failure = $state<{ op: ConfigWrite; done: string; error: WriteError } | null>(null);
  let context = 0;
  let gone = false;
  onDestroy(() => { gone = true; });
  function select(value: string) {
    context++;
    channel = value;
    failure = null;
    removing = -1;
  }
  const operation = (path: string, before: unknown, next: unknown, what: string) =>
    configWrite({ path, before, next, what: `${what} (${label(channel)})`, href: "#settings/models" });
  async function write(op: ConfigWrite, done: string) {
    const started = context;
    failure = null;
    const error = await app.write(op, done);
    if (gone || started !== context) {
      if (error) app.failedAway(op, error, done);
      return false;
    }
    if (error) {
      failure = { op, done, error };
      focusAfter(() => find(".models-failure"));
    }
    return !error;
  }
  let alias = $state({ name: "", alias: "", fork: false });
  let aliasError = $state("");
  async function addAlias(e: SubmitEvent) {
    e.preventDefault();
    const next = { name: alias.name.trim(), alias: alias.alias.trim(), ...(alias.fork ? { fork: true } : {}) };
    aliasError = aliases.some((a) => a.alias === next.alias) ? `The alias ${next.alias} already exists for ${label(channel)}.` : "";
    if (aliasError) return void focusAfter(() => find("#alias-name"));
    const path = `oauth/model-alias/${channel}`;
    if (await write(operation(path, aliases, [...aliases, next], `Adding alias ${next.alias}`), `Alias ${next.alias} added.`)) alias = { name: "", alias: "", fork: false };
  }
  /** The alias being confirmed for removal, by index; -1 for none. */
  let removing = $state(-1);
  function askRemove(i: number) {
    const was = removing;
    removing = i;
    focusAfter(() => (i < 0 ? find(`[data-alias="${was}"]`) : find(".alias-confirm .keep")));
  }
  async function removeAlias(i: number) {
    const a = aliases[i];
    const path = `oauth/model-alias/${channel}`;
    if (await write(operation(path, aliases, aliases.filter((_, n) => n !== i), `Removing alias ${a.alias}`), `Alias ${a.alias} removed. Tools asking for ${a.alias} now get an unknown-model error.`)) {
      removing = -1;
      focusAfter(() => find("#alias-model"));
    }
  }
  let pattern = $state("");
  async function exclude(e: SubmitEvent) {
    e.preventDefault();
    const p = pattern.trim();
    const path = `oauth/excluded-models/${channel}`;
    if (await write(operation(path, excluded, [...new Set([...excluded, p])], `Excluding ${p}`), `${p} excluded.`)) pattern = "";
  }
  async function allow(i: number) {
    const m = excluded[i];
    const path = `oauth/excluded-models/${channel}`;
    if (await write(operation(path, excluded, excluded.filter((_, n) => n !== i), `Allowing ${m}`), `${m} allowed again.`)) focusAfter(() => find("#exclude-pattern"));
  }
</script>

<div class="seg w-fit" role="group" aria-label="Channel">
  {#each signIns as c (c)}<button aria-pressed={channel === c} onclick={() => select(c)}>{label(c)}</button>{/each}
</div>
{#if failure}
  <div class="notice bad small models-failure" role="alert" tabindex="-1">
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

<section class="group">
  <h3>Aliases</h3>
  <p class="small muted">Tools ask for the alias; the provider receives the model.</p>
  {#if aliases.length}
    <ul class="lines">
      {#each aliases as a, i (i)}
        <li>
          <span class="mono grow">{a.alias} → {a.name}</span>{#if a.fork}<span class="small muted">keeps the original name too</span>{/if}
          {#if removing === i}
            <div class="alias-confirm grid basis-full gap-2" role="group" aria-label={`Remove alias ${a.alias}`}>
              <p class="small">Remove the alias <span class="mono">{a.alias}</span>? Tools that ask for {a.alias} stop getting {a.name}.</p>
              <div class="flex flex-wrap gap-2"><button class="btn small danger" aria-disabled={app.busy} onclick={() => removeAlias(i)}>Remove {a.alias}</button><button class="btn small keep" onclick={() => askRemove(-1)}>Keep it</button></div>
            </div>
          {:else}
            <button class="btn small quiet danger" data-alias={i} onclick={() => askRemove(i)}>Remove…<span class="sr"> alias {a.alias}</span></button>
          {/if}
        </li>
      {/each}
    </ul>
  {:else}<p class="small muted">No aliases for {label(channel)}. Tools use the provider's model names.</p>{/if}
  <form class="form-row" onsubmit={addAlias}>
    <label class="field">Model<input id="alias-model" type="text" list="defs" required bind:value={alias.name} /></label>
    <label class="field">Alias<input id="alias-name" type="text" required bind:value={alias.alias} aria-invalid={!!aliasError} aria-describedby={aliasError ? "alias-error" : undefined} /></label>
    <label class="check small"><input type="checkbox" bind:checked={alias.fork} />Keep the original name too</label>
    <button class="btn small" aria-disabled={app.busy}>Add alias</button>
  </form>
  {#if aliasError}<p class="small code-status bad" id="alias-error" role="alert">{aliasError}</p>{/if}
</section>

<section class="group">
  <h3>Excluded models</h3>
  <p class="small muted">Hidden from model lists and refused. Wildcards such as *-preview work.</p>
  {#if excluded.length}
    <ul class="lines">
      {#each excluded as m, i (i)}<li><span class="mono grow">{m}</span><button class="btn small quiet" aria-disabled={app.busy} onclick={() => allow(i)}>Allow again<span class="sr"> {m}</span></button></li>{/each}
    </ul>
  {:else}<p class="small muted">Every {label(channel)} model is available.</p>{/if}
  <form class="form-row" onsubmit={exclude}>
    <label class="field">Model or pattern<input id="exclude-pattern" type="text" required bind:value={pattern} placeholder="*-preview" /></label>
    <button class="btn small" aria-disabled={app.busy}>Exclude</button>
  </form>
</section>

<section class="group">
  <h3 id="catalog-heading" class="focus:outline-none" tabindex="-1">Catalog {#if catalog?.data}<span class="small muted">{catalog.data.length} models</span>{/if}</h3>
  {#if !catalog || (catalog.loading && !catalog.data)}<p class="small muted">Reading the catalog…</p>
  {:else if catalog.error}<p class="notice bad small" role="alert"><span class="dot bad"></span><span class="grow">Could not read the catalog: {catalog.error.message}</span><button class="btn small" onclick={() => catalog && retried(catalog.load().then((r) => r.ok), () => find("#catalog-heading"), () => app.notify("Catalog read."))}>Try again</button></p>
  {:else if catalog.data?.length}
    <div class="table-wrap card">
      <table>
        <thead><tr><th scope="col">Model</th><th scope="col">Name</th><th scope="col" class="num">Context</th><th scope="col" class="num">Output</th></tr></thead>
        <tbody>{#each catalog.data as m (m.id)}<tr><td class="mono">{m.id}</td><td>{m.display_name || ""}</td><td class="num">{k(m.context_length)}</td><td class="num">{k(m.max_completion_tokens)}</td></tr>{/each}</tbody>
      </table>
    </div>
    <datalist id="defs">{#each catalog.data as m}<option value={m.id}></option>{/each}</datalist>
  {:else}<p class="small muted">The server has no static model definitions for {label(channel)}.</p>{/if}
</section>
