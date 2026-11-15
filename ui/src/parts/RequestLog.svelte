<script lang="ts">
  // The full request log for one request ID, read only when asked for: it can be large and
  // it holds prompts. The server keeps one only while request logging is on, or, with it
  // off, for requests that failed.
  import { api } from "../api";
  import { ApiError, download } from "../api";
  import { message } from "../app.svelte";
  import Copy from "./Copy.svelte";
  import { focusAfter } from "../focus";

  let { id }: { id: string } = $props();
  type Phase = { kind: "idle" } | { kind: "loading" } | { kind: "text"; text: string } | { kind: "none" } | { kind: "error"; why: string };
  let phase = $state<Phase>({ kind: "idle" });
  let box = $state<HTMLElement>();
  // Another request was opened: its log has not been asked for yet.
  $effect(() => {
    id;
    phase = { kind: "idle" };
  });
  async function load() {
    const mine = id;
    phase = { kind: "loading" };
    try {
      const text = await api(`/observability/logs/requests/${encodeURIComponent(mine)}`, "GET", undefined, "text");
      if (mine !== id) return;
      phase = { kind: "text", text: String(text) };
      // The read button is replaced by the log and its copy and download keys: continue there.
      focusAfter(() => box?.querySelector(".btn"));
    } catch (e) {
      if (mine !== id) return;
      phase = e instanceof ApiError && e.status === 404 ? { kind: "none" } : { kind: "error", why: message(e) };
      if (phase.kind === "none") focusAfter(() => box?.querySelector("[data-none]"));
    }
  }
</script>

<section bind:this={box}>
  <h3>Full request log</h3>
  {#if phase.kind === "text"}
    <div class="row">
      <Copy text={phase.text} label="Copy log" />
      <button class="btn small" onclick={() => phase.kind === "text" && download(new Blob([phase.text]), `request-${id.slice(-8)}.log`)}>Download</button>
    </div>
    <pre class="code">{phase.text}</pre>
    <p class="small muted">Headers that carry credentials are redacted by the server before the log is written.</p>
  {:else if phase.kind === "none"}
    <p class="small muted focus:outline-none" data-none tabindex="-1">
      The server has no request log for this ID. It saves one for every request only while request logging is on; with it off, only
      failed requests are saved. <a href="#settings">Logging settings</a>
    </p>
  {:else}
    {#if phase.kind === "error"}<p class="notice bad small" role="alert"><span class="dot bad"></span><span>Could not read the request log: {phase.why}</span></p>{/if}
    <div class="row">
      <button class="btn small" aria-disabled={phase.kind === "loading"} onclick={load}>
        {phase.kind === "loading" ? "Reading…" : phase.kind === "error" ? "Try again" : "Read request and response"}
      </button>
      <span class="small muted">Includes the prompt and the provider's answer.</span>
    </div>
  {/if}
</section>
