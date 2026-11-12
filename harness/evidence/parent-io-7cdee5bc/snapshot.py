import hashlib
import json
from pathlib import Path
import subprocess
import sys

root = Path('/home/agent/git/cliproxy-rs')
def sha(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()
def git(*args):
    return subprocess.check_output(['git', '-C', str(root), *args], text=True).strip()
original = json.loads((root / 'harness/evidence/opus-test-84a40b27/snapshot-before.json').read_text())
artifacts = {p: sha(root / p) for p in original['artifacts']}
assert artifacts == original['artifacts']
result = {'head': git('rev-parse', 'HEAD'), 'tree': git('rev-parse', 'HEAD^{tree}'),
          'status': git('status', '--porcelain'), 'index_sha256': sha(root / '.git/index'),
          'tracked': {p: sha(root / p) for p in git('ls-files').splitlines() if (root / p).is_file()},
          'artifacts': artifacts}
Path(sys.argv[1]).write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps({k: v for k, v in result.items() if k not in ('tracked', 'artifacts')}, indent=2))
