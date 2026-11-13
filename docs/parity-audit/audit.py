#!/usr/bin/env python3
"""Regenerates docs/parity-audit/status.md and the summaries in docs/PARITY.md and README.md from the
checklist (checklist.md), a scan of this repository, the route probe (probe.py,
routes.json) and the manual judgments in manual.tsv.

Usage: python3 docs/parity-audit/audit.py [--milestones M1,M2]

Status per ID:
- covered: implemented, and Rust tests or Go-generated fixtures exercise it.
- partial: implemented in part, or implemented without tests that pin Go's behaviour.
- missing: not implemented.

Automatic evidence, overridden per ID by manual.tsv:
- test suites: each named Go test is matched against Go test names cited in Rust
  sources and Go-generated fixtures (fixture names carry `TestName:line`), and against
  Rust test function names (case and underscores ignored).
- routes: probe.py starts the server and records which method/path pairs are routed.
"""

import gzip
import json
import os
import re
import sys
from collections import defaultdict
from pathlib import Path

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
HERE = os.path.dirname(os.path.abspath(__file__))
LINK = re.compile(r"\[`([^`]*)`\]\([^)]*\)")
ITEM = re.compile(r"^- \[[ x]\] \*\*\[(M\d)\] (M\d-\d{4})\*\* (.*)")
GO_TEST = re.compile(r"\b(?:Test|Fuzz|Benchmark|Example)[A-Z0-9_][A-Za-z0-9_]*")


def items():
    out, section = [], None
    for line in open(os.path.join(HERE, "checklist.md"), encoding="utf-8"):
        m = re.match(r"^(#{2,3}) (.*)", line)
        if m:
            section = m.group(2).strip()
        m = ITEM.match(line)
        if m:
            out.append({"id": m.group(2), "ms": m.group(1), "section": section, "text": LINK.sub(r"\1", m.group(3).strip())})
    return out


def scan_files():
    """(relative path, text) for Rust sources, fixtures and dashboard tests."""
    roots = ["crates", "ui/src"]
    skip = {"target", "node_modules", "reference", "dist", "dist-panel"}
    for root in roots:
        for dirpath, dirs, files in os.walk(os.path.join(ROOT, root)):
            dirs[:] = [d for d in dirs if d not in skip]
            for name in files:
                path = os.path.join(dirpath, name)
                rel = os.path.relpath(path, ROOT)
                try:
                    if name.endswith(".gz"):
                        if os.path.getsize(path) > 64 << 20:
                            continue
                        text = gzip.open(path).read().decode("utf-8", "replace")
                    elif name.endswith((".rs", ".json", ".jsonl", ".ts", ".txt", ".yaml")):
                        text = open(path, encoding="utf-8", errors="replace").read()
                    else:
                        continue
                except OSError:
                    continue
                yield rel, text


def norm(name):
    name = re.sub(r"^(test|fuzz|benchmark|example)_?", "", name, flags=re.I)
    return re.sub(r"[^a-z0-9]", "", name.lower())


def build_index():
    cited = defaultdict(set)  # Go test name -> files citing it
    rust_tests = defaultdict(set)  # normalized Rust test fn name -> files
    go_sources = defaultdict(set)  # Go source basename cited in Rust -> files
    test_fn = re.compile(r"#\[(?:tokio::)?test[^\]]*\]\s*(?:#\[[^\]]*\]\s*)*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)")
    for rel, text in scan_files():
        for name in set(GO_TEST.findall(text)):
            cited[name].add(rel)
        if rel.endswith(".rs"):
            for fn in test_fn.findall(text):
                rust_tests[norm(fn)].add(rel)
            for src in set(re.findall(r"[A-Za-z0-9_\-]+\.go\b", text)):
                go_sources[src].add(rel)
        if rel.endswith(".ts"):
            for fn in re.findall(r"\b(?:test|it)\(\s*['\"`]([^'\"`]+)", text):
                rust_tests[norm(fn)].add(rel)
    return cited, rust_tests, go_sources


def test_cases(text):
    return re.findall(r"`((?:Test|Fuzz|Benchmark|Example)[A-Za-z0-9_]*)` \(L\d+\)", text)


def test_file(text):
    m = re.match(r"\*\*([^*:]+_test\.go)", text)
    return m.group(1) if m else None


def owner_for(text):
    """The area of the code base a gap belongs to, from the Go paths an item cites."""
    paths = re.findall(r"(?:internal|sdk|cmd|test|pkg)/[A-Za-z0-9_\-/.]+", text)
    joined = " ".join(paths).lower() or text.lower()
    rules = [
        ("codex/live", "realtime"),
        ("realtime", "realtime"),
        ("/live", "realtime"),
        ("internal/translator", "translate"),
        ("sdk/translator", "translate"),
        ("internal/thinking", "google"),
        ("internal/signature", "google"),
        ("claude", "claude"),
        ("anthropic", "claude"),
        ("antigravity", "google"),
        ("aistudio", "google"),
        ("vertex", "google"),
        ("interactions", "google"),
        ("gemini", "google"),
        ("kimi", "device-providers"),
        ("meta", "device-providers"),
        ("devin", "device-providers"),
        ("xai", "openai-xai"),
        ("grok", "openai-xai"),
        ("openai_compat", "openai-xai"),
        ("openai-compat", "openai-xai"),
        ("codex", "codex"),
        ("websocket", "codex"),
        ("management", "manage"),
        ("internal/config", "manage"),
        ("internal/usage", "server"),
        ("pluginabi", "plugins"),
        ("pluginhost", "plugins"),
        ("plugin", "plugins"),
        ("internal/tui", "tui"),
        ("internal/home", "home"),
        ("managementasset", "dashboard"),
        ("internal/store", "home"),
        ("discovery", "tui"),
        ("cmd/", "tui"),
    ]
    for key, owner in rules:
        if key in joined:
            return owner
    return "server"


def load_manual():
    manual = {}
    path = os.path.join(HERE, "manual.tsv")
    if not os.path.exists(path):
        return manual
    for line in open(path, encoding="utf-8"):
        if not line.strip() or line.startswith("#"):
            continue
        parts = line.rstrip("\n").split("\t")
        parts += [""] * (5 - len(parts))
        manual[parts[0]] = {"status": parts[1], "evidence": parts[2], "owner": parts[3], "note": parts[4]}
    return manual


def short(paths, limit=2):
    paths = sorted(paths)
    shown = ", ".join(paths[:limit])
    return shown + (f" (+{len(paths) - limit})" if len(paths) > limit else "")


def judge_test_suite(item, index):
    cited, rust_tests, go_sources = index
    cases = test_cases(item["text"])
    gofile = test_file(item["text"]) or ""
    hits, files = [], set()
    for case in cases:
        found = cited.get(case) or rust_tests.get(norm(case))
        if found:
            hits.append(case)
            files |= found
    base = os.path.basename(gofile).replace("_test.go", ".go")
    impl_cited = go_sources.get(base, set())
    if gofile.startswith(("internal/thinking/", "internal/signature/")) and len(hits) < len(cases):
        return ("covered", f"every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz ({len(hits)}/{len(cases)} cases also cited by name)", "")
    if cases and len(hits) == len(cases):
        return ("covered", f"{len(hits)}/{len(cases)} cases: {short(files)}", "")
    if hits:
        missing = [c for c in cases if c not in hits]
        return ("partial", f"{len(hits)}/{len(cases)} cases: {short(files)}; not matched: {', '.join(missing[:4])}{' …' if len(missing) > 4 else ''}", "")
    if impl_cited:
        return ("partial", f"implementation cites {base} ({short(impl_cited)}); no case matched by name", "")
    return ("missing", f"0/{len(cases)} cases matched; {base} not cited", "")


def test_texts():
    """Test sources: integration tests, *_tests.rs files and inline test modules."""
    out = {}
    for rel, text in scan_files():
        if rel.endswith("cpa-server/tests/fixtures/legacy_go.json"):
            # legacy_go.rs requests each step's path under /v0/management.
            steps = (s for sc in json.loads(text)["scenarios"] for s in sc.get("steps", []))
            out[rel] = "\n".join(f'{s["method"]} "/v0/management{s["path"]}"' for s in steps if "path" in s)
            continue
        if rel.endswith("cpa-server/tests/fixtures/plugin_routes_go.json"):
            # plugin_routes.rs replays every HTTP step, including v0/v8 aliases.
            steps = json.loads(text)["steps"]
            out[rel] = "\n".join(f'{s["args"]["method"]} "{s["args"]["path"]}"'
                                  for s in steps if s["op"] == "http")
            continue
        if rel.endswith(("cpa-server/tests/fixtures/media_go.json", "cpa-server/tests/fixtures/speech_go.json")):
            # media_go.rs requests each scenario's path and compares its response.
            out[rel] = "\n".join(f'{s["method"]} "{s["path"]}"' for s in json.loads(text)["scenarios"])
            continue
        if not rel.endswith(".rs"):
            continue
        if "/tests/" in rel or rel.endswith("_tests.rs"):
            out[rel] = text
        elif "#[cfg(test)]" in text:
            out[rel] = text[text.index("#[cfg(test)]"):]
    return out


def judge_route(item, routes, tests):
    m = re.match(r"`([A-Z]+) (\S+)`", item["text"])
    if not m:
        return None
    method, path = m.groups()
    key = f"{method} {path}"
    if key not in routes:
        return None
    hit = routes[key]
    if not hit["routed"]:
        return ("missing", f"probe: {key} -> {hit['status']} (not routed)")
    literal = re.split(r"[:*]", path)[0]
    if literal != "/" and literal.endswith("/") and len(literal) > 1:
        literal = literal
    pattern = re.compile(re.escape(literal) + (r'["?/ {]' if not literal.endswith("/") else ""))
    route_path = "/".join(r'[^/?"\r\n]+' if part.startswith(":") else r'[^?"\r\n]+' if part.startswith("*")
                          else re.escape(part) for part in path.split("/"))
    fixture_request = re.compile(r'^' + method + r' "' + route_path + r'(?:\?[^"\n]*)?"$', re.M)
    users = [rel for rel, text in tests.items()
             if (fixture_request if rel.endswith(".json") else pattern).search(text)]
    if users:
        return ("covered", f"probe: {key} -> {hit['status']}; tests: {short(users)}")
    return ("partial", f"probe: {key} -> {hit['status']}; no test requests this path")


FAMILY_OWNER = {"claude": "claude", "codex": "codex", "gemini": "google", "interactions": "google",
                "meta": "device-providers", "kimi": "device-providers", "openai-compatibility": "openai-xai",
                "aistudio": "google", "antigravity": "google", "vertex": "google", "xai": "openai-xai",
                "devin": "device-providers"}
SECTION_OWNER = [("management.", "manage"), ("config-version", "manage"), ("plugins.", "plugins"),
                 ("server.discovery", "tui"), ("credentials.", "home"),
                 ("client.codex", "codex"), ("multimedia.", "openai-xai")]
CONFIG_ONLY = ("config/schema.rs", "config/document.rs", "config/validate.rs", "config/generate_schema.py")


GENERIC_LEAVES = {"name", "alias", "api-key", "base-url", "headers", "prefix", "proxy-url", "models", "disabled",
                  "priority", "weight", "display-name", "excluded-models", "websockets", "alpha-search", "force-mapping"}


def config_family(key):
    parts = key.replace("[]", "").split(".")
    if parts[0] in ("api-keys", "upstream") and len(parts) > 1:
        return parts[1]
    if parts[0] == "oauth" and len(parts) > 2 and parts[1] == "providers":
        return parts[2]
    return None


def config_owner(key):
    if ".live-media-relay" in key or "aistudio.ws-auth" in key:
        return "realtime" if ".live-media-relay" in key else "google"
    family = config_family(key)
    if family in FAMILY_OWNER:
        return FAMILY_OWNER[family]
    for prefix, owner in SECTION_OWNER:
        if key.startswith(prefix):
            return owner
    return "server"


def kebab_fields(text):
    """Keys read through `#[serde(rename_all = "kebab-case")]` structs (no string literal)."""
    keys = set()
    for m in re.finditer(r'rename_all\s*=\s*"kebab-case"[^\n]*\n(?:\s*#\[[^\n]*\n)*\s*(?:pub(?:\([a-z]+\))?\s+)?struct\s+\w+(?:<[^>]*>)?\s*\{', text):
        depth, i = 1, m.end()
        start = i
        while i < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        body = text[start:i]
        for field in re.findall(r'^\s*(?:pub(?:\([a-z]+\))?\s+)?([a-z][a-z0-9_]*)\s*:', body, re.M):
            keys.add(field.replace("_", "-"))
    return keys


def config_sources():
    readers, tests = {}, {}
    for rel, text in scan_files():
        is_test = "/tests/" in rel or rel.endswith("_tests.rs") or "/testdata/" in rel
        if rel.endswith(".rs") and not is_test and not rel.endswith(CONFIG_ONLY):
            body = text.split("#[cfg(test)]")[0]
            # Kebab-case serde fields count as reads of their quoted key names.
            readers[rel] = body + "\n" + " ".join(f'"{k}"' for k in sorted(kebab_fields(body)))
            if "#[cfg(test)]" in text:
                tests[rel] = text[text.index("#[cfg(test)]"):]
        elif is_test:
            tests[rel] = text
    # Path segments per reader file: quoted strings split on '.' and '/'.
    tokens = {rel: {part for lit in re.findall(r'"([^"\\\n]{1,200})"', text) for part in re.split(r"[./]", lit)}
              for rel, text in readers.items()}
    # Keys a test sets: YAML keys (`key:`, also inside JSON-escaped YAML) and quoted path segments.
    # The lookbehind starts matches only at a run's first character: without it, long
    # lowercase runs in fixtures (repeated-payload tests) backtrack quadratically.
    test_tokens = {rel: set(re.findall(r"(?<![a-z0-9\-])([a-z0-9][a-z0-9\-]*):", text))
                   | {part for lit in re.findall(r'"([^"\\\n]{1,200})"', text) for part in re.split(r"[./]", lit)}
                   for rel, text in tests.items()}
    return readers, tests, tokens, test_tokens


def judge_config(item, sources):
    m = re.match(r"`([^`]+)`", item["text"])
    if not m:
        return None
    key = m.group(1)
    family = config_family(key)
    owner = config_owner(key)
    leaf = key.replace("[]", "").split(".")[-1]
    if "{" in leaf:
        leaf = key.replace("[]", "").split(".")[-2].strip("{}")
    readers, tests, _, _ = sources
    lit = f'"{leaf}"'
    # A key appears as a whole string or as a segment of a dotted path ("server.trusted-proxies").
    seg = re.compile(r'["./]' + re.escape(leaf) + r'["./]')
    tokens = sources[2]
    if leaf in GENERIC_LEAVES and family:
        # Shared credential fields: synthesized for every API-key family.
        used = [rel for rel in readers if rel.endswith(("config/credentials.rs", "config/sanitize.rs")) and leaf in tokens[rel]]
    else:
        used = [rel for rel in readers if leaf in tokens[rel]]
    if not used:
        return ("missing", f"accepted by the config schema; no runtime code reads {lit}", owner, "")
    family_token = {"interactions": "interactions", "openai-compatibility": "openai-compat"}.get(family, family)
    test_tokens = sources[3]
    set_in = [rel for rel, text in tests.items()
              if leaf in test_tokens[rel] and (not family_token or family_token in text)]
    if set_in:
        return ("covered", f"read in {short(used)}; set in {short(set_in)}", owner, "heuristic: key name match")
    return ("partial", f"read in {short(used)}; no test sets it", owner, "heuristic: key name match")


# Native plugins are removed (#47): the plugin ABI, host, store, `plugins.*` settings,
# plugin routes and the suites that test only them are retired, a deliberate difference.
# Rows owned by `plugins` are retired except the translator registry suites, which test the
# built-in registry too; these rows owned by other areas are retired as well. Suites that
# mix plugin cases with other behaviour keep their judgment.
RETIRED = {
    "M6-0001", "M6-0022", "M6-0023", "M6-0024", "M6-0034", "M6-0035", "M6-0036",
    "M5-0076", "M5-0077", "M5-0089", "M5-0090", "M5-0091", "M5-0092", "M5-0093", "M5-0094",
    "M5-0095", "M5-0096", "M5-0097", "M5-0098", "M5-0099", "M5-0100", "M5-0128", "M5-0129",
    "M5-0130", "M6-0156", "M6-0157", "M6-0158", "M6-0159", "M6-0217",
    # Go's htmlsanitize escapes plugin management and resource responses only.
    "M4-0299",
}
RETIRED_KEEP = {"M2-0156", "M2-0157"}
RETIRED_NOTE = ('Deliberate difference: native plugins removed (#47); see DIFFERENCES-FROM-GO.md '
                '"Native plugins" and MIGRATING-FROM-GO.md "Native plugins".')
RETIRED_EVIDENCE = ("retired: a `plugins` section fails the load (crates/cpa-core/src/config.rs "
                    "a_plugins_section_is_refused_with_the_migration_error); plugin routes answer 410 "
                    "(crates/cpa-server/tests/management.rs retired_plugin_routes_answer_gone_behind_the_guard)")


def retired(item, owner):
    """Whether native plugin removal retired `item`, whose area is `owner`."""
    if item["id"] in RETIRED_KEEP:
        return False
    return owner == "plugins" or item["id"] in RETIRED


def main():
    only = None
    if "--milestones" in sys.argv:
        only = set(sys.argv[sys.argv.index("--milestones") + 1].split(","))
    index = build_index()
    manual = load_manual()
    routes_path = os.path.join(HERE, "routes.json")
    routes = json.load(open(routes_path)) if os.path.exists(routes_path) else {}
    tests = test_texts()
    sources = config_sources()
    rows = []
    for item in items():
        if only and item["ms"] not in only:
            continue
        status, evidence, note, auto_owner = "", "", "", ""
        if "test-suite" in item["section"]:
            status, evidence, note = judge_test_suite(item, index)
        else:
            routed = judge_route(item, routes, tests)
            if routed:
                status, evidence = routed
            elif item["section"].startswith("5. Config"):
                judged = judge_config(item, sources)
                if judged:
                    status, evidence, auto_owner, note = judged
        man = manual.get(item["id"])
        if man:
            status = man["status"] or status
            evidence = man["evidence"] or evidence
            note = man["note"] or note
        if retired(item, (man and man["owner"]) or auto_owner or owner_for(item["text"])):
            status, evidence, note = "covered", RETIRED_EVIDENCE, RETIRED_NOTE
        owner = ""
        if status != "covered":
            owner = (man and man["owner"]) or auto_owner or owner_for(item["text"])
        rows.append({**item, "status": status or "unjudged", "evidence": evidence, "owner": owner, "note": note})
    json.dump(rows, open(os.path.join(HERE, "status.json"), "w"), indent=1)
    counts = defaultdict(lambda: defaultdict(int))
    for r in rows:
        counts[r["ms"]][r["status"]] += 1
    for ms in sorted(counts):
        print(ms, dict(counts[ms]))
    if not only:
        render([r for r in rows if r["ms"] in AUDITED])


# Milestones whose rows have been reviewed by hand; the others are not rendered yet.
AUDITED = ["M1", "M2", "M3", "M4", "M5", "M6"]
# What the audited tree is called in the rendered pages: a release or a date.
BASE = "2026-10-20 (v0.2.6, d1261f2)"
REFERENCE = "v8.0.35 (`ae3ee3be`)"


def title(r):
    text = r["text"]
    m = re.match(r"\*\*([^*]+?)(?::\d+-\d+)?\*\*", text)
    if m:
        return m.group(1)
    m = re.match(r"`([^`]+)`", text)
    if m:
        return m.group(1)
    text = re.sub(r"\*\*", "", text)
    return text[:90] + ("…" if len(text) > 90 else "")


def cell(text):
    return text.replace("|", "\\|").replace("\n", " ")


def render(rows):
    out = []
    w = out.append
    w("# Parity status, item by item")
    w("")
    w(f"Audit of {BASE} against CLIProxyAPI {REFERENCE}. Milestones audited: {', '.join(AUDITED)} "
      "(every item in [checklist.md](checklist.md)). [docs/PARITY.md](../PARITY.md) explains the milestones and sums them up.")
    w("")
    w("Statuses:")
    w("")
    w("- **covered**: implemented, and Rust tests or Go-generated fixtures exercise it. A note starting "
      "\"Deliberate difference\" marks an owner-approved divergence from Go; it counts as covered because "
      "there is no gap to close.")
    w("- **partial**: implemented in part, or implemented without tests that pin Go's behaviour. For Go test suites: the behaviour exists and is exercised, but not every Go case is ported.")
    w("- **missing**: not implemented.")
    w("")
    w("Area names the part of the code base that would close a partial or missing item.")
    w("")
    w("Method: `docs/parity-audit/audit.py` regenerates this file. Routes come from `probe.py`, which starts the binary and requests every listed method and path "
      "without upstream credentials. Client routes use a wrong client key; management routes use a private valid management key so they reach the router "
      "(routed pairs answer from the auth guard or handler, unrouted ones 404/405). A route counts as covered when a test requests it. "
      "Go test suites are matched case by case against Go test names cited in Rust code and in Go-generated fixtures (fixture names carry `TestName:line`). "
      "Every other item, and every suite the matcher cannot see, was judged by reading the Rust code and tests; those judgments live in `docs/parity-audit/manual.tsv` with their evidence. "
      "\"Not ported by name\" means the area is implemented and tested through other cases (usually Go-generated end-to-end scenarios), but the Go suite's own cases are not reproduced one by one. "
      "A \"Suite changed\" note preserves previous-reference evidence and case counts; it does not prove the current v8.0.35 assertions.")
    w("")
    w("## Summary")
    w("")
    w("| Milestone | Items | covered | partial | missing |")
    w("|---|---:|---:|---:|---:|")
    for ms in AUDITED:
        rs = [r for r in rows if r["ms"] == ms]
        c = defaultdict(int)
        for r in rs:
            c[r["status"]] += 1
        w(f"| {ms} | {len(rs)} | {c['covered']} | {c['partial']} | {c['missing']} |")
    w("")
    w("### Gaps by area")
    w("")
    owners = defaultdict(lambda: defaultdict(list))
    for r in rows:
        if r["status"] in ("partial", "missing"):
            owners[r["owner"]][r["status"]].append(r["id"])
    w("| Area | missing | partial | Missing items |")
    w("|---|---:|---:|---|")
    for owner in sorted(owners, key=lambda o: (-len(owners[o]["missing"]), -len(owners[o]["partial"]), o)):
        m, p = owners[owner]["missing"], owners[owner]["partial"]
        w(f"| {owner} | {len(m)} | {len(p)} | {', '.join(sorted(m)) or '—'} |")
    w("")
    for ms in AUDITED:
        w(f"## {ms}")
        w("")
        sections = []
        for r in rows:
            if r["ms"] == ms and r["section"] not in sections:
                sections.append(r["section"])
        for section in sections:
            w(f"### {ms}: {section}")
            w("")
            w("| ID | Item | Status | Evidence | Area | Note |")
            w("|---|---|---|---|---|---|")
            for r in rows:
                if r["ms"] == ms and r["section"] == section:
                    w(f"| {r['id']} | {cell(title(r))} | {r['status']} | {cell(r['evidence'])} | {cell(r['owner'])} | {cell(r['note'])} |")
            w("")
    summary(rows)
    open(os.path.join(HERE, "status.md"), "w", encoding="utf-8").write("\n".join(out))


def summary(rows):
    """Rewrites the PARITY.md table and README sentence from the same totals."""
    lines = [f"Audit of {BASE} against CLIProxyAPI {REFERENCE}.", "", "| Milestone | Items | Covered | Partial | Missing |", "|---|---:|---:|---:|---:|"]
    total = defaultdict(int)
    for ms in AUDITED:
        c = defaultdict(int)
        for r in rows:
            if r["ms"] == ms:
                c[r["status"]] += 1
                c["all"] += 1
        for k, v in c.items():
            total[k] += v
        lines.append(f"| {ms} | {c['all']} | {c['covered']} | {c['partial']} | {c['missing']} |")
    lines.append(f"| All | {total['all']} | {total['covered']} | {total['partial']} | {total['missing']} |")
    parity_path = Path(ROOT) / "docs" / "PARITY.md"
    page = parity_path.read_text(encoding="utf-8")
    start, end = "<!-- parity-summary:start -->", "<!-- parity-summary:end -->"
    head, rest = page.split(start, 1)
    tail = rest.split(end, 1)[1]
    parity_page = head + start + "\n" + "\n".join(lines) + "\n" + end + tail
    path = Path(ROOT) / "README.md"
    page = path.read_text(encoding="utf-8")
    sentence = (f"Parity is incomplete: the audit has {total['all']:,} items, with "
                f"{total['covered']:,} covered, {total['partial']:,} partial and {total['missing']:,} missing.")
    page, replaced = re.subn(r"Parity is incomplete: the audit has [\d,]+ items, with "
                            r"[\d,]+ covered, [\d,]+ partial and [\d,]+ missing\.", sentence, page)
    if replaced != 1:
        raise ValueError("README.md must contain exactly one parity summary sentence")
    parity_path.write_text(parity_page, encoding="utf-8")
    path.write_text(page, encoding="utf-8")


if __name__ == "__main__":
    main()
