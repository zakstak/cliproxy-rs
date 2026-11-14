#!/usr/bin/env python3
"""Synthetic, network-free checks for the offline replay's observable results."""

import copy
import json
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("replay.py")
API = runpy.run_path(str(SCRIPT))


def record(seconds, *, first_delay=1, scope="account-a", history="history-a", kind="main"):
    def point(name, size, path):
        return {"path": path, "ttl": "1h", "prefix": {"hash": name, "bytes": size * 3}, "lookback": []}
    cache = {"schema_version": 2, "media_present": False, "model": "claude-opus-5-5", "speed": "",
             "session_hash": "session-a", "scope_hash": scope, "thinking_hash": "adaptive",
             "effort": "high", "betas": [], "tools": {"hash": "tools"}, "system": {"hash": "system"},
             "history_prefix": {"hash": history}, "request_type": kind, "base_request_type": kind,
             "request_start_unix_ms": seconds * 1000,
             "first_response_byte_unix_ms": (seconds + first_delay) * 1000,
             "end_unix_ms": (seconds + first_delay + 1) * 1000,
             "breakpoints": [point("system-a", 4000, "system.0"), point(history, 8000, "messages.0.content.0")]}
    return {"execution_id": f"{seconds}-{scope}-{history}", "request_id": str(seconds), "claude_cache": cache,
            "tokens": {"input_tokens": 10, "output_tokens": 2, "cache_read_tokens": 0,
                       "cache_creation_tokens": 8000, "cache_creation_5m_tokens": 0,
                       "cache_creation_1h_tokens": 8000}}


class ReplayTests(unittest.TestCase):
    def replay(self, rows, policy, end=None, **options):
        end = end if end is not None else rows[-1]["claude_cache"]["end_unix_ms"]
        return API["replay"](rows, policy, 0, end, **options)

    def test_policies_have_distinct_costs_and_ping_overhead(self):
        rows = [record(0), record(600), record(4200)]
        five = self.replay(rows, "5m")
        hour = self.replay(rows, "1h")
        hybrid = self.replay(rows, "hybrid")
        ping = self.replay(rows, "1h+bounded-pings")
        self.assertEqual(five["write_5m_tokens"], 24000)
        self.assertEqual(hour["write_1h_tokens"], 16000)
        self.assertEqual(hour["read_tokens"], 8000)
        self.assertEqual(hybrid["write_1h_tokens"], 8000)
        self.assertEqual(hybrid["write_5m_tokens"], 12000)
        self.assertEqual(hybrid["read_tokens"], 4000)
        automatic = copy.deepcopy(rows)
        for row in automatic:
            row["claude_cache"]["breakpoints"][0]["path"] = "cache_control:system.0"
        self.assertEqual(self.replay(automatic, "hybrid"), hybrid, "automatic system targets also use 1h")
        self.assertEqual(ping["write_1h_tokens"], 8000)
        self.assertEqual(ping["read_tokens"], 16000)
        self.assertEqual(ping["pings"], 1)
        self.assertEqual(ping["ping_read_tokens"], 8000)
        self.assertEqual(ping["ping_input_tokens"], 128)
        # Independent calculation: 30 uncached + 16000 write-equivalent +
        # 1200 read-equivalent + 128 maintenance input, and six output tokens.
        self.assertAlmostEqual(ping["api_equivalent_usd_low"], (17358 * 4 + 6 * 20) / 1_000_000)

    def test_request_start_anchor_sliding_ttl_and_cold_concurrency(self):
        sliding = self.replay([record(0), record(240), record(480)], "5m")
        self.assertEqual(sliding["write_5m_tokens"], 8000)
        self.assertEqual(sliding["read_tokens"], 16000)
        expired = self.replay([record(0, first_delay=200), record(300)], "5m")
        self.assertEqual(expired["write_5m_tokens"], 16000, "expiry is not first-byte + TTL")
        concurrent = self.replay([record(0, first_delay=5), record(1, first_delay=5), record(6)], "1h")
        self.assertEqual(concurrent["write_1h_tokens"], 16000)
        self.assertEqual(concurrent["read_tokens"], 8000)

    def test_prefix_tail_lookback_and_scope_isolation(self):
        changed = record(10, history="history-b")
        changed["claude_cache"]["breakpoints"][-1]["prefix"]["bytes"] = 30000
        changed["tokens"]["cache_creation_tokens"] = 10000
        changed["tokens"]["cache_creation_1h_tokens"] = 10000
        changed["claude_cache"]["breakpoints"][-1]["lookback"] = [{"hash": "history-a", "bytes": 24000}]
        tail = self.replay([record(0), changed], "1h")
        self.assertEqual(tail["read_tokens"], 8000)
        self.assertEqual(tail["write_1h_tokens"], 10000, "8000 first write + 2000 new tail")
        isolated = self.replay([record(0), record(10, scope="account-b")], "1h")
        self.assertEqual(isolated["write_1h_tokens"], 16000)
        self.assertEqual(isolated["read_tokens"], 0)

    def test_abandonment_bounds_and_compaction_stop(self):
        abandoned = self.replay([record(0)], "1h+bounded-pings", end=4 * 3_600_000)
        self.assertEqual(abandoned["pings"], 3, "no future return needed to incur maintenance cost")
        short = self.replay([record(0)], "1h+bounded-pings", end=4 * 3_600_000, max_hours=1)
        self.assertEqual(short["pings"], 1)
        compact = record(1200, kind="compaction")
        compact["claude_cache"]["breakpoints"] = []
        compact["tokens"]["cache_creation_tokens"] = 0
        compact["tokens"]["cache_creation_1h_tokens"] = 0
        compact["tokens"]["output_tokens"] = 500
        stopped = self.replay([record(0), compact], "1h+bounded-pings", end=4 * 3_600_000)
        self.assertEqual(stopped.get("pings", 0), 0)
        self.assertEqual(stopped["output_tokens"], 502)
        other = record(1800, kind="helper")
        other["claude_cache"]["session_hash"] = "another-session"
        refreshed = self.replay([record(0), other], "1h+bounded-pings", end=70 * 60_000)
        self.assertEqual(refreshed.get("pings", 0), 0, "matching activity delays another session's timer")

    def test_unmodelled_writes_remain_charged_and_missing_ttl_is_unknown(self):
        failed = record(0)
        failed["failed"] = True
        failed["tokens"].pop("cache_creation_5m_tokens")
        failed["tokens"].pop("cache_creation_1h_tokens")
        result = self.replay([failed], "1h")
        self.assertEqual(result["unknown_ttl_write_tokens"], 8000)
        self.assertAlmostEqual(result["api_equivalent_usd_high"] - result["api_equivalent_usd_low"], 8000 * 3 / 1_000_000)

    def test_media_is_charged_but_never_weighted_as_text_or_pinged(self):
        media = record(0)
        media["claude_cache"]["media_present"] = True
        media["claude_cache"]["breakpoints"][-1]["prefix"]["bytes"] = 30_000_000
        result = self.replay([media], "1h+bounded-pings", end=4 * 3_600_000)
        self.assertEqual(result["media_requests"], 1)
        self.assertEqual(result["unmodelled_requests"], 1)
        self.assertEqual(result["write_1h_tokens"], 8000)
        self.assertEqual(result.get("pings", 0), 0)

    def test_cli_failure_buckets_cannot_cancel_another_requests_unknown_writes(self):
        failed = record(0)
        failed["failed"] = True
        failed["tokens"]["cache_creation_tokens"] = 0  # Go failure aggregate, not provider buckets.
        unknown = record(10)
        unknown["tokens"].pop("cache_creation_5m_tokens")
        unknown["tokens"].pop("cache_creation_1h_tokens")
        for row in [failed, unknown]:
            row["tokens"]["input_tokens"] = row["tokens"]["output_tokens"] = 0
        with tempfile.TemporaryDirectory() as tmp:
            logs = Path(tmp) / "usage.jsonl"
            logs.write_text("\n".join(json.dumps(r) for r in [failed, unknown]), encoding="utf-8")
            run = subprocess.run([sys.executable, str(SCRIPT), str(logs)], capture_output=True, text=True, check=True)
            result = json.loads(run.stdout)
        self.assertAlmostEqual(result["observed_api_equivalent_usd_low"], 0.104)
        self.assertAlmostEqual(result["observed_api_equivalent_usd_high"], 0.128)
        self.assertEqual(result["replay_validation"]["write_tokens_error"], 0)
        self.assertEqual(result["replay_validation"]["unknown_observed_write_ttl_tokens"], 8000)
        self.assertEqual(result["observed_tokens"]["cache_creation_tokens"], 8000)
        self.assertEqual(result["observed_tokens"]["effective_cache_creation_tokens"], 16000)

    def test_cli_unpriced_activity_still_cancels_pings_and_flags_wake_writes(self):
        with tempfile.TemporaryDirectory() as tmp:
            logs, wakes = Path(tmp) / "usage.jsonl", Path(tmp) / "wakeups.jsonl"
            wakes.write_text(json.dumps({"timestamp": 1_200_000, "session_hash": "session-a"}), encoding="utf-8")
            for cancellation in ["other_model", "fast", "error"]:
                with self.subTest(cancellation=cancellation):
                    activity = record(1200, kind="compaction")
                    if cancellation == "fast":
                        activity["claude_cache"]["speed"] = "fast"
                    else:
                        activity["claude_cache"]["model"] = "claude-sonnet-5-5"
                    activity["failed"] = cancellation == "error"
                    rows = [record(0), activity, record(4200)]
                    logs.write_text("\n".join(json.dumps(r) for r in rows), encoding="utf-8")
                    run = subprocess.run([sys.executable, str(SCRIPT), str(logs), "--wakeups", str(wakes)],
                                         capture_output=True, text=True, check=True)
                    result = json.loads(run.stdout)
                    ping = result["policies"]["1h+bounded-pings"]
                    self.assertEqual(ping.get("pings", 0), 0)
                    self.assertEqual(ping["write_1h_tokens"], 16000)
                    self.assertEqual(ping["requests"], 2, "only selected-model standard traffic is priced")
                    self.assertEqual(len(result["wake_flags"]), 1)

    def test_cli_week_window_dedup_and_wake_hypotheses(self):
        rows = [record(0, first_delay=5), record(1, first_delay=5), record(10, history="changed")]
        rows += [record(3600 * 24 * day) for day in range(1, 8)]
        with tempfile.TemporaryDirectory() as tmp:
            logs, wakes = Path(tmp) / "usage.jsonl", Path(tmp) / "wakeups.jsonl"
            logs.write_text("\n".join(json.dumps(r) for r in [*rows, copy.deepcopy(rows[-1])]), encoding="utf-8")
            wakes.write_text(json.dumps({"timestamp": 0, "session_hash": "session-a"}), encoding="utf-8")
            run = subprocess.run([sys.executable, str(SCRIPT), str(logs), "--wakeups", str(wakes), "--days", "8"],
                                 capture_output=True, text=True, check=True)
            result = json.loads(run.stdout)
            self.assertEqual(result["skipped"]["duplicate_execution"], 1)
            self.assertEqual(result["wake_flags"][1]["same_prefix_rewrite"], True)
            self.assertEqual(result["wake_flags"][1]["overlapping_cold_start"], True)
            self.assertIn("history_prefix", result["wake_flags"][2]["changed_fields"])
            week = subprocess.run([sys.executable, str(SCRIPT), str(logs)], capture_output=True, text=True, check=True)
            # The window ends two seconds after day seven; both opening cold
            # requests are outside it. Earlier logs still warm the replay state.
            self.assertEqual(json.loads(week.stdout)["policies"]["5m"]["requests"], 8)


if __name__ == "__main__":
    unittest.main()
