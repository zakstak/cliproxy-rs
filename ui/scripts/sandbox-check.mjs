// Start only disposable servers with private auth directories. Run through isolated.sh
// to deny external network while the browser and local peers share loopback.
// node scripts/sandbox-check.mjs /absolute/path/to/cliproxy [capture-directory]
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
const oauth = process.argv.includes("--oauth");
const binary = resolve(process.argv[2] || "../target/debug/cliproxy");
const dir = mkdtempSync(join(tmpdir(), "cliproxy-ledger-"));
mkdirSync(join(dir, "auth"));
writeFileSync(join(dir, "auth/claude-fixture.json"), JSON.stringify({ type: "claude", access_token: "fake-disabled-token", email: "operator@example.invalid", disabled: true }));
writeFileSync(join(dir, "config.yaml"), `config-version: 8
server:
  host: '127.0.0.1'
  port: 8347
management:
  secret-key: orb-dashboard-test-only
oauth:
  auth-dir: '${dir}/auth'
access:
  api-keys: [orb-client-test-only]
routing:
  strategy: round-robin
  retry:
    request-retry: 3
api-keys:
  claude:
    - name: Local test gateway
      base-url: http://127.0.0.1:9101
      models:
        - name: claude-sonnet-4-5
          alias: ledger-fixture
      keys:
        - api-key: sk-fake-provider-0001
`);
const upstream = createServer(async (req, res) => {
  for await (const _ of req);
  res.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({
    id: "msg_local_fixture", type: "message", role: "assistant", model: "claude-sonnet-4-5",
    content: [{ type: "text", text: "local fixture reply" }], stop_reason: "end_turn", stop_sequence: null,
    usage: { input_tokens: 8, output_tokens: 3 },
  }));
});
await new Promise(resolve => upstream.listen(9101, "127.0.0.1", resolve));
const fakeLogin = oauth ? spawn(process.execPath, [fileURLToPath(new URL("./fake-logins.mjs", import.meta.url)), "9102"], { stdio: "ignore" }) : null;
const server = spawn(binary, oauth ? [join(dir, "config.yaml"), "http://127.0.0.1:9102"] : ["--config", join(dir, "config.yaml")], { cwd: dir, stdio: ["ignore", "pipe", "pipe"] });
let output = "";
server.stdout.on("data", b => { output += b; });
server.stderr.on("data", b => { output += b; });
try {
  let ready = false;
  for (let i = 0; i < 100; i++) {
    if (server.exitCode !== null) throw new Error(`Server exited: ${output}`);
    ready = await fetch("http://127.0.0.1:8347/healthz").then(r => r.ok, () => false);
    if (ready) break;
    await new Promise(r => setTimeout(r, 100));
  }
  assert.ok(ready, output);
  const captures = process.argv[3] || join(dir, "captures");
  const args = oauth
    ? [fileURLToPath(new URL("./oauth-check.mjs", import.meta.url)), "http://127.0.0.1:8347/management.html", "--mode", "fake", captures]
    : [fileURLToPath(new URL("./browser-check-rust.mjs", import.meta.url)), "http://127.0.0.1:8347/management.html", captures];
  const child = spawn(process.execPath, args, { stdio: "inherit", env: process.env });
  assert.equal(await new Promise(r => child.on("exit", r)), 0, "browser verification failed");
  console.log(`Disposable fixture: ${dir}`);
} finally {
  server.kill("SIGTERM");
  upstream.close();
  fakeLogin?.kill("SIGTERM");
}
