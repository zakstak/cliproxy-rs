"""Negative regressions for the acceptance command (issue #67, H1 review of f4d8e222).

Each test reproduces a hole the review found and must fail on that revision's harness.
They build synthetic captures in the collector's layout and need no Go, Rust build or
namespace."""
import copy
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import acceptance
import golden
import judge
import manifest as revisions
import provenance

PINS = revisions.load(revisions.suite_sizes())
ROOT = revisions.ROOT
CLAUDE_AUTH = "Bearer sk-ant-oat01-FAKE-LOCAL-ONLY"
UUID = "0f0e0d0c-0b0a-4908-8706-050403020100"


def request(body=b'{"model":"claude-sonnet-4-6","max_tokens":17,"messages":[]}', headers=None,
            line="POST /v1/messages?beta=true HTTP/1.1"):
    headers = headers or [["Authorization", CLAUDE_AUTH], ["x-client-request-id", UUID],
                          ["Host", "api.anthropic.com"]]
    headers = [h for h in headers if h[0].lower() != "content-length"] + [["Content-Length", str(len(body))]]
    head = line + "\r\n" + "".join(f"{k}: {v}\r\n" for k, v in headers) + "\r\n"
    return {"request_line": line, "headers": headers, "raw_head_hex": head.encode("latin1").hex(),
            "body_hex": body.hex(), "body_text": body.decode("utf-8", "replace"), "write_result": "complete"}


def reply(body=b'{"type":"message","content":[{"type":"text","text":"local answer"}]}', headers=None, status=200):
    headers = (headers or [["Content-Type", "application/json"]]) + [["Content-Length", str(len(body))]]
    head = f"HTTP/1.1 {status} OK\r\n" + "".join(f"{k}: {v}\r\n" for k, v in headers) + "\r\n"
    return {"status": status, "reason": "OK", "headers": headers, "body_hex": body.hex(),
            "body_text": body.decode("utf-8", "replace"), "raw_response_hex": (head.encode() + body).hex(),
            "request_body_hex": b'{"model":"claude-sonnet-4-6"}'.hex()}


def observation(requests=None, replies=None, tls=None, credential=None):
    return {"downstream": replies or [reply()],
            "upstream": {"tls": tls or [], "http": [request()] if requests is None else requests, "errors": []},
            "credential_after": credential or {"type": "claude"}}


def pins_with(ids=(), skips=()):
    out = copy.deepcopy(PINS)
    out["divergences"] = [d for d in out["divergences"] if d["id"] in ids]
    out["skips"] = list(skips)
    return out


class FakeRun:
    root = ROOT

    def __init__(self, observations):
        self.observations = observations

    def observation(self, case, side):
        return copy.deepcopy(self.observations[(case, side)])


def verdict(case, go, rust, pins, target="historical", run=None):
    from differential import comparable, differences
    diffs = differences(comparable(go), comparable(rust))
    return judge.judge({"name": case}, go, rust, diffs, pins["divergences"], target, pins, run or FakeRun({}), ROOT)


# --- P1: narrow predicates and unconditional invariants -----------------------------------

FILL = {"request-id": UUID, "date": "2026-11-12", "cch": "abcde", "compact-response-id": "resp_ag_compact_" + "1" * 19,
        "compact-item-id": "cmp_ag_compact_" + "2" * 19, "compact-capsule": "cpa-ag-compact-v1:" + "A" * 60,
        "unix-seconds": "1794461308"}


def from_golden(path):
    """An observation that sends exactly a golden's requests, masks filled with sample values."""
    def fill(template):
        return re.sub(r"\{\{mask:([a-z0-9-]+)\}\}", lambda m: FILL[m.group(1)], template)
    golden = json.loads((ROOT / path).read_text())
    replies = []
    for r in golden.get("replies", []):
        body = fill(r["body"]).encode()
        headers = [["Content-Type", "application/json"], ["Content-Length", str(len(body))]]
        head = f"HTTP/1.1 {r['status']} {r['reason']}\r\n" + "".join(f"{k}: {v}\r\n" for k, v in headers) + "\r\n"
        replies.append({"status": r["status"], "reason": r["reason"], "headers": headers, "body_hex": body.hex(),
                        "body_text": body.decode(), "raw_response_hex": (head.encode() + body).hex(),
                        "request_body_hex": ""})
    tls = [dict(h, client_hello_hex="16") for h in golden.get("tls", [])]
    return observation([request(fill(r["body"]).encode(), [[k, fill(v)] for k, v in r["headers"]], r["request_line"])
                        for r in golden.get("requests", [])], replies or None, tls)


def native_pair():
    """native-signals on the selective target: Rust sends the v8.0.10 golden, Go its v8.0.67 one."""
    return (from_golden("harness/expectations/native-signals.v8.0.67.json"),
            from_golden("harness/expectations/native-signals.v8.0.10.json"))


class PredicateTests(unittest.TestCase):
    def test_native_signals_keeps_native_bytes_and_ownership_under_its_divergence(self):
        pins = pins_with(["C2-claude-native-detection"])
        go, rust = native_pair()
        self.assertEqual(verdict("native-signals", go, rust, pins, "selective")["verdict"], "pass")
        for label, edit in [
            ("model", lambda b: b.replace(b'"claude-sonnet-4-6"', b'"claude-opus-4-6"')),
            ("system", lambda b: b.replace(b"Native caller system", b"Other system")),
        ]:
            mutated = copy.deepcopy(rust)
            body = edit(bytes.fromhex(mutated["upstream"]["http"][0]["body_hex"]))
            mutated["upstream"]["http"][0] = request(body, mutated["upstream"]["http"][0]["headers"],
                                                     mutated["upstream"]["http"][0]["request_line"])
            result = verdict("native-signals", go, mutated, pins, "selective")
            self.assertEqual(result["verdict"], "fail", label)
            self.assertIn("invariant:rust", result["failed"], label)
        mutated = copy.deepcopy(rust)
        first = mutated["upstream"]["http"][0]
        headers = [[k, "Bearer sk-xai-FAKE-LOCAL-ONLY" if k == "Authorization" else v] for k, v in first["headers"]]
        mutated["upstream"]["http"][0] = request(bytes.fromhex(first["body_hex"]), headers, first["request_line"])
        result = verdict("native-signals", go, mutated, pins, "selective")
        self.assertEqual(result["verdict"], "fail")
        self.assertTrue(any("credential" in p for p in result["failed"]["invariant:rust"]))

    def test_replaced_comparisons_compare_every_unmasked_byte(self):
        """The 2581e74f review probes: a replacement used to check a few fields and then
        skip whole dimensions."""
        def edit_body(observation, old, new):
            first = observation["upstream"]["http"][0]
            body = bytes.fromhex(first["body_hex"])
            self.assertIn(old.encode(), body)
            out = copy.deepcopy(observation)
            out["upstream"]["http"][0] = request(body.replace(old.encode(), new.encode(), 1), first["headers"], first["request_line"])
            return out
        pins = pins_with(["claude-responses-compact"])
        go = from_golden("harness/expectations/compact-claude.v8.0.10.json")
        rust = from_golden("harness/expectations/compact-claude.v8.0.35.json")
        self.assertEqual(verdict("compact-claude", go, rust, pins)["verdict"], "pass")
        for old, new in [("Local question", "Other question"), ('"max_tokens":64000', '"max_tokens":1')]:
            self.assertEqual(verdict("compact-claude", go, edit_body(rust, old, new), pins)["verdict"], "fail", old)
        changed_reply = copy.deepcopy(rust)
        body = bytes.fromhex(changed_reply["downstream"][0]["body_hex"]).replace(b'"input_tokens":13', b'"input_tokens":14')
        changed_reply["downstream"] = [dict(changed_reply["downstream"][0], body_hex=body.hex(), body_text=body.decode())]
        changed_reply["downstream"][0]["raw_response_hex"] = (bytes.fromhex(rust["downstream"][0]["raw_response_hex"])
                                                             .replace(b'"input_tokens":13', b'"input_tokens":14')).hex()
        self.assertEqual(verdict("compact-claude", go, changed_reply, pins)["verdict"], "fail")
        pins = pins_with(["C2-claude-native-detection"])
        go, rust = native_pair()
        self.assertEqual(verdict("native-signals", go, rust, pins, "selective")["verdict"], "pass")
        self.assertEqual(verdict("native-signals", edit_body(go, "Local question", "Other question"), rust, pins,
                                 "selective")["verdict"], "fail")

    def test_replacement_sides_must_name_complete_goldens(self):
        base = next(d for d in PINS["divergences"] if d["id"] == "claude-responses-compact")
        for side in ({"golden": "harness/expectations/compact-claude.v8.0.35.json", "status": 200},
                     {"inference_requests": 1}, {"golden": "harness/expectations/native-signals.v8.0.10.json"}):
            bad = copy.deepcopy(base)
            bad["expect"][0]["replace"]["rust"] = side
            with self.assertRaises(revisions.ManifestError, msg=side):
                revisions.validate(pins_with() | {"divergences": [bad]})

    def test_tool_alias_restore_rejects_corrupted_tool_arguments(self):
        pins = pins_with(["claude-cloak-tool-alias-restore"])
        go_body = b'{"content":[{"type":"tool_use","name":"mcp__poem_real__leisure_fixture_lookup","input":{"city":"Local"}}]}'
        rust_body = b'{"content":[{"type":"tool_use","name":"fixture_lookup","input":{"city":"Local"}}]}'
        go, rust = observation(replies=[reply(go_body)]), observation(replies=[reply(rust_body)])
        self.assertEqual(verdict("tool-roundtrip", go, rust, pins, "selective")["verdict"], "pass")
        corrupted = observation(replies=[reply(rust_body.replace(b"Local", b"Other"))])
        self.assertEqual(verdict("tool-roundtrip", go, corrupted, pins, "selective")["verdict"], "fail")

    def test_connection_reuse_rejects_a_changed_tls_profile(self):
        pins = pins_with(["P2-claude-connection-reuse"])
        cold = {"client_hello_hex": "16", "record_layout": [[22, 769, 512]], "profile": {"ciphers": [1, 2]}, "negotiated_alpn": "http/1.1",
                "negotiated_version": "TLSv1.3", "session_reused": False}
        resumed = dict(cold, profile={"ciphers": [1, 2], "psk": 1}, session_reused=True)
        run = FakeRun({("tls-reconnect-two-turns", "rust"): observation(tls=[cold, resumed])})
        go, rust = observation(tls=[cold, resumed]), observation(tls=[cold])
        self.assertEqual(verdict("continuity-two-turns", go, rust, pins, "selective", run)["verdict"], "pass")
        changed = observation(tls=[dict(cold, profile={"ciphers": [2]})])
        self.assertEqual(verdict("continuity-two-turns", go, changed, pins, "selective", run)["verdict"], "fail")
        extra_go = observation(tls=[cold, dict(resumed, profile={"ciphers": [9]})])
        self.assertEqual(verdict("continuity-two-turns", extra_go, rust, pins, "selective", run)["verdict"], "fail")

    # client-disconnect (H1 #67): Go v8.0.35 refuses the cancelled-context save, so cliproxy-rs
    # leaves `disabled` absent. v8.0.10 persists false (historical later reference); v8.0.67
    # leaves it absent and both sides must (selective required absence).
    DISABLED_PINS = {"historical": "credential-disabled-persist-v8.0.10", "selective": "credential-disabled-absent"}

    @staticmethod
    def _saved(value, absent):
        credential = {"type": "claude"}
        if value is not absent:
            credential["disabled"] = value
        return observation(credential=credential)

    def test_disabled_after_disconnect_is_exact_on_each_target(self):
        absent = object()
        # Typed states: 0, 0.0, None, "false" and the literal string "<absent>" are not false
        # or absent, and an equal wrong state on both sides is still wrong.
        states = (absent, False, True, 0, 0.0, 1, None, "false", "<absent>", [], {})
        allowed = {"historical": (False, absent), "selective": (absent, absent)}
        for target, pin in self.DISABLED_PINS.items():
            pins = pins_with([pin])
            for go in states:
                for rust in states:
                    ok = go is allowed[target][0] and rust is allowed[target][1]
                    with self.subTest(target=target, go=go, rust=rust):
                        result = verdict("client-disconnect", self._saved(go, absent), self._saved(rust, absent),
                                         pins, target)
                        self.assertEqual(result["verdict"], "pass" if ok else "fail")
                        if ok:
                            self.assertEqual(result["hits"], [pin])
                        else:
                            self.assertIn("predicates", result["failed"])

    def test_disabled_entries_validate_only_exact_states(self):
        pins = pins_with(list(self.DISABLED_PINS.values()))
        revisions.validate(pins)
        entries = {d["id"]: d for d in pins["divergences"]}
        historical = entries[self.DISABLED_PINS["historical"]]
        selective = entries[self.DISABLED_PINS["selective"]]
        self.assertEqual((historical["kind"], historical["targets"], historical["cases"]),
                         ("later-reference", ["historical"], ["client-disconnect"]))
        self.assertEqual(historical["expect"],
                         [{"credential_field": {"field": "disabled", "go": False, "rust": "<absent>"}}])
        self.assertEqual((selective["kind"], selective["targets"], selective["cases"]),
                         ("required-absence", ["selective"], ["client-disconnect"]))
        self.assertEqual(selective["expect"],
                         [{"credential_field": {"field": "disabled", "go": "<absent>", "rust": "<absent>"}}])
        for entry in (historical, selective):
            self.assertNotIn("may_be_absent", entry)
            self.assertIn("091ff27f", entry["evidence"])
            self.assertIn("harness/evidence/attribution-20261112.json.gz", entry["evidence"])
        self.assertNotIn("credential-disabled-persist", {d["id"] for d in PINS["divergences"]})

        def spec(entry):
            return entry["expect"][0]["credential_field"]

        def edited(entry, change):
            entry = copy.deepcopy(entry)
            change(entry)
            return entry

        bad = [
            ("selective go false", edited(selective, lambda e: spec(e).update(go=False))),
            ("selective rust false", edited(selective, lambda e: spec(e).update(rust=False))),
            ("selective both false", edited(selective, lambda e: spec(e).update(go=False, rust=False))),
            ("selective rust 0", edited(selective, lambda e: spec(e).update(rust=0))),
            ("selective alternatives", edited(selective, lambda e: spec(e).update(equal_values=["<absent>"]))),
            ("selective optional", edited(selective, lambda e: e.update(may_be_absent=True))),
            ("selective on historical", edited(selective, lambda e: e.update(targets=["historical"]))),
            ("selective on both", edited(selective, lambda e: e.update(targets=["historical", "selective"]))),
            ("selective without issue", edited(selective, lambda e: e.pop("issue"))),
            ("selective without evidence", edited(selective, lambda e: e.pop("evidence"))),
            ("selective without attribution", edited(selective, lambda e: e.pop("attribution"))),
            ("selective extra predicate", edited(selective, lambda e: e["expect"].append(
                {"downstream_header": {"name": "vary", "go": None, "rust": {"equals": "a"}}}))),
            ("equal absence outside its kind", edited(selective, lambda e: e.update(kind="unowned"))),
            ("historical rust false", edited(historical, lambda e: spec(e).update(rust=False))),
            ("historical go 0", edited(historical, lambda e: spec(e).update(go=0))),
            ("historical on selective", edited(historical, lambda e: e.update(targets=["selective"]))),
        ]
        for label, entry in bad:
            with self.subTest(label), self.assertRaises(revisions.ManifestError):
                revisions.validate(pins_with() | {"divergences": [entry]})

    def test_disabled_mutations_fail_on_each_side(self):
        absent = object()
        baseline = {"historical": (False, absent), "selective": (absent, absent)}
        want = {
            "historical": {("credential persisted disabled: true", "rust"), ("credential persisted disabled: 0", "rust"),
                           ("credential persisted disabled: false again", "rust")},
            "selective": {("credential persisted disabled: true", "rust"), ("credential persisted disabled: 0", "rust"),
                          ("credential persisted disabled: false again", "rust"),
                          ("Go credential persisted disabled: 0", "go"), ("Go credential persisted disabled: false", "go")},
        }
        for target, pin in self.DISABLED_PINS.items():
            pins = pins_with([pin])
            controls = [(label, side, edit) for case, label, side, edit in acceptance.mutations_for(target)
                        if case == "client-disconnect"]
            self.assertEqual({(label, side) for label, side, _ in controls}, want[target])
            go, rust = (self._saved(v, absent) for v in baseline[target])
            self.assertEqual(verdict("client-disconnect", go, rust, pins, target)["verdict"], "pass")
            for label, side, edit in controls:
                g, r = copy.deepcopy(go), copy.deepcopy(rust)
                edit(r if side == "rust" else g)
                with self.subTest(target=target, mutation=label):
                    self.assertEqual(verdict("client-disconnect", g, r, pins, target)["verdict"], "fail")

    def test_a_header_delta_must_be_exactly_the_named_one(self):
        pins = pins_with(["X5-grok-user-agent", "plain-upstream-host-label"])

        def grok(host, agent, extra=()):
            return observation([request(b'{"model":"grok-4.7"}', [["Host", host], ["User-Agent", agent],
                                                                 ["Authorization", "Bearer sk-xai-FAKE-LOCAL-ONLY"], *extra],
                                        "POST /v1/responses HTTP/1.1")])
        go = grok("127.0.0.2:8080", "xai-grok-workspace/1.0.46")
        case = {"name": "xai-chat-stream", "provider": "xai"}

        def run(rust):
            from differential import comparable, differences
            return judge.judge(case, go, rust, differences(comparable(go), comparable(rust)), pins["divergences"],
                               "selective", pins, FakeRun({}), ROOT)["verdict"]
        self.assertEqual(run(grok("127.0.0.3:8080", "Go-http-client/1.1")), "pass")
        self.assertEqual(run(grok("127.0.0.3:8080", "Go-http-client/1.2")), "fail")
        self.assertEqual(run(grok("127.0.0.3:8080", "Go-http-client/1.1", [["X-Extra", "1"]])), "fail")

    def test_created_timestamps_may_only_differ_within_their_skew(self):
        pins = pins_with(["openai-created-timestamps"])

        def chunk(*created):
            body = "".join(f'data: {{"object":"chat.completion.chunk","created":{c}}}\n\n' for c in created).encode()
            return observation(replies=[reply(body, [["Content-Type", "text/event-stream"]])])
        go = chunk(1794453411, 1794453411)
        self.assertEqual(verdict("chat-completions", go, chunk(1794453412, 1794453412), pins)["verdict"], "pass")
        self.assertEqual(verdict("chat-completions", go, chunk(1794457011, 1794457011), pins)["verdict"], "fail")
        self.assertEqual(verdict("chat-completions", go, chunk(1794453412), pins)["verdict"], "fail")
        other_bytes = observation(replies=[reply(
            b'data: {"object":"chat.completion.chunk","created":1794453411,"x":1}\n\n'
            b'data: {"object":"chat.completion.chunk","created":1794453411}\n\n', [["Content-Type", "text/event-stream"]])])
        self.assertEqual(verdict("chat-completions", go, other_bytes, pins)["verdict"], "fail")

    def test_a_missing_delta_fails_instead_of_passing_silently(self):
        pins = pins_with(["I3-unauthenticated-connection-close"])
        same = observation(requests=[])
        self.assertEqual(verdict("auth-missing", same, copy.deepcopy(same), pins, "selective")["verdict"], "fail")

    def test_capture_consistency_is_an_invariant(self):
        rust = observation()
        rust["upstream"]["http"][0]["headers"] = rust["upstream"]["http"][0]["headers"][:-1]
        self.assertIn("invariant:rust", verdict("buffered", observation(), rust, pins_with())["failed"])
        rust = observation()
        rust["downstream"][0]["body_hex"] = b"other".hex()
        self.assertIn("invariant:rust", verdict("buffered", observation(), rust, pins_with())["failed"])

    def test_an_unknown_raw_difference_is_never_explained(self):
        go, rust = observation(), observation()
        rust["downstream"][0]["read_error"] = "IncompleteRead"
        self.assertEqual(verdict("buffered", go, rust, pins_with())["verdict"], "fail")
        rust = observation()
        rust["surprise"] = 1
        self.assertEqual(verdict("buffered", go, rust, pins_with())["verdict"], "fail")


class ManifestTests(unittest.TestCase):
    def test_the_committed_manifest_is_valid(self):
        revisions.load(revisions.suite_sizes())

    def test_path_and_assertion_exemptions_are_refused(self):
        base = {"id": "x", "kind": "nondeterministic", "targets": ["selective"], "cases": ["*"], "reason": "r",
                "evidence": "e", "expect": [{"downstream_header": {"name": "vary", "go": {"equals": "a"}, "rust": None}}]}
        revisions.validate(pins_with() | {"divergences": [base]})
        for extra in ({"paths": [".*"]}, {"assertions": ["upstream-body-bytes"]}, {"upstream_header_names": ["user-agent"]}):
            with self.assertRaises(revisions.ManifestError, msg=extra):
                revisions.validate(pins_with() | {"divergences": [base | extra]})
        for predicate in ({"upstream_header": {"name": "authorization", "go": {"equals": "a"}, "rust": None}},
                          {"upstream_body_edit": {"go": ".*", "rust": None, "count": 1}},
                          {"credential_field": {"field": "access_token", "go": "a", "rust": "b"}},
                          {"downstream_header": {"name": "vary", "go": {"equals": "a"}, "rust": {"equals": "a"}}}):
            with self.assertRaises(revisions.ManifestError, msg=predicate):
                revisions.validate(pins_with() | {"divergences": [base | {"expect": [predicate]}]})

    def test_owner_status_is_required(self):
        base = {"id": "x", "targets": ["selective"], "cases": ["*"], "reason": "r",
                "expect": [{"downstream_header": {"name": "vary", "go": {"equals": "a"}, "rust": None}}]}
        decided = base | {"kind": "not-adopted", "issue": "i", "evidence": "Go c", "attribution": "v8.0.35 run"}
        revisions.validate(pins_with() | {"divergences": [decided]})
        for bad in (base | {"kind": "unowned", "issue": "i"}, base | {"kind": "unattributed", "issue": "i"},
                    base | {"kind": "unclassified", "issue": "i"}, base | {"kind": "pending-feature"},
                    {k: v for k, v in decided.items() if k != "evidence"},
                    {k: v for k, v in decided.items() if k != "attribution"},
                    {k: v for k, v in decided.items() if k != "issue"},
                    decided | {"targets": ["historical", "selective"]}):
            with self.assertRaises(revisions.ManifestError, msg=bad):
                revisions.validate(pins_with() | {"divergences": [bad]})

    def test_a_missing_audit_requires_ownership_and_frozen_reference_evidence(self):
        audit = {"id": "audit", "kind": "missing-audit", "targets": ["selective"], "cases": ["*"],
                 "reason": "Unresolved frozen-reference discrepancy", "issue": "issue67",
                 "evidence": "Go commit", "attribution": "Frozen v8.0.35 differs from Rust",
                 "ledger": "docs/parity-audit/checklist.md#unresolved-frozen-reference-discrepancies",
                 "expect": [{"credential_field": {"field": "disabled", "go": "<absent>", "rust": False,
                                                   "equal_values": ["<absent>", False]}}]}
        revisions.validate(pins_with() | {"divergences": [audit]})
        for field in ("issue", "evidence", "attribution", "ledger"):
            with self.subTest(field=field), self.assertRaises(revisions.ManifestError):
                revisions.validate(pins_with() | {"divergences": [{k: v for k, v in audit.items() if k != field}]})
        with self.assertRaises(revisions.ManifestError):
            revisions.validate(pins_with() | {"divergences": [audit | {"targets": ["historical", "selective"]}]})

    def test_targets_keep_distinct_validated_pins(self):
        for path, value in [(("go", "targets", "historical", "commit"), PINS["go"]["targets"]["selective"]["commit"]),
                            (("go", "targets", "selective", "commit"), "8bba7a77"),
                            (("toolchain", "rust"), "1.98.0"), (("contracts", "codex", "commit"), "0" * 40)]:
            data = copy.deepcopy(PINS)
            node = data
            for key in path[:-1]:
                node = node[key]
            node[path[-1]] = value
            with self.assertRaises(revisions.ManifestError, msg=path):
                revisions.validate(data)


# --- P2: the case universe comes from the fixtures ------------------------------------------

def run_dir(tmp, target="selective", drop=(), duplicate=None, partial=False, select=None, **overrides):
    expected, _ = acceptance.expected_cases(PINS, target)
    names = [n for n in expected if n not in drop]
    selected = select if select is not None else list(names)
    ran = list(selected) + ([duplicate] if duplicate else [])
    spec = PINS["go"]["targets"][target]
    directory = Path(tmp) / "run"
    directory.mkdir()
    for name in set(ran):
        (directory / name).mkdir()
        for part, value in (("fixture", {"name": name}), ("go", observation()), ("rust", observation()), ("diff", [])):
            (directory / name / f"{part}.json").write_text(json.dumps(value))
    summary = {"target": target, "tag": spec["tag"], "reference": spec["commit"], "role": spec["role"],
               "suite": spec["suite"], "suite_cases": names, "selected": selected, "partial": partial,
               "rust_revision": "0" * 40, "cases": [{"name": n, "capture_errors": [], "transport_errors": []} for n in ran]}
    summary.update(overrides)
    (directory / "summary.json").write_text(json.dumps(summary))
    return directory


class UniverseTests(unittest.TestCase):
    def check(self, **kwargs):
        with tempfile.TemporaryDirectory() as tmp:
            directory = run_dir(tmp, **kwargs)
            summary = json.loads((directory / "summary.json").read_text())
            return acceptance.universe(summary, PINS, directory)

    def test_a_case_deleted_from_suite_selection_and_results_fails(self):
        problems, partial, _, _ = self.check(drop=["buffered"])
        self.assertFalse(partial)
        self.assertTrue(any("suite_cases is not the current fixture suite" in p for p in problems), problems)

    def test_duplicates_extras_and_metadata_fail(self):
        self.assertTrue(self.check(duplicate="buffered")[0])
        self.assertTrue(self.check(tag="v8.0.66")[0])
        self.assertTrue(self.check(reference=PINS["go"]["targets"]["historical"]["commit"])[0])
        expected, _ = acceptance.expected_cases(PINS, "selective")
        self.assertTrue(self.check(select=expected + ["invented-case"])[0])

    def test_an_unmarked_subset_fails_and_a_marked_one_is_partial(self):
        expected, _ = acceptance.expected_cases(PINS, "selective")
        self.assertTrue(self.check(select=expected[:3])[0])
        problems, partial, _, unknown = self.check(select=expected[:3], partial=True)
        self.assertEqual((problems, partial, unknown), ([], True, []))
        self.assertTrue(self.check(partial=True)[0], "a complete run cannot claim to be partial")

    def test_evaluate_without_verified_provenance_never_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = run_dir(tmp, target="historical")
            receipt = acceptance.evaluate(directory, pins_with())
        self.assertNotEqual(receipt["verdict"], "pass")
        self.assertTrue(receipt["unavailable"])


# --- P2: combined receipts need full, consistent, verified coverage --------------------------

SOURCE = {"head": "a" * 40, "tree": "b" * 40, "clean": True, "status": [],
          "manifest_sha256": provenance.sha256(ROOT / "harness/manifest.json")}


EXTRA_LISTED = "other_module::unlisted_test"


def execution_record(package, target, index):
    """A clean execution record for one manifest test target, as run_cargo writes it. Its
    executable's bytes are `binary(index)`; receipts() writes them under the artifact root."""
    requested = sorted({t["name"] for g in PINS["groups"] for t in g["tests"]
                        if (t["package"], t["target"]) == (package, target)})
    owned = {(o["package"], o["target"]) for o in PINS["owned_test_targets"]}
    listed = sorted(requested + ([] if (package, target) in owned else [EXTRA_LISTED]))
    artifact = acceptance.expected_artifact(package, target)
    executable = f"target/debug/deps/{artifact['name'].replace('-', '_')}-{index:016x}"
    artifact = dict(artifact, executable=executable, sha256=provenance.hashlib.sha256(binary(index)).hexdigest())
    return {"package": package, "target": target, "artifact": artifact, "fresh_before": False,
            "artifact_after": dict(artifact), "fresh_after": True, "requested": requested,
            "listing": {"command": [executable, "--list"], "cwd": f"crates/{package}", "exit": 0, "timeout": False,
                        "names": listed},
            "execution": {"command": [executable, "--exact", *requested], "cwd": f"crates/{package}",
                          "isolation": "ui/scripts/isolated.sh", "exit": 0, "timeout": False},
            "results": [{"name": n, "outcome": "ok"} for n in reversed(requested)],
            "summary": {"result": "ok", "passed": len(requested), "failed": 0, "ignored": 0, "measured": 0,
                        "filtered_out": len(listed) - len(requested)},
            "summary_lines": 1, "status": "pass", "problems": [], "missing": [], "failed": [], "ignored": [],
            "not_run": [], "output_tail": ""}


def binary(index):
    return f"test executable {index}\n".encode()


def complete_receipts():
    """A complete, consistent receipt set, as harness/accept writes one."""
    def go(target):
        pin = PINS["go"]["targets"][target]["commit"]
        return {"path": f"harness/.cache/go-{pin[:8]}", "sha256": "2" * 64, "vcs.revision": pin, "vcs.modified": "false",
                "checkout_head": pin, "checkout_clean": True}
    driver = {"executable": "harness/rust/target/debug/cpa-differential-driver", "sha256": "1" * 64, "fresh": True}
    items = []
    for target in ("historical", "selective"):
        spec = PINS["go"]["targets"][target]
        expected, _ = acceptance.expected_cases(PINS, target)
        ids = [d["id"] for d in PINS["divergences"] if target in d["targets"]]
        run = f"run-{target}-20261112-000000"
        cases = [{"name": n, "verdict": "pass", "failed": {}, "unexplained": [], "unexplained_count": 0,
                  "differences": 0, "hits": ids if i == 0 else []} for i, n in enumerate(expected)]
        common = {"verdict": "pass", "partial": False, "source": dict(SOURCE), "provenance_verified": True,
                  "target": target, "go_commit": spec["commit"], "run": run, "rust_revision": SOURCE["head"]}
        items.append(common | {
            "kind": "differential", "tag": spec["tag"], "role": spec["role"], "suite": spec["suite"], "cases": cases,
            "named_skips": [], "unknown_skips": [], "stale_divergences": [], "problems": [], "unavailable": [],
            "divergences": {i: {} for i in ids},
            "counts": {"suite": len(expected), "run": len(expected), "passed": len(expected), "failed": 0, "infrastructure": 0,
                       "named_skips": 0, "unknown_skips": 0, "named_divergences": len(ids), "stale_divergences": 0},
            "provenance": {"driver": dict(driver), "go": go(target), "manifest_sha256": SOURCE["manifest_sha256"],
                           "after": {"driver": dict(driver), "go": go(target), "source": dict(SOURCE)},
                           "verified": True, "problems": []}})
        controls = [{"case": c, "mutation": label, "side": side, "baseline": "pass", "verdict": "fail", "caught": True}
                    for c, label, side, _ in acceptance.mutations_for(target)]
        items.append(common | {"kind": "mutation-control", "baseline_verdict": "pass", "controls": controls,
                               "counts": {"mutations": len(controls), "caught": len(controls)}})
    groups, targets = [], {}
    for group in PINS["groups"]:
        tests = [{"package": t["package"], "target": t["target"], "name": t["name"], "paths": t["paths"],
                  "outcome": "passed"} for t in group["tests"]]
        groups.append({"id": group["id"], "provider": group["provider"], "tests": tests, "passed": len(tests), "failed": 0})
        for t in group["tests"]:
            targets[(t["package"], t["target"])] = True
    executions = [execution_record(p, t, i) for i, (p, t) in enumerate(targets)]
    total = sum(len(g["tests"]) for g in PINS["groups"])
    items.append({"kind": "cargo-groups", "verdict": "pass", "partial": False, "source": dict(SOURCE),
                  "source_after": dict(SOURCE), "provenance_verified": True, "rust_revision": SOURCE["head"],
                  "groups": groups, "executions": executions, "unavailable": [], "unknown_tests": [],
                  "counts": {"groups": len(groups), "tests": total, "passed": total, "failed": 0, "unknown_tests": 0}})
    return items


def receipts(tmp, edit=None):
    """Receipt files under tmp, and each cargo executable (before any edit) under tmp/artifacts."""
    items = complete_receipts()
    for i, execution in enumerate(items[4]["executions"]):
        path = Path(tmp) / "artifacts" / execution["artifact"]["executable"]
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(binary(i))
    if edit:
        edit(items)
    paths = []
    for i, item in enumerate(items):
        path = Path(tmp) / f"{i}.json"
        path.write_text(json.dumps(item))
        paths.append(str(path))
    return paths


def current_cargo(items=None):
    """What a successful current-artifact query reports for the unedited fixture: each
    target's artifact as cargo built it (tests replace the query; the CLI never does)."""
    items = items or complete_receipts()
    return {"queried": True, "available": True, "problems": [],
            "artifacts": {f"{e['package']} {e['target']}": dict(e["artifact"]) for e in items[4]["executions"]}}


def _swap_case(r):
    r[0]["cases"][0] = copy.deepcopy(r[0]["cases"][1])


class CombineTests(unittest.TestCase):
    def verdict(self, edit=None, current=None):
        with tempfile.TemporaryDirectory() as tmp:
            return acceptance.combine(receipts(tmp, edit), PINS, current_source=SOURCE, artifact_root=Path(tmp) / "artifacts",
                                      current_targets=current or current_cargo())

    def test_complete_consistent_receipts_pass(self):
        self.assertEqual(self.verdict()["verdict"], "pass", self.verdict()["problems"])

    def test_each_hole_fails(self):
        holes = {
            "dirty worktrees": lambda r: [x["source"].update(clean=False) for x in r],
            "different revisions": lambda r: r[0]["source"].update(head="c" * 40),
            "different manifest": lambda r: r[1]["source"].update(manifest_sha256="0" * 64),
            "wrong Go pin": lambda r: r[0].update(go_commit="0" * 40),
            "modified Go binary": lambda r: r[0]["provenance"]["go"].update({"vcs.modified": "true"}),
            "stale binaries": lambda r: r[4].update(provenance_verified=False),
            "only the controls group": lambda r: r[4].update(groups=[g for g in r[4]["groups"] if g["id"] == "controls"]),
            "missing receipt": lambda r: r.pop(3),
            "duplicate receipt": lambda r: r.append(copy.deepcopy(r[0])),
            "partial run": lambda r: r[2].update(partial=True),
            "mutation on another capture": lambda r: r[1].update(run="run-historical-20261112-000001"),
            "receipt without counts": lambda r: r[0].pop("counts"),
        }
        for label, edit in holes.items():
            self.assertEqual(self.verdict(edit)["verdict"], "fail", label)

    def test_records_are_checked_individually_not_by_totals(self):
        # The 2581e74f review probes: each keeps every aggregate total unchanged.
        holes = {
            "missing case, same count": lambda r: r[0]["cases"].pop(0),
            "duplicate case, same count": _swap_case,
            "extra case, same count": lambda r: r[0]["cases"].append({"name": "unrecognised", "verdict": "pass"}),
            "failed case, same count": lambda r: r[2]["cases"][3].update(verdict="fail", failed={"tls": ["x"]}),
            "all groups empty, same count": lambda r: [g.update(tests=[], passed=0, failed=0) for g in r[4]["groups"]],
            "duplicate test, same count": lambda r: r[4]["groups"][0]["tests"].__setitem__(
                0, copy.deepcopy(r[4]["groups"][0]["tests"][1])),
            "failed test, same count": lambda r: r[4]["groups"][0]["tests"][0].update(outcome="failed"),
            "missing controls, same count": lambda r: r[1].update(controls=[]),
            "duplicate control, same count": lambda r: r[1]["controls"].__setitem__(0, copy.deepcopy(r[1]["controls"][1])),
            "unknown control, same count": lambda r: r[1]["controls"][0].update(mutation="unrecognised"),
            "uncaught control, same count": lambda r: r[3]["controls"][2].update(caught=False),
            "no cargo binary provenance": lambda r: r[4].update(executions=[]),
            "failed execution, same count": lambda r: r[4]["executions"][0]["execution"].update(exit=42),
            "binary changed after the run": lambda r: r[4]["executions"][1]["artifact_after"].update(sha256="4" * 64),
            "missing driver provenance": lambda r: [x["provenance"].pop("driver") for x in r[0:3:2]],
            "driver hash differs after the run": lambda r: r[2]["provenance"]["after"]["driver"].update(sha256="0" * 64),
            "Go binary missing after the run": lambda r: r[0]["provenance"]["after"].update(go=None),
            "different rust_revision field": lambda r: r[2].update(rust_revision="0" * 40),
            "fake counts": lambda r: r[0]["counts"].update(passed=49),
            "stale divergence hidden": lambda r: [c.update(hits=[]) for c in r[2]["cases"]],
            "unknown divergence hit": lambda r: r[0]["cases"][1].update(hits=["invented"]),
            "wrong tag": lambda r: r[0].update(tag="v8.0.11"),
            "malformed group": lambda r: r[4]["groups"].__setitem__(0, "claude-router"),
        }
        for label, edit in holes.items():
            self.assertEqual(self.verdict(edit)["verdict"], "fail", label)

    def test_combine_cli_fails_on_empty_groups(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = receipts(tmp, lambda r: [g.update(tests=[], passed=0, failed=0) for g in r[4]["groups"]])
            done = subprocess.run([sys.executable, str(ROOT / "harness/acceptance.py"), "combine", *paths,
                                   "--receipt", str(Path(tmp) / "out.json")], capture_output=True, text=True)
        self.assertEqual(done.returncode, 1, done.stdout[-500:])

    def test_the_tested_source_must_be_current(self):
        with tempfile.TemporaryDirectory() as tmp:
            receipt = acceptance.combine(receipts(tmp), PINS, current_source=SOURCE | {"head": "d" * 40},
                                         artifact_root=Path(tmp) / "artifacts")
        self.assertEqual(receipt["verdict"], "fail")


class ProvenanceTests(unittest.TestCase):
    def record(self):
        driver = {"executable": "harness/rust/target/debug/d", "fresh": True, "sha256": "1" * 64}
        pin = PINS["go"]["targets"]["historical"]["commit"]
        return {"go_pin": pin, "source": dict(SOURCE), "driver_build": {"exit": 0}, "driver": driver,
                "go": {"sha256": "2" * 64, "vcs.revision": pin, "vcs.modified": "false", "checkout_head": pin,
                       "checkout_clean": True, "path": "harness/.cache/go"}}

    def test_unchanged_identity_verifies(self):
        self.assertEqual(provenance.verify(self.record(), self.record()), [])

    def test_stale_or_changed_artifacts_fail(self):
        cases = {
            "rebuilt after the run": lambda a: a["driver"].update(fresh=False),
            "driver hash changed": lambda a: a["driver"].update(sha256="3" * 64),
            "source changed": lambda a: a["source"].update(head="c" * 40),
            "manifest changed": lambda a: a["source"].update(manifest_sha256="0" * 64),
            "Go binary replaced": lambda a: a["go"].update(sha256="4" * 64),
            "Go stamp modified": lambda a: a["go"].update({"vcs.modified": "true"}),
            "Go checkout dirty": lambda a: a["go"].update(checkout_clean=False),
        }
        for label, edit in cases.items():
            after = self.record()
            edit(after)
            self.assertTrue(provenance.verify(self.record(), after), label)

    def test_package_ids(self):
        self.assertEqual(provenance.package_name("path+file:///x/crates/cpa-server#0.4.1"), "cpa-server")
        self.assertEqual(provenance.package_name("path+file:///x/harness/rust#cpa-differential-driver@0.1.0"),
                         "cpa-differential-driver")


# --- P2: subprocess exits are never ignored ----------------------------------------------

OK = ("running 1 test\ntest a ... ok\n\n"
      "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n")


class ExecutionTests(unittest.TestCase):
    listing = {"exit": 0, "names": {"a"}, "timeout": False}

    def status(self, execution, listing=None):
        return acceptance.judge_execution(["a"], listing or self.listing, execution)["status"]

    def test_clean_run_passes(self):
        self.assertEqual(self.status({"exit": 0, "stdout": OK, "timeout": False}), "pass")

    def test_ok_lines_with_a_nonzero_exit_do_not_pass(self):
        self.assertNotEqual(self.status({"exit": 42, "stdout": OK, "timeout": False}), "pass")

    def test_listing_failures_are_unavailable(self):
        self.assertEqual(self.status({"exit": 0, "stdout": OK, "timeout": False},
                                     {"exit": 3, "names": {"a"}, "timeout": False}), "unavailable")
        self.assertEqual(self.status({"exit": 0, "stdout": OK, "timeout": False},
                                     {"exit": None, "names": set(), "timeout": True}), "unavailable")

    def test_timeouts_signals_and_truncation_do_not_pass(self):
        self.assertEqual(self.status({"exit": None, "stdout": "test a ... ok\n", "timeout": True}), "unavailable")
        self.assertEqual(self.status({"exit": -9, "stdout": OK, "timeout": False}), "unavailable")
        self.assertNotEqual(self.status({"exit": 0, "stdout": "running 1 test\ntest a ... ok\n", "timeout": False}), "pass")
        self.assertNotEqual(self.status({"exit": 0, "stdout": OK.replace("1 passed", "2 passed"), "timeout": False}), "pass")

    def test_real_timeout_output_remains_text_and_is_unavailable(self):
        code = "import sys,time; print('test a ... ok', flush=True); print('partial stderr', file=sys.stderr, flush=True); time.sleep(10)"
        execution = acceptance._run([sys.executable, "-c", code], ROOT, None, 0.5)
        self.assertTrue(execution["timeout"])
        self.assertIsInstance(execution["stdout"], str)
        self.assertIsInstance(execution["stderr"], str)
        self.assertIn("test a ... ok", execution["stdout"])
        self.assertIn("partial stderr", execution["stderr"])
        self.assertEqual(self.status(execution), "unavailable")

    def test_complete_summary_line_is_required(self):
        summary = OK.splitlines()[-1]
        malformed = [
            summary.replace("; finished in 0.01s", "INVALID"),
            summary.replace("; finished in 0.01s", ""),
            summary + "INVALID",
            summary.replace("0.01s", "nans"),
            summary + "\n" + summary.replace("; finished in 0.01s", ""),
        ]
        for line in malformed:
            with self.subTest(summary=line):
                self.assertNotEqual(self.status({"exit": 0, "stdout": OK.replace(summary, line),
                                                "timeout": False}), "pass")

    def test_failed_ignored_and_missing_tests_fail(self):
        self.assertEqual(self.status({"exit": 101, "stdout": OK.replace("a ... ok", "a ... FAILED"), "timeout": False}), "fail")
        self.assertEqual(self.status({"exit": 0, "stdout": OK.replace("a ... ok", "a ... ignored"), "timeout": False}), "fail")
        self.assertEqual(self.status({"exit": 0, "stdout": OK, "timeout": False},
                                     {"exit": 0, "names": set(), "timeout": False}), "fail")


# --- review of 60928bbf -------------------------------------------------------------------

class CargoBindingTests(unittest.TestCase):
    """Finding 1: an execution record must be bound to its target's cargo artifact and carry
    the complete listing, every result line and the summary combine re-derives it from."""

    def verdict(self, edit):
        return CombineTests.verdict(self, edit)

    def test_target_binding_and_result_records_are_required(self):
        def other_target(r):
            first, second = r[4]["executions"][0], r[4]["executions"][1]
            r[4]["executions"][0] = dict(copy.deepcopy(second), package=first["package"], target=first["target"])

        def each(*keys):
            return lambda r: [e.pop(k) for e in r[4]["executions"] for k in keys]

        def first(edit):
            return lambda r: edit(r[4]["executions"][0])

        def owned(edit):
            def apply(r):
                keys = {(o["package"], o["target"]) for o in PINS["owned_test_targets"]}
                edit(next(e for e in r[4]["executions"] if (e["package"], e["target"]) in keys))
            return apply
        holes = {
            "another target's executable, same hash": lambda r: r[4]["executions"][0]["artifact"].update(
                executable=r[4]["executions"][1]["artifact"]["executable"]),
            "another target's whole execution under these labels": other_target,
            "result detail fields removed everywhere": each("missing", "failed", "ignored", "not_run"),
            "nonexistent executable path": first(lambda e: [a.update(executable="target/does-not-exist")
                                                            for a in (e["artifact"], e["artifact_after"])]),
            "well-named executable not on disk": first(lambda e: [
                a.update(executable=a["executable"][:-16] + "f" * 16) for a in (e["artifact"], e["artifact_after"])]),
            "executable bytes differ from the recorded hash": first(lambda e: [
                a.update(sha256="5" * 64) for a in (e["artifact"], e["artifact_after"])]),
            "artifact package differs": first(lambda e: e["artifact"].update(package="cpa-exec")),
            "artifact source file differs": first(lambda e: e["artifact"].update(src_path="crates/x/src/lib.rs")),
            "not a test-profile build": first(lambda e: e["artifact"].update(profile_test=False)),
            "artifact identity missing": first(lambda e: e.pop("artifact")),
            "artifact after the run missing": first(lambda e: e.update(artifact_after=None)),
            "not fresh after the run": first(lambda e: e.update(fresh_after=False)),
            "requested names missing": first(lambda e: e.update(requested=[])),
            "requested names changed": first(lambda e: e.update(requested=e["requested"][:-1])),
            "listing removed": first(lambda e: e.pop("listing")),
            "listing names removed": first(lambda e: e["listing"].update(names=[])),
            "listing of another executable": first(lambda e: e["listing"].update(command=["target/x", "--list"])),
            "listing exit is a boolean": first(lambda e: e["listing"].update(exit=False)),
            "run of other names": first(lambda e: e["execution"].update(command=e["execution"]["command"][:2])),
            "run without --exact": first(lambda e: e["execution"]["command"].__setitem__(1, "--include-ignored")),
            "run exit missing": first(lambda e: e["execution"].pop("exit")),
            "run timed out": first(lambda e: e["execution"].update(timeout=True)),
            "result lines removed": first(lambda e: e.update(results=[])),
            "one result line missing": first(lambda e: e["results"].pop()),
            "a result line duplicated": first(lambda e: e["results"].append(dict(e["results"][0]))),
            "a result line ignored": first(lambda e: e["results"][0].update(outcome="ignored")),
            "an extra result line": first(lambda e: e["results"].append({"name": "extra", "outcome": "ok"})),
            "summary removed": first(lambda e: e.update(summary=None)),
            "summary count is a string": first(lambda e: e["summary"].update(passed=str(e["summary"]["passed"]))),
            "summary filtered count disagrees with the listing": first(lambda e: e["summary"].update(filtered_out=99)),
            "two summary lines": first(lambda e: e.update(summary_lines=2)),
            "unlisted test claimed missing": first(lambda e: e.update(missing=["x"])),
            "output tail on a pass": first(lambda e: e.update(output_tail="x")),
            "unknown test on an owned target": owned(lambda e: [e["listing"].update(names=sorted(e["listing"]["names"] + ["zz_new"])),
                                                                e["summary"].update(filtered_out=1)]),
            "two targets share one executable": lambda r: [r[4]["executions"][1][k].update(
                executable=r[4]["executions"][0]["artifact"]["executable"], sha256=r[4]["executions"][0]["artifact"]["sha256"])
                for k in ("artifact", "artifact_after")],
            "missing rust_revision": lambda r: r[1].pop("rust_revision"),
        }
        self.assertEqual(self.verdict(None)["verdict"], "pass")
        for label, edit in holes.items():
            with self.subTest(label):
                self.assertEqual(self.verdict(edit)["verdict"], "fail", label)

    def test_a_test_label_cannot_pass_without_its_execution(self):
        def edit(r):
            first = r[4]["executions"][0]
            first["results"][0]["outcome"] = "FAILED"
            first["failed"] = [first["results"][0]["name"]]
        result = self.verdict(edit)
        self.assertEqual(result["verdict"], "fail")
        self.assertTrue(any("not every test passed in a verified execution" in p for p in result["problems"]))

    def test_libtest_records_are_parsed_individually(self):
        output = OK.replace("test a ... ok\n", "test a ... ok\ntest b ... FAILED\ntest a ... ok\n")
        self.assertEqual(acceptance.parse_results(output), [{"name": "a", "outcome": "ok"}, {"name": "b", "outcome": "FAILED"},
                                                             {"name": "a", "outcome": "ok"}])
        self.assertEqual(acceptance.parse_summary(OK), ({"result": "ok", "passed": 1, "failed": 0, "ignored": 0, "measured": 0,
                                                         "filtered_out": 0}, 1))
        self.assertEqual(acceptance.parse_summary(OK + OK.splitlines()[-1] + "\n"), (None, 2))
        self.assertEqual(acceptance.parse_summary("running 1 test\n"), (None, 0))


CAPSULE = "cpa-ag-compact-v1:" + "A" * 60


def compaction(created_at=1794463196, capsule=CAPSULE, response_id="resp_ag_compact_" + "1" * 19,
               item_id="cmp_ag_compact_" + "2" * 19, tokens="13"):
    body = ('{"object":"response.compaction","status":"completed","id":%s,"created_at":%s,"output":[{"type":"compaction",'
            '"id":%s,"encrypted_content":%s}],"usage":{"input_tokens":%s}}' % (
                json.dumps(response_id), json.dumps(created_at), json.dumps(item_id), json.dumps(capsule), tokens))
    return observation(requests=[request()], replies=[reply(body.encode())])


class GeneratedValueTests(unittest.TestCase):
    """Finding 2: generated values have an exact JSON type and the shared narrow mask format,
    checked on each side before masking; capsules must decode to a nonce and a tag."""

    def run_case(self, go, rust):
        return verdict("compact-claude", go, rust, pins_with(["compaction-capsule-generated"]), "selective")["verdict"]

    def test_typed_formats_on_either_side(self):
        self.assertEqual(self.run_case(compaction(), compaction(1794463197, "cpa-ag-compact-v1:" + "B" * 60)), "pass")
        bad = {
            "created_at digit string": {"created_at": "1794463196"},
            "created_at boolean": {"created_at": True},
            "created_at float": {"created_at": 1794463196.0},
            "created_at null": {"created_at": None},
            "created_at list": {"created_at": [1794463196]},
            "created_at dict": {"created_at": {"s": 1794463196}},
            "created_at nine digits": {"created_at": 179446319},
            "created_at negative": {"created_at": -1794463196},
            "response id integer": {"response_id": 1},
            "response id short": {"response_id": "resp_ag_compact_1"},
            "item id wrong prefix": {"item_id": "cmp_ag_other_" + "2" * 19},
            "capsule A": {"capsule": "cpa-ag-compact-v1:A"},
            "capsule 41 characters": {"capsule": "cpa-ag-compact-v1:" + "A" * 41},
            "capsule non-canonical trailing bits": {"capsule": "cpa-ag-compact-v1:" + "A" * 41 + "B"},
            "capsule padded": {"capsule": "cpa-ag-compact-v1:" + "A" * 58 + "=="},
            "capsule standard alphabet": {"capsule": "cpa-ag-compact-v1:" + "A" * 58 + "+/"},
            "capsule wrong prefix": {"capsule": "cpa-ag-compact-v2:" + "A" * 60},
            "capsule empty": {"capsule": ""},
            "capsule integer": {"capsule": 1},
        }
        for label, change in bad.items():
            for side in ("go", "rust"):
                with self.subTest(label, side=side):
                    go, rust = compaction(), compaction()
                    if side == "go":
                        go = compaction(**change)
                    else:
                        rust = compaction(**change)
                    self.assertEqual(self.run_case(go, rust), "fail")

    def test_capsule_structure_minimum(self):
        spec = golden.masks(PINS)["compact-capsule"]
        enough = "cpa-ag-compact-v1:" + golden.base64.urlsafe_b64encode(bytes(30)).decode().rstrip("=")
        short = "cpa-ag-compact-v1:" + golden.base64.urlsafe_b64encode(bytes(27)).decode().rstrip("=")
        self.assertIsNone(golden.json_problem(spec, enough))
        self.assertIsNotNone(golden.text_problem(dict(spec, pattern="cpa-ag-compact-v1:[A-Za-z0-9_-]+"), short))
        self.assertIsNotNone(golden.json_problem(golden.masks(PINS)["unix-seconds"], True))

    def test_golden_masks_share_the_capsule_check(self):
        pins = pins_with(["claude-responses-compact"])
        go = from_golden("harness/expectations/compact-claude.v8.0.10.json")
        self.assertEqual(verdict("compact-claude", go, from_golden("harness/expectations/compact-claude.v8.0.35.json"),
                                 pins)["verdict"], "pass")
        saved = FILL["compact-capsule"]
        try:
            FILL["compact-capsule"] = "cpa-ag-compact-v1:" + "A" * 41
            rust = from_golden("harness/expectations/compact-claude.v8.0.35.json")
        finally:
            FILL["compact-capsule"] = saved
        self.assertEqual(verdict("compact-claude", go, rust, pins)["verdict"], "fail")

    def test_manifest_refuses_untyped_or_parallel_formats(self):
        def generated(items):
            pins = pins_with(["compaction-capsule-generated"])
            pins["divergences"][0]["expect"][0]["downstream_json"]["generated"] = items
            return pins
        for items in ([{"pointer": "/id", "regex": "resp_ag_compact_[0-9]+"}],
                      [{"pointer": "/id", "mask": "compact-response-id", "regex": "x"}],
                      [{"pointer": "/id", "mask": "unknown"}], [{"pointer": "/id", "mask": "date"}],
                      [{"pointer": "/id", "mask": "about"}]):
            with self.subTest(items=items), self.assertRaises(revisions.ManifestError):
                revisions.validate(generated(items))
        for name, change in (("unix-seconds", {"json_type": "number"}), ("compact-capsule", {"decode": "other"}),
                             ("cch", {"pattern": "([0-9a-f]{5})"})):
            pins = pins_with()
            pins["masks"][name] = dict(pins["masks"][name], **change)
            with self.subTest(mask=name), self.assertRaises(revisions.ManifestError):
                revisions.validate(pins)


class StrictTypeTests(unittest.TestCase):
    """Python equality treats 0 as false and 13 as 13.0; the type-exact raw difference then
    looked explained. Every structured comparison is type-exact."""

    def test_unreplaced_dimensions_compare_exact_json_types(self):
        go = observation(credential={"type": "claude", "disabled": False})
        for value in (0, 0.0, None, "false", [], {}):
            with self.subTest(credential=value):
                rust = observation(credential={"type": "claude", "disabled": value})
                self.assertEqual(verdict("client-disconnect", go, rust, pins_with())["verdict"], "fail")
        hello = {"client_hello_hex": "16", "record_layout": [[22, 769, 512]], "profile": {"ciphers": [1, 2]},
                 "negotiated_alpn": "http/1.1", "negotiated_version": "TLSv1.3", "session_reused": False}
        rust = observation(tls=[dict(hello, session_reused=0)])
        self.assertEqual(verdict("buffered", observation(tls=[hello]), rust, pins_with())["verdict"], "fail")
        self.assertEqual(GeneratedValueTests.run_case(self, compaction(), compaction(tokens="13.0")), "fail")


class WriteResultTests(unittest.TestCase):
    """Finding 4: upstream write results are part of every golden and a per-side invariant."""

    def compaction_pair(self):
        return (from_golden("harness/expectations/compact-claude.v8.0.10.json"),
                from_golden("harness/expectations/compact-claude.v8.0.35.json"))

    def test_replaced_comparisons_keep_write_results(self):
        pins = pins_with(["claude-responses-compact"])
        go, rust = self.compaction_pair()
        self.assertEqual(verdict("compact-claude", go, rust, pins)["verdict"], "pass")
        for value in ("BrokenPipeError", "ConnectionResetError", "pending", None):
            with self.subTest(historical=value):
                changed = copy.deepcopy(rust)
                changed["upstream"]["http"][0]["write_result"] = value
                self.assertEqual(verdict("compact-claude", go, changed, pins)["verdict"], "fail")
        pins = pins_with(["C2-claude-native-detection"])
        go, rust = native_pair()
        for side in ("go", "rust"):
            with self.subTest(c2=side):
                pair = {"go": copy.deepcopy(go), "rust": copy.deepcopy(rust)}
                pair[side]["upstream"]["http"][0]["write_result"] = "BrokenPipeError"
                self.assertEqual(verdict("native-signals", pair["go"], pair["rust"], pins, "selective")["verdict"], "fail")

    def test_golden_check_alone_covers_missing_extra_and_failed_writes(self):
        data = json.loads((ROOT / "harness/expectations/compact-claude.v8.0.35.json").read_text())
        _, rust = self.compaction_pair()
        masks = golden.masks(PINS)
        self.assertEqual(golden.check(data, {"upstream"}, rust, None, masks), [])
        failed = copy.deepcopy(rust)
        failed["upstream"]["http"][0]["write_result"] = "BrokenPipeError"
        missing = copy.deepcopy(rust)
        del missing["upstream"]["http"][0]["write_result"]
        extra = copy.deepcopy(rust)
        extra["upstream"]["http"].append(copy.deepcopy(extra["upstream"]["http"][0]))
        unrecorded = copy.deepcopy(data)
        del unrecorded["requests"][0]["write_result"]
        for label, (want, have) in {"failed": (data, failed), "missing": (data, missing), "extra": (data, extra),
                                    "golden without write result": (unrecorded, rust)}.items():
            with self.subTest(label):
                self.assertTrue(golden.check(want, {"upstream"}, have, None, masks))

    def test_completion_is_a_per_side_invariant(self):
        pins = pins_with()
        for value in ("pending", "BrokenPipeError", None, 0):
            with self.subTest(buffered=value):
                go, rust = observation(), observation()
                for side in (go, rust):
                    if value is None:
                        del side["upstream"]["http"][0]["write_result"]
                    else:
                        side["upstream"]["http"][0]["write_result"] = value
                result = verdict("buffered", go, rust, pins)
                self.assertEqual(result["verdict"], "fail")
                self.assertIn("invariant:rust", result["failed"])
        # The abandoned stream may fail its upstream write, identically on both sides.
        broken = observation()
        broken["upstream"]["http"][0]["write_result"] = "BrokenPipeError"
        self.assertEqual(verdict("client-disconnect", broken, copy.deepcopy(broken), pins)["verdict"], "pass")
        self.assertEqual(verdict("client-disconnect", broken, observation(), pins)["verdict"], "fail")
        pending = observation()
        pending["upstream"]["http"][0]["write_result"] = "pending"
        self.assertEqual(verdict("client-disconnect", pending, copy.deepcopy(pending), pins)["verdict"], "fail")

    def test_manifest_requires_recorded_write_results(self):
        pins = pins_with()
        del pins["invariants"]["upstream_write_failures"]
        with self.assertRaises(revisions.ManifestError):
            revisions.validate(pins)


# --- evidence-only commits ---------------------------------------------------------------

class EvidenceTests(unittest.TestCase):
    def test_only_evidence_may_change_after_the_tested_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)

            def git(*args):
                return subprocess.check_output(["git", "-C", tmp, *args], env={"GIT_AUTHOR_NAME": "t",
                    "GIT_AUTHOR_EMAIL": "t@example.invalid", "GIT_COMMITTER_NAME": "t",
                    "GIT_COMMITTER_EMAIL": "t@example.invalid", "PATH": "/usr/bin:/bin", "HOME": tmp}).decode().strip()
            git("init", "-q")
            (repo / "harness/evidence").mkdir(parents=True)
            (repo / "harness/acceptance.py").write_text("logic\n")
            git("add", "-A")
            git("commit", "-qm", "source")
            tested = {"head": git("rev-parse", "HEAD"), "tree": git("rev-parse", "HEAD^{tree}")}
            receipt = repo / "harness/evidence/receipt.json"
            receipt.write_text(json.dumps({"verdict": "pass", "tested_source": tested}))
            git("add", "-A")
            git("commit", "-qm", "evidence")
            self.assertEqual(acceptance.verify_evidence(receipt, repo)["verdict"], "pass")
            (repo / "harness/acceptance.py").write_text("changed logic\n")
            git("commit", "-qam", "logic after the run")
            self.assertEqual(acceptance.verify_evidence(receipt, repo)["verdict"], "fail")



def _other_build(package, target, tmp):
    """Re-point one execution at another real, well-named build of the same crate: its bytes
    are on disk and every hash and command in the record agrees with them."""
    def edit(r):
        execution = next(e for e in r[4]["executions"] if (e["package"], e["target"]) == (package, target))
        old = execution["artifact"]["executable"]
        new = old[:-16] + "e" * 16
        data = b"another real build of the same crate\n"
        path = Path(tmp) / "artifacts" / new
        path.write_bytes(data)
        execution.update(json.loads(json.dumps(execution).replace(old, new)))
        for key in ("artifact", "artifact_after"):
            execution[key]["sha256"] = provenance.hashlib.sha256(data).hexdigest()
    return edit


class CurrentArtifactTests(unittest.TestCase):
    """The 1e0aa543 finding: combine accepted a receipt naming another real build of the same
    crate. Each execution must now be the artifact cargo reports current for its target."""

    def combine(self, edit=None, current=None):
        with tempfile.TemporaryDirectory() as tmp:
            paths = receipts(tmp, edit(tmp) if edit else None)
            return acceptance.combine(paths, PINS, current_source=SOURCE, artifact_root=Path(tmp) / "artifacts",
                                      current_targets=current or current_cargo())

    def test_another_real_build_of_the_same_crate_fails(self):
        self.assertEqual(self.combine()["verdict"], "pass")
        for package in ("cpa-core", "cpa-exec"):
            with self.subTest(package):
                result = self.combine(lambda tmp, p=package: _other_build(p, "lib", tmp))
                self.assertEqual(result["verdict"], "fail")
                self.assertEqual(len(result["problems"]), 1, result["problems"])
                self.assertIn(f"{package} lib: recorded artifact", result["problems"][0])
                self.assertIn("is not cargo's current fresh artifact for this target", result["problems"][0])

    def test_a_replay_passes_only_while_its_artifacts_are_current(self):
        def current_is_other(tmp):
            return _other_build("cpa-core", "lib", tmp)
        with tempfile.TemporaryDirectory() as tmp:
            items = complete_receipts()
            paths = receipts(tmp, current_is_other(tmp))
            edited = [json.loads(Path(p).read_text()) for p in paths]
            # Cargo reports exactly what the receipt names: a legitimate replay.
            result = acceptance.combine(paths, PINS, current_source=SOURCE, artifact_root=Path(tmp) / "artifacts",
                                        current_targets=current_cargo(edited))
            self.assertEqual(result["verdict"], "pass", result["problems"])
            # Cargo now reports the original build: the same receipt no longer passes.
            result = acceptance.combine(paths, PINS, current_source=SOURCE, artifact_root=Path(tmp) / "artifacts",
                                        current_targets=current_cargo(items))
            self.assertEqual(result["verdict"], "fail")

    def test_an_unavailable_query_is_never_a_pass(self):
        for current in ({"queried": True, "available": False, "problems": ["cargo query for cpa-core exited 101"],
                         "artifacts": {}},
                        {"queried": True, "available": None, "problems": [], "artifacts": {}}):
            result = self.combine(current=current)
            self.assertEqual(result["verdict"], "unavailable")
            self.assertEqual(acceptance.EXIT[result["verdict"]], 2)
            self.assertTrue(result["unavailable"])

    def test_a_query_result_missing_one_target_fails(self):
        current = current_cargo()
        current["artifacts"].pop(next(iter(current["artifacts"])))
        result = self.combine(current=current)
        self.assertEqual(result["verdict"], "fail")
        self.assertTrue(any("is not cargo's current fresh artifact" in p for p in result["problems"]))

    def test_failing_records_do_not_query_cargo(self):
        with mock.patch.object(acceptance, "current_cargo_targets", side_effect=AssertionError("queried")):
            with tempfile.TemporaryDirectory() as tmp:
                result = acceptance.combine(receipts(tmp, lambda r: r[4].update(partial=True)), PINS,
                                            current_source=SOURCE, artifact_root=Path(tmp) / "artifacts")
        self.assertEqual(result["verdict"], "fail")
        self.assertFalse(result["current_cargo"]["queried"])


class CargoQueryTests(unittest.TestCase):
    """current_cargo_targets with cargo replaced: target-set mismatches, missing or stale
    current artifacts and an unavailable query must each make the query unavailable."""

    def query(self, change=None, sources=None):
        items = complete_receipts()
        calls = []
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for i, execution in enumerate(items[4]["executions"]):
                path = root / execution["artifact"]["executable"]
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(binary(i))

            def cargo(args, env=None, detail=False):
                calls.append((list(args), env))
                package = args[args.index("-p") + 1]
                build = {"command": ["cargo", *args, "--message-format=json"], "exit": 0, "stderr_tail": "", "stale": [],
                         "artifacts": [dict(e["artifact"], fresh=True, profile={"test": True, "opt_level": "0"})
                                       for e in items[4]["executions"] if e["package"] == package]}
                if change:
                    change(package, build, env)
                return build
            source = mock.patch.object(provenance, "source_identity",
                                       side_effect=sources or (lambda: dict(SOURCE)))
            with mock.patch.object(provenance, "cargo_artifacts", side_effect=cargo), source, \
                    mock.patch.object(acceptance, "_toolchain", return_value={"rustc": "rustc test"}):
                result = acceptance.current_cargo_targets(PINS, dict(SOURCE), artifact_root=root)
        return result, calls

    def test_a_clean_query_names_every_target_and_records_how_it_ran(self):
        result, calls = self.query()
        self.assertTrue(result["available"], result["problems"])
        self.assertEqual(set(result["artifacts"]), {f"{p} {t}" for p, t in acceptance.manifest_targets(PINS)})
        self.assertEqual(len(calls), len({p for p, _ in acceptance.manifest_targets(PINS)}))
        for args, env in calls:
            self.assertIn("--offline", args)
            self.assertIn("--locked", args)
            self.assertIn("--no-run", args)
            self.assertEqual(env["RUSTC_WRAPPER"], str(acceptance.NO_COMPILE_RUSTC))
            self.assertTrue(env["CPA_NO_COMPILE_LOG"])
        self.assertEqual(result["environment"]["RUSTC_WRAPPER"], str(acceptance.NO_COMPILE_RUSTC))
        self.assertEqual(sorted(result["commands"]), sorted({p for p, _ in acceptance.manifest_targets(PINS)}))
        self.assertTrue(all(p == {"test": True, "opt_level": "0"} for p in result["profiles"].values()))

    def test_each_unavailable_state(self):
        def first_target(package, build):
            return next(iter(build["artifacts"]), None)

        def only(name, edit):
            return lambda package, build, env: edit(package, build, env) if package == name else None
        states = {
            "a target missing": only("cpa-core", lambda p, b, e: b.update(artifacts=[])),
            "a target duplicated": only("cpa-core", lambda p, b, e: b["artifacts"].append(dict(b["artifacts"][0]))),
            "an extra test executable": only("cpa-server", lambda p, b, e: b["artifacts"].append(
                dict(b["artifacts"][0], name="unlisted", src_path="crates/cpa-server/tests/unlisted.rs",
                     executable="target/debug/deps/unlisted-" + "0" * 16))),
            "a target not fresh": only("cpa-exec", lambda p, b, e: b["artifacts"][0].update(fresh=False)),
            "a dependency not fresh": only("cpa-exec", lambda p, b, e: b.update(stale=["tokio tokio"])),
            "cargo exited nonzero": only("cpa-server", lambda p, b, e: b.update(exit=101)),
            "a compilation was refused": only("cpa-core", lambda p, b, e: Path(e["CPA_NO_COMPILE_LOG"]).write_text(
                "--crate-name cpa_core\n")),
            "another source file": only("cpa-core", lambda p, b, e: b["artifacts"][0].update(src_path="crates/cpa-core/src/x.rs")),
            "not a test profile": only("cpa-core", lambda p, b, e: b["artifacts"][0].update(profile_test=False)),
            "bytes differ from cargo's hash": only("cpa-core", lambda p, b, e: b["artifacts"][0].update(sha256="6" * 64)),
        }
        for label, change in states.items():
            with self.subTest(label):
                result, _ = self.query(change)
                self.assertFalse(result["available"], label)
                self.assertTrue(result["problems"], label)
        changing = iter([dict(SOURCE), dict(SOURCE, head="c" * 40)])
        result, _ = self.query(sources=lambda: next(changing))
        self.assertFalse(result["available"])
        self.assertIn("the source changed during the cargo query or is not the combined source", result["problems"])


class NoCompileWrapperTests(unittest.TestCase):
    """harness/no-compile-rustc passes compiler information through and refuses compilation."""

    def run_wrapper(self, *args, log=True):
        with tempfile.TemporaryDirectory() as tmp:
            rustc = Path(tmp) / "rustc"
            rustc.write_text("#!/bin/sh\necho RAN \"$@\"\n")
            rustc.chmod(0o755)
            env = {"PATH": "/usr/bin:/bin"}
            if log:
                env["CPA_NO_COMPILE_LOG"] = str(Path(tmp) / "refused.log")
            done = subprocess.run([str(acceptance.NO_COMPILE_RUSTC), str(rustc), *args], capture_output=True, text=True,
                                  env=env)
            refused = (Path(tmp) / "refused.log").read_text() if (Path(tmp) / "refused.log").exists() else ""
        return done, refused

    def test_compiler_information_passes(self):
        for args in (["-vV"], ["-", "--crate-name", "___", "--print=file-names", "--crate-type", "bin"]):
            done, refused = self.run_wrapper(*args)
            self.assertEqual((done.returncode, refused), (0, ""))
            self.assertTrue(done.stdout.startswith("RAN"))

    def test_compilation_is_refused_and_logged(self):
        args = ["--crate-name", "cpa_core", "--edition=2024", "crates/cpa-core/src/lib.rs", "--test"]
        done, refused = self.run_wrapper(*args)
        self.assertEqual(done.returncode, 101)
        self.assertNotIn("RAN", done.stdout)
        self.assertIn("--crate-name cpa_core", refused)
        done, _ = self.run_wrapper(*args, log=False)
        self.assertEqual(done.returncode, 101)
        self.assertNotIn("RAN", done.stdout)

    def test_probe_tokens_in_a_compilation_do_not_authorize_it(self):
        compile_args = ["--crate-name", "cpa_core", "--edition=2024", "crates/cpa-core/src/lib.rs", "--test"]
        cases = [compile_args + ["--cfg", "___"], compile_args + ["-vV"],
                 ["--crate-name", "___", "src/lib.rs", "--print=file-names", "--emit=link"],
                 ["-", "--crate-name", "___", "--print=file-names", "--emit=link"],
                 ["-", "--crate-name", "___", "--print=cfg", "-o", "output"],
                 ["-", "--crate-name", "___", "--crate-type", "bin"],
                 ["-", "--crate-name", "___", "--print=cfg", "--crate-type"],
                 ["-", "--crate-name", "___", "--print=cfg", "src/lib.rs"]]
        for args in cases:
            with self.subTest(args=args):
                done, refused = self.run_wrapper(*args)
                self.assertEqual(done.returncode, 101)
                self.assertNotIn("RAN", done.stdout)
                self.assertTrue(refused)


class RecordedArtifactUnavailableTests(unittest.TestCase):
    def test_stat_and_hash_io_errors_are_unavailable_before_query(self):
        for stage in ("stat", "hash"):
            for error in (PermissionError("permission denied"), FileNotFoundError("disappeared"), OSError("read failed")):
                with self.subTest(stage=stage, error=error), tempfile.TemporaryDirectory() as tmp:
                    paths = receipts(tmp)
                    executable = complete_receipts()[4]["executions"][0]["artifact"]["executable"]
                    artifact = Path(tmp) / "artifacts" / executable
                    original_stat, original_hash = Path.is_file, provenance.sha256

                    def stat(path):
                        if stage == "stat" and path == artifact:
                            raise error
                        return original_stat(path)

                    def sha(path):
                        if stage == "hash" and path == artifact:
                            raise error
                        return original_hash(path)

                    with mock.patch.object(Path, "is_file", stat), \
                            mock.patch.object(provenance, "sha256", sha), \
                            mock.patch.object(acceptance, "current_cargo_targets", side_effect=AssertionError("queried")):
                        result = acceptance.combine(paths, PINS, current_source=SOURCE,
                                                    artifact_root=Path(tmp) / "artifacts")
                    self.assertEqual(result["verdict"], "unavailable", result)
                    self.assertFalse(result["problems"], result["problems"])
                    self.assertFalse(result["current_cargo"]["queried"])
                    self.assertTrue(any(executable in p and str(error) in p for p in result["unavailable"]))

    def test_known_record_and_hash_failures_remain_failures(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = receipts(tmp, lambda r: r[4]["executions"][0].update(summary={}))
            with mock.patch.object(provenance, "sha256", side_effect=PermissionError("unreadable")):
                result = acceptance.combine(paths, PINS, current_source=SOURCE, artifact_root=Path(tmp) / "artifacts")
            self.assertEqual(result["verdict"], "fail", result)
            self.assertTrue(any("summary" in p for p in result["problems"]))
            self.assertTrue(result["unavailable"])
            self.assertFalse(result["current_cargo"]["queried"])
        with tempfile.TemporaryDirectory() as tmp:
            paths = receipts(tmp)
            artifact = complete_receipts()[4]["executions"][0]["artifact"]["executable"]
            (Path(tmp) / "artifacts" / artifact).write_bytes(b"changed bytes")
            result = acceptance.combine(paths, PINS, current_source=SOURCE, artifact_root=Path(tmp) / "artifacts")
            self.assertEqual(result["verdict"], "fail", result)
            self.assertTrue(any("recorded SHA-256" in p for p in result["problems"]))
            self.assertFalse(result["unavailable"])

    def test_unreadable_recorded_artifact_cli_writes_an_unavailable_receipt(self):
        # Only the synthetic executable's permissions change. The child uses the real
        # CLI, stat/hash boundary, receipt writer and exit classification.
        with tempfile.TemporaryDirectory() as tmp:
            paths = receipts(tmp)
            executable = complete_receipts()[4]["executions"][0]["artifact"]["executable"]
            (Path(tmp) / "artifacts" / executable).chmod(0)
            out = Path(tmp) / "out.json"
            script = """import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import acceptance, test_acceptance as fixture
acceptance.provenance.source_identity = lambda: dict(fixture.SOURCE)
combine = acceptance.combine
acceptance.combine = lambda paths, pins: combine(paths, pins, current_source=fixture.SOURCE,
                                                artifact_root=Path(sys.argv[2]) / 'artifacts')
sys.exit(acceptance.main(sys.argv[3:]))
"""
            done = subprocess.run([sys.executable, "-c", script, str(ROOT / "harness"), tmp,
                                   "combine", *paths, "--receipt", str(out)], capture_output=True, text=True)
            self.assertEqual(done.returncode, 2, done.stderr)
            result = json.loads(out.read_text())
            self.assertEqual(result["verdict"], "unavailable")
            self.assertFalse(result["problems"], result["problems"])
            self.assertFalse(result["current_cargo"]["queried"])
            self.assertTrue(any(executable in p and "Permission denied" in p for p in result["unavailable"]))
            self.assertNotIn("Traceback", done.stderr)


class CargoQueryUnavailableTests(unittest.TestCase):
    def test_tool_startup_and_artifact_io_errors_are_unavailable(self):
        for error in (FileNotFoundError("cargo missing"), PermissionError("artifact unreadable")):
            with self.subTest(error=error), \
                    mock.patch.object(provenance, "source_identity", return_value=dict(SOURCE)), \
                    mock.patch.object(provenance, "cargo_artifacts", side_effect=error), \
                    mock.patch.object(acceptance, "_toolchain", return_value={}):
                result = acceptance.current_cargo_targets(PINS, dict(SOURCE))
                self.assertFalse(result["available"])
                self.assertTrue(any(str(error) in p for p in result["problems"]))
                self.assertTrue(all(v is None for v in result["exits"].values()))

    def test_missing_cargo_cli_writes_an_unavailable_receipt(self):
        # Exercise the CLI in a new process, with a genuinely absent executable.
        # Only fixture source identity and artifact location are injected; the
        # query, subprocess startup failure, receipt writer and exit are real.
        with tempfile.TemporaryDirectory() as tmp:
            paths = receipts(tmp)
            out = Path(tmp) / "out.json"
            script = """import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import acceptance, test_acceptance as fixture
acceptance.provenance.source_identity = lambda: dict(fixture.SOURCE)
combine = acceptance.combine
acceptance.combine = lambda paths, pins: combine(paths, pins, current_source=fixture.SOURCE,
                                                artifact_root=Path(sys.argv[2]) / 'artifacts')
sys.exit(acceptance.main(sys.argv[3:]))
"""
            env = dict(os.environ, PATH=str(Path(tmp) / "no-executables"))
            done = subprocess.run([sys.executable, "-c", script, str(ROOT / "harness"), tmp,
                                   "combine", *paths, "--receipt", str(out)], env=env,
                                  capture_output=True, text=True)
            self.assertEqual(done.returncode, 2, done.stderr)
            result = json.loads(out.read_text())
            self.assertEqual(result["verdict"], "unavailable")
            self.assertFalse(result["problems"], result["problems"])
            self.assertTrue(result["current_cargo"]["queried"])
            self.assertTrue(any("No such file or directory" in p and "cargo" in p for p in result["unavailable"]))
            self.assertNotIn("Traceback", done.stderr)


if __name__ == "__main__":
    unittest.main()
