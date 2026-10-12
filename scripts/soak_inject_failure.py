"""Inject one network failure into the soaking miner and record its recovery.

Usage: python3 soak_inject_failure.py <evidence-dir> [<cut-seconds>]
Disconnects the container from its only Docker network for the cut (default 90
seconds), reconnects it with the same aliases, and waits for a healthy runtime
with fresh server credit on every selected watch slot. Writes
injected-failure.jsonl and injected-failure-summary.json beside the anchor.
Run it once, inside the soak window. A cut that holds a task at five
consecutive failures fails the soak; keep it short.
SOAK_CONTAINER overrides the container name.
"""
import datetime as dt
import json
import os
from pathlib import Path
import subprocess
import sys
import time

CONTAINER = os.environ.get('SOAK_CONTAINER', 'twitch-miner')
RECOVERY_LIMIT_SECONDS = 900


def run(*args, timeout=30):
    return subprocess.run(args, capture_output=True, text=True, timeout=timeout)


def state():
    """Sanitized health: no channel, account or topic identifiers."""
    health = run('docker', 'exec', CONTAINER, '/twitch-miner', '--health').returncode
    status = json.loads(run('docker', 'exec', CONTAINER, '/twitch-miner', '--status').stdout)
    es, ps = status.get('eventsub') or {}, status.get('pubsub') or {}
    caps = ps.get('capabilities') or []
    return dict(
        health_exit=health,
        tasks_clean=all(t.get('consecutive_failures') == 0 for t in status.get('tasks', [])),
        eventsub_ok=bool(es.get('verified') and 0 < es.get('active_subscriptions', 0) == es.get('planned_subscriptions')),
        pubsub_ok=0 < ps.get('total_topics', 0) == sum(c.get('acknowledged_topics', 0) for c in caps),
        credit=[w.get('last_server_confirmed_points_unix') or 0 for w in status.get('watch_slots', []) if w.get('selected')],
        restarts=json.loads(run('docker', 'inspect', CONTAINER).stdout)[0]['RestartCount'])


def recovered(current, restored_unix):
    return bool(current['health_exit'] == 0 and current['tasks_clean'] and current['eventsub_ok'] and current['pubsub_ok']
                and current['credit'] and all(credit >= restored_unix for credit in current['credit']))


def main():
    evidence = Path(sys.argv[1]).resolve()
    cut_seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 90.0
    assert (evidence / 'soak-anchor.json').is_file(), 'evidence directory has no soak anchor'
    assert 0 < cut_seconds <= 120, 'cut must be between 0 and 120 seconds'
    os.umask(0o077)
    log = evidence / 'injected-failure.jsonl'
    assert not log.exists(), 'a failure was already injected into this soak'

    def emit(stage, **values):
        with log.open('a') as handle:
            handle.write(json.dumps(dict(utc=dt.datetime.now(dt.timezone.utc).isoformat(), stage=stage, **values)) + '\n')

    networks = json.loads(run('docker', 'inspect', CONTAINER).stdout)[0]['NetworkSettings']['Networks']
    assert len(networks) == 1, 'expected the container on exactly one network'
    (network, attachment), = networks.items()
    aliases = [alias for alias in attachment.get('Aliases') or [] if alias]
    before = state()
    assert recovered(before, 0), 'the miner is not healthy and earning; not injecting a failure'
    emit('before', **before)

    started = dt.datetime.now(dt.timezone.utc)
    cut = time.monotonic()
    emit('disconnected', exit_code=run('docker', 'network', 'disconnect', network, CONTAINER).returncode)
    try:
        time.sleep(cut_seconds)
    finally:
        connect = run('docker', 'network', 'connect', *[part for alias in aliases for part in ('--alias', alias)], network, CONTAINER)
        restored = time.monotonic()
        restored_unix = int(time.time())
        ended = dt.datetime.now(dt.timezone.utc)
        emit('reconnected', exit_code=connect.returncode, cut_seconds=round(restored - cut, 1))
    if connect.returncode:
        raise SystemExit('network reconnect failed; restore the container network by hand')

    recovery = None
    while time.monotonic() - restored < RECOVERY_LIMIT_SECONDS:
        time.sleep(5)
        try:
            current = state()
        except (subprocess.TimeoutExpired, ValueError, KeyError, IndexError) as error:
            emit('probe-error', error=type(error).__name__)
            continue
        if recovered(current, restored_unix):
            recovery = round(time.monotonic() - restored, 1)
            emit('recovered', seconds_after_restore=recovery, **current)
            break
    after = state()
    passed = recovery is not None and after['restarts'] == before['restarts']
    summary = dict(kind='injected-failure', method='docker network disconnect/connect of the miner container',
                   cut_started_utc=started.isoformat(), cut_ended_utc=ended.isoformat(),
                   cut_seconds=round(restored - cut, 1), injected_failures=1, recovered_failures=int(passed),
                   max_recovery_seconds=recovery, container_restarts=after['restarts'] - before['restarts'],
                   recovery_definition='healthy runtime, clean tasks, EventSub and PubSub complete, and server credit '
                                       'newer than the reconnect on every selected watch slot',
                   result='pass' if passed else 'fail')
    (evidence / 'injected-failure-summary.json').write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary), flush=True)
    raise SystemExit(0 if passed else 1)


if __name__ == '__main__':
    main()
