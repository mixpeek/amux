#!/usr/bin/env python3
"""Interactive fake CLI. Real tmux, HTTP claims, files, Git and gates remain real."""
import json, os, re, ssl, subprocess, sys, time, urllib.request
from tty_input import terminal_input
from pathlib import Path
home=Path(os.environ['AMUX_HOME']);base=os.environ['AMUX_URL'].rstrip('/')
worker=os.environ['AMUX_SESSION']
# Refuse before files/Git/HTTP if launcher recovery lost the isolated workspace.
if not Path.cwd().resolve().is_relative_to((home/'worktrees').resolve()):
    raise SystemExit('fixture refused provider cwd outside isolated worktrees: '+str(Path.cwd()))
def post(path,body):
    req=urllib.request.Request(base+path,data=json.dumps(body).encode(),headers={'Content-Type':'application/json','X-Amux-Session':worker})
    with urllib.request.urlopen(req,context=ssl._create_unverified_context(),timeout=10) as r: return json.load(r)
def status(state,source):
    try: post('/api/sessions/'+worker+'/report',{'state':state,'source':source})
    except Exception as e: print('fixture status:',str(e),flush=True)
def idle(): print('\x1b[2J\x1b[HClaude Code fixture\n────────────────────\n❯ \n────────────────────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)',flush=True)
with terminal_input() as packets:
    idle()
    status('done','stop-hook')
    header=''
    for packet in packets:
        if packet.strip()=='/exit': break
        with open(home/'fixture-input.jsonl','a') as receipt:
            receipt.write(json.dumps({'worker':worker,'packet':packet,'bytes':len(packet.encode())})+'\n')
        for raw in packet.splitlines():
            line=re.sub(r'\x1b\[[0-9;]*[A-Za-z~]','',raw).strip()
            if line=='/exit': break
            if line.startswith('Execute this finite'): header=line
            try: task=json.loads(line)
            except Exception: continue
            if not isinstance(task,dict) or not all(k in task for k in ['worker','criteria','id','project']):continue
            match=re.search(r'"generation":(\d+),"input_hash":"([a-f0-9]+)"',header)
            if not match:print('Missing claim header',flush=True);continue
            generation=int(match[1]);digest=match[2]
            print('\x1b[2J\x1b[H'+task['title']+'\n✶ Thinking… (1s · 1 token · esc to interrupt)\n\n  ⏵⏵ bypass permissions on · esc to interrupt',flush=True)
            status('working','prompt-hook')
            with open(home/'fixture-calls.jsonl','a') as out:out.write(json.dumps({'phase':'execution','worker':worker,'task':task['id'],'attempt':task['attempt']})+'\n')
            name=task['criteria'][0].split('.')[0]
            while (home/('hold-'+name)).exists():
                (home/('heartbeat-'+name)).write_text(str(time.time()));time.sleep(.2)
            content='wrong' if name=='repair' and task['attempt']==1 else name
            Path(name+'.txt').write_text(content+'\n')
            subprocess.run(['git','add',name+'.txt'],check=True)
            subprocess.run(['git','commit','-m','Create '+name+' report'],check=False,stdout=subprocess.DEVNULL)
            if name=='dirty': Path('uncommitted-evidence.txt').write_text('Preserve this work\n')
            head=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()
            report={'head':head,'summary':'Created '+name+' report','checks':[{'criterion':task['criteria'][0],'command':f'test "$(cat {name}.txt)" = "{name}"'}]}
            try:print('Result',post('/api/projects/'+task['project']+'/tasks/'+task['id']+'/report',{'generation':generation,'input_hash':digest,'report':report}),flush=True)
            except Exception as e:print('Report failed:',str(e),flush=True)
            idle();status('done','stop-hook')
