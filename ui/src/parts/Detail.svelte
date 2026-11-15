<script lang="ts">
  // One detail view: an account, a request, a log line. Inline (1100 px and wider) it is a
  // non-modal pane beside the list, so the list stays usable. Narrower, it covers the page as
  // a side sheet, full screen on a phone: a native modal dialog, so the page behind is inert,
  // Tab stays inside, Escape and the backdrop close it.
  // Either way it takes focus when it opens or shows another item, and gives focus back to
  // whatever opened it when it closes.
  import { onMount, tick, type Snippet } from "svelte";
  import { app } from "../app.svelte";

  let {
    label,
    modal,
    item,
    onclose,
    returnTo = [],
    head,
    children,
  }: {
    /** Accessible name, such as "zak@example.invalid details". */
    label: string;
    /** Cover the page as a modal dialog instead of sitting inline. */
    modal: boolean;
    /** Identity of what is shown; a new value moves focus into the detail again. */
    item: string;
    onclose: () => void;
    /** Where focus goes on close when the element that opened the detail is gone: selectors, tried in order. */
    returnTo?: readonly string[];
    head: Snippet;
    children: Snippet;
  } = $props();

  let box = $state<HTMLElement>();
  // Captured before anything renders: showModal() moves focus into the dialog at once.
  let opener: Element | null = document.activeElement;
  let dismissed = false;
  /** The screen this detail belongs to. Leaving the screen moves focus to the new one instead. */
  const page = app.route.page;
  // A plain copy of `returnTo`, kept while the detail is live. Closing can tear the screen
  // down with it, and reading the screen's state then would read values already destroyed.
  let fallback: readonly string[] = [];
  $effect.pre(() => {
    fallback = returnTo;
  });
  function fallbackTarget(): HTMLElement | null {
    for (const selector of fallback) {
      const el = document.querySelector<HTMLElement>(selector);
      if (el && !el.closest("[inert]")) return el;
    }
    return null;
  }

  function dismiss() {
    // Resolve the fallback before navigation removes the selected item. A responsive
    // layout can replace the original row while its detail stays open.
    const target = opener instanceof HTMLElement && opener.isConnected && opener.getClientRects().length && !opener.closest("[inert]")
      ? opener
      : fallbackTarget();
    dismissed = true;
    // Hash navigation renders on hashchange. End modal inertness before restoring
    // focus rather than trying to focus the background while the dialog is open.
    if (box instanceof HTMLDialogElement && box.open) box.close();
    onclose();
    tick().then(() => {
      if (target?.isConnected) target.focus({ preventScroll: false });
    });
  }

  function takeFocus() {
    const title = box?.querySelector<HTMLElement>("[data-detail-title]");
    (title ?? box)?.focus({ preventScroll: true });
  }
  // The element changes when the viewport crosses the 1100 px breakpoint (pane ⇄ sheet):
  // open the new dialog, and keep focus in the detail if it was there.
  let current: HTMLElement | undefined;
  $effect(() => {
    const el = box;
    if (!el || el === current) return;
    const first = !current;
    current = el;
    if (el instanceof HTMLDialogElement && !el.open) el.showModal();
    const active = document.activeElement;
    if (!first && (!active || active === document.body)) takeFocus();
  });
  onMount(() => {
    takeFocus();
    return () => {
      // Teardown reads no reactive state: `current` is the element itself, not the `box` state.
      // Give focus back only if it was in here, or nowhere: never steal it from a control
      // the user moved to, such as the next row in the list.
      const el = current;
      const active = document.activeElement;
      const had = !active || active === document.body || !!el?.contains(active);
      if (el instanceof HTMLDialogElement && el.open) el.close();
      if (dismissed || !had) return;
      tick().then(() => {
        // The route is read once the change has rendered; during teardown it can still show the
        // screen being left. Leaving the screen moves focus to the new screen instead.
        if (app.route.page !== page) return;
        const target = opener instanceof HTMLElement && opener.isConnected && !opener.closest("[inert]") ? opener : fallbackTarget();
        target?.focus({ preventScroll: false });
      });
    };
  });
  // Another item opened in the same detail.
  let shown = "";
  $effect(() => {
    const k = item;
    if (shown && k !== shown) {
      if (document.activeElement && !box?.contains(document.activeElement)) opener = document.activeElement;
      tick().then(takeFocus);
    }
    shown = k;
  });

  function onkey(e: KeyboardEvent) {
    // Inline: Escape closes, unless a modal dialog above (Add account) has it.
    if (modal || e.key !== "Escape" || e.defaultPrevented || document.querySelector("dialog[open]")) return;
    const t = e.target;
    // Escape inside a field first leaves the field as browsers expect; a second press closes.
    if (t instanceof HTMLInputElement && t.type === "search" && t.value) return;
    e.preventDefault();
    dismiss();
  }
</script>

<svelte:window onkeydown={onkey} />

{#snippet inner()}
  <header>
    <div class="grow">{@render head()}</div>
    <button class="btn small close" onclick={dismiss}>Close<span class="sr"> {label}</span></button>
  </header>
  <div class="body">{@render children()}</div>
{/snippet}

{#if modal}
  <dialog
    bind:this={box}
    class="panel detail-dialog"
    aria-label={label}
    oncancel={(e) => (e.preventDefault(), dismiss())}
    onclick={(e) => e.target === e.currentTarget && dismiss()}
  >
    {@render inner()}
  </dialog>
{:else}
  <aside bind:this={box} class="panel" tabindex="-1" aria-label={label}>
    {@render inner()}
  </aside>
{/if}
