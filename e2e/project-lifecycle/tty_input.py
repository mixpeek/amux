"""Small TUI input contract: noncanonical input, bracketed paste, explicit Enter."""
import os
import termios
from contextlib import contextmanager

@contextmanager
def terminal_input(fd=0):
    original = termios.tcgetattr(fd)
    mode = termios.tcgetattr(fd)
    mode[3] &= ~(termios.ICANON | termios.ECHO)
    mode[0] &= ~termios.ICRNL
    mode[6][termios.VMIN] = 1
    mode[6][termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, mode)
    os.write(1, b'\x1b[?2004h')
    try:
        yield submissions(fd)
    finally:
        os.write(1, b'\x1b[?2004l')
        termios.tcsetattr(fd, termios.TCSANOW, original)

def submissions(fd):
    text = bytearray()
    escape = bytearray()
    pasted = False
    while True:
        ch = os.read(fd, 1)
        if not ch:
            return
        if escape or ch == b'\x1b':
            escape.extend(ch)
            if bytes(escape) in (b'\x1b[200~', b'\x1b[201~'):
                pasted = bytes(escape) == b'\x1b[200~'
                escape.clear()
            elif not any(seq.startswith(escape) for seq in (b'\x1b[200~', b'\x1b[201~')):
                # The fixture rejects unsupported editing escapes, never inserts them.
                escape.clear()
            continue
        if not pasted and ch in (b'\r', b'\n'):
            yield text.decode('utf-8')
            text.clear()
        elif not pasted and ch == b'\x15':
            text.clear()
        elif not pasted and ch in (b'\x7f', b'\x08'):
            if text:
                start = len(text)-1
                while start > 0 and text[start] & 0xc0 == 0x80:
                    start -= 1
                del text[start:]
        else:
            text.extend(ch)
