# Documentation

## Run the proxy

- [Install and run](INSTALL.md): source builds, Docker, first config, services, and safe access.
- [Agent-assisted setup](AI-SETUP.md): installation without overwriting an existing setup or exposing keys.
- [Client settings](CLIENTS.md): Claude Code, Codex CLI, Amp, other tools, and SDKs.
- [Configuration](CONFIGURATION.md): supported settings and reload behaviour.
- [Several accounts](MULTI-ACCOUNT.md): routing, affinity, cooldowns, quotas, and remote access.
- [Moving from Go CLIProxyAPI](MIGRATING-FROM-GO.md): migration, credential safety, and switching back.
- [Differences from Go](DIFFERENCES-FROM-GO.md): operator-visible changes and limitations.
- [Claude cache telemetry](CLAUDE-CACHE-TELEMETRY.md): collection, privacy, and offline replay.

## Develop the fork

- [Contributing](../CONTRIBUTING.md): build requirements and local checks.
- [Fork workflow](FORK.md): branches, upstream integration, and local-build policy.
- [Codex wire contract](codex/CONTRACT.md): payloads, transports, endpoint profiles, and fixture rules.
- [Codex identity and transport](codex/IDENTITY.md): headers, credential isolation, and TLS policy.
- [Go parity test migration](GO-PARITY-REMOVAL.md) and [original inventory](GO-PARITY-TEST-INVENTORY.md): independent behavior coverage, dispositions and replacement difficulty.
- [Features not adopted](NOT-ADOPTED.md): current omissions and the reasons for them.
- [Dashboard development](../ui/README.md) and [design system](../ui/DESIGN.md).
