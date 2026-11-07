# Installing cliproxy-rs

cliproxy-rs is one executable, `cliproxy`, with the dashboard built in. It needs a `config.yaml` and a directory for credential files. Nothing else is installed.

## Supported hosts

This fork runs on Linux only: natively on a Linux host, or in the Linux container from the repository's `Dockerfile`. It builds no native Windows or macOS binaries. A native Windows or macOS build stops at once with an error that points here.

On a Windows or macOS computer, run the proxy in the Linux container ([Windows and macOS: run the Linux container](#windows-and-macos-run-the-linux-container)) or on a Linux machine you can reach. The tools that use the proxy, such as Codex CLI, Claude Code and the dashboard in your browser, still run on Windows and macOS and connect to it over HTTP.

## Pick an installation path

This fork, [zakstak/cliproxy-rs](https://github.com/zakstak/cliproxy-rs), uses local builds and has no release publishing or package distribution. Install it in one of two ways:

- On a Linux host, [build it from source](#from-source), then [write the config](#first-run) or [run it as a service](#running-as-a-system-service).
- On any host with a container runtime, [build and run the Linux container](#docker).

## From source

Build this fork on a Linux host. You need a Rust toolchain (stable, 1.88 or newer) and the tools to build BoringSSL: `cmake`, `clang` and `perl`. On Debian or Ubuntu:

```sh
sudo apt-get install -y build-essential cmake clang perl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

The build target must be Linux. A `cargo build` for a Windows or macOS target stops with an error that points to the container instead.

Then build:

```sh
git clone https://github.com/zakstak/cliproxy-rs.git
cd cliproxy-rs
cargo build --release -p cliproxy
./target/release/cliproxy --version
```

To run it as `cliproxy`, copy it to a directory on your `PATH`, for example:

```sh
mkdir -p ~/.local/bin
install -m 0755 target/release/cliproxy ~/.local/bin/cliproxy
```

The WebRTC media relay is an optional feature and is disabled in default builds and the Docker image. To include it, build with `cargo build --release -p cliproxy --features cpa-server/media-relay`.

The dashboard in `ui/dist` is committed, so Node is not needed for a normal build. To change the dashboard, see [ui/README.md](../ui/README.md); rebuild the UI before building the binary.

## Docker

The repository's `Dockerfile` builds the binary and copies it into a small Debian image that runs as an unprivileged user (uid and gid 10001). The build compiles BoringSSL and needs about 2 GB of memory; on a small machine add `--build-arg BUILD_JOBS=1`. This is also how to run the proxy on Windows and macOS; see [Windows and macOS: run the Linux container](#windows-and-macos-run-the-linux-container).

On a Linux host, a bind mount keeps the config and credentials in a folder you can read:

```sh
docker build -t cliproxy-rs .
mkdir -p data/auth
$EDITOR data/config.yaml                  # see the notes below
sudo chown -R 10001:10001 data            # the container runs as uid and gid 10001
docker run -d --name cliproxy -p 127.0.0.1:8317:8317 -v "$PWD/data:/data" cliproxy-rs
```

Inside the container the config is `/data/config.yaml`. In that config:

- Set `server.host` to `""` or `0.0.0.0` so the server listens on the container's interface.
- Set `oauth.auth-dir` to `/data/auth` so credentials persist in the mounted volume.
- Set `management.allow-remote: true` if you want the dashboard. Docker forwards the published port from its bridge network, so even requests from your own machine reach the server from a non-local address, and with `allow-remote: false` the management API refuses them ("remote management disabled"). Publishing the port on `127.0.0.1` as above still keeps other machines out. The Go server behaves the same way in a container.

The image declares no `VOLUME`, so `/data` persists only where you mount one: `-v` as above, a named Docker volume, or your host's volume feature. On Railway, attach a Railway Volume mounted at `/data`; Railway mounts volumes as root, so also set `RAILWAY_RUN_UID=0` on the service, as [Railway's volume docs](https://docs.railway.com/volumes#permissions) say for images that run as another user (the proxy then runs as root). Without a mount, credentials and dashboard changes stay in the container and are lost when it is removed. Images up to 0.2.5 declared `VOLUME ["/data"]`, so a container started from one without a mount kept `/data` in an anonymous volume; reattach or copy that volume before you recreate the container from a newer image.

The server must be able to write to `/data`: it saves the hashed management key into `config.yaml` on first start, and the dashboard writes settings and credential files. If it cannot, the key stays in plain text and saves fail, which is what the `chown` above prevents. Publish the port on `127.0.0.1` unless other machines need access; see [running it safely](GETTING-STARTED.md#running-it-safely).

### Sign in to accounts from a container

A browser sign-in ends with a redirect to a callback port on `127.0.0.1`. How that redirect reaches the proxy depends on where you start the sign-in.

**From the dashboard (recommended).** For Claude, Codex and Antigravity, the server starts a callback forwarder on the provider's port, listening on all of the container's interfaces. It redirects the browser on to `http://127.0.0.1:<server.port>/<provider>/callback`. For that to work:

- Publish each callback port with the same number on the host: 54545 (Claude), 1455 (Codex) and 51121 (Antigravity).
- Publish the server port with the same number too (`-p 127.0.0.1:8317:8317` for the default `server.port: 8317`), because the redirect names `server.port`.

```sh
docker run -d --name cliproxy \
  -p 127.0.0.1:8317:8317 -p 127.0.0.1:54545:54545 -p 127.0.0.1:1455:1455 -p 127.0.0.1:51121:51121 \
  -v "$PWD/data:/data" cliproxy-rs
```

**From the command line inside the container.** These logins run their own callback listener instead of the forwarder:

- `--claude-login` (port 54545) and `--antigravity-login` (port 51121) listen on all interfaces, so the published ports above reach them. Add `--no-browser` and open the printed URL on the host.
- `--codex-login` (port 1455), `-siwc-bind` (port 1455) and `--devin-login` listen only on the container's own `127.0.0.1`, which a published port cannot reach. Sign in from the dashboard or use a device-code login instead. `--devin-login` also offers to paste the redirect URL after five seconds.

Device-code logins need no callback port: `--codex-device-login`, Kimi, Meta and xAI. If you cannot publish the callback ports, use one of those, or paste the final callback URL into the dashboard:

```sh
docker exec -it cliproxy cliproxy --config /data/config.yaml --codex-device-login
```

### Windows and macOS: run the Linux container

This fork has no native Windows or macOS build. On those systems, run the same Linux container with a container runtime such as Docker Desktop or Podman. Your tools keep running natively and connect to `http://127.0.0.1:8317`.

A named volume avoids host file ownership differences: the volume starts with the image's `/data`, owned by the container user. These commands work in a POSIX shell and in PowerShell:

1. Build the image and create the volume:

   ```sh
   docker build -t cliproxy-rs .
   docker volume create cliproxy-data
   ```

2. Write `config.yaml` on your computer with the settings from [Docker](#docker), for example:

   ```yaml
   server:
     host: ""
     port: 8317
   access:
     api-keys:
       - "PASTE-CLIENT-KEY"
   management:
     secret-key: "PASTE-MANAGEMENT-KEY"
     allow-remote: true
   oauth:
     auth-dir: "/data/auth"
   ```

3. Copy it into the volume as the container user, so the server can rewrite it later:

   ```sh
   cat config.yaml | docker run --rm -i -v cliproxy-data:/data --entrypoint sh cliproxy-rs -c 'mkdir -p /data/auth && cat > /data/config.yaml'
   ```

   In PowerShell, use `Get-Content config.yaml | docker run ...` with the same arguments.

4. Start the proxy with the server and callback ports published:

   ```sh
   docker run -d --name cliproxy --restart unless-stopped -p 127.0.0.1:8317:8317 -p 127.0.0.1:54545:54545 -p 127.0.0.1:1455:1455 -p 127.0.0.1:51121:51121 -v cliproxy-data:/data cliproxy-rs
   ```

5. Check it with `curl http://127.0.0.1:8317/healthz`, then open `http://127.0.0.1:8317/management.html`.

The volume keeps the config and every signed-in account when you remove and recreate the container, for example to move to a newer image. `--restart unless-stopped` starts the proxy again when the container runtime starts.

These steps were tested with rootless Podman on a Linux x86_64 host. They have not been tested on Windows or macOS hosts, on Docker Desktop, or on arm64. Check these points on such a host:

- Whether the container runtime starts at login, which `--restart unless-stopped` needs to bring the proxy back after a reboot.
- That the browser on the host reaches the published `127.0.0.1` ports, which the callback redirects use.
- On an Apple silicon Mac, the image builds for arm64 Linux. That build has not been tested.

## First run

After building the binary, set it up by hand:

1. Make two random keys, for example with `openssl rand -hex 24`, and write `config.yaml`:

   ```yaml
   server:
     host: "127.0.0.1"          # only this computer can connect
     port: 8317
   access:
     api-keys:
       - "PASTE-CLIENT-KEY"     # your tools send this key to the proxy
   management:
     secret-key: "PASTE-MANAGEMENT-KEY"  # the dashboard password
   oauth:
     auth-dir: "~/.cliproxy-rs/auth"     # where account sign-ins are saved
   ```

   Keep a copy of the management key: on first start the server replaces it in `config.yaml` with a bcrypt hash. Every setting in CLIProxyAPI's [`config.example.yaml`](https://github.com/router-for-me/CLIProxyAPI/blob/main/config.example.yaml) is accepted, although settings for features listed as not yet supported have no effect; [CONFIGURATION.md](CONFIGURATION.md) explains the common ones.
2. Start the server: `cliproxy --config config.yaml`. With no `--config`, it reads `config.yaml` in the current directory. Check it with `curl http://127.0.0.1:8317/healthz`, which answers `{"status":"ok"}`.
3. Connect accounts with `--claude-login`, `--codex-login`, `--codex-device-login`, `--antigravity-login`, `--kimi-login`, `--kimi-ai-login`, `--meta-login`, `--xai-login` or `--devin-login`, import a Vertex AI service account with `--vertex-import key.json`, or connect accounts from the dashboard at `/management.html`. Add `--no-browser` on a machine without a browser.

Go-style single-dash flags such as `-config config.yaml` work too.

The server reloads `config.yaml` and the credential directory when they change. Set `RUST_LOG=debug` for more detailed logs; the default level is `info`.

## Start at login

For a binary you built yourself, write a systemd user unit. Adjust the paths:

```ini
# ~/.config/systemd/user/cliproxy.service
[Unit]
Description=cliproxy-rs

[Service]
ExecStart=%h/.local/bin/cliproxy --config %h/.cliproxy-rs/config.yaml
WorkingDirectory=%h/.cliproxy-rs
Restart=on-failure

[Install]
WantedBy=default.target
```

Then run `systemctl --user daemon-reload` and `systemctl --user enable --now cliproxy`.

Stop it with `systemctl --user disable --now cliproxy`. To run without a login session, enable lingering with `loginctl enable-linger` according to your host's policy.

## Running as a system service

For a per-user service, use the systemd user unit from [Start at login](#start-at-login). On a Linux server, a system unit like this runs it under a dedicated user:

```ini
# /etc/systemd/system/cliproxy.service
[Unit]
Description=cliproxy-rs
After=network-online.target
Wants=network-online.target

[Service]
User=cliproxy
WorkingDirectory=/var/lib/cliproxy
ExecStart=/usr/local/bin/cliproxy --config /var/lib/cliproxy/config.yaml
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/cliproxy

[Install]
WantedBy=multi-user.target
```

```sh
sudo useradd --system --home /var/lib/cliproxy --create-home cliproxy
sudo systemctl enable --now cliproxy
journalctl -u cliproxy -f
```

The server writes to `config.yaml` (when the dashboard saves settings or hashes a plaintext management key) and to the credential directory, so both must be writable by the service user.

