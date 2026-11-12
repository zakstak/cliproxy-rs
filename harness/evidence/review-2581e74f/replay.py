"""Old-head witnesses for the 2581e74f review findings.

    python3 -I replay.py REPO OLD_TREE OLD_RECEIPTS NEW_RECEIPTS

OLD_TREE is `git archive 2581e74f` of the harness and the pin sources it reads. The
replacement probes apply the reviewer's three edits to 2581e74f's own committed captures
(harness/evidence/2026-11-12-*.json.gz) and evaluate them with each harness. The combine
probes apply the reviewer's record edits to a complete receipt set: the reviewer's fresh
2581e74f receipts for the old harness, and this revision's fresh receipts for the new one.
In each, the unedited baseline passes under its own harness, so a failure is the targeted
check, not a schema mismatch.
"""
import copy, gzip, importlib, json, shutil, sys, tempfile
from pathlib import Path

REPO, OLD, OLD_RECEIPTS, NEW_RECEIPTS = map(Path, sys.argv[1:5])
NAMES = ("acceptance", "manifest", "differential", "fixtures", "tls_capture", "judge", "provenance", "golden")


def harness(base):
    for name in NAMES:
        sys.modules.pop(name, None)
    sys.path.insert(0, str(base / "harness"))
    module = importlib.import_module("acceptance")
    sys.path.pop(0)
    return module


def capture(target):
    archive = json.load(gzip.open(OLD / f"harness/evidence/2026-11-12-{target}.json.gz"))
    run = Path(tempfile.mkdtemp()) / archive["summary"].get("run", f"run-{target}-20261112-000000")
    run = run.parent / f"run-{target}"
    run.mkdir()
    (run / "summary.json").write_text(json.dumps(archive["summary"]))
    for case in archive["cases"]:
        d = run / case["fixture"]["name"]
        d.mkdir()
        for part in ("fixture", "go", "rust", "diff"):
            (d / f"{part}.json").write_text(json.dumps(case[part]))
    return run


def edit_body(observation, old, new):
    request = [r for r in observation["upstream"]["http"] if r["request_line"].startswith("POST /v1/messages")][0]
    body = bytes.fromhex(request["body_hex"])
    assert old.encode() in body, old
    body = body.replace(old.encode(), new.encode(), 1)
    request["body_hex"], request["body_text"] = body.hex(), body.decode()
    request["headers"] = [[k, str(len(body)) if k.lower() == "content-length" else v] for k, v in request["headers"]]
    request["raw_head_hex"] = (request["request_line"] + "\r\n" + "".join(f"{k}: {v}\r\n" for k, v in request["headers"])
                               + "\r\n").encode("latin1").hex()


REPLACEMENT = [("historical", "compact-claude", "rust", "Local question", "Wrong question"),
               ("historical", "compact-claude", "rust", '"max_tokens":64000', '"max_tokens":1'),
               ("selective", "native-signals", "go", "Local question", "Wrong question")]

COMBINE = {
    "missing case, same count": lambda r: r[0]["cases"].pop(0),
    "duplicate case, same count": lambda r: r[0]["cases"].__setitem__(0, copy.deepcopy(r[0]["cases"][1])),
    "extra case, same count": lambda r: r[0]["cases"].append({"name": "unrecognised", "verdict": "pass"}),
    "all groups empty, same count": lambda r: [g.update(tests=[], passed=0, failed=0) for g in r[4]["groups"]],
    "duplicate test, same count": lambda r: r[4]["groups"][0]["tests"].__setitem__(0, copy.deepcopy(r[4]["groups"][0]["tests"][1])),
    "failed test, same count": lambda r: r[4]["groups"][0]["tests"][0].update(outcome="failed"),
    "missing controls, same count": lambda r: r[2].update(controls=[]),
    "duplicate control, same count": lambda r: r[2]["controls"].__setitem__(0, copy.deepcopy(r[2]["controls"][1])),
    "unknown control, same count": lambda r: r[2]["controls"][0].update(mutation="unrecognised"),
    "no cargo binary provenance": lambda r: r[4].update(executions=[]),
    "failed execution, same count": lambda r: r[4]["executions"][0].update(exit=42, status="fail", problems=["exit 42"]),
    "missing driver provenance": lambda r: [x["provenance"].pop("driver") for x in r[:2]],
    "wrong driver hash": lambda r: r[1]["provenance"]["driver"].update(sha256="0" * 64),
    "different rust_revision field": lambda r: r[1].update(rust_revision="0" * 40),
}
FILES = ["differential-historical.json", "differential-selective.json", "mutation-historical.json",
         "mutation-selective.json", "cargo-groups.json"]


for label, base, receipts_dir in (("reviewed 2581e74f", OLD, OLD_RECEIPTS), ("fixed", REPO, NEW_RECEIPTS)):
    acc = harness(base)
    pins = acc.revisions.load(acc.revisions.suite_sizes())
    diff = sys.modules["differential"]
    runs = {t: capture(t) for t in ("historical", "selective")}
    for target, run in runs.items():
        print(f"[{label}] baseline {target} capture: {acc.evaluate(run, pins)['verdict']}")
    for target, case, side, old, new in REPLACEMENT:
        run = Path(tempfile.mkdtemp()) / runs[target].name
        shutil.copytree(runs[target], run)
        go, rust = (json.loads((run / case / f"{s}.json").read_text()) for s in ("go", "rust"))
        edit_body(go if side == "go" else rust, old, new)
        (run / case / f"{side}.json").write_text(json.dumps(go if side == "go" else rust))
        (run / case / "diff.json").write_text(json.dumps(diff.differences(diff.comparable(go), diff.comparable(rust))))
        result = acc.evaluate(run, pins)
        verdict = next(c for c in result["cases"] if c["name"] == case)["verdict"]
        print(f"[{label}] {target} {case} {side} {old!r} -> {new!r}: case {verdict}, run {result['verdict']}")
    original = [json.loads((receipts_dir / f).read_text()) for f in FILES]
    source = original[0]["source"]

    def combined(items):
        with tempfile.TemporaryDirectory() as tmp:
            paths = []
            for i, item in enumerate(items):
                path = Path(tmp) / f"{i}.json"
                path.write_text(json.dumps(item))
                paths.append(str(path))
            return acc.combine(paths, pins, current_source=source)["verdict"]
    print(f"[{label}] combine baseline ({receipts_dir.name}): {combined(copy.deepcopy(original))}")
    for name, edit in COMBINE.items():
        items = copy.deepcopy(original)
        edit(items)
        print(f"[{label}] combine {name}: {combined(items)}")
