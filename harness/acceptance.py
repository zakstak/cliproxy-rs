"""Acceptance over harness evidence (issue #67, H1).

The collector (differential.py) records everything and exits 0 when capture worked.
This command decides acceptance. It applies semantic assertions to each Go/Rust case
and requires every raw collector difference to match a named divergence in
harness/manifest.json. It also requires every suite case to have run or to be a named
skip, and it runs the retained-provider cargo groups. Each command writes a receipt.

    acceptance.py evaluate RUN_DIR           # one differential run
    acceptance.py cargo [--group ID ...]     # cargo test groups, network-isolated
    acceptance.py mutation-control RUN_DIR   # proves evaluate fails on real deviations
    acceptance.py combine RECEIPT ...        # one verdict over several receipts

Exit 0: accepted. Exit 1: a semantic assertion failed, a difference was not named, a
case was skipped without a named reason, a named divergence went stale, or a cargo test
failed, was ignored or was missing. Exit 2: execution was unavailable (prerequisite,
capture or transport failure). An unavailable run is never reported as a pass.
"""
import argparse
import copy
import fnmatch
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time

import manifest as revisions
from differential import comparable, differences
from fixtures import CREDENTIAL, PROVIDER_KEYS, TOKEN_REPLY

ROOT = revisions.ROOT
CACHE = ROOT / "harness/.cache"
CLIENT_KEYS = ("fixture-client-key", "wrong-fixture-key")
UPSTREAM_SECRETS = (CREDENTIAL["access_token"], CREDENTIAL["refresh_token"],
                    *(json.loads(TOKEN_REPLY)[k] for k in ("access_token", "refresh_token")),
                    *PROVIDER_KEYS.values())
# Fields a provider or proxy generates per response; the meaning comparison ignores
# their values. Their raw differences still need a named divergence.
GENERATED = {"id", "created", "created_at", "system_fingerprint", "item_id", "call_id", "tool_use_id"}
# Cases where neither proxy may reach the inference upstream (differential.py).
NO_TRAFFIC = {"auth-missing", "auth-wrong", "auth-query-first", "models-openai", "models-anthropic",
              "disabled-credential"}


# --- semantic assertions -------------------------------------------------------------

def _hex_text(value):
    return bytes.fromhex(value).decode("utf-8", errors="replace") if value else ""


def _content_type(reply):
    for name, value in reply.get("headers", []):
        if name.lower() == "content-type":
            return value.split(";")[0].strip().lower()
    return None


def _inference(observation, case):
    prefix = "POST /v1/responses " if case.get("provider") else "POST /v1/messages"
    return [r for r in observation["upstream"]["http"] if r["request_line"].startswith(prefix)]


def _strip_generated(value):
    if isinstance(value, dict):
        return {k: _strip_generated(v) for k, v in value.items() if k not in GENERATED}
    if isinstance(value, list):
        return [_strip_generated(v) for v in value]
    return value


def _events(text):
    """(event types, concatenated text deltas, terminal marker) of an SSE body."""
    kinds, chunks, done = [], [], False
    for line in text.replace("\r\n", "\n").split("\n"):
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if data == "[DONE]":
            done = True
            continue
        try:
            event = json.loads(data)
        except ValueError:
            kinds.append("<undecodable>")
            continue
        if not isinstance(event, dict):
            kinds.append("<non-object>")
            continue
        kinds.append(event.get("type") or event.get("object") or "<untyped>")
        delta = event.get("delta")
        if isinstance(delta, dict):
            chunks.append(delta.get("text") or delta.get("partial_json") or "")
        elif isinstance(delta, str):
            chunks.append(delta)
        for choice in event.get("choices") or []:
            if isinstance(choice, dict):
                chunks.append((choice.get("delta") or {}).get("content") or choice.get("text") or "")
    return kinds, "".join(chunks), done


def _meaning(reply):
    """What a downstream body means, independent of generated values and framing."""
    text = _hex_text(reply.get("body_hex"))
    if _content_type(reply) == "text/event-stream":
        kinds, joined, done = _events(text)
        return {"sse": kinds, "text": joined, "done": done, "read_error": reply.get("read_error")}
    try:
        return {"json": _strip_generated(json.loads(text))}
    except ValueError:
        return {"bytes": text}


def _request_meaning(request):
    try:
        body = json.loads(_hex_text(request["body_hex"]))
    except ValueError:
        return {"bytes": request["body_hex"]}
    keep = ("model", "messages", "input", "stream", "max_tokens", "tools", "tool_choice")
    return {k: body.get(k) for k in keep if isinstance(body, dict) and k in body}


def _masked(headers, generated, dropped=frozenset()):
    """Ordered headers with generated values masked and divergence-named fields left out."""
    return [[name, "<generated>" if name.lower() in generated else value]
            for name, value in headers if name.lower() not in dropped]


def _header_map(headers, generated):
    """Field name (case-insensitive) -> values in order; RFC 9110 field semantics."""
    out = {}
    for name, value in headers:
        key = name.lower()
        out.setdefault(key, []).append("<generated>" if key in generated else value)
    return out


def header_differences(go_reply, rust_reply, generated):
    go_map = _header_map(go_reply.get("headers", []), generated)
    rust_map = _header_map(rust_reply.get("headers", []), generated)
    return sorted(name for name in go_map.keys() | rust_map.keys() if go_map.get(name) != rust_map.get(name))


def assertions(case, go, rust, generated=None, excused_headers=(), upstream_excuses=None):
    """[{name, ok, detail}] for one case. Byte checks cover request and response bodies
    and the ordered upstream request headers; meaning checks cover what a client reads.
    `generated` names headers whose values are minted per request (manifest
    "generated_headers"); `excused_headers` are downstream field names a named
    divergence covers. `upstream_excuses` holds upstream header names and body regexes
    named divergences cover; each is masked on both sides before the byte comparison."""
    generated = generated or {"upstream": [], "downstream": []}
    upstream_excuses = upstream_excuses or {"headers": set(), "patterns": []}
    up_generated = {h.lower() for h in generated["upstream"]}
    down_generated = {h.lower() for h in generated["downstream"]}
    out = []

    def check(name, ok, detail=""):
        out.append({"name": name, "ok": bool(ok), "detail": "" if ok else detail})

    go_down, rust_down = go["downstream"], rust["downstream"]
    check("status", [r.get("status") for r in go_down] == [r.get("status") for r in rust_down],
          f"Go {[r.get('status') for r in go_down]} Rust {[r.get('status') for r in rust_down]}")
    check("content-type", [_content_type(r) for r in go_down] == [_content_type(r) for r in rust_down],
          f"Go {[_content_type(r) for r in go_down]} Rust {[_content_type(r) for r in rust_down]}")
    go_up, rust_up = _inference(go, case), _inference(rust, case)
    if case["name"] in NO_TRAFFIC:
        # A refused client or an unusable credential must never reach the provider.
        check("upstream-traffic", not go_up and not rust_up,
              f"inference requests: Go {len(go_up)} Rust {len(rust_up)}; none allowed")
    elif case.get("no_upstream"):
        check("upstream-traffic", len(go_up) == len(rust_up),
              f"inference requests: Go {len(go_up)} Rust {len(rust_up)}")
    else:
        check("upstream-traffic", go_up and rust_up and len(go_up) == len(rust_up),
              f"inference requests: Go {len(go_up)} Rust {len(rust_up)}; both required")
    check("request-lines", [r["request_line"] for r in go_up] == [r["request_line"] for r in rust_up],
          f"Go {[r['request_line'] for r in go_up]} Rust {[r['request_line'] for r in rust_up]}")
    go_heads = [_masked(r["headers"], up_generated, upstream_excuses["headers"]) for r in go_up]
    rust_heads = [_masked(r["headers"], up_generated, upstream_excuses["headers"]) for r in rust_up]
    # Upstream header order and case are part of a client fingerprint: compared exactly.
    check("upstream-headers", go_heads == rust_heads,
          next((f"request {i}: Go {g} Rust {r}" for i, (g, r) in enumerate(zip(go_heads, rust_heads)) if g != r),
               "request count differs"))
    def body(request):
        raw = bytes.fromhex(request["body_hex"])
        for pattern in upstream_excuses["patterns"]:
            raw = re.sub(pattern.encode(), b"", raw)
        return raw
    check("upstream-body-bytes", [body(r) for r in go_up] == [body(r) for r in rust_up],
          "an inference request body differs byte for byte outside named divergence patterns")
    check("upstream-body-meaning",
          [_request_meaning(r) for r in go_up] == [_request_meaning(r) for r in rust_up],
          "model, messages/input, stream, max_tokens, tools or tool_choice differ upstream")
    differing = sorted({name for g, r in zip(go_down, rust_down)
                        for name in header_differences(g, r, down_generated)} - set(excused_headers))
    check("downstream-headers", not differing, f"field values differ: {differing}")
    if case.get("disconnect"):
        # The client closes after the first event; how much it read is timing.
        check("downstream-body-bytes", True)
        check("downstream-meaning", True)
    else:
        check("downstream-body-bytes", [r.get("body_hex") for r in go_down] == [r.get("body_hex") for r in rust_down],
              "a downstream body differs byte for byte")
        go_meaning, rust_meaning = [_meaning(r) for r in go_down], [_meaning(r) for r in rust_down]
        check("downstream-meaning", go_meaning == rust_meaning,
              f"Go {json.dumps(go_meaning)[:400]} Rust {json.dumps(rust_meaning)[:400]}")
    # Credential isolation is absolute: Go's behaviour does not excuse a leak.
    leaks = []
    for request in rust["upstream"]["http"]:
        raw = _hex_text(request["raw_head_hex"]) + _hex_text(request["body_hex"])
        leaks += [f"client key {k[:8]}... reached the upstream" for k in CLIENT_KEYS if k in raw]
    for reply in rust_down:
        raw = _hex_text(reply.get("raw_response_hex"))
        leaks += [f"upstream secret {s[:12]}... reached the client" for s in UPSTREAM_SECRETS if s in raw]
    check("credential-isolation", not leaks, "; ".join(sorted(set(leaks))))
    return out


# --- divergences ---------------------------------------------------------------------

def _applies(divergence, target, case_name):
    def matches(patterns):
        return any(fnmatch.fnmatchcase(case_name, pattern) for pattern in patterns)
    return (target in divergence["targets"] and matches(divergence["cases"])
            and not matches(divergence.get("except_cases", [])))


def classify(diffs, divergences, target, case_name):
    """Split raw differences into named (by divergence id) and unexpected."""
    named, unexpected = {}, []
    candidates = [d for d in divergences if _applies(d, target, case_name)]
    for diff in diffs:
        for divergence in candidates:
            if any(re.fullmatch(pattern, diff["path"]) for pattern in divergence.get("paths", [])):
                named[divergence["id"]] = named.get(divergence["id"], 0) + 1
                break
        else:
            unexpected.append(diff)
    return named, unexpected


def excused(divergences, target, case_name, assertion):
    return [d["id"] for d in divergences
            if _applies(d, target, case_name) and assertion in d.get("assertions", [])]


def upstream_excuses(divergences, target, case_name):
    """Upstream header names and body regexes named divergences cover for this case."""
    out = {"headers": set(), "patterns": [], "sources": {}}
    for divergence in divergences:
        if _applies(divergence, target, case_name):
            for name in divergence.get("upstream_header_names", []):
                out["headers"].add(name.lower())
                out["sources"].setdefault(("header", name.lower()), divergence["id"])
            for pattern in divergence.get("upstream_body_patterns", []):
                out["patterns"].append(pattern)
                out["sources"].setdefault(("pattern", pattern), divergence["id"])
    return out


def upstream_excuse_hits(excuses, go_up, rust_up):
    """Divergence ids whose upstream header or body excuse covers a real difference."""
    hit = set()
    for go_request, rust_request in zip(go_up, rust_up):
        go_map = _header_map(go_request["headers"], set())
        rust_map = _header_map(rust_request["headers"], set())
        go_body, rust_body = bytes.fromhex(go_request["body_hex"]), bytes.fromhex(rust_request["body_hex"])
        for (kind, key), divergence in excuses["sources"].items():
            if kind == "header" and go_map.get(key) != rust_map.get(key):
                hit.add(divergence)
            if kind == "pattern" and go_body != rust_body and \
                    (re.search(key.encode(), go_body) or re.search(key.encode(), rust_body)):
                hit.add(divergence)
    return hit


def excused_headers(divergences, target, case_name):
    """{lowercase downstream field name: divergence id} for this case."""
    out = {}
    for divergence in divergences:
        if _applies(divergence, target, case_name):
            for name in divergence.get("header_names", []):
                out.setdefault(name.lower(), divergence["id"])
    return out


# --- evaluate ------------------------------------------------------------------------

def evaluate(run_dir, pins=None):
    """A receipt for one differential run directory."""
    pins = pins or revisions.load(revisions.suite_sizes())
    summary = json.loads((run_dir / "summary.json").read_text())
    target = summary["target"]
    expected = pins["go"]["targets"][target]
    divergences = pins["divergences"]
    problems = []
    if summary["reference"] != expected["commit"]:
        problems.append(f"run used {summary['reference']}, manifest pins {expected['commit']}")
    skips = {s["case"]: s for s in pins.get("skips", []) if target in s["targets"]}
    ran = {result["name"] for result in summary["cases"]}
    named_skips = sorted(set(summary["suite_cases"]) - ran & set(skips))
    unknown_skips = sorted(set(summary["suite_cases"]) - ran - set(skips))
    cases, hits = [], {}
    infrastructure = []
    for result in summary["cases"]:
        name = result["name"]
        directory = run_dir / name
        fixture = json.loads((directory / "fixture.json").read_text())
        go = json.loads((directory / "go.json").read_text())
        rust = json.loads((directory / "rust.json").read_text())
        diffs = json.loads((directory / "diff.json").read_text())
        errors = result["capture_errors"] + result["transport_errors"]
        if errors:
            infrastructure.append({"case": name, "errors": errors})
        named, unexpected = classify(diffs, divergences, target, name)
        for key, count in named.items():
            hits[key] = hits.get(key, 0) + count
        failed = []
        header_excuses = excused_headers(divergences, target, name)
        generated = pins.get("generated_headers")
        for key in {header_excuses[h] for g, r in zip(go["downstream"], rust["downstream"])
                    for h in header_differences(g, r, {x.lower() for x in (generated or {}).get("downstream", [])})
                    if h in header_excuses}:
            named[key] = named.get(key, 0) + 1
            hits[key] = hits.get(key, 0) + 1
        upstream_named = upstream_excuses(divergences, target, name)
        for key in upstream_excuse_hits(upstream_named, _inference(go, fixture), _inference(rust, fixture)):
            named[key] = named.get(key, 0) + 1
            hits[key] = hits.get(key, 0) + 1
        for item in assertions(fixture, go, rust, generated, header_excuses, upstream_named):
            if item["ok"]:
                continue
            excuse = excused(divergences, target, name, item["name"])
            if excuse:
                for key in excuse:
                    named[key] = named.get(key, 0) + 1
                    hits[key] = hits.get(key, 0) + 1
                item["excused_by"] = excuse
            else:
                failed.append(item)
        verdict = "infrastructure" if errors else ("fail" if failed or unexpected else "pass")
        cases.append({"name": name, "verdict": verdict, "differences": len(diffs), "named": named,
                      "failed_assertions": failed,
                      "unexpected": unexpected[:20], "unexpected_count": len(unexpected)})
    complete = not unknown_skips and set(summary["selected"]) == set(summary["suite_cases"]) - set(named_skips)
    stale = []
    if complete:
        for divergence in divergences:
            if target in divergence["targets"] and divergence["id"] not in hits \
                    and not divergence.get("may_be_absent"):
                stale.append(divergence["id"])
    counts = {
        "suite": len(summary["suite_cases"]), "run": len(summary["cases"]),
        "passed": sum(c["verdict"] == "pass" for c in cases),
        "failed": sum(c["verdict"] == "fail" for c in cases),
        "infrastructure": sum(c["verdict"] == "infrastructure" for c in cases),
        "named_skips": len(named_skips), "unknown_skips": len(unknown_skips),
        "named_divergences": len(hits), "stale_divergences": len(stale),
    }
    if infrastructure or summary.get("infrastructure_failed"):
        verdict = "unavailable"
    elif problems or unknown_skips or stale or counts["failed"]:
        verdict = "fail"
    else:
        verdict = "pass"
    return {
        "kind": "differential", "verdict": verdict, "target": target, "tag": summary["tag"],
        "go_commit": summary["reference"], "role": summary["role"], "suite": summary["suite"],
        "rust_revision": summary["rust_revision"], "rust_worktree_clean": summary.get("rust_worktree_clean"),
        "toolchain": summary.get("toolchain"), "isolation": summary.get("isolation"),
        "normalizations": summary.get("normalizations"), "run": run_dir.name,
        "counts": counts, "problems": problems, "named_skips": named_skips, "unknown_skips": unknown_skips,
        "divergences": {key: {"differences": hits[key], **_brief(divergences, key)} for key in sorted(hits)},
        "stale_divergences": stale, "infrastructure": infrastructure, "cases": cases,
    }


def _brief(divergences, key):
    divergence = next(d for d in divergences if d["id"] == key)
    return {k: divergence[k] for k in ("kind", "issue", "ledger") if divergence.get(k)}


# --- cargo groups ----------------------------------------------------------------------

def _cargo_args(package, target):
    if target == "lib":
        return ["-p", package, "--lib"]
    return ["-p", package, "--test", target.split(":", 1)[1]]


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


def judge_group(group, results, listed):
    """Each manifest test must exist and pass. Missing, ignored or failed is a failure."""
    out = []
    for test in group["tests"]:
        key = (test["package"], test["target"])
        if test["name"] not in listed.get(key, set()):
            outcome = "missing"
        else:
            outcome = {"ok": "passed", "FAILED": "failed", "ignored": "ignored"}.get(
                results.get(key, {}).get(test["name"]), "not-run")
        out.append({"name": test["name"], "package": test["package"], "target": test["target"],
                    "paths": test["paths"], "outcome": outcome})
    return out


def run_cargo(pins, group_ids):
    groups = [g for g in pins["groups"] if not group_ids or g["id"] in group_ids]
    if group_ids and len(groups) != len(set(group_ids)):
        raise SystemExit(f"unknown group in {group_ids}")
    targets = {}
    for group in groups:
        for test in group["tests"]:
            targets.setdefault((test["package"], test["target"]), []).append(test["name"])
    env = dict(os.environ, CPA_TEST_NO_SKIP="1", CARGO_TERM_COLOR="never")
    log = []
    # Build before isolation (dependencies may need fetching); run offline inside it.
    for package in sorted({package for package, _ in targets}):
        build = ["cargo", "test", "--locked", "--no-run", "-p", package]
        for (other, target) in sorted(targets):
            if other == package:
                build += _cargo_args(package, target)[2:]
        built = subprocess.run(build, cwd=ROOT, env=env, capture_output=True, text=True)
        log.append({"command": build, "exit": built.returncode, "tail": built.stderr[-4000:]})
        if built.returncode != 0:
            return {"kind": "cargo-groups", "verdict": "unavailable", "log": log}
    results, listed = {}, {}
    isolated = ROOT / "ui/scripts/isolated.sh"
    for (package, target), names in sorted(targets.items()):
        base = ["cargo", "test", "--locked", "--offline", *_cargo_args(package, target), "--"]
        listing = subprocess.run(base + ["--list"], cwd=ROOT, env=env, capture_output=True, text=True)
        listed[(package, target)] = parse_list(listing.stdout)
        command = base + ["--exact", *sorted(set(names))]
        shell = " ".join(_quote(part) for part in command)
        # Loopback-only namespace with the caller's uid: tests cannot reach a provider.
        ran = subprocess.run([str(isolated), shell], cwd=ROOT, env=env, capture_output=True, text=True)
        results[(package, target)] = parse_libtest(ran.stdout)
        log.append({"command": command, "isolated": True, "exit": ran.returncode,
                    "tail": (ran.stdout + ran.stderr)[-4000:] if ran.returncode else ""})
    unknown = []
    for owned in pins.get("owned_test_targets", []):
        key = (owned["package"], owned["target"])
        if key not in listed:
            continue
        known = {t["name"] for g in pins["groups"] for t in g["tests"]
                 if (t["package"], t["target"]) == key}
        unknown += [f"{key[0]} {key[1]} {name}" for name in sorted(listed[key] - known)]
    report = []
    for group in groups:
        tests = judge_group(group, results, listed)
        report.append({"id": group["id"], "provider": group["provider"], "tests": tests,
                       "passed": sum(t["outcome"] == "passed" for t in tests),
                       "failed": sum(t["outcome"] != "passed" for t in tests)})
    failed = sum(g["failed"] for g in report)
    return {"kind": "cargo-groups", "verdict": "fail" if failed or unknown else "pass",
            "rust_revision": _git("rev-parse", "HEAD"), "rust_worktree_clean": not _git("status", "--porcelain",
                                                                                      "--untracked-files=no"),
            "toolchain": _toolchain(), "isolation": "ui/scripts/isolated.sh: loopback-only user+net namespace, "
                                                     "caller uid/gid; CPA_TEST_NO_SKIP=1",
            "counts": {"groups": len(report), "tests": sum(len(g["tests"]) for g in report),
                       "passed": sum(g["passed"] for g in report), "failed": failed,
                       "unknown_tests": len(unknown)},
            "unknown_tests": unknown, "groups": report, "log": log}


def _quote(part):
    return part if re.fullmatch(r"[A-Za-z0-9_:./=-]+", part) else "'" + part.replace("'", "'\\''") + "'"


def _git(*args):
    return subprocess.check_output(["git", "-C", str(ROOT), *args]).decode().strip()


def _toolchain():
    out = {}
    for name, command in [("rustc", ["rustc", "--version"]), ("cargo", ["cargo", "--version"])]:
        try:
            out[name] = subprocess.check_output(command).decode().strip()
        except (OSError, subprocess.CalledProcessError) as exc:
            out[name] = f"unavailable: {exc}"
    return out


# --- mutation control ----------------------------------------------------------------

def _first_inference_case(run_dir, summary):
    for result in summary["cases"]:
        fixture = json.loads((run_dir / result["name"] / "fixture.json").read_text())
        rust = json.loads((run_dir / result["name"] / "rust.json").read_text())
        if _inference(rust, fixture) and not fixture.get("disconnect") and result["name"] == "buffered":
            return result["name"]
    raise SystemExit("mutation control needs the buffered case in the run")


def _edit_body(field, old, new):
    def edit(observation):
        request = next(r for r in observation["upstream"]["http"] if r["request_line"].startswith("POST /v1/messages"))
        body = bytes.fromhex(request["body_hex"])
        assert old.encode() in body, f"{field}: {old} not in the request"
        body = body.replace(old.encode(), new.encode(), 1)
        request["body_hex"] = body.hex()
        request["body_text"] = body.decode("utf-8", errors="replace")
    return edit


def _edit_status(observation):
    observation["downstream"][0]["status"] = 500


def _leak_client_key(observation):
    request = observation["upstream"]["http"][0]
    head = bytes.fromhex(request["raw_head_hex"])
    head = head.replace(b"\r\n\r\n", b"\r\nX-Leak: fixture-client-key\r\n\r\n", 1)
    request["raw_head_hex"] = head.hex()


def _edit_answer(observation):
    reply = observation["downstream"][0]
    reply["body_hex"] = bytes.fromhex(reply["body_hex"]).replace(b"local answer", b"other answer").hex()


MUTATIONS = [
    ("upstream max_tokens 17 -> 18", _edit_body("max_tokens", '"max_tokens":17', '"max_tokens":18')),
    ("downstream status 200 -> 500", _edit_status),
    ("client key forwarded upstream", _leak_client_key),
    ("downstream text changed", _edit_answer),
]


def mutation_control(run_dir, pins):
    """Mutate the Rust side of a recorded case in a copy and re-evaluate. Each mutation
    must turn an accepted case into a failure; one that still passes means the
    acceptance command cannot see that deviation."""
    summary = json.loads((run_dir / "summary.json").read_text())
    name = _first_inference_case(run_dir, summary)
    baseline = evaluate(run_dir, pins)
    base_case = next(c for c in baseline["cases"] if c["name"] == name)
    controls = []
    for label, edit in MUTATIONS:
        with tempfile.TemporaryDirectory() as scratch:
            copy_dir = Path(scratch) / run_dir.name
            shutil.copytree(run_dir, copy_dir, symlinks=True)
            directory = copy_dir / name
            go = json.loads((directory / "go.json").read_text())
            rust = json.loads((directory / "rust.json").read_text())
            edit(rust)
            (directory / "rust.json").write_text(json.dumps(rust))
            (directory / "diff.json").write_text(json.dumps(differences(comparable(go), comparable(rust))))
            receipt = evaluate(copy_dir, pins)
            case = next(c for c in receipt["cases"] if c["name"] == name)
            controls.append({"mutation": label, "case": name, "case_verdict": case["verdict"],
                             "failed_assertions": [a["name"] for a in case["failed_assertions"]],
                             "unexpected_count": case["unexpected_count"], "caught": case["verdict"] == "fail"})
    verdict = "pass" if base_case["verdict"] == "pass" and all(c["caught"] for c in controls) else "fail"
    return {"kind": "mutation-control", "verdict": verdict, "run": run_dir.name, "target": summary["target"],
            "go_commit": summary["reference"], "rust_revision": summary["rust_revision"],
            "baseline_case_verdict": base_case["verdict"], "controls": controls}


# --- receipts ------------------------------------------------------------------------

def write_receipt(receipt, name):
    directory = CACHE / "receipts"
    directory.mkdir(parents=True, exist_ok=True)
    receipt = {"written": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), **receipt}
    path = directory / f"{time.strftime('%Y%m%d-%H%M%S')}-{name}.json"
    path.write_text(json.dumps(receipt, indent=2, ensure_ascii=False) + "\n")
    return path


def combine(paths, pins):
    receipts = [json.loads(Path(p).read_text()) for p in paths]
    kinds = {(r["kind"], r.get("target")) for r in receipts}
    required = {("differential", "historical"), ("differential", "selective"), ("cargo-groups", None),
                ("mutation-control", "historical")}
    missing = sorted(f"{k}:{t}" if t else k for k, t in required - kinds)
    verdicts = [r["verdict"] for r in receipts]
    if missing or "fail" in verdicts:
        verdict = "fail"
    elif "unavailable" in verdicts:
        verdict = "unavailable"
    else:
        verdict = "pass"
    return {"kind": "acceptance", "verdict": verdict, "missing_receipts": missing,
            "pins": {"go": {name: {"tag": t["tag"], "commit": t["commit"], "role": t["role"],
                                   "executed": t["executed"]} for name, t in pins["go"]["targets"].items()},
                     "codex": pins["contracts"]["codex"]["commit"], "rust_toolchain": pins["toolchain"]["rust"]},
            "receipts": [{"file": Path(p).name, "kind": r["kind"], "target": r.get("target"),
                          "verdict": r["verdict"], "counts": r.get("counts"),
                          "rust_revision": r.get("rust_revision")} for p, r in zip(paths, receipts)]}


EXIT = {"pass": 0, "fail": 1, "unavailable": 2}


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("evaluate", "mutation-control"):
        sub.add_parser(name).add_argument("run", type=Path)
    cargo = sub.add_parser("cargo")
    cargo.add_argument("--group", action="append", default=[])
    sub.add_parser("combine").add_argument("receipts", nargs="+")
    args = parser.parse_args(argv)
    pins = revisions.load(revisions.suite_sizes())
    if args.command == "evaluate":
        receipt = evaluate(args.run.resolve(), pins)
        label = f"differential-{receipt['target']}"
    elif args.command == "mutation-control":
        receipt = mutation_control(args.run.resolve(), pins)
        label = f"mutation-{receipt['target']}"
    elif args.command == "cargo":
        receipt = run_cargo(pins, args.group)
        label = "cargo-groups"
    else:
        receipt = combine(args.receipts, pins)
        label = "acceptance"
    path = write_receipt(receipt, label)
    print(json.dumps({k: receipt.get(k) for k in ("kind", "target", "verdict", "counts", "missing_receipts")
                      if k in receipt}, indent=2))
    if receipt.get("kind") == "differential":
        for case in receipt["cases"]:
            if case["verdict"] != "pass":
                print(f"  {case['name']}: {case['verdict']}; assertions "
                      f"{[a['name'] for a in case['failed_assertions']]}; unexpected {case['unexpected_count']}")
        for key in ("unknown_skips", "stale_divergences", "problems"):
            if receipt.get(key):
                print(f"  {key}: {receipt[key]}")
    print(f"receipt: {path}")
    return EXIT[receipt["verdict"]]


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
