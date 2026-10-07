"""Synthetic evidence: failures must remain visible after recovery."""
import datetime as dt
import importlib.util
import json
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).parents[1] / "review_soak.py"
if SCRIPT.exists():
    SPEC = importlib.util.spec_from_file_location("review_soak", SCRIPT)
    REVIEW = importlib.util.module_from_spec(SPEC)
    SPEC.loader.exec_module(REVIEW)

START = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc)
ANCHOR = dict(image="image@sha256:synthetic", revision="synthetic-revision",
              container_started="2025-12-31T23:59:00Z", runtime_started=1767225540,
              observation_anchor_utc=START.isoformat(), monotonic_seconds=100.0)


def sample(seconds=0, **changes):
    now = START + dt.timedelta(seconds=seconds)
    result = dict(ANCHOR, utc=now.isoformat(), monotonic_seconds=100.0 + seconds,
                  soak_elapsed_wall_seconds=seconds, restarts=0, health="healthy",
                  status_fresh=True, heartbeat_age_seconds=20, runtime_state="ready",
                  tasks=[dict(name="minute-watch", failures=0, last_error_class=None)],
                  eventsub=dict(verified=True, active_subscriptions=2,
                                planned_subscriptions=2, failed_subscriptions=0),
                  pubsub=dict(total=2, acknowledged=2, failed_connections=0),
                  watch_slots=[dict(slot=n, selected=True, channel_key=f"channel-{n}",
                                    broadcast_key=f"broadcast-{n}", progress="earning",
                                    progress_age_seconds=20, consecutive_failures=0,
                                    last_accepted_watch_unix=int(now.timestamp()) - 10,
                                    last_server_confirmed_points_unix=int(now.timestamp()) - 20)
                               for n in range(2)],
                  soak_rewards={"WATCH": dict(events=130, points=1560,
                                              award_histogram={"12": 130},
                                              base_equivalent_points=1300),
                                "CLAIM": dict(events=40, points=2000,
                                              award_histogram={"50": 40},
                                              base_equivalent_points=2000)},
                  watch_last_6h=dict(full_window_elapsed=seconds >= 21600,
                                     base_equivalent_points=1300, points=1560),
                  max_watch_gap_seconds=300, watch_drought_seconds=20,
                  warnings={}, errors=0, watch_timeouts=0, drop_progress=[],
                  historical_slot_eligibility_and_multiplier_minutes_available=False)
    result.update(changes)
    return result


def record(checkpoint):
    return dict(kind="checkpoint", collected_at=checkpoint["utc"], checkpoint=checkpoint)


def review(samples, current=None, records=None, now=None):
    current = current or samples[-1]
    return REVIEW.analyze(ANCHOR, records if records is not None else map(record, samples),
                          current, now or dt.datetime.fromisoformat(current["utc"]))


class SoakReviewTests(unittest.TestCase):
    def test_health_threshold_matches_the_application_contract(self):
        source = (SCRIPT.parents[1] / "crates/tm-app/src/status.rs").read_text(encoding="utf-8")
        threshold = re.search(r"const MAX_CONSECUTIVE_FAILURES: u32 = (\d+);", source)
        self.assertIsNotNone(threshold)
        self.assertEqual(REVIEW.MAX_CONSECUTIVE_FAILURES, int(threshold[1]))

    def test_anchor_requiring_application_health_rejects_missing_probe(self):
        anchor = dict(ANCHOR, application_health_exit_code=0)
        current = sample(60)
        result = REVIEW.analyze(anchor, [record(current)], current,
                                dt.datetime.fromisoformat(current["utc"]))
        self.assertIn("missing-field:application-health", result["findings"])

    def test_dns_override_must_match_when_bound_by_anchor(self):
        for value in (False, None):
            current = sample(60, dns_override_matches=value)
            result = REVIEW.analyze(dict(ANCHOR, dns_override_matches=True),
                                    [record(current)], current,
                                    dt.datetime.fromisoformat(current["utc"]))
            self.assertIn("identity:dns-override", result["findings"])

    def test_task_threshold_failure_survives_docker_healthy_and_recovery(self):
        for failures in (5, 7):
            with self.subTest(failures=failures):
                bad = sample(60, tasks=[dict(name="minute-watch", failures=failures,
                                             last_error_class="watch-request")])
                result = review([sample(), bad, sample(120)])
                self.assertIn("task-unhealthy:minute-watch", result["findings"])
                finding = result["findings"]["task-unhealthy:minute-watch"]
                self.assertEqual(finding["level"], "failure")
                self.assertEqual(finding["first_utc"], bad["utc"])
                self.assertFalse(finding["active"])
                self.assertFalse(result["coverage"]["complete"])

    def test_task_threshold_escalation_requests_a_new_decision(self):
        mild = sample(60, tasks=[dict(name="minute-watch", failures=4,
                                     last_error_class="watch-request")])
        bad = sample(120, tasks=[dict(name="minute-watch", failures=5,
                                     last_error_class="watch-request")])
        previous = review([sample(), mild])
        self.assertNotIn("task-unhealthy:minute-watch", previous["findings"])
        result = REVIEW.compare(review([sample(), mild, bad]), previous)
        self.assertTrue(result["decision_required"])
        self.assertIn("task-unhealthy:minute-watch", result["changes"]["new_or_recurring_findings"])

    def test_selected_slot_threshold_is_a_failure(self):
        bad = sample(60)
        bad["watch_slots"][0]["consecutive_failures"] = 5
        result = review([sample(), bad, sample(120)])
        self.assertIn("watch-slot-unhealthy:0", result["findings"])
        self.assertEqual(result["findings"]["watch-slot-unhealthy:0"]["level"], "failure")

    def test_health_failure_run_matches_the_compose_healthcheck_retries(self):
        compose = (SCRIPT.parents[1] / "deploy/docker-compose.bind-mount.yml").read_text(encoding="utf-8")
        retries = re.search(r"healthcheck:.*?retries: (\d+)", compose, re.S)
        self.assertIsNotNone(retries)
        self.assertEqual(REVIEW.HEALTH_FAILURE_SAMPLES, int(retries[1]))

    def test_isolated_application_health_failures_are_retained_for_review(self):
        unhealthy = [60, 120, 240, 300, 420]  # never three samples in a row
        result = review([sample(n * 60, application_health_exit_code=int(n * 60 in unhealthy))
                         for n in range(9)])
        finding = result["findings"]["application-health"]
        self.assertEqual((finding["level"], finding["samples"], finding["active"]), ("review", 5, False))
        self.assertTrue(result["coverage"]["complete"])

    def test_sustained_application_health_failure_fails_and_stays_after_recovery(self):
        result = review([sample(n * 60, application_health_exit_code=int(1 <= n <= 3))
                         for n in range(6)])
        finding = result["findings"]["application-health"]
        self.assertEqual((finding["level"], finding["samples"], finding["active"]), ("failure", 3, False))
        self.assertFalse(result["coverage"]["complete"])

    def test_cli_replay_persists_review_and_compares_unchanged_input(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / "anchor.json").write_text(json.dumps(ANCHOR), encoding="utf-8")
            current = sample(60)
            (root / "checkpoint.json").write_text(json.dumps(current), encoding="utf-8")
            (root / "history.jsonl").write_text(json.dumps(record(sample())) + "\n" +
                                                json.dumps(record(current)) + "\n", encoding="utf-8")
            command = [sys.executable, str(SCRIPT), "--anchor", str(root / "anchor.json"),
                       "--history", str(root / "history.jsonl"), "--checkpoint",
                       str(root / "checkpoint.json"), "--now", current["utc"],
                       "--output-dir", str(root / "reviews")]
            first = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            second = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(second.returncode, 0, second.stderr)
            report = json.loads(second.stdout)
            self.assertFalse(report["decision_required"])
            self.assertEqual(report["changes"]["new_history_samples"], 0)
            saved = json.loads((root / "reviews/latest.json").read_text(encoding="utf-8"))
            self.assertEqual(saved["coverage"]["history_samples"], 2)

    def test_recovery_cannot_erase_identity_restart_or_health_failures(self):
        changed = sample(60, revision="other", restarts=1, health="unhealthy")
        result = review([sample(), changed, sample(120)])
        self.assertIn("identity:revision", result["findings"])
        self.assertIn("restart", result["findings"])
        self.assertIn("health", result["findings"])
        self.assertEqual(result["findings"]["restart"]["first_utc"], changed["utc"])
        self.assertFalse(result["findings"]["restart"]["active"])

    def test_clock_drift_probe_errors_and_gaps_prevent_clean_coverage(self):
        bad_clock = sample(60, monotonic_seconds=163)
        rows = [record(sample()), record(bad_clock),
                dict(kind="probe-error", collected_at=sample(120)["utc"]), record(sample(240))]
        result = review([], current=sample(240), records=rows)
        self.assertIn("clock-continuity", result["findings"])
        self.assertIn("probe-error", result["findings"])
        self.assertIn("coverage-gap", result["findings"])
        self.assertFalse(result["coverage"]["complete"])

    def test_stale_snapshot_is_not_fresh_even_when_its_embedded_status_is_healthy(self):
        result = review([sample()], now=START + dt.timedelta(seconds=500))
        self.assertIn("stale-snapshot", result["findings"])
        self.assertTrue(result["decision_required"])

    def test_heartbeat_freshness_includes_snapshot_age(self):
        result = review([sample(heartbeat_age_seconds=80)], now=START + dt.timedelta(seconds=60))
        self.assertIn("status-stale", result["findings"])

    def test_identity_failure_invalidates_coverage(self):
        result = review([sample(), sample(60, revision="other"), sample(120)])
        self.assertFalse(result["coverage"]["complete"])

    def test_partial_window_is_not_a_rate_failure(self):
        current = sample(60, watch_last_6h=dict(full_window_elapsed=False,
                                              base_equivalent_points=10, points=10))
        result = review([sample(), current])
        self.assertEqual(result["watch"]["full_6h_samples"], 0)
        self.assertNotIn("watch-rate-review", result["findings"])

    def test_low_rate_with_unknown_eligibility_requires_review_not_a_normalized_verdict(self):
        current = sample(21600, watch_last_6h=dict(full_window_elapsed=True,
                                                 base_equivalent_points=1200, points=1440))
        result = review([sample(), current])
        self.assertIn("watch-rate-review", result["findings"])
        self.assertEqual(result["watch"]["normalized_verdict"], "not-established")
        self.assertEqual(result["rates_per_hour"]["WATCH"]["raw"], 260)
        self.assertAlmostEqual(result["rates_per_hour"]["WATCH"]["known_award_base"], 216.666667, places=5)

    def test_unknown_awards_are_never_assigned_a_base_rate(self):
        current = sample(21600)
        current["soak_rewards"]["WATCH"]["award_histogram"] = {"13": 130}
        current["soak_rewards"]["WATCH"]["points"] = 1690
        result = review([current])
        self.assertIsNone(result["rates_per_hour"]["WATCH"]["known_award_base"])

    def test_drought_and_task_transport_failures_remain_reviewable(self):
        bad = sample(660, watch_drought_seconds=620,
                     tasks=[dict(name="minute-watch", failures=1, last_error_class="timeout")],
                     eventsub=dict(verified=False, active_subscriptions=1,
                                   planned_subscriptions=2, failed_subscriptions=1))
        result = review([sample(), bad, sample(720)])
        self.assertIn("watch-drought-review", result["findings"])
        self.assertIn("task:minute-watch", result["findings"])
        self.assertIn("eventsub", result["findings"])

    def test_disappearing_drop_is_not_a_claim_and_a_claim_transition_is_counted_once(self):
        drop = dict(drop_key="drop-0123456789abcdef", current_minutes_watched=179,
                    required_minutes_watched=180, is_claimed=False,
                    observed_at_unix=int(START.timestamp()), last_progress_increase_unix=None)
        first = sample(drop_progress=[drop])
        absent = sample(60)
        result = review([first, absent])
        self.assertEqual(result["drops"]["claimed_transitions"], 0)
        self.assertEqual(result["drops"]["unclaimed_disappearances"], 1)
        self.assertEqual(result["drops"]["near_complete_disappearances"], 1)
        claimed = dict(drop, is_claimed=True, current_minutes_watched=180)
        result = review([first, absent, sample(120, drop_progress=[claimed]),
                         sample(180, drop_progress=[claimed])])
        self.assertEqual(result["drops"]["claimed_transitions"], 1)

    def test_reaching_72_hours_requests_review_and_never_accepts_the_candidate(self):
        samples = [sample(n) for n in range(0, 259201, 60)]
        result = review(samples)
        self.assertTrue(result["milestone"]["elapsed"])
        self.assertEqual(result["acceptance"], "manual-review-required")
        self.assertTrue(result["decision_required"])

    def test_finished_collector_does_not_create_an_artificial_gap_before_review(self):
        rows = [record(sample(n)) for n in range(0, 259201, 60)]
        rows[-1]["kind"] = "window-complete-awaiting-review"
        result = review([], records=rows, current=sample(262800))
        self.assertTrue(result["coverage"]["complete"])
        self.assertNotIn("coverage-gap", result["findings"])

    def test_empty_or_malformed_history_cannot_look_complete(self):
        for rows in ([], [{"kind": "invalid-json", "collected_at": None}]):
            with self.subTest(rows=rows):
                result = review([], current=sample(60), records=rows)
                self.assertFalse(result["coverage"]["complete"])
                self.assertTrue(result["decision_required"])

    def test_missing_checkpoint_fields_are_evidence_errors(self):
        damaged = sample(60)
        del damaged["tasks"]
        result = review([sample(), damaged])
        self.assertIn("missing-field:tasks", result["findings"])

    def test_previous_failure_survives_history_regression(self):
        previous = review([sample(), sample(60, restarts=1), sample(120)])
        current = review([sample()])
        result = REVIEW.compare(current, previous)
        self.assertIn("history-regressed", result["findings"])
        self.assertIn("restart", result["findings"])
        self.assertTrue(result["decision_required"])

    def test_health_failure_completed_by_a_review_snapshot_survives_the_next_review(self):
        failing = [sample(n * 60, application_health_exit_code=int(n >= 1)) for n in range(4)]
        previous = review(failing[:3], current=failing[3])
        self.assertEqual(previous["findings"]["application-health"]["level"], "failure")
        result = REVIEW.compare(review(failing[:3] + [sample(180), sample(240)]), previous)
        self.assertEqual(result["findings"]["application-health"]["level"], "failure")
        self.assertFalse(result["coverage"]["complete"])

    def test_prior_evidence_problem_cannot_become_complete_when_its_row_disappears(self):
        previous = review([], current=sample(60), records=[record(sample()),
                          {"kind": "probe-error", "collected_at": sample(30)["utc"]}, record(sample(60))])
        result = REVIEW.compare(review([sample(), sample(60)]), previous)
        self.assertFalse(result["coverage"]["complete"])
        self.assertIn("probe-error", result["findings"])

    def test_unchanged_persistent_condition_does_not_repeat_the_alert(self):
        task = [dict(name="minute-watch", failures=1, last_error_class="timeout")]
        previous = review([sample(), sample(60, tasks=task)])
        result = REVIEW.compare(review([sample(), sample(60, tasks=task), sample(120, tasks=task)]), previous)
        self.assertFalse(result["decision_required"])
        self.assertEqual(result["changes"]["new_or_recurring_findings"], [])

    def test_missing_nested_window_and_reward_fields_cannot_look_complete(self):
        for changes in ({"watch_last_6h": {}}, {"soak_rewards": {}},
                        {"watch_last_6h": dict(full_window_elapsed=False)}):
            with self.subTest(changes=changes):
                result = review([sample(n, **changes) for n in range(0, 21601, 60)])
                self.assertFalse(result["coverage"]["complete"])
                self.assertTrue(result["decision_required"])

    def test_logged_drought_between_samples_requires_review(self):
        result = review([sample(n, max_watch_gap_seconds=650, watch_drought_seconds=595)
                         for n in range(0, 721, 60)])
        self.assertIn("watch-logged-gap-review", result["findings"])
        self.assertTrue(result["decision_required"])

    def test_new_error_counters_request_inspection(self):
        previous = review([sample()])
        result = REVIEW.compare(review([sample(), sample(60, errors=1, watch_timeouts=1,
                                                          warnings={"upstream": 1})]), previous)
        self.assertTrue(result["decision_required"])

    def test_live_only_claim_is_retained_and_does_not_become_a_false_disappearance(self):
        drop = dict(drop_key="drop-0123456789abcdef", current_minutes_watched=179,
                    required_minutes_watched=180, is_claimed=False,
                    observed_at_unix=int(START.timestamp()), last_progress_increase_unix=None)
        baseline = sample(drop_progress=[drop])
        live = sample(30, drop_progress=[dict(drop, current_minutes_watched=180, is_claimed=True)])
        previous = review([baseline], current=live)
        result = REVIEW.compare(review([baseline, sample(60)]), previous)
        self.assertEqual(result["drops"]["claimed_transitions"], 1)
        self.assertEqual(result["changes"]["claimed_transitions"], 0)
        self.assertEqual(result["drops"]["near_complete_disappearances"], 0)

    def test_missing_task_is_detected_against_the_first_checkpoint(self):
        first = sample(tasks=[dict(name="minute-watch", failures=0, last_error_class=None),
                              dict(name="eventsub", failures=0, last_error_class=None)])
        result = review([first, sample(60)])
        self.assertIn("task-set-changed", result["findings"])

    def test_drop_observation_and_progress_ages_remain_available(self):
        drop = dict(drop_key="drop-0123456789abcdef", current_minutes_watched=10,
                    required_minutes_watched=180, is_claimed=False,
                    observed_at_unix=int(START.timestamp()),
                    last_progress_increase_unix=int(START.timestamp()) - 20)
        result = review([sample(60, drop_progress=[drop])])
        current = result["drops"]["current"][0]
        self.assertEqual(current["observation_age_seconds"], 60)
        self.assertIn("progress_age_seconds", current)
        self.assertEqual(current["progress_age_seconds"], 80)

    def test_two_live_only_snapshots_can_establish_a_claim_transition(self):
        drop = dict(drop_key="drop-0123456789abcdef", current_minutes_watched=179,
                    required_minutes_watched=180, is_claimed=False,
                    observed_at_unix=int(START.timestamp()), last_progress_increase_unix=None)
        previous = review([sample()], current=sample(30, drop_progress=[drop]))
        result = REVIEW.compare(review([sample(), sample(60)],
                                      current=sample(90, drop_progress=[dict(drop, is_claimed=True,
                                                                           current_minutes_watched=180)])), previous)
        self.assertEqual(result["drops"]["claimed_transitions"], 1)
        self.assertEqual(result["changes"]["claimed_transitions"], 1)

    def test_later_near_complete_disappearance_is_not_hidden_by_an_earlier_one(self):
        drop = dict(drop_key="drop-0123456789abcdef", current_minutes_watched=20,
                    required_minutes_watched=180, is_claimed=False,
                    observed_at_unix=int(START.timestamp()), last_progress_increase_unix=None)
        rows = [sample(drop_progress=[drop]), sample(60)]
        previous = review(rows)
        rows.extend([sample(120, drop_progress=[dict(drop, current_minutes_watched=179)]), sample(180)])
        result = REVIEW.compare(review(rows), previous)
        self.assertEqual(result["drops"]["near_complete_disappearances"], 1)
        self.assertTrue(result["decision_required"])

    def test_first_live_only_failure_time_cannot_move_forward(self):
        previous = review([sample()], current=sample(30, health="unhealthy"))
        result = REVIEW.compare(review([sample(), sample(60)],
                                      current=sample(90, health="unhealthy")), previous)
        self.assertEqual(result["findings"]["health"]["first_utc"], sample(30)["utc"])


if __name__ == "__main__":
    unittest.main()
