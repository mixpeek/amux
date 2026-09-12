"""Per-job isolation switches for a real browser-reaper-only scratch server."""
import re
from pathlib import Path

def disabled_variables():
    source = (Path(__file__).resolve().parents[2] / 'crates/amux-server/src/runtime_jobs/registry.rs').read_text()
    block = source.split('pub mod ids {', 1)[1].split('\n}', 1)[0]
    ids = re.findall(r'pub const \w+: &str = "([a-z_-]+)";', block)
    assert 'browser-idle-reaper' in ids and 'mac-health' in ids and len(ids) > 20
    return ['AMUX_' + name.upper().replace('-', '_') + '_SECS' for name in ids if name != 'browser-idle-reaper']

if __name__ == '__main__':
    print('\n'.join(disabled_variables()))
