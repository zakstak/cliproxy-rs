"""Replays the four reviewed holes against f4d8e222's harness and the fixed harness.

    python3 -I replay.py REPO OLD_TREE CAPTURE_GZ

OLD_TREE holds `git archive f4d8e222` of the harness files and the pin sources they
read; CAPTURE_GZ is f4d8e222's own harness/evidence/2026-11-12-selective.json.gz, the
capture the review replayed. The fixed harness evaluates it without provenance (the
capture predates provenance); combined acceptance never accepts that.
"""
import copy, gzip, importlib, json, os, shutil, sys, tempfile
from pathlib import Path

REPO = Path(sys.argv[1]); OLD = Path(sys.argv[2])
ARCHIVE = json.load(gzip.open(sys.argv[3]))

def load(base):
    for name in ("acceptance", "manifest", "differential", "fixtures", "tls_capture", "judge", "provenance"):
        sys.modules.pop(name, None)
    sys.path.insert(0, str(base / "harness"))
    mod = importlib.import_module("acceptance")
    sys.path.pop(0)
    return mod

def copy_run():
    tmp = Path(tempfile.mkdtemp()) / "run"
    tmp.mkdir()
    (tmp / "summary.json").write_text(json.dumps(ARCHIVE["summary"]))
    for case in ARCHIVE["cases"]:
        d = tmp / case["fixture"]["name"]
        d.mkdir()
        for part in ("fixture", "go", "rust", "diff"):
            (d / f"{part}.json").write_text(json.dumps(case[part]))
    return tmp

def mutate_native(run, diffmod):
    d = run / "native-signals"; go = json.loads((d / "go.json").read_text()); rust = json.loads((d / "rust.json").read_text())
    req = [r for r in rust["upstream"]["http"] if r["request_line"].startswith("POST /v1/messages")][0]
    body = bytes.fromhex(req["body_hex"]).replace(b'"claude-sonnet-4-6"', b'"claude-opus-4-6"', 1)
    req["body_hex"] = body.hex(); req["body_text"] = body.decode()
    req["headers"] = [[k, "Bearer sk-xai-FAKE-LOCAL-ONLY" if k == "Authorization" else v] for k, v in req["headers"]]
    req["raw_head_hex"] = (req["request_line"] + "\r\n" + "".join(f"{k}: {v}\r\n" for k, v in req["headers"]) + "\r\n").encode().hex()
    (d / "rust.json").write_text(json.dumps(rust))
    (d / "diff.json").write_text(json.dumps(diffmod.differences(diffmod.comparable(go), diffmod.comparable(rust))))

def drop_case(run):
    shutil.rmtree(run / "buffered")
    s = json.loads((run / "summary.json").read_text())
    for key in ("suite_cases", "selected"):
        s[key] = [n for n in s[key] if n != "buffered"]
    s["cases"] = [c for c in s["cases"] if c["name"] != "buffered"]
    (run / "summary.json").write_text(json.dumps(s))

for label, base in (("reviewed f4d8e222", OLD), ("fixed", REPO)):
    acc = load(base)
    pins = acc.revisions.load(acc.revisions.suite_sizes())
    kw = {} if label.startswith("reviewed") else {"check_provenance": False}
    run = copy_run(); mutate_native(run, sys.modules["differential"])
    r = acc.evaluate(run, pins, **kw)
    case = next(c for c in r["cases"] if c["name"] == "native-signals")
    print(f"[{label}] P1 native-signals model + Grok credential mutated: case {case['verdict']}")
    run = copy_run(); drop_case(run)
    r = acc.evaluate(run, pins, **kw)
    print(f"[{label}] P2 buffered deleted from suite/selection/cases: verdict {r['verdict']}, run {r['counts']['run']}, unknown skips {r['counts']['unknown_skips']}")
    with tempfile.TemporaryDirectory() as tmp:
        paths = []
        for i, (kind, target) in enumerate([("differential", "historical"), ("differential", "selective"),
                                            ("mutation-control", "historical"), ("cargo-groups", None)]):
            p = Path(tmp) / f"{i}.json"
            p.write_text(json.dumps({"kind": kind, "target": target, "verdict": "pass", "rust_revision": str(i) * 40,
                                     "rust_worktree_clean": False, "groups": [{"id": "controls"}],
                                     "counts": {"tests": 1, "passed": 1}}))
            paths.append(str(p))
        print(f"[{label}] P2 four dirty unrelated revisions, cargo only 'controls': combine {acc.combine(paths, pins)['verdict']}")
    out = "running 1 test\ntest a ... ok\n"
    if hasattr(acc, "judge_execution"):
        status = acc.judge_execution(["a"], {"exit": 0, "names": {"a"}, "timeout": False}, {"exit": 42, "stdout": out, "timeout": False})["status"]
    else:
        group = {"tests": [{"package": "p", "target": "lib", "name": "a", "paths": []}]}
        status = acc.judge_group(group, {("p", "lib"): acc.parse_libtest(out)}, {("p", "lib"): {"a"}})[0]["outcome"]
    print(f"[{label}] P2 'test a ... ok' with exit 42: {status}")
