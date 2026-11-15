<script lang="ts">
  // One limit window: how much is used and when it resets. A window that was not read says
  // why instead of drawing an empty bar, which would look like 0% used.
  let { name = "", used, note = "", why = "", stale = false }: { name?: string; used: number | null; note?: string; why?: string; stale?: boolean } = $props();
</script>

{#if used === null}
  <span class="small muted">{name ? `${name}: ` : ""}{why || "not reported"}</span>
{:else}
  <div class="meter" class:high={used >= 85 && used < 100} class:full={used >= 100} class:stale>
    <div class="label"><span>{name ? `${name} ` : ""}<b>{Math.round(used)}%</b> used</span><span>{note}</span></div>
    <div class="track" role="meter" aria-valuenow={Math.round(used)} aria-valuemin="0" aria-valuemax="100" aria-label={`${name || "Limit"} used`}>
      <div class="fill" style:width={`${Math.min(100, used)}%`}></div>
    </div>
  </div>
{/if}
