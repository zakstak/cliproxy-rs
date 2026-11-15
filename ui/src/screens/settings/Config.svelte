<script lang="ts">
  // Every config section as JSON, and the whole file as YAML. Each edit is a draft kept by the
  // app, reviewed as a diff and written only if the server copy did not change meanwhile.
  import { app } from "../../app.svelte";
  import ConfigEditor from "../../parts/ConfigEditor.svelte";

  const about: Record<string, string> = {
    server: "Listener and TLS. Some changes need a restart.",
    management: "Remote access and the management key.",
    access: "Client keys. Easier on Use with tools.",
    routing: "Strategy, retries and cooldowns.",
    requests: "Outbound proxy, streaming and payload rules.",
    "api-keys": "Provider API keys. Easier under Provider keys.",
    oauth: "Auth directory, aliases and exclusions.",
    client: "Per-client tweaks.",
    multimedia: "Image and video generation.",
    observability: "Logs and usage statistics.",
  };
  const sections = $derived([...new Set([...Object.keys(about), ...Object.keys(app.config.data || {})])].filter((k) => k !== "config-version" && k !== "port"));
  const drafts = $derived(app.unsaved);
</script>

{#if drafts.length}
  <p class="notice warn small" role="status"><span class="dot warn"></span><span>{drafts.length === 1 ? "One unsaved draft" : `${drafts.length} unsaved drafts`}: {drafts.map((d) => `${d.title}${d.versions.length ? ` (with ${d.versions.length === 1 ? "an earlier version" : `${d.versions.length} earlier versions`})` : ""}`).join(", ")}. Drafts stay while you move around the dashboard; signing out or leaving the page asks first.</span></p>
{/if}
<p class="small muted">A write goes through only if the server copy is still the one you started from: the server checks the file's version as it saves, so another person's change is never overwritten.</p>

<section class="group">
  <ConfigEditor path="/config.yaml" title="config.yaml" yaml help="The whole file. Everything below is a part of it." />
</section>

<section class="group">
  <h3>Sections</h3>
  <p class="small muted">Shows what is saved in the file, not the server's defaults for what is left out.</p>
  <ul class="grid">
    {#each sections as k (k)}
      {@const v = app.config.data?.[k]}
      <li class="border-t border-line py-2.5">
        <ConfigEditor path={`/config/${k}`} title={k} help={`${about[k] || "Saved section."} ${v === undefined ? "Not set: server defaults." : v && typeof v === "object" ? `${Object.keys(v).length} set.` : ""}`} />
      </li>
    {/each}
  </ul>
</section>
