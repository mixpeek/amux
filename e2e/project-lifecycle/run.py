#!/usr/bin/env python3
"""Start a NEW isolated server, run all UI scenarios, stop only test processes."""
import argparse, json, os, signal, ssl, subprocess, tempfile, time, urllib.request
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--binary',required=True);p.add_argument('--out',default='test-results/project-lifecycle');p.add_argument('--port',type=int,default=18964);a=p.parse_args()
root=Path(__file__).resolve().parents[2];out=Path(a.out).resolve();out.mkdir(parents=True,exist_ok=True)
home=Path(tempfile.mkdtemp(prefix='amux-project-',dir='/tmp')).resolve();binary=str(Path(a.binary).resolve())
log=open(out/'server.log','w');server=subprocess.Popen(['python3',str(Path(__file__).with_name('serve.py')),'--binary',binary,'--home',str(home),'--port',str(a.port)],cwd=root,stdout=log,stderr=subprocess.STDOUT)
try:
    for _ in range(120):
        if server.poll() is not None:raise RuntimeError('isolated server exited: '+str(server.returncode))
        try:
            with urllib.request.urlopen(f'https://localhost:{a.port}/health',context=ssl._create_unverified_context(),timeout=1) as r:health=json.load(r)
            if health['pid']==server.pid:break
        except Exception:pass
        time.sleep(.25)
    else:raise RuntimeError('isolated server did not become healthy')
    (out/'health-start.json').write_text(json.dumps(health,indent=2))
    result=subprocess.Popen(['node',str(Path(__file__).with_name('ui.mjs')),str(home/'fixture.json'),str(out)],cwd=root)
    while result.poll() is None:
        server.poll()  # Reap the initial process while UI exercises server restart.
        time.sleep(.25)
    try:
        with urllib.request.urlopen(f'https://localhost:{a.port}/health',context=ssl._create_unverified_context(),timeout=2) as r:final_health=json.load(r)
        (out/'health-end.json').write_text(json.dumps(final_health,indent=2))
    except Exception as e:(out/'health-end.json').write_text(json.dumps({'error':str(e)}))
    raise SystemExit(result.returncode)
finally:
    # ui.mjs deliberately restarts the server; use its current manifest, not the original PID.
    manifest=home/'fixture.json'
    if manifest.exists():
        current=json.loads(manifest.read_text());pid=current['pid']
        command=subprocess.run(['ps','-p',str(pid),'-o','command='],capture_output=True,text=True).stdout.strip()
        if command==binary:
            os.kill(pid,signal.SIGTERM)
    tmux=home/'bin'/'tmux'
    if tmux.exists():subprocess.run([str(tmux),'kill-server'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    try:server.wait(timeout=5)
    except subprocess.TimeoutExpired:pass
    log.close()
    print(json.dumps({'fixture':str(home),'artifacts':str(out),'cleanup':'test server and its isolated tmux socket stopped; repositories retained'}),flush=True)
