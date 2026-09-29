"""Review sanitized soak evidence without accepting or changing a deployment.

Run locally with --checkpoint, or send this script over SSH with --host and
--checker. SSH mode writes no remote files and reuses the existing collector.
Only compact reviews cross SSH; raw histories stay beside their original anchor.
"""
from __future__ import annotations

import argparse
import collections
import datetime as dt
import json
from pathlib import Path
import re
import shlex
import subprocess
import sys
from typing import Callable, Iterable

IDENTITY = ("image", "revision", "container_started", "runtime_started")
REQUIRED = (*IDENTITY, "utc", "monotonic_seconds", "observation_anchor_utc",
            "soak_elapsed_wall_seconds", "restarts", "health", "status_fresh",
            "heartbeat_age_seconds", "tasks", "eventsub", "pubsub", "watch_slots",
            "soak_rewards", "watch_last_6h", "drop_progress")
UTC = dt.timezone.utc
# Match tm-app/src/status.rs; Docker's retry grace does not waive task health.
MAX_CONSECUTIVE_FAILURES = 5


def timestamp(value: str) -> dt.datetime:
    result = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    if result.tzinfo is None:
        raise ValueError("Timestamp requires an explicit timezone")
    return result


def read_records(path: str) -> Iterable[dict]:
    with open(path, encoding="utf-8-sig") as handle:
        # Bound the read to the initial file size: a collector may still append.
        remaining = Path(path).stat().st_size
        while remaining > 0:
            line = handle.readline()
            if not line:
                break
            remaining -= len(line.encode("utf-8"))
            try:
                row = json.loads(line)
                yield row if isinstance(row, dict) else {"kind": "invalid-json"}
            except ValueError:
                yield {"kind": "invalid-json"}


def analyze(anchor: dict, records: Iterable[dict], current: dict | Callable[[], dict],
            now: dt.datetime | None = None) -> dict:
    start = timestamp(anchor["observation_anchor_utc"])
    for key in (*IDENTITY, "monotonic_seconds"):
        if anchor.get(key) is None:
            raise ValueError("Incomplete observation anchor")
    findings: dict = {}
    active: set[str] = set()
    last: dict = {}
    previous_time = start
    previous_mono = anchor["monotonic_seconds"]
    history_samples = 0
    other_records: collections.Counter = collections.Counter()
    max_gap = max_drift = max_drought = 0.0
    full_windows = 0
    minimum_6h: int | None = None
    drop_ledger: dict = {}
    first_unclaimed: dict = {}
    first_claimed: dict = {}
    disappearance_events: list = []
    baseline_task_names: set | None = None
    selected_set: tuple | None = None
    selection_changes: list[dt.datetime] = []
    present_drops: set[str] = set()
    terminal = None

    def note(code: str, when: str, level: str = "review") -> None:
        item = findings.setdefault(code, dict(first_utc=when, last_utc=when,
                                              samples=0, level=level))
        item["samples"] += 1
        item["last_utc"] = when
        active.add(code)

    def observe(cp: dict, expect_continuous_sample: bool = True) -> None:
        nonlocal last, previous_time, previous_mono, max_gap, max_drift, max_drought
        nonlocal full_windows, minimum_6h, baseline_task_names, selected_set, present_drops
        active.clear()
        when = cp.get("utc", previous_time.isoformat())
        for key in REQUIRED:
            if key not in cp or cp[key] is None:
                note("missing-field:" + key, when, "evidence")
        moment = timestamp(when)
        mono = cp.get("monotonic_seconds", previous_mono)
        elapsed = mono - anchor["monotonic_seconds"]
        gap = (moment - previous_time).total_seconds()
        if expect_continuous_sample:
            max_gap = max(max_gap, gap)
        if (expect_continuous_sample and gap > 90) or gap < 0 or mono < previous_mono:
            note("coverage-gap", when, "evidence")
        drift = max(abs(elapsed - (moment - start).total_seconds()),
                    abs(elapsed - cp.get("soak_elapsed_wall_seconds", elapsed)))
        max_drift = max(max_drift, drift)
        if elapsed < 0 or drift > 2:
            note("clock-continuity", when, "failure")
        for key in IDENTITY:
            if cp.get(key) != anchor[key]:
                note("identity:" + key, when, "failure")
        if cp.get("observation_anchor_utc") != anchor["observation_anchor_utc"]:
            note("identity:observation-anchor", when, "failure")
        if anchor.get("dns_override_matches") is True and cp.get("dns_override_matches") is not True:
            note("identity:dns-override", when, "failure")
        if cp.get("restarts", 0) != 0:
            note("restart", when, "failure")
        if cp.get("health") != "healthy":
            note("health", when, "failure")
        if "application_health_exit_code" in anchor and cp.get("application_health_exit_code") is None:
            note("missing-field:application-health", when, "evidence")
        if cp.get("application_health_exit_code", 0) != 0:
            note("application-health", when, "failure")
        if cp.get("status_fresh") is not True or not 0 <= cp.get("heartbeat_age_seconds", -1) <= 120:
            note("status-stale", when, "failure")
        if not cp.get("tasks"):
            note("task-list-empty", when, "evidence")
        task_names = {t.get("name") for t in cp.get("tasks", [])}
        if baseline_task_names is None and task_names:
            baseline_task_names = task_names
        elif task_names != baseline_task_names:
            note("task-set-changed", when, "evidence")
        for task in cp.get("tasks", []):
            if task.get("failures", 0) or task.get("last_error_class"):
                name = task.get("name", "unknown")
                safe_name = name if re.fullmatch(r"[a-z0-9-]{1,60}", name) else "unknown"
                note("task:" + safe_name, when)
                if task.get("failures", 0) >= MAX_CONSECUTIVE_FAILURES:
                    note("task-unhealthy:" + safe_name, when, "failure")
        es, ps = cp.get("eventsub") or {}, cp.get("pubsub") or {}
        if (es.get("verified") is not True or es.get("failed_subscriptions") != 0
                or es.get("active_subscriptions", 0) < 1
                or es.get("active_subscriptions") != es.get("planned_subscriptions")):
            note("eventsub", when)
        if (ps.get("failed_connections") != 0 or ps.get("total", 0) < 1
                or ps.get("total") != ps.get("acknowledged")):
            note("pubsub", when)
        slots = [s for s in cp.get("watch_slots", []) if s.get("selected") is True]
        if len(slots) > 2:
            note("watch-slot-limit", when, "failure")
        for slot in slots:
            if slot.get("consecutive_failures", 0) >= MAX_CONSECUTIVE_FAILURES:
                number = slot.get("slot")
                safe_slot = str(number) if number in (0, 1) else "unknown"
                note("watch-slot-unhealthy:" + safe_slot, when, "failure")
        selection = tuple(sorted((s.get("channel_key") or "", s.get("broadcast_key") or "") for s in slots))
        if selected_set is not None and selection != selected_set:
            selection_changes.append(moment)
        selected_set = selection
        drought = cp.get("watch_drought_seconds")
        if drought is not None:
            max_drought = max(max_drought, min(drought, max(0, elapsed)))
            if drought >= 600 and elapsed >= 600:
                note("watch-drought-review", when)
        if cp.get("max_watch_gap_seconds") is not None and cp["max_watch_gap_seconds"] >= 600:
            note("watch-logged-gap-review", when)
        window = cp.get("watch_last_6h") or {}
        if not {"full_window_elapsed", "points", "base_equivalent_points"} <= window.keys():
            note("missing-field:watch-window", when, "evidence")
        if window.get("full_window_elapsed") is not (cp.get("soak_elapsed_wall_seconds", 0) >= 21600):
            note("watch-window-duration", when, "evidence")
        for reason in ("WATCH", "CLAIM"):
            reward = cp.get("soak_rewards", {}).get(reason) or {}
            if not {"events", "points", "award_histogram"} <= reward.keys():
                note("missing-field:reward-" + reason, when, "evidence")
        if window.get("full_window_elapsed") is True and elapsed >= 21600:
            full_windows += 1
            value = window.get("base_equivalent_points")
            if value is None:
                note("watch-rate-unknown", when, "evidence")
            else:
                minimum_6h = value if minimum_6h is None else min(minimum_6h, value)
                if value < 1224:
                    # Selected slots do not establish historical eligibility.
                    note("watch-rate-review", when)
        drops = {d["drop_key"]: d for d in cp.get("drop_progress", [])
                 if re.fullmatch(r"drop-[0-9a-f]{16}", d.get("drop_key") or "")}
        for key in present_drops - drops.keys():
            old = drop_ledger[key]
            if not old.get("is_claimed"):
                disappearance_events.append(dict(key=key, utc=when, minutes=old.get("current_minutes_watched"),
                                                  required=old.get("required_minutes_watched")))
        for key, value in drops.items():
            if value.get("is_claimed") is True:
                first_claimed.setdefault(key, when)
            elif value.get("is_claimed") is False:
                first_unclaimed.setdefault(key, when)
            drop_ledger[key] = value
        present_drops = set(drops)
        last = cp
        previous_time, previous_mono = moment, mono

    for row in records:
        kind = row.get("kind")
        if kind in ("continuity-failure", "window-complete-awaiting-review",
                    "collector-ended-without-final-checkpoint"):
            terminal = kind
        if kind not in ("checkpoint", "window-complete-awaiting-review"):
            other_records[str(kind)] += 1
            note(str(kind), row.get("collected_at") or previous_time.isoformat(),
                 "failure" if kind == "continuity-failure" else "evidence")
        if isinstance(row.get("checkpoint"), dict):
            history_samples += 1
            observe(row["checkpoint"])
    completed_collection = (terminal == "window-complete-awaiting-review"
                            and last.get("monotonic_seconds", 0) - anchor["monotonic_seconds"] >= 259200)
    snapshot = current() if callable(current) else current
    if snapshot != last:
        observe(snapshot, expect_continuous_sample=not completed_collection)
    now = now or dt.datetime.now(UTC)
    age = (now - timestamp(last["utc"])).total_seconds()
    if age < -2 or age > 120:
        note("stale-snapshot", last["utc"], "evidence")
    if age + last.get("heartbeat_age_seconds", 121) > 120:
        note("status-stale", last["utc"], "failure")
    if not history_samples:
        note("history-empty", last["utc"], "evidence")
    for key, item in findings.items():
        item["active"] = key in active
    elapsed = last["monotonic_seconds"] - anchor["monotonic_seconds"]
    hours = elapsed / 3600
    rates = {}
    for reason, base in (("WATCH", 10), ("CLAIM", 50)):
        reward = last.get("soak_rewards", {}).get(reason, {})
        histogram = reward.get("award_histogram")
        known = (isinstance(histogram, dict)
                 and all(float(v) in (base, base * 1.2, base * 1.4, base * 2) for v in histogram)
                 and sum(histogram.values()) == reward.get("events")
                 and sum(float(v) * n for v, n in histogram.items()) == reward.get("points"))
        rates[reason] = dict(raw=reward.get("points", 0) / hours if hours > 0 else None,
                             known_award_base=base * reward["events"] / hours if known and hours > 0 else None)
    drops_now = []
    for key in sorted(present_drops):
        drop = drop_ledger[key]
        drops_now.append(dict(key=key, minutes=drop.get("current_minutes_watched"),
                              required=drop.get("required_minutes_watched"), claimed=drop.get("is_claimed"),
                              progress_age_seconds=(timestamp(last["utc"]).timestamp() - drop["last_progress_increase_unix"])
                              if drop.get("last_progress_increase_unix") else None,
                              observation_age_seconds=(timestamp(last["utc"]).timestamp() - drop["observed_at_unix"])
                              if drop.get("observed_at_unix") else None))
    complete = history_samples > 0 and not any(v["level"] in ("evidence", "failure") for v in findings.values())
    report = dict(
        utc=last["utc"], identity={k: anchor[k] for k in (*IDENTITY, "observation_anchor_utc", "monotonic_seconds")},
        acceptance="manual-review-required", decision_required=bool(findings) or elapsed >= 259200,
        coverage=dict(history_samples=history_samples, other_records=dict(other_records), complete=complete,
                      max_gap_seconds=max_gap, max_clock_drift_seconds=max_drift,
                      snapshot_age_seconds=age, collector_terminal=terminal),
        milestone=dict(elapsed=elapsed >= 259200, hours=hours, remaining_seconds=max(0, 259200 - elapsed),
                       earliest_review_utc=(start + dt.timedelta(seconds=259200)).isoformat()),
        findings=findings, rates_per_hour=rates,
        watch=dict(full_6h_samples=full_windows, minimum_full_6h_base_points=minimum_6h,
                   latest_6h=last.get("watch_last_6h"), threshold_6h_base_points=1224,
                   max_sampled_drought_seconds=max_drought,
                   max_logged_gap_seconds=last.get("max_watch_gap_seconds"),
                   normalized_verdict="not-established", selection_changes=len(selection_changes),
                   selection_changes_last_hour=sum((timestamp(last["utc"]) - t).total_seconds() <= 3600
                                                   for t in selection_changes),
                   slots=[{k: s.get(k) for k in ("slot", "selected", "progress", "progress_age_seconds",
                                                "last_accepted_watch_unix", "last_server_confirmed_points_unix",
                                                "consecutive_failures")} for s in last.get("watch_slots", [])]),
        drops=dict(first_unclaimed=first_unclaimed, first_claimed=first_claimed,
                   disappearance_events=disappearance_events, current=drops_now),
        health=dict({k: last.get(k) for k in ("health", "application_health_exit_code", "status_fresh", "heartbeat_age_seconds", "eventsub", "pubsub")},
                    task_count=len(last.get("tasks", [])), expected_task_count=len(baseline_task_names or set())),
        counters=dict(warnings=sum(last.get("warnings", {}).values()), errors=last.get("errors"),
                      watch_timeouts=last.get("watch_timeouts"),
                      rewards={k: {n: v.get(n) for n in ("events", "points")}
                               for k, v in last.get("soak_rewards", {}).items()}),
        limits=["Eligibility and subscriber multipliers are not established for every historical minute.",
                "Selected slots and accepted watch requests do not prove server-confirmed credit or Drops eligibility.",
                "Disappearing rewards do not prove a claim; campaign coverage and predictions need separate review.",
                "Logged maximum gaps may include pre-anchor events; establish timing and eligibility before judging failure."])
    summarize_drops(report["drops"])
    return report


def summarize_drops(drops: dict) -> None:
    claims = {k: when for k, when in drops["first_claimed"].items()
              if k in drops["first_unclaimed"] and timestamp(when) > timestamp(drops["first_unclaimed"][k])}
    drops["claim_events"] = claims
    removed = [v for v in drops["disappearance_events"]
               if v["key"] not in claims or timestamp(claims[v["key"]]) > timestamp(v["utc"])]
    drops["claimed_transitions"] = len(claims)
    drops["unclaimed_disappearances"] = len(removed)
    drops["near_complete_disappearances"] = sum(
        (v["required"] or 0) > 0 and (v["minutes"] or 0) >= v["required"] - 1 for v in removed)


def compare(report: dict, previous: dict | None) -> dict:
    if previous is None:
        report["changes"] = {"baseline": True}
        return report
    if report["identity"] != previous["identity"]:
        raise ValueError("Output directory belongs to a different observation anchor")
    new_samples = report["coverage"]["history_samples"] - previous["coverage"]["history_samples"]
    if new_samples < 0 or timestamp(report["utc"]) < timestamp(previous["utc"]):
        report["findings"]["history-regressed"] = dict(first_utc=report["utc"], last_utc=report["utc"],
                                                       samples=1, level="evidence", active=True)
        report["coverage"]["complete"] = False
    before = previous.get("findings", {})
    new = [k for k, v in report["findings"].items()
           if k not in before or (not before[k].get("active")
                                  and timestamp(v["last_utc"]) > timestamp(before[k]["last_utc"]))]
    recovered = [k for k, v in before.items() if v.get("active")
                 and not report["findings"].get(k, {}).get("active")]
    for key, finding in before.items():
        if key not in report["findings"]:
            report["findings"][key] = dict(finding, active=False)
        else:
            current = report["findings"][key]
            current["first_utc"] = min(current["first_utc"], finding["first_utc"], key=timestamp)
            current["last_utc"] = max(current["last_utc"], finding["last_utc"], key=timestamp)
            current["samples"] = max(current["samples"], finding["samples"])
    report["coverage"]["complete"] = report["coverage"]["complete"] and not any(
        v["level"] in ("evidence", "failure") for v in report["findings"].values())
    for field in ("first_unclaimed", "first_claimed"):
        for key, when in previous["drops"][field].items():
            existing = report["drops"][field].get(key)
            if existing is None or timestamp(when) < timestamp(existing):
                report["drops"][field][key] = when
    event_ids = {(v["key"], v["utc"]) for v in report["drops"]["disappearance_events"]}
    report["drops"]["disappearance_events"].extend(
        v for v in previous["drops"]["disappearance_events"] if (v["key"], v["utc"]) not in event_ids)
    summarize_drops(report["drops"])
    changes = dict(since_utc=previous["utc"], new_history_samples=new_samples,
                   new_or_recurring_findings=new, recovered_conditions=recovered)
    for key in ("claimed_transitions", "unclaimed_disappearances", "near_complete_disappearances"):
        changes[key] = report["drops"][key] - previous["drops"][key]
    prior_drops = {d["key"]: d for d in previous["drops"]["current"]}
    changes["drop_progress"] = [dict(key=d["key"], before=prior_drops[d["key"]]["minutes"], after=d["minutes"])
                                for d in report["drops"]["current"] if d["key"] in prior_drops
                                and d["minutes"] != prior_drops[d["key"]]["minutes"]]
    for key in ("warnings", "errors", "watch_timeouts"):
        old, current = previous["counters"].get(key), report["counters"].get(key)
        changes[key] = current - old if current is not None and old is not None else None
    report["changes"] = changes
    counter_change = any(changes[key] not in (None, 0) for key in ("warnings", "errors", "watch_timeouts"))
    report["decision_required"] = bool(new or recovered or counter_change or changes["near_complete_disappearances"] > 0
                                        or (report["milestone"]["elapsed"] and not previous["milestone"]["elapsed"]))
    return report


def command_json(command: list[str], source: str | None = None) -> dict:
    result = subprocess.run(command, input=source, text=True, encoding="utf-8", capture_output=True, timeout=180)
    if result.returncode:
        # A checker/SSH diagnostic can contain raw operational data. Never echo it.
        raise RuntimeError(f"Evidence command failed with exit {result.returncode}")
    return json.loads(result.stdout)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--anchor", required=True)
    parser.add_argument("--history", required=True)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--checkpoint")
    source.add_argument("--checker")
    parser.add_argument("--host", help="Existing SSH alias; Python 3 is required on the remote host")
    parser.add_argument("--container", help="Optional container for bounded docker stats")
    parser.add_argument("--output-dir", help="Local directory for timestamped reviews and latest.json")
    parser.add_argument("--now", help="Explicit timestamp for replaying retained evidence")
    args = parser.parse_args()
    if args.host:
        remote = ["python3", "-", "--anchor", args.anchor, "--history", args.history]
        for key in ("checkpoint", "checker", "container", "now"):
            if getattr(args, key):
                remote.extend(["--" + key, getattr(args, key)])
        report = command_json(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", args.host,
                               shlex.join(remote)], Path(__file__).read_text(encoding="utf-8"))
    else:
        anchor = json.loads(Path(args.anchor).read_text(encoding="utf-8-sig"))
        def checkpoint() -> dict:
            if args.checkpoint:
                return json.loads(Path(args.checkpoint).read_text(encoding="utf-8-sig"))
            return command_json([sys.executable, args.checker, anchor["observation_anchor_utc"]])
        report = analyze(anchor, read_records(args.history), checkpoint, timestamp(args.now) if args.now else None)
        if args.container:
            stats = command_json(["docker", "stats", "--no-stream", "--format", "json", args.container])
            report["resources"] = {k: stats.get(k) for k in ("CPUPerc", "MemUsage", "PIDs")}
    previous = None
    if args.output_dir:
        directory = Path(args.output_dir)
        latest = directory / "latest.json"
        if latest.exists():
            previous = json.loads(latest.read_text(encoding="utf-8"))
        report = compare(report, previous)
        directory.mkdir(parents=True, exist_ok=True)
        content = json.dumps(report, indent=2, sort_keys=True) + "\n"
        name = "review-" + dt.datetime.now(UTC).strftime("%Y%m%dT%H%M%S.%fZ") + ".json"
        (directory / name).write_text(content, encoding="utf-8")
        pending = directory / "latest.pending"
        pending.write_text(content, encoding="utf-8")
        pending.replace(latest)
    if args.output_dir:
        # Retain event detail for subsequent comparisons without replaying it
        # into the model on every heartbeat. SSH transport still carries it.
        report["drops"] = {k: v for k, v in report["drops"].items()
                           if k not in ("first_unclaimed", "first_claimed", "claim_events", "disappearance_events")}
    print(json.dumps(report, separators=(",", ":")))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(json.dumps({"error": type(error).__name__, "evidence_status": "unknown",
                          "action": "Review evidence acquisition; prior reviews were not replaced."}), file=sys.stderr)
        sys.exit(2)
