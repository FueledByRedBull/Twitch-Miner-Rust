"""Assemble promotion evidence from a reviewed soak, or say what was not measured.

Usage: python3 soak_assemble_evidence.py <evidence-dir> <review.json> <windows-run.json> <vX.Y.Z> <approver>

<evidence-dir> holds soak-start.json, deployment.json, verified-candidate.json,
injected-failure-summary.json and rollback-check.json. <review.json> is a
review_soak.py report taken after the window closed. <windows-run.json> is
`gh run view <id> --json databaseId,headSha,event,conclusion,headBranch,workflowName`
for the tag's Windows Release run. <approver> is the person accepting the soak.

Prints the JSON for verify-release-evidence.ps1. Every value is copied from a
recorded measurement; if one is absent or failing, the script lists the gaps
and exits 1 without printing evidence. It never fills a field by assumption.
"""
import datetime as dt
import json
from pathlib import Path
import sys

WORKFLOWS = {'ci.yml': 'CI', 'multiarch-build.yml': 'Multiarch Build', 'deep-quality.yml': 'Deep Quality'}


def assemble(files: dict, review: dict, windows: dict, tag: str, approver: str, now: dt.datetime):
    gaps = []
    def need(condition, gap):
        if not condition:
            gaps.append(gap)
    start, candidate = files['soak-start.json'], files['verified-candidate.json']
    revision, digest = start['revision'], start['image'].split('@', 1)[1]
    identity, health, counters = review['identity'], review['health'], review['counters']
    need(identity['revision'] == revision == candidate['revision'], 'review and candidate verification name the soak revision')
    need(identity['image'] == start['image'] and candidate['manifest_digest'] == digest, 'review and candidate verification name the soak image')
    need(candidate.get('signed_provenance_and_spdx_verified') is True, 'signed provenance and SBOM verified')

    checks = [dict(workflow=WORKFLOWS[name], workflow_path=run['path'], event=run['event'], ref='refs/heads/main',
                   run_id=run['id'], head_sha=run['head_sha'], conclusion=run['conclusion'])
              for name, run in candidate['exact_main_runs'].items()]
    need(windows.get('workflowName') == 'Windows Release' and windows.get('headBranch') == tag
         and windows.get('headSha') == revision and windows.get('conclusion') == 'success',
         'successful Windows Release run for the tag at the soak revision')
    checks.append(dict(workflow='Windows Release', workflow_path='.github/workflows/windows-release.yml',
                       event=windows.get('event'), ref='refs/tags/' + tag, run_id=windows.get('databaseId'),
                       head_sha=windows.get('headSha'), conclusion=windows.get('conclusion')))

    canaries = [event for event in files['deployment.json'] if event['stage'] == 'exclusive-canary-passed']
    begins = [event for event in files['deployment.json'] if event['stage'] == 'begin']
    need(canaries and begins and begins[-1]['revision'] == revision and begins[-1]['candidate'] == start['image'],
         'exclusive canary recorded for the soak image')
    canary = canaries[-1] if canaries else {}
    need(canary.get('config_fingerprint') == start.get('config_fingerprint'), 'canary and soak configuration fingerprints match')

    levels = {finding['level'] for finding in review['findings'].values()}
    need(review['coverage']['complete'] and review['milestone']['elapsed'], 'complete 72-hour coverage')
    need(not levels & {'evidence', 'failure'}, 'no evidence or failure finding in the review')
    eventsub, pubsub = health['eventsub'], health['pubsub']
    rewards = counters['rewards']
    # The review's last snapshot is taken when it runs, so it can see claims made after the window.
    window_end = dt.datetime.fromisoformat(review['milestone']['earliest_review_utc'])
    drops = sum(dt.datetime.fromisoformat(when) <= window_end for when in review['drops']['claim_events'].values())
    capabilities = dict(
        eventsub='pass' if eventsub['verified'] and not eventsub['failed_subscriptions'] else 'fail',
        pubsub='pass' if pubsub['total'] and pubsub['acknowledged'] == pubsub['total'] else 'fail',
        watch='pass' if rewards['WATCH']['events'] else 'not-exercised',
        drops='pass' if drops else 'not-exercised',
        # The soak tools record no server-confirmed prediction outcome.
        predictions='not-exercised')
    for name in ('eventsub', 'pubsub', 'watch', 'drops'):
        need(capabilities[name] == 'pass', name + ' capability passed with a server-confirmed event')
    need(rewards['CLAIM']['events'], 'server-confirmed bonus claim')
    recovery, rollback = files['injected-failure-summary.json'], files['rollback-check.json']
    need(recovery.get('result') == 'pass', 'injected failure recovered')
    need((review.get('resource_use') or {}).get('result') == 'pass', 'resource use measured across the window')
    need(rollback.get('result') == 'pass', 'rollback exercise passed')
    if gaps:
        return None, gaps
    return dict(
        schema_version=2, source_revision=revision, manifest_digest=digest,
        platform_digests={'linux/' + arch: child for arch, child in candidate['children'].items()},
        required_checks=checks,
        canary=dict(result='pass', source_revision=revision, image_digest=digest, mutations=canary['mutations'],
                    config_fingerprint=canary['config_fingerprint'], started_at=canary['started_at'],
                    ended_at=canary['ended_at'], observed_at=canary['ended_at'],
                    monotonic_duration_seconds=canary['monotonic_duration_seconds']),
        soak=dict(result='pass', source_revision=revision, image_digest=digest,
                  config_fingerprint=start['config_fingerprint'], started_at=identity['observation_anchor_utc'],
                  ended_at=review['utc'], monotonic_duration_seconds=review['milestone']['hours'] * 3600,
                  capabilities=capabilities, opportunity=review['opportunity'],
                  server_confirmed=dict(watch_rewards=rewards['WATCH']['events'], claims=rewards['CLAIM']['events'],
                                        drops=drops, predictions=0),
                  failure_recovery={key: recovery[key] for key in
                                    ('result', 'injected_failures', 'recovered_failures', 'max_recovery_seconds')},
                  resource_use={key: review['resource_use'][key] for key in
                                ('result', 'max_rss_bytes', 'cpu_seconds', 'disk_bytes')}),
        rollback={key: rollback[key] for key in
                  ('result', 'source_revision', 'image_digest', 'compatibility', 'state_restore',
                   'data_snapshot', 'candidate_data_preserved', 'format_policy')},
        approval=dict(approved=True, actor=approver, approved_at=now.isoformat(timespec='seconds')),
    ), []


def main() -> None:
    folder, review, windows, tag, approver = sys.argv[1:]
    read = lambda path: json.loads(Path(path).read_text(encoding='utf-8-sig'))
    files = {name: read(Path(folder) / name) for name in
             ('soak-start.json', 'deployment.json', 'verified-candidate.json',
              'injected-failure-summary.json', 'rollback-check.json')}
    evidence, gaps = assemble(files, read(review), read(windows), tag, approver, dt.datetime.now(dt.timezone.utc))
    if gaps:
        raise SystemExit('Not measured, so no evidence was written:\n- ' + '\n- '.join(gaps))
    print(json.dumps(evidence, indent=2))


if __name__ == '__main__':
    main()
