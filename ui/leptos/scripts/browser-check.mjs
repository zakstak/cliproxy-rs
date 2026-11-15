import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright-core";

// This harness starts its own fixture server and a temporary browser profile.
// It accepts no backend URL and cannot be pointed at the production proxy.
const root = fileURLToPath(new URL("../", import.meta.url));
const captures = resolve(process.argv[2] ?? "/tmp/ledger-leptos-captures");
assert(process.env.CHROME_PATH, "Set CHROME_PATH to a Chromium executable.");
await mkdir(captures, { recursive: true });
const server = spawn(resolve(root, "target/release/ledger-sandbox"), ["0"], {
  stdio: ["ignore", "pipe", "inherit"],
});
let browser;
try {
  const base = await new Promise((accept, reject) => {
    const timer = setTimeout(
      () => reject(new Error("Sandbox did not start.")),
      20_000,
    );
    server.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    server.once("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`Sandbox exited: ${code}`));
    });
    server.stdout.on("data", (data) => {
      const match = data.toString().match(/http:\/\/127\.0\.0\.1:\d+/);
      if (match) {
        clearTimeout(timer);
        accept(match[0]);
      }
    });
  });
  browser = await chromium.launch({
    executablePath: process.env.CHROME_PATH,
    headless: true,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });
  const context = await browser.newContext({
    viewport: { width: 1280, height: 900 },
  });
  const failures = [];
  const external = [];
  await context.route("**/*", (route) => {
    if (new URL(route.request().url()).origin !== base) {
      external.push(route.request().url());
      return route.abort();
    }
    return route.continue();
  });
  const page = await context.newPage();
  page.on("pageerror", (error) => failures.push(error.message));
  await page.goto(base);
  await page.getByRole("heading", { name: /^Requests \d+$/ }).waitFor();
  assert((await page.locator("tbody tr").count()) > 0);
  await page
    .getByRole("button", { name: "Pause watching", exact: true })
    .click();
  await page.getByRole("searchbox", { name: "Search requests" }).fill("Claude");
  for (const row of await page.locator("tbody tr").allTextContents())
    assert(row.includes("Claude"));
  await page.getByRole("checkbox", { name: "Needs attention" }).check();
  for (const badge of await page.locator("tbody .badge").allTextContents())
    assert.notEqual(badge, "Completed");
  const row = page.locator("tbody .row-link").first();
  const rowId = await row.getAttribute("id");
  await row.focus();
  await page.keyboard.press("Enter");
  await page.waitForFunction(
    () => document.activeElement.id === "detail-heading",
  );
  await page.keyboard.press("Escape");
  await page.waitForFunction((id) => document.activeElement.id === id, rowId);
  await page.getByRole("searchbox").fill("");
  await page.getByRole("checkbox", { name: "Needs attention" }).uncheck();
  await page.screenshot({ path: resolve(captures, "activity-dark.png") });
  await page.getByText("Try a failure scenario", { exact: true }).click();
  const retained = await page.locator("tbody tr").count();
  await page
    .getByRole("button", { name: "Fail the next Activity read" })
    .click();
  await page.getByText("Activity is unavailable", { exact: true }).waitFor();
  assert.equal(await page.locator("tbody tr").count(), retained);
  await page.getByRole("button", { name: "Retry", exact: true }).click();
  await page.locator(".notice.error").waitFor({ state: "detached" });
  await page
    .getByRole("button", { name: "Resume watching", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Pause watching", exact: true })
    .click();

  await page
    .getByRole("button", { name: "Configuration", exact: true })
    .click();
  const textarea = page.getByRole("textbox", { name: "YAML configuration" });
  await textarea.fill("routing: [");
  await page
    .getByRole("button", { name: "Review changes", exact: true })
    .click();
  assert.equal(await textarea.getAttribute("aria-invalid"), "true");
  const draft =
    "# Retain this comment exactly\nrouting:\n  strategy: round-robin\n  retry:\n    request-retry: 5\n";
  await textarea.fill(draft);
  await page.getByRole("button", { name: "Activity", exact: true }).click();
  await page.getByRole("button", { name: /^Configuration/ }).click();
  assert.equal(await textarea.inputValue(), draft);
  await page
    .getByRole("button", { name: "Review changes", exact: true })
    .click();
  await page.waitForFunction(
    () => document.activeElement.id === "review-heading",
  );
  assert.equal(await textarea.getAttribute("readonly"), "");
  await page.getByText("Try a conflicting write", { exact: true }).click();
  await page
    .getByRole("button", { name: "Simulate another operator saving" })
    .click();
  await page
    .getByRole("status")
    .filter({ hasText: "Another operator saved" })
    .waitFor();
  await page.getByRole("button", { name: "Apply reviewed changes" }).click();
  await page.waitForFunction(
    () => document.activeElement.id === "conflict-heading",
  );
  assert.equal(await textarea.inputValue(), draft);
  await page
    .locator(".conflict")
    .screenshot({ path: resolve(captures, "config-conflict.png") });
  await page.getByRole("button", { name: "Keep my draft and edit" }).click();
  assert.equal(await textarea.inputValue(), draft);
  await page
    .getByRole("button", { name: "Review changes", exact: true })
    .click();
  await page.getByRole("button", { name: "Apply reviewed changes" }).click();
  await page
    .getByRole("status")
    .filter({ hasText: "Saved revision 3" })
    .waitFor();
  assert.equal(
    (await (await context.request.get(`${base}/sandbox/config`)).json()).yaml,
    draft,
  );
  assert.equal(await page.locator(".draft-label").textContent(), "Up to date");
  await page.waitForFunction(
    () => document.activeElement.id === "config-heading",
  );
  await page.evaluate(() => scrollTo(0, 0));
  await page.screenshot({ path: resolve(captures, "config-dark.png") });

  for (const width of [390, 768, 1280]) {
    await page.setViewportSize({ width, height: 900 });
    for (const screen of ["Activity", "Configuration"]) {
      await page.getByRole("button", { name: screen, exact: true }).click();
      assert(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      );
      for (const theme of ["light", "dark"]) {
        const desired = page.getByRole("button", {
          name: `Use ${theme} theme`,
        });
        if (await desired.count()) await desired.click();
        await page.evaluate(() => scrollTo(0, 0));
        await page.screenshot({
          path: resolve(
            captures,
            `${screen.toLowerCase()}-${width}-${theme}.png`,
          ),
        });
      }
    }
  }
  assert.deepEqual(failures, [], "Browser exceptions");
  assert.deepEqual(external, [], "Unexpected external requests");
  console.log(
    `Passed: WASM startup, activity, recovery, draft retention, conflict, save, keyboard focus, 3 widths, 2 themes. Captures: ${captures}`,
  );
} finally {
  await browser?.close();
  server.kill("SIGTERM");
}
