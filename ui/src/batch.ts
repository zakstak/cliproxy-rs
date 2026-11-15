// Batch actions on accounts. Each unit runs on its own, past ordinary failures, so one bad
// account never stops the rest; the outcome names every unit that failed and why. With
// `done`, the outcome is read back from the server instead of trusted from the answers:
// a unit already in the wanted state is skipped, and a unit is failed when it is not in
// that state afterwards, whatever its request said. Pure: covered by batch.test.ts, and with
// the shared latest-wins read in latest.test.ts.
import type { ReadResult } from "./latest.ts";

export type Outcome<T> = { ok: T[]; failed: { unit: T; why: string }[] };

export async function runBatch<T>(
  units: T[],
  run: (u: T) => Promise<unknown>,
  opts: { reread?: () => Promise<void>; done?: (u: T) => boolean } = {},
): Promise<Outcome<T>> {
  const why = new Map<T, string>();
  for (const u of units) {
    if (opts.done?.(u)) continue;
    try {
      await run(u);
    } catch (e) {
      why.set(u, e instanceof Error ? e.message : String(e));
    }
    // A reread after each unit shows progress and lets `done` see changes another unit made.
    await opts.reread?.();
  }
  const failedUnits = units.filter((u) => (opts.done ? !opts.done(u) : why.has(u)));
  return {
    ok: units.filter((u) => !failedUnits.includes(u)),
    failed: failedUnits.map((unit) => ({ unit, why: why.get(unit) ?? "the server did not report the change afterwards" })),
  };
}

/**
 * A reread for a readback: it waits for a read that started after the change and answered.
 * A read that failed or was cancelled stops the batch, so `done` never judges the list from
 * before the change.
 */
export const confirmingReread =
  (read: () => Promise<ReadResult<unknown, { message: string }>>) =>
  async (): Promise<void> => {
    const r = await read();
    if (!r.ok) throw new Error(`The account list did not reload (${r.error.message}), so the result could not be confirmed. Check the list before trying again.`);
  };

/** "3 accounts disabled. 1 failed: a@x (HTTP 500)" */
export function describe<T>(o: Outcome<T>, name: (u: T) => string, verb: string, noun = "account"): string {
  const n = (k: number) => `${k} ${noun}${k === 1 ? "" : "s"}`;
  const head = `${n(o.ok.length)} ${verb}.`;
  if (!o.failed.length) return head;
  return `${head} ${o.failed.length} failed: ${o.failed.map((f) => `${name(f.unit)} (${f.why})`).join(", ")}.`;
}
