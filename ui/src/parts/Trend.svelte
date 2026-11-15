<script lang="ts">
  // One live measure: its total for the window, and how it moved across the window. Bars
  // for counts (optionally split in two), dots for a per-request value such as first-token
  // time. All trends of a screen share one time axis and one pointer position.
  //
  // Sizing: the plot takes its width from its container only (`contain: inline-size`), and
  // the SVG fills it through a viewBox, so a narrower container (a detail opening, a window
  // resize) can never be pushed wider by an old drawing.
  //
  // Keyboard: the plot is one tab stop. Left and Right step through the steps (or the
  // dots), Home and End jump to the ends, Enter opens the request under a dot, Escape
  // leaves the readout. The figure above reads the step out, as the pointer does.
  import type { Request } from "../records";
  import { watchedMs, type Blind } from "../observe";
  import type { Step } from "./types";
  import { niceMax, time } from "../fmt";

  let {
    title,
    value,
    note,
    from,
    to,
    size,
    steps = [],
    blind = [],
    dots = [],
    names,
    tone = "none",
    fmt,
    read,
    hover,
    height = 76,
    empty = "No requests in this window",
    onhover,
    onpick,
  }: {
    title: string;
    /** The window's figure, already formatted. */
    value: string;
    note: string;
    from: number;
    to: number;
    /** Milliseconds per step. */
    size: number;
    steps?: Step[];
    blind?: Blind[];
    dots?: { t: number; v: number; r: Request }[];
    /** Names of the two stacked parts, when there are two. */
    names?: [string, string];
    /** How the second part is drawn: as a failure, or as a lighter share of the same thing. */
    tone?: "bad" | "light" | "none";
    fmt: (n: number) => string;
    /** What one step says when the pointer is on it. */
    read?: (s: Step) => string;
    hover: number | null;
    /** Height of the plot in pixels. */
    height?: number;
    /** What the plot says when it has nothing to draw. */
    empty?: string;
    onhover: (t: number | null) => void;
    onpick?: (r: Request) => void;
  } = $props();

  let width = $state(0);
  const W = $derived(Math.max(1, width));
  const H = $derived(height);
  const x = (t: number) => ((t - from) / (to - from)) * W;
  const has = $derived(steps.some((s) => s.a + s.b > 0) || dots.length > 0);
  const top = $derived(niceMax(Math.max(0, ...steps.map((s) => s.a + s.b), ...dots.map((d) => d.v))));
  const y = (v: number) => H - (v / top) * (H - 4);
  const stepAt = $derived(hover === null ? undefined : steps.find((s) => hover >= s.t && hover < s.t + size));
  /** The dot nearest the pointer in time, within a tenth of the window. */
  const dotAt = $derived.by(() => {
    if (hover === null || !dots.length) return undefined;
    const near = dots.reduce((best, d) => (Math.abs(d.t - hover) < Math.abs(best.t - hover) ? d : best));
    return Math.abs(near.t - hover) < (to - from) / 10 ? near : undefined;
  });
  const reading = $derived.by(() => {
    if (stepAt && read) {
      const start = Math.max(from, stepAt.t), end = Math.min(to, stepAt.t + size);
      const watched = watchedMs(start, end, blind);
      return { value: watched === 0 ? "Unknown" : watched < end - start ? `At least ${read(stepAt)}` : read(stepAt), note: `${time(start)} to ${time(end)}` };
    }
    return dotAt ? { value: fmt(dotAt.v), note: `${time(dotAt.t)}, ${dotAt.r.model}` } : null;
  });
  function move(e: PointerEvent) {
    const box = (e.currentTarget as HTMLElement).getBoundingClientRect();
    onhover(from + ((e.clientX - box.left) / box.width) * (to - from));
  }

  // ---- keyboard -------------------------------------------------------------------------
  let keyed = $state(false);
  const points = $derived(dots.length ? dots.map((d) => d.t) : steps.map((s) => s.t + size / 2));
  function key(e: KeyboardEvent) {
    if (!points.length) return;
    const at = hover === null ? -1 : points.findIndex((p) => (dots.length ? p === dotAt?.t : stepAt && p === stepAt.t + size / 2));
    let next: number | null = null;
    if (e.key === "ArrowRight") next = at < 0 ? 0 : Math.min(points.length - 1, at + 1);
    else if (e.key === "ArrowLeft") next = at < 0 ? points.length - 1 : Math.max(0, at - 1);
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = points.length - 1;
    else if (e.key === "Escape" && hover !== null) {
      e.preventDefault();
      e.stopPropagation();
      keyed = false;
      return onhover(null);
    } else if (e.key === "Enter" && dotAt && onpick) {
      e.preventDefault();
      return onpick(dotAt.r);
    }
    if (next === null) return;
    e.preventDefault();
    keyed = true;
    onhover(points[next]);
  }
  const hint = $derived(dots.length ? "Left and right arrows step through requests; Enter opens one." : "Left and right arrows step through the window.");
</script>

<div class="trend">
  <div class="headline"><span class="title">{title}</span></div>
  <div class="figure-line">
    <span class="value">{reading?.value ?? value}</span>
    <span class="note">{reading?.note ?? note}</span>
  </div>
  <span class="sr" aria-live="polite">{keyed && reading ? `${reading.value}, ${reading.note}` : ""}</span>
  <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions: one tab stop; the arrow keys move through the chart and the live line above reads it out -->
  <div
    class="plot"
    class:empty={!has}
    style:height={`${H}px`}
    bind:clientWidth={width}
    onpointermove={move}
    onpointerleave={() => onhover(null)}
    role="group"
    aria-label={`${title} chart: ${value}, ${note}. ${blind.length ? "Hatched time is unknown. " : ""}${has ? hint : empty}`}
    tabindex={has ? 0 : -1}
    onkeydown={key}
    onblur={() => keyed && ((keyed = false), onhover(null))}
  >
    {#each blind as b}<span class="unknown" aria-hidden="true" style:left={`${(Math.max(from, b.from) - from) / (to - from) * 100}%`} style:width={`${(Math.min(to, b.to) - Math.max(from, b.from)) / (to - from) * 100}%`}></span>{/each}
    {#if has}<span class="tick" aria-hidden="true">{fmt(top)}</span>{:else}<span class="nodata">{blind.length ? "No measurements; hatched time is unknown" : empty}</span>{/if}
    <svg width="100%" height={H} viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" aria-hidden="true">
      <line class="grid" x1="0" x2={W} y1="3.5" y2="3.5" />
      {#if stepAt}<rect class="band" x={x(stepAt.t)} y="0" width={x(stepAt.t + size) - x(stepAt.t)} height={H} />{/if}
      {#each steps as s (s.t)}
        {@const slot = x(s.t + size) - x(s.t)}
        {@const w = Math.max(1, Math.min(24, slot - 2))}
        {@const left = x(s.t) + (slot - w) / 2}
        {#if s.a > 0}<rect class="a" x={left} y={y(s.a)} width={w} height={H - y(s.a)} rx="2" />{/if}
        {#if s.b > 0}<rect class="b {tone}" x={left} y={y(s.a + s.b)} width={w} height={Math.max(1, y(s.a) - y(s.a + s.b) - (s.a > 0 ? 2 : 0))} rx="2" />{/if}
      {/each}
      {#each dots as d (d.r.n)}
        <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions: Enter on the focused chart opens the request under the cursor -->
        <circle class="dot" class:on={dotAt === d} cx={x(d.t)} cy={y(d.v)} r={dotAt === d ? 5 : 3.5} onclick={() => onpick?.(d.r)} />
      {/each}
      {#if hover !== null && !stepAt && hover >= from && hover <= to}<line class="cross" x1={x(hover)} x2={x(hover)} y1="0" y2={H} />{/if}
      <line class="base" x1="0" x2={W} y1={H - 0.5} y2={H - 0.5} />
    </svg>
  </div>
  <!-- Two parts need a key; one part is named by the title. -->
  <div class="legend">{#if names}<span><i></i>{names[0]}</span><span><i class={tone}></i>{names[1]}</span>{/if}</div>
</div>

<style>
  .trend {
    display: grid;
    gap: 2px;
    min-width: 0;
  }
  .headline {
    display: flex;
    justify-content: space-between;
    gap: 8px;
    font-size: 13px;
    color: var(--ink-2);
    white-space: nowrap;
  }
  .legend {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 2px 12px;
    min-height: 18px;
    font-size: 12px;
    color: var(--ink-2);
  }
  .legend span {
    display: inline-flex;
    align-items: center;
    gap: 5px;
  }
  .legend i {
    width: 9px;
    height: 9px;
    border-radius: 2px;
    background: var(--ink-2);
  }
  .legend i.bad {
    background: var(--bad);
  }
  .legend i.light {
    background: var(--line-2);
  }
  .figure-line {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 0 8px;
    min-height: 30px;
    min-width: 0;
  }
  .value {
    flex: none;
    font-size: 22px;
    font-weight: 600;
    letter-spacing: -0.01em;
  }
  .note {
    min-width: 0;
    font-size: 13px;
    color: var(--ink-2);
  }
  .plot {
    position: relative;
    contain: inline-size;
    min-width: 0;
    border-radius: 4px;
  }
  .plot:focus-visible {
    outline-offset: 3px;
  }
  .unknown {
    position: absolute;
    inset-block: 0;
    pointer-events: none;
    background: repeating-linear-gradient(135deg, transparent 0 4px, var(--line) 4px 5px);
  }
  .tick,
  .nodata {
    position: absolute;
    left: 0;
    top: 6px;
    font-size: 12px;
    color: var(--ink-2);
    pointer-events: none;
  }
  .nodata {
    top: 50%;
    transform: translateY(-50%);
  }
  svg {
    display: block;
    overflow: hidden;
  }
  .grid {
    stroke: var(--line);
  }
  .base {
    stroke: var(--line-2);
  }
  .cross {
    stroke: var(--ink-2);
  }
  .band {
    fill: var(--sunk);
  }
  .a,
  .b {
    fill: var(--ink-2);
  }
  .b.bad {
    fill: var(--bad);
  }
  .b.light {
    fill: var(--line-2);
  }
  .dot {
    fill: var(--ink-2);
    stroke: var(--surface);
    stroke-width: 1.5;
    cursor: pointer;
  }
  .dot.on {
    fill: var(--ink);
  }
</style>
