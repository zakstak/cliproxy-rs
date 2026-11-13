# Benchmarks (retired)

Issue [#45](https://github.com/zakstak/cliproxy-rs/issues/45) retired the benchmark
program. Features, correct behaviour and reliable operation come first. Benchmark scores
are not a product goal, and none of them gates a change.

Go CLIProxyAPI uses a garbage collector, so its resident memory, heap retention and
collection timing differ from cliproxy-rs by design. Matching Go's memory profile, or
winning a side-by-side comparison, is not a goal.

What was removed:

- The CI size, idle and image-size gates, and `bench/budgets.txt`.
- The weekly and release-tag memory soaks (`soak.yml`) and the idle comparison workflow
  (`idle.yml`).
- The per-request heap gate (`crates/cpa-server/tests/alloc_budget.rs`), and the
  allocation and wall-clock bounds in other tests.
- The load generators, fake upstreams, and the soak, idle and latency scripts under
  `bench/`, and the `claude_latency` example.
- The dashboard bundle-size check (`ui/scripts/size.mjs`).

Tests that check a configured timeout, a cancellation, a deadline or a resource limit stay.
Those limits are behaviour, not benchmarks.

The raw results of earlier runs stay in [`bench/results/`](../bench/results) as dated
historical records. This page's earlier tables, methods and comparisons with Go are in Git
history, at any commit before #45. Treat them as observations from that time, not as
current claims.

The prompt-cache check and offline replay tools are functional tests of cache behaviour.
They moved to [`tools/claude-cache/`](../tools/claude-cache); see
[Claude cache telemetry](CLAUDE-CACHE-TELEMETRY.md).
