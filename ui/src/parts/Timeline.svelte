<script lang="ts">
  // The live timeline: one lane per account, one bar per request, from when it started to
  // when it finished. It shows at a glance which account is taking traffic, how long
  // requests run, where they overlap and where they fail.
  //
  // Sizing: the timeline takes its width from its container only (`contain: inline-size`)
  // and draws into a viewBox of that width, so it never holds a container wider than it is.
  //
  // Keyboard: one tab stop. Left and Right step through the requests in the order they
  // started, Home and End jump to the ends, Enter opens the request, Escape leaves it. The
  // line under the timeline reads the request out.
  import type { Request } from "../records";
  import type { Lane } from "./types";
  import type { Blind } from "../observe";
  import { duration, num, time } from "../fmt";

  let {
    lanes,
    requests,
    from,
    to,
    blind,
    selected,
    hover,
    onhover,
    onpick,
  }: {
    lanes: Lane[];
    requests: Request[];
    from: number;
    to: number;
    /** Time in the window this tab did not see: before watching, gaps, after stopping. */
    blind: Blind[];
    selected: number | null;
    /** The time under the pointer in any chart of the group, so all of them mark it. */
    hover: number | null;
    onhover: (t: number | null) => void;
    onpick: (r: Request) => void;
  } = $props();

  let width = $state(0);
  const W = $derived(Math.max(1, width));
  let tip = $state<{ r: Request; x: number; y: number } | null>(null);
  const narrow = $derived(W < 560);
  const label = $derived(narrow ? 128 : 230);
  const plot = $derived(Math.max(1, W - label - 8));
  const x = (t: number) => label + ((t - from) / (to - from)) * plot;
  const ROW = 11;
  const PAD = 8;
  const AXIS = 22;
  const whyText: Record<Blind["why"], string> = { before: "Before this tab was watching", gap: "Not captured", stopped: "Not watching" };
  /** Cut `text` to `fit` characters with an ellipsis. */
  const cut = (text: string, fit: number) => (text.length > fit ? `${text.slice(0, Math.max(1, fit - 1))}…` : text);
  /**
   * A lane's name as it fits. An API key's name ends in its shortened key ("Team gateway ·
   * sk-…b9ad"), which tells keys of one group apart, so the group part is cut, never the key.
   * On narrow screens an email keeps the part before @, followed by the provider.
   */
  function laneName(lane: Lane) {
    const [group, key] = lane.name.includes(" · ") ? lane.name.split(" · ") : [lane.name, ""];
    const tail = key ? ` · ${key}` : "";
    if (!narrow) return `${cut(group, Math.floor((label - 78) / 7.4) - tail.length)}${tail}`;
    const base = !key && group.includes("@") ? group.split("@")[0] : group;
    const sub = lane.sub ? ` · ${lane.sub}` : "";
    return `${cut(base, Math.max(4, Math.floor((label - 18) / 7.2) - sub.length - tail.length))}${tail}${key ? "" : sub}`;
  }

  /** Requests of each lane packed into rows, so overlapping requests sit one above another. */
  const packed = $derived(
    lanes.map((lane) => {
      const mine = requests.filter((r) => r.auth === lane.key && r.at + r.latency > from && r.at < to).sort((a, b) => a.at - b.at);
      const ends: number[] = [];
      const bars = mine.map((r) => {
        let row = ends.findIndex((end) => end <= r.at);
        // ponytail: at most 4 rows a lane; a fifth overlapping request shares the row that
        // frees up first. Widen the lane if accounts routinely run more in parallel.
        if (row < 0) row = ends.length < 4 ? ends.length : ends.indexOf(Math.min(...ends));
        ends[row] = r.at + r.latency;
        return { r, row };
      });
      return { lane, bars, rows: Math.max(1, ends.length), count: mine.length };
    }),
  );
  const tops = $derived.by(() => {
    let y = AXIS;
    return packed.map((p) => {
      const top = y;
      y += p.rows * ROW + PAD * 2 - 3;
      return top;
    });
  });
  const height = $derived((tops.at(-1) ?? AXIS) + (packed.at(-1) ? packed.at(-1)!.rows * ROW + PAD * 2 - 3 : 0));
  const ticks = $derived.by(() => {
    const span = to - from;
    // A label needs about 80px; the last one must not run into "now".
    const most = Math.max(2, Math.floor(plot / 80));
    const step = [10_000, 30_000, 60_000, 120_000, 300_000, 600_000, 900_000, 1_800_000].find((s) => span / s <= most) ?? 1_800_000;
    const out: number[] = [];
    for (let t = Math.ceil(from / step) * step; t < to - (span * 56) / plot; t += step) out.push(t);
    return { list: out, seconds: step < 60_000 };
  });
  const shade = $derived(blind.map((g) => ({ x: x(g.from), w: x(g.to) - x(g.from), why: whyText[g.why] })));
  function move(e: PointerEvent) {
    const box = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const px = ((e.clientX - box.left) / box.width) * W;
    onhover(px < label ? null : from + ((px - label) / plot) * (to - from));
  }

  // ---- keyboard -------------------------------------------------------------------------
  const order = $derived(packed.flatMap((p) => p.bars.map((b) => b.r)).sort((a, b) => a.at - b.at));
  let cursor = $state<number | null>(null);
  const current = $derived(cursor === null ? undefined : order.find((r) => r.n === cursor));
  const laneOf = (r: Request) => lanes.find((l) => l.key === r.auth);
  function key(e: KeyboardEvent) {
    if (!order.length) return;
    const at = current ? order.indexOf(current) : -1;
    let next: number | null = null;
    if (e.key === "ArrowRight") next = at < 0 ? order.length - 1 : Math.min(order.length - 1, at + 1);
    else if (e.key === "ArrowLeft") next = at < 0 ? order.length - 1 : Math.max(0, at - 1);
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = order.length - 1;
    else if (e.key === "Enter" && current) {
      e.preventDefault();
      return onpick(current);
    } else if (e.key === "Escape" && cursor !== null) {
      e.preventDefault();
      e.stopPropagation();
      cursor = null;
      return onhover(null);
    }
    if (next === null) return;
    e.preventDefault();
    cursor = order[next].n;
    onhover(order[next].at + order[next].latency / 2);
  }
  const describe = (r: Request) => {
    const lane = laneOf(r);
    return `${r.status || "Failed"} ${r.model} on ${lane ? `${lane.name}${lane.sub ? ` (${lane.sub})` : ""}` : "an unknown account"}, started ${time(r.at)}, took ${duration(r.latency)}`;
  };
</script>

<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions: one tab stop; arrow keys move between requests and the line below reads each one out -->
<div
  class="timeline"
  bind:clientWidth={width}
  onpointermove={move}
  onpointerleave={() => (onhover(null), (tip = null))}
  role="group"
  aria-label={`Request timeline: ${requests.length} requests across ${lanes.length} accounts between ${time(from)} and ${time(to)}.${order.length ? " Left and right arrows step through requests; Enter opens one." : ""} The table below lists the same requests.`}
  tabindex={order.length ? 0 : -1}
  onkeydown={key}
  onblur={() => cursor !== null && ((cursor = null), onhover(null))}
>
  <svg width="100%" {height} viewBox={`0 0 ${W} ${height}`} preserveAspectRatio="none" aria-hidden="true">
    <defs>
      <pattern id="blind" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><rect width="2" height="6" class="hatch" /></pattern>
      <clipPath id="tl-plot"><rect x={label} y="0" width={plot} {height} /></clipPath>
    </defs>
    {#each ticks.list as t}
      <line class="grid" x1={x(t)} x2={x(t)} y1={AXIS - 4} y2={height} />
      <text class="tick" x={x(t)} y="13" text-anchor="middle">{time(t, ticks.seconds)}</text>
    {/each}
    <text class="tick" x={label + plot} y="13" text-anchor="end">now</text>
    {#each packed as p, i (p.lane.key)}
      <line class="rule" x1="0" x2={W} y1={tops[i]} y2={tops[i]} />
      <circle class="state {p.lane.state}" cx="5" cy={tops[i] + PAD + 4} r="4"><title>{{ ok: "Can serve", warn: "Limited or cooling down", bad: "Needs attention", off: "Disabled" }[p.lane.state]}</title></circle>
      <text class="name" x="15" y={tops[i] + PAD + 8.5}>{laneName(p.lane)}<title>{p.lane.name}{p.lane.sub ? ` (${p.lane.sub})` : ""}</title></text>
      {#if !narrow}<text class="sub" x={label - 12} y={tops[i] + PAD + 8.5} text-anchor="end">{p.lane.sub}</text>{/if}
    {/each}
    <line class="rule" x1="0" x2={W} y1={height - 0.5} y2={height - 0.5} />
    <g clip-path="url(#tl-plot)">
      {#each shade as b}<rect x={b.x} y={AXIS} width={Math.max(0, b.w)} height={height - AXIS} fill="url(#blind)"><title>{b.why}</title></rect>{/each}
      {#each packed as p, i (p.lane.key)}
        {#each p.bars as { r, row } (r.n)}
          <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions: the focused timeline takes Enter for the request under its cursor -->
          <rect
            class="bar"
            class:failed={r.failed}
            class:sel={selected === r.n}
            class:cursor={cursor === r.n}
            x={x(r.at)}
            y={tops[i] + PAD + row * ROW}
            width={Math.max(3, x(r.at + r.latency) - x(r.at) - 1)}
            height={ROW - 3}
            rx="2"
            onclick={() => onpick(r)}
            onpointerenter={(e) => (tip = { r, x: e.clientX, y: e.clientY })}
            onpointerleave={() => (tip = null)}
          />
        {/each}
      {/each}
      {#if hover !== null && hover >= from && hover <= to}<line class="cross" x1={x(hover)} x2={x(hover)} y1={AXIS - 4} y2={height} />{/if}
    </g>
  </svg>
  {#if tip}
    <div class="tip" class:left={tip.x > innerWidth - 260} style:left={`${tip.x}px`} style:top={`${tip.y}px`} role="presentation">
      <strong class:bad={tip.r.failed}>{tip.r.status || "Failed"}</strong> <span class="mono">{tip.r.model}</span><br />
      Started {time(tip.r.at)}, took {duration(tip.r.latency)}<br />
      {#if !tip.r.failed}{num(tip.r.tokens.input)} in, {num(tip.r.tokens.output)} out{:else}{tip.r.failure?.code || "failed"}{/if}
    </div>
  {/if}
</div>
<p class="readout small" aria-live="polite">{current ? `${describe(current)}. Enter opens it.` : ""}</p>

<style>
  .timeline {
    position: relative;
    contain: inline-size;
    min-width: 0;
    min-height: 60px;
    border-radius: 4px;
  }
  .timeline:focus-visible {
    outline-offset: 3px;
  }
  svg {
    display: block;
    overflow: hidden;
  }
  .readout {
    min-height: 0;
    color: var(--ink-2);
  }
  .readout:empty {
    display: none;
  }
  .grid,
  .rule {
    stroke: var(--line);
    stroke-width: 1;
  }
  .cross {
    stroke: var(--ink-2);
    stroke-width: 1;
    pointer-events: none;
  }
  .tick,
  .sub {
    fill: var(--ink-2);
    font-size: 12px;
  }
  .name {
    fill: var(--ink);
    font-size: 12.5px;
  }
  .hatch {
    fill: var(--line-2);
    opacity: 0.6;
  }
  .bar {
    fill: var(--ink-2);
    cursor: pointer;
  }
  .bar:hover {
    fill: var(--ink);
  }
  .bar.failed {
    fill: var(--bad);
  }
  .bar.sel,
  .bar.cursor {
    fill: var(--accent);
    stroke: var(--accent);
    stroke-width: 2;
  }
  .tip {
    position: fixed;
    z-index: 45;
    transform: translate(12px, 14px);
    padding: 7px 10px;
    border: 1px solid var(--line-2);
    border-radius: 6px;
    background: var(--surface);
    box-shadow: 0 6px 20px rgb(0 0 0 / 0.4);
    font-size: 13px;
    line-height: 1.5;
    pointer-events: none;
    white-space: nowrap;
  }
  .tip.left {
    transform: translate(calc(-100% - 12px), 14px);
  }
  .state {
    fill: var(--ok);
  }
  .state.warn {
    fill: var(--warn);
  }
  .state.bad {
    fill: var(--bad);
  }
  .state.off {
    fill: none;
    stroke: var(--ink-2);
    stroke-width: 1.5;
  }
  .tip .bad {
    color: var(--bad);
  }
</style>
