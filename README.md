# cliproxy-rs

A local proxy for Claude, Codex (ChatGPT), Grok (xAI), and Muse (Meta). It translates Anthropic Messages, OpenAI Chat Completions, and OpenAI Responses, with streaming, tool calls, credential routing, and failover.

This is [zakstak/cliproxy-rs](https://github.com/zakstak/cliproxy-rs), a fork of the [upstream Rust project](https://github.com/vayungodara/cliproxy-rs). It runs on Linux, natively or in a Linux container. Build it from source or with the local `Dockerfile`; this fork has no package distribution or release publishing workflow.

## Quick start

1. [Build the binary](docs/INSTALL.md#from-source) or [build the container](docs/INSTALL.md#docker).
2. [Create `config.yaml`](docs/INSTALL.md#first-run) with separate client and management keys, a private credential directory, and a loopback listen address.
3. Start `cliproxy --config config.yaml` and check `http://127.0.0.1:8317/healthz`.
4. Open `http://127.0.0.1:8317/management.html` and sign in with the management key. Add a provider API key or connect your own account.
5. Open **Use with tools** in the dashboard, or follow [Client settings](docs/CLIENTS.md). Your tools use the client key.

For agent-assisted installation, follow [AI-SETUP.md](docs/AI-SETUP.md). [INSTALL.md](docs/INSTALL.md) also covers containers on Windows and macOS, systemd services, and [safe access](docs/INSTALL.md#running-it-safely).

## API keys

Provider keys go under `api-keys`, grouped by `claude`, `codex`, `xai`, or `meta`. They are separate from the proxy's client keys under `access.api-keys` and the dashboard key under `management.secret-key`.

```yaml
api-keys:
  claude:
    - keys:
        - api-key: "<anthropic-api-key>"
  codex:
    - keys:
        - api-key: "<openai-api-key>"
  xai:
    - base-url: "https://api.x.ai/v1"
      keys:
        - api-key: "<xai-api-key>"
```

Keep the rest of your config. The proxy reloads keys and most settings when the file changes. Add keys to a provider's `keys` list or connect several accounts for failover. [Several accounts](docs/MULTI-ACCOUNT.md) explains routing, session affinity, and cooldowns.

The OpenAI base URL is `http://127.0.0.1:8317/v1`. Claude Code uses `http://127.0.0.1:8317`, without `/v1`. Pick an available model from `GET /v1/models`.

## Accounts and provider terms

Use your own accounts on your own machines. Keep the proxy, its keys, and credential files private.

- OpenAI offers [Sign in with ChatGPT](https://developers.openai.com/cookbook/articles/sign-in-with-chatgpt) for participating integrations. The existing Codex login and SIWC token binding are separate flows; [the Codex contract](docs/codex/CONTRACT.md#endpoint-profiles) describes their endpoint profiles and limits.
- Claude Code supports [gateway configuration](https://code.claude.com/docs/en/llm-gateway-connect). Anthropic's [authentication rules](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use) reserve Claude subscription OAuth for native Anthropic applications. Use API keys for Claude in third-party tools.

## CLIProxyAPI compatibility

The proxy reads CLIProxyAPI's `config.yaml` and credential files and serves its client routes and v8 Management API for the four retained providers. The dashboard is embedded in the binary.

Kimi, Devin, Gemini, Vertex AI, AI Studio, Antigravity, generic OpenAI-compatible upstreams, and native plugins were removed. Configs that enable them fail with migration guidance. Their saved logins stay on disk unread. See [Moving from Go](docs/MIGRATING-FROM-GO.md) and [Differences from Go](docs/DIFFERENCES-FROM-GO.md).

Provider documentation, first-party clients, and verified wire behaviour decide provider behaviour. The [Codex wire contract](docs/codex/CONTRACT.md) defines Codex payload, transport, and identity rules. Go fixtures remain regression evidence with their recorded versions.

## Terminal UI

`cliproxy -tui` manages a running server. It uses `-management-base-url`, then `management.base-url`, then `http://127.0.0.1:<port>`, and asks for the management key.

`cliproxy -tui -standalone` starts the server in the same process and stops it when you quit. Standalone mode needs a loopback or wildcard host and no `server.tls`.

## Documentation and development

[Documentation index](docs/README.md) links setup, configuration, client settings, routing, telemetry, and the Codex contract. [CONTRIBUTING.md](CONTRIBUTING.md) covers local checks, [ui/README.md](ui/README.md) covers the dashboard, and [FORK.md](docs/FORK.md) covers branches and upstream integration.

The Rust workspace is under `crates/`. Tests use loopback mock upstreams and private credential directories. Run checks locally; this fork has no GitHub Actions workflows.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## License and credits

MIT. See [LICENSE](LICENSE), which includes the upstream copyright notices. cliproxy-rs derives from [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) by Luis Pater and Router-For.ME and from [vayungodara/cliproxy-rs](https://github.com/vayungodara/cliproxy-rs).
