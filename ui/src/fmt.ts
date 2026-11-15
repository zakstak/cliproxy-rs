const two = (n: number) => String(n).padStart(2, "0");

export const num = (n: number) => n.toLocaleString("en-US");
/** 1,234 below ten thousand; 48.2K and 1.3M above, for figures that are read at a glance. */
export const compact = (n: number) =>
  n < 10_000 ? num(n) : new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 }).format(n);
export const duration = (ms: number) => (ms < 1000 ? `${Math.round(ms)} ms` : `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`);
/** A share as a whole percentage; "—" when there is nothing to take a share of. */
export const percent = (part: number, whole: number) => (whole > 0 ? `${Math.round((part / whole) * 100)}%` : "—");
/** 14:02:11, on the 24-hour clock the server log uses. */
export function time(t: number, seconds = true) {
  const d = new Date(t);
  return `${two(d.getHours())}:${two(d.getMinutes())}${seconds ? `:${two(d.getSeconds())}` : ""}`;
}
/** A time today as 14:02; another day as Tue 14:02. */
export function when(t: number, now = Date.now()) {
  const d = new Date(t);
  const day = d.toDateString() === new Date(now).toDateString() ? "" : `${d.toLocaleDateString("en-US", { weekday: "short" })} `;
  return `${day}${time(t, false)}`;
}
export const plural = (n: number, one: string, many = `${one}s`) => `${num(n)} ${n === 1 ? one : many}`;
/** Pretty JSON when the text is JSON; the text unchanged otherwise. */
export function pretty(text: string) {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}
/** A round number at or above `n`, for the top of an axis: 1, 2 or 5 times a power of ten. */
export function niceMax(n: number) {
  if (n <= 0) return 1;
  const step = 10 ** Math.floor(Math.log10(n));
  return [1, 2, 5, 10].map((m) => m * step).find((v) => v >= n) ?? n;
}
