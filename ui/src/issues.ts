// Issues at the proxy level: what is wrong right now that stops, or will soon stop, requests
// from being served. Built only from what the proxy already knows:
//   - each account's state and limits (GET /credentials, and limit checks made in this tab);
//   - requests that failed in the last 15 minutes of the live watch, grouped by cause;
//   - whether the server reports requests at all.
// Failures inside a response that the proxy forwarded successfully (a tool call that went
// wrong, a refusal) are not here: the proxy does not parse responses yet.
import { accountName as credName } from "./account";
import { credState, label, provider, span, type Data } from "./core";
import { app, watch } from "./app.svelte";
import { key, limits } from "./limits";
import type { Request } from "./records";
import { plural, when } from "./fmt";

export type Issue = {
  key: string;
  level: "bad" | "warn";
  /** Who has to act: the account's owner, the provider, whoever sent the request, or the proxy's operator. */
  kind: "Account" | "Provider" | "Request" | "Proxy";
  title: string;
  detail: string;
  /** When it last happened; 0 for a standing condition. */
  last: number;
  action: { label: string; href: string } | null;
};

export const ISSUE_WINDOW = 15 * 60_000;
const unavailable = (a: Data, now: number) => {
  const state = credState(a, now);
  return !!a.disabled || state.lamp === "bad" || state.lamp === "warn";
};
const who = (a: Data) => `${credName(a)} (${label(provider(a))})`;

function accountIssues(accounts: Data[], now: number): Issue[] {
  const out: Issue[] = [];
  for (const a of accounts) {
    if (a.disabled) continue;
    const s = credState(a, now);
    const href = `#accounts/${encodeURIComponent(key(a))}`;
    if (s.lamp === "bad")
      out.push({
        key: `state:${key(a)}`,
        level: "bad",
        kind: "Account",
        title:
          s.label === "Sign in again"
            ? `${who(a)} needs you to sign in again`
            : `${who(a)} cannot serve requests`,
        detail: s.detail || "The server marks this account as failing.",
        last: 0,
        action: { label: "Open account", href },
      });
    else if (s.lamp === "warn")
      out.push({
        key: `state:${key(a)}`,
        level: "warn",
        kind: "Account",
        title: `${who(a)} is out of rotation until ${s.until ? when(s.until, now) : "it recovers"}`,
        detail:
          s.detail ||
          "The provider limited it; the proxy is waiting before it tries again.",
        last: 0,
        action: { label: "Open account", href },
      });
    else {
      // Nearly spent: it works now and will stop soon.
      const l = limits(a);
      const tight =
        l && "windows" in l
          ? l.windows
              .filter((w) => w.used !== null && w.used >= 90)
              .sort((x, y) => y.used! - x.used!)[0]
          : undefined;
      if (tight)
        out.push({
          key: `limit:${key(a)}`,
          level: "warn",
          kind: "Account",
          title: `${who(a)} has used ${Math.round(tight.used!)}% of its limit`,
          detail: `${tight.label}${Date.parse(tight.reset) > now ? `, resets in ${span(Date.parse(tight.reset) - now)}` : ""}.`,
          last: 0,
          action: { label: "Open account", href },
        });
    }
  }
  if (accounts.length && accounts.every((a) => a.disabled))
    out.push({
      key: "all-disabled",
      level: "bad",
      kind: "Proxy",
      title: "Every account is switched off",
      detail:
        "The proxy has no account to send requests to, so every request fails.",
      last: 0,
      action: { label: "Open Accounts", href: "#accounts" },
    });
  // Report an outage only when every account is known unavailable. Config keys have
  // unknown runtime health, which is not evidence that their provider cannot serve.
  for (const p of new Set(accounts.filter((a) => !a.disabled).map(provider)))
    if (accounts.filter((a) => provider(a) === p).every((a) => unavailable(a, now)))
      out.push({
        key: `provider:${p}`,
        level: "bad",
        kind: "Proxy",
        title: `No ${label(p)} account can serve right now`,
        detail: `Requests for ${label(p)} models fail until one of its accounts recovers.`,
        last: 0,
        action: { label: "Open Accounts", href: "#accounts" },
      });
  return out;
}

function failureIssues(
  requests: Request[],
  accounts: Data[],
  now: number,
): Issue[] {
  const groups = new Map<string, Request[]>();
  for (const r of requests) {
    if (!r.failed || r.at + r.latency < now - ISSUE_WINDOW) continue;
    const k = `${r.auth}:${r.status}`;
    groups.set(k, [...(groups.get(k) || []), r]);
  }
  return [...groups.entries()].map(([k, list]): Issue => {
    const r = list[list.length - 1];
    const a = accounts.find((x) => x.auth_index === r.auth);
    const on = a ? who(a) : label(r.provider);
    const n = plural(list.length, "request");
    const status = r.status || 0;
    const href = `#logs/requests?failed=1&account=${encodeURIComponent(r.auth)}&q=${status || "failed"}`;
    const [kind, level, title]: [Issue["kind"], Issue["level"], string] =
      status === 401 || status === 403
        ? ["Account", "bad", `${on} had its token rejected on ${n} (${status})`]
        : status === 429
          ? ["Account", "warn", `${on} was rate limited on ${n} (429)`]
          : status >= 500
            ? [
                "Provider",
                list.length >= 3 ? "bad" : "warn",
                `${label(r.provider)} failed ${n} on ${a ? credName(a) : "an account"} (${status})`,
              ]
            : status === 0
              ? ["Proxy", "bad", `${n} on ${on} ended without an answer`]
              : [
                  "Request",
                  "warn",
                  `${label(r.provider)} refused ${n} on ${a ? credName(a) : "an account"} (${status})`,
                ];
    return {
      key: `fail:${k}`,
      level,
      kind,
      title,
      detail:
        r.failure?.message ||
        r.failure?.code ||
        "The server recorded no reason.",
      last: r.at + r.latency,
      action: { label: "Show requests", href },
    };
  });
}

/** What is wrong now, worst first. `now` comes from the caller so the list ages with the clock. */
export function currentIssues(now: number): Issue[] {
  const accounts = app.accounts.data || [];
  const out = [
    ...accountIssues(accounts, now),
    ...failureIssues(watch.requests, accounts, now),
  ];
  if (!watch.statistics)
    out.push({
      key: "statistics",
      level: "warn",
      kind: "Proxy",
      title: "The server is not reporting requests",
      detail:
        "Usage statistics are off, so failed requests cannot be seen here.",
      last: 0,
      action: { label: "Open Settings", href: "#settings" },
    });
  else if (watch.problem)
    out.push({
      key: "watch",
      level: "warn",
      kind: "Proxy",
      title: "Live requests cannot be read",
      detail: watch.problem,
      last: 0,
      action: null,
    });
  return out.sort(
    (x, y) =>
      Number(y.level === "bad") - Number(x.level === "bad") || y.last - x.last,
  );
}
