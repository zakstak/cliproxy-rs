<script lang="ts">
  // Usage: consumption over a period the server remembers. Today that is one period: request
  // and failure counts per account for the last 200 minutes, in twenty 10-minute steps, held
  // in the server's memory. It holds no tokens, so the screen says so instead of showing 0.
  // What is happening right now, tokens and cache included, is on Activity.
  import { accountName as credName } from "../account";
  import { buckets, label, provider, sum, sumBuckets, type Buckets, type Data } from "../core";
  import { app } from "../app.svelte";
  import { find, retried } from "../focus";
  import type { Bar } from "../parts/types";
  import { compact, num, percent, plural, time } from "../fmt";
  import Bars from "../parts/Bars.svelte";

  let group = $state<"account" | "provider">("account");
  /** "" for everything, "p:<provider>" or an account's auth index. */
  let scope = $state(app.route.query.get("account") || "");

  const accounts = $derived(app.accounts.data || []);
  const providers = $derived([...new Set(accounts.map(provider))]);
  const inScope = (a: Data) => !scope || (scope.startsWith("p:") ? scope.slice(2) === provider(a) : scope === a.auth_index);
  const counted = $derived(accounts.filter(inScope).map((a) => ({ a, b: buckets(a) })));
  const all = $derived(sumBuckets(counted.map((c) => c.b)));
  const times = $derived<string[]>((accounts.find((a) => Array.isArray(a.recent_requests))?.recent_requests || []).map((b: Data) => String(b.time || "")));
  const bars = $derived<Bar[]>(
    (all?.total || []).map((total, i) => {
      const failed = all!.failed[i];
      return { label: times[i]?.split("-")[0] || "", value: total, flagged: failed, tip: `${times[i]?.replace("-", " to ") || ""}: ${plural(total, "request")}${failed ? `, ${num(failed)} failed` : ""}` };
    }),
  );
  type Row = { key: string; name: string; sub: string; link: string; requests: number | null; failed: number | null };
  const rows = $derived<Row[]>(
    (group === "account"
      ? counted.map(({ a, b }) => ({ key: String(a.auth_index), name: credName(a), sub: label(provider(a)), link: String(a.auth_index), requests: b && sum(b.total), failed: b && sum(b.failed) }))
      : [...new Set(counted.map((c) => provider(c.a)))].map((p) => {
          const mine = counted.filter((c) => provider(c.a) === p);
          const b: Buckets | null = sumBuckets(mine.map((c) => c.b));
          return { key: p, name: label(p), sub: plural(mine.length, "account"), link: "", requests: b && sum(b.total), failed: b && sum(b.failed) };
        })
    ).sort((x, y) => (y.requests ?? -1) - (x.requests ?? -1)),
  );
</script>

<div class="page">
  <div class="head">
    <h1 class="usage-heading" tabindex="-1">Usage</h1>
    <!-- An option names the whole account, however long; the closed control never widens the page. -->
    <label class="block min-w-0 max-w-full">
      <span class="sr">Accounts to count</span>
      <select class="max-w-full" bind:value={scope}>
        <option value="">All accounts</option>
        {#each providers as p}<option value={`p:${p}`}>All {label(p)} accounts</option>{/each}
        {#each accounts as a (a.auth_index)}<option value={a.auth_index}>{label(provider(a))}: {credName(a)}</option>{/each}
      </select>
    </label>
  </div>

  {#if app.accounts.error}
    <p class="notice bad small" role="alert" style="margin-bottom:12px">
      <span class="dot bad"></span>
      <span class="grow">Could not refresh: {app.accounts.error.message}{app.accounts.at ? ` Counts are from ${time(app.accounts.at)}.` : ""}</span>
      <button class="btn small" aria-disabled={app.accounts.loading} onclick={() => retried(app.accounts.load().then((r) => r.ok), () => find(".usage-heading"), () => app.notify("Request counts read again."))}>{app.accounts.loading ? "Reading…" : "Try again"}</button>
    </p>
  {/if}

  <p class="muted small scope">
    The last 200 minutes{times.length ? `, ${times[0].split("-")[0]} to ${times.at(-1)?.split("-")[1]}` : ""}, as the server counted them in 10-minute steps. This is all the history the server keeps
    today: the counts live in its memory, start again when it restarts, and do not include tokens.
  </p>

  {#if !app.accounts.data}
    <p class="empty muted">{app.accounts.error ? "" : "Reading request counts…"}</p>
  {:else if !accounts.length}
    <div class="empty">
      <h2>No accounts yet</h2>
      <p class="muted">Usage is counted per account. Add one, point a tool at the proxy, and requests show up here.</p>
      <div class="row"><a class="btn primary" href="#accounts">Add an account</a><a class="btn" href="#tools">Set up a tool</a></div>
    </div>
  {:else}
    <div class="figures">
      <div class="figure">
        <span class="k">Requests</span>
        {#if all}<span class="v">{compact(sum(all.total))}</span>{:else}<span class="v none">Not reported by this server</span>{/if}
      </div>
      <div class="figure">
        <span class="k">Failed</span>
        {#if all}
          <span class="v">{compact(sum(all.failed))}</span>
          <span class="k">{percent(sum(all.failed), sum(all.total))} of requests</span>
        {:else}<span class="v none">Not reported by this server</span>{/if}
      </div>
      <div class="figure col-span-full md:col-span-2">
        <span class="k">Tokens and cache</span>
        <span class="v none">Not recorded for this period. The server counts requests, not tokens.</span>
        <span><a class="small inline-flex min-h-11 items-center min-[861px]:min-h-0" href="#logs">Tokens and cache for requests finishing now are on Activity</a></span>
      </div>
    </div>
    {#if all}
      <section class="section">
        <h2>Requests per 10 minutes</h2>
        <div class="card pad">
          <Bars {bars} name="Succeeded" flaggedName="Failed" summary={`${plural(sum(all.total), "request")} in 200 minutes, ${num(sum(all.failed))} failed.`} />
        </div>
      </section>
    {/if}
    <section class="section">
      <div class="row mb-2.5">
        <h2 class="grow">By {group}</h2>
        <div class="seg" role="group" aria-label="Group by">
          <button aria-pressed={group === "account"} onclick={() => (group = "account")}>Account</button>
          <button aria-pressed={group === "provider"} onclick={() => (group = "provider")}>Provider</button>
        </div>
      </div>
      <div class="card table-wrap">
        <table>
          <thead>
            <tr>
              <th>{group === "account" ? "Account" : "Provider"}</th><th class="num">Requests</th><th class="num">Failed</th>
              {#if group === "account"}<th class="hidden md:table-cell"><span class="sr">Details</span></th>{/if}
            </tr>
          </thead>
          <tbody>
            {#each rows as r (r.key)}
              <tr>
                <td class="wrap-anywhere">
                  {r.name} <span class="faint">{r.sub}</span>
                  {#if group === "account"}<a class="flex min-h-11 items-center text-[13px] md:hidden" href={`#logs/requests?account=${encodeURIComponent(r.link)}`}>Live activity<span class="sr"> for {r.name} ({r.sub})</span></a>{/if}
                </td>
                {#if r.requests === null}
                  <td class="num muted" colspan="2">Not reported</td>
                {:else}
                  <td class="num">{num(r.requests)}</td>
                  <td class="num">{#if r.failed}<span class="code-status bad">{num(r.failed)}</span>{:else}0{/if}</td>
                {/if}
                {#if group === "account"}
                  <td class="num hidden md:table-cell"><a class="small" href={`#logs/requests?account=${encodeURIComponent(r.link)}`}>Live activity<span class="sr"> for {r.name} ({r.sub})</span></a></td>
                {/if}
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </section>
  {/if}
</div>

<style>
  .scope {
    max-width: 92ch;
    margin-bottom: 14px;
  }
</style>
