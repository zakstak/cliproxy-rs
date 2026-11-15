<script lang="ts">
  // Edit config.yaml or one config section as text. The draft lives in the app (app.drafts),
  // so changing screens keeps it, and signing out or leaving the page asks first.
  // Writing is reviewed as a diff of the changed places. The write is compare-and-swap: it is
  // refused if the server copy changed since the draft started (server.ts). Then nothing is
  // written and the draft is kept; continuing from the newer copy keeps the current text as a
  // numbered version and applies the user's changes that do not touch the server's.
  import { Effect } from "effect";
  import { onDestroy } from "svelte";
  import { download } from "../api";
  import { app, server } from "../app.svelte";
  import { explain, type TextWrite, type WriteError } from "../server";
  import { changesServer, dropVersion, handled, reconcile, shown, startDraft, type Draft, type Unapplied } from "../draft";
  import { describeSummary, diffLines, lines, summarize } from "../diff";
  import { focusAfter, find } from "../focus";
  import { time } from "../fmt";
  import Diff from "./Diff.svelte";
  import Copy from "./Copy.svelte";

  let { path, title, yaml = false, help = "" }: { path: string; title: string; yaml?: boolean; help?: string } = $props();
  const id = $derived(path.replace(/[^a-z0-9]+/gi, "-"));
  const draft = $derived<Draft | undefined>(app.drafts[path]);
  const unsaved = $derived(!!draft && (draft.text !== draft.base || draft.versions.length > 0));
  const shown2 = (text: string) => (draft ? shown(draft, text) : text);
  /** Only the layout differs from the server copy: nothing would be written. */
  const layoutOnly = $derived(!!draft && draft.text !== draft.base && !changesServer(draft));

  type EditingPhase =
    | { kind: "edit" }
    | { kind: "review"; reviewed: TextWrite }
    | { kind: "conflict"; latest: string; at: number }
    | { kind: "failed"; error: WriteError; reviewed: TextWrite };
  type Phase = EditingPhase | { kind: "saving" } | { kind: "discard"; previous: EditingPhase };
  let phase = $state<Phase>({ kind: "edit" });
  let readError = $state("");
  let reading = $state(false);
  let invalid = $state("");
  let removing = $state<number | null>(null);
  let gone = false;
  onDestroy(() => (gone = true));

  const set = (next: Draft) => (app.drafts[path] = next);
  async function open() {
    reading = true;
    readError = "";
    const r = await Effect.runPromise(Effect.result(server.copyOf(path, yaml)));
    reading = false;
    if (r._tag === "Failure") return void (readError = r.failure.message);
    set(startDraft({ path, title, yaml, server: r.success }));
    phase = { kind: "edit" };
    focusAfter(() => find(`#${id}-text`));
  }
  function review() {
    if (!draft) return;
    invalid = "";
    if (!yaml)
      try {
        JSON.parse(draft.text);
      } catch (e) {
        invalid = `This is not valid JSON, so it cannot be written: ${e instanceof Error ? e.message : String(e)}`;
        return void focusAfter(() => find(`#${id}-text`));
      }
    phase = { kind: "review", reviewed: { path, yaml, base: draft.base, text: draft.text } };
    focusAfter(() => find(`#${id}-review`));
  }
  function keepEditing() {
    phase = { kind: "edit" };
    focusAfter(() => find(`#${id}-text`));
  }
  function edit(text: string) {
    if (!draft) return;
    set({ ...draft, text });
    // A failed write can only be retried while its reviewed text is unchanged.
    // Editing starts a new operation, which must go through review again.
    // A conflict stays until the user continues from the server's copy or discards: typing
    // keeps its notice and choices, and what was typed is part of the version it keeps.
    if (phase.kind !== "conflict") phase = { kind: "edit" };
    invalid = "";
  }
  async function save(reviewed: TextWrite) {
    if (!draft) return;
    const savingDraft = draft;
    phase = { kind: "saving" };
    const error = await app.perform(server.saveText(reviewed), `${title} saved.`);
    if (!error) {
      // Another mounted editor may have changed this draft while the write was pending.
      // Keep that newer work; its next review will compare against the saved copy.
      if (app.drafts[path] === savingDraft) delete app.drafts[path];
      if (gone) return;
      phase = { kind: "edit" };
      return void focusAfter(() => find(`#${id}-open`));
    }
    // Left the screen meanwhile: the draft is still kept; say so wherever the user is now.
    if (gone)
      return app.notify(`${title} was not saved: ${error._tag === "Changed" ? "it changed on the server meanwhile." : explain(error)} Your draft is kept.`, true, [
        { label: "Show it", href: "#settings/config" },
      ]);
    if (error._tag === "Changed") {
      const latest = typeof error.latest === "string" ? error.latest : await Effect.runPromise(Effect.orElseSucceed(server.copyOf(path, yaml), () => ""));
      phase = { kind: "conflict", latest, at: Date.now() };
      return void focusAfter(() => find(`#${id}-conflict`));
    }
    phase = { kind: "failed", error, reviewed };
    focusAfter(() => find(`#${id}-failed`));
  }
  function continueFromLatest(latest: string) {
    if (!draft) return;
    set(reconcile($state.snapshot(draft) as Draft, latest, Date.now()));
    phase = { kind: "edit" };
    focusAfter(() => find(`#${id}-reconciled`));
  }
  function askDiscard() {
    if (phase.kind === "saving" || phase.kind === "discard") return;
    phase = { kind: "discard", previous: phase };
    focusAfter(() => find(`#${id}-keep`));
  }
  function cancelDiscard() {
    if (phase.kind !== "discard") return;
    phase = phase.previous;
    const target = phase.kind === "conflict" ? "conflict" : phase.kind === "failed" ? "failed" : phase.kind === "review" ? "review" : "text";
    focusAfter(() => find(`#${id}-${target}`));
  }
  function discard() {
    delete app.drafts[path];
    phase = { kind: "edit" };
    focusAfter(() => find(`#${id}-open`));
  }
  function closeUnchanged() {
    delete app.drafts[path];
    focusAfter(() => find(`#${id}-open`));
  }
  function askRemove(n: number | null) {
    const was = removing;
    removing = n;
    focusAfter(() => find(n === null ? `#${id}-v${was}-remove` : `#${id}-v${n}-keep`));
  }
  function removeVersion(n: number) {
    if (!draft) return;
    set(dropVersion($state.snapshot(draft) as Draft, n));
    removing = null;
    app.notify(`Version ${n} of your ${title} draft removed.`);
    focusAfter(() => find(`#${id}-versions`) ?? find(`#${id}-text`));
  }
  function markHandled(change: Unapplied) {
    if (!draft) return;
    const i = draft.unapplied.indexOf(change);
    set(handled(draft, change));
    app.notify(`Marked as handled: your change from version ${change.version} near line ${change.line}.`);
    focusAfter(() => find(`#${id}-unapplied-${i}`) ?? find(`#${id}-unapplied-${i - 1}`) ?? find(`#${id}-reconciled`));
  }
  /** Starts a sentence: "One change", "2 changes". */
  const changeWord = (n: number) => (n === 1 ? "One change" : `${n} changes`);
  /** "1 from version 1 and 2 from version 2" */
  const byVersion = (list: readonly Unapplied[]) =>
    [...new Set(list.map((u) => u.version))].map((v) => `${list.filter((u) => u.version === v).length} from version ${v}`).join(" and ");
  async function downloadCopy() {
    readError = "";
    const r = await Effect.runPromise(Effect.result(server.copyOf(path, true)));
    if (r._tag === "Failure") readError = r.failure.message;
    else download(new Blob([r.success]), "config.yaml");
  }
  const versionWord = (n: number) => (n === 1 ? "earlier version" : `${n} earlier versions`);
</script>

<div class="editor grid gap-2" aria-labelledby={`${id}-title`} role="group">
  <div class="flex flex-wrap items-center gap-2">
    <h3 class="grow" id={`${id}-title`}><span class="mono">{title}</span>{#if unsaved}<span class="draft-tag small"> unsaved draft{draft?.versions.length ? `, ${versionWord(draft.versions.length)}` : ""}</span>{/if}</h3>
    {#if !draft}
      {#if yaml}<button class="btn small quiet" onclick={downloadCopy}>Download</button>{/if}
      <button class="btn small" id={`${id}-open`} aria-disabled={reading} onclick={() => !reading && open()}>{reading ? "Reading…" : readError ? "Try reading again" : "Edit"}<span class="sr"> {title}</span></button>
    {/if}
  </div>
  {#if help}<p class="small muted">{help}</p>{/if}
  {#if readError}<p class="notice bad small" role="alert"><span class="dot bad"></span><span>Could not read {title}: {readError}</span></p>{/if}

  {#if draft}
    {#if phase.kind === "conflict"}
      {@const latest = phase.latest}
      <div class="notice warn small" role="alert" id={`${id}-conflict`} tabindex="-1">
        <span class="dot warn"></span>
        <div class="grid min-w-0 gap-2">
          <span><strong>{title} changed on the server since you started editing.</strong> Nothing was written, and your draft is kept.</span>
          <span>What changed on the server, read at {time(phase.at)}:</span>
          <Diff before={shown2(draft.base)} after={shown2(latest)} label={`Changes on the server to ${title}`} />
          <span>Continuing keeps your current text as version {draft.appliedVersion + 1} and starts again from the server's copy, with each of your changes that does not touch the server's already applied. Changes that do touch them are listed for you to redo.</span>
          <span>Your text below stays editable. Writing waits until you continue: a write now would be refused again.</span>
          <div class="flex flex-wrap gap-2">
            <button class="btn small primary" onclick={() => continueFromLatest(latest)}>Continue from the server's copy</button>
            <Copy text={draft.text} label="Copy my text" />
            <button class="btn small quiet danger" onclick={askDiscard}>Discard my draft…</button>
          </div>
        </div>
      </div>
    {:else if phase.kind === "failed"}
      <div class="notice bad small" role="alert" id={`${id}-failed`} tabindex="-1">
        <span class="dot bad"></span>
        <div class="grid min-w-0 gap-2">
          <span>{title} was not saved: {explain(phase.error)} Your draft is kept.</span>
          <div class="flex flex-wrap gap-2">
            {#if phase.error._tag !== "Invalid"}<button class="btn small" aria-disabled={app.busy} onclick={() => phase.kind === "failed" && save(phase.reviewed)}>Try again</button>{/if}
            <button class="btn small quiet" onclick={keepEditing}>Keep editing</button>
          </div>
        </div>
      </div>
    {/if}

    {#if phase.kind === "review" || phase.kind === "saving"}
      {@const counts = summarize(diffLines(lines(shown2(draft.base)), lines(shown2(draft.text))))}
      <p class="small" id={`${id}-review`} tabindex="-1"><strong>Your changes to {title}:</strong> {describeSummary(counts).toLowerCase()}.{yaml ? " The server checks the YAML before saving and may tidy its layout." : " Shown as indented JSON; the values are what is written."}</p>
      <Diff before={shown2(draft.base)} after={shown2(draft.text)} label={`Your changes to ${title}`} summary={false} />
      {#if draft.versions.length}
        {@const it = draft.unapplied.length === 1 ? "it" : "them"}
        <p class="notice warn small"><span class="dot warn"></span><span>Writing also discards your {versionWord(draft.versions.length)} of this draft.{draft.unapplied.length ? ` ${draft.unapplied.length === 1 ? "One change" : `${draft.unapplied.length} changes`} could not be applied for you (${byVersion(draft.unapplied)}) and ${draft.unapplied.length === 1 ? "is" : "are"} not marked as handled: check the text above includes ${it} if you still want ${it}.` : ""}</span></p>
      {/if}
      <div class="flex flex-wrap gap-2">
        <button class="btn primary small" id={`${id}-write`} aria-disabled={app.busy} onclick={() => phase.kind === "review" && !app.busy && save(phase.reviewed)}>
          {phase.kind === "saving" ? "Checking the server copy…" : draft.versions.length ? `Write, and discard ${versionWord(draft.versions.length)}` : "Write these changes"}
        </button>
        <button class="btn small" aria-disabled={phase.kind === "saving"} onclick={() => phase.kind === "review" && keepEditing()}>Keep editing</button>
      </div>
    {:else if phase.kind === "discard"}
      <div class="notice warn small" role="group" aria-label="Discard changes">
        <span class="dot warn"></span>
        <div class="grid gap-2">
          <span>Discard your draft of <span class="mono">{title}</span>{draft.versions.length ? `, including your ${versionWord(draft.versions.length)}` : ""}? None of it was written to the server.</span>
          <div class="flex flex-wrap gap-2">
            <button class="btn small primary" id={`${id}-keep`} onclick={cancelDiscard}>Keep editing</button>
            <button class="btn small danger" onclick={discard}>Discard{draft.versions.length ? " all" : ""}</button>
          </div>
        </div>
      </div>
    {:else}
      {#if draft.versions.length}
        <div class="notice live small" id={`${id}-reconciled`} tabindex="-1" role="status">
          <span class="dot live"></span>
          <div class="grid min-w-0 gap-2">
            <span>
              When you last continued from the server's copy, {draft.applied} {draft.applied === 1 ? "change" : "changes"} from version {draft.appliedVersion} {draft.applied === 1 ? "was" : "were"} applied automatically. Your text below may include later edits.
              {draft.unapplied.length ? `${changeWord(draft.unapplied.length)} could not be applied (${byVersion(draft.unapplied)}). Each stays listed until you mark it as handled or remove its version.` : "No changes remain marked as needing attention."}
              Marking a change as handled or removing its version does not add it to the text. Check the text before writing.
            </span>
            {#each draft.unapplied as c, i (i)}
              <div class="grid gap-1" id={`${id}-unapplied-${i}`} tabindex="-1">
                <span class="small"><strong>Not applied, from version {c.version}:</strong> near line {c.line} of the text as it was then, where the server also changed lines, you had:</span>
                <div class="code diff">
                  {#each c.mine.removed as t, j (j)}<div class="removed"><span class="ln" aria-hidden="true"></span><span class="mark" aria-hidden="true">−</span><span class="sr">Your change removes: </span>{t || " "}</div>{/each}
                  {#each c.mine.lines as t, j (j)}<div class="added"><span class="ln" aria-hidden="true"></span><span class="mark" aria-hidden="true">+</span><span class="sr">Your change adds: </span>{t || " "}</div>{/each}
                </div>
                <div class="flex flex-wrap gap-2">
                  {#if c.mine.lines.length}<Copy text={c.mine.lines.join("\n")} label="Copy your lines" />{/if}
                  <button class="btn small quiet" onclick={() => markHandled(c)}>Mark as handled<span class="sr">: version {c.version}, near line {c.line}</span></button>
                </div>
              </div>
            {/each}
          </div>
        </div>
        <section class="grid gap-1.5" id={`${id}-versions`} tabindex="-1" aria-label="Earlier versions">
          <h4 class="small">Your earlier versions, kept until you remove them or write</h4>
          {#each draft.versions as v (v.n)}
            {@const vc = summarize(diffLines(lines(shown2(v.base)), lines(shown2(v.text))))}
            <details class="version">
              <summary class="small">Version {v.n}, kept at {time(v.keptAt, false)}: {describeSummary(vc).toLowerCase()}</summary>
              <div class="grid gap-2">
                <Diff before={shown2(v.base)} after={shown2(v.text)} label={`Your changes in version ${v.n}`} summary={false} />
                {#if removing === v.n}
                  <div class="flex flex-wrap items-center gap-2" role="group" aria-label={`Remove version ${v.n}`}>
                    <span class="small">Remove version {v.n}? It was never written to the server.{draft.unapplied.some((u) => u.version === v.n) ? ` Its ${draft.unapplied.filter((u) => u.version === v.n).length === 1 ? "change that was not applied leaves" : "changes that were not applied leave"} the list too.` : ""}</span>
                    <button class="btn small danger" onclick={() => removeVersion(v.n)}>Remove version {v.n}</button>
                    <button class="btn small" id={`${id}-v${v.n}-keep`} onclick={() => askRemove(null)}>Keep it</button>
                  </div>
                {:else}
                  <div class="flex flex-wrap gap-2">
                    <Copy text={v.text} label={`Copy version ${v.n}`} />
                    <button class="btn small quiet danger" id={`${id}-v${v.n}-remove`} onclick={() => askRemove(v.n)}>Remove…<span class="sr"> version {v.n}</span></button>
                  </div>
                {/if}
              </div>
            </details>
          {/each}
        </section>
      {/if}
      <label class="field" for={`${id}-text`}>{draft.versions.length ? "Text to write" : `${title}, ${yaml ? "YAML" : "JSON"}`}</label>
      <textarea id={`${id}-text`} rows={yaml ? 18 : 10} spellcheck="false" aria-invalid={!!invalid} aria-describedby={invalid ? `${id}-invalid` : `${id}-hint`} value={draft.text} oninput={(e) => edit(e.currentTarget.value)}></textarea>
      {#if invalid}<p class="small code-status bad" id={`${id}-invalid`} role="alert">{invalid}</p>{/if}
      <p class="small muted" id={`${id}-hint`}>
        {layoutOnly ? "Only the layout differs from the server's copy, so there is nothing to write." : yaml ? "This file holds client keys and provider API keys in full." : "The section as JSON. Leave out a key to use the server's default."}
      </p>
      {#if phase.kind === "conflict"}
        <p class="small muted">To review and write this text, first continue from the server's copy, above.</p>
      {:else}
        <div class="flex flex-wrap gap-2">
          <button class="btn small primary" aria-disabled={!changesServer(draft)} onclick={() => changesServer(draft) && review()}>Review changes</button>
          {#if unsaved}<button class="btn small quiet" onclick={askDiscard}>Discard draft…</button>
          {:else}<button class="btn small quiet" onclick={closeUnchanged}>Close</button>{/if}
        </div>
      {/if}
    {/if}
  {/if}
</div>

<style>
  .editor h3 {
    color: var(--ink);
    font-size: 14px;
  }
  .draft-tag {
    margin-left: 8px;
    font-family: var(--sans);
    font-weight: 560;
    color: var(--warn-text);
  }
  [tabindex="-1"]:focus {
    outline: none;
  }
  .version > summary {
    padding-block: 5px;
  }
</style>
