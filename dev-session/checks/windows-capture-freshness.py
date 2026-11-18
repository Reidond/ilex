"""Real native capture probe. Run while the guest motion fixture is visible.

QMP stop freezes the owned guest for the negative PrintWindow check. The
finally block resumes it even on failure. No posted host window messages.
"""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--info', type=Path, required=True)
p.add_argument('--vmctl', type=Path, required=True)
p.add_argument('--qmp-client', type=Path, required=True)
p.add_argument('--out', type=Path, required=True)
a = p.parse_args()
i = json.loads(a.info.read_text())
assert i['schema'] == 1 and i['qmp_endpoint'].startswith('unix:')
a.out.mkdir(parents=True, exist_ok=True)
rows = []


def execute(argv, name, env=None):
    started = time.monotonic()
    result = subprocess.run([str(x) for x in argv], capture_output=True, timeout=15, env=env)
    (a.out / (name + '.stdout.log')).write_bytes(result.stdout)
    (a.out / (name + '.stderr.log')).write_bytes(result.stderr)
    row = {'utc': datetime.now(timezone.utc).isoformat(), 'name': name,
           'exit': result.returncode, 'seconds': time.monotonic() - started,
           'stderr': result.stderr.decode(errors='replace')}
    rows.append(row)
    return result, row


def screenshot(name, backend):
    env = dict(os.environ)
    env.pop('ILEX_CAPTURE_BACKEND', None)
    if backend == 'printwindow':
        env['ILEX_CAPTURE_BACKEND'] = backend
    path = a.out / (name + '.png')
    assert not path.exists(), 'Use a new artifact directory'
    result, row = execute([a.vmctl, '--vm', i['purpose'], 'screenshot', '-o', path], name, env)
    row['output_exists'] = path.exists()
    if result.returncode == 0:
        row['frame'] = json.loads(path.with_suffix('.frame.json').read_text())
    return result, row


try:
    moving, normal = screenshot('moving-wgc', 'wgc')
    assert moving.returncode == 0 and normal['frame']['source'] == 'wgc'
    fallback, proof = screenshot('moving-printwindow', 'printwindow')
    if fallback.returncode == 0:
        frame = proof['frame']
        assert frame['source'] == 'printwindow-content-change'
        assert frame['source_ticks'] is None
        assert frame['freshness']['kind'] == 'content-change'
        assert frame['freshness']['baseline_sha256'] != frame['freshness']['frame_sha256']
    else:
        assert not proof['output_exists'] and 'no fresh frame' in proof['stderr']
    stopped, _ = execute([a.qmp_client, '--connect', i['qmp_endpoint'], 'stop'], 'guest-stop')
    assert stopped.returncode == 0
    time.sleep(1)
    for n in range(2):
        result, row = screenshot(f'paused-printwindow-{n}', 'printwindow')
        assert result.returncode != 0 and not row['output_exists']
        assert 'no fresh frame' in row['stderr'] and row['seconds'] < 5
finally:
    resumed, _ = execute([a.qmp_client, '--connect', i['qmp_endpoint'], 'cont'], 'guest-resume')
    (a.out / 'result.json').write_text(json.dumps({'schema': 1, 'run': i['run'], 'results': rows}, indent=2))
    assert resumed.returncode == 0
print('WGC acquired; PrintWindow requires changed content and refuses the paused guest')
