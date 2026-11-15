// Ledger account sign-in through a real disposable Rust server and local fake providers.
// CHROME_PATH=... node scripts/oauth-check.mjs <page-url> --mode fake|dead [capture-dir]
import { chromium } from "playwright-core";
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";
import { assertPrivateAuthDir } from "./private-auth-dir.mjs";
const [url, flag, mode, dir] = process.argv.slice(2);
assert.ok(url && flag === "--mode" && ["fake", "dead"].includes(mode), "usage: oauth-check.mjs <url> --mode fake|dead [captures]");
const key = "orb-dashboard-test-only";
await assertPrivateAuthDir(url, key);
const out = resolve(dir || "/tmp/ledger-oauth"); mkdirSync(out, { recursive: true });
const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH });
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
const errors = []; page.on("pageerror", e => errors.push(String(e)));
const dialog = page.getByRole("dialog");
async function begin(provider) {
  if (provider) {
    await page.evaluate(() => { location.hash = "accounts?add=1"; });
    await dialog.getByLabel(provider, { exact: true }).check();
    await dialog.getByRole("button", { name: "Continue", exact: true }).click();
  }
  await dialog.locator("strong").filter({ hasText: /Waiting for you to approve/ }).waitFor();
  const href = await dialog.getByRole("link", { name: "Open sign-in page", exact: true }).getAttribute("href");
  assert.ok(/^https?:/.test(href), "real provider link remains operable");
  return new URL(href).searchParams.get("state");
}
async function paste(provider, state) {
  await dialog.getByText("The browser ended on a page that won't load", { exact: true }).click();
  await dialog.getByLabel("Paste that page's full address").fill(provider === "Claude"
    ? `http://localhost:54545/callback?code=fake-code&state=${state}`
    : `http://localhost:1455/auth/callback?code=fake-code&state=${state}`);
  await dialog.getByRole("button", { name: "Send address", exact: true }).click();
}
async function done(provider) {
  await dialog.locator("strong").filter({ hasText: new RegExp(`${provider} account connected`) }).waitFor({ timeout: 30_000 });
  await page.screenshot({ path: `${out}/${provider.toLowerCase()}-connected.png` });
  await dialog.getByRole("button", { name: "Done", exact: true }).click();
}
try {
  await page.goto(url);
  await page.getByLabel("Management key").fill(key);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.locator("#main").waitFor();
  if (mode === "fake") {
    for (const provider of ["Claude", "Codex"]) {
      const state = await begin(provider); assert.ok(state);
      await paste(provider, state); await done(provider);
      console.log(`PASS ${provider} real callback, exchange and credential save through local peer`);
    }
    for (const [provider, code] of [["Meta", "META-FAKE"], ["xAI", "XAI-FAKE"]]) {
      await begin(provider);
      await dialog.getByText(code, { exact: true }).waitFor();
      await done(provider);
      console.log(`PASS ${provider} real device flow through local peer`);
    }
    await page.evaluate(() => { location.hash = "accounts"; });
    const endpoint = new URL("v8/management/credentials", new URL(".", url));
    const files = async () => (await (await fetch(endpoint, { headers: { Authorization: `Bearer ${key}` } })).json()).files.map(f => f.name).sort();
    const before = await files();
    for (const email of ["claude-login@example.invalid", "operator@example.invalid"]) {
      await page.locator("#main").getByRole("button", { name: email, exact: true }).click();
      await page.getByRole("button", { name: "Sign in again", exact: true }).click();
      const state = await begin(); await paste("Claude", state);
      if (email.startsWith("claude-login")) {
        await dialog.locator("strong").filter({ hasText: /^Signed in again/ }).waitFor({ timeout: 30_000 });
        await dialog.getByRole("button", { name: "Done", exact: true }).click();
      } else {
        await dialog.locator("[data-step-lead]").filter({ hasText: /different account than/ }).waitFor({ timeout: 30_000 });
        await dialog.getByRole("button", { name: "Close", exact: true }).first().click();
      }
      assert.deepEqual(await files(), before, "targeted login never creates a second file");
      const closeDetail = page.getByRole("button", { name: "Close", exact: true });
      if (await closeDetail.count()) await closeDetail.first().click();
    }
    console.log("PASS targeted reauthentication and wrong-account refusal");
  } else {
    const state = await begin("Codex"); await paste("Codex", state);
    await dialog.getByText(/No account was added/).waitFor({ timeout: 40_000 });
    assert.match(await dialog.innerText(), /exchange|connect|request failed/i);
    await dialog.getByRole("button", { name: "Start again", exact: true }).click();
    await dialog.locator("strong").filter({ hasText: /Waiting for you to approve/ }).waitFor();
    await dialog.getByRole("button", { name: "Cancel sign-in", exact: true }).click();
    assert.equal(await dialog.count(), 0);
    console.log("PASS failed exchange, fresh session retry and server-confirmed cancellation");
  }
  assert.deepEqual(errors, []);
} catch (e) {
  await page.screenshot({ path: `${out}/failure.png`, fullPage: true }).catch(() => {});
  console.error((await page.locator("body").innerText()).slice(-4000));
  throw e;
} finally { await browser.close(); }
