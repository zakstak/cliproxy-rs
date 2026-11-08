# cliproxy-rs

[![CI](https://github.com/vayungodara/cliproxy-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/vayungodara/cliproxy-rs/actions/workflows/ci.yml)
[![Stars](https://img.shields.io/github/stars/vayungodara/cliproxy-rs)](https://github.com/vayungodara/cliproxy-rs/stargazers)

Use your Claude, Codex (ChatGPT), Grok and Muse accounts and API keys from Claude Code, Codex CLI and other OpenAI or Anthropic clients: one local endpoint, API translation, failover. Single Rust binary, CLIProxyAPI-compatible for the providers it keeps.

- **Four upstreams.** Claude, Codex, Grok (xAI) and Muse (Meta), by account sign-in or API key. Kimi, Devin, Gemini, Vertex AI, AI Studio, Antigravity and generic OpenAI-compatible upstreams were removed from this fork; [Removed providers](docs/DIFFERENCES-FROM-GO.md#removed-providers) lists what that changes.
- **API translation.** Anthropic Messages, OpenAI Chat Completions and OpenAI Responses clients reach every upstream, tool calls and streaming included.
- **Several keys and providers behind one model name.** By default, a 429 cools that key for the model and the request moves to another ready key or provider with the same alias.
- **One binary with the dashboard built in.** It reads CLIProxyAPI's `config.yaml` and credential files. Parity with CLIProxyAPI v8.0.35 is partial; [PARITY.md](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/PARITY.md) has the counts.

This fork ([zakstak/cliproxy-rs](https://github.com/zakstak/cliproxy-rs)) runs on Linux only, natively or in the Linux container, and uses local builds without release publishing or package distribution. Build it from source on Linux, or build and run the container on any host; see [Quick start](#quick-start).

0.4.0 changes how some configs load. Read [Before you upgrade](https://github.com/vayungodara/cliproxy-rs/releases/tag/v0.4.0#:~:text=Before%20you%20upgrade) in the [release notes](https://github.com/vayungodara/cliproxy-rs/releases/tag/v0.4.0) first.

> **Set it up with your coding agent.** Paste this into Claude Code, Codex or Amp on the machine where the proxy should run:
>
> ```text
> Install cliproxy-rs on this machine by following https://github.com/zakstak/cliproxy-rs/blob/fork/docs/AI-SETUP.md
> ```

## Quick start

1. On Linux, [build from source](docs/INSTALL.md#from-source) and [write `config.yaml` with two keys](docs/INSTALL.md#first-run), then start `cliproxy --config config.yaml`.

   On Windows or macOS, run the Linux container: [Windows and macOS: run the Linux container](docs/INSTALL.md#windows-and-macos-run-the-linux-container). This fork has no native build for those systems. Codex CLI, Claude Code and your browser still run there and connect to the container. [INSTALL.md](docs/INSTALL.md) lists every way to install.

2. Sign in to the dashboard at `http://127.0.0.1:8317/management.html` with your management key. Add a provider API key, or connect your own account. Read [Accounts and provider terms](#accounts-and-provider-terms) before connecting a subscription.

3. Open the dashboard's **Use with tools** page and copy the Codex CLI or Claude Code settings. Your tools use `CLIPROXY_CLIENT_KEY`, not the management key.

To upgrade, pull and rebuild; the config and credentials stay where they are. [INSTALL.md](docs/INSTALL.md) covers start at login, Docker and building from source. [GETTING-STARTED.md](docs/GETTING-STARTED.md) has the longer walkthrough.

## What is this?

cliproxy-rs is a Rust rewrite of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI). It reads the same `config.yaml` and credential formats and implements the client routes and v8 Management API. The dashboard shows provider keys and their traffic, request history and tool settings.

The compatibility target is CLIProxyAPI v8.0.35. Parity is incomplete: the audit has 1,951 items, with 1,140 covered, 794 partial and 17 missing. [PARITY.md](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/PARITY.md) defines those counts.

## How it compares

Checked against each project's own README on 28 October 2026. All five translate between the OpenAI and Anthropic formats, Responses to Chat Completions included, and can move a request to another key when one fails¹.

| | cliproxy-rs | [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) | [claude-code-router](https://github.com/musistudio/claude-code-router) | [LiteLLM](https://github.com/BerriAI/litellm) | [opencodex](https://github.com/lidge-jun/opencodex) |
| --- | --- | --- | --- | --- | --- |
| Language and packaging | Rust, one binary | Go, one binary¹ | Node.js 22+ (npm), desktop app, Docker | Python (pip or uv), Docker | Bun (npm), desktop app, Docker |
| Linux x86_64 release² | 19.0 MB download, 44.6 MB binary with the dashboard | 23.6 MB download, 71.1 MB binary; it downloads its panel at start | Not measured | Not measured | Not measured |
| Memory² | 25.8 MB idle at start; in the field-mix hour, 161.5 MB peak and 97.8 MB after 5 min idle | 49.7 MB idle at start; in the field-mix hour, 198.3 MB peak and 75.8 MB after 5 min idle | Not measured | Not measured | Not measured |
| Built-in dashboard | Yes, in the binary | Separate project (Management Center) | Yes | Yes | Yes |
| Reads CLIProxyAPI's config and credential files | Yes, with [partial parity](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/PARITY.md) | It is the reference | No | No | No |

¹ Not stated in CLIProxyAPI's README; checked in its v8.0.35 source and release assets. cliproxy-rs's translation and failover are ports of that code.

² cliproxy-rs 0.4.0 and CLIProxyAPI v8.0.35 release binaries. Idle at start is the RSS 15 seconds after launch with one API key and no traffic, measured on one Linux machine on 31 October 2026. The field-mix hour sent streamed Claude and Codex requests of about 1 MB for an hour, each server on its own GitHub-hosted runner at the same time, on 28 October 2026. [BENCHMARKS.md](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/BENCHMARKS.md#size-and-idle-memory-040-against-v8035) has both.

Where the others are stronger:

- CLIProxyAPI is the complete implementation that cliproxy-rs follows. It was faster in the synthetic runs under [Performance](#performance), and its README lists many apps built on it.
- LiteLLM covers 100+ providers and adds virtual keys, spend tracking, guardrails and a Python SDK.
- claude-code-router has routing conditions on headers and bodies, ordered fallback models, its own client keys with request and token limits, and a desktop app.
- opencodex supports 40+ providers out of the box, also serves Claude Desktop and Grok Build, and has a desktop app.

## Use it with your tools

The local OpenAI base URL is `http://127.0.0.1:8317/v1`. Claude Code uses `http://127.0.0.1:8317` without `/v1`. Use your installation's port and client key.

[CLIENTS.md](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/CLIENTS.md) has client settings, including [GPT in Claude Code](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/CLIENTS.md#gpt-in-claude-code). Not every model and tool combination has been tested.

## API keys

Provider API keys go under `api-keys` in `config.yaml`, grouped by provider: `claude`, `codex`, `xai` and `meta`. Keep `access.api-keys`, `management.secret-key` and the rest of your config; the proxy reloads the configuration on save. The upstream keys are separate from your proxy's client key.

```yaml
api-keys:
  claude:
    - keys:
        - api-key: "<anthropic-api-key>"
  xai:
    - base-url: "https://api.x.ai/v1"
      keys:
        - api-key: "<xai-api-key>"
```

Put this in `~/.codex/config.toml` to use the proxy from Codex CLI:

```toml
model_provider = "cliproxy"

[model_providers.cliproxy]
name = "cliproxy-rs"
base_url = "http://127.0.0.1:8317/v1"
env_key = "CLIPROXY_CLIENT_KEY"
wire_api = "responses"
```

For failover, add another key to a provider's `keys` list or connect several accounts. By default, a 429 cools that credential for the failing model, and the proxy tries another eligible credential, within the configured attempt limits and error rules. [MULTI-ACCOUNT.md](docs/MULTI-ACCOUNT.md#cooldowns-and-limits) explains retries when none are ready.

A config that still configures a removed provider (`openai-compatibility`, `gemini`, `vertex`, ...) does not load; the error names the setting to remove.

## Accounts and provider terms

This project is for one person using their own accounts on their own machines. Keep the proxy and its keys private. It is not a shared subscription service.

Use the provider's sanctioned path where one exists:

- OpenAI offers [Sign in with ChatGPT](https://developers.openai.com/cookbook/articles/sign-in-with-chatgpt) so eligible users can use their plan in participating tools, including local personal projects. That permission depends on the integration and consent scopes. cliproxy-rs's Codex login follows CLIProxyAPI's existing login flow; it is not the new Sign in with ChatGPT integration. The Management API can bind Sign in with ChatGPT tokens to a Codex credential, and Codex requests use them when the credential's `use_siwc` is on.
- Claude Code supports [gateway configuration](https://code.claude.com/docs/en/llm-gateway-connect). You can route other models into Claude Code through the proxy using their API keys. Anthropic's [authentication rules](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use) reserve Claude subscription OAuth for Claude Code and other native Anthropic applications. Use an API key or supported cloud provider for Claude in third-party tools.

Personal use is still subject to each provider's terms, and providers can restrict or suspend accounts that break them. Sharing an account, or using it to serve other people, is the clearest case.

### Several accounts

Each sign-in creates a file in `auth-dir`. Routing chooses among ready credentials with `soonest-reset` (the default: the account whose weekly limit resets soonest goes first), `round-robin`, `fill-first` or `weighted-round-robin`. Set `routing.session-affinity: true` to keep a conversation on one account. When the setting is omitted, the server default is off. A 429 can move the request to another ready credential for that model. [MULTI-ACCOUNT.md](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/MULTI-ACCOUNT.md) covers routing and access from your other machines.

## CLIProxyAPI compatibility

Config and protocol tests check compatibility with Go CLIProxyAPI at `6fecc6e`. The bundled dashboard has tests against both servers. No third-party CLIProxyAPI app has been tested with cliproxy-rs yet.

Before switching, read [MIGRATING-FROM-GO.md](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/MIGRATING-FROM-GO.md), especially the refresh-token warning. The default Docker image expects `/data/config.yaml` and runs as UID/GID 10001. Update mounts and permissions, or override `--config` and set an accessible `oauth.auth-dir` to keep existing paths.

## Terminal UI

`cliproxy -tui` manages a running server. It uses `-management-base-url`, then `management.base-url`, then `http://127.0.0.1:<port>`, and asks for the management key. `cliproxy -tui -standalone` starts the server in the same process and stops it when you quit. Standalone mode needs a loopback or wildcard `host` and no `server.tls`.

## Upcoming features

Not in cliproxy-rs yet:

- The remaining parity gaps and Go test cases, listed in [PARITY.md](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/PARITY.md).

## Performance

On 23 October 2026, 67 minutes after 0.3.0 started on the maintainer's server, it had answered all 592 requests since start with status 200. Its resident memory was 43.9 MB, and its peak since start 63.7 MB. This is a field observation of real traffic, so it can't be reproduced on demand; [BENCHMARKS.md](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/BENCHMARKS.md#field-memory) records how it was read.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://github.com/vayungodara/cliproxy-rs/raw/master/docs/img/perf-field-soak-dark.svg?sanitize=true">
    <img src="https://github.com/vayungodara/cliproxy-rs/raw/master/docs/img/perf-field-soak-light.svg?sanitize=true" alt="Bar chart of the field-mix soak on 28 October 2026, 60 minutes on two runners at the same time. Requests of 200 KB to 2 MB, about 1 MB on average, peak RSS: Go 198.3 MB, cliproxy-rs 161.5 MB; RSS after 300 s idle: Go 75.8 MB, cliproxy-rs 97.8 MB." width="880">
  </picture>
</p>

In the [field-mix soak](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/BENCHMARKS.md#against-go-v8035) of 28 October 2026, the CLIProxyAPI v8.0.35 and cliproxy-rs 0.4.0 release binaries each served an hour of streamed Claude and Codex requests of 200 KB to 2 MB, plus count_tokens, on a GitHub-hosted runner of their own at the same time. Neither failed a request. cliproxy-rs peaked at 161.5 MB and Go at 198.3 MB. Read 20 seconds after each batch, cliproxy-rs held 85.7 to 102.0 MB and Go 91.4 to 152.8 MB. After 5 minutes without traffic Go had given more back: it held 75.8 MB and cliproxy-rs 97.8 MB, which keeps the o200k_base tokenizer that count_tokens built (46.8 MB when [measured](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/BENCHMARKS.md#what-the-heap-held) on 6 October). On the same CPU model, cliproxy-rs used 49 ms of CPU per request and Go 146 ms.

Go was faster in the 0.1.0 launch build's synthetic runs. It served 1,568 non-streaming requests/s against 1,168, with 0.62 ms of CPU per request against 0.85 ms, and 916 plain streams/s against 833. With 256 slow streams, cliproxy-rs had the worse p99 latency: 1,414.5 ms against Go's 1,272.2 ms.

[BENCHMARKS.md](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/BENCHMARKS.md) has the methods, raw results and other measurements, including the [Claude soak](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/BENCHMARKS.md#claude-soak-large-prompts-and-memory) against CLIProxyAPI 6fecc6e and the [field mix](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/BENCHMARKS.md#field-mix-claude-codex-and-count_tokens) against earlier releases. [Two 32 MB tokenizers](https://dev.to/vayun/two-32-mb-tokenizers-hunting-a-memory-floor-in-a-rust-proxy-4hfl) explains how its memory floor was found.

## Documentation

- [Getting started](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/GETTING-STARTED.md) and [installation](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/INSTALL.md).
- [Client settings](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/CLIENTS.md), [several accounts](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/MULTI-ACCOUNT.md) and [configuration](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/CONFIGURATION.md).
- [Moving from CLIProxyAPI](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/MIGRATING-FROM-GO.md) and [differences from Go](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/DIFFERENCES-FROM-GO.md).
- [Parity](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/PARITY.md) and [benchmarks](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/BENCHMARKS.md).
- [Working in the zakstak fork](docs/FORK.md): branches, upstream sync and contributing fixes upstream.

## Security

The dashboard is built into the binary. Model catalogs are downloaded at start and every three hours; `--local-model` turns that off.

Keep `access.api-keys` set and `server.host` on `127.0.0.1` unless your other machines need access. Behind a local tunnel or reverse proxy, set `server.trusted-proxies`; otherwise internet clients count as local. [Running it safely](https://github.com/vayungodara/cliproxy-rs/blob/master/docs/GETTING-STARTED.md#running-it-safely) explains this. [SECURITY.md](https://github.com/vayungodara/cliproxy-rs/blob/master/SECURITY.md) covers vulnerability reports.

## Development

The Rust workspace is under `crates/`; the dashboard is under `ui/`. Tests use local mock upstreams. CI runs fmt, clippy and the full suite, with Go and PostgreSQL comparison tests. The [differential harness](https://github.com/vayungodara/cliproxy-rs/blob/master/harness/README.md) compares 57 cases against CLIProxyAPI.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

[CONTRIBUTING.md](https://github.com/vayungodara/cliproxy-rs/blob/master/CONTRIBUTING.md) explains how to contribute.

## License and credits

MIT. See [LICENSE](https://github.com/vayungodara/cliproxy-rs/blob/master/LICENSE).

cliproxy-rs is a Rust rewrite of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) by Luis Pater and Router-For.ME, which is also MIT licensed. [NOTICE](https://github.com/vayungodara/cliproxy-rs/blob/master/NOTICE) records the portions derived from it.
