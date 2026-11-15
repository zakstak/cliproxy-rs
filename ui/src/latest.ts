// One replaceable server read: the latest read wins. Starting a read interrupts the one still
// running, which aborts its request, and an older read can never report or store its answer
// after a newer one started.
//
// What a caller awaits is a fresh answer, not just "the read stopped": the promise from
// `start()` settles with the first read started at or after that call that finishes. When a
// newer read replaces it (the 10-second account refresh starting during a batch's readback,
// say), the promise follows that newer read, which also started after the caller's change.
// A failure, or `cancel()`, settles it as a failure, so a caller confirming a change never
// mistakes the previous answer for a new one. Covered by latest.test.ts.
import { Data, Effect, Exit, Result, type Fiber } from "effect";

/** The read was cancelled (sign-out) or stopped unexpectedly: no new answer came. */
export class Stopped extends Data.TaggedError("Stopped")<{ readonly message: string }> {}

export type ReadResult<T, E> = { readonly ok: true; readonly data: T } | { readonly ok: false; readonly error: E | Stopped };

/** Called for the latest read only, before its waiters hear. */
type Handlers<T, E> = { success: (data: T) => void; failure: (error: E) => void; settled: () => void };

export class LatestRead<T, E> {
  #generation = 0;
  #fiber: Fiber.Fiber<Result.Result<T, E>> | null = null;
  #waiting: ((r: ReadResult<T, E>) => void)[] = [];

  readonly #read: Effect.Effect<T, E>;
  readonly #on: Handlers<T, E>;
  constructor(read: Effect.Effect<T, E>, on: Handlers<T, E>) {
    this.#read = read;
    this.#on = on;
  }

  start(): Promise<ReadResult<T, E>> {
    const mine = ++this.#generation;
    this.#fiber?.interruptUnsafe();
    const answer = new Promise<ReadResult<T, E>>((resolve) => this.#waiting.push(resolve));
    const fiber = Effect.runFork(Effect.result(this.#read));
    this.#fiber = fiber;
    fiber.addObserver((exit) => {
      // An interrupted or replaced read says nothing: a newer one owns the waiters.
      if (mine !== this.#generation) return;
      this.#fiber = null;
      if (Exit.isSuccess(exit)) {
        const r = exit.value;
        if (Result.isSuccess(r)) {
          this.#on.success(r.success);
          return this.#settle({ ok: true, data: r.success });
        }
        this.#on.failure(r.failure);
        return this.#settle({ ok: false, error: r.failure });
      }
      // A defect in the read program: no answer, and nothing to store.
      this.#settle({ ok: false, error: new Stopped({ message: "The read stopped before the server answered." }) });
    });
    return answer;
  }

  /**
   * A background refresh: joins the read already running instead of replacing it, so a
   * refresh never aborts a read someone is waiting on, and reads slower than the refresh
   * interval still finish.
   */
  refresh(): Promise<ReadResult<T, E>> {
    return this.#fiber ? new Promise((resolve) => this.#waiting.push(resolve)) : this.start();
  }

  /** Stops the running read. Anyone waiting hears that no new answer came. */
  cancel() {
    this.#generation++;
    this.#fiber?.interruptUnsafe();
    this.#fiber = null;
    this.#settle({ ok: false, error: new Stopped({ message: "The read was cancelled." }) });
  }

  #settle(r: ReadResult<T, E>) {
    const waiting = this.#waiting;
    this.#waiting = [];
    this.#on.settled();
    waiting.forEach((resolve) => resolve(r));
  }
}
