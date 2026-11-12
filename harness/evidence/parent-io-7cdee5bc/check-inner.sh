#!/usr/bin/env bash
set -euo pipefail
python3 /review/safety.py > /review/safety.json
python3 /review/snapshot.py /review/before-accept.json
(cd harness && python3 -m unittest discover -v) > /review/logs/all-python.txt 2>&1
./harness/accept > /review/logs/accept.txt 2>&1
python3 /review/real-cli.py > /review/logs/real-cli.txt 2>&1
python3 /review/snapshot.py /review/after-accept.json
python3 -I - <<'PY'
import json
from pathlib import Path
before,after=[json.loads(Path('/review/'+n+'-accept.json').read_text()) for n in ('before','after')]
assert before==after and not after['status']
print('HEAD, source, index and all 12 protected executables unchanged')
PY
