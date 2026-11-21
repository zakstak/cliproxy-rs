<script lang="ts">
  // Usage: consumption over a period, from the server's history store (`GET /history/totals`).
  // Requests, failures and tokens per hour or day, for every account or one, grouped by
  // account, provider or model. The store keeps what its retention allows (30 days by default)
  // and survives restarts.
  import { accountName as credName } from "../account";
  import { label, provider, type Data } from "../core";
  import { api, ApiError } from "../api";
  import { app, message } from "../app.svelte";
  import type { Bar } from "../parts/types";
  import { compact, num, percent, plural, time } from "../fmt";
  import Bars from "../parts/Bars.svelte";

  type Group = "account" | "provider" | "model";
  type Totals = { start: number; key: string; requests: number; failed: number; input_tokens: number; output_tokens: number; reasoning_tokens: number; cache_read_tokens: number; cache_creation_tokens: number; total_tokens: number };
  const HOUR = 3_600_000;
  const DAY = 24 * HOUR;
  const periods = [
    { id: "24h", name: "Last 24 hours", span: DAY, bucket: HOUR, step: "hour" },
    { id: "7d", name: "Last 7 days", span: 7 * DAY, bucket: DAY, step: "day" },
    { id: "30d", name: "Last 30 days", span: 30 * DAY, bucket: DAY, step: "day" },
  ] as const;

  let group = $state<Group>("account");
  let periodId = $state<(typeof periods)[number]["id"]>("24h");
  /** "" for every account, or one account's auth index. */
  let scope = $state(app.route.query.get("account") || "");
  const period = $derived(periods.find((p) => p.id === periodId) ?? periods[0]);

  const accounts = $derived(app.accounts.data || []);
  const byIndex = $derived(new Map(accounts.map((a: Data) => [String(a.auth_index), a])));

  let series = $state.raw<Totals[] | null>(null);
  let groups = $state.raw<Totals[] | null>(null);
  let phase = $state<"loading" | "ok" | "off">("loading");
  let error = $state("");
  let readAt = $state(0);
  let from = $state(0);
  let to = $state(0);

  const parse = (raw: unknown): Totals[] => {
    const rows = raw && typeof raw === "object" && Array.isArray((raw as Data).totals) ? (raw as Data).totals : null;
    if (!rows) throw new Error("Unexpected history format.");
    return rows.filter((r: unknown): r is Totals => !!r && typeof r === "object" && typeof (r as Totals).start === "number");
  };

  let run = 0;
  async function read() {
    const mine = ++run;
    const end = Math.ceil(Date.now() / period.bucket) * period.bucket;
    const start = end - period.span;
    const q = `since=${start}&until=${end}${scope ? `&account=${encodeURIComponent(scope)}` : ""}`;
    try {
      const [s, g] = await Promise.all([
        api(`/history/totals?${q}&bucket=${period.bucket}`),
        api(`/history/totals?${q}&bucket=${period.span}&group=${group}`),
      ]);
      if (mine !== run) return;
      series = parse(s);
      groups = parse(g);
      [from, to] = [start, end];
      phase = "ok";
      error = "";
      readAt = Date.now();
    } catch (e) {
      if (mine !== run) return;
      if (e instanceof ApiError && e.status === 503) {
        phase = "off";
        error = "";
      } else {
        error = message(e);
        if (phase === "loading") phase = "ok";
      }
    }
  }
  $effect(() => {
    void [periodId, group, scope];
    read();
    const timer = setInterval(() => document.hidden || read(), 60_000);
    return () => clearInterval(timer);
  });

  const total = $derived(
    (series || []).reduce(
      (t, r) => ({ requests: t.requests + r.requests, failed: t.failed + r.failed, input: t.input + r.input_tokens, cacheRead: t.cacheRead + r.cache_read_tokens, output: t.output + r.output_tokens }),
      { requests: 0, failed: 0, input: 0, cacheRead: 0, output: 0 },
    ),
  );
  const stepLabel = (t: number) =>
    period.bucket === HOUR ? new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : new Date(t).toLocaleDateString([], { month: "short", day: "numeric" });
  const bars = $derived<Bar[]>(
    from
      ? Array.from({ length: Math.round((to - from) / period.bucket) }, (_, i) => {
          const t = from + i * period.bucket;
          const r = (series || []).find((x) => x.start === t);
          const value = r?.requests ?? 0;
          const failed = r?.failed ?? 0;
          return { label: stepLabel(t), value, flagged: failed, tip: `${stepLabel(t)}: ${plural(value, "request")}${failed ? `, ${num(failed)} failed` : ""}` };
        })
      : [],
  );

  const name = (key: string) => {
    if (group === "provider") return { name: label(key), sub: "" };
    if (group === "model") return { name: key || "unknown", sub: "" };
    const a = byIndex.get(key);
    return a ? { name: credName(a), sub: label(provider(a)) } : { name: key ? `Account ${key}` : "Unknown account", sub: "removed or not loaded" };
  };
  const rows = $derived((groups || []).map((r) => ({ ...r, ...name(r.key) })).sort((x, y) => y.requests - x.requests));
</script>

<div class="page">
  <div class="head">
    <h1 class="usage-heading" tabindex="-1">Usage</h1>
    <div class="row min-w-0">
      <label class="block min-w-0">
        <span class="sr">Period</span>
        <select bind:value={periodId}>
          {#each periods as p}<option value={p.id}>{p.name}</option>{/each}
        </select>
      </label>
      <!-- An option names the whole account, however long; the closed control never widens the page. -->
      <label class="block min-w-0 max-w-full">
        <span class="sr">Accounts to count</span>
        <select class="max-w-full" bind:value={scope}>
          <option value="">All accounts</option>
          {#each accounts as a (a.auth_index)}<option value={a.auth_index}>{label(provider(a))}: {credName(a)}</option>{/each}
        </select>
      </label>
    </div>
  </div>

  {#if error}
    <p class="notice bad small" role="alert" style="margin-bottom:12px">
      <span class="dot bad"></span>
      <span class="grow">Could not read usage: {error}{readAt ? ` Figures are from ${time(readAt)}.` : ""}</span>
      <button class="btn small" onclick={read}>Try again</button>
    </p>
  {/if}

  {#if phase === "loading"}
    <p class="empty muted">Reading usage…</p>
  {:else if phase === "off"}
    <div class="empty">
      <h2>The server records no history</h2>
      <p class="muted">Usage comes from the server's history store, which is off (<span class="mono">storage.backend: none</span>) or failed to open. The server log says which.</p>
    </div>
  {:else}
    <p class="muted small scope">{period.name}, per {period.step}, from the server's history. It survives restarts and is kept for as long as the server's retention allows.</p>
    <div class="figures">
      <div class="figure">
        <span class="k">Requests</span>
        <span class="v">{compact(total.requests)}</span>
      </div>
      <div class="figure">
        <span class="k">Failed</span>
        <span class="v">{compact(total.failed)}</span>
        <span class="k">{percent(total.failed, total.requests)} of requests</span>
      </div>
      <div class="figure">
        <span class="k">Input tokens</span>
        <span class="v">{compact(total.input)}</span>
        <span class="k">{percent(total.cacheRead, total.input)} read from cache</span>
      </div>
      <div class="figure">
        <span class="k">Output tokens</span>
        <span class="v">{compact(total.output)}</span>
      </div>
    </div>
    <section class="section">
      <h2>Requests per {period.step}</h2>
      <div class="card pad">
        <Bars {bars} name="Succeeded" flaggedName="Failed" summary={`${plural(total.requests, "request")} in the ${period.name.toLowerCase()}, ${num(total.failed)} failed.`} />
      </div>
    </section>
    <section class="section">
      <div class="row mb-2.5">
        <h2 class="grow">By {group}</h2>
        <div class="seg" role="group" aria-label="Group by">
          <button aria-pressed={group === "account"} onclick={() => (group = "account")}>Account</button>
          <button aria-pressed={group === "provider"} onclick={() => (group = "provider")}>Provider</button>
          <button aria-pressed={group === "model"} onclick={() => (group = "model")}>Model</button>
        </div>
      </div>
      {#if !rows.length}
        <p class="empty muted">No requests in this period.</p>
      {:else}
        <div class="card table-wrap">
          <table>
            <thead>
              <tr>
                <th>{group === "account" ? "Account" : group === "provider" ? "Provider" : "Model"}</th>
                <th class="num">Requests</th><th class="num">Failed</th>
                <th class="num c-mid">Input</th><th class="num c-wide">Cache read</th><th class="num c-mid">Output</th>
              </tr>
            </thead>
            <tbody>
              {#each rows as r (r.key)}
                <tr>
                  <td class="wrap-anywhere">
                    {#if group === "account" && r.key}<a href={`#logs/requests?account=${encodeURIComponent(r.key)}`}>{r.name}</a>{:else}<span class={group === "model" ? "mono" : ""}>{r.name}</span>{/if}
                    {#if r.sub}<span class="faint">{r.sub}</span>{/if}
                  </td>
                  <td class="num">{num(r.requests)}</td>
                  <td class="num">{#if r.failed}<span class="code-status bad">{num(r.failed)}</span>{:else}0{/if}</td>
                  <td class="num c-mid">{compact(r.input_tokens)}</td>
                  <td class="num c-wide">{percent(r.cache_read_tokens, r.input_tokens)}</td>
                  <td class="num c-mid">{compact(r.output_tokens)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </section>
  {/if}
</div>

<style>
  .scope {
    max-width: 92ch;
    margin-bottom: 14px;
  }
</style>
