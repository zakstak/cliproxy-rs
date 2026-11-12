import json, subprocess, sys
from pathlib import Path
root=Path('/home/agent/git/cliproxy-rs')
fresh=sorted((root/'harness/.cache').glob('accept-*'))[-1]
cargo=json.loads((fresh/'cargo-groups.json').read_text())
artifact=cargo['executions'][0]['artifact']['executable']
names=('differential-historical','mutation-historical','differential-selective','mutation-selective','cargo-groups')
paths=[str(fresh/(n+'.json')) for n in names]
base=[sys.executable,str(root/'harness/acceptance.py'),'combine',*paths]
results=[]
for label, mode, verdict, code in (('unreadable',0,'unavailable',2),('changed-bytes',0o444,'fail',1),('positive',None,'pass',0)):
    out=Path('/review')/('cli-'+label);out.mkdir()
    receipt=out/'acceptance.json'
    command=base+['--receipt',str(receipt)]
    if mode is not None:
        fake=out/'artifact';fake.write_bytes(b'synthetic scratch view; original artifact unchanged\n');fake.chmod(mode)
        command=['bwrap','--unshare-user','--die-with-parent','--cap-drop','ALL','--ro-bind','/','/','--dev','/dev','--proc','/proc','--bind','/review','/review','--ro-bind',str(fake),str(root/artifact),'--chdir',str(root),*command]
    done=subprocess.run(command,capture_output=True,text=True)
    (out/'stdout').write_text(done.stdout);(out/'stderr').write_text(done.stderr)
    assert done.returncode==code,(label,done.returncode,done.stderr)
    result=json.loads(receipt.read_text())
    assert result['verdict']==verdict,(label,result)
    assert 'Traceback' not in done.stderr
    if label=='unreadable':
        assert not result['problems'] and not result['current_cargo']['queried']
        assert any(artifact in p and 'Permission denied' in p for p in result['unavailable'])
    elif label=='changed-bytes':
        assert not result['current_cargo']['queried'] and not result['unavailable']
        assert any('recorded SHA-256' in p for p in result['problems'])
    else:
        assert result['current_cargo']['available'] and result['current_cargo']['queried']
    results.append({'label':label,'command':command,'exit':done.returncode,'receipt_written':True,'verdict':verdict,'artifact':artifact,'traceback':False})
Path('/review/real-cli-results.json').write_text(json.dumps(results,indent=2)+'\n')
print(json.dumps(results,indent=2))
