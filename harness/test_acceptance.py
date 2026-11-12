"""Regression checks for the acceptance command and the revision manifest (issue #67).

They build small synthetic run directories in the collector's layout, so they need no
Go, Rust build or namespace."""
import copy
import json
from pathlib import Path
import tempfile
import unittest

import acceptance
import manifest as revisions
from differential import comparable, differences


def reply(status=200, body=b'{"type":"message","content":[{"type":"text","text":"local answer"}]}',
          headers=None):
    headers = headers or [["Content-Type", "application/json"], ["Date", "now"]]
    head = f"HTTP/1.1 {status} X\r\n" + "".join(f"{k}: {v}\r\n" for k, v in headers) + "\r\n"
    return {"status": status, "headers": headers, "body_hex": body.hex(),
            "raw_response_hex": (head.encode() + body).hex()}


def request(body=b'{"model":"m","max_tokens":17,"messages":[]}', headers=None):
    headers = headers or [["Authorization", "Bearer sk-ant-oat01-FAKE-LOCAL-ONLY"], ["Host", "api.anthropic.com"]]
    head = "POST /v1/messages?beta=true HTTP/1.1\r\n" + "".join(f"{k}: {v}\r\n" for k, v in headers) + "\r\n"
    return {"request_line": "POST /v1/messages?beta=true HTTP/1.1", "headers": headers,
            "raw_head_hex": head.encode().hex(), "body_hex": body.hex()}


def observation(upstream=None, downstream=None):
    return {"downstream": downstream or [reply()],
            "upstream": {"tls": [], "http": [request()] if upstream is None else upstream, "errors": []},
            "credential_after": {}}


PINS = revisions.load()


def pins(divergences=(), skips=()):
    out = copy.deepcopy(PINS)
    out["divergences"] = list(divergences)
    out["skips"] = list(skips)
    return out


class Run:
    """A run directory in differential.py's layout."""

    def __init__(self, cases, suite=None, target="historical"):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / "run"
        self.path.mkdir()
        names = [name for name, _, _ in cases]
        summary = {"target": target, "tag": "t", "reference": PINS["go"]["targets"][target]["commit"],
                   "role": "r", "suite": "s", "suite_cases": suite or names, "selected": names,
                   "rust_revision": "0" * 40, "rust_worktree_clean": True, "cases": []}
        for name, go, rust in cases:
            directory = self.path / name
            directory.mkdir()
            (directory / "fixture.json").write_text(json.dumps({"name": name}))
            (directory / "go.json").write_text(json.dumps(go))
            (directory / "rust.json").write_text(json.dumps(rust))
            (directory / "diff.json").write_text(json.dumps(differences(comparable(go), comparable(rust))))
            summary["cases"].append({"name": name, "capture_errors": [], "transport_errors": []})
        (self.path / "summary.json").write_text(json.dumps(summary))

    def __enter__(self):
        return self.path

    def __exit__(self, *exc):
        self.tmp.cleanup()


def case_verdict(receipt, name):
    return next(c for c in receipt["cases"] if c["name"] == name)


class EvaluateTests(unittest.TestCase):
    def test_identical_sides_pass(self):
        with Run([("buffered", observation(), observation())]) as path:
            receipt = acceptance.evaluate(path, pins())
        self.assertEqual(receipt["verdict"], "pass")
        self.assertEqual(receipt["counts"]["passed"], 1)

    def test_an_unnamed_body_difference_fails(self):
        rust = observation(upstream=[request(b'{"model":"m","max_tokens":18,"messages":[]}')])
        with Run([("buffered", observation(), rust)]) as path:
            receipt = acceptance.evaluate(path, pins())
        self.assertEqual(receipt["verdict"], "fail")
        failed = [a["name"] for a in case_verdict(receipt, "buffered")["failed_assertions"]]
        self.assertIn("upstream-body-bytes", failed)
        self.assertIn("upstream-body-meaning", failed)

    def test_a_named_divergence_covers_only_its_target_cases_and_fields(self):
        divergence = {"id": "d", "kind": "nondeterministic", "targets": ["historical"], "cases": ["buffered"],
                      "paths": [r"/downstream/\d+/headers(/.*)?", r"/downstream/\d+/raw_response_hex"],
                      "header_names": ["x-new"], "reason": "r"}
        go = observation()
        rust = observation(downstream=[reply(headers=[["Content-Type", "application/json"], ["X-New", "1"],
                                                      ["Date", "now"]])])
        with Run([("buffered", go, rust), ("other", go, rust)]) as path:
            receipt = acceptance.evaluate(path, pins([divergence]))
        self.assertEqual(case_verdict(receipt, "buffered")["verdict"], "pass")
        self.assertEqual(case_verdict(receipt, "other")["verdict"], "fail")
        self.assertEqual(receipt["divergences"]["d"]["differences"] > 0, True)
        # Another header on the same named case is still a failure.
        rust = observation(downstream=[reply(headers=[["Content-Type", "text/plain"], ["X-New", "1"],
                                                      ["Date", "now"]])])
        with Run([("buffered", go, rust)]) as path:
            receipt = acceptance.evaluate(path, pins([divergence]))
        self.assertEqual(receipt["verdict"], "fail")

    def test_header_case_and_order_alone_need_only_the_framing_divergence(self):
        framing = {"id": "framing", "kind": "documented-difference", "targets": ["historical"], "cases": ["*"],
                   "paths": [r"/downstream/\d+/headers(/.*)?", r"/downstream/\d+/raw_response_hex"],
                   "reason": "r", "ledger": "l"}
        rust = observation(downstream=[reply(headers=[["date", "now"], ["content-type", "application/json"]])])
        with Run([("buffered", observation(), rust)]) as path:
            self.assertEqual(acceptance.evaluate(path, pins())["verdict"], "fail")
            self.assertEqual(acceptance.evaluate(path, pins([framing]))["verdict"], "pass")

    def test_an_unknown_skip_fails_and_a_named_one_does_not(self):
        with Run([("buffered", observation(), observation())], suite=["buffered", "missing"]) as path:
            self.assertEqual(acceptance.evaluate(path, pins())["unknown_skips"], ["missing"])
            self.assertEqual(acceptance.evaluate(path, pins())["verdict"], "fail")
            skip = {"case": "missing", "targets": ["historical"], "reason": "r", "issue": "i"}
            receipt = acceptance.evaluate(path, pins(skips=[skip]))
        self.assertEqual(receipt["verdict"], "pass")
        self.assertEqual(receipt["named_skips"], ["missing"])

    def test_a_stale_divergence_fails_a_complete_run(self):
        stale = {"id": "s", "kind": "nondeterministic", "targets": ["historical"], "cases": ["*"],
                 "paths": ["/never"], "reason": "r"}
        with Run([("buffered", observation(), observation())]) as path:
            self.assertEqual(acceptance.evaluate(path, pins([stale]))["stale_divergences"], ["s"])
            self.assertEqual(acceptance.evaluate(path, pins([stale | {"may_be_absent": True}]))["verdict"], "pass")

    def test_no_divergence_excuses_a_credential_leak(self):
        leak = request(headers=[["Authorization", "Bearer sk-ant-oat01-FAKE-LOCAL-ONLY"],
                                ["X-Leak", "fixture-client-key"]])
        everything = {"id": "all", "kind": "unclassified", "issue": "i", "targets": ["historical"], "cases": ["*"],
                      "paths": [".*"], "upstream_header_names": ["x-leak"], "reason": "r",
                      "assertions": sorted(revisions.ASSERTIONS)}
        both = observation(upstream=[leak])
        with Run([("buffered", both, copy.deepcopy(both))]) as path:
            receipt = acceptance.evaluate(path, pins([everything]))
        self.assertEqual(receipt["verdict"], "fail")
        self.assertEqual([a["name"] for a in case_verdict(receipt, "buffered")["failed_assertions"]],
                         ["credential-isolation"])
        with self.assertRaises(revisions.ManifestError):
            revisions.validate(pins([everything | {"assertions": ["credential-isolation"]}]))

    def test_a_refused_client_must_never_reach_the_provider(self):
        both = observation()
        with Run([("auth-missing", both, copy.deepcopy(both))]) as path:
            receipt = acceptance.evaluate(path, pins())
        self.assertEqual([a["name"] for a in case_verdict(receipt, "auth-missing")["failed_assertions"]],
                         ["upstream-traffic"])

    def test_capture_failures_are_unavailable_not_passes(self):
        with Run([("buffered", observation(), observation())]) as path:
            summary = json.loads((path / "summary.json").read_text())
            summary["cases"][0]["capture_errors"] = ["rust: fixture did not reach the inference upstream"]
            (path / "summary.json").write_text(json.dumps(summary))
            self.assertEqual(acceptance.evaluate(path, pins())["verdict"], "unavailable")

    def test_a_run_against_another_revision_fails(self):
        with Run([("buffered", observation(), observation())]) as path:
            summary = json.loads((path / "summary.json").read_text())
            summary["reference"] = PINS["go"]["targets"]["selective"]["commit"]
            (path / "summary.json").write_text(json.dumps(summary))
            self.assertEqual(acceptance.evaluate(path, pins())["verdict"], "fail")


class CargoTests(unittest.TestCase):
    OUTPUT = ("running 3 tests\ntest a ... ok\ntest b ... FAILED\ntest c ... ignored\n"
              "test result: FAILED. 1 passed; 1 failed; 1 ignored\n")

    def test_missing_failed_and_ignored_tests_fail_the_group(self):
        group = {"tests": [{"package": "p", "target": "lib", "name": n, "paths": []} for n in "abcd"]}
        listed = {("p", "lib"): {"a", "b", "c"}}
        outcomes = [t["outcome"] for t in acceptance.judge_group(
            group, {("p", "lib"): acceptance.parse_libtest(self.OUTPUT)}, listed)]
        self.assertEqual(outcomes, ["passed", "failed", "ignored", "missing"])

    def test_list_output(self):
        self.assertEqual(acceptance.parse_list("a::b: test\nc: test\nd: benchmark\n\n2 tests"), {"a::b", "c"})


class CombineTests(unittest.TestCase):
    def test_every_required_receipt_must_be_present(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "r.json"
            path.write_text(json.dumps({"kind": "differential", "target": "historical", "verdict": "pass"}))
            receipt = acceptance.combine([str(path)], PINS)
        self.assertEqual(receipt["verdict"], "fail")
        self.assertIn("cargo-groups", receipt["missing_receipts"])


class ManifestTests(unittest.TestCase):
    def test_the_committed_manifest_is_valid_and_matches_the_fixtures(self):
        revisions.load(revisions.suite_sizes())

    def test_targets_keep_distinct_pins(self):
        data = copy.deepcopy(PINS)
        data["go"]["targets"]["historical"]["commit"] = data["go"]["targets"]["selective"]["commit"]
        with self.assertRaises(revisions.ManifestError):
            revisions.validate(data)
        data = copy.deepcopy(PINS)
        data["go"]["targets"]["policy"]["commit"] = data["go"]["targets"]["selective"]["commit"][:-1] + "0"
        with self.assertRaises(revisions.ManifestError):
            revisions.validate(data)
        data = copy.deepcopy(PINS)
        data["go"]["targets"]["selective"]["commit"] = "8bba7a77"
        with self.assertRaises(revisions.ManifestError):
            revisions.validate(data)

    def test_mirrored_pins_must_agree_with_their_sources(self):
        for path, value in [(("toolchain", "rust"), "1.98.0"), (("contracts", "codex", "commit"), "0" * 40)]:
            data = copy.deepcopy(PINS)
            node = data
            for key in path[:-1]:
                node = node[key]
            node[path[-1]] = value
            with self.assertRaises(revisions.ManifestError, msg=path):
                revisions.validate(data)

    def test_divergences_name_their_cause(self):
        base = {"id": "x", "targets": ["selective"], "cases": ["*"], "paths": ["/a"], "reason": "r"}
        for bad in [base | {"kind": "pending-feature"}, base | {"kind": "documented-difference"},
                    base | {"kind": "later-reference", "targets": ["selective"], "evidence": "e"},
                    base | {"kind": "nondeterministic", "targets": ["policy"]},
                    base | {"kind": "nondeterministic", "surprise": 1}]:
            with self.assertRaises(revisions.ManifestError, msg=bad):
                revisions.validate(pins([bad]))
        revisions.validate(pins([base | {"kind": "pending-feature", "issue": "i"}]))

    def test_suite_sizes_must_match(self):
        with self.assertRaises(revisions.ManifestError):
            revisions.load({"claude-differential": 49})


if __name__ == "__main__":
    unittest.main()
