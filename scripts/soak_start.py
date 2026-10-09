"""Anchor a 72-hour soak once the deployed candidate is earning, then start the collector.

Usage: python3 soak_start.py <evidence-dir> <image@sha256:digest> <revision>
Run on the soak host after a guarded deploy. The evidence directory must exist,
name the short revision, and hold no anchor yet. Writes anchor-prerequisites.json,
soak-anchor.json, collector-output.log and soak-start.json there. The anchor
waits for fresh server credit on every watch slot; it never grants acceptance.
With SOAK_COLLECTOR_UNIT set to a systemd user template unit (see
deploy/twitch-miner-soak@.service), the collector runs as that unit and
restarts after a reboot; otherwise it runs as a detached process.
"""
import datetime as dt
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
CHECKER = HERE / 'soak_checkpoint.py'
COLLECTOR = HERE / 'soak_collect.py'
WATCH_SLOTS = 2


def earning(cp, slot_count=WATCH_SLOTS):
    """Whether a checkpoint shows a healthy runtime earning on every watch slot."""
    slots = [s for s in cp['watch_slots'] if s.get('selected')]
    es, ps = cp['eventsub'], cp['pubsub']
    return bool(cp['health'] == 'healthy' and cp['application_health_exit_code'] == 0
                and cp['status_fresh'] and cp['tasks']
                and all(t['failures'] == 0 and not t['last_error_class'] for t in cp['tasks'])
                and es['verified'] and es['failed_subscriptions'] == 0
                and 0 < es['planned_subscriptions'] == es['active_subscriptions']
                and 0 < ps['total'] == ps['acknowledged'] and ps['failed_connections'] == 0
                and len(slots) == slot_count
                and all(s.get('progress') == 'earning' and s.get('consecutive_failures') == 0
                        and (s.get('last_server_confirmed_points_unix') or 0) >= cp['runtime_started']
                        and (s.get('last_accepted_watch_unix') or 0) >= cp['runtime_started'] for s in slots))


def main():
    evidence = Path(sys.argv[1]).resolve()
    image, revision = sys.argv[2], sys.argv[3]
    assert re.fullmatch(r'[0-9a-f]{40}', revision), 'revision must be a full commit SHA'
    assert re.fullmatch(r'[a-z0-9./_-]+@sha256:[0-9a-f]{64}', image), 'image must be digest-pinned'
    assert evidence.is_dir() and revision[:7] in evidence.name, 'evidence directory must name the short revision'
    os.umask(0o077)
    assert not (evidence / 'soak-anchor.json').exists(), 'anchor already exists'
    for attempt in range(48):
        result = subprocess.run([sys.executable, str(CHECKER)], capture_output=True, text=True, timeout=55)
        assert result.returncode == 0, 'checker failed; no anchor'
        cp = json.loads(result.stdout)
        assert cp['image'] == image and cp['revision'] == revision, 'identity mismatch'
        assert cp['restarts'] == 0 and cp['dns_override_matches'], 'deployment mismatch'
        if earning(cp):
            break
        if attempt % 6 == 0:
            print(json.dumps({'stage': 'waiting-for-fresh-server-credit', 'attempt': attempt}), flush=True)
        time.sleep(10)
    else:
        raise RuntimeError('earning anchor gate timed out')

    now = dt.datetime.now(dt.timezone.utc)
    anchor = {k: cp[k] for k in ('image', 'revision', 'container_started', 'runtime_started')}
    anchor.update(observation_anchor_utc=now.isoformat(), monotonic_seconds=time.monotonic(),
                  application_health_exit_code=0, dns_override_matches=True)
    (evidence / 'anchor-prerequisites.json').write_text(json.dumps(cp, indent=2))
    with (evidence / 'soak-anchor.json').open('x') as handle:
        json.dump(anchor, handle, indent=2)
        handle.flush()
        os.fsync(handle.fileno())
    unit = os.environ.get('SOAK_COLLECTOR_UNIT')
    if unit:
        collector = f'{unit}@{evidence.name}.service'
        subprocess.run(['systemctl', '--user', 'enable', '--now', collector], check=True)
    else:
        with (evidence / 'collector-output.log').open('x') as output:
            collector = subprocess.Popen([sys.executable, str(COLLECTOR), str(evidence / 'soak-anchor.json'), str(CHECKER)],
                                         stdin=subprocess.DEVNULL, stdout=output, stderr=output, start_new_session=True).pid
    summary = dict(anchor, collector=collector,
                   earliest_review_utc=(now + dt.timedelta(seconds=259200)).isoformat(),
                   pre_anchor_warnings=cp['warnings'], pre_anchor_errors=cp['errors'],
                   pre_anchor_watch_timeouts=cp['watch_timeouts'], evidence=str(evidence))
    (evidence / 'soak-start.json').write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary), flush=True)


if __name__ == '__main__':
    main()
