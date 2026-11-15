<script lang="ts">
  // One measure over time, as bars from a zero baseline. `flagged` is the part of each bar
  // that failed; `hole` marks a bar whose period was only partly observed.
  import type { Bar } from "./types";
  let { bars, name, flaggedName = "", summary }: { bars: Bar[]; name: string; flaggedName?: string; summary: string } = $props();

  let active = $state<number | null>(null);
  const top = $derived(Math.max(1, ...bars.map((b) => b.value)));
  /** A round axis maximum at or above the tallest bar. */
  const scale = $derived.by(() => {
    const step = 10 ** Math.floor(Math.log10(top));
    return [1, 2, 5, 10].map((m) => m * step).find((v) => v >= top) ?? top;
  });
  const short = (n: number) => new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 }).format(n);
  const ticks = $derived([0, Math.floor(bars.length / 2), bars.length - 1].filter((i, at, all) => i >= 0 && all.indexOf(i) === at));
  function key(e: KeyboardEvent) {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    const at = active ?? (e.key === "ArrowLeft" ? bars.length : -1);
    active = Math.max(0, Math.min(bars.length - 1, at + (e.key === "ArrowLeft" ? -1 : 1)));
  }
</script>

<figure class="bars">
  {#if flaggedName || bars.some((b) => b.hole)}
    <figcaption class="legend small">
      {#if flaggedName}<span><i class="swatch"></i>{name}</span><span><i class="swatch flagged"></i>{flaggedName}</span>{/if}
      {#if bars.some((b) => b.hole)}<span><i class="swatch hole"></i>Partly not captured</span>{/if}
    </figcaption>
  {/if}
  <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions: one tab stop; the arrow keys move between bars and the line below reads the bar out -->
  <div class="plot" role="img" aria-label={`${summary} Use the left and right arrow keys to read each bar.`} tabindex="0" onkeydown={key} onmouseleave={() => (active = null)} onblur={() => (active = null)}>
    <span class="axis small" aria-hidden="true">{short(scale)}</span>
    {#each bars as b, i}
      <!-- svelte-ignore a11y_no_static_element_interactions: hover is a second way to the readout the arrow keys give -->
      <div class="col" class:on={active === i} class:hole={b.hole} onmouseenter={() => (active = i)}>
        {#if b.flagged}<i class="flagged" style:height={`${(b.flagged / scale) * 100}%`}></i>{/if}
        {#if b.value - (b.flagged || 0) > 0}<i style:height={`${((b.value - (b.flagged || 0)) / scale) * 100}%`}></i>{/if}
      </div>
    {/each}
  </div>
  <div class="x small" aria-hidden="true">
    {#each ticks as i}<span style:left={`${((i + 0.5) / bars.length) * 100}%`}>{bars[i].label}</span>{/each}
  </div>
  <p class="readout small" aria-live="polite">{active === null ? summary : bars[active].tip}</p>
</figure>

<style>
  .bars {
    margin: 0;
    display: grid;
    gap: 6px;
  }
  .legend {
    display: flex;
    gap: 16px;
    flex-wrap: wrap;
    color: var(--ink-2);
  }
  .swatch {
    display: inline-block;
    width: 10px;
    height: 10px;
    margin-right: 6px;
    border-radius: 2px;
    background: var(--ink-2);
  }
  .swatch.flagged {
    background: var(--bad);
  }
  .swatch.hole {
    background: repeating-linear-gradient(135deg, var(--line-2) 0 2px, transparent 2px 5px);
  }
  .plot {
    position: relative;
    display: flex;
    align-items: flex-end;
    gap: 2px;
    height: 150px;
    border-top: 1px dashed var(--line);
    border-bottom: 1px solid var(--line-2);
  }
  .axis {
    position: absolute;
    top: 2px;
    left: 0;
    color: var(--ink-3);
    pointer-events: none;
  }
  .col {
    flex: 1;
    align-self: stretch;
    display: flex;
    flex-direction: column;
    justify-content: flex-end;
    gap: 2px;
    min-width: 0;
  }
  .col.hole {
    background: repeating-linear-gradient(135deg, var(--line) 0 2px, transparent 2px 6px);
  }
  .col.on {
    background-color: var(--sunk);
  }
  .col i {
    display: block;
    min-height: 2px;
    max-width: 28px;
    width: 100%;
    margin: 0 auto;
    border-radius: 3px 3px 0 0;
    background: var(--ink-2);
  }
  .col i.flagged {
    background: var(--bad);
  }
  .col i + i {
    border-radius: 0;
  }
  .col.on i:not(.flagged) {
    background: var(--ink);
  }
  .x {
    position: relative;
    height: 1.4em;
    color: var(--ink-3);
  }
  .x span {
    position: absolute;
    transform: translateX(-50%);
    white-space: nowrap;
  }
  .x span:first-child {
    transform: none;
    left: 0 !important;
  }
  .x span:last-child {
    transform: none;
    left: auto !important;
    right: 0;
  }
  .readout {
    min-height: 1.5em;
    color: var(--ink-2);
  }
</style>
