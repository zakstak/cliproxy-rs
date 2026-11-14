// End-to-end account sign-in through the dashboard. Disposable servers only.
//
// --mode fake: the server's login endpoints point at scripts/fake-logins.mjs (the
//   login-harness build of cliproxy-rs). Claude and Codex finish through a pasted callback
//   URL, Meta and xAI through the device-code flow; each new credential must appear in
//   the list.
// --mode dead: the shipped binary with requests.proxy-url on a dead port. A pasted callback
//   must end in an honest failure, never "Connected". No request reaches a provider.
//
// Usage: CHROME_PATH=... node scripts/oauth-check.mjs <page-url> --mode fake|dead [capture-dir]
import { chromium } from "playwright-core";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";
import assert from "node:assert/strict";
import { assertPrivateAuthDir } from "./private-auth-dir.mjs";

const [url, , mode, dir] = process.argv.slice(2);
assert.ok(url && ["fake", "dead"].includes(mode), "usage: oauth-check.mjs <page-url> --mode fake|dead [capture-dir]");
// Both modes sign in through a real server.
await assertPrivateAuthDir(url, "orb-dashboard-test-only");
const out = resolve(dir || "oauth-check");
mkdirSync(out, { recursive: true });
const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH });
const page = await (await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2 })).newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
const main = page.locator("main");
const button = (name) => main.getByRole("button", { name, exact: true });
const pass = (t) => console.log(`PASS ${t}`);
/** The toast, if any, must not still say a sign-in is in progress once it has finished. */
async function noProgressToast(when) {
  const toast = await page.locator(".toast").textContent({ timeout: 500 }).catch(() => "");
  assert.ok(!/sent|finishing|waiting/i.test(toast), `${when}: stale toast "${toast.trim()}"`);
}

/** Start a sign-in (or, without a provider, wait for one started elsewhere) and return the session state from the provider link. */
async function begin(provider) {
  if (provider) await button(provider).click();
  await main.getByText("Waiting for approval").waitFor();
  const href = await main.getByRole("link", { name: "Open sign-in page" }).getAttribute("href");
  return { href, state: new URL(href).searchParams.get("state") };
}
async function paste(redirect) {
  await main.getByText("The browser ended on a page that won’t load").click();
  await main.getByLabel("Paste that page’s full address").fill(redirect);
  await button("Send").click();
}

try {
  await page.goto(url);
  await page.getByLabel("Management key").fill("orb-dashboard-test-only");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await main.getByText("operator@example.invalid").first().waitFor();
  await page.evaluate(() => (location.hash = "#connect"));
  await main.getByRole("heading", { name: "Sign in with a provider" }).waitFor();

  if (mode === "fake") {
    for (const [provider, redirect, email] of [
      ["Claude", (s) => `http://localhost:54545/callback?code=fake-code&state=${s}`, "claude-login@example.invalid"],
      ["Codex", (s) => `http://localhost:1455/auth/callback?code=fake-code&state=${s}`, "codex-login@example.invalid"],
    ]) {
      const { href, state } = await begin(provider);
      assert.ok(href.startsWith("https://") && state, `${provider}: provider link ${href}`);
      await paste(redirect(state));
      await main.getByText("Connected", { exact: true }).waitFor({ timeout: 20_000 });
      await noProgressToast(`${provider} connected`);
      await page.screenshot({ path: `${out}/oauth-${provider.toLowerCase()}-connected.png` });
      pass(`${provider}: authorize link, pasted callback, code exchange, credential saved (${email})`);
    }
    for (const [provider, code] of [["Meta", "META-FAKE"], ["xAI", "XAI-FAKE"]]) {
      await button(provider).click();
      await main.getByText(code, { exact: true }).waitFor();
      if (provider === "Meta") await page.screenshot({ path: `${out}/oauth-meta-device-code.png` });
      await main.getByText("Connected", { exact: true }).waitFor({ timeout: 30_000 });
      pass(`${provider}: device code ${code} shown, polled to completion`);
    }
    await page.evaluate(() => (location.hash = "#accounts"));
    for (const email of ["claude-login@example.invalid", "codex-login@example.invalid", "meta-login@example.invalid", "xai-login@example.invalid"])
      await main.getByText(email).first().waitFor({ timeout: 15_000 });
    await page.screenshot({ path: `${out}/oauth-credentials-after.png`, fullPage: true });
    pass("all four new credentials are listed on Accounts");

    // Sign in again from Accounts: the account's own file, no new one; another account is refused.
    const files = async () => {
      const r = await fetch(new URL("v8/management/credentials", url), { headers: { Authorization: "Bearer orb-dashboard-test-only" } });
      return (await r.json()).files.map((f) => f.name).sort();
    };
    const again = async (email) => {
      await page.evaluate(() => (location.hash = "#accounts"));
      await main.getByRole("button", { name: email }).first().click();
      await button("Sign in again").click();
      await main.getByRole("heading", { name: `Claude · ${email}` }).waitFor();
      const { state } = await begin(null);
      await paste(`http://localhost:54545/callback?code=fake-code&state=${state}`);
    };
    const before = await files();
    await again("claude-login@example.invalid");
    await main.getByText("Signed in again", { exact: true }).waitFor({ timeout: 20_000 });
    await noProgressToast("Claude signed in again");
    assert.deepEqual(await files(), before);
    await page.screenshot({ path: `${out}/oauth-claude-signed-in-again.png` });
    pass("Sign in again on Accounts: targeted session finishes by its status, no new credential file");
    await again("operator@example.invalid");
    await main.getByText("Failed", { exact: true }).waitFor({ timeout: 20_000 });
    const refusal = (await main.locator(".flow .error").textContent()).trim();
    assert.equal(refusal, "Signed in to a different account than the one being signed in again; nothing was saved. Sign in to that account and try again.");
    assert.deepEqual(await files(), before);
    await page.screenshot({ path: `${out}/oauth-claude-wrong-account.png` });
    pass(`Sign in again as another account shows the server's refusal verbatim: "${refusal}"`);

  } else {
    const { state } = await begin("Codex");
    await paste(`http://localhost:1455/auth/callback?code=fake-code&state=${state}`);
    await main.getByText("Failed", { exact: true }).waitFor({ timeout: 30_000 });
    const problem = await main.locator(".flow .error").textContent();
    assert.ok(/exchange/i.test(problem), problem);
    await noProgressToast("Codex failed");
    await page.screenshot({ path: `${out}/oauth-codex-failed.png` });
    pass(`Codex with an unreachable token endpoint fails honestly: "${problem.trim()}"`);
    await button("Start again").click();
    await main.getByText("Waiting for approval").waitFor();
    await button("Cancel sign-in").click();
    await main.getByText("Cancelled", { exact: true }).waitFor();
    await noProgressToast("Codex cancelled");
    pass("Start again opens a new session; Cancel ends it; no in-progress toast remains after any end state");
  }
  assert.deepEqual(errors, []);
} finally {
  await browser.close();
}
