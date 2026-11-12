"""Judging one Go/Rust differential case (issue #67, H1).

A case passes when three things hold:

1. Per-side invariants hold. They are unconditional: no divergence can relax them.
   They cover credential ownership and isolation, consistency of the raw capture with
   its parsed fields, the formats of generated values, Content-Length against the body,
   and the native-byte goldens and pass-through cases the manifest lists.
2. Every compared dimension (status, upstream requests, upstream headers and bodies,
   downstream headers and bodies, TLS, persisted credential) is equal between Go and
   Rust. The exception is a named divergence predicate that states the exact delta
   each side must show. A predicate whose delta is missing or different fails.
3. Every raw collector difference falls in a dimension whose check passed. A
   difference in any other place (an unknown path, a TLS change, an extra request, a
   read error) fails.

There are no path or assertion exemptions. A divergence describes values, and
everything it does not describe is compared byte for byte.
"""
import copy
import datetime
import fnmatch
import json
from pathlib import Path
import re

from fixtures import CREDENTIAL, PROVIDER_KEYS, TOKEN_REPLY

CLIENT_KEYS = ("fixture-client-key", "wrong-fixture-key")
CLAUDE_TOKENS = (CREDENTIAL["access_token"], json.loads(TOKEN_REPLY)["access_token"])
CLAUDE_SECRETS = (CREDENTIAL["access_token"], CREDENTIAL["refresh_token"],
                  *(json.loads(TOKEN_REPLY)[k] for k in ("access_token", "refresh_token")))
UPSTREAM_SECRETS = (*CLAUDE_SECRETS, *PROVIDER_KEYS.values())
# Cases whose client is refused or whose credential is unusable: no provider traffic.
NO_TRAFFIC = {"auth-missing", "auth-wrong", "auth-query-first", "models-openai", "models-anthropic",
              "disabled-credential"}
DIMENSIONS = ("status", "upstream-requests", "upstream-headers", "upstream-body", "downstream-headers",
              "downstream-body", "tls", "credential")


def _bytes(value):
    return bytes.fromhex(value) if value else b""


def _match(spec, value):
    """A value spec: None means absent; {"equals": s} or {"regex": r} a present value."""
    if spec is None:
        return value is None
    if value is None:
        return False
    if "equals" in spec:
        return value == spec["equals"]
    return re.fullmatch(spec["regex"], value) is not None


def _applies(divergence, target, case_name):
    def matches(patterns):
        return any(fnmatch.fnmatchcase(case_name, p) for p in patterns)
    return (target in divergence["targets"] and matches(divergence["cases"])
            and not matches(divergence.get("except_cases", [])))


def is_inference(request, case):
    prefix = "POST /v1/responses " if case.get("provider") else "POST /v1/messages"
    return request["request_line"].startswith(prefix)


# --- consistency of the capture itself -------------------------------------------------

def _parse_head(raw):
    head, separator, tail = raw.partition(b"\r\n\r\n")
    if not separator:
        return None, [], tail
    lines = head.split(b"\r\n")
    fields = []
    for line in lines[1:]:
        name, _, value = line.partition(b":")
        fields.append([name.decode("latin1"), value.decode("latin1").strip()])
    return lines[0].decode("latin1"), fields, tail


def _dechunk(data):
    """Body bytes of a chunked tail, and whether its terminating chunk was seen."""
    out, pos = bytearray(), 0
    while True:
        end = data.find(b"\r\n", pos)
        if end < 0:
            return bytes(out), False
        size = int(data[pos:end].split(b";")[0] or b"0", 16)
        if size == 0:
            return bytes(out), True
        chunk = data[end + 2:end + 2 + size]
        out += chunk
        if len(chunk) < size:
            return bytes(out), False
        pos = end + 2 + size + 2


def _field(headers, name):
    values = [v for k, v in headers if k.lower() == name]
    return values[0] if len(values) == 1 else (None if not values else values)


def consistency(observation):
    """Raw bytes must agree with the parsed fields both sides are compared on."""
    problems = []
    for i, request in enumerate(observation["upstream"]["http"]):
        line, fields, rest = _parse_head(_bytes(request["raw_head_hex"]))
        if line != request["request_line"] or fields != request["headers"] or rest:
            problems.append(f"upstream request {i}: raw head disagrees with its parsed line or headers")
        body = _bytes(request["body_hex"])
        if request.get("body_text") is not None and request["body_text"] != body.decode("utf-8", errors="replace"):
            problems.append(f"upstream request {i}: body_text disagrees with body_hex")
        length = _field(request["headers"], "content-length")
        if length is not None and length != str(len(body)):
            problems.append(f"upstream request {i}: Content-Length {length} for a {len(body)}-byte body")
    for i, reply in enumerate(observation["downstream"]):
        if "raw_response_hex" not in reply or "status" not in reply:
            continue
        line, fields, tail = _parse_head(_bytes(reply["raw_response_hex"]))
        if line is None or not line.startswith(f"HTTP/1.1 {reply['status']} ") or fields != reply["headers"]:
            problems.append(f"downstream reply {i}: raw head disagrees with its status or headers")
            continue
        body = _bytes(reply.get("body_hex"))
        partial = bool(reply.get("read_error") or reply.get("client_closed_after_event"))
        if str(_field(fields, "transfer-encoding") or "").lower() == "chunked":
            decoded, complete = _dechunk(tail)
            if partial:
                if not decoded.startswith(body):
                    problems.append(f"downstream reply {i}: body is not what the chunked wire carried")
            elif decoded != body or not complete:
                problems.append(f"downstream reply {i}: chunked wire does not decode to the body")
        elif tail != body:
            problems.append(f"downstream reply {i}: wire body differs from the body")
        length = _field(fields, "content-length")
        if length is not None and tail and length != str(len(tail)):
            problems.append(f"downstream reply {i}: Content-Length {length} for a {len(tail)}-byte body")
    return problems


# --- unconditional invariants ----------------------------------------------------------

def ownership(case, observation):
    """Each upstream request carries only the credential its provider owns, no client
    key reaches an upstream, and no upstream secret reaches the client."""
    problems = []
    provider = case.get("provider")
    foreign = [k for p, k in PROVIDER_KEYS.items() if p != provider]
    if provider:
        foreign += list(CLAUDE_SECRETS)
    for i, request in enumerate(observation["upstream"]["http"]):
        raw = (_bytes(request["raw_head_hex"]) + _bytes(request["body_hex"])).decode("utf-8", errors="replace")
        problems += [f"upstream request {i}: client key {k[:9]}... forwarded" for k in CLIENT_KEYS if k in raw]
        problems += [f"upstream request {i}: foreign credential {k[:10]}... sent" for k in foreign if k in raw]
        path = request["request_line"].split()[1].split("?")[0]
        authorization = [v for k, v in request["headers"] if k.lower() == "authorization"]
        api_keys = [v for k, v in request["headers"] if k.lower() in ("x-api-key", "api-key")]
        if api_keys:
            problems.append(f"upstream request {i}: unexpected API-key header")
        if provider:
            expected = [f"Bearer {PROVIDER_KEYS[provider]}"]
            if authorization != expected:
                problems.append(f"upstream request {i}: Authorization is not the {provider} credential")
        elif path in ("/v1/messages", "/v1/messages/count_tokens", "/api/oauth/profile",
                      "/api/oauth/claude_cli/roles"):
            if len(authorization) != 1 or authorization[0] not in [f"Bearer {t}" for t in CLAUDE_TOKENS]:
                problems.append(f"upstream request {i}: Authorization is not the Claude credential")
        elif path == "/v1/oauth/token":
            if authorization:
                problems.append(f"upstream request {i}: token refresh carries an Authorization header")
    for i, reply in enumerate(observation["downstream"]):
        raw = _bytes(reply.get("raw_response_hex")).decode("utf-8", errors="replace")
        problems += [f"downstream reply {i}: upstream secret {s[:12]}... reached the client"
                     for s in UPSTREAM_SECRETS if s in raw]
    return problems


def generated_formats(observation, generated):
    """Values minted per request must at least have their documented shape."""
    problems = []
    for i, request in enumerate(observation["upstream"]["http"]):
        for name, pattern in generated["upstream"].items():
            for value in [v for k, v in request["headers"] if k.lower() == name]:
                if not re.fullmatch(pattern, value):
                    problems.append(f"upstream request {i}: {name} {value!r} is not {pattern}")
    for i, reply in enumerate(observation["downstream"]):
        for name, pattern in generated["downstream"].items():
            for value in [v for k, v in reply.get("headers", []) if k.lower() == name]:
                if not re.fullmatch(pattern, value):
                    problems.append(f"downstream reply {i}: {name} {value!r} is not {pattern}")
    return problems


def native_bytes(case, observation, invariants, root):
    """Rust-side native requests: byte-exact pass-through, or the recorded golden."""
    problems = []
    inference = [r for r in observation["upstream"]["http"] if is_inference(r, case)]
    if case["name"] in invariants.get("native_passthrough", []):
        sent = [_bytes(r["request_body_hex"]) for r in observation["downstream"]]
        if [_bytes(r["body_hex"]) for r in inference] != sent:
            problems.append("native pass-through: the upstream body is not the client's bytes")
    for golden in invariants.get("upstream_goldens", []):
        if golden["case"] != case["name"]:
            continue
        expected = json.loads((root / golden["file"]).read_text())
        masked = set(golden.get("masked_headers", []))
        got = [{"request_line": r["request_line"],
                "headers": [[k, "<generated>" if k.lower() in masked else v] for k, v in r["headers"]],
                "body_hex": r["body_hex"]} for r in inference]
        if got != expected["requests"]:
            problems.append(f"native golden {golden['file']}: upstream request differs from the recorded bytes")
    return problems


def invariants(case, observation, side, pins, root):
    problems = consistency(observation) + ownership(case, observation)
    problems += generated_formats(observation, pins["generated_headers"])
    inference = [r for r in observation["upstream"]["http"] if is_inference(r, case)]
    if case["name"] in NO_TRAFFIC and inference:
        problems.append("a refused client or unusable credential reached the provider")
    if side == "rust":
        problems += native_bytes(case, observation, pins["invariants"], root)
    return problems


# --- predicates --------------------------------------------------------------------

class Predicates:
    """The named deltas that apply to one case, grouped by dimension."""

    def __init__(self, divergences, target, case_name):
        self.items = []
        for divergence in divergences:
            if _applies(divergence, target, case_name):
                for index, predicate in enumerate(divergence["expect"]):
                    self.items.append((divergence, index, predicate))
        self.hits = set()
        self.failures = []

    def of(self, key):
        return [(d, i, p[key]) for d, i, p in self.items if key in p]

    def hit(self, divergence):
        self.hits.add(divergence["id"])

    def fail(self, divergence, index, message):
        self.failures.append(f"{divergence['id']}[{index}]: {message}")


def _edit(data, pattern, count):
    """(edited bytes, matches) with every match of `pattern` removed."""
    if pattern is None:
        return data, 0
    return re.subn(pattern.encode(), b"", data)


def _apply_edits(predicates, key, pairs):
    """Remove each body edit's matches from both sides. Counts must be exact."""
    out = []
    for go_body, rust_body in pairs:
        for divergence, index, edit in predicates.of(key):
            go_body, go_n = _edit(go_body, edit["go"], edit["count"])
            rust_body, rust_n = _edit(rust_body, edit["rust"], edit["count"])
            if edit["count"] == "same":
                # Both sides carry the same number of their own spelling (0 included).
                if go_n != rust_n:
                    predicates.fail(divergence, index, f"{key}: Go matched {go_n}, Rust matched {rust_n}")
            else:
                want_go = edit["count"] if edit["go"] is not None else 0
                want_rust = edit["count"] if edit["rust"] is not None else 0
                if (go_n, rust_n) != (want_go, want_rust):
                    predicates.fail(divergence, index,
                                    f"{key}: Go matched {go_n}, Rust {rust_n}; expected {want_go} and {want_rust}")
            if go_n or rust_n:
                predicates.hit(divergence)
        out.append((go_body, rust_body))
    return out


def _apply_timestamps(predicates, pairs):
    """Wall-clock seconds minted per response: both sides carry the same number, all
    within the stated skew of each other; then they are zeroed on both sides."""
    out = []
    for go_body, rust_body in pairs:
        for divergence, index, spec in predicates.of("downstream_body_timestamps"):
            pattern = re.compile(spec["pattern"].encode())
            go_values = [int(m.group(1)) for m in pattern.finditer(go_body)]
            rust_values = [int(m.group(1)) for m in pattern.finditer(rust_body)]
            values = go_values + rust_values
            if not go_values or len(go_values) != len(rust_values) \
                    or max(values) - min(values) > spec["max_skew_seconds"]:
                predicates.fail(divergence, index, f"timestamps Go {sorted(set(go_values))} Rust "
                                                   f"{sorted(set(rust_values))} (skew limit {spec['max_skew_seconds']}s)")
                continue
            if go_values != rust_values:
                predicates.hit(divergence)

            def zero(match):
                return match.group(0)[:match.start(1) - match.start(0)] + b"0" + match.group(0)[match.end(1) - match.start(0):]
            go_body, rust_body = pattern.sub(zero, go_body), pattern.sub(zero, rust_body)
        out.append((go_body, rust_body))
    return out


def _header_values(headers, name):
    values = [v for k, v in headers if k.lower() == name]
    return ", ".join(values) if values else None


def _check_header_predicate(predicates, divergence, index, spec, go_value, rust_value):
    if "relation" in spec:
        relation = spec["relation"]
        moved = _move_token(rust_value, relation) if rust_value is not None else None
        if moved != go_value:
            predicates.fail(divergence, index, f"{spec['name']}: Go {go_value!r} is not Rust {rust_value!r} "
                                               f"with {relation['move_token']} moved after {relation['after']}")
    elif not _match(spec["go"], go_value) or not _match(spec["rust"], rust_value):
        predicates.fail(divergence, index, f"{spec['name']}: Go {go_value!r} Rust {rust_value!r} "
                                           f"(expected Go {spec['go']} Rust {spec['rust']})")
    if go_value != rust_value:
        predicates.hit(divergence)


def _move_token(value, relation):
    tokens = value.split(relation.get("separator", ","))
    token, after = relation["move_token"], relation["after"]
    if token not in tokens or after not in tokens:
        return value
    tokens.remove(token)
    tokens.insert(tokens.index(after) + 1, token)
    return relation.get("separator", ",").join(tokens)


def _without(headers, names):
    return [[k, v] for k, v in headers if k.lower() not in names]


def _masked(headers, generated):
    return [[k, "<generated>" if k.lower() in generated else v] for k, v in headers]


def _json_at(document, pointer):
    node = document
    for part in [p for p in pointer.split("/") if p]:
        node = node[int(part)] if isinstance(node, list) else node[part]
    return node


def _set_json(document, pointer, value):
    parts = [p for p in pointer.split("/") if p]
    node = document
    for part in parts[:-1]:
        node = node[int(part)] if isinstance(node, list) else node[part]
    if isinstance(node, list):
        node[int(parts[-1])] = value
    else:
        node[parts[-1]] = value


def _json_transform(predicates, divergence, index, spec, go_doc, rust_doc):
    for side, doc in (("go", go_doc), ("rust", rust_doc)):
        for item in spec.get(f"{side}_only", []):
            array = _json_at(doc, item["array"])
            matches = [e for e in array if isinstance(e, dict) and e.get(item["key"]) == item["value"]]
            other = _json_at(rust_doc if side == "go" else go_doc, item["array"])
            if len(matches) != 1 or any(isinstance(e, dict) and e.get(item["key"]) == item["value"] for e in other):
                predicates.fail(divergence, index, f"{item['value']} is not exactly once and only on {side}")
                continue
            array.remove(matches[0])
            predicates.hit(divergence)
    for item in spec.get("unordered", []):
        arrays = [_json_at(doc, item["array"]) for doc in (go_doc, rust_doc)]
        keys = [[e.get(item["key"]) if isinstance(e, dict) else None for e in a] for a in arrays]
        if any(None in k or len(k) != len(set(k)) for k in keys):
            predicates.fail(divergence, index, f"{item['array']} entries lack a unique {item['key']}")
            continue
        if keys[0] != keys[1]:
            predicates.hit(divergence)
        for array in arrays:
            array.sort(key=lambda e: e[item["key"]])
    for item in spec.get("generated", []):
        for doc in (go_doc, rust_doc):
            try:
                value = _json_at(doc, item["pointer"])
            except (KeyError, IndexError, TypeError):
                value = None
            if not isinstance(value, (str, int)) or not re.fullmatch(item["regex"], str(value)):
                predicates.fail(divergence, index, f"{item['pointer']} {value!r} is not {item['regex']}")
                continue
            _set_json(doc, item["pointer"], "<generated>")
        predicates.hit(divergence)


def _template(template, value):
    """A side expectation: strings 're:...' are regexes, other values exact."""
    if isinstance(template, dict):
        return isinstance(value, dict) and set(template) == set(value) and all(
            _template(t, value[k]) for k, t in template.items())
    if isinstance(template, list):
        return isinstance(value, list) and len(template) == len(value) and all(
            _template(t, v) for t, v in zip(template, value))
    if isinstance(template, str) and template.startswith("re:"):
        return re.fullmatch(template[3:], str(value)) is not None
    return template == value


def _side_expectation(predicates, divergence, index, spec, side, observation, case, run):
    def fail(message):
        predicates.fail(divergence, index, f"{side}: {message}")
    replies = observation["downstream"]
    if "status" in spec and [r.get("status") for r in replies] != [spec["status"]] * len(replies):
        fail(f"status {[r.get('status') for r in replies]} is not {spec['status']}")
    if "body_equals" in spec and [_bytes(r.get("body_hex")).decode("utf-8", "replace") for r in replies] \
            != [spec["body_equals"]] * len(replies):
        fail("body differs from the expected bytes")
    if "body_json" in spec:
        for reply in replies:
            try:
                ok = _template(spec["body_json"], json.loads(_bytes(reply.get("body_hex"))))
            except ValueError:
                ok = False
            if not ok:
                fail("body does not have the expected JSON shape")
    inference = [r for r in observation["upstream"]["http"] if is_inference(r, case)]
    if "inference_requests" in spec and len(inference) != spec["inference_requests"]:
        fail(f"{len(inference)} inference requests, expected {spec['inference_requests']}")
    for check in spec.get("upstream_json", []):
        for request in inference:
            try:
                value = _json_at(json.loads(_bytes(request["body_hex"])), check["pointer"])
            except (ValueError, KeyError, IndexError, TypeError):
                value = None
            text = value if value is None or isinstance(value, str) else json.dumps(value)
            if not _match({k: check[k] for k in ("equals", "regex") if k in check}, text):
                fail(f"upstream {check['pointer']} {value!r} does not match")
    for check in spec.get("upstream_headers", []):
        for request in inference:
            if not _match(check["value"], _header_values(request["headers"], check["name"])):
                fail(f"upstream header {check['name']} does not match")
    if "tls_like" in spec:
        reference = run.observation(spec["tls_like"]["case"], spec["tls_like"]["side"])
        if _tls(observation) != _tls(reference):
            fail(f"TLS handshakes differ from {spec['tls_like']['side']} {spec['tls_like']['case']}")
    if "tls_count" in spec and len(observation["upstream"]["tls"]) != spec["tls_count"]:
        fail(f"{len(observation['upstream']['tls'])} TLS handshakes, expected {spec['tls_count']}")
    predicates.hit(divergence)


def _tls(observation):
    return [{k: h.get(k) for k in ("record_layout", "profile", "negotiated_alpn", "negotiated_version",
                                   "session_reused")} for h in observation["upstream"]["tls"]]


# --- comparison --------------------------------------------------------------------

def compare(case, go, rust, predicates, pins, run):
    """{dimension: [problems]} after the named predicates are applied."""
    results = {d: [] for d in DIMENSIONS}
    replaced = {}
    for divergence, index, spec in predicates.of("replace"):
        for side, observation in (("go", go), ("rust", rust)):
            _side_expectation(predicates, divergence, index, spec[side], side, observation, case, run)
        for dimension in spec["covers"]:
            replaced[dimension] = divergence["id"]
    generated_up = set(pins["generated_headers"]["upstream"])
    generated_down = set(pins["generated_headers"]["downstream"])

    if "status" not in replaced:
        go_status = [(r.get("status"), r.get("reason")) for r in go["downstream"]]
        rust_status = [(r.get("status"), r.get("reason")) for r in rust["downstream"]]
        if go_status != rust_status:
            results["status"].append(f"Go {go_status} Rust {rust_status}")
    if len(go["downstream"]) != len(rust["downstream"]):
        results["status"].append("different number of client exchanges")
    for key in ("transport_error", "read_error", "client_closed_after_event"):
        if [r.get(key) for r in go["downstream"]] != [r.get(key) for r in rust["downstream"]]:
            results["status"].append(f"{key} differs")

    go_up, rust_up = go["upstream"]["http"], rust["upstream"]["http"]
    if "upstream" not in replaced:
        if [r["request_line"] for r in go_up] != [r["request_line"] for r in rust_up]:
            results["upstream-requests"].append(
                f"Go {[r['request_line'] for r in go_up]} Rust {[r['request_line'] for r in rust_up]}")
        if [r.get("write_result") for r in go_up] != [r.get("write_result") for r in rust_up]:
            results["upstream-requests"].append("upstream write results differ")
        header_specs = predicates.of("upstream_header")
        named = {spec["name"] for _, _, spec in header_specs}
        for i, (g, r) in enumerate(zip(go_up, rust_up)):
            if is_inference(g, case):
                for divergence, index, spec in header_specs:
                    _check_header_predicate(predicates, divergence, index, spec,
                                            _header_values(g["headers"], spec["name"]),
                                            _header_values(r["headers"], spec["name"]))
                drop = named
            else:
                drop = set()
            length_differs = _header_values(g["headers"], "content-length") != _header_values(r["headers"], "content-length")
            ignore = drop | ({"content-length"} if length_differs else set())
            go_heads = _masked(_without(g["headers"], ignore), generated_up)
            rust_heads = _masked(_without(r["headers"], ignore), generated_up)
            if go_heads != rust_heads:
                results["upstream-headers"].append(f"request {i}: Go {go_heads} Rust {rust_heads}")
        pairs = [(_bytes(g["body_hex"]), _bytes(r["body_hex"])) for g, r in zip(go_up, rust_up)
                 if is_inference(g, case)]
        for i, (g, r) in enumerate(_apply_edits(predicates, "upstream_body_edit", pairs)):
            if g != r:
                results["upstream-body"].append(f"inference request {i} differs byte for byte")
        for i, (g, r) in enumerate(zip(go_up, rust_up)):
            if not is_inference(g, case) and g["body_hex"] != r["body_hex"]:
                results["upstream-body"].append(f"request {i} ({g['request_line']}) differs byte for byte")

    for i, (g, r) in enumerate(zip(go["downstream"], rust["downstream"])):
        down_specs = predicates.of("downstream_header")
        for divergence, index, spec in down_specs:
            _check_header_predicate(predicates, divergence, index, spec,
                                    _header_values(g.get("headers", []), spec["name"]),
                                    _header_values(r.get("headers", []), spec["name"]))
        names = {spec["name"] for _, _, spec in down_specs}
        length_differs = _header_values(g.get("headers", []), "content-length") != \
            _header_values(r.get("headers", []), "content-length")
        ignore = names | ({"content-length"} if length_differs else set())
        go_map, rust_map = _field_map(g.get("headers", []), ignore, generated_down), \
            _field_map(r.get("headers", []), ignore, generated_down)
        if go_map != rust_map:
            differing = sorted(k for k in go_map.keys() | rust_map.keys() if go_map.get(k) != rust_map.get(k))
            results["downstream-headers"].append(f"reply {i}: field values differ: {differing}")

    if "downstream-body" not in replaced:
        pairs = [(_bytes(g.get("body_hex")), _bytes(r.get("body_hex"))) for g, r in zip(go["downstream"], rust["downstream"])]
        pairs = _apply_timestamps(predicates, pairs)
        for i, (gb, rb) in enumerate(_apply_edits(predicates, "downstream_body_edit", pairs)):
            if gb == rb:
                continue
            json_specs = predicates.of("downstream_json")
            if json_specs:
                try:
                    go_doc, rust_doc = json.loads(gb), json.loads(rb)
                except ValueError:
                    results["downstream-body"].append(f"reply {i}: differs and is not JSON")
                    continue
                for divergence, index, spec in json_specs:
                    _json_transform(predicates, divergence, index, spec, go_doc, rust_doc)
                if go_doc == rust_doc:
                    continue
            results["downstream-body"].append(f"reply {i} differs byte for byte")

    if "tls" not in replaced:
        go_tls, rust_tls = _tls(go), _tls(rust)
        specs = predicates.of("tls_extra_go_handshakes")
        for divergence, index, spec in specs:
            extra = len(go_tls) - len(rust_tls)
            reference = _tls(run.observation(spec["reference"]["case"], spec["reference"]["side"]))
            reference = reference[spec["reference"]["index"]] if len(reference) > spec["reference"]["index"] else None
            if extra != spec["count"] or any(h != reference for h in go_tls[len(rust_tls):]):
                predicates.fail(divergence, index, f"Go has {extra} extra handshakes, expected {spec['count']} "
                                                   "matching the reference handshake")
            else:
                predicates.hit(divergence)
                go_tls = go_tls[:len(rust_tls)]
        if go_tls != rust_tls:
            results["tls"].append("TLS handshakes differ")

    go_cred, rust_cred = dict(go.get("credential_after", {})), dict(rust.get("credential_after", {}))
    for divergence, index, spec in predicates.of("credential_field"):
        field = spec["field"]
        values = {side: cred.get(field, "<absent>") for side, cred in (("go", go_cred), ("rust", rust_cred))}
        if values["go"] == values["rust"] and divergence.get("may_be_absent"):
            continue
        if values != {"go": spec["go"], "rust": spec["rust"]}:
            predicates.fail(divergence, index, f"{field}: Go {values['go']!r} Rust {values['rust']!r}")
        else:
            predicates.hit(divergence)
        go_cred.pop(field, None)
        rust_cred.pop(field, None)
    for divergence, index, spec in predicates.of("credential_timestamps"):
        for field in spec["fields"]:
            g, r = go_cred.get(field), rust_cred.get(field)
            if g == r:
                continue
            try:
                skew = abs((_time(g) - _time(r)).total_seconds())
            except (TypeError, ValueError):
                skew = None
            if skew is None or skew > spec["max_skew_seconds"]:
                predicates.fail(divergence, index, f"{field}: Go {g!r} Rust {r!r}")
                continue
            predicates.hit(divergence)
            go_cred.pop(field)
            rust_cred.pop(field)
    if go_cred != rust_cred:
        differing = sorted(k for k in go_cred.keys() | rust_cred.keys() if go_cred.get(k) != rust_cred.get(k))
        results["credential"].append(f"persisted credential differs: {differing}")
    return results


def _time(value):
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))


def _field_map(headers, ignore, generated):
    out = {}
    for name, value in headers:
        key = name.lower()
        if key in ignore:
            continue
        out.setdefault(key, []).append("<generated>" if key in generated else value)
    return out


# --- raw differences -------------------------------------------------------------

RAW = [
    (r"/downstream/\d+/(status|reason)", ("status",)),
    (r"/downstream/\d+/headers(/.*)?", ("downstream-headers",)),
    (r"/downstream/\d+/(body_hex|body_text)", ("downstream-body",)),
    (r"/downstream/\d+/raw_response_hex", ("downstream-headers", "downstream-body", "status")),
    (r"/upstream/http/\d+/(headers(/.*)?|raw_head_hex)", ("upstream-headers",)),
    (r"/upstream/http/\d+/(body_hex|body_text)", ("upstream-body",)),
    (r"/upstream/http(/length|/\d+|/\d+/(request_line|write_result))", ("upstream-requests",)),
    (r"/upstream/tls(/.*)?", ("tls",)),
    (r"/credential_after(/.*)?", ("credential",)),
]


def explain(diffs, results):
    """Raw differences outside a passing dimension (unknown paths included)."""
    unexplained = []
    for diff in diffs:
        for pattern, dimensions in RAW:
            if re.fullmatch(pattern, diff["path"]):
                if any(results[d] for d in dimensions):
                    unexplained.append(diff)
                break
        else:
            unexplained.append(diff)
    return unexplained


def judge(case, go, rust, diffs, divergences, target, pins, run, root):
    """The verdict of one case: invariants, dimensions, predicates, raw differences."""
    predicates = Predicates(divergences, target, case["name"])
    problems = {f"invariant:{side}": invariants(case, obs, side, pins, root)
                for side, obs in (("go", go), ("rust", rust))}
    results = compare(case, go, rust, predicates, pins, run)
    unexplained = explain(diffs, results)
    failed = {k: v for k, v in {**problems, **results}.items() if v}
    if predicates.failures:
        failed["predicates"] = predicates.failures
    return {"name": case["name"], "verdict": "fail" if failed or unexplained else "pass",
            "failed": failed, "unexplained": unexplained[:20], "unexplained_count": len(unexplained),
            "differences": len(diffs), "hits": sorted(predicates.hits)}


class Run:
    """Lazy access to a run directory's observations, for cross-case references."""

    def __init__(self, directory):
        self.directory = Path(directory)
        self.cache = {}

    def observation(self, case, side):
        key = (case, side)
        if key not in self.cache:
            self.cache[key] = json.loads((self.directory / case / f"{side}.json").read_text())
        return copy.deepcopy(self.cache[key])
