"""Negative regressions for the acceptance command (issue #67, H1 review of f4d8e222).

Each test reproduces a hole the review found and must fail on that revision's harness.
They build synthetic captures in the collector's layout and need no Go, Rust build or
namespace."""
import copy
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

import acceptance
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
    out["divergences"] = [d for d in PINS["divergences"] if d["id"] in ids]
    out["skips"] = list(skips)
    return out


class FakeRun:
    def __init__(self, observations):
        self.observations = observations

    def observation(self, case, side):
        return copy.deepcopy(self.observations[(case, side)])


def verdict(case, go, rust, pins, target="historical", run=None):
    from differential import comparable, differences
    diffs = differences(comparable(go), comparable(rust))
    return judge.judge({"name": case}, go, rust, diffs, pins["divergences"], target, pins, run or FakeRun({}), ROOT)


# --- P1: narrow predicates and unconditional invariants -----------------------------------

def native_pair():
    """native-signals on the selective target: Rust sends the recorded golden, Go cloaks."""
    golden = json.loads((ROOT / "harness/expectations/native-signals.upstream.json").read_text())["requests"][0]
    headers = [[k, UUID if v == "<generated>" else v] for k, v in golden["headers"]]
    rust = observation([request(bytes.fromhex(golden["body_hex"]), headers, golden["request_line"])])
    cloaked = (b'{"model":"claude-sonnet-4-6","system":[{"type":"text","text":"x-anthropic-billing-header: '
               b'cc_version=2.1.321.4f1; cc_entrypoint=cli; cch=abcde;"}],"messages":[]}')
    go_headers = [["Authorization", CLAUDE_AUTH], ["User-Agent", "claude-cli/2.1.321 (external, cli)"],
                  ["X-Claude-Code-Session-Id", UUID], ["Host", "api.anthropic.com"]]
    go = observation([request(cloaked, go_headers)])
    return go, rust


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

    def test_disabled_persist_rejects_disabled_true(self):
        pins = pins_with(["credential-disabled-persist"])
        go = observation(credential={"type": "claude"})
        self.assertEqual(verdict("client-disconnect", go, observation(credential={"type": "claude", "disabled": False}),
                                 pins, "selective")["verdict"], "pass")
        self.assertEqual(verdict("client-disconnect", go, observation(credential={"type": "claude", "disabled": True}),
                                 pins, "selective")["verdict"], "fail")

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
        for bad in (base | {"kind": "unowned", "issue": "i"}, base | {"kind": "unattributed", "issue": "i"},
                    base | {"kind": "unclassified", "issue": "i"}, base | {"kind": "pending-feature"}):
            with self.assertRaises(revisions.ManifestError, msg=bad):
                revisions.validate(pins_with() | {"divergences": [bad]})

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


def receipts(tmp, edit=None):
    go = lambda t: {"vcs.revision": PINS["go"]["targets"][t]["commit"], "vcs.modified": "false"}
    total = sum(len(g["tests"]) for g in PINS["groups"])
    items = []
    for target in ("historical", "selective"):
        expected, _ = acceptance.expected_cases(PINS, target)
        common = {"verdict": "pass", "partial": False, "source": dict(SOURCE), "provenance_verified": True,
                  "target": target, "go_commit": PINS["go"]["targets"][target]["commit"], "run": f"run-{target}",
                  "rust_revision": SOURCE["head"]}
        items.append(common | {"kind": "differential", "provenance": {"go": go(target)},
                               "counts": {"run": len(expected), "named_skips": 0}})
        want = len([m for m in acceptance.MUTATIONS if m[0] in ("*", target)])
        items.append(common | {"kind": "mutation-control", "counts": {"mutations": want, "caught": want}})
    items.append({"kind": "cargo-groups", "verdict": "pass", "partial": False, "source": dict(SOURCE),
                  "provenance_verified": True, "groups": [{"id": g["id"]} for g in PINS["groups"]],
                  "counts": {"tests": total, "passed": total}})
    if edit:
        edit(items)
    paths = []
    for i, item in enumerate(items):
        path = Path(tmp) / f"{i}.json"
        path.write_text(json.dumps(item))
        paths.append(str(path))
    return paths


class CombineTests(unittest.TestCase):
    def verdict(self, edit=None):
        with tempfile.TemporaryDirectory() as tmp:
            return acceptance.combine(receipts(tmp, edit), PINS, current_source=SOURCE)

    def test_complete_consistent_receipts_pass(self):
        self.assertEqual(self.verdict()["verdict"], "pass")

    def test_each_hole_fails(self):
        holes = {
            "dirty worktrees": lambda r: [x["source"].update(clean=False) for x in r],
            "different revisions": lambda r: r[0]["source"].update(head="c" * 40),
            "different manifest": lambda r: r[1]["source"].update(manifest_sha256="0" * 64),
            "wrong Go pin": lambda r: r[0].update(go_commit="0" * 40),
            "modified Go binary": lambda r: r[0]["provenance"]["go"].update({"vcs.modified": "true"}),
            "stale binaries": lambda r: r[4].update(provenance_verified=False),
            "only the controls group": lambda r: r[4].update(groups=[{"id": "controls"}],
                                                             counts={"tests": 1, "passed": 1}),
            "missing receipt": lambda r: r.pop(3),
            "duplicate receipt": lambda r: r.append(copy.deepcopy(r[0])),
            "partial run": lambda r: r[2].update(partial=True),
            "incomplete case coverage": lambda r: r[2]["counts"].update(run=61),
            "mutation on another capture": lambda r: r[1].update(run="run-other"),
            "too few mutations": lambda r: r[3]["counts"].update(mutations=1, caught=1),
            "receipt without counts": lambda r: r[0].pop("counts"),
        }
        for label, edit in holes.items():
            self.assertEqual(self.verdict(edit)["verdict"], "fail", label)

    def test_the_tested_source_must_be_current(self):
        with tempfile.TemporaryDirectory() as tmp:
            receipt = acceptance.combine(receipts(tmp), PINS, current_source=SOURCE | {"head": "d" * 40})
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

    def test_failed_ignored_and_missing_tests_fail(self):
        self.assertEqual(self.status({"exit": 101, "stdout": OK.replace("a ... ok", "a ... FAILED"), "timeout": False}), "fail")
        self.assertEqual(self.status({"exit": 0, "stdout": OK.replace("a ... ok", "a ... ignored"), "timeout": False}), "fail")
        self.assertEqual(self.status({"exit": 0, "stdout": OK, "timeout": False},
                                     {"exit": 0, "names": set(), "timeout": False}), "fail")


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


if __name__ == "__main__":
    unittest.main()
