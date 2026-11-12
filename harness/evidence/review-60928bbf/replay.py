"""Old-head witnesses for the 60928bbf review findings (saved replay, not a fresh run).

    python3 -I replay.py REPO OLD_TREE OLD_RECEIPTS NEW_RECEIPTS

OLD_TREE is `git archive 60928bbf` of the harness and the pin sources it reads. The capture
probes apply the reviewer's edits to 60928bbf's own committed captures
(harness/evidence/2026-11-12-*.json.gz, recorded by the unchanged collector) and evaluate
the whole capture with each harness. The combine probes apply the reviewer's record edits to
a complete receipt set: the reviewer's fresh 60928bbf receipts for the reviewed harness, and
this revision's fresh receipts for the fixed one, each in its own schema. Each unedited
baseline passes under its own harness, so a failure is the targeted check, not a schema
mismatch. The fixed harness's combine also requires each recorded test executable on disk
with its hash, so its combine probes reproduce only while the tested binaries are still in
target/.
"""
import copy, gzip, importlib, inspect, json, re, shutil, sys, tempfile
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
    run = Path(tempfile.mkdtemp()) / f"run-{target}"
    run.mkdir()
    (run / "summary.json").write_text(json.dumps(archive["summary"]))
    for case in archive["cases"]:
        d = run / case["fixture"]["name"]
        d.mkdir()
        for part in ("fixture", "go", "rust", "diff"):
            (d / f"{part}.json").write_text(json.dumps(case[part]))
    return run


def reply_regex(observation, pattern, replacement):
    """The reviewer's consistent reply edit: body, body text, raw response and Content-Length."""
    reply = observation["downstream"][0]
    body, n = re.subn(pattern.encode(), replacement.encode(), bytes.fromhex(reply["body_hex"]), count=1)
    assert n == 1, pattern
    reply["body_hex"], reply["body_text"] = body.hex(), body.decode()
    reply["headers"] = [[k, str(len(body)) if k.lower() == "content-length" else v] for k, v in reply["headers"]]
    head = bytes.fromhex(reply["raw_response_hex"]).partition(b"\r\n\r\n")[0]
    head = re.sub(rb"(?im)^(content-length:[ \t]*)[0-9]+", lambda m: m.group(1) + str(len(body)).encode(), head)
    reply["raw_response_hex"] = (head + b"\r\n\r\n" + body).hex()


def inference_write(value):
    def edit(observation):
        [r for r in observation["upstream"]["http"] if r["request_line"].startswith("POST /v1/messages")][0]["write_result"] = value
    return edit


def disabled(value):
    return lambda observation: observation["credential_after"].update(disabled=value)


CREATED = (r'"created_at":([0-9]+)', r'"created_at":"\1"')
CAPSULE = (r"cpa-ag-compact-v1:[A-Za-z0-9_-]+", "cpa-ag-compact-v1:A")
# (finding, target, case, label, {side: edit})
CAPTURE = [
    ("2", "selective", "compact-claude", "Go created_at integer -> digit string", {"go": lambda o: reply_regex(o, *CREATED)}),
    ("2", "selective", "compact-claude", "Rust created_at integer -> digit string", {"rust": lambda o: reply_regex(o, *CREATED)}),
    ("2", "selective", "compact-claude", "Go capsule cpa-ag-compact-v1:A", {"go": lambda o: reply_regex(o, *CAPSULE)}),
    ("2", "selective", "compact-claude", "Rust capsule cpa-ag-compact-v1:A", {"rust": lambda o: reply_regex(o, *CAPSULE)}),
    ("3", "selective", "client-disconnect", "Rust disabled false -> 0", {"rust": disabled(0)}),
    ("3", "selective", "client-disconnect", "Go disabled absent -> 0", {"go": disabled(0)}),
    ("3", "selective", "client-disconnect", "both sides disabled true", {"go": disabled(True), "rust": disabled(True)}),
    ("4", "historical", "compact-claude", "Rust upstream write BrokenPipeError", {"rust": inference_write("BrokenPipeError")}),
    ("4", "selective", "native-signals", "C2 Go upstream write BrokenPipeError", {"go": inference_write("BrokenPipeError")}),
    ("4", "selective", "native-signals", "C2 Rust upstream write BrokenPipeError", {"rust": inference_write("BrokenPipeError")}),
    ("types", "historical", "client-disconnect", "Rust disabled false -> 0 (no predicate)", {"rust": disabled(0)}),
    ("types", "selective", "compact-claude", "Rust input_tokens 13 -> 13.0",
     {"rust": lambda o: reply_regex(o, r'"input_tokens":13\b', '"input_tokens":13.0')}),
]


def everywhere(execution, old, new):
    """Replace an executable path in every field of one execution record (either schema)."""
    return json.loads(json.dumps(execution).replace(json.dumps(old)[1:-1], json.dumps(new)[1:-1]))


def executable(execution):
    return execution["artifact"]["executable"] if "artifact" in execution else execution["executable"]


def swap_executable(r):
    first, second = r[4]["executions"][0], r[4]["executions"][1]
    r[4]["executions"][0] = everywhere(first, executable(first), executable(second))


COMBINE = {
    "1 another target's existing executable, same hash": swap_executable,
    "1 another target's execution under these labels": lambda r: r[4]["executions"].__setitem__(0, {
        **copy.deepcopy(r[4]["executions"][1]), "package": r[4]["executions"][0]["package"],
        "target": r[4]["executions"][0]["target"]}),
    "1 missing/failed/ignored/not_run removed from all nine": lambda r: [
        e.pop(k, None) for e in r[4]["executions"] for k in ("missing", "failed", "ignored", "not_run")],
    "1 nonexistent executable target/does-not-exist": lambda r: r[4]["executions"].__setitem__(
        0, everywhere(r[4]["executions"][0], executable(r[4]["executions"][0]), "target/does-not-exist")),
    "rust_revision removed": lambda r: r[1].pop("rust_revision"),
}
FILES = ["differential-historical.json", "differential-selective.json", "mutation-historical.json",
         "mutation-selective.json", "cargo-groups.json"]


for label, base, receipts_dir in (("reviewed 60928bbf", OLD, OLD_RECEIPTS), ("fixed", REPO, NEW_RECEIPTS)):
    acc = harness(base)
    pins = acc.revisions.load(acc.revisions.suite_sizes())
    diff = sys.modules["differential"]
    runs = {t: capture(t) for t in ("historical", "selective")}
    for target, run in runs.items():
        print(f"[{label}] baseline {target} capture: {acc.evaluate(run, pins)['verdict']}")
    for finding, target, case, name, edits in CAPTURE:
        run = Path(tempfile.mkdtemp()) / runs[target].name
        shutil.copytree(runs[target], run)
        sides = {s: json.loads((run / case / f"{s}.json").read_text()) for s in ("go", "rust")}
        for side, edit in edits.items():
            edit(sides[side])
            (run / case / f"{side}.json").write_text(json.dumps(sides[side]))
        (run / case / "diff.json").write_text(json.dumps(diff.differences(diff.comparable(sides["go"]),
                                                                          diff.comparable(sides["rust"]))))
        result = acc.evaluate(run, pins)
        verdict = next(c for c in result["cases"] if c["name"] == case)["verdict"]
        print(f"[{label}] finding {finding}: {target} {case} {name}: case {verdict}, run {result['verdict']} "
              f"{result['counts']['passed']}/{result['counts']['run']}")
    original = [json.loads((receipts_dir / f).read_text()) for f in FILES]
    source = original[0]["source"]
    extra = {"artifact_root": REPO} if "artifact_root" in inspect.signature(acc.combine).parameters else {}

    def combined(items):
        with tempfile.TemporaryDirectory() as tmp:
            paths = []
            for i, item in enumerate(items):
                path = Path(tmp) / f"{i}.json"
                path.write_text(json.dumps(item))
                paths.append(str(path))
            return acc.combine(paths, pins, current_source=source, **extra)["verdict"]
    print(f"[{label}] combine baseline ({receipts_dir.name}): {combined(copy.deepcopy(original))}")
    for name, edit in COMBINE.items():
        items = copy.deepcopy(original)
        edit(items)
        print(f"[{label}] combine {name}: {combined(items)}")
