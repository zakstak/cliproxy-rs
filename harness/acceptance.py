"""Acceptance over harness evidence (issue #67, H1).

The collector (differential.py) records; this command decides. Every command writes a
receipt under harness/.cache/receipts/.

    acceptance.py evaluate RUN_DIR           # one differential run (harness/judge.py)
    acceptance.py cargo                      # every manifest cargo group, isolated
    acceptance.py mutation-control RUN_DIR   # evaluate must fail on each real deviation
    acceptance.py combine RECEIPT ...        # one verdict; requires full, consistent coverage
    acceptance.py verify-evidence RECEIPT    # HEAD differs from the tested source only in evidence

Verdicts and exit codes:
- pass (0): accepted.
- fail (1): an invariant, comparison, named delta or test failed; a case or test is missing,
  extra or duplicated; metadata disagrees.
- unavailable (2): execution or provenance could not be established.
- partial-pass (3): an explicitly partial run whose cases passed. It is never acceptance.
"""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time

import judge
import manifest as revisions
import provenance
from differential import comparable, differences, suite

ROOT = revisions.ROOT
CACHE = ROOT / "harness/.cache"
EXIT = {"pass": 0, "fail": 1, "unavailable": 2, "partial-pass": 3}
TEST_TIMEOUT = 1800
LIST_TIMEOUT = 300


# --- differential runs -----------------------------------------------------------------

def expected_cases(pins, target):
    """The case names the current fixtures define for a target's suite, checked unique."""
    names = [case["name"] for case in suite(pins["go"]["targets"][target]["suite"])]
    duplicates = sorted({n for n in names if names.count(n) > 1})
    return names, duplicates


def universe(summary, pins, run_dir):
    """Problems with the run's case set and metadata, plus whether it was partial."""
    problems = []
    target = summary.get("target")
    if target not in pins["go"]["targets"] or not pins["go"]["targets"][target]["executed"]:
        return [f"unknown or unexecuted target {target!r}"], False, [], []
    spec = pins["go"]["targets"][target]
    for key, want in (("reference", spec["commit"]), ("tag", spec["tag"]), ("role", spec["role"]),
                      ("suite", spec["suite"])):
        if summary.get(key) != want:
            problems.append(f"summary {key} {summary.get(key)!r} is not the manifest's {want!r}")
    expected, duplicates = expected_cases(pins, target)
    if duplicates:
        problems.append(f"fixtures define duplicate cases {duplicates}")
    if len(expected) != pins["suites"][spec["suite"]]["count"]:
        problems.append(f"fixtures define {len(expected)} cases, manifest expects {pins['suites'][spec['suite']]['count']}")
    if summary.get("suite_cases") != expected:
        problems.append("summary suite_cases is not the current fixture suite")
    selected = summary.get("selected", [])
    ran = [c["name"] for c in summary.get("cases", [])]
    for label, names in (("selected", selected), ("ran", ran)):
        if len(names) != len(set(names)):
            problems.append(f"{label} cases repeat")
        extra = sorted(set(names) - set(expected))
        if extra:
            problems.append(f"{label} cases outside the suite: {extra}")
    if ran != [n for n in expected if n in set(selected)]:
        problems.append("cases that ran differ from the selection")
    for name in ran:
        if not all((run_dir / name / f"{part}.json").is_file() for part in ("fixture", "go", "rust", "diff")):
            problems.append(f"{name}: capture files missing")
    skips = {s["case"] for s in pins.get("skips", []) if target in s["targets"]}
    complete = [n for n in expected if n not in skips]
    partial = summary.get("partial") is True
    if not partial and selected != complete:
        problems.append("a run not marked partial did not select the complete suite")
    if partial and selected == complete:
        problems.append("a run marked partial selected the complete suite")
    named = sorted(set(expected) - set(ran) & skips)
    unknown = [] if partial else sorted(set(expected) - set(ran) - skips)
    return problems, partial, named, unknown


def evaluate(run_dir, pins=None, check_provenance=True):
    pins = pins or revisions.load(revisions.suite_sizes())
    summary = json.loads((run_dir / "summary.json").read_text())
    problems, partial, named_skips, unknown_skips = universe(summary, pins, run_dir)
    target = summary.get("target")
    record = summary.get("provenance") or {}
    unavailable = []
    if check_provenance:
        if record.get("verified") is not True:
            unavailable.append("provenance missing or unverified: " + "; ".join(record.get("problems", [])))
        elif record["before"]["source"]["head"] != summary.get("rust_revision"):
            problems.append("summary rust_revision is not the source provenance recorded")
    cases, hits = [], set()
    run = judge.Run(run_dir)
    if not problems:
        for result in summary["cases"]:
            name = result["name"]
            directory = run_dir / name
            fixture = json.loads((directory / "fixture.json").read_text())
            if fixture.get("name") != name:
                problems.append(f"{name}: fixture.json names {fixture.get('name')!r}")
                continue
            errors = result.get("capture_errors", []) + result.get("transport_errors", [])
            verdict = judge.judge(fixture, run.observation(name, "go"), run.observation(name, "rust"),
                                  json.loads((directory / "diff.json").read_text()), pins["divergences"],
                                  target, pins, run, ROOT)
            if errors:
                verdict["verdict"] = "infrastructure"
                verdict["infrastructure"] = errors
            hits |= set(verdict["hits"])
            cases.append(verdict)
    stale = []
    if not partial and not problems:
        for divergence in pins["divergences"]:
            if target in divergence["targets"] and divergence["id"] not in hits \
                    and not divergence.get("may_be_absent"):
                stale.append(divergence["id"])
    counts = {"suite": len(summary.get("suite_cases", [])), "run": len(cases),
              "passed": sum(c["verdict"] == "pass" for c in cases),
              "failed": sum(c["verdict"] == "fail" for c in cases),
              "infrastructure": sum(c["verdict"] == "infrastructure" for c in cases),
              "named_skips": len(named_skips), "unknown_skips": len(unknown_skips),
              "named_divergences": len(hits), "stale_divergences": len(stale)}
    if problems or unknown_skips or stale or counts["failed"]:
        verdict = "fail"
    elif unavailable or counts["infrastructure"] or summary.get("infrastructure_failed"):
        verdict = "unavailable"
    elif partial:
        verdict = "partial-pass"
    else:
        verdict = "pass"
    divergences = {d["id"]: d for d in pins["divergences"]}
    return {"kind": "differential", "verdict": verdict, "partial": partial, "target": target,
            "tag": summary.get("tag"), "go_commit": summary.get("reference"), "role": summary.get("role"),
            "suite": summary.get("suite"), "run": run_dir.name, "rust_revision": summary.get("rust_revision"),
            "source": (record.get("before") or {}).get("source"), "provenance_verified": record.get("verified"),
            "provenance": _brief_provenance(record), "toolchain": summary.get("toolchain"),
            "isolation": summary.get("isolation"), "counts": counts, "problems": problems,
            "unavailable": unavailable, "named_skips": named_skips, "unknown_skips": unknown_skips,
            "divergences": {k: {f: divergences[k].get(f) for f in ("kind", "issue", "evidence", "attribution", "ledger")
                                if divergences[k].get(f)} for k in sorted(hits)},
            "stale_divergences": stale, "cases": cases}


def _brief_provenance(record):
    if not record.get("before"):
        return None
    before, after = record["before"], record.get("after") or {}
    return {"driver": before.get("driver"), "go": before.get("go"), "manifest_sha256": before["source"]["manifest_sha256"],
            "after": {"driver": after.get("driver"), "go": after.get("go"), "source": after.get("source")},
            "verified": record.get("verified"), "problems": record.get("problems")}


# --- cargo groups ------------------------------------------------------------------------

SUMMARY = re.compile(r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
                     r"(\d+) measured; (\d+) filtered out; finished in \d+(?:\.\d+)?s")


def parse_libtest(output):
    """{test name: ok|FAILED|ignored} from libtest's plain output."""
    results = {}
    for line in output.splitlines():
        match = re.match(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\b", line)
        if match:
            results[match.group(1)] = match.group(2)
    return results


def parse_list(output):
    return {line[:-len(": test")] for line in output.splitlines() if line.endswith(": test")}


def judge_execution(names, listing, execution):
    """Outcome of one test binary run. Never pass unless the process exited 0, printed
    exactly one summary line counting every requested test, and reported each one ok."""
    out = {"listed": listing.get("exit"), "exit": execution.get("exit"), "problems": []}
    if listing.get("timeout") or listing.get("exit") != 0:
        out["problems"].append(f"listing exited {listing.get('exit')}{' (timeout)' if listing.get('timeout') else ''}")
        out["status"] = "unavailable"
        return out
    missing = sorted(set(names) - listing["names"])
    results = parse_libtest(execution.get("stdout", ""))
    summary_lines = [line for line in execution.get("stdout", "").splitlines()
                     if line.startswith("test result:")]
    summaries = [match.groups() for line in summary_lines if (match := SUMMARY.fullmatch(line))]
    failed = sorted(n for n in names if results.get(n) == "FAILED")
    ignored = sorted(n for n in names if results.get(n) == "ignored")
    not_run = sorted(n for n in names if n not in results and n not in missing)
    out.update(missing=missing, failed=failed, ignored=ignored, not_run=not_run)
    if missing:
        out["problems"].append(f"not in the binary: {missing}")
    if failed or ignored:
        out["problems"].append(f"failed {failed} ignored {ignored}")
    if execution.get("timeout") or execution.get("exit") is None or execution["exit"] < 0:
        out["problems"].append("execution timed out or was killed")
    elif execution["exit"] != 0:
        out["problems"].append(f"execution exited {execution['exit']}")
    if len(summaries) != 1 or len(summary_lines) != 1:
        out["problems"].append(f"{len(summaries)} libtest summary lines (truncated or extra output)")
    else:
        result, passed, n_failed, n_ignored = summaries[0][0], int(summaries[0][1]), int(summaries[0][2]), int(summaries[0][3])
        if (result, passed, n_failed, n_ignored) != ("ok", len(set(names)), 0, 0):
            out["problems"].append(f"summary says {result}: {passed} passed, {n_failed} failed, {n_ignored} ignored "
                                   f"for {len(set(names))} requested")
    if not_run:
        out["problems"].append(f"no result line for {not_run}")
    if failed or missing or ignored:
        out["status"] = "fail"
    elif out["problems"]:
        out["status"] = "unavailable" if (execution.get("timeout") or execution.get("exit", 0) < 0
                                          or execution.get("exit") is None) else "fail"
    else:
        out["status"] = "pass"
    return out


def _cargo_selection(package, targets):
    args = ["test", "--locked", "--no-run", "-p", package]
    for target in sorted(targets):
        args += ["--lib"] if target == "lib" else ["--test", target.split(":", 1)[1]]
    return args


def _artifact_for(build, package, target):
    kind, name = ("lib", None) if target == "lib" else ("test", target.split(":", 1)[1])
    found = [a for a in build["artifacts"] if a["package"] == package and a["kind"] == kind
             and (name is None or a["name"] == name)]
    return found[0] if len(found) == 1 else None


def _run(command, cwd, env, timeout):
    try:
        done = subprocess.run(command, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)
        return {"exit": done.returncode, "stdout": done.stdout, "stderr": done.stderr, "timeout": False}
    except subprocess.TimeoutExpired as exc:
        # TimeoutExpired retains bytes even when subprocess.run requested text.
        stdout, stderr = exc.stdout or "", exc.stderr or ""
        if isinstance(stdout, bytes):
            stdout = stdout.decode("utf-8", "replace")
        if isinstance(stderr, bytes):
            stderr = stderr.decode("utf-8", "replace")
        return {"exit": None, "stdout": stdout, "stderr": stderr, "timeout": True}


def run_cargo(pins):
    """Every manifest group. Build, hash, list and run each test binary directly in a
    loopback-only namespace, then confirm cargo still calls the same binaries fresh."""
    targets = {}
    for group in pins["groups"]:
        for test in group["tests"]:
            targets.setdefault((test["package"], test["target"]), []).append(test["name"])
    env = dict(os.environ, CPA_TEST_NO_SKIP="1", CARGO_TERM_COLOR="never")
    source_before = provenance.source_identity()
    packages = sorted({p for p, _ in targets})
    before = {p: provenance.cargo_artifacts(_cargo_selection(p, [t for q, t in targets if q == p]), env=env)
              for p in packages}
    unavailable = [f"build of {p} exited {b['exit']}" for p, b in before.items() if b["exit"] != 0]
    executions = {}
    if not unavailable:
        isolated = ROOT / "ui/scripts/isolated.sh"
        for (package, target), names in sorted(targets.items()):
            artifact = _artifact_for(before[package], package, target)
            if artifact is None:
                unavailable.append(f"{package} {target}: no single test executable")
                continue
            executable = ROOT / artifact["executable"]
            package_dir = ROOT / "crates" / package
            listing = _run([str(executable), "--list"], package_dir, env, LIST_TIMEOUT)
            listing["names"] = parse_list(listing["stdout"])
            shell = "cd " + _quote(str(package_dir)) + " && exec " + " ".join(
                _quote(p) for p in [str(executable), "--exact", *sorted(set(names))])
            execution = _run([str(isolated), shell], ROOT, env, TEST_TIMEOUT)
            outcome = judge_execution(names, listing, execution)
            outcome.update(executable=artifact["executable"], sha256=artifact["sha256"], fresh_before=artifact["fresh"],
                           listed_names=sorted(listing["names"]), output_tail=(execution["stdout"] + execution["stderr"])[-3000:]
                           if outcome["status"] != "pass" else "")
            executions[(package, target)] = outcome
    after = {p: provenance.cargo_artifacts(_cargo_selection(p, [t for q, t in targets if q == p]), env=env)
             for p in packages}
    source_after = provenance.source_identity()
    if source_before != source_after:
        unavailable.append("Rust source or manifest changed during the run")
    for (package, target), outcome in executions.items():
        artifact = _artifact_for(after[package], package, target)
        outcome.update(sha256_after=artifact and artifact["sha256"], fresh_after=artifact and artifact["fresh"])
        if artifact is None or artifact["fresh"] is not True or artifact["sha256"] != outcome["sha256"]:
            unavailable.append(f"{package} {target}: tested binary is not cargo's fresh binary after the run")
    unknown = []
    for owned in pins.get("owned_test_targets", []):
        key = (owned["package"], owned["target"])
        if key in executions:
            known = {t["name"] for g in pins["groups"] for t in g["tests"] if (t["package"], t["target"]) == key}
            unknown += [f"{key[0]} {key[1]} {n}" for n in sorted(set(executions[key]["listed_names"]) - known)]
    groups = []
    for group in pins["groups"]:
        tests = []
        for test in group["tests"]:
            outcome = executions.get((test["package"], test["target"]))
            if outcome is None:
                state = "unavailable"
            elif test["name"] in outcome.get("missing", []):
                state = "missing"
            elif test["name"] in outcome.get("failed", []):
                state = "failed"
            elif test["name"] in outcome.get("ignored", []):
                state = "ignored"
            elif outcome["status"] == "pass":
                state = "passed"
            else:
                state = outcome["status"]
            tests.append({"name": test["name"], "package": test["package"], "target": test["target"],
                          "paths": test["paths"], "outcome": state})
        groups.append({"id": group["id"], "provider": group["provider"], "tests": tests,
                       "passed": sum(t["outcome"] == "passed" for t in tests),
                       "failed": sum(t["outcome"] != "passed" for t in tests)})
    statuses = [o["status"] for o in executions.values()]
    if unknown or "fail" in statuses:
        verdict = "fail"
    elif unavailable or "unavailable" in statuses or len(executions) != len(targets):
        verdict = "unavailable"
    else:
        verdict = "pass"
    return {"kind": "cargo-groups", "verdict": verdict, "partial": False,
            "rust_revision": source_before["head"], "source": source_before, "source_after": source_after,
            "provenance_verified": not unavailable, "toolchain": _toolchain(),
            "isolation": "ui/scripts/isolated.sh: loopback-only user+net namespace, caller uid/gid; "
                         "test binaries run directly with CPA_TEST_NO_SKIP=1",
            "counts": {"groups": len(groups), "tests": sum(len(g["tests"]) for g in groups),
                       "passed": sum(g["passed"] for g in groups), "failed": sum(g["failed"] for g in groups),
                       "unknown_tests": len(unknown)},
            "unavailable": unavailable, "unknown_tests": unknown, "groups": groups,
            "executions": [{"package": p, "target": t, **{k: v for k, v in o.items() if k != "listed_names"}}
                           for (p, t), o in sorted(executions.items())]}


def _quote(part):
    return part if re.fullmatch(r"[A-Za-z0-9_:./=+-]+", part) else "'" + part.replace("'", "'\\''") + "'"


def _toolchain():
    out = {}
    for name, command in (("rustc", ["rustc", "--version"]), ("cargo", ["cargo", "--version"])):
        try:
            out[name] = subprocess.check_output(command).decode().strip()
        except (OSError, subprocess.CalledProcessError) as exc:
            out[name] = f"unavailable: {exc}"
    return out


# --- mutation control --------------------------------------------------------------------

def _inference(observation, provider=None):
    prefix = "POST /v1/responses " if provider else "POST /v1/messages"
    return [r for r in observation["upstream"]["http"] if r["request_line"].startswith(prefix)]


def _set_header(request, name, value):
    headers = [[k, value if k.lower() == name.lower() else v] for k, v in request["headers"]]
    if not any(k.lower() == name.lower() for k, _ in request["headers"]):
        headers.insert(len(headers) - 1, [name, value])
    _rewrite_head(request, headers)


def _rewrite_head(request, headers):
    request["headers"] = headers
    head = request["request_line"] + "\r\n" + "".join(f"{k}: {v}\r\n" for k, v in headers) + "\r\n"
    request["raw_head_hex"] = head.encode("latin1").hex()


def _set_body(request, body):
    request["body_hex"] = body.hex()
    request["body_text"] = body.decode("utf-8", errors="replace")
    headers = [[k, str(len(body)) if k.lower() == "content-length" else v] for k, v in request["headers"]]
    _rewrite_head(request, headers)


def _replace_body(old, new, provider=None):
    def edit(observation):
        request = _inference(observation, provider)[0]
        body = bytes.fromhex(request["body_hex"])
        if old.encode() not in body:
            raise AssertionError(f"mutation target {old!r} not in the request")
        _set_body(request, body.replace(old.encode(), new.encode(), 1))
    return edit


def _replace_reply(old, new):
    def edit(observation):
        reply = observation["downstream"][0]
        body = bytes.fromhex(reply["body_hex"])
        if old.encode() not in body:
            raise AssertionError(f"mutation target {old!r} not in the reply")
        changed = body.replace(old.encode(), new.encode(), 1)
        reply["body_hex"] = changed.hex()
        reply["body_text"] = changed.decode("utf-8", errors="replace")
        raw = bytes.fromhex(reply["raw_response_hex"])
        reply["raw_response_hex"] = raw.replace(old.encode(), new.encode(), 1).hex()
    return edit


def _header(name, value, provider=None):
    def edit(observation):
        _set_header(_inference(observation, provider)[0], name, value)
    return edit


def _status(observation):
    reply = observation["downstream"][0]
    reply["status"] = 500


def _reply_header(name, value):
    def edit(observation):
        reply = observation["downstream"][0]
        reply["headers"] = reply["headers"] + [[name, value]]
        raw = bytes.fromhex(reply["raw_response_hex"])
        head, sep, tail = raw.partition(b"\r\n\r\n")
        reply["raw_response_hex"] = (head + f"\r\n{name}: {value}".encode() + sep + tail).hex()
    return edit


def _tls_cipher(observation):
    observation["upstream"]["tls"][0]["profile"]["ciphers"] = observation["upstream"]["tls"][0]["profile"]["ciphers"][1:]


def _credential(field, value):
    def edit(observation):
        observation["credential_after"][field] = value
    return edit


def _skew_created(observation):
    reply = observation["downstream"][0]
    for key in ("body_hex", "raw_response_hex"):
        data = bytes.fromhex(reply[key])
        data = re.sub(rb'"created":([0-9]{10})', lambda m: b'"created":' + str(int(m.group(1)) + 3600).encode(), data)
        reply[key] = data.hex()
    reply["body_text"] = bytes.fromhex(reply["body_hex"]).decode("utf-8", errors="replace")


# (target, case, label, edit[, side]). Each must turn a passing case into a failure. The
# side is Rust unless named; Go-side edits prove a replaced comparison still pins Go.
MUTATIONS = [
    ("*", "buffered", "upstream max_tokens 17 -> 18", _replace_body('"max_tokens":17', '"max_tokens":18')),
    ("*", "buffered", "downstream status 200 -> 500", _status),
    ("*", "buffered", "client key forwarded upstream", _header("X-Leak", "fixture-client-key")),
    ("*", "buffered", "downstream text changed", _replace_reply("local answer", "other answer")),
    ("*", "native-signals", "native body model changed", _replace_body('"claude-sonnet-4-6"', '"claude-opus-4-6"')),
    ("*", "native-signals", "Grok credential in the Claude Authorization",
     _header("Authorization", "Bearer sk-xai-FAKE-LOCAL-ONLY")),
    ("*", "tool-roundtrip", "tool arguments corrupted", _replace_reply('{"city":"Local"}', '{"city":"Other"}')),
    ("*", "continuity-two-turns", "TLS ClientHello ciphers changed", _tls_cipher),
    ("*", "client-disconnect", "credential persisted disabled: true", _credential("disabled", True)),
    ("*", "upstream-429", "unexpected downstream header", _reply_header("X-Unexpected", "1")),
    ("*", "chat-completions", "created timestamp skewed by an hour", _skew_created),
    ("historical", "compact-claude", "compaction answer shape changed", _replace_reply('"status":"completed"', '"status":"failed"')),
    ("historical", "models-openai", "catalog entry dropped", _replace_reply('"id":"claude-sonnet-4-6"', '"id":"claude-sonnet-4-7"')),
    ("selective", "buffered", "C1 Rust User-Agent drifts", _header("User-Agent", "claude-cli/2.1.281 (external, cli)")),
    ("selective", "cloak-tools-beta", "tool schema changed outside the alias",
     _replace_body('"description":"Local tool"', '"description":"Other tool"')),
    ("selective", "meta-chat-stream", "M1 Rust client id drifts", _header("X-Client-Id", "tbh:other", "meta")),
    ("selective", "xai-chat-stream", "Muse credential sent to Grok", _header("Authorization", "Bearer sk-meta-FAKE-LOCAL-ONLY", "xai")),
    ("selective", "xai-messages-stream", "Rust also sends max_output_tokens",
     _replace_body('"stream":true', '"max_output_tokens":17,"stream":true', "xai")),
    ("selective", "upstream-401", "credential expiry skewed by an hour", _credential("expired", "2099-01-01T00:00:00Z")),
    ("selective", "compact-claude", "compaction capsule prefix changed", _replace_reply("cpa-ag-compact-v1:", "cpa-ag-compact-v2:")),
    ("historical", "compact-claude", "compaction request caller text changed", _replace_body("Local question", "Other question")),
    ("historical", "compact-claude", "compaction request max_tokens 64000 -> 1", _replace_body('"max_tokens":64000', '"max_tokens":1')),
    ("historical", "compact-claude", "Go 501 body changed", _replace_reply("not supported", "unsupported"), "go"),
    ("selective", "native-signals", "C2 Go caller text changed", _replace_body("Local question", "Other question"), "go"),
    ("selective", "native-signals", "C2 Go header changed", _header("X-Stainless-Timeout", "601"), "go"),
]


def mutations_for(target):
    """[(case, label, side, edit)] required on a target, in order."""
    return [(m[1], m[2], m[4] if len(m) > 4 else "rust", m[3]) for m in MUTATIONS if m[0] in ("*", target)]


def mutation_control(run_dir, pins, check_provenance=True):
    summary = json.loads((run_dir / "summary.json").read_text())
    target = summary["target"]
    baseline = evaluate(run_dir, pins, check_provenance)
    by_name = {c["name"]: c for c in baseline["cases"]}
    controls = []
    for name, label, side, edit in mutations_for(target):
        control = {"mutation": label, "case": name, "side": side, "baseline": by_name.get(name, {}).get("verdict")}
        with tempfile.TemporaryDirectory() as scratch:
            copy_dir = Path(scratch) / run_dir.name
            shutil.copytree(run_dir, copy_dir, symlinks=True,
                            ignore=shutil.ignore_patterns("auth", "process.log", "config.yaml", "certs"))
            directory = copy_dir / name
            go = json.loads((directory / "go.json").read_text())
            rust = json.loads((directory / "rust.json").read_text())
            try:
                edit(rust if side == "rust" else go)
            except (AssertionError, IndexError, KeyError) as exc:
                control.update(caught=False, error=f"mutation not applicable: {exc}")
                controls.append(control)
                continue
            (directory / f"{side}.json").write_text(json.dumps(rust if side == "rust" else go))
            (directory / "diff.json").write_text(json.dumps(differences(comparable(go), comparable(rust))))
            receipt = evaluate(copy_dir, pins, check_provenance)
            case = next((c for c in receipt["cases"] if c["name"] == name), None)
            control.update(verdict=case and case["verdict"], failed=sorted((case or {}).get("failed", {})),
                           unexplained=(case or {}).get("unexplained_count"),
                           caught=bool(case) and case["verdict"] == "fail")
        controls.append(control)
    caught = all(c["caught"] and c["baseline"] == "pass" for c in controls)
    return {"kind": "mutation-control", "verdict": "pass" if caught and baseline["verdict"] == "pass" else "fail",
            "partial": False, "run": run_dir.name, "target": target, "go_commit": summary.get("reference"),
            "rust_revision": summary.get("rust_revision"), "source": baseline.get("source"),
            "provenance_verified": baseline.get("provenance_verified"), "baseline_verdict": baseline["verdict"],
            "counts": {"mutations": len(controls), "caught": sum(c["caught"] for c in controls)},
            "controls": controls}


# --- combined acceptance ---------------------------------------------------------------

REQUIRED = [("differential", "historical"), ("differential", "selective"), ("mutation-control", "historical"),
            ("mutation-control", "selective"), ("cargo-groups", None)]
HEX64 = re.compile(r"[0-9a-f]{64}")


def _hash(value):
    return isinstance(value, str) and HEX64.fullmatch(value) is not None


def _check_common(receipt, current, problems):
    """Verdict, completeness and the one clean, current source every receipt must name."""
    if receipt.get("verdict") != "pass" or receipt.get("partial") is not False:
        problems.append(f"verdict {receipt.get('verdict')!r}, partial {receipt.get('partial')!r}")
    if receipt.get("provenance_verified") is not True:
        problems.append("provenance not verified")
    source = receipt.get("source")
    if not isinstance(source, dict) or source.get("clean") is not True:
        problems.append("tested source was not a clean worktree")
    elif source != current:
        problems.append(f"tested source {source.get('head')} is not the current source {current.get('head')} "
                        "(revision, tree, cleanliness or manifest differ)")
    if receipt.get("rust_revision") is not None and receipt.get("rust_revision") != (source or {}).get("head"):
        problems.append("rust_revision is not the recorded source revision")


def _check_go(record, pin, label, problems):
    if not isinstance(record, dict):
        problems.append(f"{label}: Go binary record missing")
        return
    if not _hash(record.get("sha256")) or record.get("vcs.revision") != pin or record.get("vcs.modified") != "false" \
            or record.get("checkout_head") != pin or record.get("checkout_clean") is not True:
        problems.append(f"{label}: Go binary record is not a clean build of the pin {pin}")


def _check_differential(receipt, pins, current, problems):
    target = receipt.get("target")
    spec = pins["go"]["targets"].get(target)
    if not spec or not spec["executed"]:
        problems.append(f"unknown target {target!r}")
        return
    for key, want in (("go_commit", spec["commit"]), ("tag", spec["tag"]), ("role", spec["role"]),
                      ("suite", spec["suite"])):
        if receipt.get(key) != want:
            problems.append(f"{key} {receipt.get(key)!r} is not the manifest's {want!r}")
    if not re.fullmatch(rf"run-{target}-[0-9]{{8}}-[0-9]{{6}}", str(receipt.get("run"))):
        problems.append(f"run {receipt.get('run')!r} is not a {target} capture")
    expected, _ = expected_cases(pins, target)
    skips = [s["case"] for s in pins.get("skips", []) if target in s["targets"]]
    want = [n for n in expected if n not in skips]
    cases = receipt.get("cases") if isinstance(receipt.get("cases"), list) else []
    names = [c.get("name") if isinstance(c, dict) else None for c in cases]
    if names != want:
        missing = sorted(set(want) - set(names))
        extra = sorted(str(n) for n in set(names) - set(want))
        duplicates = sorted({str(n) for n in names if names.count(n) > 1})
        problems.append(f"cases are not the {len(want)} expected once each in order "
                        f"(missing {missing}, extra {extra}, duplicated {duplicates})")
    if receipt.get("named_skips") != sorted(skips) or receipt.get("unknown_skips") != []:
        problems.append("skips are not exactly the manifest's named skips")
    ids = {d["id"] for d in pins["divergences"] if target in d["targets"]}
    hits = set()
    for case in cases:
        if not isinstance(case, dict):
            continue
        if case.get("verdict") != "pass" or case.get("failed") != {} or case.get("unexplained_count") != 0 \
                or case.get("unexplained") != []:
            problems.append(f"case {case.get('name')}: not a clean pass")
        if not isinstance(case.get("hits"), list) or not set(case["hits"]) <= ids:
            problems.append(f"case {case.get('name')}: hits name divergences outside the manifest")
        else:
            hits |= set(case["hits"])
    stale = sorted(d["id"] for d in pins["divergences"]
                   if target in d["targets"] and d["id"] not in hits and not d.get("may_be_absent"))
    if stale or receipt.get("stale_divergences") != []:
        problems.append(f"stale divergences {stale or receipt.get('stale_divergences')}")
    if set(receipt.get("divergences") or {}) != hits:
        problems.append("divergence summary is not the union of the case hits")
    recomputed = {"suite": len(expected), "run": len(cases), "passed": sum(isinstance(c, dict) and c.get("verdict") == "pass"
                                                                          for c in cases),
                  "failed": 0, "infrastructure": 0, "named_skips": len(skips), "unknown_skips": 0,
                  "named_divergences": len(hits), "stale_divergences": 0}
    if receipt.get("counts") != recomputed:
        problems.append(f"counts {receipt.get('counts')} are not the recomputed {recomputed}")
    if receipt.get("problems") != [] or receipt.get("unavailable") != []:
        problems.append("receipt lists problems or unavailable steps")
    record = receipt.get("provenance") if isinstance(receipt.get("provenance"), dict) else {}
    after = record.get("after") if isinstance(record.get("after"), dict) else {}
    driver, driver_after = record.get("driver"), after.get("driver")
    if not isinstance(driver, dict) or not _hash(driver.get("sha256")) or not str(driver.get("executable", "")).startswith("harness/rust/target/"):
        problems.append("driver binary record missing or malformed")
    elif not isinstance(driver_after, dict) or driver_after.get("sha256") != driver["sha256"] \
            or driver_after.get("executable") != driver["executable"] or driver_after.get("fresh") is not True:
        problems.append("driver binary after the run is not the same fresh executable")
    _check_go(record.get("go"), spec["commit"], "before", problems)
    _check_go(after.get("go"), spec["commit"], "after", problems)
    if isinstance(record.get("go"), dict) and isinstance(after.get("go"), dict) \
            and record["go"].get("sha256") != after["go"].get("sha256"):
        problems.append("Go binary changed during the run")
    if after.get("source") != current or record.get("manifest_sha256") != current.get("manifest_sha256"):
        problems.append("source or manifest after the run is not the current identity")
    if record.get("verified") is not True or record.get("problems") != []:
        problems.append("collector provenance not verified")


def _check_mutation(receipt, pins, differential, problems):
    target = receipt.get("target")
    spec = pins["go"]["targets"].get(target, {})
    if receipt.get("go_commit") != spec.get("commit"):
        problems.append("Go commit is not the target pin")
    if differential is None or receipt.get("run") != differential.get("run"):
        problems.append("mutation control ran on a different capture than the evaluated one")
    if receipt.get("baseline_verdict") != "pass":
        problems.append("baseline evaluation did not pass")
    want = [(case, label, side) for case, label, side, _ in mutations_for(target)]
    controls = receipt.get("controls") if isinstance(receipt.get("controls"), list) else []
    got = [(c.get("case"), c.get("mutation"), c.get("side")) if isinstance(c, dict) else None for c in controls]
    if got != want:
        problems.append(f"controls are not the {len(want)} required mutations once each in order")
    for control in controls:
        if not isinstance(control, dict) or control.get("caught") is not True or control.get("baseline") != "pass" \
                or control.get("verdict") != "fail" or control.get("error"):
            problems.append(f"mutation {control.get('mutation') if isinstance(control, dict) else control!r}: not caught")
    recomputed = {"mutations": len(controls), "caught": sum(isinstance(c, dict) and c.get("caught") is True for c in controls)}
    if receipt.get("counts") != recomputed or recomputed["mutations"] != len(want):
        problems.append(f"counts {receipt.get('counts')} are not the recomputed {recomputed}")


def _check_cargo(receipt, pins, current, problems):
    groups = receipt.get("groups") if isinstance(receipt.get("groups"), list) else []
    if [g.get("id") if isinstance(g, dict) else None for g in groups] != [g["id"] for g in pins["groups"]]:
        problems.append("groups are not every manifest group once each in order")
    total = 0
    targets = {}
    for want_group, group in zip(pins["groups"], groups):
        tests = group.get("tests") if isinstance(group, dict) and isinstance(group.get("tests"), list) else []
        want = [(t["package"], t["target"], t["name"]) for t in want_group["tests"]]
        got = [(t.get("package"), t.get("target"), t.get("name")) if isinstance(t, dict) else None for t in tests]
        if got != want:
            problems.append(f"group {want_group['id']}: tests are not the manifest's {len(want)} once each in order")
        passed = sum(isinstance(t, dict) and t.get("outcome") == "passed" for t in tests)
        stated = (group.get("passed"), group.get("failed")) if isinstance(group, dict) else None
        if passed != len(tests) or stated != (passed, len(tests) - passed):
            problems.append(f"group {want_group['id']}: not every test passed")
        total += passed
        for package, target, _ in want:
            targets[(package, target)] = True
    executions = receipt.get("executions") if isinstance(receipt.get("executions"), list) else []
    got = [(e.get("package"), e.get("target")) if isinstance(e, dict) else None for e in executions]
    if sorted(map(str, got)) != sorted(map(str, targets)) or len(got) != len(set(map(str, got))):
        problems.append("test executions are not every manifest test target once each")
    for execution in executions:
        if not isinstance(execution, dict):
            continue
        label = f"{execution.get('package')} {execution.get('target')}"
        if execution.get("status") != "pass" or execution.get("exit") != 0 or execution.get("listed") != 0 \
                or execution.get("problems") != [] or any(execution.get(k) for k in ("missing", "failed", "ignored", "not_run")):
            problems.append(f"{label}: execution is not a clean exit-0 run of every requested test")
        if not _hash(execution.get("sha256")) or execution.get("sha256_after") != execution.get("sha256") \
                or execution.get("fresh_after") is not True or not str(execution.get("executable", "")).startswith("target/"):
            problems.append(f"{label}: binary record missing, or not the same fresh executable after the run")
    expected_total = sum(len(g["tests"]) for g in pins["groups"])
    recomputed = {"groups": len(pins["groups"]), "tests": expected_total, "passed": total, "failed": expected_total - total,
                  "unknown_tests": 0}
    if receipt.get("counts") != recomputed or total != expected_total:
        problems.append(f"counts {receipt.get('counts')} are not the recomputed {recomputed}")
    if receipt.get("unavailable") != [] or receipt.get("unknown_tests") != []:
        problems.append("receipt lists unavailable steps or unknown tests")
    if receipt.get("source_after") != current:
        problems.append("source after the cargo run is not the current identity")


def combine(paths, pins, current_source=None):
    """One verdict over exactly the five required receipts. Every record is validated
    individually; totals are recomputed from them, never trusted."""
    receipts, problems = [], []
    for p in paths:
        try:
            raw = Path(p).read_bytes()
            receipt = json.loads(raw)
            if not isinstance(receipt, dict) or "kind" not in receipt:
                raise ValueError("not a receipt")
            receipts.append((Path(p).name, receipt, hashlib.sha256(raw).hexdigest()))
        except (OSError, ValueError) as exc:
            problems.append(f"{p}: unreadable receipt ({exc})")
    keys = [(r["kind"], r.get("target")) for _, r, _ in receipts]
    for key in REQUIRED:
        if keys.count(key) != 1:
            problems.append(f"{key[0]}:{key[1]} appears {keys.count(key)} times, exactly one required")
    extra = sorted({f"{k}:{t}" for k, t in keys if (k, t) not in REQUIRED})
    if extra:
        problems.append(f"unexpected receipts {extra}")
    current = current_source or provenance.source_identity()
    differentials = {r.get("target"): r for _, r, _ in receipts if r["kind"] == "differential"}
    for name, receipt, _ in receipts:
        found = []
        _check_common(receipt, current, found)
        if receipt["kind"] == "differential":
            _check_differential(receipt, pins, current, found)
        elif receipt["kind"] == "mutation-control":
            _check_mutation(receipt, pins, differentials.get(receipt.get("target")), found)
        elif receipt["kind"] == "cargo-groups":
            _check_cargo(receipt, pins, current, found)
        problems += [f"{name}: {f}" for f in found]
    return {"kind": "acceptance", "verdict": "fail" if problems else "pass", "partial": False, "problems": problems,
            "tested_source": current,
            "pins": {"go": {n: {k: t[k] for k in ("tag", "commit", "role", "executed")}
                            for n, t in pins["go"]["targets"].items()},
                     "codex": pins["contracts"]["codex"]["commit"], "rust_toolchain": pins["toolchain"]["rust"]},
            "receipts": [{"file": n, "sha256": h, "kind": r["kind"], "target": r.get("target"), "verdict": r.get("verdict"),
                          "counts": r.get("counts"), "rust_revision": r.get("rust_revision")} for n, r, h in receipts]}


def verify_evidence(receipt_path, root=ROOT, allowed=("harness/evidence/",)):
    """A committed acceptance receipt names the source it tested. Later commits may add
    evidence only: HEAD's tree must equal the tested tree outside the evidence paths."""
    receipt = json.loads(Path(receipt_path).read_text())
    tested = receipt["tested_source"]
    problems = []

    def git(*args):
        return subprocess.check_output(["git", "-C", str(root), *args]).decode().strip()
    if receipt.get("verdict") != "pass":
        problems.append(f"receipt verdict is {receipt.get('verdict')}")
    try:
        if git("rev-parse", f"{tested['head']}^{{tree}}") != tested["tree"]:
            problems.append("tested commit's tree is not the recorded tree")
        changed = [p for p in git("diff", "--name-only", tested["head"], "HEAD").splitlines() if p]
    except subprocess.CalledProcessError as exc:
        return {"verdict": "fail", "problems": [f"git: {exc}"]}
    outside = [p for p in changed if not p.startswith(allowed)]
    if outside:
        problems.append(f"HEAD changes files beyond evidence since the tested source: {outside}")
    if git("status", "--porcelain"):
        problems.append("worktree is not clean")
    return {"verdict": "fail" if problems else "pass", "tested_head": tested["head"], "head": git("rev-parse", "HEAD"),
            "evidence_only_changes": changed, "problems": problems}


# --- receipts and CLI ------------------------------------------------------------------

def write_receipt(receipt, name, path=None):
    directory = CACHE / "receipts"
    directory.mkdir(parents=True, exist_ok=True)
    receipt = {"written": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), **receipt}
    path = Path(path) if path else directory / f"{time.strftime('%Y%m%d-%H%M%S')}-{name}.json"
    path.write_text(json.dumps(receipt, indent=2, ensure_ascii=False) + "\n")
    return path


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    parsers = {}
    for name in ("evaluate", "mutation-control"):
        parsers[name] = sub.add_parser(name)
        parsers[name].add_argument("run", type=Path)
    parsers["cargo"] = sub.add_parser("cargo")
    parsers["combine"] = sub.add_parser("combine")
    parsers["combine"].add_argument("receipts", nargs="*")
    for parser_ in parsers.values():
        parser_.add_argument("--receipt", type=Path, help="write the receipt to this path")
    sub.add_parser("verify-evidence").add_argument("receipt", type=Path)
    args = parser.parse_args(argv)
    if args.command == "verify-evidence":
        result = verify_evidence(args.receipt)
        print(json.dumps(result, indent=2))
        return EXIT[result["verdict"]]
    pins = revisions.load(revisions.suite_sizes())
    if args.command == "evaluate":
        receipt = evaluate(args.run.resolve(), pins)
        label = f"differential-{receipt['target']}"
    elif args.command == "mutation-control":
        receipt = mutation_control(args.run.resolve(), pins)
        label = f"mutation-{receipt['target']}"
    elif args.command == "cargo":
        receipt = run_cargo(pins)
        label = "cargo-groups"
    else:
        receipt = combine(args.receipts, pins)
        label = "acceptance"
    path = write_receipt(receipt, label, args.receipt)
    print(json.dumps({k: receipt.get(k) for k in ("kind", "target", "verdict", "partial", "counts") if k in receipt},
                     indent=2))
    for case in receipt.get("cases", []):
        if case["verdict"] != "pass":
            print(f"  {case['name']}: {case['verdict']} {json.dumps(case['failed'])[:600]} "
                  f"unexplained {case['unexplained_count']}")
    for control in receipt.get("controls", []):
        if not control.get("caught"):
            print(f"  NOT CAUGHT: {control}")
    for key in ("problems", "unavailable", "unknown_skips", "stale_divergences", "unknown_tests"):
        if receipt.get(key):
            print(f"  {key}: {receipt[key]}")
    print(f"receipt: {path}")
    return EXIT[receipt["verdict"]]


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
