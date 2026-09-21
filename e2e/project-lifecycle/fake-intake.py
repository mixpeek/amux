#!/usr/bin/env python3
"""Deterministic provider boundary, never used by a production configuration."""
import json, os, sys
from pathlib import Path
prompt=sys.stdin.read()
records=[line for line in prompt.splitlines() if line.startswith('{"') and '"command"' in line]
data=json.loads(records[-1]);text=data['command'].lower()
with open(Path(os.environ['AMUX_HOME'])/'fixture-calls.jsonl','a') as out:
    out.write(json.dumps({'phase':'intake','command':data['command'],'model':sys.argv[sys.argv.index('--model')+1] if '--model' in sys.argv else None})+'\n')
if 'provider quota' in text:
    # Captured shape of the real weekly-limit response; no billable call.
    print(json.dumps({'metadata':'x'*1000,'is_error':True,'result':"You've hit your weekly limit · resets Sep 23 at 11am (America/New_York)"}))
    sys.exit(1)
if 'malformed' in text:
    result='not valid JSON'
else:
    names=['alpha','beta'] if 'parallel' in text else ['alpha']
    if 'restart' in text: names=['restart']
    if 'pause' in text: names=['pause']
    if 'repair' in text: names=['repair']
    if 'budget' in text: names=['budget']
    if 'dirty' in text: names=['dirty']
    tasks=[]
    for i,name in enumerate(names):
        existing=next((c for c in data['candidates'] if c['title']==f'Create {name} report'),None)
        tasks.append({'key':chr(97+i),'title':f'Create {name} report','description':f'Write the requested {name} report in {name}.txt','type':'doc','action':('verify' if existing and existing['status']=='verified' else 'update') if existing else 'create','existing_id':existing['id'] if existing else None,'next_action':f'Write {name}.txt and verify its contents','acceptance_criteria':[f'{name}.txt contains exactly {name}'],'needs':[],'dependency_reason':''})
    result=json.dumps({'kind':'tasks','reason':'finite fixture outputs','confidence':0.99,'tasks':tasks})
print(json.dumps({'type':'result','result':result,'usage':{'input_tokens':40,'output_tokens':80}}))
