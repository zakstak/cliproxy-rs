/** One bar of `Bars.svelte`: a value, the part of it that failed, and what to read out for it. */
export type Bar = { label: string; value: number; flagged?: number; hole?: boolean; tip: string };
/** One row of `Timeline.svelte`: an account, keyed by its auth index. */
export type Lane = { key: string; name: string; sub: string; state: "ok" | "warn" | "bad" | "off" };
/** One time step of `Trend.svelte`: a main amount and a second one stacked on it. */
export type Step = { t: number; a: number; b: number };
