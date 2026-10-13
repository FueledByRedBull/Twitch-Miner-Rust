"""Synthetic checks for the soak host tools; no Docker or miner required."""
import contextlib
import datetime as dt
import importlib.util
import io
import json
from pathlib import Path
import sys
import types
import unittest
from unittest import mock

from scripts.tests.test_review_soak import REVIEW, sample

SCRIPTS = Path(__file__).parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


CHECKPOINT, COLLECT, START = load("soak_checkpoint"), load("soak_collect"), load("soak_start")
INJECT, ASSEMBLE = load("soak_inject_failure"), load("soak_assemble_evidence")


def fake_docker(args, **_):
    if "inspect" in args:
        out = [dict(State=dict(StartedAt="2026-01-01T00:00:00Z", Health=dict(Status="healthy")),
                    Config=dict(Image="image@sha256:synthetic"), RestartCount=0,
                    HostConfig=dict(Dns=["1.1.1.1", "8.8.8.8"]))]
    elif "--status" in args:
        out = dict(revision="synthetic-revision", started_at_unix=1767225600, heartbeat_at_unix=1767225600,
                   state="ready", tasks=[], watch_slots=[], eventsub={}, pubsub={})
    elif "--health" in args:
        out = ""
    else:
        out = "2026-01-01T00:05:00.000000Z INFO +12 Streamer(username=a, channel_id=1, x) Reason: WATCH\n"
    return types.SimpleNamespace(returncode=0, stdout=out if isinstance(out, str) else json.dumps(out), stderr="")


class SoakToolTests(unittest.TestCase):
    def test_checkpoint_has_every_field_the_review_requires(self):
        stdout = io.StringIO()
        with mock.patch.object(CHECKPOINT.subprocess, "run", fake_docker), \
                mock.patch.object(sys, "argv", ["soak_checkpoint.py", "2026-01-01T00:01:00Z"]), \
                contextlib.redirect_stdout(stdout):
            CHECKPOINT.main()
        checkpoint = json.loads(stdout.getvalue())
        self.assertEqual([key for key in REVIEW.REQUIRED if key not in checkpoint], [])
        self.assertTrue(checkpoint["dns_override_matches"])
        self.assertEqual(checkpoint["soak_rewards"]["WATCH"]["points"], 12)

    def test_collector_continuity(self):
        anchor = dict(image="image", revision="revision", container_started="start",
                      runtime_started=1, monotonic_seconds=10)
        checkpoint = dict(anchor, monotonic_seconds=70, soak_elapsed_wall_seconds=60)
        self.assertIsNone(COLLECT.continuity(anchor, checkpoint))
        self.assertEqual(COLLECT.continuity(anchor, dict(checkpoint, runtime_started=2)), "runtime_started")
        self.assertEqual(COLLECT.continuity(anchor, dict(checkpoint, monotonic_seconds=73)), "clock-continuity")

    def test_restarted_collector_appends_a_continuity_failure(self):
        import tempfile
        anchor = dict(image="image", revision="revision", container_started="start", runtime_started=1,
                      monotonic_seconds=10, observation_anchor_utc="2026-01-01T00:00:00+00:00")
        rebooted = dict(anchor, runtime_started=2, monotonic_seconds=5, soak_elapsed_wall_seconds=600)
        probe = types.SimpleNamespace(returncode=0, stdout=json.dumps(rebooted))
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / "soak-anchor.json").write_text(json.dumps(anchor), encoding="utf-8")
            history = root / "minute-checkpoints.jsonl"
            history.write_text('{"kind": "checkpoint"}\n', encoding="utf-8")
            with mock.patch.object(COLLECT.subprocess, "run", return_value=probe), \
                    mock.patch.object(sys, "argv", ["soak_collect.py", str(root / "soak-anchor.json"), "checker"]):
                COLLECT.main()
            kinds = [json.loads(line)["kind"] for line in history.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(kinds, ["checkpoint", "continuity-failure"])

    def test_injected_failure_recovers_only_with_credit_newer_than_the_reconnect(self):
        healthy = dict(health_exit=0, tasks_clean=True, eventsub_ok=True, pubsub_ok=True, credit=[1000, 1010])
        self.assertTrue(INJECT.recovered(healthy, 1000))
        self.assertFalse(INJECT.recovered(healthy, 1005))
        self.assertFalse(INJECT.recovered(dict(healthy, credit=[]), 0))
        for broken in (dict(health_exit=1), dict(tasks_clean=False), dict(eventsub_ok=False), dict(pubsub_ok=False)):
            self.assertFalse(INJECT.recovered(dict(healthy, **broken), 1000))

    def test_evidence_is_assembled_only_when_every_field_was_measured(self):
        image, revision, hash_ = "repo@sha256:" + "a" * 64, "c" * 40, "f" * 64
        run = dict(id=1, head_sha=revision, event="push", conclusion="success")
        files = {
            "soak-start.json": dict(image=image, revision=revision, config_fingerprint=hash_),
            "verified-candidate.json": dict(
                revision=revision, manifest_digest=image.split("@")[1], children=dict(arm64="sha256:1", amd64="sha256:2"),
                signed_provenance_and_spdx_verified=True,
                exact_main_runs={name: dict(run, path=".github/workflows/" + name) for name in ASSEMBLE.WORKFLOWS}),
            "deployment.json": [
                dict(stage="begin", candidate=image, revision=revision),
                dict(stage="exclusive-canary-passed", started_at="2026-01-01T00:00:00+00:00",
                     ended_at="2026-01-01T00:00:30+00:00", monotonic_duration_seconds=30, mutations="none",
                     config_fingerprint=hash_)],
            "injected-failure-summary.json": dict(result="pass", injected_failures=1, recovered_failures=1,
                                                  max_recovery_seconds=300.0),
            "rollback-check.json": dict(result="pass", source_revision="b" * 40, image_digest="sha256:" + "b" * 64,
                                        compatibility="pass", state_restore="pass", data_snapshot="pass",
                                        candidate_data_preserved="pass", format_policy="compatible"),
        }
        review = dict(
            utc="2026-01-04T00:10:00+00:00", findings={"eventsub": dict(level="review")},
            identity=dict(image=image, revision=revision, observation_anchor_utc="2026-01-01T00:10:00+00:00"),
            coverage=dict(complete=True), milestone=dict(elapsed=True, hours=72.0),
            health=dict(eventsub=dict(verified=True, failed_subscriptions=0), pubsub=dict(total=2, acknowledged=2)),
            counters=dict(rewards=dict(WATCH=dict(events=5), CLAIM=dict(events=2))),
            drops=dict(claimed_transitions=1), opportunity=dict(eligible_seconds=10.0, live_seconds=5.0),
            resource_use=dict(result="pass", max_rss_bytes=1, cpu_seconds=1.0, disk_bytes=1, samples=3))
        windows = dict(workflowName="Windows Release", headBranch="v1.2.3", headSha=revision, event="push",
                       conclusion="success", databaseId=4)
        now = dt.datetime(2026, 1, 4, 1, tzinfo=dt.timezone.utc)
        evidence, gaps = ASSEMBLE.assemble(files, review, windows, "v1.2.3", "operator", now)
        self.assertEqual(gaps, [])
        self.assertEqual(evidence["soak"]["monotonic_duration_seconds"], 259200.0)
        self.assertEqual(evidence["soak"]["server_confirmed"], dict(watch_rewards=5, claims=2, drops=1, predictions=0))
        self.assertEqual(sorted(evidence["platform_digests"]), ["linux/amd64", "linux/arm64"])
        self.assertEqual(len(evidence["required_checks"]), 4)
        review["drops"]["claimed_transitions"] = 0
        review["findings"]["restart"] = dict(level="failure")
        evidence, gaps = ASSEMBLE.assemble(files, review, {}, "v1.2.3", "operator", now)
        self.assertIsNone(evidence)
        self.assertEqual(len(gaps), 3)

    def test_checkpoint_reports_unmeasured_resources_as_none(self):
        self.assertEqual(CHECKPOINT.resource_use({}), dict(memory_current_bytes=None, memory_peak_bytes=None,
                                                           cpu_usage_seconds=None, data_dir_bytes=None))

    def test_anchor_waits_for_a_fully_earning_runtime(self):
        ready = sample(60, application_health_exit_code=0)
        self.assertTrue(START.earning(ready))
        idle = [dict(ready["watch_slots"][0], progress="idle"), ready["watch_slots"][1]]
        stale = [dict(slot, last_server_confirmed_points_unix=ready["runtime_started"] - 1)
                 for slot in ready["watch_slots"]]
        for name, changes in [
            ("health probe failed", dict(application_health_exit_code=1)),
            ("one slot idle", dict(watch_slots=idle)),
            ("credit predates runtime", dict(watch_slots=stale)),
            ("no eventsub plan", dict(eventsub=dict(ready["eventsub"], planned_subscriptions=0, active_subscriptions=0))),
            ("eventsub incomplete", dict(eventsub=dict(ready["eventsub"], active_subscriptions=1))),
            ("no pubsub topics", dict(pubsub=dict(total=0, acknowledged=0, failed_connections=0))),
            ("no tasks", dict(tasks=[])),
        ]:
            with self.subTest(name):
                self.assertFalse(START.earning(dict(ready, **changes)))


if __name__ == "__main__":
    unittest.main()
