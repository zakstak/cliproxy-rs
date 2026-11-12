"""Mutation controls for the current-artifact fix: each mutant removes one guard from the
working-tree harness (copied into an exported tree) and the named tests must then fail.
Compile-free. Usage: python3 -I fix-mutants.py EXPORT_DIR REPO"""
import shutil
import subprocess
import sys
from pathlib import Path

EXPORT, REPO = Path(sys.argv[1]), Path(sys.argv[2])
FILES = ("acceptance.py", "provenance.py", "test_acceptance.py", "no-compile-rustc")
T = "test_acceptance."
MUTANTS = [
    ("binding-removed", "acceptance.py",
     '    problems = []\n    for execution in receipt["executions"]:\n        key = f"{execution[\'package\']} {execution[\'target\']}"\n        want, got',
     '    return []\n    problems = []\n    for execution in receipt["executions"]:\n        key = f"{execution[\'package\']} {execution[\'target\']}"\n        want, got',
     [T + "CurrentArtifactTests.test_another_real_build_of_the_same_crate_fails",
      T + "CurrentArtifactTests.test_a_replay_passes_only_while_its_artifacts_are_current",
      T + "CurrentArtifactTests.test_a_query_result_missing_one_target_fails"]),
    ("unavailable-counts-as-pass", "acceptance.py",
     'verdict = "fail" if problems else ("pass" if cargo.get("available") is True else "unavailable")',
     'verdict = "fail" if problems else "pass"',
     [T + "CurrentArtifactTests.test_an_unavailable_query_is_never_a_pass"]),
    ("stale-ignored", "acceptance.py", '        if build["stale"]:\n', '        if False:\n',
     [T + "CargoQueryTests.test_each_unavailable_state"]),
    ("extra-executables-ignored", "acceptance.py", '        if extra:\n            problems.append(f"cargo query for',
     '        if False:\n            problems.append(f"cargo query for', [T + "CargoQueryTests.test_each_unavailable_state"]),
    ("refusals-ignored", "acceptance.py", '    if refused:\n        problems.append(f"no-compile-rustc',
     '    if False:\n        problems.append(f"no-compile-rustc', [T + "CargoQueryTests.test_each_unavailable_state"]),
    ("source-change-ignored", "acceptance.py", '    if not source_before == source_after == current_source:',
     '    if False:', [T + "CargoQueryTests.test_each_unavailable_state"]),
    ("post-hash-ignored", "acceptance.py", '        if not path.is_file() or provenance.sha256(path) != artifact["sha256"]:\n            problems.append(f"{key}: executable bytes changed',
     '        if False:\n            problems.append(f"{key}: executable bytes changed', [T + "CargoQueryTests.test_each_unavailable_state"]),
    ("online-query", "acceptance.py", '[t for q, t in targets if q == p]), "--offline"]', '[t for q, t in targets if q == p])]',
     [T + "CargoQueryTests.test_a_clean_query_names_every_target_and_records_how_it_ran"]),
    ("wrapper-allows-compilation", "no-compile-rustc", '    if [[ $arg == -vV || $arg == ___ ]]; then',
     '    if true; then', [T + "NoCompileWrapperTests.test_compilation_is_refused_and_logged"]),
    ("failing-records-query-cargo", "acceptance.py", '    if problems:\n        cargo = {"queried": False',
     '    if False:\n        cargo = {"queried": False', [T + "CurrentArtifactTests.test_failing_records_do_not_query_cargo"]),
]


def install(mutant=None):
    for name in FILES:
        shutil.copy2(REPO / "harness" / name, EXPORT / "harness" / name)
    if mutant:
        _, file, old, new, _ = mutant
        path = EXPORT / "harness" / file
        text = path.read_text()
        assert text.count(old) == 1, (mutant[0], text.count(old))
        path.write_text(text.replace(old, new))


def run(tests):
    return subprocess.run([sys.executable, "-m", "unittest", *tests], cwd=EXPORT / "harness", capture_output=True,
                          text=True, env={"PATH": "/usr/bin:/bin", "PYTHONDONTWRITEBYTECODE": "1"})


install()
baseline = run([T + c for c in ("CurrentArtifactTests", "CargoQueryTests", "NoCompileWrapperTests", "CombineTests",
                                 "CargoBindingTests")])
print(f"baseline (unmutated working tree): exit {baseline.returncode} {baseline.stderr.strip().splitlines()[-1]}")
bad = baseline.returncode != 0
for mutant in MUTANTS:
    install(mutant)
    done = run(mutant[4])
    caught = done.returncode != 0 and "FAIL" in done.stderr
    bad |= not caught
    print(f"{'CAUGHT' if caught else 'MISSED'} {mutant[0]}: exit {done.returncode} {done.stderr.strip().splitlines()[-1]}")
install()
raise SystemExit(bad)
