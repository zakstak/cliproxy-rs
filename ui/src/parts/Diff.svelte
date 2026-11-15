<script lang="ts">
  // A reviewed change: how much changed, then only the changed places with three lines of
  // context and their line numbers, so the first change is in view when the review opens.
  // The whole file is one click away. Line numbers are those of the newer text.
  import { describeSummary, diffLines, hunks, lines, summarize, type Hunk } from "../diff";

  let { before, after, label, summary = true }: { before: string; after: string; label: string; summary?: boolean } = $props();
  let whole = $state(false);
  const diff = $derived(diffLines(lines(before), lines(after)));
  const places = $derived(hunks(diff));
  const counts = $derived(summarize(diff));
  const shown = $derived<Hunk[]>(whole ? [{ fromLine: 1, toLine: 1, lines: diff, added: counts.added, removed: counts.removed }] : places);
  function range(h: Hunk) {
    const nums = h.lines.flatMap((l) => (l.kind === "removed" ? [] : [l.b + 1]));
    if (!nums.length) return `After line ${h.toLine - 1}`;
    return nums.length === 1 ? `Line ${nums[0]}` : `Lines ${nums[0]}–${nums.at(-1)}`;
  }
</script>

<div class="grid gap-1.5">
  {#if summary}<p class="small">{describeSummary(counts)}.</p>{/if}
  {#if !places.length && before !== after && lines(before).join("\n") === lines(after).join("\n")}<p class="small muted">Only the line break at the end of the text differs.</p>{/if}
  {#if places.length}
    <div class="code diff" role="group" aria-label={label}>
      {#each shown as h, i (i)}
        {#if !whole}<div class="hunk-head">{range(h)}</div>{/if}
        {#each h.lines as l, j (j)}
          <div class={l.kind}>
            <span class="ln" aria-hidden="true">{l.kind === "removed" ? "" : l.b + 1}</span><span class="mark" aria-hidden="true">{l.kind === "added" ? "+" : l.kind === "removed" ? "−" : ""}</span><span class="sr">{l.kind === "added" ? "Added: " : l.kind === "removed" ? "Removed: " : ""}</span>{l.text || " "}
          </div>
        {/each}
      {/each}
    </div>
    <div><button type="button" class="link small" aria-pressed={whole} onclick={() => (whole = !whole)}>{whole ? "Show only the changed lines" : "Show the whole text"}</button></div>
  {/if}
</div>
