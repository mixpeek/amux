#!/usr/bin/env python3
"""New server, DB, local Git remote, tmux socket and nonbillable provider fakes.
Pass a directory explicitly to reuse it for restart recovery testing.
"""
import argparse,hashlib,json,os,re,shutil,subprocess,sys,tempfile
from pathlib import Path
parser=argparse.ArgumentParser();parser.add_argument('--binary',required=True);parser.add_argument('--home');parser.add_argument('--port',type=int,default=18964);parser.add_argument('--live-codex',action='store_true',help='Use installed Codex with existing account auth; never substitute fake providers');a=parser.parse_args()
root=Path(__file__).resolve().parents[2]
home=Path(a.home or tempfile.mkdtemp(prefix='amux-project-',dir='/tmp')).resolve();home.mkdir(parents=True,exist_ok=True)
repo=home/'repository';remote=home/'remote.git';bin_dir=home/'bin';bin_dir.mkdir(exist_ok=True)
def git(*args):subprocess.run(['git',*args],check=True,stdout=subprocess.DEVNULL)
if not repo.exists():
    git('init','--bare','--initial-branch=main',str(remote));git('clone',str(remote),str(repo))
    git('-C',str(repo),'config','user.email','fixture@example.invalid');git('-C',str(repo),'config','user.name','Project fixture')
    (repo/'README.md').write_text('Isolated project lifecycle fixture. No production remote.\n')
    git('-C',str(repo),'add','.');git('-C',str(repo),'commit','-m','Fixture base');git('-C',str(repo),'push','origin','main')
for src,name in ([] if a.live_codex else [('fake-intake.py','fake-intake'),('fake-claude.py','claude'),('tty_input.py','tty_input.py')]):
    shutil.copy2(Path(__file__).with_name(src),bin_dir/name);(bin_dir/name).chmod(0o755)
# tmux isolation does not depend on a shell preserving TMPDIR.
tmux=shutil.which('tmux');assert tmux,'tmux is required for real provider transport'
sock=home/('tmux-'+str(os.getuid()))/'default';sock.parent.mkdir(mode=0o700,exist_ok=True);(bin_dir/'tmux').write_text('#!/bin/sh\nexec '+tmux+' -S '+str(sock)+' "$@"\n');(bin_dir/'tmux').chmod(0o755)
if not a.live_codex: (home/'server.env').write_text('ANTHROPIC_API_KEY=fixture-not-a-credential\n')
env={k:v for k,v in os.environ.items() if not k.startswith('AMUX_') and k not in ('TMUX','CLAUDECODE','CLAUDE_CODE_ENTRYPOINT')}
# Disable unrelated maintenance. The actual board driver and steering pump run.
registry=(root/'crates/amux-server/src/runtime_jobs/registry.rs').read_text()
for name in re.findall(r'pub const \w+: &str = "([a-z_-]+)"',registry):env['AMUX_'+name.upper().replace('-','_')+'_SECS']='0'
for file in (root/'crates/amux-server/src/runtime_jobs').glob('*.rs'):
    for name in re.findall(r'(?:const JOB: &str = |spawn_periodic\()"([a-z_-]+)"',file.read_text()):env['AMUX_'+name.upper().replace('-','_')+'_SECS']='0'
env.update(TMUX_TMPDIR=str(home),AMUX_HOME=str(home),AMUX_RS_PORT=str(a.port),AMUX_URL=f'https://localhost:{a.port}',AMUX_NO_SELF_ADOPT='1',AMUX_BACKEND='tmux',AMUX_BOARD_DRIVE_SECS='1',AMUX_PROJECT_EXECUTION_SECS='1',AMUX_SCHEDULER_SECS='1',AMUX_STEER_DELIVER_SECS='1',AMUX_CLAUDE_CMD=str(bin_dir/'claude'),AMUX_HELPER_CLI=str(bin_dir/'fake-intake'),AMUX_WARM_HELPER='0',AMUX_ALLOW_TMUX_SPAWN_FROM_TEST_HOME='1',PATH=str(bin_dir)+':'+env['PATH'])
if a.live_codex:
    assert shutil.which('codex'), 'Installed Codex CLI is required'
    env.pop('AMUX_CLAUDE_CMD',None);env.pop('AMUX_HELPER_CLI',None)
    env.update(AMUX_HELPER_PROVIDER='codex', AMUX_HELPER_MODEL='gpt-6-luna', AMUX_CODEX_HELPER_CLI=shutil.which('codex'))
    # Keep normal status/capture behavior for real CLI delivery checks.
    env.update(AMUX_MESSAGE_CAPTURE_SECS='5', AMUX_RS_TICK_SECS='3', AMUX_RS_SCAN_SECS='5')
    subprocess.run([str(root/'scripts/install-cli.sh'),str(bin_dir)],env=env,check=True)
# Fixture executors inherit the explicit test home and URL through the launcher.
manifest={'pid':os.getpid(),'home':str(home),'repo':str(repo),'remote':str(remote),'url':env['AMUX_URL'],'socket':str(sock),'binary':str(Path(a.binary).resolve()),'binary_sha256':hashlib.sha256(Path(a.binary).read_bytes()).hexdigest(),'provider':'real Codex; existing account auth' if a.live_codex else 'fake CLI; real tmux and Git'}
(home/'fixture.json').write_text(json.dumps(manifest,indent=2));print(json.dumps(manifest),flush=True)
os.execve(str(Path(a.binary).resolve()),[str(Path(a.binary).resolve())],env)
