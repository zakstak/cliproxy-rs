#!/usr/bin/env python3
"""Offline Claude cache replay. Reads JSONL; never imports a network client."""

import argparse
from collections import Counter
from datetime import datetime
import heapq
import json
from pathlib import Path


def unix_ms(value):
    if isinstance(value, (int, float)):
        return int(value)
    return int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000)


def read_jsonl(paths):
    for path in paths:
        with Path(path).open(encoding="utf-8") as source:
            for number, line in enumerate(source, 1):
                if line.strip():
                    try:
                        yield json.loads(line)
                    except ValueError as error:
                        raise ValueError(f"{path}:{number}: {error}") from error


def ttl(policy, breakpoint):
    if policy == "observed":
        return 3600 if breakpoint["ttl"] == "1h" else 300
    if policy.startswith("1h"):
        return 3600
    if policy == "hybrid" and breakpoint["path"].removeprefix("cache_control:").startswith(("tools", "system")):
        return 3600
    return 300


def cache_writes(tokens):
    """Exact returned buckets survive zeroed failure aggregates; missing TTL is unknown."""
    five, hour = tokens.get("cache_creation_5m_tokens", 0), tokens.get("cache_creation_1h_tokens", 0)
    return five, hour, max(0, tokens.get("cache_creation_tokens", 0) - five - hour)


def replay(records, policy, start, end, *, ping_minutes=57, max_pings=3,
           max_hours=4, ping_input=128, input_price=4, output_price=20,
           read_multiplier=0.05, model=None):
    """Sliding TTL from request start; cold writes become visible at first byte.

    Text byte positions are scaled to each request's actual read+write total.
    Media requests remain unmodelled. Retain real input/output (including compaction).
    The ping policy is causal: it pays even if the user never returns.
    """
    entries, generations = {}, Counter()
    events, serial = [], 0
    totals = Counter()

    def enqueue(at, kind, data):
        nonlocal serial
        serial += 1
        # Real activity at the same instant cancels a pending ping; writes at that
        # instant are visible before either request starts.
        priority = {"write": 0, "real": 1, "ping": 2}[kind]
        heapq.heappush(events, (at, priority, serial, kind, data))

    for record in records:
        enqueue(record["claude_cache"]["request_start_unix_ms"], "real", record)

    while events:
        now, _, _, kind, data = heapq.heappop(events)
        if now > end:
            break
        counted = now >= start
        if kind == "write":
            key, expiry, duration = data
            old = entries.get(key)
            if old is None or old[0] < expiry:
                entries[key] = (expiry, duration)
            continue
        if kind == "ping":
            record, generation, n, last_real = data
            cache = record["claude_cache"]
            session = cache["session_hash"]
            if generations[session] != generation:
                continue
            points = cache["breakpoints"]
            key = (cache["scope_hash"], points[-1]["prefix"]["hash"])
            entry = entries.get(key)
            if entry is None or entry[0] <= now:
                if counted:
                    totals["pings_stopped_on_miss"] += 1
                continue  # Never simulate a cold rewrite as a successful refresh.
            # Matching activity in another session can refresh the same cache.
            # Delay, rather than paying for a redundant maintenance read.
            due = entry[0] - 3_600_000 + ping_minutes * 60_000
            if due > now:
                if due <= last_real + max_hours * 3_600_000:
                    enqueue(due, "ping", data)
                continue
            tokens = record["tokens"]
            size = tokens.get("cache_read_tokens", 0) + tokens.get("cache_creation_tokens", 0)
            if counted:
                totals["pings"] += 1
                totals["ping_read_tokens"] += size
                totals["ping_input_tokens"] += ping_input
            for point in points:
                pkey = (cache["scope_hash"], point["prefix"]["hash"])
                if pkey in entries and entries[pkey][0] > now:
                    entries[pkey] = (now + 3_600_000, 3600)
            next_at = now + ping_minutes * 60_000
            if n < max_pings and next_at <= last_real + max_hours * 3_600_000:
                enqueue(next_at, "ping", (record, generation, n + 1, last_real))
            continue

        record = data
        cache, tokens = record["claude_cache"], record["tokens"]
        session = cache["session_hash"]
        generations[session] += 1  # Any real activity cancels this session's old timer.
        if model is not None and (cache["model"] != model or cache.get("speed") == "fast"):
            if counted:
                totals["unpriced_activity_requests"] += 1
            continue
        if counted:
            totals["requests"] += 1
            totals["input_tokens"] += tokens.get("input_tokens", 0)
            totals["output_tokens"] += tokens.get("output_tokens", 0)
        points = cache["breakpoints"]
        write_5m, write_1h, unknown_write = cache_writes(tokens)
        total = tokens.get("cache_read_tokens", 0) + write_5m + write_1h + unknown_write
        first = cache.get("first_response_byte_unix_ms")
        if (record.get("failed") or record.get("usage_observed") is False
                or cache.get("media_present") or not points or total <= 0 or first is None):
            if counted:
                totals["unmodelled_requests"] += 1
                totals["media_requests"] += bool(cache.get("media_present"))
                totals["unmodelled_cache_tokens"] += total
                totals["read_tokens"] += tokens.get("cache_read_tokens", 0)
                totals["write_5m_tokens"] += write_5m
                totals["write_1h_tokens"] += write_1h
                totals["unknown_ttl_write_tokens"] += unknown_write
            continue
        last_bytes = points[-1]["prefix"]["bytes"]
        if last_bytes <= 0:
            raise ValueError("non-positive final prefix length")
        scale = total / last_bytes
        read = 0
        matches = {}
        for point in points:
            for candidate in [*point["lookback"], point["prefix"]]:
                key = (cache["scope_hash"], candidate["hash"])
                entry = entries.get(key)
                if entry is not None and entry[0] > now:
                    read = max(read, candidate["bytes"])
                    matches[key] = entry[1]
        read = min(read, last_bytes)
        if counted:
            totals["read_tokens"] += read * scale
            totals["cold_requests"] += read == 0
        for key, duration in matches.items():
            entries[key] = (now + duration * 1000, duration)
        cursor = read
        for point in points:
            duration = ttl(policy, point)
            position = point["prefix"]["bytes"]
            written = max(0, position - cursor) * scale
            if counted:
                totals[f"write_{'1h' if duration == 3600 else '5m'}_tokens"] += written
            cursor = max(cursor, position)
            key = (cache["scope_hash"], point["prefix"]["hash"])
            enqueue(max(now, first), "write", (key, now + duration * 1000, duration))
        if (policy == "1h+bounded-pings" and session and max_pings > 0
                and cache["base_request_type"] == "main"):
            next_at = now + ping_minutes * 60_000
            if next_at >= first and next_at <= now + max_hours * 3_600_000:
                enqueue(next_at, "ping", (record, generations[session], 1, now))

    totals["api_equivalent_usd"] = (
        (totals["input_tokens"] + totals["ping_input_tokens"]
         + 1.25 * totals["write_5m_tokens"] + 2 * totals["write_1h_tokens"]
         + read_multiplier * (totals["read_tokens"] + totals["ping_read_tokens"])) * input_price
        + totals["output_tokens"] * output_price
    ) / 1_000_000
    known = totals.pop("api_equivalent_usd")
    totals["api_equivalent_usd_low"] = known + 1.25 * totals["unknown_ttl_write_tokens"] * input_price / 1_000_000
    totals["api_equivalent_usd_high"] = known + 2 * totals["unknown_ttl_write_tokens"] * input_price / 1_000_000
    return dict(totals)


def wake_flags(records, wakes, window_ms=120_000):
    """Flag observations, not causes. Wake exports must contain hashed sessions."""
    flags = []
    for wake in wakes:
        at = unix_ms(wake["timestamp"])
        near = [r for r in records if r["claude_cache"]["session_hash"] == wake["session_hash"]
                and at <= r["claude_cache"]["request_start_unix_ms"] <= at + window_ms]
        cold = [r for r in near if sum(cache_writes(r["tokens"])) > 0
                and r["tokens"].get("cache_read_tokens", 0) == 0]
        for i, record in enumerate(cold):
            cache = record["claude_cache"]
            previous = cold[:i]
            identical = [r for r in previous if r["claude_cache"]["breakpoints"]
                         and cache["breakpoints"]
                         and r["claude_cache"]["scope_hash"] == cache["scope_hash"]
                         and r["claude_cache"]["breakpoints"][-1]["prefix"]["hash"]
                         == cache["breakpoints"][-1]["prefix"]["hash"]]
            overlap = any(r["claude_cache"].get("first_response_byte_unix_ms") is not None
                          and cache["request_start_unix_ms"] < r["claude_cache"]["first_response_byte_unix_ms"]
                          for r in identical)
            changes = []
            if previous:
                old = previous[-1]["claude_cache"]
                for key in ["scope_hash", "model", "thinking_hash", "effort", "betas", "tools", "system", "history_prefix"]:
                    if old[key] != cache[key]:
                        changes.append(key)
            flags.append({"wake_unix_ms": at, "request_id": record.get("request_id", ""),
                          "start_unix_ms": cache["request_start_unix_ms"],
                          "write_tokens": sum(cache_writes(record["tokens"])),
                          "same_prefix_rewrite": bool(identical), "overlapping_cold_start": overlap,
                          "changed_fields": changes, "request_type": cache["request_type"]})
    return flags


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="+", help="JSONL usage records")
    parser.add_argument("--wakeups", help="JSONL timestamp + session_hash schedule export")
    parser.add_argument("--days", type=float, default=7)
    parser.add_argument("--model", default="claude-opus-5-5")
    parser.add_argument("--input-usd", type=float, default=4)
    parser.add_argument("--output-usd", type=float, default=20)
    parser.add_argument("--read-multiplier", type=float, default=0.05)
    parser.add_argument("--ping-minutes", type=float, default=57)
    parser.add_argument("--max-pings", type=int, default=3)
    parser.add_argument("--max-hours", type=float, default=4)
    parser.add_argument("--ping-input-tokens", type=int, default=128)
    args = parser.parse_args()
    if not (args.days > 0 and 0 < args.ping_minutes < 60 and 0 <= args.max_pings <= 3
            and 0 <= args.max_hours <= 4 and args.ping_input_tokens >= 0
            and args.input_usd >= 0 and args.output_usd >= 0 and args.read_multiplier >= 0):
        parser.error("invalid window, prices or bounded-ping limits (max 3 pings / 4 hours)")
    records, skipped, seen = [], Counter(), set()
    for record in read_jsonl(args.logs):
        cache = record.get("claude_cache")
        if not cache or cache.get("schema_version") != 2 or cache.get("request_start_unix_ms") is None:
            skipped["missing_telemetry"] += 1
            continue
        execution = record.get("execution_id")
        if execution and execution in seen:
            skipped["duplicate_execution"] += 1
            continue
        if execution:
            seen.add(execution)
        records.append(record)
    if not records:
        parser.error("no eligible Claude telemetry records")
    records.sort(key=lambda r: r["claude_cache"]["request_start_unix_ms"])
    end = max(r["claude_cache"].get("end_unix_ms") or r["claude_cache"]["request_start_unix_ms"] for r in records)
    start = end - int(args.days * 86_400_000)
    activity = [r for r in records if r["claude_cache"]["request_start_unix_ms"] >= start]
    selected = [r for r in activity if r["claude_cache"]["model"] == args.model
                and r["claude_cache"].get("speed") != "fast"]
    if not selected:
        parser.error("no selected-model standard-price records in this window")
    options = dict(ping_minutes=args.ping_minutes, max_pings=args.max_pings, max_hours=args.max_hours,
                   ping_input=args.ping_input_tokens, input_price=args.input_usd,
                   output_price=args.output_usd, read_multiplier=args.read_multiplier, model=args.model)
    observed = Counter()
    for record in selected:
        observed.update({k: v for k, v in record["tokens"].items() if isinstance(v, int) and not isinstance(v, bool)})
        five, hour, unknown = cache_writes(record["tokens"])
        observed["effective_cache_creation_tokens"] += five + hour + unknown
        observed["unknown_ttl_write_tokens"] += unknown
    policies = {p: replay(records, p, start, end, **options)
                for p in ["observed", "5m", "1h", "hybrid", "1h+bounded-pings"]}
    validation = policies["observed"]
    unknown_write = observed["unknown_ttl_write_tokens"]
    actual_cost = (args.input_usd * (observed["input_tokens"]
                   + args.read_multiplier * observed["cache_read_tokens"]
                   + 1.25 * observed["cache_creation_5m_tokens"] + 2 * observed["cache_creation_1h_tokens"])
                   + args.output_usd * observed["output_tokens"]) / 1_000_000
    result = {"window_start_unix_ms": start, "window_end_unix_ms": end,
              "coverage_hours": (end - selected[0]["claude_cache"]["request_start_unix_ms"]) / 3_600_000,
              "skipped": dict(skipped), "observed_tokens": dict(observed),
              "request_types": dict(Counter(r["claude_cache"]["request_type"] for r in selected)),
              "assumptions": {"prices_usd_per_million": {"input": args.input_usd, "output": args.output_usd},
                              **options, "not_subscription_quota": True,
                              "counterfactual_token_positions_are_estimates": True,
                              "position_estimator": "serialized_text_bytes_scaled_to_aggregate_usage",
                              "media_requests_are_unmodelled": True,
                              "fingerprints_rotate_on_proxy_restart": True,
                              "ping_compatibility_and_success_are_unverified": True,
                              "left_boundary_is_cold_without_earlier_logs": True,
                              "right_boundary_return_or_abandonment_is_unknown": True},
              "observed_api_equivalent_usd_low": actual_cost + 1.25 * unknown_write * args.input_usd / 1_000_000,
              "observed_api_equivalent_usd_high": actual_cost + 2 * unknown_write * args.input_usd / 1_000_000,
              "replay_validation": {
                  "read_tokens_error": validation.get("read_tokens", 0) - observed["cache_read_tokens"],
                  "write_tokens_error": (validation.get("write_5m_tokens", 0) + validation.get("write_1h_tokens", 0)
                                         + validation.get("unknown_ttl_write_tokens", 0)
                                         - observed["effective_cache_creation_tokens"]),
                  "unknown_observed_write_ttl_tokens": unknown_write},
              "policies": policies,
              "wake_flags": wake_flags(activity, list(read_jsonl([args.wakeups]))) if args.wakeups else [],
              "wakeups_provided": bool(args.wakeups)}
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
