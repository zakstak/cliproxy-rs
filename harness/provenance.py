"""Source and build identity for harness evidence (issue #67, H1).

A receipt claims what ran only if the identity was taken before execution and
confirmed unchanged after it. Identity covers:

- the Rust source: HEAD, its tree, whether the worktree was clean (untracked files
  included, ignored ones not), and the manifest's SHA-256;
- each executed binary's SHA-256, plus cargo's own freshness report. Cargo must call
  the exact executable fresh for the current source, both before and after the run;
- the Go binary's embedded VCS stamp (`go version -m`). It must name the manifest pin
  with no local modifications, and the checkout must stay clean at that pin.

    provenance.py pre --target T --go-binary B --go-reference DIR --out FILE
    provenance.py post --pre FILE --run RUN_DIR      # exit 2 unless verified
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

import manifest as revisions

ROOT = revisions.ROOT


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def _git(*args, cwd=ROOT):
    return subprocess.check_output(["git", "-C", str(cwd), *args]).decode().strip()


def source_identity(root=ROOT):
    status = _git("status", "--porcelain", cwd=root)
    return {"head": _git("rev-parse", "HEAD", cwd=root), "tree": _git("rev-parse", "HEAD^{tree}", cwd=root),
            "clean": not status, "status": status.splitlines()[:20],
            "manifest_sha256": sha256(Path(root) / "harness/manifest.json")}


def package_name(package_id):
    """`path+file:///x/cpa-server#0.4.1` or `...#cpa-server@0.4.1` -> `cpa-server`."""
    url, _, fragment = package_id.partition("#")
    if "@" in fragment:
        return fragment.split("@")[0]
    if not fragment:  # pre-1.77 format: "name version (source)"
        return package_id.split()[0]
    return url.rstrip("/").rsplit("/", 1)[-1]


def cargo_artifacts(args, root=ROOT, env=None):
    """(exit code, [{package, kind, name, executable, fresh, sha256}]) for executables."""
    command = ["cargo", *args, "--message-format=json"]
    done = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True)
    artifacts = []
    for line in done.stdout.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        target = message["target"]
        name = package_name(message["package_id"])
        executable = message["executable"]
        artifacts.append({"package": name, "kind": target["kind"][0], "name": target["name"],
                          "executable": os.path.relpath(executable, root), "fresh": message.get("fresh"),
                          "sha256": sha256(executable)})
    return {"command": command, "exit": done.returncode, "stderr_tail": done.stderr[-2000:] if done.returncode else "",
            "artifacts": artifacts}


def go_binary(binary, checkout):
    info = {"path": os.path.relpath(binary, ROOT), "sha256": sha256(binary)}
    try:
        stamp = subprocess.check_output(["go", "version", "-m", str(binary)]).decode()
    except (OSError, subprocess.CalledProcessError) as exc:
        stamp = ""
        info["stamp_error"] = str(exc)
    for line in stamp.splitlines():
        parts = line.strip().split("\t")
        if len(parts) == 2 and parts[0] == "build" and parts[1].startswith("vcs."):
            key, _, value = parts[1].partition("=")
            info[key] = value
    info["checkout_head"] = _git("rev-parse", "HEAD", cwd=checkout)
    info["checkout_clean"] = not _git("status", "--porcelain", cwd=checkout)
    return info


DRIVER_BUILD = ["build", "--locked", "--manifest-path", "harness/rust/Cargo.toml"]


def driver(build):
    found = [a for a in build["artifacts"] if a["name"] == "cpa-differential-driver" and a["kind"] == "bin"]
    return found[0] if len(found) == 1 else None


def pre(target, go_bin, go_checkout):
    pins = revisions.load(revisions.suite_sizes())
    build = cargo_artifacts(DRIVER_BUILD)
    return {"target": target, "go_pin": pins["go"]["targets"][target]["commit"],
            "source": source_identity(), "driver_build": build, "driver": driver(build),
            "go": go_binary(go_bin, go_checkout), "go_checkout": os.path.relpath(go_checkout, ROOT)}


def verify(before, after):
    """Problems that make a run's provenance unverifiable."""
    problems = []
    if before["source"] != after["source"]:
        problems.append("Rust source or manifest changed during the run")
    for label, record in (("before", before), ("after", after)):
        if record["driver_build"]["exit"] != 0 or record["driver"] is None:
            problems.append(f"driver build {label} the run failed or produced no single executable")
        elif record["driver"]["fresh"] is not True and label == "after":
            problems.append("cargo rebuilt the driver after the run: the tested binary was stale")
    if before["driver"] and after["driver"] and before["driver"]["sha256"] != after["driver"]["sha256"]:
        problems.append("driver executable changed during the run")
    go_before, go_after = before["go"], after["go"]
    if go_before["sha256"] != go_after["sha256"]:
        problems.append("Go binary changed during the run")
    for record in (go_before, go_after):
        if record.get("vcs.revision") != before["go_pin"] or record.get("vcs.modified") != "false":
            problems.append(f"Go binary stamp {record.get('vcs.revision')} modified={record.get('vcs.modified')} "
                            f"is not the clean pin {before['go_pin']}")
        if record["checkout_head"] != before["go_pin"] or not record["checkout_clean"]:
            problems.append("Go checkout is not clean at the pin")
    return sorted(set(problems))


def post(pre_file, run_dir):
    before = json.loads(Path(pre_file).read_text())
    build = cargo_artifacts(DRIVER_BUILD)
    after = {**before, "source": source_identity(), "driver_build": build, "driver": driver(build),
             "go": go_binary(ROOT / before["go"]["path"], ROOT / before["go_checkout"])}
    problems = verify(before, after)
    summary_path = Path(run_dir) / "summary.json"
    summary = json.loads(summary_path.read_text())
    executed = summary.get("executed", {})
    hashed_driver = before["driver"]["executable"] if before["driver"] else None
    if hashed_driver is None or executed.get("driver") != hashed_driver:
        problems.append("the collector ran a different driver than the one hashed")
    if executed.get("go_binary") != before["go"]["path"]:
        problems.append("the collector ran a different Go binary than the one hashed")
    summary["provenance"] = {"before": before, "after": after, "verified": not problems, "problems": problems}
    summary_path.write_text(json.dumps(summary, indent=2, ensure_ascii=False) + "\n")
    return problems


def main(argv):
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("pre")
    p.add_argument("--target", required=True)
    p.add_argument("--go-binary", type=Path, required=True)
    p.add_argument("--go-reference", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    q = sub.add_parser("post")
    q.add_argument("--pre", type=Path, required=True)
    q.add_argument("--run", type=Path, required=True)
    args = parser.parse_args(argv)
    if args.command == "pre":
        record = pre(args.target, args.go_binary.resolve(), args.go_reference.resolve())
        args.out.write_text(json.dumps(record, indent=2) + "\n")
        if record["driver"] is None or record["driver_build"]["exit"] != 0:
            print("driver build failed", file=sys.stderr)
            return 2
        print(record["driver"]["executable"])
        return 0
    problems = post(args.pre, args.run)
    for problem in problems:
        print(f"provenance: {problem}", file=sys.stderr)
    return 2 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
