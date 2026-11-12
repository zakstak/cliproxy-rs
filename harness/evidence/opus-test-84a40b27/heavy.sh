#!/usr/bin/env bash
set -euo pipefail
exec 9>/tmp/cliproxy-selective-heavy-build.lock
flock 9
python3 -I - <<'PY'
import shutil
for path in ('/home/agent', '/tmp'):
    free = shutil.disk_usage(path).free
    print(f'post-lock free: {path}: {free}', flush=True)
    if free < 8_000_000_000:
        raise SystemExit(f'heavy command refused below disk floor: {path}')
PY
exec /tmp/h1-rereview-60928bbf/opus-test-1e0aa543/sandbox.sh "$@"
