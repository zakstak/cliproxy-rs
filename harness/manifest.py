"""The harness revision manifest (harness/manifest.json): loading and validation.

Every Go, first-party and toolchain pin the harness uses comes from the manifest. The
runner, the collector and the acceptance command read pins only through `load()`,
which refuses a manifest whose pins disagree with the sources they claim to mirror.

    python3 harness/manifest.py check          # validate, exit 1 on a problem
    python3 harness/manifest.py shell TARGET   # shell assignments for harness/run
"""
import json
from pathlib import Path
import re
import shlex
import sys

ROOT = Path(__file__).resolve().parent.parent
PATH = ROOT / "harness" / "manifest.json"
SHA = re.compile(r"^[0-9a-f]{40}$")
ROLES = {"historical-recording", "frozen-default-reference", "selective-comparison"}
DIVERGENCE_KINDS = {
    # A v8.0.67 behaviour an issue in the selective batch will adopt; not yet in Rust.
    "pending-feature",
    # Rust deliberately differs, recorded in docs/DIFFERENCES-FROM-GO.md or a contract.
    "documented-difference",
    # Generated IDs, timing, framing: the collector sees them, meaning is unaffected.
    "nondeterministic",
    # On the historical target: Rust follows a later Go reference (v8.0.35 policy or a
    # selective adoption) than the v8.0.10 recording. Names the Go evidence.
    "later-reference",
    # The harness itself makes the sides differ (for example each proxy's upstream
    # address label). Named so the cause is visible, never a provider behaviour.
    "harness-boundary",
    # A v8.0.67 behaviour outside the approved core batch (deferred or follow-up); names
    # the issue that would adopt it.
    "deferred-feature",
    # A difference the harness has not yet classified as either; it stays visible in
    # every receipt and names the follow-up that resolves it.
    "unclassified",
}


# Semantic assertions harness/acceptance.py applies; a divergence may excuse these.
ASSERTIONS = {"status", "content-type", "upstream-traffic", "request-lines", "upstream-headers",
              "upstream-body-bytes", "upstream-body-meaning", "downstream-headers", "downstream-body-bytes",
              "downstream-meaning"}
# credential-isolation is deliberately absent: no divergence can excuse a leak.
DIVERGENCE_FIELDS = {"id", "kind", "targets", "cases", "except_cases", "paths", "assertions", "header_names",
                     "upstream_header_names", "upstream_body_patterns", "reason", "issue", "ledger", "evidence",
                     "may_be_absent"}


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
    ids = set()
    for divergence in data["divergences"]:
        _require(divergence["id"] not in ids, f"divergence {divergence['id']} listed twice")
        ids.add(divergence["id"])
        _require(divergence["kind"] in DIVERGENCE_KINDS, f"{divergence['id']}: unknown kind")
        _require(set(divergence["targets"]) <= {"historical", "selective"} and divergence["targets"],
                 f"{divergence['id']}: targets must name executed Go targets")
        _require(divergence["cases"], f"{divergence['id']}: needs cases")
        for pattern in divergence.get("upstream_body_patterns", []):
            re.compile(pattern)
        _require(any(divergence.get(k) for k in ("paths", "assertions", "header_names", "upstream_header_names",
                                                 "upstream_body_patterns")),
                 f"{divergence['id']}: needs raw paths, header names or assertions to cover")
        for pattern in divergence.get("paths", []):
            re.compile(pattern)
        _require(divergence.get("reason"), f"{divergence['id']}: needs a reason")
        if divergence["kind"] in ("pending-feature", "deferred-feature", "unclassified"):
            _require(divergence.get("issue"), f"{divergence['id']}: needs the issue that resolves it")
        if divergence["kind"] == "documented-difference":
            _require(divergence.get("ledger"), f"{divergence['id']}: needs its ledger entry")
        if divergence["kind"] == "later-reference":
            _require(divergence["targets"] == ["historical"] and divergence.get("evidence"),
                     f"{divergence['id']}: a later-reference divergence is historical-only and names its Go evidence")
        unknown = set(divergence.get("assertions", [])) - ASSERTIONS
        _require(not unknown, f"{divergence['id']}: unknown assertions {sorted(unknown)}")
        _require(set(divergence) <= DIVERGENCE_FIELDS, f"{divergence['id']}: unknown fields "
                 f"{sorted(set(divergence) - DIVERGENCE_FIELDS)}")
    generated = data.get("generated_headers", {})
    _require(set(generated) == {"upstream", "downstream"}, "generated_headers needs upstream and downstream")
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
    if len(argv) == 2 and argv[0] == "shell":
        data = load()
        target = data["go"]["targets"].get(argv[1])
        if target is None or not target["executed"]:
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
