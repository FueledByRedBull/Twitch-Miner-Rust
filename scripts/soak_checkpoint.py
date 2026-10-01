"""Print one sanitized soak checkpoint for the running miner container as JSON.

Usage: python3 soak_checkpoint.py [<observation-anchor-utc>]
Reads only Docker metadata, `--status`, `--health`, and current-session logs.
SOAK_CONTAINER and SOAK_EXPECTED_DNS (comma-separated, empty for none)
override the container name and the expected DNS servers.
"""
import collections
import datetime as dt
import json
import os
import re
import statistics
import subprocess
import sys
import time

CONTAINER = os.environ.get('SOAK_CONTAINER', 'twitch-miner')
EXPECTED_DNS = [server for server in os.environ.get('SOAK_EXPECTED_DNS', '1.1.1.1,8.8.8.8').split(',') if server]


def run(*args):
    p = subprocess.run(args, capture_output=True, text=True, timeout=60)
    if p.returncode:
        raise RuntimeError(f'{args[0]} check failed: exit {p.returncode}')
    return p.stdout


def main():
    c = json.loads(run('docker', 'inspect', CONTAINER))[0]
    s = json.loads(run('docker', 'exec', CONTAINER, '/twitch-miner', '--status'))
    application_health = subprocess.run(['docker', 'exec', CONTAINER, '/twitch-miner', '--health'], capture_output=True, text=True, timeout=10).returncode
    started = c['State']['StartedAt']
    started_epoch = int(dt.datetime.fromisoformat(started.replace('Z', '+00:00')).timestamp())
    p = subprocess.run(['docker', 'logs', '--timestamps', '--since', started, CONTAINER], capture_output=True, text=True, timeout=60)
    if p.returncode:
        raise RuntimeError('current-session log retrieval failed')
    logs = re.sub(r'\x1b\[[0-9;]*m', '', p.stdout + p.stderr)
    rewards = []
    warnings = collections.Counter()
    errors = 0
    timeouts = []
    deltas = []
    for line in logs.splitlines():
        try:
            timestamp = dt.datetime.fromisoformat(line.split(' ', 1)[0].replace('Z', '+00:00'))
        except ValueError:
            continue
        reward = re.search(r'\+(\d[\d,]*)\s+.*?Reason:\s*(WATCH_STREAK|WATCH|CLAIM|RAID)\b', line)
        if reward:
            rewards.append((timestamp, reward[2], int(reward[1].replace(',', ''))))
        if re.search(r'\bWARN\b', line):
            kind = re.search(r'error_class="?([a-z0-9-]+)', line)
            warnings[kind[1] if kind else 'unclassified'] += 1
        if re.search(r'\bERROR\b', line):
            errors += 1
        if 'minute watched timed out' in line:
            timeouts.append(timestamp.isoformat())
        delta = re.search(r'\bbalance_delta=(\d+)', line)
        if delta:
            deltas.append((timestamp.isoformat(), int(delta[1])))
    now = dt.datetime.now(dt.timezone.utc)
    watch = sorted(x for x in rewards if x[1] == 'WATCH')
    es, ps = s.get('eventsub') or {}, s.get('pubsub') or {}
    caps = ps.get('capabilities') or []
    summary = {
        'utc': now.isoformat(), 'monotonic_seconds': time.monotonic(), 'image': c['Config']['Image'], 'revision': s.get('revision'),
        'container_started': started, 'runtime_started': s.get('started_at_unix'), 'restarts': c['RestartCount'],
        'status_fresh': s.get('started_at_unix', 0) >= started_epoch and 0 <= int(time.time()) - s.get('heartbeat_at_unix', 0) <= 120,
        'application_health_exit_code': application_health,
        'health': c['State'].get('Health', {}).get('Status'), 'runtime_state': s.get('state'),
        'heartbeat_age_seconds': int(time.time()) - s.get('heartbeat_at_unix', 0),
        'tasks': [{'name': t.get('name'), 'failures': t.get('consecutive_failures'), 'last_error_class': t.get('last_error_class')} for t in s.get('tasks', [])],
        'eventsub': {k: es.get(k) for k in ['active_subscriptions', 'planned_subscriptions', 'failed_subscriptions', 'verified']},
        'pubsub': {'total': ps.get('total_topics'), 'acknowledged': sum(x.get('acknowledged_topics', 0) for x in caps), 'failed_connections': sum(x.get('failure_class') is not None for x in caps)},
        'watch_slots': [{k: x.get(k) for k in ['slot', 'channel_key', 'broadcast_key', 'selected', 'progress', 'progress_age_seconds', 'selection_reason', 'last_accepted_watch_unix', 'last_server_confirmed_points_unix', 'last_context_observed_unix', 'consecutive_failures', 'last_error_class']} for x in s.get('watch_slots', [])],
        'rewards': {reason: {'events': sum(x[1] == reason for x in rewards), 'points': sum(x[2] for x in rewards if x[1] == reason)} for reason in ['WATCH', 'CLAIM', 'WATCH_STREAK', 'RAID']},
        'first_watch_utc': watch[0][0].isoformat() if watch else None,
        'last_watch_utc': watch[-1][0].isoformat() if watch else None,
        'max_watch_gap_seconds': max(((b[0] - a[0]).total_seconds() for a, b in zip(watch, watch[1:])), default=None),
        'watch_drought_seconds': (now - watch[-1][0]).total_seconds() if watch else None,
        'warnings': dict(warnings), 'errors': errors, 'watch_timeouts': len(timeouts), 'last_watch_timeout_utc': timeouts[-1] if timeouts else None,
        'reward_log_lines': sum('Reason:' in line for line in logs.splitlines()),
        'positive_context_deltas': {'count': len(deltas), 'sum': sum(x[1] for x in deltas), 'first_utc': deltas[0][0] if deltas else None},
        'drop_progress': [{**{'drop_key': d.get('drop_key') if re.fullmatch(r'drop-[0-9a-f]{16}', d.get('drop_key', '')) else None}, **{k: d.get(k) for k in ['current_minutes_watched', 'required_minutes_watched', 'is_claimed', 'observed_at_unix', 'last_progress_increase_unix']}} for d in s.get('counters', {}).get('drop_progress', [])],
        'dns_override_matches': (c['HostConfig'].get('Dns') or []) == EXPECTED_DNS,
        'resources': {'memory_limit_bytes': c['HostConfig'].get('Memory'), 'nano_cpus': c['HostConfig'].get('NanoCpus'), 'read_only': c['HostConfig'].get('ReadonlyRootfs'), 'cap_drop': c['HostConfig'].get('CapDrop'), 'security_options': c['HostConfig'].get('SecurityOpt')},
    }
    anchor = dt.datetime.fromisoformat(sys.argv[1].replace('Z', '+00:00')) if len(sys.argv) > 1 else dt.datetime.fromisoformat(started.replace('Z', '+00:00'))
    assert anchor.tzinfo is not None and anchor <= now, 'Invalid observation anchor'
    anchored = [r for r in rewards if r[0] >= anchor]
    summary['observation_anchor_utc'] = anchor.isoformat()
    summary['soak_elapsed_wall_seconds'] = (now - anchor).total_seconds()
    summary['soak_rewards'] = {}
    for reason in ['WATCH', 'CLAIM', 'WATCH_STREAK', 'RAID']:
        points = [r[2] for r in anchored if r[1] == reason]
        item = {'events': len(points), 'points': sum(points), 'award_histogram': dict(collections.Counter(points))}
        base = {'WATCH': 10, 'CLAIM': 50}.get(reason)
        if base and all(value in [base, base * 1.2, base * 1.4, base * 2] for value in points):
            item['base_equivalent_points'] = base * len(points)
        summary['soak_rewards'][reason] = item
    for hours in [1, 6]:
        rows = [r for r in anchored if r[1] == 'WATCH' and (now - r[0]).total_seconds() <= hours * 3600]
        summary[f'watch_last_{hours}h'] = {'events': len(rows), 'points': sum(r[2] for r in rows),
                                           'base_equivalent_points': 10 * len(rows) if all(r[2] in [10, 12, 14, 20] for r in rows) else None,
                                           'full_window_elapsed': (now - anchor).total_seconds() >= hours * 3600}

    groups = collections.defaultdict(list)
    for line in logs.splitlines():
        match = re.search(r'\+(\d+)\s+.*?Streamer\(username=[^,]+, channel_id=([^,]+),.*?Reason:\s*(WATCH|CLAIM)\b', line)
        if match:
            timestamp = dt.datetime.fromisoformat(line.split(' ', 1)[0].replace('Z', '+00:00'))
            if timestamp >= anchor:
                groups[(match[2], match[3])].append(timestamp)
    summary['reward_cadence'] = {}
    for reason, ceiling in [('WATCH', 600), ('CLAIM', 1800)]:
        gaps = []
        for (_, group_reason), timestamps in groups.items():
            if reason == group_reason:
                timestamps.sort()
                gaps.extend((b - a).total_seconds() for a, b in zip(timestamps, timestamps[1:]) if (b - a).total_seconds() <= ceiling)
        summary['reward_cadence'][reason] = {'adjacent_same_channel_gaps': len(gaps),
                                             'maximum_included_gap_seconds': ceiling, 'median_seconds': statistics.median(gaps) if gaps else None}
    summary['historical_slot_eligibility_and_multiplier_minutes_available'] = False
    print(json.dumps(summary, sort_keys=True))


if __name__ == '__main__':
    main()
