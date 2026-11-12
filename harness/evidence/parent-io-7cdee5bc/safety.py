import errno, json, os, socket, subprocess
from pathlib import Path
assert os.getuid()==os.geteuid()==1000 and os.nice(0)==19
caps={x.split(':')[0]:x.split()[1] for x in Path('/proc/self/status').read_text().splitlines() if x.startswith('Cap')}
assert all(int(x,16)==0 for x in caps.values())
assert 'CARGO_INCREMENTAL' not in os.environ and os.environ['CARGO_BUILD_JOBS']=='2'
assert os.environ['RUSTC_WRAPPER']=='/review/outer-rustc-guard.sh'
assert set(os.listdir('/sys/class/net'))=={'lo'} if Path('/sys/class/net').exists() else True
assert os.environ['HOME']=='/tmp/review-home' and not list(Path('/tmp/review-home/.cli-proxy-api').iterdir())
for p in ['/home/agent/.ssh','/home/agent/.claude','/home/agent/.config','/home/agent/.cli-proxy-api','/run/user','/tmp/h1-rereview-60928bbf','/home/agent/.cache/cliproxy-next-issues']:
    assert not Path(p).exists(), p
s=socket.socket(); assert s.connect_ex(('192.0.2.1',443))==errno.ENETUNREACH
for p in ['AGENTS.md','.git/index','harness/acceptance.py',*Path('/review/artifact-paths.txt').read_text().splitlines()]:
    try: f=open(p,'r+b')
    except OSError as e: assert e.errno==errno.EROFS,(p,e)
    else: f.close(); raise AssertionError('writable '+p)
tools={c[0]:subprocess.check_output(c,text=True).splitlines()[0] for c in [['rustc','--version'],['cargo','--version'],['go','version'],['python3','--version'],['cmake','--version']]}
for p,w in [('harness/.cache/CLIProxyAPI','6fecc6e5567912661654a4eaf9b8f5436facd1c2'),('harness/.cache/CLIProxyAPI-8bba7a77','8bba7a77c6bcbfd993516d26606bfecaa4ff49d0')]:
    assert subprocess.check_output(['git','-C',p,'rev-parse','HEAD'],text=True).strip()==w
    assert not subprocess.check_output(['git','-C',p,'status','--porcelain'],text=True).strip()
print(json.dumps({'uid':os.getuid(),'capabilities':caps,'egress':'ENETUNREACH','tools':tools,'source_index_12_artifacts':'readonly','auth':'private empty'},indent=2))
