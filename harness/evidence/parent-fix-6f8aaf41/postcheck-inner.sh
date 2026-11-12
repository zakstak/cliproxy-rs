set -euo pipefail
python3 /review/fake-compiler-cargo.py > /review/logs/fake-compiler-cargo.txt
python3 - <<'PY'
import json, os, subprocess, sys
from pathlib import Path
root=Path('/home/agent/git/cliproxy-rs')
paths=[root/'harness/.cache/accept-20261112-102510'/f'{name}.json' for name in ('differential-historical','mutation-historical','differential-selective','mutation-selective','cargo-groups')]
dest=Path('/review/missing-cargo-real-receipt.json')
done=subprocess.run([sys.executable,str(root/'harness/acceptance.py'),'combine',*map(str,paths),'--receipt',str(dest)],env=dict(os.environ,PATH='/usr/bin:/bin'),capture_output=True,text=True)
Path('/review/logs/missing-cargo.txt').write_text(done.stdout+done.stderr)
result=json.loads(dest.read_text())
assert done.returncode==2 and result['verdict']=='unavailable' and not result['problems'],(done.returncode,result)
assert result['current_cargo']['queried'] and any('cargo' in p and 'No such file' in p for p in result['unavailable'])
assert 'Traceback' not in done.stderr
print('Real combine CLI: missing Cargo writes unavailable receipt and exits 2')
PY
python3 /review/snapshot.py /review/after-probes.json
