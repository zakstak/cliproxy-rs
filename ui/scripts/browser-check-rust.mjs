// Primary Ledger dashboard against an actual disposable Rust server. Never a live proxy.
// CHROME_PATH=/path/to/chrome node scripts/browser-check-rust.mjs <page-url> [capture-dir]
import { chromium } from "playwright-core";
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";
import { assertPrivateAuthDir } from "./private-auth-dir.mjs";

const url = process.argv[2];
assert.ok(url, "A disposable dashboard URL is required");
const key = "orb-dashboard-test-only";
await assertPrivateAuthDir(url, key);
const out = resolve(process.argv[3] || "/tmp/cliproxy-ledger-browser");
mkdirSync(out, { recursive: true });
const endpoint = new URL("v8/management", new URL(".", url)).href;
async function api(path, method = "GET", body) {
  const r = await fetch(endpoint + path, { method, headers: { Authorization: `Bearer ${key}`, "Content-Type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
  assert.equal(r.ok, true, `${method} ${path}: ${await (!r.ok ? r.text() : Promise.resolve(""))}`);
  return r.json();
}
const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH });
const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
const page = await context.newPage();
const errors = [], writes = [];
page.on("pageerror", e => errors.push(String(e)));
page.on("request", r => { if (r.method() === "PUT") writes.push({ url: r.url(), etag: r.headers()["if-match"] }); });
const route = async hash => {
  await page.evaluate(hash => { location.hash = hash; }, hash);
  await page.waitForFunction(hash => location.hash === `#${hash}`, hash);
  await page.evaluate(() => document.fonts.ready.then(() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))));
};
const main = page.locator("#main");
try {
  await page.goto(url);
  await page.getByLabel("Management key").fill("wrong-test-key");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.getByText(/That key is not right/).waitFor();
  await page.getByLabel("Management key").fill(key);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await main.getByRole("heading", { name: "Activity", exact: true }).waitFor();
  await route("accounts");
  await main.getByText("operator@example.invalid", { exact: true }).first().waitFor();
  assert.equal(await page.getByText(/Prototype|Simulate approval|Review controls/).count(), 0);
  await route("logs");
  assert.equal(await main.getByText("No Claude account can serve right now", { exact: true }).count(), 0,
    "a configured key with unknown health must not produce a provider outage");
  await main.getByText(/1 configured key with unknown health/).waitFor();

  for (const width of [1440, 768, 390, 320]) {
    await page.setViewportSize({ width, height: width > 1000 ? 900 : 844 });
    for (const theme of ["dark", "light"]) {
      await route("settings");
      await main.getByRole("button", { name: theme === "dark" ? "Dark" : "Light", exact: true }).click();
      for (const hash of ["logs", "logs/server", "usage", "accounts", "tools", "settings", "settings/keys", "settings/models", "settings/payload", "settings/config", "settings/system"]) {
        await route(hash);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, `${width} ${theme} ${hash}: horizontal overflow`);
        assert.equal(await main.locator('[role="alert"]').count(), 0, `${width} ${theme} ${hash}: ${await main.innerText()}`);
      }
      await route("logs");
      await page.screenshot({ path: `${out}/ledger-${width}-${theme}.png`, fullPage: true });
    }
  }
  console.log("PASS all primary screens, 320/390/768/1440, light and dark, real server");

  await page.setViewportSize({ width: 1440, height: 900 });
  await route("tools");
  await Promise.all([
    page.waitForResponse(r => r.request().method() === "PUT" && r.url().endsWith("/config/access/api-keys") && r.ok()),
    main.getByRole("button", { name: "Create another key", exact: true }).click(),
  ]);
  await page.waitForFunction(() => document.querySelector('#client-key')?.options.length >= 2);
  await page.waitForFunction(() => document.querySelector('#main [role="status"]')?.textContent.includes("proxy accepts"));
  assert.ok((await api("/config/access/api-keys")).length >= 2, "generated client key saved on server");
  assert.equal(await main.getByLabel("Proxy address").inputValue(), new URL(".", url).href.replace(/\/$/, ""));
  console.log("PASS client-key creation and real public model-list check");
  const response = await fetch(new URL("v1/messages", new URL(".", url)), {
    method: "POST", headers: { Authorization: "Bearer orb-client-test-only", "Content-Type": "application/json", "anthropic-version": "2023-06-01" },
    body: JSON.stringify({ model: "ledger-fixture", max_tokens: 16, messages: [{ role: "user", content: "local fixture only" }] }),
  });
  assert.equal(response.status, 200, await response.text());
  await route("logs");
  await main.getByRole("row").filter({ hasText: /ledger-fixture|claude-sonnet-4-5/ }).first().waitFor({ timeout: 15_000 });
  console.log("PASS request through local mock upstream appears in real live Activity");

  await route("settings/keys?provider=codex");
  const addGroup = main.locator("form.pk-add");
  await addGroup.getByLabel("Group name").fill("Disposable Codex group");
  await addGroup.getByLabel("API key", { exact: true }).fill("sk-fake-codex-test-0002");
  await Promise.all([
    page.waitForResponse(r => r.request().method() === "PUT" && r.url().endsWith("/config/api-keys/codex") && r.ok()),
    addGroup.getByRole("button", { name: "Add group", exact: true }).click(),
  ]);
  const codexGroups = await api("/config/api-keys/codex");
  assert.equal(codexGroups[0]["base-url"], "https://api.openai.com/v1");
  assert.ok(codexGroups[0].keys[0].auth_index, "real server assigns the configured key's identity");
  await main.getByRole("button", { name: "Delete group… Disposable Codex group", exact: true }).click();
  await Promise.all([
    page.waitForResponse(r => r.request().method() === "PUT" && r.url().endsWith("/config/api-keys/codex") && r.ok()),
    main.getByRole("button", { name: "Delete group Disposable Codex group", exact: true }).click(),
  ]);
  assert.deepEqual(await api("/config/api-keys/codex"), []);
  assert.equal((await api("/config/api-keys/claude")).length, 1, "removing a group preserves other providers");
  console.log("PASS provider-key creation, server identity and exact group removal");

  // A PUT that landed but lost its response must retry the frozen append, not append again.
  await route("accounts");
  await main.getByRole("button", { name: "Add account", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByRole("radio", { name: "Codex", exact: true }).check();
  await dialog.getByRole("radio", { name: /^Use a Codex API key/ }).check();
  await dialog.getByRole("button", { name: "Continue", exact: true }).click();
  await dialog.getByRole("textbox", { name: "Name", exact: true }).fill("Lost-response test");
  await dialog.getByLabel("Codex API key", { exact: true }).fill("sk-fake-codex-lost-response");
  let keyPuts = 0;
  const dropAnswer = async intercepted => {
    if (intercepted.request().method() !== "PUT") return intercepted.continue();
    keyPuts++;
    if (keyPuts !== 1) return intercepted.continue();
    const answer = await intercepted.fetch();
    assert.ok(answer.ok());
    await intercepted.abort("connectionfailed");
  };
  await page.route("**/config/api-keys/codex", dropAnswer);
  await dialog.getByRole("button", { name: "Add API key", exact: true }).click();
  await dialog.getByText(/Could not confirm the key was added/).waitFor();
  assert.equal((await api("/config/api-keys/codex")).length, 1);
  await dialog.getByRole("button", { name: "Try again", exact: true }).click();
  await dialog.waitFor({ state: "hidden" });
  assert.equal(keyPuts, 1, "retry recognizes the first write and sends no second PUT");
  assert.equal((await api("/config/api-keys/codex")).length, 1, "retry does not duplicate the key group");
  await page.unroute("**/config/api-keys/codex", dropAnswer);
  await page.keyboard.press("Escape");
  console.log("PASS Add account retries an unanswered successful key write without duplication");

  await route("settings");
  const strategy = main.getByLabel("Which account takes the next request");
  const initial = await strategy.inputValue();
  const next = initial === "fill-first" ? "round-robin" : "fill-first";
  await Promise.all([
    page.waitForResponse(r => r.request().method() === "PUT" && r.url().endsWith("/config/routing/strategy") && r.ok()),
    strategy.selectOption(next),
  ]);
  await page.waitForFunction(() => !document.querySelector('[aria-disabled="true"]'));
  assert.equal(await api("/config/routing/strategy"), next);
  assert.ok(writes.some(w => w.url.endsWith("/config/routing/strategy") && w.etag), "real If-Match sent on guided write");
  await Promise.all([
    page.waitForResponse(r => r.request().method() === "PUT" && r.url().endsWith("/config/routing/strategy") && r.ok()),
    strategy.selectOption(initial),
  ]);
  await page.waitForFunction(() => !document.querySelector('[aria-disabled="true"]'));
  console.log("PASS guided config write with If-Match and readback");

  await route("settings/config");
  const yamlSection = main.locator("section.group").first();
  await yamlSection.getByRole("button", { name: /^Edit config.yaml$/ }).click();
  const textarea = yamlSection.getByRole("textbox");
  const original = await textarea.inputValue();
  await textarea.fill(original.replace("request-retry: 3", "request-retry: 4"));
  await main.getByRole("button", { name: "Review changes", exact: true }).click();
  await Promise.all([
    page.waitForResponse(r => r.request().method() === "PUT" && r.url().endsWith("/config.yaml") && r.ok()),
    main.getByRole("button", { name: /^Write/ }).click(),
  ]);
  await yamlSection.getByRole("button", { name: /^Edit config.yaml$/ }).waitFor();
  assert.equal(await api("/config/routing/retry/request-retry"), 4);
  assert.ok(writes.some(w => w.url.endsWith("/config.yaml") && w.etag));
  await yamlSection.getByRole("button", { name: /^Edit config.yaml$/ }).click();
  await textarea.fill((await textarea.inputValue()).replace("request-retry: 4", "request-retry: 5"));
  await api("/config/routing/retry/request-retry", "PUT", 6);
  await main.getByRole("button", { name: "Review changes", exact: true }).click();
  await main.getByRole("button", { name: /^Write/ }).click();
  await main.getByText(/changed on the server/).first().waitFor();
  assert.equal(await api("/config/routing/retry/request-retry"), 6, "concurrent change retained");
  await route("accounts");
  await page.getByRole("button", { name: "Sign out", exact: true }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Keep editing", exact: true }).click();
  await main.getByRole("button", { name: /Discard.*draft|Discard my draft/ }).click();
  await main.getByRole("button", { name: "Discard", exact: true }).click();
  console.log("PASS actual YAML parsing, config conflict, navigation draft and sign-out guard");

  await route("accounts");
  const account = main.locator("button[data-acct]").filter({ hasText: "operator@example.invalid" }).first();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.evaluate(() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))));
  await account.click();
  await page.getByRole("dialog").waitFor();
  await page.keyboard.press("Tab");
  await page.keyboard.press("Shift+Tab");
  await page.keyboard.press("Escape");
  assert.equal(await page.getByRole("dialog").count(), 0);
  assert.match(await page.evaluate(() => document.activeElement?.textContent.trim()), /operator@example.invalid/);
  assert.equal(await page.evaluate(k => JSON.stringify([localStorage, sessionStorage]).includes(k), key), false);
  // Synthetic history stress at the HTTP boundary; the rest of this suite uses the real server.
  // One read returns more rows than the tab keeps (2,000), so the oldest become unknown.
  let supplied = false;
  await page.route("**/history/requests?*", async intercepted => {
    const now = Date.now();
    const requests = supplied ? [] : Array.from({ length: 2101 }, (_, n) => ({
      id: 1_000_000 + n, at: now - 2201 + n + 1, latency_ms: 1,
      request_id: `failed-stream-${n}`, model: "reported-failure", provider: "claude", failed: true, status: 529,
      usage_reported: true, input_tokens: 9, output_tokens: 6, timings: {},
    }));
    supplied = true;
    await intercepted.fulfill({ json: { requests, more: false } });
  });
  await page.setViewportSize({ width: 1440, height: 900 });
  await route("logs");
  await main.getByText("Older requests in this window are not loaded here.", { exact: false }).waitFor();
  await main.getByText("18K", { exact: true }).first().waitFor();
  await main.getByText("12K", { exact: true }).first().waitFor();
  await main.getByRole("button", { name: /^Trends/ }).click();
  assert.ok(await main.locator(".trend .unknown").count() >= 4);
  const chart = main.getByRole("group", { name: /^Input tokens chart:/ });
  await chart.focus();
  await page.keyboard.press("Home");
  await main.locator(".trend").filter({ hasText: "Input tokens" }).getByText("Unknown", { exact: true }).waitFor();
  await page.screenshot({ path: `${out}/ledger-retention-failed-streams.png`, fullPage: true });
  console.log("PASS >2000 history rows in a tab leave older time visibly unknown; failed-stream usage totals remain 18K/12K");
  assert.ok(writes.length >= 3);
  assert.deepEqual(errors, []);
  console.log("PASS keyboard detail dismissal and focus return; no management key in storage");
} catch (e) {
  await page.screenshot({ path: `${out}/failure.png`, fullPage: true }).catch(() => {});
  console.error((await page.locator("body").innerText()).slice(-6000));
  throw e;
} finally {
  await browser.close();
}
