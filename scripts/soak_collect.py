"""Collect sanitized minute checkpoints on the soak host for one bounded soak.

Usage: python3 soak_collect.py <soak-anchor.json> <soak_checkpoint.py>
Writes minute-checkpoints.jsonl beside the anchor and stops at the end of the
72-hour window or on the first continuity failure. It never grants acceptance.
"""
import datetime as dt
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def continuity(anchor, checkpoint):
    for key in ('image', 'revision', 'container_started', 'runtime_started'):
        if checkpoint.get(key) != anchor[key]:
            return key
    elapsed = checkpoint['monotonic_seconds'] - anchor['monotonic_seconds']
    if elapsed < 0 or abs(elapsed - checkpoint['soak_elapsed_wall_seconds']) > 2:
        return 'clock-continuity'
    return None


def main():
    os.umask(0o077)
    path = Path(sys.argv[1])
    anchor = json.loads(path.read_text())
    helper = sys.argv[2]
    output = path.with_name('minute-checkpoints.jsonl')
    with output.open('x') as handle:
        while True:
            cycle = time.monotonic()
            record = {'collected_at': dt.datetime.now(dt.timezone.utc).isoformat()}
            try:
                result = subprocess.run([sys.executable, helper, anchor['observation_anchor_utc']], capture_output=True, text=True, timeout=55)
                if result.returncode:
                    record.update(kind='probe-error', exit_code=result.returncode)
                else:
                    checkpoint = json.loads(result.stdout)
                    mismatch = continuity(anchor, checkpoint)
                    record.update(kind='checkpoint', checkpoint=checkpoint)
                    if mismatch:
                        record.update(kind='continuity-failure', mismatch=mismatch)
                    elif checkpoint['monotonic_seconds'] - anchor['monotonic_seconds'] >= 259200:
                        record['kind'] = 'window-complete-awaiting-review'
            except (subprocess.TimeoutExpired, ValueError, KeyError) as error:
                record.update(kind='probe-error', error_type=type(error).__name__)
            handle.write(json.dumps(record) + '\n')
            handle.flush()
            os.fsync(handle.fileno())
            if record['kind'] in ('continuity-failure', 'window-complete-awaiting-review'):
                break
            # Stop even if probes fail around the deadline. Never claim that
            # reaching it alone grants acceptance or fill in absent samples.
            if time.monotonic() - anchor['monotonic_seconds'] >= 259320:
                handle.write(json.dumps({'kind': 'collector-ended-without-final-checkpoint', 'collected_at': dt.datetime.now(dt.timezone.utc).isoformat()}) + '\n')
                handle.flush()
                os.fsync(handle.fileno())
                break
            time.sleep(max(0, 60 - (time.monotonic() - cycle)))


if __name__ == '__main__':
    main()
