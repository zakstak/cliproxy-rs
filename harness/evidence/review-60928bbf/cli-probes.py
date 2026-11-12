"""CLI exit codes for one probe per finding on this revision's fresh captures and receipts.

    python3 -I cli-probes.py REPO RECEIPTS HISTORICAL_RUN SELECTIVE_RUN OUT
"""
import copy, json, re, shutil, subprocess, sys
from pathlib import Path

REPO, RECEIPTS, HIST, SEL, OUT = map(Path, sys.argv[1:6])
sys.path.insert(0, str(REPO / "harness"))
from differential import comparable, differences  # noqa: E402

OUT.mkdir(parents=True, exist_ok=True)


def reply_regex(o, pattern, replacement):
    reply = o["downstream"][0]
    body, n = re.subn(pattern.encode(), replacement.encode(), bytes.fromhex(reply["body_hex"]), count=1)
    assert n == 1
    reply["body_hex"], reply["body_text"] = body.hex(), body.decode()
    reply["headers"] = [[k, str(len(body)) if k.lower() == "content-length" else v] for k, v in reply["headers"]]
    head = bytes.fromhex(reply["raw_response_hex"]).partition(b"\r\n\r\n")[0]
    head = re.sub(rb"(?im)^(content-length:[ \t]*)[0-9]+", lambda m: m.group(1) + str(len(body)).encode(), head)
    reply["raw_response_hex"] = (head + b"\r\n\r\n" + body).hex()


def write(o, value):
    [r for r in o["upstream"]["http"] if r["request_line"].startswith("POST /v1/messages")][0]["write_result"] = value


EVALUATE = [
    ("unedited-historical", HIST, None, None),
    ("unedited-selective", SEL, None, None),
    ("f2-rust-created-at-string", SEL, "compact-claude", lambda g, r: reply_regex(r, r'"created_at":([0-9]+)', r'"created_at":"\1"')),
    ("f2-go-capsule-A", SEL, "compact-claude", lambda g, r: reply_regex(g, r"cpa-ag-compact-v1:[A-Za-z0-9_-]+", "cpa-ag-compact-v1:A")),
    ("f3-rust-disabled-zero", SEL, "client-disconnect", lambda g, r: r["credential_after"].update(disabled=0)),
    ("f4-historical-compact-rust-write", HIST, "compact-claude", lambda g, r: write(r, "BrokenPipeError")),
    ("f4-c2-go-write", SEL, "native-signals", lambda g, r: write(g, "BrokenPipeError")),
]
results = []
for label, source, case, edit in EVALUATE:
    dest = OUT / label / source.name
    shutil.rmtree(dest.parent, ignore_errors=True)
    shutil.copytree(source, dest, ignore=shutil.ignore_patterns("auth", "process.log", "config.yaml", "certs"))
    if edit:
        d = dest / case
        go, rust = (json.loads((d / f"{s}.json").read_text()) for s in ("go", "rust"))
        edit(go, rust)
        (d / "go.json").write_text(json.dumps(go))
        (d / "rust.json").write_text(json.dumps(rust))
        (d / "diff.json").write_text(json.dumps(differences(comparable(go), comparable(rust))))
    receipt = dest.parent / "evaluation.json"
    done = subprocess.run([sys.executable, "harness/acceptance.py", "evaluate", str(dest), "--receipt", str(receipt)],
                          cwd=REPO, capture_output=True, text=True)
    r = json.loads(receipt.read_text())
    results.append({"probe": label, "exit": done.returncode, "verdict": r["verdict"],
                    "passed": f"{r['counts']['passed']}/{r['counts']['run']}"})
    print(json.dumps(results[-1]), flush=True)

FILES = ["differential-historical.json", "differential-selective.json", "mutation-historical.json",
         "mutation-selective.json", "cargo-groups.json"]


def swap(r):
    a, b = r[4]["executions"][0], r[4]["executions"][1]
    old, new = a["artifact"]["executable"], b["artifact"]["executable"]
    r[4]["executions"][0] = json.loads(json.dumps(a).replace(old, new))


COMBINE = [
    ("unedited-receipts", lambda r: None),
    ("f1-other-target-executable-everywhere", swap),
    ("f1-other-target-execution-retaining-labels", lambda r: r[4]["executions"].__setitem__(0, {
        **copy.deepcopy(r[4]["executions"][1]), "package": r[4]["executions"][0]["package"],
        "target": r[4]["executions"][0]["target"]})),
    ("f1-result-fields-removed-all-nine", lambda r: [e.pop(k) for e in r[4]["executions"]
                                                     for k in ("missing", "failed", "ignored", "not_run")]),
    ("f1-individual-results-removed", lambda r: [e.update(results=[]) for e in r[4]["executions"]]),
    ("f1-nonexistent-executable", lambda r: r[4]["executions"].__setitem__(0, json.loads(json.dumps(
        r[4]["executions"][0]).replace(r[4]["executions"][0]["artifact"]["executable"], "target/does-not-exist")))),
]
for label, edit in COMBINE:
    items = [json.loads((RECEIPTS / f).read_text()) for f in FILES]
    edit(items)
    d = OUT / label
    d.mkdir(exist_ok=True)
    paths = []
    for f, item in zip(FILES, items):
        (d / f).write_text(json.dumps(item))
        paths.append(str(d / f))
    done = subprocess.run([sys.executable, "harness/acceptance.py", "combine", *paths, "--receipt", str(d / "acceptance.json")],
                          cwd=REPO, capture_output=True, text=True)
    r = json.loads((d / "acceptance.json").read_text())
    results.append({"probe": label, "exit": done.returncode, "verdict": r["verdict"], "problems": len(r["problems"])})
    print(json.dumps(results[-1]), flush=True)
(OUT / "results.json").write_text(json.dumps(results, indent=2))
