#!/usr/bin/env bash
set -euo pipefail
exec 9>/tmp/cliproxy-selective-heavy-build.lock
flock -w 45 9 || { echo 'shared lock busy after one wait; no command started'; exit 2; }
python3 -I - <<'PY'
import json, shutil
free = {p: shutil.disk_usage(p).free for p in ('/home/agent', '/tmp')}
print(json.dumps({'available_bytes_after_lock': free}), flush=True)
assert all(n >= 8_000_000_000 for n in free.values())
PY
exec /tmp/h1-parent-r7/sandbox.sh bash /review/check-inner.sh
