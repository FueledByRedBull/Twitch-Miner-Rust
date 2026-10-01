"""Synthetic checks for the soak host tools; no Docker or miner required."""
import contextlib
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
