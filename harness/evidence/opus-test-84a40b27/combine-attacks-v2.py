"""v2 of combine-attacks.py: the same 150 probes, run on the FRESH receipts named by argv[1]
(produced by ./harness/accept at the clean head under test). An attack counts only if the
CLI exits 1 with verdict fail AND a problem matches the reason INTENDED() names for it.
Verdict unavailable (exit 2) is recorded as such and never counts as a rejection.
(Opus test-owner evidence; no projection.) Each probe edits a
deep copy, writes it, runs the real CLI and records exit/problems. Legitimate controls
expect exit 0; attacks expect exit 1.

    python3 -I combine-attacks.py FRESH_RECEIPT_DIR OUT_DIR
"""
import copy
import json
import re
from pathlib import Path
import subprocess
import sys

ROOT = Path('/home/agent/git/cliproxy-rs')
FRESH, OUT = Path(sys.argv[1]), Path(sys.argv[2])
OUT.mkdir(parents=True, exist_ok=False)
ORDER = ['differential-historical', 'mutation-historical', 'differential-selective', 'mutation-selective',
         'cargo-groups']
RAW = {n: (FRESH / f'{n}.json').read_bytes() for n in ORDER}
BASE = {n: json.loads(b) for n, b in RAW.items()}
RESULTS = []


def run(label, edit=None, expected=1, files=None, raw=False, note=''):
    d = OUT / label
    d.mkdir()
    rs = copy.deepcopy(BASE)
    if edit:
        edit(rs)
    paths = []
    for name in (files or ORDER):
        src = name.split('#')[0]
        path = d / f'{name.replace("#", "-")}.json'
        if raw and edit is None:
            path.write_bytes(RAW[src])          # byte-identical fresh receipt
        else:
            path.write_text(json.dumps(rs[src], indent=None if raw else 2, sort_keys=raw))
        paths.append(str(path))
    cmd = [sys.executable, str(ROOT / 'harness/acceptance.py'), 'combine', *paths,
           '--receipt', str(d / 'acceptance.json')]
    done = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    (d / 'stdout.txt').write_text(done.stdout)
    (d / 'stderr.txt').write_text(done.stderr)
    result = json.loads((d / 'acceptance.json').read_text()) if (d / 'acceptance.json').exists() else {}
    rec = dict(label=label, expected=expected, exit=done.returncode, verdict=result.get('verdict'),
               problems=result.get('problems'), unavailable=result.get('unavailable'),
               cargo_queried=(result.get('current_cargo') or {}).get('queried'), note=note, command=cmd)
    RESULTS.append(rec)
    if done.returncode != {'pass': 0, 'fail': 1, 'unavailable': 2}.get(result.get('verdict')):
        rec['exit'] = f'harness-error({done.returncode})'   # a crash never counts as a rejection
    rec['reason'] = None if expected == 0 else INTENDED(label)
    rec['reason_found'] = expected == 0 or any(re.search(rec['reason'], p) for p in result.get('problems') or [])
    ok = 'OK ' if rec['exit'] == expected and rec['reason_found'] else 'UNEXPECTED'
    print(f'{ok} {label}: exit {rec["exit"]} (expected {expected}) {json.dumps((result.get("problems") or [])[:2])[:300]}',
          flush=True)


R = {"dh": "differential-historical", "mh": "mutation-historical", "ds": "differential-selective",
     "ms": "mutation-selective", "cg": "cargo-groups"}
RULES = [  # (label regex, required problem regex or function of the match); first match wins
    (r"^receipt-missing-differential-(historical|selective)$", lambda m: f"differential:{m.group(1)} appears 0 times"),
    (r"^receipt-missing-mutation-(historical|selective)$", lambda m: f"mutation-control:{m.group(1)} appears 0 times"),
    (r"^receipt-missing-cargo-groups$", "cargo-groups:None appears 0 times"),
    (r"^receipt-duplicate-cargo$", "cargo-groups:None appears 2 times"),
    (r"^t-sub-(other-target-real-hash|path-only|escape-path)$", r"cpa-core lib: executable .* is not this target's test binary"),
    (r"^t-sub-(nonexistent|wrong-hash)$", r"cpa-core lib: .* is not on disk with the recorded SHA-256"),
    (r"^t-sub-(artifact-after-differs|fresh-after-false)$", "cpa-core lib: the artifact after the run is not the same fresh test binary"),
    (r"^t-sub-src-path$", "cpa-core lib: artifact src_path"),
    (r"^t-sub-profile-test-false$", "cpa-core lib: artifact profile_test False is not True"),
    (r"^t-sub-same-crate-other-build-(cpa_core|cpa_exec)-", lambda m: (
        m.group(1).replace("_", "-") + r" lib: recorded artifact target/debug/deps/" + m.group(1)
        + r"-[0-9a-f]{16} [0-9a-f]{12} is not cargo's current fresh artifact for this target "
        + r"\(current: target/debug/deps/" + m.group(1) + r"-[0-9a-f]{16} [0-9a-f]{12}\)")),
    (r"^copy-record-relabelled$", "cpa-core lib: artifact package 'cpa-exec' is not 'cpa-core'"),
    (r"^copy-record-(duplicated|replaces-another)$", "test executions are not every manifest test target once each"),
    (r"^copy-results-from-other$", "cpa-core lib: result lines are not one ok per requested test"),
    (r"^copy-listing-from-other$", r"cpa-core lib: summary .* is not the one complete summary"),
    (r"^copy-selective-cases-into-historical$", r"differential-historical\.json: cases are not the 50 expected"),
    (r"^copy-historical-mutations-into-selective$", r"mutation-selective\.json: controls are not the 30 required mutations"),
    (r"^missing-exec-(package|target)$", "test executions are not every manifest test target once each"),
    (r"^missing-exec-", "cpa-core lib: execution record needs exactly"),
    (r"^missing-artifact-", "cpa-core lib: artifact identity needs exactly"),
    (r"^missing-listing-", "cpa-core lib: listing is not a complete, clean --list of this executable"),
    (r"^missing-execution-", "cpa-core lib: execution is not a clean exit-0 --exact run"),
    (r"^missing-cargo-source_after$", "source after the cargo run is not the current identity"),
    (r"^missing-(historical|selective)-driver$", lambda m: f"differential-{m.group(1)}" + r"\.json: driver binary record missing"),
    (r"^missing-(historical|selective)-go$", lambda m: f"differential-{m.group(1)}" + r"\.json: before: Go binary record missing"),
    (r"^missing-(.*)-source$", lambda m: m.group(1) + r"\.json: tested source was not a clean worktree"),
    (r"^missing-(.*)-provenance_verified$", lambda m: m.group(1) + r"\.json: provenance not verified"),
    (r"^missing-(.*)-verdict$", lambda m: m.group(1) + r"\.json: verdict None"),
    (r"^missing-(.*)-partial$", lambda m: m.group(1) + r"\.json: verdict 'pass', partial None"),
    (r"^rust_revision-deleted-(.*)$", lambda m: m.group(1) + r"\.json: rust_revision is missing or not the recorded source revision"),
    (r"^rust_revision-", r"cargo-groups\.json: rust_revision is missing or not the recorded source revision"),
    (r"^source-head-prior-with-matching-revision$", r"cargo-groups\.json: tested source 2f2b1209\w+ is not the current source"),
    (r"^source-dirty$", r"cargo-groups\.json: tested source was not a clean worktree"),
    (r"^source-manifest-hash$", r"differential-selective\.json: tested source \w+ is not the current source"),
    (r"^agg-counts-(inflated|deflated)$", r"cargo-groups\.json: counts .* are not the recomputed"),
    (r"^agg-groups-empty$", "tests are not the manifest's"),
    (r"^agg-group-dropped$|^agg-groups-reordered$", "groups are not every manifest group once each in order"),
    (r"^agg-test-duplicate$", "group claude-router: tests are not the manifest's 4 once each in order"),
    (r"^agg-test-failed-label$|^agg-group-passed-stated-wrong$", "group claude-router: not every test passed in a verified execution"),
    (r"^agg-exec-dropped-labels-kept$|^agg-no-executions$", "test executions are not every manifest test target once each"),
    (r"^agg-unavailable-nonempty$", "receipt lists unavailable steps"),
    (r"^agg-unknown-tests-claimed$", "owned targets list tests outside the manifest"),
    (r"^agg-partial-true$", "verdict 'pass', partial True"),
    (r"^agg-(exit42|timeout)$", "cpa-core lib: execution is not a clean exit-0 --exact run"),
    (r"^agg-(results-empty|result-failed-line)$", "cpa-core lib: result lines are not one ok per requested test"),
    (r"^agg-summary-", r"cpa-core lib: summary .* is not the one complete summary"),
    (r"^agg-requested-subset$", r"cpa-core lib: requested tests .* are not the manifest"),
    (r"^agg-(historical|selective)-case-(missing|duplicate|extra)$",
     lambda m: f"differential-{m.group(1)}" + r"\.json: cases are not the \d+ expected once each in order"),
    (r"^agg-(historical|selective)-case-fail$", lambda m: f"differential-{m.group(1)}" + r"\.json: case buffered: not a clean pass"),
    (r"^agg-(historical|selective)-counts-wrong$", lambda m: f"differential-{m.group(1)}" + r"\.json: counts .* are not"),
    (r"^agg-(historical|selective)-go-(dirty|wrong-pin)$",
     lambda m: f"differential-{m.group(1)}" + r"\.json: before: Go binary record is not a clean build of the pin"),
    (r"^agg-(historical|selective)-driver-wrong-hash$", lambda m: f"differential-{m.group(1)}" + r"\.json: driver binary"),
    (r"^agg-(historical|selective)-control(s-missing|-duplicate|-unknown)$",
     lambda m: f"mutation-{m.group(1)}" + r"\.json: controls are not the \d+ required mutations"),
    (r"^agg-(historical|selective)-control-not-caught$", lambda m: f"mutation-{m.group(1)}" + r"\.json: mutation .*: not caught"),
    (r"^agg-target-swapped$", r"differential-historical\.json: go_commit .* is not the manifest's"),
]


def INTENDED(label):
    for pattern, reason in RULES:
        m = re.match(pattern, label)
        if m:
            return reason(m) if callable(reason) else reason
    raise KeyError(f"no intended reason for {label}")


C = 'cargo-groups'
ex = lambda rs, i=0: rs[C]['executions'][i]


def swap_identity(rs, i, j):
    """Target substitution: execution i claims execution j's real artifact (path+hash), commands kept consistent."""
    a, b = ex(rs, i), ex(rs, j)
    old, new = a['artifact']['executable'], b['artifact']['executable']
    rs[C]['executions'][i] = json.loads(json.dumps(a).replace(old, new))
    for k in ('artifact', 'artifact_after'):
        rs[C]['executions'][i][k] = dict(rs[C]['executions'][i][k], sha256=b['artifact']['sha256'])


def stale_same_crate(rs, i, path, digest):
    """Substitute an on-disk binary of the same crate (other metadata hash) with its real SHA-256."""
    a = ex(rs, i)
    old = a['artifact']['executable']
    rs[C]['executions'][i] = json.loads(json.dumps(a).replace(old, path))
    for k in ('artifact', 'artifact_after'):
        rs[C]['executions'][i][k]['sha256'] = digest


# ---- legitimate controls ---------------------------------------------------------------
run('control-fresh-bytes', expected=0, raw=True, note='byte-identical fresh receipts')
run('control-reserialized-sorted', lambda rs: None, expected=0, raw=True, note='same data, sorted keys, compact')
run('control-permuted-order', expected=0, raw=True, files=list(reversed(ORDER)), note='receipt argument order')

# ---- missing / extra / duplicate receipts ------------------------------------------------
for n in ORDER:
    run(f'receipt-missing-{n}', files=[x for x in ORDER if x != n])
run('receipt-duplicate-cargo', files=ORDER + [f'{C}#dup'])

# ---- target substitution ------------------------------------------------------------------
run('t-sub-other-target-real-hash', lambda rs: swap_identity(rs, 0, 1))
run('t-sub-path-only', lambda rs: rs[C]['executions'].__setitem__(
    0, json.loads(json.dumps(ex(rs)).replace(ex(rs)['artifact']['executable'], ex(rs, 1)['artifact']['executable']))))
run('t-sub-nonexistent', lambda rs: rs[C]['executions'].__setitem__(
    0, json.loads(json.dumps(ex(rs)).replace(ex(rs)['artifact']['executable'], 'target/debug/deps/cpa_core-0000000000000000'))))
run('t-sub-wrong-hash', lambda rs: [ex(rs)[k].update(sha256='0' * 64) for k in ('artifact', 'artifact_after')])
run('t-sub-artifact-after-differs', lambda rs: ex(rs)['artifact_after'].update(sha256=ex(rs, 1)['artifact']['sha256']))
run('t-sub-fresh-after-false', lambda rs: ex(rs).update(fresh_after=False))
run('t-sub-src-path', lambda rs: [ex(rs)[k].update(src_path='crates/cpa-core/src/main.rs') for k in ('artifact', 'artifact_after')])
run('t-sub-profile-test-false', lambda rs: [ex(rs)[k].update(profile_test=False) for k in ('artifact', 'artifact_after')])
run('t-sub-escape-path', lambda rs: rs[C]['executions'].__setitem__(
    0, json.loads(json.dumps(ex(rs)).replace(ex(rs)['artifact']['executable'], 'target/debug/deps/../deps/cpa_core-bd184f74f32568ca'))))
for path, digest in json.loads(Path(sys.argv[3]).read_text()).items() if len(sys.argv) > 3 else []:
    idx = next(i for i, e in enumerate(BASE[C]['executions'])
               if e['artifact']['executable'].rsplit('-', 1)[0] == path.rsplit('-', 1)[0])
    run(f't-sub-same-crate-other-build-{Path(path).name}', lambda rs, i=idx, p=path, h=digest: stale_same_crate(rs, i, p, h),
        note='real on-disk non-H1 binary of the same crate with its true SHA-256; tests the structural ceiling')

# ---- copied executions --------------------------------------------------------------------
run('copy-record-relabelled', lambda rs: rs[C]['executions'].__setitem__(
    0, dict(copy.deepcopy(ex(rs, 1)), package=ex(rs)['package'], target=ex(rs)['target'])))
run('copy-record-duplicated', lambda rs: rs[C]['executions'].append(copy.deepcopy(ex(rs))))
run('copy-record-replaces-another', lambda rs: rs[C]['executions'].__setitem__(1, copy.deepcopy(ex(rs))))
run('copy-results-from-other', lambda rs: ex(rs).update(results=copy.deepcopy(ex(rs, 7)['results'])))
run('copy-listing-from-other', lambda rs: ex(rs)['listing'].update(names=copy.deepcopy(ex(rs, 1)['listing']['names'])))
run('copy-selective-cases-into-historical', lambda rs: rs['differential-historical'].update(
    cases=copy.deepcopy(rs['differential-selective']['cases'])))
run('copy-historical-mutations-into-selective', lambda rs: rs['mutation-selective'].update(
    controls=copy.deepcopy(rs['mutation-historical']['controls']), counts=copy.deepcopy(rs['mutation-historical']['counts'])))

# ---- missing fields -----------------------------------------------------------------------
for k in sorted(BASE[C]['executions'][0]):
    run(f'missing-exec-{k}', lambda rs, k=k: ex(rs).pop(k))
for k in sorted(BASE[C]['executions'][0]['artifact']):
    run(f'missing-artifact-{k}', lambda rs, k=k: ex(rs)['artifact'].pop(k))
for k in ('command', 'cwd', 'exit', 'timeout', 'names'):
    run(f'missing-listing-{k}', lambda rs, k=k: ex(rs)['listing'].pop(k))
for k in ('command', 'cwd', 'isolation', 'exit', 'timeout'):
    run(f'missing-execution-{k}', lambda rs, k=k: ex(rs)['execution'].pop(k))
for n in ORDER:
    for k in ('source', 'provenance_verified', 'verdict', 'partial'):
        run(f'missing-{n}-{k}', lambda rs, n=n, k=k: rs[n].pop(k))
run('missing-cargo-source_after', lambda rs: rs[C].pop('source_after'))
for t in ('historical', 'selective'):
    run(f'missing-{t}-driver', lambda rs, t=t: rs[f'differential-{t}']['provenance'].pop('driver'))
    run(f'missing-{t}-go', lambda rs, t=t: rs[f'differential-{t}']['provenance'].pop('go'))

# ---- rust_revision ------------------------------------------------------------------------
head = BASE[C]['rust_revision']
for n in ORDER:
    run(f'rust_revision-deleted-{n}', lambda rs, n=n: rs[n].pop('rust_revision'))
run('rust_revision-zero', lambda rs: rs[C].update(rust_revision='0' * 40))
run('rust_revision-short-prefix', lambda rs: rs[C].update(rust_revision=head[:12]))
run('rust_revision-uppercase', lambda rs: rs[C].update(rust_revision=head.upper()))
run('rust_revision-null', lambda rs: rs[C].update(rust_revision=None))
run('rust_revision-prior-tested-2f2b1209', lambda rs: rs[C].update(rust_revision='2f2b12092a10b244be56964157f43aaa30db29d2'))
run('source-head-prior-with-matching-revision', lambda rs: [rs[C]['source'].update(head='2f2b12092a10b244be56964157f43aaa30db29d2'),
                                                          rs[C].update(rust_revision='2f2b12092a10b244be56964157f43aaa30db29d2')])
run('source-dirty', lambda rs: rs[C]['source'].update(clean=False))
run('source-manifest-hash', lambda rs: rs['differential-selective']['source'].update(manifest_sha256='0' * 64))

# ---- aggregate holes ----------------------------------------------------------------------
g0 = lambda rs: rs[C]['groups'][0]
run('agg-counts-inflated', lambda rs: rs[C]['counts'].update(tests=33, passed=33))
run('agg-counts-deflated', lambda rs: rs[C]['counts'].update(tests=31, passed=31))
run('agg-groups-empty', lambda rs: [g.update(tests=[], passed=0, failed=0) for g in rs[C]['groups']])
run('agg-group-dropped', lambda rs: rs[C]['groups'].pop())
run('agg-groups-reordered', lambda rs: rs[C]['groups'].reverse())
run('agg-test-duplicate', lambda rs: g0(rs)['tests'].__setitem__(0, copy.deepcopy(g0(rs)['tests'][1])))
run('agg-test-failed-label', lambda rs: g0(rs)['tests'][0].update(outcome='failed'))
run('agg-group-passed-stated-wrong', lambda rs: g0(rs).update(passed=g0(rs)['passed'] + 1))
run('agg-exec-dropped-labels-kept', lambda rs: rs[C]['executions'].pop(0))
run('agg-no-executions', lambda rs: rs[C].update(executions=[]))
run('agg-unavailable-nonempty', lambda rs: rs[C].update(unavailable=['x']))
run('agg-unknown-tests-claimed', lambda rs: rs[C].update(unknown_tests=['cpa-core lib extra']))
run('agg-partial-true', lambda rs: rs[C].update(partial=True))
run('agg-exit42', lambda rs: ex(rs)['execution'].update(exit=42))
run('agg-timeout', lambda rs: ex(rs)['execution'].update(timeout=True))
run('agg-results-empty', lambda rs: ex(rs).update(results=[]))
run('agg-result-failed-line', lambda rs: ex(rs)['results'][0].update(outcome='FAILED'))
run('agg-summary-missing', lambda rs: ex(rs).update(summary=None))
run('agg-summary-lines-2', lambda rs: ex(rs).update(summary_lines=2))
run('agg-summary-filtered-wrong', lambda rs: ex(rs)['summary'].update(filtered_out=-1))
run('agg-requested-subset', lambda rs: ex(rs).update(requested=ex(rs)['requested'][:1]))
for t, nm in (('historical', 50), ('selective', 62)):
    D = f'differential-{t}'
    run(f'agg-{t}-case-missing', lambda rs, D=D: rs[D]['cases'].pop(0))
    run(f'agg-{t}-case-duplicate', lambda rs, D=D: rs[D]['cases'].__setitem__(0, copy.deepcopy(rs[D]['cases'][1])))
    run(f'agg-{t}-case-extra', lambda rs, D=D: rs[D]['cases'].append({'name': 'unknown', 'verdict': 'pass'}))
    run(f'agg-{t}-case-fail', lambda rs, D=D: rs[D]['cases'][0].update(verdict='fail'))
    run(f'agg-{t}-counts-wrong', lambda rs, D=D: rs[D]['counts'].update(passed=nm - 1))
    run(f'agg-{t}-go-dirty', lambda rs, D=D: rs[D]['provenance']['go'].update(**{'vcs.modified': 'true'}))
    run(f'agg-{t}-go-wrong-pin', lambda rs, D=D: rs[D]['provenance']['go'].update(**{'vcs.revision': 'ae3ee3be303fb07fb8173279923413005bf32177'}))
    run(f'agg-{t}-driver-wrong-hash', lambda rs, D=D: rs[D]['provenance']['driver'].update(sha256='0' * 64))
    M = f'mutation-{t}'
    run(f'agg-{t}-controls-missing', lambda rs, M=M: rs[M].update(controls=[]))
    run(f'agg-{t}-control-duplicate', lambda rs, M=M: rs[M]['controls'].__setitem__(0, copy.deepcopy(rs[M]['controls'][1])))
    run(f'agg-{t}-control-unknown', lambda rs, M=M: rs[M]['controls'][0].update(mutation='unknown'))
    run(f'agg-{t}-control-not-caught', lambda rs, M=M: rs[M]['controls'][0].update(caught=False))
run('agg-target-swapped', lambda rs: [rs['differential-historical'].update(target='selective'),
                                      rs['differential-selective'].update(target='historical')])

bad = [r for r in RESULTS if r['exit'] != r['expected'] or not r['reason_found']]
(OUT / 'results.json').write_text(json.dumps(RESULTS, indent=2) + '\n')
print(json.dumps({'total': len(RESULTS), 'controls': sum(r['expected'] == 0 for r in RESULTS),
                  'attacks': sum(r['expected'] == 1 for r in RESULTS),
                  'unexpected': [(r['label'], r['exit'], r['expected'], r['reason']) for r in bad]}), flush=True)
raise SystemExit(bool(bad))
