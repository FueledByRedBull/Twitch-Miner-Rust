"""Exercise the rollback image against the pre-deploy snapshot and the live data.

Usage: python3 soak_rollback_check.py <evidence-dir> <rollback-image@sha256:digest> <rollback-revision> [<snapshot.tar>]
Read-only towards the running miner: nothing is stopped, replaced or written
to its data. Every container runs without a network. The snapshot defaults to
runtime-data.tar in the evidence directory. Writes rollback-check.json there:

- data_snapshot: the snapshot archive lists and extracts completely.
- state_restore: the rollback image accepts the restored snapshot.
- compatibility: the rollback image accepts the candidate's current data.
- candidate_data_preserved: the live data was only ever mounted read-only.

The snapshot holds credentials. It is extracted to a private directory that
is removed before the script exits, and no file content is printed.
SOAK_CONTAINER overrides the container name.
"""
import datetime as dt
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

CONTAINER = os.environ.get('SOAK_CONTAINER', 'twitch-miner')


def accepts(image, user, data):
    """Whether the image's offline configuration check passes for a data directory."""
    return subprocess.run(
        ['docker', 'run', '--rm', '--network', 'none', '--read-only', '--cap-drop', 'ALL',
         '--security-opt', 'no-new-privileges:true', '--user', user, '-v', f'{data}:/data:ro',
         image, '--data-dir', '/data', '--check-config'],
        capture_output=True, text=True, timeout=120).returncode == 0


def main():
    evidence = Path(sys.argv[1]).resolve()
    image, revision = sys.argv[2], sys.argv[3]
    snapshot = Path(sys.argv[4]).resolve() if len(sys.argv) > 4 else evidence / 'runtime-data.tar'
    assert evidence.is_dir(), 'evidence directory is missing'
    assert re.fullmatch(r'[a-z0-9./_-]+@sha256:[0-9a-f]{64}', image), 'rollback image must be digest-pinned'
    assert re.fullmatch(r'[0-9a-f]{40}', revision), 'rollback revision must be a full commit SHA'
    os.umask(0o077)

    container = json.loads(subprocess.run(['docker', 'inspect', CONTAINER], capture_output=True, text=True, timeout=30, check=True).stdout)[0]
    assert container['Config']['Image'] != image, 'the rollback image is the running image'
    user = container['Config'].get('User') or f'{os.getuid()}:{os.getgid()}'
    live = next(m['Source'] for m in container['Mounts'] if m['Destination'] == '/data')
    version = subprocess.run(['docker', 'run', '--rm', '--network', 'none', image, '--version'], capture_output=True, text=True, timeout=120)
    assert version.returncode == 0 and revision in version.stdout, 'rollback image does not report the expected revision'

    result = dict(checked_at=dt.datetime.now(dt.timezone.utc).isoformat(), source_revision=revision,
                  image_digest=image.split('@', 1)[1], data_snapshot='fail', state_restore='fail',
                  compatibility='fail', candidate_data_preserved='pass', format_policy='compatible')
    restored = Path(tempfile.mkdtemp(prefix='rollback-restore-', dir=evidence))
    try:
        with tarfile.open(snapshot) as archive:
            files = [member for member in archive.getmembers() if member.isfile()]
            archive.extractall(restored, filter='data')
        extracted = sum(1 for _, _, names in os.walk(restored) for _ in names)
        if files and extracted == len(files):
            result['data_snapshot'] = 'pass'
            result['snapshot_files'] = extracted
        if result['data_snapshot'] == 'pass' and accepts(image, user, restored):
            result['state_restore'] = 'pass'
    except (OSError, tarfile.TarError) as error:
        result['snapshot_error'] = type(error).__name__
    finally:
        shutil.rmtree(restored, ignore_errors=True)
    if accepts(image, user, live):
        result['compatibility'] = 'pass'
    passed = all(result[key] == 'pass' for key in ('data_snapshot', 'state_restore', 'compatibility', 'candidate_data_preserved'))
    result['result'] = 'pass' if passed else 'fail'
    (evidence / 'rollback-check.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result), flush=True)
    raise SystemExit(0 if passed else 1)


if __name__ == '__main__':
    main()
