"""Complete per-side expectations ("goldens") for harness/judge.py (issue #67, H1).

A golden records everything one side of a case sends and answers in the dimensions it
covers: every upstream request (line, ordered headers, body), every client reply (status,
reason, body) and every TLS handshake. Each value is a template. Text is literal, and
only `{{mask:NAME}}` tokens may vary; each one must match its narrow pattern from the
manifest's `masks`. A golden is recorded from a pinned Go capture and keeps that
capture's provenance. It is never recorded from cliproxy-rs output. Each recorded upstream
request also keeps the collector's `write_result`: a golden replacement must not turn an
unfinished or failed upstream write into accepted evidence.

The mask definitions here are the only value formats a golden or a generated JSON value
(judge.py `downstream_json.generated`) may leave open; there is no second, looser predicate.

    golden.py record RUN_DIR CASE --side go --covers upstream,status,downstream-body,tls --out FILE
"""
import argparse
import base64
import binascii
import json
from pathlib import Path
import re
import sys

TOKEN = re.compile(r"\{\{mask:([a-z0-9-]+)\}\}")
TLS_FIELDS = ("record_layout", "profile", "negotiated_alpn", "negotiated_version", "session_reused")
COVERS = {"upstream", "status", "downstream-body", "tls"}
REQUEST_FIELDS = {"request_line", "headers", "body", "write_result"}
JSON_TYPES = {"string": str, "integer": int}
# crates/cpa-exec/src/compaction_capsule.rs: the prefix, then unpadded URL-safe base64 of a
# 12-byte nonce, the ciphertext and a 16-byte tag. Only this structure is checked, never the
# random ciphertext.
CAPSULE_PREFIX = "cpa-ag-compact-v1:"
CAPSULE_MIN_BYTES = 12 + 16


def masks(pins):
    return {k: v for k, v in pins["masks"].items() if k != "about"}


def _capsule_problem(text):
    payload = text[len(CAPSULE_PREFIX):] if text.startswith(CAPSULE_PREFIX) else None
    if payload is None or not re.fullmatch(r"[A-Za-z0-9_-]*", payload) or len(payload) % 4 == 1:
        return "is not the capsule prefix and unpadded URL-safe base64"
    try:
        sealed = base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4))
    except (binascii.Error, ValueError):
        return "is not decodable base64"
    if base64.urlsafe_b64encode(sealed).decode().rstrip("=") != payload:
        return "is not canonical unpadded base64"
    if len(sealed) < CAPSULE_MIN_BYTES:
        return f"decodes to {len(sealed)} bytes, fewer than a nonce and a tag"
    return None


DECODERS = {"compaction-capsule": _capsule_problem}


def text_problem(spec, text):
    """Why a mask value's text does not have the mask's format, or None."""
    if not isinstance(text, str) or not re.fullmatch(spec["pattern"], text):
        return f"{text!r} is not {spec['pattern']}"
    if "decode" in spec:
        problem = DECODERS[spec["decode"]](text)
        if problem:
            return f"{text[:40]!r}... {problem}"
    return None


def json_problem(spec, value):
    """Why a JSON value is not the mask's exact JSON type and format, or None. The type is
    checked first and exactly: a digit string is not an integer, and a boolean is neither."""
    want = JSON_TYPES.get(spec.get("json_type"))
    if want is None:
        return "the mask declares no JSON type"
    if type(value) is not want:
        return f"{value!r} is a JSON {type(value).__name__}, not a {spec['json_type']}"
    return text_problem(spec, value if want is str else str(value))


def compile_template(template, mask_specs):
    """(full-match regex, mask name per group) for a template; unknown masks are an error."""
    parts, names, pos = [], [], 0
    for match in TOKEN.finditer(template):
        parts.append(re.escape(template[pos:match.start()]))
        name = match.group(1)
        if name not in mask_specs:
            raise ValueError(f"unknown mask {name}")
        parts.append(f"({mask_specs[name]['pattern']})")
        names.append(name)
        pos = match.end()
    parts.append(re.escape(template[pos:]))
    return re.compile("".join(parts), re.S), names


def matches(template, value, mask_specs):
    """The value is the template's literal text with each mask holding a valid value."""
    if value is None:
        return False
    pattern, names = compile_template(template, mask_specs)
    found = pattern.fullmatch(value)
    return found is not None and all(text_problem(mask_specs[name], found.group(i + 1)) is None
                                     for i, name in enumerate(names))


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
            if not isinstance(want.get("write_result"), str) or have.get("write_result") != want["write_result"]:
                problems.append(f"request {i}: upstream write {have.get('write_result')!r} is not the golden's "
                                f"{want.get('write_result')!r}")
    if covers & {"status", "downstream-body"}:
        expected, got = golden["replies"], observation["downstream"]
        if len(expected) != len(got):
            problems.append(f"{len(got)} client replies, golden has {len(expected)}")
        for i, (want, have) in enumerate(zip(expected, got)):
            if "status" in covers and not (type(have.get("status")) is int and have.get("status") == want["status"]
                                           and have.get("reason") == want["reason"]):
                problems.append(f"reply {i}: {have.get('status')} {have.get('reason')} is not {want['status']} {want['reason']}")
            if "downstream-body" in covers and not matches(want["body"], _text(have.get("body_hex")), mask_specs):
                problems.append(f"reply {i}: body differs from the golden")
    if "tls" in covers:
        got = [{k: h.get(k) for k in TLS_FIELDS} for h in observation["upstream"]["tls"]]
        if not same(got, golden["tls"]):
            problems.append("TLS handshakes differ from the golden")
    return problems


def same(left, right):
    """JSON equality with exact types: 0 is not false, 1 is not 1.0."""
    if type(left) is not type(right):
        return False
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(same(left[k], right[k]) for k in left)
    if isinstance(left, (list, tuple)):
        return len(left) == len(right) and all(same(a, b) for a, b in zip(left, right))
    return left == right


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
                            "body": _mask_text(_text(r["body_hex"]), mask_specs, body_masks),
                            "write_result": r["write_result"]}
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
    import manifest as revisions  # manifest validation imports this module
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
