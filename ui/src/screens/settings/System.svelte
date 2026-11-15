<script lang="ts">
  // What the server reports about itself. The management API has no uptime, CPU or memory.
  import { api } from "../../api";
  import { readPath } from "../../core";
  import { app, message } from "../../app.svelte";

  const running = $derived(app.meta.version.replace(/^cliproxy-rs[\/-]/, ""));
  const c = $derived(app.config.data || {});
  const facts = $derived(
    [
      ["Server", app.meta.version || "Version not reported"],
      ["Management API", "/v8/management"],
      ["Config layout", c["config-version"] ? `v${c["config-version"]}` : "Legacy (migrated on save)"],
      ["Routing strategy", readPath(c, "routing/strategy", "Server default")],
      ["Auth directory", readPath(c, "oauth/auth-dir", "Server default")],
      ["Remote management", readPath(c, "management/allow-remote", false) ? "Allowed" : "Local only, unless MANAGEMENT_PASSWORD is set"],
      ["Dashboard", "Built into the server"],
    ] as [string, string][],
  );
  let update = $state<{ kind: "idle" } | { kind: "checking" } | { kind: "latest"; version: string; at: number } | { kind: "failed"; why: string }>({ kind: "idle" });
  async function checkUpdate() {
    update = { kind: "checking" };
    try {
      update = { kind: "latest", version: String((await api("/server/latest-version"))["latest-version"]), at: Date.now() };
    } catch (e) {
      update = { kind: "failed", why: message(e) };
    }
  }
</script>

<section class="group">
  <dl class="facts">
    {#each facts as [k, v]}<dt>{k}</dt><dd class:mono={k === "Management API" || k === "Auth directory"}>{v}</dd>{/each}
  </dl>
  <p class="small muted">The management API does not report process uptime, CPU or memory, so they are not shown.</p>
</section>

<section class="group">
  <h3>Updates</h3>
  <div class="row">
    <button class="btn small" aria-disabled={update.kind === "checking"} onclick={checkUpdate}>{update.kind === "checking" ? "Checking…" : update.kind === "failed" ? "Check again" : "Check for a newer version"}</button>
    <span class="small" role="status">
      {#if update.kind === "latest"}{update.version.replace(/^v/, "") === running ? "This is the latest version." : `${update.version} is available. This server runs ${running}.`}
      {:else if update.kind === "failed"}<span class="code-status bad">Could not check: {update.why}</span>
      {:else if update.kind === "idle"}<span class="muted">Nothing is checked automatically. The server asks GitHub, without credentials, when you click.</span>{/if}
    </span>
  </div>
</section>
