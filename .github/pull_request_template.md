## What this changes

<!-- One or two sentences: the behaviour before and after. Link the issue if there is one. -->

## How it is tested

<!-- The test that fails without this change, and anything you checked by hand. -->

## Checklist

- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings` and `cargo test --workspace --locked` pass.
- [ ] No real keys, tokens or account data in code, tests or logs.
- [ ] Supported clients, configs and credential files that worked before still work. A change an operator moving from CLIProxyAPI would notice has a note in `docs/DIFFERENCES-FROM-GO.md`.
- [ ] Docs updated if users see the change.
