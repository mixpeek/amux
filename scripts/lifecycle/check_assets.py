#!/usr/bin/env python3
"""Refuse to certify a browser run against another checkout's embedded assets."""
import hashlib
import json
import os
from pathlib import Path
import ssl
import time
import urllib.error
import urllib.request


def check(base, expected, timeout=60):
    deadline = time.monotonic() + timeout
    context = ssl._create_unverified_context()
    while True:
        try:
            with urllib.request.urlopen(base + '/health', context=context, timeout=2) as response:
                health = json.load(response)
            break
        except (OSError, urllib.error.URLError):
            if time.monotonic() >= deadline:
                raise RuntimeError('lifecycle server did not become ready for asset verification')
            time.sleep(.25)
    actual = {}
    for route in expected:
        with urllib.request.urlopen(base + route, context=context, timeout=10) as response:
            actual[route] = hashlib.sha256(response.read()).hexdigest()
    mismatches = [route for route in expected if expected[route] != actual[route]]
    return {'health': health, 'expected': expected, 'actual': actual,
            'mismatches': mismatches, 'verdict': 'asset_mismatch' if mismatches else 'assets_match'}


if __name__ == '__main__':
    port = os.environ['AMUX_RS_PORT']
    expected = json.loads(Path(os.environ['AMUX_LIFECYCLE_ASSET_MANIFEST']).read_text())
    result = check('https://localhost:' + port, expected)
    output = Path(os.environ['AMUX_LIFECYCLE_OUTPUT']) / ('asset-provenance-' + port + '.json')
    output.write_text(json.dumps(result, indent=2) + '\n')
    print('[lifecycle] ' + result['verdict'] + ': ' + str(output), flush=True)
    if result['mismatches']:
        raise SystemExit('Browser acceptance refused: server assets differ from the source under test: ' + ', '.join(result['mismatches']))
