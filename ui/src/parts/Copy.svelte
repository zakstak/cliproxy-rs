<script lang="ts">
  // A copy button that says what happened where the user is looking: on the button itself.
  let { text, label = "Copy", done = "Copied", small = true, primary = false }: { text: string | (() => string); label?: string; done?: string; small?: boolean; primary?: boolean } = $props();
  let copied = $state<"idle" | "done" | "failed">("idle");
  let timer: ReturnType<typeof setTimeout> | undefined;
  async function copy() {
    try {
      await navigator.clipboard.writeText(typeof text === "function" ? text() : text);
      copied = "done";
    } catch {
      copied = "failed";
    }
    clearTimeout(timer);
    timer = setTimeout(() => (copied = "idle"), 2600);
  }
</script>

<button type="button" class="btn" class:small class:primary onclick={copy}>
  {copied === "done" ? done : copied === "failed" ? "Copy failed: select the text instead" : label}
</button>
<span class="sr" role="status">{copied === "done" ? done : copied === "failed" ? "Copy failed" : ""}</span>
