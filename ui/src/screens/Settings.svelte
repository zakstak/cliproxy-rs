<script lang="ts">
  // Settings: the switches the four screens depend on, and the guided editors for the rest
  // of the configuration (provider keys, models, payload rules, every section, the whole
  // file) and what the server reports about itself. One screen, one address per section.
  import { untrack } from "svelte";
  import { app, settingsPages, type SettingsPage } from "../app.svelte";
  import General from "./settings/General.svelte";
  import ProviderKeys from "./settings/ProviderKeys.svelte";
  import Models from "./settings/Models.svelte";
  import Payload from "./settings/Payload.svelte";
  import Config from "./settings/Config.svelte";
  import System from "./settings/System.svelte";

  const section = $derived<SettingsPage>((settingsPages.find((p) => p[0] === (app.route.rest[0] || ""))?.[0] ?? "") as SettingsPage);
  const name = $derived(settingsPages.find((p) => p[0] === section)?.[1] ?? "General");
  const drafts = $derived(app.unsaved);
  // A section change inside Settings moves focus to that section's heading.
  let shown = untrack(() => section);
  let heading = $state<HTMLElement>();
  $effect(() => {
    if (section === shown) return;
    shown = section;
    heading?.focus({ preventScroll: true });
  });
</script>

<div class="page">
  <div class="head"><h1>Settings</h1></div>
  <div class="grid grid-cols-[minmax(0,1fr)] items-start gap-3.5 md:grid-cols-[190px_minmax(0,1fr)] md:gap-7">
    <nav class="flex flex-wrap gap-1.5 md:sticky md:top-[calc(var(--top)+16px)] md:grid md:gap-0.5" aria-label="Settings sections">
      {#each settingsPages as [id, label] (id)}
        <a
          class="flex min-h-11 items-center gap-2 rounded-full border border-line-2 px-3.5 text-ink-2 no-underline hover:bg-sunk hover:text-ink aria-[current=page]:border-accent aria-[current=page]:bg-accent aria-[current=page]:font-semibold aria-[current=page]:text-on-accent min-[861px]:min-h-9 md:rounded-none md:rounded-r-md md:border-0 md:border-l-2 md:border-transparent md:px-2.5 md:aria-[current=page]:border-accent md:aria-[current=page]:bg-transparent md:aria-[current=page]:text-ink"
          href={`#settings${id ? `/${id}` : ""}`}
          aria-current={section === id ? "page" : undefined}
        >
          {label}{#if id === "config" && drafts.length}<span class="dot warn" aria-hidden="true"></span><span class="sr">, {drafts.length} unsaved</span>{/if}
        </a>
      {/each}
    </nav>
    <div class="grid min-w-0 max-w-[880px] gap-3.5">
      <h2 class="text-[17px] focus:outline-none" tabindex="-1" bind:this={heading}>{name}</h2>
      {#if drafts.length && section !== "config"}
        <p class="notice warn small"><span class="dot warn"></span><span>Unsaved: {drafts.map((d) => d.title).join(", ")}. <a href="#settings/config">Review or discard in Configuration</a></span></p>
      {/if}
      {#if !app.config.data}
        <p class="empty muted">{app.config.error ? "" : "Reading settings…"}</p>
      {:else if section === "keys"}<ProviderKeys />
      {:else if section === "models"}<Models />
      {:else if section === "payload"}<Payload />
      {:else if section === "config"}<Config />
      {:else if section === "system"}<System />
      {:else}<General />{/if}
    </div>
  </div>
</div>
