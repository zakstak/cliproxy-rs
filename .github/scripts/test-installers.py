#!/usr/bin/env python3
"""Exercise installers against private, local release archives, never user accounts."""
import hashlib
import http.server
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import ssl
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.request


def capture(command, *, timeout=120, **kwargs):
    # A child the server outlives can inherit a pipe and keep communicate() blocked
    # after the shell exits. A file keeps output without waiting for EOF. No fixture
    # reads stdin.
    kwargs.setdefault("stdin", subprocess.DEVNULL)
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace") as output:
        try:
            result = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT,
                                    timeout=timeout, **kwargs)
        except subprocess.TimeoutExpired:
            output.seek(0)
            print(output.read(), file=sys.stderr)
            raise
        output.seek(0)
        return subprocess.CompletedProcess(command, result.returncode, output.read())


def main():
    binary = Path(sys.argv[1]).resolve()
    repo = Path(__file__).resolve().parents[2]
    version = subprocess.check_output([binary, "--version"], text=True, timeout=20).splitlines()[-1].split()[-1]
    target = {
        ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
        ("Linux", "aarch64"): "aarch64-unknown-linux-gnu",
    }[(platform.system(), platform.machine())]
    exe = "cliproxy"
    requests = []

    with tempfile.TemporaryDirectory(prefix="installer-test-") as tmp:
        root = Path(tmp)
        releases = root / "releases"
        releases.mkdir()

        def package(tag, source):
            name = f"cliproxy-{tag.removeprefix('v')}-{target}"
            directory = root / name
            directory.mkdir()
            shutil.copy2(source, directory / exe)
            download = releases / "download" / tag
            download.mkdir(parents=True)
            archive = download / (name + ".tar.gz")
            with tarfile.open(archive, "w:gz") as t:
                t.add(directory, arcname=name)
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            (download / "SHA256SUMS").write_text(f"{digest}  {archive.name}\n")

        package(f"v{version}", binary)
        wrapper = root / "version-wrapper"
        wrapper.write_text(f'#!/bin/sh\nif [ "$1" = --version ]; then echo "cliproxy 999.1.0"; else exec {shlex.quote(str(binary))} "$@"; fi\n')
        wrapper.chmod(0o755)
        package("v999.1.0", wrapper)
        fixtures = repo / "target/installer-fixtures"
        hanging = fixtures / "hanging"
        package("v999.3.0", hanging)
        bad = root / "broken"
        bad.write_text("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'cliproxy 999.2.0'; else exit 1; fi\n")
        bad.chmod(0o755)
        package("v999.2.0", bad)

        class Releases(http.server.SimpleHTTPRequestHandler):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, directory=root, **kwargs)

            def do_HEAD(self):
                requests.append(self.path)
                if self.path == "/releases/latest":
                    self.send_response(302)
                    self.send_header("Location", f"/releases/tag/v{version}")
                    self.end_headers()
                elif self.path == f"/releases/tag/v{version}":
                    self.send_response(200)
                    self.end_headers()
                else:
                    super().do_HEAD()

            def do_GET(self):
                requests.append(self.path)
                super().do_GET()

            def log_message(self, *_):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Releases)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        home = root / "private home"
        bindir = root / "private bin"
        env = {k: v for k, v in os.environ.items() if k == "PATH"}
        env.update({"HOME": str(root),
               "CLIPROXY_RELEASES": f"http://127.0.0.1:{server.server_port}/releases",
               "CLIPROXY_HOME": str(home), "CLIPROXY_INSTALL_DIR": str(bindir),
               "CLIPROXY_NO_OPEN": "1", "CLIPROXY_NO_UPDATE_CHECK": "1"})
        # Catalog fetches cannot leave loopback even on native runners without namespaces.
        env.update(HTTP_PROXY="http://127.0.0.1:9", HTTPS_PROXY="http://127.0.0.1:9",
                   ALL_PROXY="http://127.0.0.1:9", NO_PROXY="127.0.0.1,localhost,::1")

        def install(*args, success=True):
            command = ["sh", str(repo / "install.sh"), *args]
            result = capture(command, env=env)
            assert (result.returncode == 0) == success, result.stdout
            return result.stdout

        def health():
            for _ in range(40):
                try:
                    with urllib.request.urlopen("http://127.0.0.1:8317/healthz", timeout=2) as r:
                        assert r.status == 200
                    return
                except OSError:
                    time.sleep(0.25)
            raise AssertionError("server not healthy")

        def pid():
            return int((home / "cliproxy.pid").read_text().strip())

        try:
            process_log = root / "process.log"
            process_log.write_text("keep existing log\n")
            failed = capture([binary, "--config", str(root / "missing-config.yaml"),
                                     "--log-file", process_log, "--local-model"], cwd=root, env=env,
                                    timeout=20)
            assert failed.returncode != 0
            logged = process_log.read_text()
            assert logged.startswith("keep existing log\n") and "config" in logged[len("keep existing log\n"):], logged
            # A process can exit between a liveness check and either signal.
            # An actual signal failure against a live PID must still fail.
            marker = root / "signal-test.pid"
            for phase in ("term", "kill"):
                for state in ("exited", "alive"):
                    marker.write_text("4242\n")
                    result = capture(["sh", "-c", '''
eval "$(sed '$d' "$1")"
pidfile=$2; phase=$3; expected=$4; killed=0
managed_pid() { pid=4242; return 0; }
pid_running() { [ "$killed" = 0 ] || [ "$expected" = alive ]; }
sleep() { :; }
kill() {
  if [ "$phase" = term ] || [ "$1" = -KILL ]; then killed=1; return 1; fi
  return 0
}
if stop_pidfile; then exit 0; else exit 1; fi
''', "signal-test", repo / "install.sh", marker, phase, state], env=env)
                    assert result.returncode == (0 if state == "exited" else 1), result.stdout
                    assert marker.exists() == (state == "alive")
            assert "not installed" in install("--check")
            assert not home.exists() and not bindir.exists()
            assert not any("/download/" in r for r in requests)
            # Check the bracketed IPv6 wildcard and YAML boolean aliases without
            # asking the server to bind a bracketed literal (not a bind address).
            probe_config = root / "probe-only.yaml"
            probe_config.write_text('server:\n  host: "[::]"\n  tls: # self-signed\n    enable: ON\n')
            code = (repo / "install.sh").read_text().removesuffix('main "$@"\n')
            base = capture(["sh", "-c", code + '\nconfig=$1; read_probe; printf "%s" "$base"', "sh", probe_config], env=env).stdout
            assert base.strip() == "https://[::1]:8317", base
            install()
            health()
            keys = dict(line.split("=", 1) for line in (home / "keys.env").read_text().splitlines() if line.startswith("CLIPROXY_"))
            # The dashboard page alone is not enough: its nested scripts, styles and
            # fonts once returned 404, leaving the page blank.
            with urllib.request.urlopen("http://127.0.0.1:8317/management.html", timeout=5) as response:
                assert (response.status, response.headers.get_content_type()) == (200, "text/html")
                page = response.read().decode()
            # ponytail: the page's own double-quoted src and href values only; follow url()
            # and import() in the fetched CSS and JS too if the build starts splitting chunks.
            referenced = re.findall(r'(?:src|href)="\./([^"]+)"', page)
            assert {".js", ".css", ".woff2"} <= {Path(r).suffix for r in referenced if "/" in r}, referenced
            types = {".js": "text/javascript", ".css": "text/css", ".woff2": "font/woff2", ".svg": "image/svg+xml"}
            failures = []
            for reference in referenced:
                try:
                    with urllib.request.urlopen("http://127.0.0.1:8317/" + reference, timeout=5) as response:
                        served = response.status, response.headers.get_content_type(), bool(response.read())
                except urllib.error.HTTPError as error:
                    served = error.code, None, False
                if served != (200, types.get(Path(reference).suffix), True):
                    failures.append((reference, served))
            assert not failures, failures
            for path, key, expected in [
                ("/v1/models", None, 401),
                ("/v1/models", keys["CLIPROXY_CLIENT_KEY"], 200),
                ("/v8/management/config", keys["CLIPROXY_MANAGEMENT_KEY"], 200),
                ("/assets/missing.js", None, 404),
            ]:
                request = urllib.request.Request("http://127.0.0.1:8317" + path)
                if key:
                    request.add_header("Authorization", "Bearer " + key)
                try:
                    with urllib.request.urlopen(request, timeout=5) as response:
                        code = response.status
                except urllib.error.HTTPError as error:
                    code = error.code
                assert code == expected, (path, code)
            assert (home / "keys.env").stat().st_mode & 0o777 == 0o600
            before = [(home / f).read_bytes() for f in ("config.yaml", "keys.env")]
            old_pid = pid()
            old_inode = (bindir / exe).stat().st_ino
            requests.clear()
            assert "already current" in install("--check")
            assert "already current" in install()
            assert pid() == old_pid
            assert (bindir / exe).stat().st_ino == old_inode
            assert not any("/download/" in r for r in requests)
            env["CLIPROXY_VERSION"] = "v999.1.0"
            install("--binary-only")
            health()
            assert pid() == old_pid, "binary-only must leave the old server running"
            assert (bindir / "cliproxy.prev").stat().st_ino == old_inode
            requests.clear()
            install()
            health()
            assert pid() != old_pid
            assert [(home / f).read_bytes() for f in ("config.yaml", "keys.env")] == before
            assert not any("/download/" in r for r in requests), "current on-disk upgrade must restart without downloading"
            restarted = pid()
            assert "already current" in install()
            assert pid() == restarted
            subprocess.run(["kill", str(restarted)], check=True, timeout=10)
            time.sleep(2)
            install()
            health()
            assert pid() != restarted, "a stopped current binary must start again"
            # The installer uses procfs, never procps: a refusing ps changes nothing.
            shimdir = root / "no-ps"
            shimdir.mkdir()
            original_path = env["PATH"]
            env["PATH"] = str(shimdir) + os.pathsep + original_path
            try:
                for refused in (1, 126, 127):
                    (shimdir / "ps").write_text(f"#!/bin/sh\nexit {refused}\n")
                    (shimdir / "ps").chmod(0o755)
                    previous_pid = pid()
                    os.utime(bindir / exe, None)
                    install()
                    health()
                    assert pid() != previous_pid, f"ps status {refused} must not skip stopping the old server"
                    subprocess.run(["kill", str(pid())], check=True, timeout=10)
                    time.sleep(2)
                    unrelated = subprocess.Popen(["sleep", "120"], env=env)
                    try:
                        (home / "cliproxy.pid").write_text(str(unrelated.pid))
                        os.utime(bindir / exe, None)
                        install()
                        health()
                        assert unrelated.poll() is None, f"ps status {refused} on {platform.system()} must never signal an unrelated stale PID"
                    finally:
                        unrelated.terminate()
                        unrelated.wait(timeout=10)
            finally:
                env["PATH"] = original_path
            second = root / "second"
            env.update(CLIPROXY_HOME=str(second))
            try:
                install()
                assert "CLIPROXY_PORT=8318" in (second / "keys.env").read_text()
                with urllib.request.urlopen("http://127.0.0.1:8318/healthz", timeout=5) as response:
                    assert response.status == 200
                second_pid = int((second / "cliproxy.pid").read_text().strip())
                env.update(CLIPROXY_HOME=str(home))
                install()
                health()
                with urllib.request.urlopen("http://127.0.0.1:8318/healthz", timeout=5) as response:
                    assert response.status == 200, "upgrading one config must not stop another config sharing the binary"
                health()
            finally:
                try:
                    if (second / "cliproxy.pid").exists():
                        second_pid = int((second / "cliproxy.pid").read_text().strip())
                        subprocess.run(["kill", str(second_pid)], check=False, timeout=10)
                except (OSError, ValueError, subprocess.SubprocessError) as error:
                    print(f"Best-effort cleanup: {error}", file=sys.stderr)
                env.update(CLIPROXY_HOME=str(home))
            # A service command fails once after the swap; rollback must restart and
            # verify the previous image rather than losing it through `set -e`.
            mockbin = root / "service-mocks"
            mockbin.mkdir()
            shim = mockbin / "systemctl"
            shim.write_text('''#!/bin/sh
case "$*" in
  *show-environment*|*daemon-reload*|*enable*) exit 0 ;;
  *is-active*)
    if [ -f "$CLIPROXY_HOME/service.pid" ] && kill -0 "$(cat "$CLIPROXY_HOME/service.pid")" 2>/dev/null; then
      exit 0
    fi
    exit 1 ;;
  *restart*)
    if [ -f "$CLIPROXY_HOME/fail-service-once" ]; then
      rm "$CLIPROXY_HOME/fail-service-once"; echo 'injected service failure' >&2; exit 1
    fi
    (cd "$CLIPROXY_HOME" && exec nohup "$CLIPROXY_INSTALL_DIR/cliproxy" --config "$CLIPROXY_HOME/config.yaml" </dev/null >>"$CLIPROXY_HOME/cliproxy.log" 2>&1) &
    echo $! > "$CLIPROXY_HOME/service.pid" ;;
  *) exit 1 ;;
esac
''')
            shim.chmod(0o755)
            (home / "fail-service-once").touch()
            original_path = env["PATH"]
            env["PATH"] = str(mockbin) + os.pathsep + original_path
            env["CLIPROXY_VERSION"] = f"v{version}"
            image = (bindir / exe).read_bytes()
            try:
                output = install("--service", success=False)
                assert "injected service failure" in output and "verified it is healthy" in output, output
                assert (bindir / exe).read_bytes() == image
                health()
            finally:
                marker = home / "service.pid"
                if marker.exists():
                    (home / "cliproxy.pid").write_text(marker.read_text())
                unit = root / ".config/systemd/user/cliproxy.service"
                unit.unlink(missing_ok=True)
                env["PATH"] = original_path
            env["CLIPROXY_VERSION"] = "v999.3.0"
            image = (bindir / exe).read_bytes()
            output = install(success=False)
            assert "verified it is healthy" in output, output
            assert "sending SIGKILL" in output, "a hung process ignoring SIGTERM must be killed before restarting"
            assert (bindir / exe).read_bytes() == image, "unhealthy live upgrade must restore the old image"
            health()
            env["CLIPROXY_VERSION"] = "v999.2.0"
            inode = (bindir / exe).stat().st_ino
            assert "restored the previous binary" in install(success=False)
            assert (bindir / exe).stat().st_ino == inode
            health()
            # A checksum failure must not replace a healthy image or restart it.
            env["CLIPROXY_VERSION"] = f"v{version}"
            sums = releases / f"download/v{version}/SHA256SUMS"
            sums.write_text(sums.read_text().replace(sums.read_text()[:64], "0" * 64))
            old_pid = pid()
            assert "checksum mismatch" in install(success=False)
            assert pid() == old_pid and (bindir / exe).stat().st_ino == inode
            health()
            # Non-default bind host: derive both the health probe and dashboard URL.
            config = home / "config.yaml"
            config.write_text(config.read_text().replace('host: "127.0.0.1"', 'host: "::1"'))
            env["CLIPROXY_VERSION"] = f"v{version}"
            # Make the on-disk current file newer than its last-start marker.
            env["CLIPROXY_VERSION"] = "v999.1.0"
            os.utime(bindir / exe, None)
            assert "http://[::1]:8317/management.html" in install()
            connection = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with connection.open("http://[::1]:8317/healthz", timeout=5) as response:
                assert response.status == 200
            config.write_text(config.read_text().replace('host: "::1"', 'host: "::"'))
            os.utime(bindir / exe, None)
            assert "http://[::1]:8317/management.html" in install()
            with connection.open("http://[::1]:8317/healthz", timeout=5) as response:
                assert response.status == 200
            cert = root / "fixture-cert.pem"
            key = root / "fixture-key.pem"
            subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                            "-subj", "/CN=localhost", "-keyout", key, "-out", cert],
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
            plain = config.read_text()
            for mapping, enabled in [("tls: # self-signed", "true"), ("tls:", "yes")]:
                config.write_text(plain.replace("  port: 8317", f"  port: 8317\n  {mapping}\n    enable: {enabled}\n    cert: '{cert}'\n    key: '{key}'"))
                os.utime(bindir / exe, None)
                assert "https://[::1]:8317/management.html" in install()
                connection = urllib.request.build_opener(urllib.request.ProxyHandler({}),
                              urllib.request.HTTPSHandler(context=ssl._create_unverified_context()))
                with connection.open("https://[::1]:8317/healthz", timeout=5) as response:
                    assert response.status == 200
            print(f"PASS {platform.system()}: install, upgrade, binary-only, unchanged keys" +
                  ", check, current no-op, hard link, rollback, checksum rejection")
        except BaseException:
            def diagnostic(action):
                try:
                    action()
                except (OSError, subprocess.SubprocessError) as error:
                    print(f"Best-effort diagnostics: {error}", file=sys.stderr)

            for folder in (home, root / "second"):
                def logs():
                    print(f"Diagnostics: {folder}", file=sys.stderr)
                    logfile = folder / "cliproxy.log"
                    if logfile.exists():
                        print("\n".join(logfile.read_text(errors="replace").splitlines()[-40:]), file=sys.stderr)
                diagnostic(logs)
                def process():
                    marker = folder / "cliproxy.pid"
                    if marker.exists():
                        recorded_pid = marker.read_text().strip()
                        print(f"pid file: {recorded_pid}", file=sys.stderr)
                        # Observe shell exit statuses too: a direct Python exec
                        # reports EPERM, not the status the installer sees.
                        status = capture(["sh", "-c", '''
kill -0 "$1"; echo "kill -0 status=$?"
name=$(ps -p "$1" -o "$2="); rc=$?; printf 'name status=%s output=<%s>\n' "$rc" "$name"
state=$(ps -p "$1" -o stat=); rc=$?; printf 'stat status=%s output=<%s>\n' "$rc" "$state"
eval "$(sed '$d' "$3")"
pidfile=$4; os=$(uname -s)
if managed_pid && pid_running "$pid"; then
  echo 'installer PID helpers: PASS'
else
  rc=$?; printf 'installer PID helpers: FAIL status=%s pid=<%s> ps_status=<%s>\n' "$rc" "${pid-unset}" "${status-unset}"
fi
''', "diagnostics", recorded_pid, "comm", repo / "install.sh", marker],
                                         env=env, timeout=10)
                        print(status.stdout, file=sys.stderr)
                        subprocess.run(["ps", "-p", recorded_pid, "-o", "pid=,ppid=,stat=,comm=,args="], check=False, timeout=10)
                diagnostic(process)
            if shutil.which("lsof"):
                diagnostic(lambda: subprocess.run(["lsof", "-nP", "-iTCP:8317", "-sTCP:LISTEN"], check=False, timeout=10))
            if shutil.which("ss"):
                diagnostic(lambda: subprocess.run(["ss", "-ltnp"], check=False, timeout=10))
            raise
        finally:
            try:
                if (home / "cliproxy.pid").exists():
                    subprocess.run(["kill", str(pid())], check=False, timeout=10)
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                print(f"Best-effort cleanup: {error}", file=sys.stderr)
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
