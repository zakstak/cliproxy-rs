"""Complete per-side expectations ("goldens") for harness/judge.py (issue #67, H1).

A golden records everything one side of a case sends and answers in the dimensions it
covers: every upstream request (line, ordered headers, body), every client reply (status,
reason, body) and every TLS handshake. Each value is a template. Text is literal, and
only `{{mask:NAME}}` tokens may vary; each one must match its narrow pattern from the
manifest's `masks`. A golden is recorded from a pinned Go capture and keeps that
capture's provenance. It is never recorded from cliproxy-rs output.

    golden.py record RUN_DIR CASE --side go --covers upstream,status,downstream-body,tls --out FILE
"""
import argparse
import json
from pathlib import Path
import re
import sys

import manifest as revisions

TOKEN = re.compile(r"\{\{mask:([a-z0-9-]+)\}\}")
TLS_FIELDS = ("record_layout", "profile", "negotiated_alpn", "negotiated_version", "session_reused")
COVERS = {"upstream", "status", "downstream-body", "tls"}


def masks(pins):
    return {k: v for k, v in pins["masks"].items() if k != "about"}


def compile_template(template, mask_specs):
    """A full-match regex for a template; unknown masks are an error."""
    parts, pos = [], 0
    for match in TOKEN.finditer(template):
        parts.append(re.escape(template[pos:match.start()]))
        name = match.group(1)
        if name not in mask_specs:
            raise ValueError(f"unknown mask {name}")
        parts.append(f"(?:{mask_specs[name]['pattern']})")
        pos = match.end()
    parts.append(re.escape(template[pos:]))
    return re.compile("".join(parts), re.S)


def matches(template, value, mask_specs):
    return value is not None and compile_template(template, mask_specs).fullmatch(value) is not None


def _text(hex_value):
    try:
        return bytes.fromhex(hex_value or "").decode("utf-8")
    except UnicodeDecodeError:
        return None


def check(golden, covers, observation, is_inference, mask_specs):
    """Problems with one side of a case against a golden, for the covered dimensions."""
    problems = []
    if "upstream" in covers:
        expected, got = golden["requests"], observation["upstream"]["http"]
        if len(expected) != len(got):
            problems.append(f"{len(got)} upstream requests, golden has {len(expected)}")
        for i, (want, have) in enumerate(zip(expected, got)):
            if have["request_line"] != want["request_line"]:
                problems.append(f"request {i}: line {have['request_line']!r} is not {want['request_line']!r}")
            names = [k for k, _ in have["headers"]]
            if names != [k for k, _ in want["headers"]]:
                problems.append(f"request {i}: header names or order differ from the golden")
            else:
                for (name, template), (_, value) in zip(want["headers"], have["headers"]):
                    if not matches(template, value, mask_specs):
                        problems.append(f"request {i}: header {name} differs from the golden")
            if not matches(want["body"], _text(have["body_hex"]), mask_specs):
                problems.append(f"request {i}: body differs from the golden")
    if covers & {"status", "downstream-body"}:
        expected, got = golden["replies"], observation["downstream"]
        if len(expected) != len(got):
            problems.append(f"{len(got)} client replies, golden has {len(expected)}")
        for i, (want, have) in enumerate(zip(expected, got)):
            if "status" in covers and (have.get("status"), have.get("reason")) != (want["status"], want["reason"]):
                problems.append(f"reply {i}: {have.get('status')} {have.get('reason')} is not {want['status']} {want['reason']}")
            if "downstream-body" in covers and not matches(want["body"], _text(have.get("body_hex")), mask_specs):
                problems.append(f"reply {i}: body differs from the golden")
    if "tls" in covers:
        got = [{k: h.get(k) for k in TLS_FIELDS} for h in observation["upstream"]["tls"]]
        if got != golden["tls"]:
            problems.append("TLS handshakes differ from the golden")
    return problems


def _mask_text(text, mask_specs, body_masks):
    for name, spec in mask_specs.items():
        if "record" in spec and name in body_masks:
            text = re.sub(spec["record"], "{{mask:" + name + "}}", text)
    return text


def record(run_dir, case, side, covers, pins, body_masks=()):
    run_dir = Path(run_dir)
    summary = json.loads((run_dir / "summary.json").read_text())
    observation = json.loads((run_dir / case / f"{side}.json").read_text())
    mask_specs = masks(pins)
    header_masks = {h: name for name, spec in mask_specs.items() for h in spec.get("headers", [])}
    if side != "go":
        raise ValueError("goldens come from the Go side of a pinned capture")
    if not (summary.get("provenance") or {}).get("verified"):
        raise ValueError("the capture's provenance is not verified")
    out = {"about": "", "provenance": {"go_tag": summary["tag"], "go_commit": summary["reference"],
                                       "target": summary["target"], "run": run_dir.name, "case": case, "side": side,
                                       "go_binary_sha256": summary["provenance"]["before"]["go"]["sha256"]},
           "covers": sorted(covers), "body_masks": sorted(body_masks)}
    if "upstream" in covers:
        out["requests"] = [{"request_line": r["request_line"],
                            "headers": [[k, "{{mask:" + header_masks[k.lower()] + "}}" if k.lower() in header_masks else v]
                                        for k, v in r["headers"]],
                            "body": _mask_text(_text(r["body_hex"]), mask_specs, body_masks)}
                           for r in observation["upstream"]["http"]]
    if covers & {"status", "downstream-body"}:
        out["replies"] = [{"status": r["status"], "reason": r["reason"],
                           "body": _mask_text(_text(r.get("body_hex")), mask_specs, body_masks)}
                          for r in observation["downstream"]]
    if "tls" in covers:
        out["tls"] = [{k: h.get(k) for k in TLS_FIELDS} for h in observation["upstream"]["tls"]]
    # The golden must match the capture it came from.
    problems = check(out, set(covers), observation, None, mask_specs)
    if problems:
        raise ValueError(f"recorded golden does not match its capture: {problems}")
    return out


def main(argv):
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    rec = sub.add_parser("record")
    rec.add_argument("run", type=Path)
    rec.add_argument("case")
    rec.add_argument("--side", default="go")
    rec.add_argument("--covers", required=True)
    rec.add_argument("--about", required=True)
    rec.add_argument("--masks", default="", help="comma-separated body masks to apply (none by default)")
    rec.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    covers = set(args.covers.split(","))
    if not covers <= COVERS:
        print(f"covers must be within {sorted(COVERS)}", file=sys.stderr)
        return 2
    body_masks = [m for m in args.masks.split(",") if m]
    golden = record(args.run, args.case, args.side, covers, revisions.load(), body_masks)
    golden["about"] = args.about
    args.out.write_text(json.dumps(golden, indent=2, ensure_ascii=False) + "\n")
    print(args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
