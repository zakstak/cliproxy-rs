import { signIns, type Data } from "./core.ts";

/** Config API keys are absent from /credentials. Join the server-assigned auth index in
 * /config to /observability/usage/api-keys without inventing runtime health or history. */
export function configuredAccounts(config: Data, usage: Data): Data[] {
  return signIns.flatMap(provider => (Array.isArray(config["api-keys"]?.[provider]) ? config["api-keys"][provider] : []).flatMap((group: Data) =>
    (Array.isArray(group.keys) ? group.keys : []).flatMap((key: Data) => {
      if (typeof key.auth_index !== "string" || typeof key["api-key"] !== "string") return [];
      const base = String(key["base-url"] ?? group["base-url"] ?? "").trim();
      const secret = key["api-key"].trim();
      const counts = usage[provider]?.[`${base}|${secret}`];
      return [{
        id: key.auth_index, name: key.auth_index, auth_index: key.auth_index,
        provider, type: provider, account_type: "api_key", account: secret,
        label: group.name || "API key", runtime_only: true, configured_only: true,
        status: "unknown", disabled: false,
        status_message: "Configured provider key. The server reports its traffic separately, but does not report its runtime health here.",
        ...(counts && typeof counts === "object" ? { recent_requests: counts.recent_requests, success: counts.success, failed: counts.failed } : {}),
      }];
    }),
  ));
}
