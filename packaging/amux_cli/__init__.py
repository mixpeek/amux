"""PyPI entry point for amux.

The real product is the adjacent single-file server (amux-server.py) plus the
bash CLI (amux) — both shipped verbatim inside this package. This shim only
exec's the bash CLI, which resolves amux-server.py next to its own real path
(so `amux serve` finds the server inside site-packages with no configuration).
"""
import os
import sys

__version__ = "0.9.108"


def main() -> None:
    here = os.path.dirname(os.path.abspath(__file__))
    cli = os.path.join(here, "amux")
    if not os.path.exists(cli):
        sys.stderr.write("amux: bundled CLI missing — reinstall with: pipx reinstall amux\n")
        sys.exit(1)
    # exec (not subprocess) keeps the tty, signals, and exit code intact —
    # required for the interactive dashboard and `amux attach`.
    os.execv("/bin/bash", ["/bin/bash", cli] + sys.argv[1:])
