<script lang="ts">
  import { store, Res, every } from "../store.svelte";
  import { api, missing, text } from "../api";
  import { buckets, sumBuckets, sum, label, provider, keepEvents, livePage, usageStats, type Buckets, type Data, type Event } from "../core";
  import Grille from "../Grille.svelte";
  import Missing from "../Missing.svelte";

  const keys = new Res<Data>(() => api("/observability/usage/api-keys"));
  keys.load();
  $effect(() => every(15_000, () => Promise.all([store.creds.load(true), keys.load(true)])));
  const toBuckets = (rows: Data[]): Buckets => ({
    total: rows.map((b) => (b.success || 0) + (b.failed || 0)),
    failed: rows.map((b) => b.failed || 0),
  });
  const rows = $derived.by(() => {
    const out = new Map<string, { name: string; via: string; b: Buckets | null }>();
    for (const a of store.creds.data || []) {
      const p = provider(a);
      const prev = out.get(`a:${p}`);
      out.set(`a:${p}`, { name: label(p), via: "Accounts", b: sumBuckets([prev?.b || null, buckets(a)]) });
    }
    for (const [p, entries] of Object.entries<Data>(keys.data || {}))
      out.set(`k:${p}`, {
        name: label(p),
        via: "API keys",
        b: sumBuckets(Object.values<Data>(entries).map((e) => toBuckets(e.recent_requests || []))),
      });
    return [...out.values()].sort((x, y) => sum(y.b?.total || []) - sum(x.b?.total || []));
  });
  const max = $derived(Math.max(1, ...rows.flatMap((r) => r.b?.total || [])));

  // The live view pages through GET /observability/usage/live by cursor. Reading takes no
  // record off the usage queue, so collectors and other tabs see every record too.
  const feed = "/observability/usage/live";
  let live = $state(false),
    cursor: number | null = null,
    // Bumped on start and stop, so a read that was in flight cannot move the new cursor.
    run = 0,
    events = $state.raw<Event[]>([]),
    problem = $state(""),
    missed = $state(false),
    statistics = $state(true);
  const stats = $derived(usageStats(events));
  const read = async (after: number | null) => {
    const page = livePage(await api(after === null ? feed : `${feed}?after=${after}`));
    statistics = page.statistics;
    return page;
  };
  const stop = () => {
    run++;
    live = false;
    cursor = null;
  };
  const start = async () => {
    const mine = ++run;
    try {
      const at = (await read(null)).cursor;
      if (mine !== run) return;
      cursor = at;
      problem = "";
      missed = false;
      live = true;
    } catch (e) {
      problem = e instanceof Error ? e.message : String(e);
    }
  };
  $effect(() =>
    every(
      3000,
      async () => {
        if (!live || cursor === null) return;
        const mine = run;
        try {
          // ponytail: one page (at most 500 records) every 3 s, about 166 records a second; past
          // that the tab falls behind and the server reports a gap. Read while `more` to lift it.
          const page = await read(cursor);
          if (mine !== run) return;
          cursor = page.cursor;
          if (page.gap) missed = true;
          if (page.events.length) events = keepEvents([...events, ...page.events], Date.now());
          problem = "";
        } catch (e) {
          if (mine !== run) return;
          problem = e instanceof Error ? e.message : String(e);
          stop();
        }
      },
      true,
    ),
  );
  const fmt = (n: number | null, unit = "") => (n === null ? "—" : `${n.toLocaleString()}${unit}`);
  const time = (t: number) => new Date(t).toLocaleTimeString([], { hourCycle: "h23", hour: "2-digit", minute: "2-digit", second: "2-digit" });
</script>

<div class="head"><h1>Usage</h1></div>
<Missing actions={[["GET", "/observability/usage/live?count=0", "the live request view"]]} />

<section class="section">
  <div class="section-head"><h2>Last 200 minutes</h2><span class="legend">Reported by the server · 10-minute buckets</span></div>
  {#if rows.length}
    <ul class="list">
      {#each rows as r}
        <li class="item usage-row">
          <span class="name grow"><strong>{r.name}</strong><small>{r.via}</small></span>
          {#if r.b}<Grille data={r.b} {max} /><span class="num count">{sum(r.b.total).toLocaleString()}</span><span class="num count legend"
              >{sum(r.b.failed) ? `${sum(r.b.failed)} failed` : ""}</span
            >{:else}<span class="legend">Not reported</span>{/if}
        </li>
      {/each}
    </ul>
  {:else}<p class="muted">{store.creds.error ? text(store.creds.error) : store.creds.data ? "No traffic sources yet." : "Loading…"}</p>{/if}
  {#if keys.error}<p class="note"><span class="lamp {missing(keys.error) ? 'off' : 'bad'}"></span>{missing(keys.error) ? "API-key traffic is not reported by this server." : text(keys.error)}</p>{/if}
</section>

<section class="section">
  <div class="section-head">
    <h2>Live requests</h2>
    <button
      class="key"
      disabled={!store.can("GET", feed)}
      aria-pressed={live}
      onclick={() => {
        if (live) stop();
        else start();
      }}><span class="lamp {live ? 'ok live' : 'off'}"></span>{live ? "Stop" : "Start live view"}</button
    >
  </div>
  {#if problem}<p class="note error" role="alert"><span class="lamp bad"></span>{problem}</p>{/if}
  {#if live && !statistics}<p class="note"><span class="lamp off"></span>Usage statistics are off on this server.</p>{/if}
  {#if missed}<p class="note"><span class="lamp warn"></span>Some requests were missed while this tab was hidden or behind.</p>{/if}
  {#if !events.length}
    <p class="muted">
      {live
        ? "Listening for requests…"
        : "Model, latency and tokens per request, kept in this tab only."}
    </p>
  {:else}
    <div class="window display live-readings">
      <div class="reading"><span class="legend">Requests</span><strong>{fmt(stats.count)}</strong><span class="legend">{stats.failed} failed</span></div>
      <div class="reading"><span class="legend">Median latency</span><strong>{fmt(stats.latency)}<small>&nbsp;ms</small></strong><span class="legend">first token {fmt(stats.ttft, " ms")}</span></div>
      <div class="reading"><span class="legend">Tokens</span><strong>{fmt(stats.input + stats.output)}</strong><span class="legend">{fmt(stats.input)} in · {fmt(stats.output)} out</span></div>
    </div>
    <ul class="list">
      <li class="item usage-model legend" aria-hidden="true"><span>Model</span><span class="count">Requests</span><span class="count">Failed</span><span class="count">Tokens</span></li>
      {#each stats.models as m (m.model)}
        <li class="item usage-model">
          <code class="ellipsis">{m.model}</code><span class="num count">{m.count}</span><span class="num count">{m.failed || ""}</span><span class="num count">{(m.input + m.output).toLocaleString()}</span>
        </li>
      {/each}
    </ul>
    <h3>Latest</h3>
    <ul class="list">
      {#each events.slice(-30).reverse() as e}
        <li class="item usage-event">
          <span class="lamp {e.failed ? 'bad' : 'ok'}"></span>
          <span class="legend">{time(e.at)}</span>
          <code class="ellipsis grow">{e.model}</code>
          <span class="num">{e.latency} ms · {e.failed ? `HTTP ${e.status || "?"}` : `${e.input + e.output} tok`}</span>
          <a class="legend mono ellipsis" href={`#logs/${encodeURIComponent(e.id)}`} title={e.id}>{e.id}</a>
        </li>
      {/each}
    </ul>
  {/if}
</section>
