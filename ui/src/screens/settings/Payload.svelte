<script lang="ts">
  // Payload rules: values the proxy sets in, or removes from, matching requests. Writes are
  // frozen operations (server.ts); removing a rule asks first, as other removals do.
  import { onDestroy } from "svelte";
  import { type Data } from "../../core";
  import { app } from "../../app.svelte";
  import { configWrite, explain, type ConfigWrite, type WriteError } from "../../server";
  import { focusAfter, find } from "../../focus";

  const kinds: [string, string][] = [
    ["default", "Set a value the request lacks."],
    ["default-raw", "Default, with raw JSON values."],
    ["override", "Always set the value."],
    ["override-raw", "Override, with raw JSON values."],
    ["filter", "Remove paths from the request."],
  ];
  let kind = $state("default");
  const payload = $derived(app.setting<Data>("requests/payload", {}));
  const rules = $derived((payload[kind] || []) as Data[]);
  const ruleName = (rule: Data) => (rule.models || []).map((m: Data) => `${m.name}${m.protocol ? ` (${m.protocol})` : ""}`).join(", ") || "every model";
  let form = $state({ model: "*", protocol: "", params: '{\n  "temperature": 0.7\n}' });
  let formError = $state("");
  let failure = $state<{ op: ConfigWrite; done: string; error: WriteError } | null>(null);
  let context = 0;
  let gone = false;
  onDestroy(() => { gone = true; });
  function select(value: string) {
    context++;
    kind = value;
    failure = null;
    removing = -1;
  }
  const operation = (next: Data[], what: string) => configWrite({ path: `requests/payload/${kind}`, before: rules, next, what, href: "#settings/payload" });
  async function write(op: ConfigWrite, done: string) {
    const started = context;
    failure = null;
    const error = await app.write(op, done);
    // An answer for another rule type is reported where the user is now, not in this list.
    if (gone || started !== context) {
      if (error) app.failedAway(op, error, done);
      return false;
    }
    if (error) {
      failure = { op, done, error };
      focusAfter(() => find(".payload-failure"));
    }
    return !error;
  }
  async function add(e: SubmitEvent) {
    e.preventDefault();
    formError = "";
    let params: unknown;
    try {
      params = JSON.parse(form.params);
    } catch {
      formError = "The values are not valid JSON.";
    }
    if (!formError) {
      const list = Array.isArray(params);
      if (kind === "filter" ? !list : list || !params || typeof params !== "object")
        formError = kind === "filter" ? 'Filter values are a JSON list of paths, such as ["metadata"].' : "Values are a JSON object of path → value.";
    }
    if (formError) return void focusAfter(() => find("#rule-params"));
    const model: Data = { name: form.model.trim() };
    if (form.protocol) model.protocol = form.protocol;
    await write(operation([...rules, { models: [model], params }], `Adding ${article(kind)} ${kind} rule for ${model.name}`), "Rule added.");
  }
  const article = (k: string) => (/^[aeiou]/.test(k) ? "an" : "a");
  /** The rule being confirmed for removal, by index; -1 for none. */
  let removing = $state(-1);
  function askRemove(i: number) {
    const was = removing;
    removing = i;
    focusAfter(() => (i < 0 ? find(`[data-rule="${was}"]`) : find(".rule-confirm .keep")));
  }
  async function remove(i: number) {
    const rule = rules[i];
    if (await write(operation(rules.filter((_, n) => n !== i), `Removing the ${kind} rule for ${ruleName(rule)}`), `The ${kind} rule for ${ruleName(rule)} was removed.`)) {
      removing = -1;
      focusAfter(() => find("#rule-model"));
    }
  }
</script>

<div class="seg max-w-full flex-wrap" role="group" aria-label="Rule type">
  {#each kinds as [k] (k)}<button aria-pressed={kind === k} onclick={() => select(k)}>{k} <span class="small">{(payload[k] || []).length || ""}</span></button>{/each}
</div>
<p class="small muted">{kinds.find((k) => k[0] === kind)?.[1]} Rules match on model and protocol. Codex requests are never rewritten.</p>
{#if failure}
  <div class="notice bad small payload-failure" role="alert" tabindex="-1">
    <span class="dot bad"></span>
    <div class="grid min-w-0 gap-2">
      <span>{failure.op.what} failed: {explain(failure.error)}</span>
      <div class="flex flex-wrap gap-2">
        {#if failure.error._tag !== "Changed"}<button class="btn small" aria-disabled={app.busy} onclick={() => failure && write(failure.op, failure.done)}>Try again</button>{/if}
        <button class="btn small quiet" onclick={() => (failure = null)}>Dismiss</button>
      </div>
    </div>
  </div>
{/if}
{#if rules.length}
  <ul class="lines">
    {#each rules as rule, i (i)}
      <li class="rule">
        <div class="grid min-w-0 grow gap-1">
          <span class="small">{ruleName(rule)}</span>
          <pre class="code">{JSON.stringify(rule.params, null, 2)}</pre>
        </div>
        {#if removing === i}
          <div class="rule-confirm grid basis-full gap-2" role="group" aria-label={`Remove the ${kind} rule for ${ruleName(rule)}`}>
            <p class="small">Remove the {kind} rule for {ruleName(rule)}? Matching requests are sent without it from the next request.</p>
            <div class="flex flex-wrap gap-2"><button class="btn small danger" aria-disabled={app.busy} onclick={() => remove(i)}>Remove rule</button><button class="btn small keep" onclick={() => askRemove(-1)}>Keep it</button></div>
          </div>
        {:else}
          <button class="btn small quiet danger" data-rule={i} onclick={() => askRemove(i)}>Remove…<span class="sr"> the {kind} rule for {ruleName(rule)}</span></button>
        {/if}
      </li>
    {/each}
  </ul>
{:else}<p class="small muted">No {kind} rules.</p>{/if}
<form class="group" onsubmit={add} novalidate>
  <h3>Add {article(kind)} {kind} rule</h3>
  <div class="form-row">
    <label class="field">Model pattern<input id="rule-model" type="text" required bind:value={form.model} /></label>
    <label class="field">Protocol<select bind:value={form.protocol}><option value="">Any</option>{#each ["openai", "claude"] as p (p)}<option>{p}</option>{/each}</select></label>
  </div>
  <label class="field">{kind === "filter" ? "Paths, as a JSON list" : "Values, as a JSON object of path → value"}<textarea id="rule-params" rows="5" spellcheck="false" bind:value={form.params} aria-invalid={!!formError} aria-describedby={formError ? "rule-error" : undefined}></textarea></label>
  {#if formError}<p class="small code-status bad" id="rule-error" role="alert">{formError}</p>{/if}
  <div class="row"><button class="btn small primary" aria-disabled={app.busy}>Add rule</button></div>
</form>
