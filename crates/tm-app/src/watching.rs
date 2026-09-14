use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant as StdInstant};

use tm_runtime::RuntimeTime;

const MINUTE_WATCHER_RESUME_GAP: i64 = 10 * 60;
const MAX_CONCURRENT_WATCHERS: usize = 2;
const WATCH_ROTATION_SECONDS: i64 = 15 * 60;
// Streak promotions defer the fair rotation clock, so cap how long a channel at
// the front of the queue can hold its slot before fair rotation wins outright.
const MAX_ROTATION_DEFERRAL_SECONDS: i64 = 2 * WATCH_ROTATION_SECONDS;

#[derive(Debug, Clone)]
pub(crate) struct CachedSpadeUrl {
    pub(crate) url: String,
    pub(crate) fetched_at: StdInstant,
}

#[derive(Debug, Clone)]
pub(crate) enum SpadeCacheEntry {
    Ready(CachedSpadeUrl),
}

#[derive(Debug, Default)]
pub(crate) struct WatchRotation {
    queue: VecDeque<String>,
    pinned_campaign: Option<String>,
    spare_since: Option<RuntimeTime>,
    promoted_streak_broadcasts: HashMap<String, String>,
    last_voluntary_switch: Option<RuntimeTime>,
    last_fair_rotation: Option<RuntimeTime>,
    selection_reasons: HashMap<String, &'static str>,
    watchdog_rotation_pending: bool,
    fair_hold: Option<(RuntimeTime, Vec<(String, RuntimeTime)>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreakCandidate {
    pub(crate) login: String,
    pub(crate) broadcast_id: String,
}

impl WatchRotation {
    #[cfg(test)]
    fn select_with_campaigns(
        &mut self,
        eligible: &[String],
        campaigns: &[String],
        streaks: &[StreakCandidate],
        now: RuntimeTime,
    ) -> Vec<String> {
        self.select_with_progress(eligible, campaigns, streaks, &HashMap::new(), now)
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn select_with_progress(
        &mut self,
        ordered_eligible: &[String],
        campaign_logins: &[String],
        streak_candidates: &[StreakCandidate],
        credit_times: &HashMap<String, RuntimeTime>,
        now: RuntimeTime,
    ) -> Vec<String> {
        let pinned_campaign = campaign_logins
            .iter()
            .find(|login| ordered_eligible.iter().any(|eligible| eligible == *login))
            .cloned();
        let campaign_changed = self.pinned_campaign != pinned_campaign;
        if campaign_changed {
            self.fair_hold = None;
        }
        self.pinned_campaign = pinned_campaign;
        // One promotion per broadcast. A record is released only when the
        // channel reports a different broadcast, never when it drops out of the
        // eligible set for a pass, so a suspension or presence blip cannot hand
        // the same broadcast a second jump. Keys come from configured logins, so
        // the map stays bounded by the configured channel count.
        for candidate in streak_candidates {
            if self
                .promoted_streak_broadcasts
                .get(&candidate.login)
                .is_some_and(|promoted| promoted != &candidate.broadcast_id)
            {
                self.promoted_streak_broadcasts.remove(&candidate.login);
            }
        }

        // Twitch advances Drop progress on only one channel. Pin the first
        // ranked campaign and prefer non-campaign channels for the spare slot.
        // When every eligible channel is a campaign channel, the eligible
        // channels other than the pin are the real spare pool. Reconcile the
        // queue against that pool instead of rebuilding it on every pass.
        let mut spare_candidates = ordered_eligible
            .iter()
            .filter(|login| !campaign_logins.contains(login))
            .cloned()
            .collect::<Vec<_>>();
        if spare_candidates.is_empty() {
            spare_candidates = ordered_eligible
                .iter()
                .filter(|login| Some(*login) != self.pinned_campaign.as_ref())
                .cloned()
                .collect();
        }
        self.queue
            .retain(|login| spare_candidates.iter().any(|candidate| candidate == login));
        for login in spare_candidates {
            if !self.queue.iter().any(|queued| queued == &login) {
                self.queue.push_back(login);
            }
        }

        if self.queue.is_empty() {
            self.spare_since = None;
            self.last_fair_rotation = None;
            self.last_voluntary_switch = None;
            self.selection_reasons.clear();
            self.watchdog_rotation_pending = false;
            self.fair_hold = None;
            if let Some(pinned) = &self.pinned_campaign {
                self.selection_reasons
                    .insert(pinned.clone(), "campaign-priority");
            }
            return self.pinned_campaign.iter().cloned().collect();
        }

        if self.spare_since.is_none() {
            self.spare_since = Some(now);
        }
        if self.last_fair_rotation.is_none() {
            self.last_fair_rotation = Some(now);
        }
        if self.watchdog_rotation_pending {
            // The watchdog already moved the queue. Give its replacement a
            // turn instead of rotating again and reselecting the stalled slot.
            self.spare_since = Some(now);
            self.last_voluntary_switch = Some(now);
            self.fair_hold = None;
        }

        let rotating_slots = MAX_CONCURRENT_WATCHERS - usize::from(self.pinned_campaign.is_some());
        let fair_rotation_overdue = self
            .last_fair_rotation
            .is_some_and(|last| (now - last).whole_seconds() >= MAX_ROTATION_DEFERRAL_SECONDS);
        let promotion_allowed = !fair_rotation_overdue
            && self
                .last_voluntary_switch
                .is_none_or(|last| (now - last).whole_seconds() >= WATCH_ROTATION_SECONDS);
        let promotion = promotion_allowed.then(|| {
            streak_candidates.iter().find_map(|candidate| {
                let position = self
                    .queue
                    .iter()
                    .position(|login| login == &candidate.login)?;
                let already_promoted = self.promoted_streak_broadcasts.get(&candidate.login)
                    == Some(&candidate.broadcast_id);
                (position >= rotating_slots && !already_promoted)
                    .then(|| (position, candidate.clone()))
            })
        });
        let mut fair_rotated = false;
        if let Some(Some((position, candidate))) = promotion {
            if let Some(login) = self.queue.remove(position) {
                self.queue.push_front(login);
                self.promoted_streak_broadcasts
                    .insert(candidate.login, candidate.broadcast_id);
                self.last_voluntary_switch = Some(now);
                self.spare_since = Some(now);
                self.fair_hold = None;
            }
        } else if self.queue.len() > rotating_slots
            && self
                .spare_since
                .is_some_and(|started| (now - started).whole_seconds() >= WATCH_ROTATION_SECONDS)
        {
            let outgoing = self
                .queue
                .iter()
                .take(rotating_slots)
                .filter(|login| {
                    !self
                        .queue
                        .iter()
                        .cycle()
                        .skip(rotating_slots)
                        .take(rotating_slots)
                        .any(|next| next == *login)
                })
                .cloned()
                .collect::<Vec<_>>();
            if !self.hold_for_credit(
                &outgoing,
                credit_times,
                now,
                fair_rotation_overdue || campaign_changed,
            ) {
                for _ in 0..rotating_slots {
                    if let Some(login) = self.queue.pop_front() {
                        self.queue.push_back(login);
                    }
                }
                self.spare_since = Some(now);
                self.last_fair_rotation = Some(now);
                // Give the fair-selected channels their turn before a promotion
                // can replace them and interrupt newly started credit progress.
                self.last_voluntary_switch = Some(now);
                fair_rotated = true;
            }
        } else {
            self.fair_hold = None;
        }

        let selected = self
            .pinned_campaign
            .iter()
            .cloned()
            .chain(self.queue.iter().take(rotating_slots).cloned())
            .collect::<Vec<_>>();
        self.selection_reasons.clear();
        for login in &selected {
            let reason = if self.pinned_campaign.as_ref() == Some(login) {
                "campaign-priority"
            } else if !fair_rotated
                && self
                    .last_voluntary_switch
                    .is_some_and(|promoted_at| promoted_at == now)
                && streak_candidates.iter().any(|candidate| {
                    candidate.login == *login
                        && self.promoted_streak_broadcasts.get(&candidate.login)
                            == Some(&candidate.broadcast_id)
                })
            {
                "streak-promotion"
            } else if self.watchdog_rotation_pending {
                "watchdog-switch"
            } else if fair_rotated {
                "fair-rotation"
            } else {
                "watch-order"
            };
            self.selection_reasons.insert(login.clone(), reason);
        }
        self.watchdog_rotation_pending = false;
        for candidate in streak_candidates
            .iter()
            .filter(|candidate| selected.contains(&candidate.login))
        {
            self.promoted_streak_broadcasts
                .insert(candidate.login.clone(), candidate.broadcast_id.clone());
        }
        selected
    }

    fn hold_for_credit(
        &mut self,
        outgoing: &[String],
        credits: &HashMap<String, RuntimeTime>,
        now: RuntimeTime,
        bypass: bool,
    ) -> bool {
        if bypass || outgoing.iter().any(|login| !credits.contains_key(login)) {
            self.fair_hold = None;
            return false;
        }
        if self.fair_hold.as_ref().is_some_and(|(_, prior)| {
            prior.len() != outgoing.len()
                || prior.iter().any(|(login, _)| !outgoing.contains(login))
        }) {
            self.fair_hold = None;
            return false;
        }
        // A recent observed watch reward can justify a short wait, never an
        // assumption that the server will award the next one on schedule.
        if self.fair_hold.is_none()
            && outgoing.iter().all(|login| {
                credits
                    .get(login)
                    .is_some_and(|last| (210..300).contains(&(now - *last).whole_seconds()))
            })
        {
            self.fair_hold = Some((
                now,
                outgoing
                    .iter()
                    .filter_map(|login| credits.get(login).map(|last| (login.clone(), *last)))
                    .collect(),
            ));
        }
        let hold = self.fair_hold.as_ref().is_some_and(|(started, prior)| {
            (0..120).contains(&(now - *started).whole_seconds())
                && !prior
                    .iter()
                    .all(|(login, last)| credits.get(login).is_some_and(|current| current > last))
        });
        if !hold {
            self.fair_hold = None;
        }
        hold
    }

    pub(crate) fn selection_reason(&self, login: &str) -> &'static str {
        self.selection_reasons
            .get(login)
            .copied()
            .unwrap_or("watch-order")
    }

    pub(crate) fn defer_stalled(&mut self, login: &str) -> bool {
        if self.pinned_campaign.as_deref() == Some(login) {
            return false;
        }
        let rotating_slots = MAX_CONCURRENT_WATCHERS - usize::from(self.pinned_campaign.is_some());
        if self.queue.len() <= rotating_slots {
            return false;
        }
        let Some(position) = self.queue.iter().position(|queued| queued == login) else {
            return false;
        };
        let Some(login) = self.queue.remove(position) else {
            return false;
        };
        self.queue.push_back(login);
        self.watchdog_rotation_pending = true;
        true
    }
}

pub(crate) fn minute_watcher_resume_gap(
    previous: RuntimeTime,
    current: RuntimeTime,
) -> Option<Duration> {
    let gap = (current - previous).whole_seconds();
    (gap >= MINUTE_WATCHER_RESUME_GAP).then(|| Duration::from_secs(gap.cast_unsigned()))
}

#[cfg(test)]
mod tests {
    use super::{StreakCandidate, WatchRotation};
    use tm_runtime::RuntimeTime;

    fn ts(seconds: u64) -> RuntimeTime {
        RuntimeTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds)
    }

    fn logins(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn streak(login: &str, broadcast_id: &str) -> StreakCandidate {
        StreakCandidate {
            login: login.to_owned(),
            broadcast_id: broadcast_id.to_owned(),
        }
    }

    #[test]
    fn fair_rotation_waits_only_for_nearby_credit_with_a_fixed_deadline() {
        use std::collections::HashMap;
        let eligible = logins(&["alpha", "bravo", "charlie", "delta"]);
        let campaigns = logins(&["alpha"]);
        for (release, credit) in [(960, Some(950)), (1_020, None)] {
            let mut rotation = WatchRotation::default();
            let mut credits = HashMap::from([(String::from("bravo"), ts(660))]);
            let streaks = [streak("bravo", "broadcast-b")];
            rotation.select_with_progress(&eligible, &campaigns, &streaks, &credits, ts(0));
            for now in [900, 920] {
                assert_eq!(
                    rotation.select_with_progress(
                        &eligible,
                        &campaigns,
                        &streaks,
                        &credits,
                        ts(now)
                    ),
                    logins(&["alpha", "bravo"])
                );
            }
            if let Some(credit) = credit {
                credits.insert(String::from("bravo"), ts(credit));
            }
            assert_eq!(
                rotation.select_with_progress(&eligible, &campaigns, &[], &credits, ts(release)),
                logins(&["alpha", "charlie"])
            );
            assert_eq!(rotation.selection_reason("charlie"), "fair-rotation");
            assert!(rotation.fair_hold.is_none());
        }
        for credit in [None, Some(100), Some(900), Some(901)] {
            let mut rotation = WatchRotation::default();
            let credits = credit
                .map(|at| (String::from("bravo"), ts(at)))
                .into_iter()
                .collect();
            rotation.select_with_progress(&eligible, &campaigns, &[], &credits, ts(0));
            assert_eq!(
                rotation.select_with_progress(&eligible, &campaigns, &[], &credits, ts(900)),
                logins(&["alpha", "charlie"])
            );
        }
    }

    #[test]
    fn credit_wait_never_delays_forced_changes_or_fairness_ceiling() {
        use std::collections::HashMap;
        let eligible = logins(&["alpha", "bravo", "charlie", "delta"]);
        let campaigns = logins(&["alpha"]);
        for case in 0..6 {
            let mut rotation = WatchRotation::default();
            let mut credits = HashMap::from([(String::from("bravo"), ts(660))]);
            rotation.select_with_progress(&eligible, &campaigns, &[], &credits, ts(0));
            rotation.select_with_progress(&eligible, &campaigns, &[], &credits, ts(900));
            let mut eligible = eligible.clone();
            let mut campaigns = campaigns.clone();
            let mut streaks = Vec::new();
            match case {
                0 => credits.clear(), // stale metadata, transport or request failure
                1 => eligible.retain(|login| login != "bravo"),
                2 => {
                    assert!(rotation.defer_stalled("bravo"));
                }
                3 => campaigns = logins(&["charlie"]),
                4 => streaks.push(streak("delta", "broadcast-d")),
                _ => {}
            }
            let now = if case == 5 { 1_800 } else { 920 };
            let selected =
                rotation.select_with_progress(&eligible, &campaigns, &streaks, &credits, ts(now));
            assert_ne!(selected, logins(&["alpha", "bravo"]), "case {case}");
            assert!(rotation.fair_hold.is_none(), "case {case}");
        }
    }

    #[test]
    fn two_rotating_slots_wait_for_both_credits() {
        use std::collections::HashMap;
        let eligible = logins(&["alpha", "bravo", "charlie", "delta"]);
        let mut rotation = WatchRotation::default();
        let mut credits = HashMap::from([
            (String::from("alpha"), ts(660)),
            (String::from("bravo"), ts(670)),
        ]);
        rotation.select_with_progress(&eligible, &[], &[], &credits, ts(0));
        assert_eq!(
            rotation.select_with_progress(&eligible, &[], &[], &credits, ts(900)),
            logins(&["alpha", "bravo"])
        );
        credits.insert(String::from("alpha"), ts(950));
        assert_eq!(
            rotation.select_with_progress(&eligible, &[], &[], &credits, ts(960)),
            logins(&["alpha", "bravo"])
        );
        credits.insert(String::from("bravo"), ts(970));
        assert_eq!(
            rotation.select_with_progress(&eligible, &[], &[], &credits, ts(980)),
            logins(&["charlie", "delta"])
        );
    }

    #[test]
    fn credit_wait_ignores_the_channel_retained_by_a_three_channel_rotation() {
        let eligible = logins(&["alpha", "bravo", "charlie"]);
        let mut rotation = WatchRotation::default();
        let credits = std::collections::HashMap::from([(String::from("bravo"), ts(660))]);
        rotation.select_with_progress(&eligible, &[], &[], &credits, ts(0));
        assert_eq!(
            rotation.select_with_progress(&eligible, &[], &[], &credits, ts(900)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_progress(&eligible, &[], &[], &credits, ts(1_020)),
            logins(&["charlie", "alpha"])
        );
    }

    #[test]
    fn rotates_two_creditable_slots_every_fifteen_minutes() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta", "echo"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(899)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(900)),
            logins(&["charlie", "delta"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(1_800)),
            logins(&["echo", "alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(2_700)),
            logins(&["bravo", "charlie"])
        );
    }

    #[test]
    fn fair_rotation_gets_a_full_turn_before_another_streak_promotion() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta"]);
        let campaigns = logins(&["alpha"]);
        let mut rotation = WatchRotation::default();
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &campaigns, &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &campaigns, &[], ts(900)),
            logins(&["alpha", "charlie"])
        );
        assert_eq!(rotation.selection_reason("charlie"), "fair-rotation");
        let candidates = [streak("delta", "broadcast-d")];
        for seconds in [920, 1_799] {
            assert_eq!(
                rotation.select_with_campaigns(&eligible, &campaigns, &candidates, ts(seconds)),
                logins(&["alpha", "charlie"])
            );
        }
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &campaigns, &candidates, ts(1_800)),
            logins(&["alpha", "delta"])
        );
        assert_eq!(rotation.selection_reason("delta"), "streak-promotion");
    }

    #[test]
    fn removes_ineligible_channels_and_refills_without_waiting() {
        let mut rotation = WatchRotation::default();
        assert_eq!(
            rotation.select_with_campaigns(
                &logins(&["alpha", "bravo", "charlie"]),
                &[],
                &[],
                ts(0),
            ),
            logins(&["alpha", "bravo"])
        );

        assert_eq!(
            rotation.select_with_campaigns(&logins(&["bravo", "charlie"]), &[], &[], ts(30),),
            logins(&["bravo", "charlie"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &logins(&["bravo", "charlie", "delta"]),
                &[],
                &[],
                ts(899),
            ),
            logins(&["bravo", "charlie"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &logins(&["bravo", "charlie", "delta"]),
                &[],
                &[],
                ts(900),
            ),
            logins(&["delta", "bravo"])
        );
    }

    #[test]
    fn returns_every_available_channel_when_at_or_below_the_limit() {
        let mut rotation = WatchRotation::default();
        assert_eq!(
            rotation.select_with_campaigns(&logins(&["alpha"]), &[], &[], ts(0)),
            logins(&["alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&logins(&["alpha", "bravo"]), &[], &[], ts(1)),
            logins(&["alpha", "bravo"])
        );
        assert!(rotation
            .select_with_campaigns(&[], &[], &[], ts(2))
            .is_empty());
    }

    #[test]
    fn campaign_preempts_immediately_and_keeps_the_other_slot_rotating() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta", "echo"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["delta"]), &[], ts(100)),
            logins(&["delta", "alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["delta"]), &[], ts(999)),
            logins(&["delta", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["delta"]), &[], ts(1_000)),
            logins(&["delta", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(1_001)),
            logins(&["bravo", "charlie"])
        );
    }

    #[test]
    fn campaign_change_preempts_the_previous_campaign() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["delta", "charlie"]), &[], ts(0),),
            logins(&["delta", "alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["charlie"]), &[], ts(1)),
            logins(&["charlie", "alpha"])
        );
    }

    #[test]
    fn competing_campaigns_fill_and_rotate_the_spare_slot() {
        let eligible = logins(&["alpha", "bravo", "charlie"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&eligible, &eligible, &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &eligible, &[], ts(900)),
            logins(&["alpha", "charlie"])
        );
    }

    #[test]
    fn campaign_fallback_keeps_rotated_order_between_polls() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&eligible, &eligible, &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &eligible, &[], ts(900)),
            logins(&["alpha", "charlie"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &eligible, &[], ts(920)),
            logins(&["alpha", "charlie"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &eligible, &[], ts(1_800)),
            logins(&["alpha", "delta"])
        );
    }

    #[test]
    fn campaign_pin_changes_do_not_reset_spare_fairness() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["alpha"]), &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["bravo"]), &[], ts(600)),
            logins(&["bravo", "charlie"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["alpha"]), &[], ts(1_200)),
            logins(&["alpha", "delta"])
        );
    }

    #[test]
    fn all_campaign_spares_eventually_receive_service() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta", "echo"]);
        let mut rotation = WatchRotation::default();
        let mut observed = std::collections::HashSet::new();

        for seconds in (0..=9_000).step_by(20) {
            let selected = rotation.select_with_campaigns(&eligible, &eligible, &[], ts(seconds));
            observed.extend(selected.into_iter().skip(1));
        }

        assert_eq!(
            observed,
            logins(&["bravo", "charlie", "delta", "echo"])
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn watchdog_replacement_gets_a_turn_even_when_fair_rotation_is_due() {
        for campaigns in [Vec::new(), logins(&["alpha"])] {
            let eligible = logins(&["alpha", "bravo", "charlie"]);
            let mut rotation = WatchRotation::default();
            rotation.select_with_campaigns(&eligible, &campaigns, &[], ts(0));
            assert!(rotation.defer_stalled("bravo"));
            for seconds in [900, 920, 1_799] {
                let selected =
                    rotation.select_with_campaigns(&eligible, &campaigns, &[], ts(seconds));
                assert!(selected.contains(&String::from("charlie")));
                assert!(!selected.contains(&String::from("bravo")));
            }
            assert!(rotation
                .select_with_campaigns(&eligible, &campaigns, &[], ts(1_800))
                .contains(&String::from("bravo")));
        }
    }

    #[test]
    fn watchdog_can_defer_one_stalled_spare_without_changing_campaign_priority() {
        let eligible = logins(&["alpha", "bravo", "charlie"]);
        let mut rotation = WatchRotation::default();
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert!(rotation.defer_stalled("alpha"));
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(1)),
            logins(&["bravo", "charlie"])
        );
        assert_eq!(rotation.selection_reason("charlie"), "watchdog-switch");

        let mut campaign_rotation = WatchRotation::default();
        assert_eq!(
            campaign_rotation.select_with_campaigns(&eligible, &logins(&["alpha"]), &[], ts(0),),
            logins(&["alpha", "bravo"])
        );
        assert!(!campaign_rotation.defer_stalled("alpha"));
    }

    #[test]
    fn newly_available_spare_slot_starts_at_the_front_of_the_queue() {
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&logins(&["delta"]), &logins(&["delta"]), &[], ts(0),),
            logins(&["delta"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &logins(&["alpha", "bravo", "delta"]),
                &logins(&["delta"]),
                &[],
                ts(1_000),
            ),
            logins(&["delta", "alpha"])
        );
    }

    #[test]
    fn streak_candidates_get_one_bounded_jump_without_starving_rotation() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &eligible,
                &[],
                &[streak("charlie", "broadcast-c")],
                ts(100),
            ),
            logins(&["charlie", "alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &eligible,
                &[],
                &[
                    streak("charlie", "broadcast-c"),
                    streak("bravo", "broadcast-b"),
                ],
                ts(101),
            ),
            logins(&["charlie", "alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &eligible,
                &[],
                &[
                    streak("charlie", "broadcast-c"),
                    streak("bravo", "broadcast-b"),
                ],
                ts(1_000),
            ),
            logins(&["bravo", "charlie"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &eligible,
                &[],
                &[
                    streak("charlie", "broadcast-c"),
                    streak("bravo", "broadcast-b"),
                ],
                ts(1_900),
            ),
            logins(&["alpha", "delta"])
        );
    }

    #[test]
    fn eligibility_blip_does_not_return_a_second_jump_for_the_same_broadcast() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &eligible,
                &[],
                &[streak("charlie", "broadcast-c")],
                ts(100),
            ),
            logins(&["charlie", "alpha"])
        );

        // charlie leaves the eligible set for one pass, then returns on the same
        // broadcast. Its promotion record must survive the gap.
        assert_eq!(
            rotation.select_with_campaigns(
                &logins(&["alpha", "bravo", "delta"]),
                &[],
                &[],
                ts(200)
            ),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &eligible,
                &[],
                &[streak("charlie", "broadcast-c")],
                ts(1_100),
            ),
            logins(&["delta", "charlie"])
        );
    }

    #[test]
    fn a_new_broadcast_releases_the_promotion_record() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &eligible,
                &[],
                &[streak("charlie", "broadcast-c1")],
                ts(100),
            ),
            logins(&["charlie", "alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &eligible,
                &[],
                &[streak("charlie", "broadcast-c1")],
                ts(1_000),
            ),
            logins(&["bravo", "delta"])
        );

        // charlie starts a different broadcast, which releases the record and
        // makes it eligible for exactly one more jump.
        assert_eq!(
            rotation.select_with_campaigns(
                &eligible,
                &[],
                &[streak("charlie", "broadcast-c2")],
                ts(1_900),
            ),
            logins(&["charlie", "bravo"])
        );
    }

    #[test]
    fn fair_rotation_wins_once_streak_promotions_defer_it_too_long() {
        let eligible = (0..17)
            .map(|index| format!("s{index:02}"))
            .collect::<Vec<_>>();
        let candidates = eligible
            .iter()
            .map(|login| streak(login, &format!("broadcast-{login}")))
            .collect::<Vec<_>>();
        let mut rotation = WatchRotation::default();

        // Every eligible channel wants a streak jump, so promotions keep
        // resetting the rotation clock.
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &candidates, ts(0)),
            logins(&["s02", "s00"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &candidates, ts(900)),
            logins(&["s01", "s02"])
        );

        // At the deferral ceiling fair rotation takes the pass back from
        // promotion, so the queue keeps turning over at production scale.
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], &candidates, ts(1_800)),
            logins(&["s00", "s03"])
        );
    }
}
