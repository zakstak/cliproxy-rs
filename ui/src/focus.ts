// Keyboard focus across re-renders. When a control replaces itself (Remove… becomes Remove /
// Keep it, Review becomes Write / Keep editing), the focused element leaves the DOM and the
// browser drops focus to the page body. These helpers put focus where the user's next step
// is, after Svelte has rendered the new controls.
import { tick } from "svelte";

/** Focus what `get` returns once the pending render is done. Falls back to `fallback`. */
export async function focusAfter(get: () => Element | null | undefined, fallback?: () => Element | null | undefined) {
  await tick();
  const el = get() ?? fallback?.();
  if (el instanceof HTMLElement) el.focus({ preventScroll: false });
}

/** The first element matching `selector` inside `root` (or the document). */
export const find = (selector: string, root?: ParentNode | null) => (root ?? document).querySelector<HTMLElement>(selector);

/**
 * Try again on a failed read. When the read works, the notice holding the button goes away,
 * which would drop focus to the page: focus moves to `next`, content the read brought back,
 * and `announce` says so. A read that fails again keeps its notice and button, and focus
 * stays there. Focus the user moved elsewhere meanwhile is left alone.
 */
export async function retried(read: Promise<boolean>, next: () => Element | null | undefined, announce: () => void) {
  if (!(await read)) return;
  announce();
  await tick();
  const active = document.activeElement;
  if (active && active !== document.body && active.isConnected) return;
  const el = next() ?? document.getElementById("main");
  if (el instanceof HTMLElement) el.focus({ preventScroll: false });
}
