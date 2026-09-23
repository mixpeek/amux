#!/usr/bin/env python3
"""Real PTY regression: >4KB single line + multiline Unicode, no model/HTTP."""
import json, os, pty, select, subprocess, sys, tempfile, termios, shutil
from pathlib import Path
here = Path(__file__).resolve().parent
packet = 'Execute ' + 'α—' * 2500 + '\nTask packet:\n' + json.dumps({'id':'A','description':'β\nnext'},ensure_ascii=False)
with tempfile.TemporaryDirectory() as tmp:
    output = Path(tmp)/'received.json'
    master, slave = pty.openpty()
    program = "from tty_input import terminal_input; import json,sys; from pathlib import Path\nwith terminal_input() as packets:\n for packet in packets:\n  Path(sys.argv[1]).write_text(json.dumps(packet)); break\n"
    proc = subprocess.Popen([sys.executable,'-c',program,str(output)],cwd=here,stdin=slave,stdout=slave,stderr=slave)
    os.close(slave)
    try:
        assert select.select([master],[],[],5)[0], 'provider did not initialize'
        assert b'\x1b[?2004h' in os.read(master,4096)
        # Clear stale input like the real sender, paste intact, then explicit Enter.
        payload=b'stale draft\x15\x1b[200~'+packet.encode()+b'\x1b[201~\r'
        while payload:
            n=os.write(master,payload[:512]);payload=payload[n:]
        assert proc.wait(timeout=5)==0
        assert json.loads(output.read_text())==packet
        print('PTY exact receipt PASS:',len(packet.encode()),'bytes; multiline Unicode; stale draft cleared')
    finally:
        if proc.poll() is None: proc.kill();proc.wait()
        os.close(master)

# Negative control retains the old canonical contract and demonstrates truncation.
with tempfile.TemporaryDirectory() as tmp:
    output=Path(tmp)/'canonical.txt'
    master,slave=pty.openpty()
    mode=termios.tcgetattr(slave);mode[3] &= ~termios.ECHO;termios.tcsetattr(slave,termios.TCSANOW,mode)
    proc=subprocess.Popen([sys.executable,'-c',"import sys;from pathlib import Path;Path(sys.argv[1]).write_text(sys.stdin.readline())",str(output)],stdin=slave,stdout=slave,stderr=slave)
    os.close(slave)
    try:
        control=b'Execute '+b'x'*12000+b'\n'
        pending=control
        while pending:
            n=os.write(master,pending[:512]);pending=pending[n:]
        try:
            assert proc.wait(timeout=2)==0
        except subprocess.TimeoutExpired:
            print('Canonical negative control reproduced: oversized line did not submit within 2s')
        else:
            received=output.read_bytes()
            assert received!=control and len(received)<len(control)
            print('Canonical negative control reproduced:',len(received),'of',len(control),'bytes received')
    finally:
        if proc.poll() is None:proc.kill();proc.wait()
        os.close(master)

# Installed-helper and UTF-8 editing proof, isolated from the repository import path.
def edited_receipt(keys, expected, broken=False):
    with tempfile.TemporaryDirectory() as tmp:
        installed=Path(tmp)/'tty_input.py'
        shutil.copy2(here/'tty_input.py',installed)
        if broken:
            source=installed.read_text()
            good="""                start = len(text)-1
                while start > 0 and text[start] & 0xc0 == 0x80:
                    start -= 1
                del text[start:]"""
            bad="""                text.pop()
                while text and text[-1] & 0xc0 == 0x80:
                    text.pop()"""
            assert good in source
            installed.write_text(source.replace(good,bad))
        output=Path(tmp)/'received.json'
        master,slave=pty.openpty()
        env=dict(os.environ);env.pop('PYTHONPATH',None)
        proc=subprocess.Popen([sys.executable,'-c',program,str(output)],cwd=tmp,env=env,stdin=slave,stdout=slave,stderr=slave)
        os.close(slave)
        try:
            assert select.select([master],[],[],5)[0]
            assert b'\x1b[?2004h' in os.read(master,4096)
            os.write(master,keys+b'\r')
            code=proc.wait(timeout=5)
            if broken:
                assert code!=0 and not output.exists(), 'negative control must reject dangling UTF-8'
                print('UTF-8 legacy Backspace negative control: expected decode failure')
            else:
                assert code==0 and json.loads(output.read_text())==expected
        finally:
            if proc.poll() is None:proc.kill();proc.wait()
            os.close(master)

edited_receipt('keepα'.encode()+b'\x7f','keep')
edited_receipt('keep🧪'.encode()+b'\x08','keep')
edited_receipt(b'\x7f'+ 'α🧪'.encode()+b'\x7f\x7f'+'β'.encode(),'β')
edited_receipt(b'\x1b[200~'+ 'α'.encode()+b'\x7f\x1b[201~','α\x7f')
edited_receipt('α'.encode()+b'\x7f','',broken=True)
print('Installed helper UTF-8 Backspace: PASS (2/4-byte characters, empty input, repeated editing, paste literal control)')
