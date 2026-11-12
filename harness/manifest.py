"""The harness revision manifest (harness/manifest.json): loading and validation.

Every Go, first-party and toolchain pin the harness uses comes from the manifest. The
runner, the collector and the acceptance command read pins only through `load()`,
which refuses a manifest whose pins disagree with the sources they claim to mirror.

    python3 harness/manifest.py check          # validate, exit 1 on a problem
    python3 harness/manifest.py shell TARGET [--golden]   # shell assignments for harness/run
"""
import fnmatch
import json
from pathlib import Path
import re
import shlex
import sys

import golden as goldens

ROOT = Path(__file__).resolve().parent.parent
PATH = ROOT / "harness" / "manifest.json"
SHA = re.compile(r"^[0-9a-f]{40}$")
ROLES = {"historical-recording", "frozen-default-reference", "selective-comparison"}
DIVERGENCE_KINDS = {
    # A v8.0.67 behaviour an issue in the approved core batch owns; not yet in Rust.
    "pending-feature",
    # A v8.0.67 behaviour an issue outside the core batch owns (deferred or follow-up).
    "deferred-feature",
    # Rust deliberately differs, recorded in docs/DIFFERENCES-FROM-GO.md or a contract.
    "documented-difference",
    # Values minted per request or nondeterministic upstream ordering, with evidence.
    "nondeterministic",
    # Historical target only: Rust follows the v8.0.35 policy reference, not v8.0.10.
    "later-reference",
    # The harness itself makes the sides differ (each proxy's upstream address label).
    "harness-boundary",
    # The Go commit is identified but no tracker issue owns the behaviour.
    "unowned",
    # Neither a commit nor an owner is established; the attribution says what is known.
    "unattributed",
    # An owned, unresolved discrepancy with the frozen reference. The audit ledger
    # records what is missing; this neither adopts behaviour nor declares a policy.
    "missing-audit",
    # Selective only: the tracker decided not to adopt this v8.0.67 behaviour in this batch.
    # cliproxy-rs keeps the frozen v8.0.35 behaviour. Names the Go commit (evidence), the
    # deciding issue, and the executed v8.0.35 comparison that shows Rust matches it
    # (attribution). The predicate stays exact; this is a decision record, not an exemption.
    "not-adopted",
}
PREDICATES = {"downstream_header", "upstream_header", "upstream_body_edit", "downstream_body_edit",
              "downstream_body_timestamps",
              "downstream_json", "tls_extra_go_handshakes", "credential_field", "credential_timestamps", "replace"}
REPLACE_COVERS = {"status", "downstream-body", "upstream", "tls"}
SIDE_KEYS = {"golden"}
DIVERGENCE_FIELDS = {"id", "kind", "targets", "cases", "except_cases", "expect", "reason", "issue", "ledger",
                     "evidence", "attribution", "may_be_absent"}
# Removed in the predicate schema: they exempted whole paths or assertions.
FORBIDDEN_FIELDS = {"paths", "assertions", "header_names", "upstream_header_names", "upstream_body_patterns"}


def _value_spec(spec, where):
    _require(spec is None or (isinstance(spec, dict) and len(spec) == 1 and set(spec) <= {"equals", "regex"}),
             f"{where}: a value spec is null (absent), {{equals}} or {{regex}}")
    if spec and "regex" in spec:
        re.compile(spec["regex"])


def _golden(path, where, data=None):
    """A golden file: pinned Go provenance, complete content for what it covers, known masks."""
    golden = json.loads((ROOT / path).read_text())
    manifest = data or json.loads(PATH.read_text())
    commits = {t["commit"] for t in manifest["go"]["targets"].values()}
    masks = {k for k in manifest.get("masks", {}) if k != "about"}
    _require(golden["provenance"]["go_commit"] in commits and golden["provenance"]["side"] == "go",
             f"{where}: {path} must come from the Go side of a pinned capture")
    _require(set(golden["covers"]) <= {"upstream", "status", "downstream-body", "tls"}, f"{where}: {path} covers")
    needed = {"upstream": "requests", "status": "replies", "downstream-body": "replies", "tls": "tls"}
    for cover in golden["covers"]:
        _require(needed[cover] in golden, f"{where}: {path} covers {cover} but has no {needed[cover]}")
    for request in golden.get("requests", []):
        _require(set(request) == goldens.REQUEST_FIELDS and isinstance(request["write_result"], str),
                 f"{where}: {path} requests need exactly {sorted(goldens.REQUEST_FIELDS)}, with the upstream write result")
    text = json.dumps(golden.get("requests", [])) + json.dumps(golden.get("replies", []))
    used = set(re.findall(r"\{\{mask:([a-z0-9-]+)\}\}", text))
    _require(used <= masks, f"{where}: {path} uses unknown masks {sorted(used - masks)}")
    _require(used <= set(golden.get("body_masks", [])) | {"request-id"}, f"{where}: {path} masks more than it declares")
    return golden


def _predicate(predicate, where, divergence, data):
    _require(isinstance(predicate, dict) and len(predicate) == 1 and set(predicate) <= PREDICATES,
             f"{where}: one predicate of {sorted(PREDICATES)}")
    (kind, spec), = predicate.items()
    if kind in ("downstream_header", "upstream_header"):
        _require(spec["name"] == spec["name"].lower(), f"{where}: header names are lowercase")
        _require(spec["name"] not in ("authorization", "x-api-key", "api-key", "cookie", "set-cookie"),
                 f"{where}: credential headers cannot be a divergence")
        if "relation" in spec:
            _require(set(spec) == {"name", "relation"} and kind == "upstream_header", f"{where}: relation form")
        else:
            _require(set(spec) == {"name", "go", "rust"} and spec["go"] != spec["rust"],
                     f"{where}: needs distinct go and rust value specs")
            _value_spec(spec["go"], where)
            _value_spec(spec["rust"], where)
    elif kind in ("upstream_body_edit", "downstream_body_edit"):
        _require(set(spec) == {"go", "rust", "count"}, f"{where}: needs go, rust and count")
        _require(spec["go"] is not None or spec["rust"] is not None, f"{where}: an edit names a pattern")
        _require(spec["count"] == "same" and spec["go"] and spec["rust"] or isinstance(spec["count"], int)
                 and spec["count"] > 0, f"{where}: count is a positive integer, or 'same' with both patterns")
        for pattern in (spec["go"], spec["rust"]):
            if pattern:
                re.compile(pattern)
                _require(not re.fullmatch(pattern, "") and ".*" != pattern, f"{where}: pattern must be specific")
    elif kind == "downstream_body_timestamps":
        _require(set(spec) == {"pattern", "max_skew_seconds"} and 0 < spec["max_skew_seconds"] <= 60,
                 f"{where}: needs a pattern and a skew of at most 60 seconds")
        _require(re.compile(spec["pattern"]).groups == 1 and "([0-9]{10})" in spec["pattern"],
                 f"{where}: the pattern captures exactly one 10-digit Unix time")
    elif kind == "downstream_json":
        _require(set(spec) <= {"go_only", "rust_only", "unordered", "generated"} and spec, f"{where}: json form")
        masks = data.get("masks", {})
        for item in spec.get("generated", []):
            # A generated value names a shared mask: one narrow, typed format definition for
            # goldens and generated values alike, never a parallel looser regex.
            _require(isinstance(item, dict) and set(item) == {"pointer", "mask"} and item["mask"] != "about"
                     and item["mask"] in masks and masks[item["mask"]].get("json_type"),
                     f"{where}: a generated value names a pointer and a typed mask")
    elif kind == "tls_extra_go_handshakes":
        _require(isinstance(spec["count"], int) and spec["count"] > 0 and set(spec["reference"]) == {"case", "side", "index"},
                 f"{where}: needs a count and a reference handshake")
    elif kind == "credential_field":
        _require({"field", "go", "rust"} <= set(spec) <= {"field", "go", "rust", "equal_values"},
                 f"{where}: credential field needs go/rust and optional explicit equal values")
        _require(spec["field"] not in ("access_token", "refresh_token", "id_token", "type", "email"),
                 f"{where}: credential identity fields cannot diverge")
        _require(spec["go"] != spec["rust"], f"{where}: go and rust values must differ")
        _require(not divergence.get("may_be_absent") or isinstance(spec.get("equal_values"), list)
                 and spec["equal_values"], f"{where}: optional credential delta needs explicit equal states")
        if spec["field"] == "disabled":
            _require(all(v is False or isinstance(v, str) and v == "<absent>"
                         for v in [spec["go"], spec["rust"], *spec.get("equal_values", [])]),
                     f"{where}: disabled is exactly false or absent")
    elif kind == "credential_timestamps":
        _require(set(spec["fields"]) <= {"expired", "last_refresh"} and 0 < spec["max_skew_seconds"] <= 60,
                 f"{where}: timestamps are expired or last_refresh within 60s")
    elif kind == "replace":
        _require(set(spec["covers"]) <= REPLACE_COVERS and spec["covers"], f"{where}: covers {sorted(REPLACE_COVERS)}")
        for side in ("go", "rust"):
            _require(set(spec[side]) == SIDE_KEYS, f"{where}: {side} names exactly one complete golden")
            recorded = _golden(spec[side]["golden"], where, data)
            _require(set(spec["covers"]) <= set(recorded["covers"]), f"{where}: {side} golden does not cover {spec['covers']}")
        _require(not divergence.get("may_be_absent"), f"{where}: a replaced comparison cannot be optional")


class ManifestError(ValueError):
    pass


def _require(condition, message):
    if not condition:
        raise ManifestError(message)


def _ci_rust_toolchain():
    text = (ROOT / ".github/workflows/ci.yml").read_text()
    match = re.search(r'^\s*RUST_TOOLCHAIN:\s*"([^"]+)"', text, re.M)
    return match.group(1) if match else None


def _codex_constant():
    text = (ROOT / "crates/cpa-exec/src/codex_contract.rs").read_text()
    match = re.search(r'pub const FIRST_PARTY_COMMIT: &str = "([0-9a-f]{40})";', text)
    return match.group(1) if match else None


def validate(data, suite_sizes=None):
    """Raise ManifestError for the first inconsistency. `suite_sizes` maps suite name
    to the number of cases the fixtures define, when the caller has them."""
    _require(data.get("schema") == 1, "schema must be 1")
    targets = data["go"]["targets"]
    _require(set(targets) == {"historical", "policy", "selective"},
             "go targets must be exactly historical, policy and selective")
    commits = [t["commit"] for t in targets.values()]
    _require(len(set(commits)) == len(commits), "each Go target needs its own commit")
    tags = [t["tag"] for t in targets.values()]
    _require(len(set(tags)) == len(tags), "each Go target needs its own tag")
    for name, target in targets.items():
        _require(SHA.match(target["commit"]), f"{name}: commit must be a full 40-hex SHA")
        _require(target["role"] in ROLES, f"{name}: unknown role {target['role']}")
        _require(isinstance(target["executed"], bool), f"{name}: executed must be a boolean")
        _require((target["suite"] is not None) == target["executed"],
                 f"{name}: an executed target names its suite, a recorded-only one does not")
        if target["suite"] is not None:
            _require(target["suite"] in data["suites"], f"{name}: unknown suite {target['suite']}")
    _require(targets["policy"]["commit"] == "ae3ee3be303fb07fb8173279923413005bf32177",
             "policy target must stay AGENTS.md's frozen v8.0.35 reference")
    _require(targets["policy"]["executed"] is False, "the policy reference is not a harness target")
    for name, target in targets.items():
        golden = target.get("golden_cases", [])
        _require(not golden or not target["executed"], f"{name}: only a recorded-only target records goldens")
        if golden and suite_sizes is not None:
            sys.path.insert(0, str(ROOT / "harness"))
            from fixtures import cases
            _require(set(golden) <= {c["name"] for c in cases()}, f"{name}: golden_cases must be suite cases")
    masks = {k: v for k, v in data.get("masks", {}).items() if k != "about"}
    for mask, spec in masks.items():
        _require(re.fullmatch(r"[a-z0-9-]+", mask) and set(spec) <= {"pattern", "record", "headers", "json_type", "decode"},
                 f"mask {mask}: name and fields")
        _require(spec.get("json_type", "string") in goldens.JSON_TYPES and spec.get("decode", "compaction-capsule")
                 in goldens.DECODERS, f"mask {mask}: json_type {sorted(goldens.JSON_TYPES)}, decode {sorted(goldens.DECODERS)}")
        _require(re.compile(spec["pattern"]).groups == 0, f"mask {mask}: pattern has no capture groups")
        _require(not re.fullmatch(spec["pattern"], "") and ".*" not in spec["pattern"] and ".+" not in spec["pattern"],
                 f"mask {mask}: pattern must be narrow")
        re.compile(spec.get("record", spec["pattern"]))
    ci = _ci_rust_toolchain()
    _require(data["toolchain"]["rust"] == ci,
             f"toolchain.rust {data['toolchain']['rust']} differs from CI RUST_TOOLCHAIN {ci}")
    codex = data["contracts"]["codex"]
    _require(codex["commit"] == _codex_constant(), "codex commit differs from FIRST_PARTY_COMMIT")
    pinned = json.loads((ROOT / codex["manifest"]).read_text())["pin"]
    _require(pinned["commit"] == codex["commit"] and pinned["repository"] == codex["repository"],
             "codex pin differs from the Codex fixture manifest")
    for suite, size in (suite_sizes or {}).items():
        _require(data["suites"][suite]["count"] == size,
                 f"suite {suite}: manifest count {data['suites'][suite]['count']}, fixtures define {size}")
    seen = set()
    for group in data["groups"]:
        _require(group["tests"], f"group {group['id']} has no tests")
        for test in group["tests"]:
            key = (test["package"], test["target"], test["name"])
            _require(key not in seen, f"test listed twice: {key}")
            seen.add(key)
            _require(test["target"] == "lib" or test["target"].startswith("test:"),
                     f"{key}: target must be lib or test:<name>")
    providers = {group["provider"] for group in data["groups"]}
    _require({"claude", "codex", "xai", "meta", "removed"} <= providers,
             "groups must cover claude, codex, xai, meta and the removed-provider refusals")
    case_names = {}
    if suite_sizes is not None:
        sys.path.insert(0, str(ROOT / "harness"))
        from fixtures import cases, selective_cases
        case_names = {"historical": [c["name"] for c in cases()],
                      "selective": [c["name"] for c in cases() + selective_cases()]}
    ids = set()
    for divergence in data["divergences"]:
        name = divergence["id"]
        _require(name not in ids, f"divergence {name} listed twice")
        ids.add(name)
        _require(not set(divergence) & FORBIDDEN_FIELDS,
                 f"{name}: path or assertion exemptions are not allowed ({sorted(set(divergence) & FORBIDDEN_FIELDS)})")
        _require(set(divergence) <= DIVERGENCE_FIELDS, f"{name}: unknown fields {sorted(set(divergence) - DIVERGENCE_FIELDS)}")
        _require(divergence["kind"] in DIVERGENCE_KINDS, f"{name}: unknown kind")
        _require(set(divergence["targets"]) <= {"historical", "selective"} and divergence["targets"],
                 f"{name}: targets must name executed Go targets")
        _require(divergence["cases"], f"{name}: needs cases")
        _require(divergence.get("reason"), f"{name}: needs a reason")
        _require(divergence.get("expect"), f"{name}: needs at least one predicate")
        for index, predicate in enumerate(divergence["expect"]):
            _predicate(predicate, f"{name}[{index}]", divergence, data)
        kind = divergence["kind"]
        if kind in ("pending-feature", "deferred-feature", "unowned", "unattributed", "not-adopted", "missing-audit"):
            _require(divergence.get("issue"), f"{name}: needs the issue that tracks it")
        if kind == "missing-audit":
            _require(divergence["targets"] == ["selective"] and divergence.get("evidence")
                     and divergence.get("attribution") and divergence.get("ledger"),
                     f"{name}: a missing audit is selective-only and names its Go evidence, "
                     "frozen-reference attribution and unresolved audit ledger entry")
        if kind == "unowned":
            _require(divergence.get("evidence"), f"{name}: an unowned divergence names its Go commit")
        if kind == "unattributed":
            _require(divergence.get("attribution"), f"{name}: says what is and is not attributed")
        if kind == "not-adopted":
            _require(divergence["targets"] == ["selective"] and divergence.get("evidence") and divergence.get("attribution"),
                     f"{name}: a not-adopted divergence is selective-only, names its Go commit (evidence) and the "
                     "executed v8.0.35 comparison (attribution)")
        if kind == "documented-difference":
            _require(divergence.get("ledger"), f"{name}: needs its ledger entry")
        if kind == "nondeterministic":
            _require(divergence.get("evidence") or all(
                set(p) <= {"downstream_json", "credential_timestamps"} for p in divergence["expect"]),
                f"{name}: nondeterminism outside generated values needs evidence")
        if kind == "later-reference":
            _require(divergence["targets"] == ["historical"] and divergence.get("evidence"),
                     f"{name}: a later-reference divergence is historical-only and names its Go evidence")
        for target in divergence["targets"]:
            if case_names:
                for pattern in divergence["cases"]:
                    _require(any(fnmatch.fnmatchcase(c, pattern) for c in case_names[target]),
                             f"{name}: case pattern {pattern} matches no {target} case")
    generated = data.get("generated_headers", {})
    _require(set(generated) == {"upstream", "downstream"} and all(isinstance(v, dict) for v in generated.values()),
             "generated_headers maps upstream and downstream header names to patterns")
    for patterns in generated.values():
        for header, pattern in patterns.items():
            _require(header not in ("authorization", "content-type", "host"), f"{header} cannot be generated")
            re.compile(pattern)
    invariants = data.get("invariants", {})
    _require(set(invariants) >= {"native_passthrough", "upstream_goldens"}, "invariants need native rules")
    failures = invariants.get("upstream_write_failures")
    _require(isinstance(failures, list) and all(isinstance(c, str) for c in failures),
             "invariants.upstream_write_failures lists the cases whose upstream write may fail")
    if case_names:
        _require(set(failures) <= set(case_names["selective"]), "upstream_write_failures must be suite cases")
    for entry in invariants["upstream_goldens"]:
        _require(set(entry) == {"case", "file"}, f"{entry}: a native golden names a case and a file")
        _require("upstream" in _golden(entry["file"], "invariants", data)["covers"], f"{entry['file']}: covers upstream")
    for skip in data.get("skips", []):
        _require(skip.get("reason") and skip.get("issue"), f"skip {skip.get('case')}: needs reason and issue")
    return data


def load(suite_sizes=None):
    return validate(json.loads(PATH.read_text()), suite_sizes)


def suite_sizes():
    sys.path.insert(0, str(ROOT / "harness"))
    from fixtures import cases, selective_cases
    return {"claude-differential": len(cases()),
            "selective-differential": len(cases()) + len(selective_cases())}


def main(argv):
    if argv[:1] == ["check"]:
        load(suite_sizes())
        print("harness/manifest.json: ok")
        return 0
    if len(argv) in (2, 3) and argv[0] == "shell" and argv[2:] in ([], ["--golden"]):
        data = load()
        target = data["go"]["targets"].get(argv[1])
        golden = argv[2:] == ["--golden"]
        if target is None or (not target["executed"] and not (golden and target.get("golden_cases"))):
            print(f"not an executed Go target: {argv[1]}", file=sys.stderr)
            return 2
        print(f"go_repository={shlex.quote(data['go']['repository'])}")
        print(f"go_commit={shlex.quote(target['commit'])}")
        print(f"go_tag={shlex.quote(target['tag'])}")
        return 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
