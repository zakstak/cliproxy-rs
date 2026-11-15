<script lang="ts">
  import { app, pages, message } from "./app.svelte";
  import Logs from "./screens/Logs.svelte";
  import Usage from "./screens/Usage.svelte";
  import Accounts from "./screens/Accounts.svelte";
  import Tools from "./screens/Tools.svelte";
  import Settings from "./screens/Settings.svelte";
  import { currentIssues } from "./issues";
  import { plural } from "./fmt";
  import { tick } from "svelte";

  // Re-read a few times a minute; an account changing state or a failed request updates it at once.
  const issues = $derived(app.logged ? currentIssues(Math.ceil(app.now / 15_000) * 15_000) : []);
  const unsaved = $derived(app.logged ? app.unsaved : []);

  const icons: Record<string, string> = {
    logs: "M5 7h14M5 12h14M5 17h9",
    usage: "M6 19V11M12 19V5M18 19v-6",
    accounts: "M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8M5 20a7 7 0 0 1 14 0",
    tools: "M5 8l4 4-4 4M12 16h7",
    settings: "M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6M19 12h2M3 12h2M12 3v2M12 19v2M17 7l1.5-1.5M5.5 18.5 7 17M17 17l1.5 1.5M5.5 5.5 7 7",
  };

  // After a screen change, focus the new screen's content, as a page load would. Changes
  // inside a screen (an account opening, a filter in the address) leave focus alone.
  let page = app.route.page;
  $effect(() => {
    const now = app.route.page;
    if (now === page) return;
    page = now;
    document.getElementById("main")?.focus({ preventScroll: true });
    scrollTo(0, 0);
  });

  let secret = $state("");
  let error = $state("");
  let signing = $state(false);
  async function signIn(e: SubmitEvent) {
    e.preventDefault();
    signing = true;
    error = "";
    app.toast = null;
    try {
      await app.login(secret);
      // The key now lives only in the API module's memory.
      secret = "";
    } catch (e) {
      error = message(e);
    } finally {
      signing = false;
    }
  }

  // A control that cannot act right now is marked aria-disabled instead of disabled, so the
  // keyboard focus on it survives (a disabled button drops focus to the page). Its clicks and
  // the forms it submits are stopped here, before any handler runs.
  function stopDisabled(e: MouseEvent) {
    if (e.target instanceof Element && e.target.closest('[aria-disabled="true"]')) {
      e.preventDefault();
      e.stopImmediatePropagation();
    }
  }
  function stopDisabledSubmit(e: SubmitEvent) {
    if (e.submitter?.getAttribute("aria-disabled") === "true") {
      e.preventDefault();
      e.stopImmediatePropagation();
    }
  }
  const versions = (n: number) => (n ? `, with ${n === 1 ? "an earlier version" : `${n} earlier versions`}` : "");

  // A toast's own action (Try again) keeps the toast, and its focus, while it runs. Failing
  // again replaces the text and keeps the same button; succeeding announces the result and
  // the button goes, so focus moves to the page rather than being dropped.
  let toastBox = $state<HTMLElement>();
  let acting = $state(false);
  const focusPageIfLost = () => {
    const active = document.activeElement;
    if (!active || active === document.body || !active.isConnected) document.getElementById("main")?.focus({ preventScroll: true });
  };
  async function act(a: { run?: () => unknown }) {
    if (acting || !a.run) return;
    acting = true;
    try {
      await a.run();
    } finally {
      acting = false;
    }
    await tick();
    focusPageIfLost();
  }
  async function dismissToast() {
    const had = !!toastBox?.contains(document.activeElement);
    app.toast = null;
    await tick();
    if (had) focusPageIfLost();
  }

  let confirm = $state<HTMLDialogElement>();
  $effect(() => {
    if (app.confirmingLogout && confirm && !confirm.open) confirm.showModal();
  });
  function keepEditing() {
    app.confirmingLogout = false;
    confirm?.close();
    document.querySelector<HTMLElement>(".signout")?.focus();
  }
</script>

<svelte:window onhashchange={() => app.sync()} onbeforeunload={(e) => unsaved.length && e.preventDefault()} onclickcapture={stopDisabled} onsubmitcapture={stopDisabledSubmit} />

{#if !app.logged}
  <main class="gate grid min-h-dvh place-items-center p-5">
    <form class="grid w-full max-w-[380px] gap-3.5" onsubmit={signIn}>
      <h1>cliproxy</h1>
      <label class="field">
        Management key
        <!-- svelte-ignore a11y_autofocus: the key is the only thing to do on this screen -->
        <input type="password" autocomplete="off" required bind:value={secret} autofocus aria-invalid={!!error} aria-describedby={error ? "signin-error" : undefined} />
      </label>
      {#if app.toast?.bad}<p class="notice bad small" role="alert">{app.toast.text}</p>{/if}
      {#if error}<p class="notice bad small" role="alert" id="signin-error"><span class="dot bad"></span><span>{error}</span></p>{/if}
      <button class="btn primary" aria-disabled={signing}>{signing ? "Signing in…" : "Sign in"}</button>
      <p class="small muted">The key stays in this tab's memory. Reloading the page signs you out.</p>
    </form>
  </main>
{:else}
  <a class="skip" href="#main" onclick={(e) => (e.preventDefault(), document.getElementById("main")?.focus())}>Skip to content</a>
  <div class="shell">
    <header class="sticky top-0 z-20 flex h-(--top) items-center gap-1.5 border-b border-line bg-surface px-3 sm:gap-2.5 md:gap-3.5 md:px-5 lg:gap-5">
      <a class="flex min-h-11 items-center font-[650] tracking-[-0.01em] no-underline min-[861px]:min-h-6" href="#logs">cliproxy</a>
      <nav
        class="fixed inset-x-0 bottom-0 z-30 flex h-(--bottom) border-t border-line bg-surface pb-[env(safe-area-inset-bottom)] md:static md:h-auto md:gap-0.5 md:self-stretch md:border-0 md:bg-transparent md:pb-0"
        aria-label="Main"
      >
        {#each pages as [id, long, short] (id)}
          <a
            class="tab flex flex-1 flex-col items-center justify-center gap-0.5 border-y-2 border-transparent px-1 text-[12.5px] text-ink-2 no-underline hover:text-ink aria-[current=page]:font-semibold aria-[current=page]:text-ink max-md:aria-[current=page]:border-t-accent md:flex-none md:flex-row md:px-2.5 md:text-sm md:aria-[current=page]:border-b-accent"
            href={`#${id}`}
            aria-current={app.route.page === id ? "page" : undefined}
          >
            <svg class="icon md:hidden" viewBox="0 0 24 24" aria-hidden="true"><path d={icons[id]} /></svg>
            <span class="hidden lg:inline">{long}</span><span class="lg:hidden">{short}</span>
          </a>
        {/each}
      </nav>
      <span class="grow"></span>
      <div class="flex flex-none items-center gap-1 md:gap-3.5">
        {#if issues.length}
          <a
            class="flex min-h-11 min-w-11 items-center justify-center gap-1.5 text-[13px] font-[560] whitespace-nowrap no-underline hover:underline min-[861px]:min-h-6 min-[861px]:min-w-0"
            href="#logs"
            aria-label={`${plural(issues.length, "issue")}. Open Activity to see them`}
          >
            <span class="dot {issues.some((i) => i.level === 'bad') ? 'bad' : 'warn'}"></span>{issues.length}<span class="hidden min-[901px]:inline">&nbsp;{issues.length === 1 ? "issue" : "issues"}</span>
          </a>
        {/if}
        <!-- Below 901px the lamp stands alone when connected, and "Offline" replaces the longer
             words when not, so Sign out stays on screen. The status always says it in full. -->
        <span class="flex items-center gap-1.5 text-[13px] whitespace-nowrap {app.online ? 'text-ink-2' : 'font-[560] text-bad'}" role="status">
          <span class="dot {app.online ? 'ok' : 'bad'}"></span>
          {#if app.online}<span class="sr-only min-[901px]:not-sr-only">Connected</span>
          {:else}<span class="min-[901px]:hidden" aria-hidden="true">Offline</span><span class="sr-only min-[901px]:not-sr-only">Server unreachable</span>{/if}
        </span>
      </div>
      <div class="flex flex-none items-center gap-1">
        <a class="btn quiet small min-w-11 min-[861px]:min-w-0" href="#settings" aria-current={app.route.page === "settings" ? "page" : undefined}>
          <svg class="icon min-[901px]:hidden" viewBox="0 0 24 24" aria-hidden="true"><path d={icons.settings} /></svg>
          <span class="sr-only min-[901px]:not-sr-only">Settings</span>
          {#if unsaved.length}<span class="dot warn" aria-hidden="true"></span><span class="sr">, {plural(unsaved.length, "unsaved draft")}</span>{/if}
        </a>
        <button class="btn quiet small signout" onclick={() => app.requestLogout()}>Sign out</button>
      </div>
    </header>
    <main id="main" tabindex="-1" class="focus:outline-none">
      {#if app.route.page === "logs"}<Logs />
      {:else if app.route.page === "usage"}<Usage />
      {:else if app.route.page === "accounts"}<Accounts />
      {:else if app.route.page === "tools"}<Tools />
      {:else}<Settings />{/if}
    </main>
  </div>
  {#if app.toast}
    {@const t = app.toast}
    <div class="toast" class:bad={t.bad} bind:this={toastBox}>
      <span class="toast-message">{t.text}</span>
      <div class="toast-actions">
      {#each t.actions || [] as a (a.label)}
        {#if a.href}<a href={a.href} onclick={() => (app.toast = null)}>{a.label}</a>
        {:else}<button aria-disabled={acting} onclick={() => act(a)}>{acting ? "Trying again…" : a.label}</button>{/if}
      {/each}
      {#if t.bad}<button onclick={dismissToast}>Dismiss</button>{/if}
      </div>
    </div>
  {/if}
  {#if app.confirmingLogout}
    <dialog bind:this={confirm} class="confirm" aria-labelledby="signout-title" oncancel={(e) => (e.preventDefault(), keepEditing())}>
      <header><h2 id="signout-title">Sign out and discard unsaved changes?</h2></header>
      <div class="body">
        <p>Signing out clears this tab's memory, including {unsaved.length === 1 ? "an unsaved draft" : `${unsaved.length} unsaved drafts`}:</p>
        <ul class="bullets">{#each unsaved as d (d.path)}<li><span class="mono">{d.title}</span>{versions(d.versions.length)}</li>{/each}</ul>
        <p class="small muted">Nothing has been written to the server.</p>
        <div class="row">
          <!-- svelte-ignore a11y_autofocus: the safe choice takes focus in a destructive confirmation -->
          <button class="btn primary" autofocus onclick={() => { keepEditing(); app.go("settings/config"); }}>Keep editing</button>
          <button class="btn danger" onclick={() => app.logout()}>Discard and sign out</button>
        </div>
      </div>
    </dialog>
  {/if}
{/if}
<!-- One place that announces results: status for successes, alert for failures. -->
<div class="sr" role="status">{app.logged && app.toast && !app.toast.bad ? app.toast.text : ""}</div>
<div class="sr" role="alert">{app.logged && app.toast?.bad ? app.toast.text : ""}</div>
