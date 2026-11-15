import { test } from "node:test";
import assert from "node:assert/strict";
import { configuredAccounts } from "./configured-accounts.ts";
test("config keys join their exact server index and usage, including endpoint overrides", () => {
  const config = { "api-keys": { claude: [{ name: "Team", "base-url": "http://localhost", disabled: true, keys: [
    { "api-key": "first-test", auth_index: "index-1" },
    { "api-key": "second-test", "base-url": "http://localhost/second", auth_index: "index-2" },
    { "api-key": "not-registered" },
  ] }] } };
  const usage = { claude: { "http://localhost|first-test": { recent_requests: [{ time: "now", success: 2, failed: 1 }] } } };
  const [first, second] = configuredAccounts(config, usage);
  assert.equal(configuredAccounts(config, usage).length, 2);
  assert.equal(first.auth_index, "index-1");
  assert.deepEqual(first.recent_requests, usage.claude["http://localhost|first-test"].recent_requests);
  assert.equal(second.recent_requests, undefined, "missing usage is unknown, never zero");
  assert.equal(first.disabled, false, "an ignored group field cannot masquerade as runtime state");
  assert.equal(first.status, "unknown");
  assert.equal(first.configured_only, true);
});
