#!/usr/bin/env bash
# Snapshot source BEFORE a test; publish byte coverage only if it is unchanged
# afterwards. Hashing just the final tree certified edits no compiler saw.
# Usage: write-test-receipt.sh --snapshot <path>
#        AMUX_TEST_SOURCE_SNAPSHOT=<path> write-test-receipt.sh <rc> <args...>
# Always preserve the caller's test verdict; missing evidence is unmeasured.
python3 - "$@" <<'PY'
import hashlib, json, os, pathlib, subprocess, sys, tempfile, time

def git(*args):
    return subprocess.check_output(['git', *args], stderr=subprocess.DEVNULL)

def snapshot():
    root = git('rev-parse', '--show-toplevel').decode().strip()
    algorithm = git('rev-parse', '--show-object-format').decode().strip()
    paths = sorted(set(git('ls-files', '-co', '-z', '--exclude-standard').decode().split('\0')) - {''})
    blobs = {}
    for name in paths:
        if '\t' in name or '\n' in name:
            raise ValueError('source path cannot be represented in receipt TSV')
        path = pathlib.Path(root, name)
        if not path.is_file():
            blobs[name] = 'missing'
            continue
        data = os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
        blobs[name] = hashlib.new(algorithm, b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest()
    return {'repo':root, 'head':git('rev-parse', 'HEAD').decode().strip(), 'blobs':blobs}

try:
    if len(sys.argv) == 3 and sys.argv[1] == '--snapshot':
        pathlib.Path(sys.argv[2]).write_text(json.dumps(snapshot(), sort_keys=True))
    else:
        rc = sys.argv[1] if len(sys.argv) > 1 else '0'
        try:
            before = json.loads(pathlib.Path(os.environ['AMUX_TEST_SOURCE_SNAPSHOT']).read_text())
            after = snapshot()
            stable = before['repo'] == after['repo'] and before['blobs'] == after['blobs']
            state = 'stable_before_after' if stable else 'changed'
        except Exception:
            before = {'repo':git('rev-parse', '--show-toplevel').decode().strip(), 'head':'unknown', 'blobs':{}}
            stable, state = False, 'unmeasured'
        folder = pathlib.Path(os.environ.get('AMUX_HOME', str(pathlib.Path.home()/'.amux')), 'test-receipts')
        folder.mkdir(parents=True, exist_ok=True)
        receipt = folder/(os.environ.get('AMUX_SESSION', 'unknown') + '.tsv')
        with tempfile.NamedTemporaryFile(mode='w', dir=folder, prefix='.receipt-', delete=False) as f:
            temp = f.name
            for key,value in [('repo',before['repo']),('head',before['head']),('rc',rc),('at',int(time.time())),('args',' '.join(sys.argv[2:])),('source_state',state)]:
                f.write(f'# {key}\t{value}\n')
            if stable:
                for path, sha in before['blobs'].items():
                    f.write(f'{sha}\t{path}\n')
            f.flush(); os.fsync(f.fileno())
        os.replace(temp, receipt)
        if not stable:
            print(f'test receipt: source {state}; byte coverage withheld (test exit {rc} retained)', file=sys.stderr)
except Exception as error:
    print(f'test receipt: unmeasured; cannot capture source: {error}', file=sys.stderr)
# Receipt instrumentation never converts a green/red test result into another.
PY
exit 0
