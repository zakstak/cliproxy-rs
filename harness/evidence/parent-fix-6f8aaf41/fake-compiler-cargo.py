"""Real Cargo, fake final compiler. No Rust compilation is permitted.
Run only through heavy.sh after the current receipt probes finish.
"""
import json, os, subprocess
from pathlib import Path
R=Path('/home/agent/git/cliproxy-rs'); O=R/'harness/.cache/fake-compiler-cargo'; O.mkdir()
real=subprocess.check_output(['rustup','which','rustc'],text=True).strip()
fake=O/'final-rustc-guard.py'
fake.write_text('''#!/usr/bin/python3
import json,os,sys
from pathlib import Path
a=sys.argv[1:]
if a==['-vV'] or a==['--version'] or (len(a)>3 and a[:3]==['-','--crate-name','___'] and any(x.startswith('--print=') for x in a) and not any(x.startswith('--emit') or x=='-o' for x in a)):
    os.execv(os.environ['PROOF_REAL_RUSTC'],[os.environ['PROOF_REAL_RUSTC'],*a])
with open(os.environ['PROOF_COMPILE_ATTEMPTS'],'a') as f: f.write(json.dumps(a)+'\\n')
print('final fake compiler refused compilation',file=sys.stderr)
sys.exit(101)
'''); fake.chmod(0o755)
results=[]
for name,flags in [('ordinary',''),('cfg-sentinel','--cfg ___')]:
    d=O/name; (d/'src').mkdir(parents=True)
    (d/'Cargo.toml').write_text('[package]\nname="wrapper-proof"\nversion="0.0.0"\nedition="2024"\n[workspace]\n')
    (d/'Cargo.lock').write_text('version = 4\n[[package]]\nname = "wrapper-proof"\nversion = "0.0.0"\n')
    (d/'src/lib.rs').write_text('pub fn proof() {}\n')
    attempts=d/'fake-compiler-attempts.jsonl'
    env=dict(os.environ,RUSTC=str(fake),RUSTC_WRAPPER=str(R/'harness/no-compile-rustc'),RUSTFLAGS=flags,
             PROOF_REAL_RUSTC=real,PROOF_COMPILE_ATTEMPTS=str(attempts),CPA_NO_COMPILE_LOG=str(d/'refused.log'))
    cmd=['cargo','test','--locked','--no-run','--offline','--message-format=json']
    done=subprocess.run(cmd,cwd=d,env=env,capture_output=True,text=True)
    (d/'stdout').write_text(done.stdout); (d/'stderr').write_text(done.stderr)
    calls=[json.loads(x) for x in attempts.read_text().splitlines()] if attempts.exists() else []
    rec={'name':name,'command':cmd,'RUSTFLAGS':flags,'exit':done.returncode,'real_compilation':False,
         'compile_commands_forwarded_to_final_fake_guard':calls,'wrapper_refusals':(d/'refused.log').read_text() if (d/'refused.log').exists() else ''}
    results.append(rec)
    assert done.returncode!=0
    assert not calls and rec['wrapper_refusals']
Path('/review/fake-compiler-cargo.json').write_text(json.dumps(results,indent=2)+'\n')
print(json.dumps(results,indent=2))
