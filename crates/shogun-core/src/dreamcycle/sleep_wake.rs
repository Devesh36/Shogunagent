//! What a sleep or a wake should do about the Dream Cycle (issue #285) — the pure decision only.
//!
//! The Batch lane is an asynchronous cloud job, so the long wait can happen while the Mac sleeps:
//! submit before sleep, collect on wake. A laptop whose lid stayed shut through the whole window
//! gets one more chance on wake, as long as the window closed recently enough that the result is
//! still "last night's". Past that, the ordinary gate takes over (a degraded catch-up now, the full
//! cycle the next night) — nothing here re-runs an old night on its own.
//!
//! The native hooks (`NSWorkspace.willSleepNotification` / `didWakeNotification`) live in the
//! desktop's Dream driver. They call [`power_event_run`] and run the one cycle it names under
//! [`power_event_conditions`].

use super::outcome::NightWindow;

/// How long after the nightly window closes a wake still submits that night's full cycle.
pub const WAKE_CATCH_UP_MS: i64 = 6 * 60 * 60 * 1000;

/// How long before the next window opens a sleep already submits that night's job. A lid closed at
/// 21:00 sleeps through a 02:00 window; one closed at 14:00 most likely opens again the same day,
/// and submitting then would cut the day's input short.
pub const SLEEP_SUBMIT_LEAD_MS: i64 = 6 * 60 * 60 * 1000;

/// What to do on `didWake`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeAction {
    /// Nothing: the night is done, or its window is too far behind to still count as last night.
    Nothing,
    /// A job was submitted before sleep and is waiting at the provider: collect it now.
    Collect,
    /// The night never ran and its window closed less than the catch-up allowance ago: submit it.
    SubmitNight { night: String },
}

/// What to do on `willSleep`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SleepAction {
    Nothing,
    /// Submit this night's job now, so it runs while the Mac sleeps.
    SubmitNight {
        night: String,
    },
}

/// Decide on wake. `due` is the latest window that has begun ([`super::outcome::due_night`]);
/// `due_done` whether its full cycle completed; `awaiting_results` whether a submitted cycle is
/// waiting at the provider (a durable handle, not yet collected).
pub fn on_wake(
    now_ms: i64,
    due: &NightWindow,
    due_done: bool,
    awaiting_results: bool,
    catch_up_ms: i64,
) -> WakeAction {
    if awaiting_results {
        return WakeAction::Collect;
    }
    if due_done {
        return WakeAction::Nothing;
    }
    if now_ms <= due.end_ms.saturating_add(catch_up_ms) {
        WakeAction::SubmitNight {
            night: due.night.clone(),
        }
    } else {
        WakeAction::Nothing
    }
}

/// Decide on sleep. The due night comes first while it can still count as last night; otherwise
/// the next night ([`super::outcome::next_night`]), when its window opens within `lead_ms`.
pub fn on_will_sleep(
    now_ms: i64,
    due: &NightWindow,
    next: &NightWindow,
    due_done: bool,
    next_done: bool,
    catch_up_ms: i64,
    lead_ms: i64,
) -> SleepAction {
    if !due_done && now_ms <= due.end_ms.saturating_add(catch_up_ms) {
        return SleepAction::SubmitNight {
            night: due.night.clone(),
        };
    }
    if !next_done && next.start_ms.saturating_sub(now_ms) <= lead_ms {
        SleepAction::SubmitNight {
            night: next.night.clone(),
        }
    } else {
        SleepAction::Nothing
    }
}

/// A power transition the desktop observes (`NSWorkspace` will-sleep / did-wake).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    WillSleep,
    DidWake,
}

/// What the ledger says about the nights around a power transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NightFacts {
    /// The due night's full cycle completed.
    pub due_done: bool,
    /// The next night's full cycle completed (a lid closed the evening before already ran it).
    pub next_done: bool,
    /// An unfinished full cycle is past freezing and owes the provider a collect: it was handed
    /// to the provider, or is being handed over and may have been accepted.
    pub awaiting_results: bool,
}

/// Run one cycle for `night` on a power transition, or nothing. `collect` is true when the run
/// only resumes a cycle already at the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PowerEventRun {
    Nothing,
    Run { night: String, collect: bool },
}

/// Decide a power transition from the ledger facts. A sleep never re-submits while results are
/// already owed: the wake collects them.
pub fn plan_power_event(
    event: PowerEvent,
    now_ms: i64,
    due: &NightWindow,
    next: &NightWindow,
    facts: NightFacts,
) -> PowerEventRun {
    match event {
        PowerEvent::DidWake => {
            match on_wake(
                now_ms,
                due,
                facts.due_done,
                facts.awaiting_results,
                WAKE_CATCH_UP_MS,
            ) {
                WakeAction::Nothing => PowerEventRun::Nothing,
                WakeAction::Collect => PowerEventRun::Run {
                    night: due.night.clone(),
                    collect: true,
                },
                WakeAction::SubmitNight { night } => PowerEventRun::Run {
                    night,
                    collect: false,
                },
            }
        }
        PowerEvent::WillSleep if facts.awaiting_results => PowerEventRun::Nothing,
        PowerEvent::WillSleep => match on_will_sleep(
            now_ms,
            due,
            next,
            facts.due_done,
            facts.next_done,
            WAKE_CATCH_UP_MS,
            SLEEP_SUBMIT_LEAD_MS,
        ) {
            SleepAction::Nothing => PowerEventRun::Nothing,
            SleepAction::SubmitNight { night } => PowerEventRun::Run {
                night,
                collect: false,
            },
        },
    }
}

/// The gate conditions for a run a power transition decided on. The window and idleness are
/// answered by the decision itself: before sleep the user is leaving, and after wake the work is
/// a wait on the provider, not local load. The battery floor still applies, and the observed
/// power facts are kept.
pub fn power_event_conditions(
    observed: &super::gate::RunConditions,
) -> super::gate::RunConditions {
    super::gate::RunConditions {
        within_window: true,
        window_elapsed: false,
        screen_locked: true,
        full_run_done_today: false,
        ..*observed
    }
}

/// Read the [`NightFacts`] for `due` and `next` from the cycle ledgers.
#[cfg(feature = "db")]
pub fn night_facts(
    db: &crate::daemon::Db,
    due: &NightWindow,
    next: &NightWindow,
) -> Result<NightFacts, String> {
    use super::plan::CycleKind;
    use shogun_memory::dream_cycles::BatchStage;

    // Two weeks covers any due/next pair; the ledger is newest-first.
    let recent = db.recent_cycles(14);
    let done = |night: &str| {
        recent
            .iter()
            .any(|c| c.cycle_id == night && c.kind == CycleKind::Full && c.succeeded)
    };
    let awaiting_results = db
        .try_oldest_unfinished_full_cycle()?
        .is_some_and(|cycle| {
            matches!(
                cycle.batch_stage,
                BatchStage::Submitting | BatchStage::Submitted | BatchStage::ResultsReady
            )
        });
    Ok(NightFacts {
        due_done: done(&due.night),
        next_done: done(&next.night),
        awaiting_results,
    })
}

/// Read the ledger and decide a power transition at `now_ms` in the default nightly window.
#[cfg(feature = "db")]
pub fn power_event_run(
    db: &crate::daemon::Db,
    event: PowerEvent,
    now_ms: i64,
    gmt_offset_secs: i32,
) -> Result<PowerEventRun, String> {
    use super::outcome::{due_night, next_night};
    use super::schedule::{DEFAULT_WINDOW_END_HOUR, DEFAULT_WINDOW_START_HOUR};

    let due = due_night(
        now_ms,
        gmt_offset_secs,
        DEFAULT_WINDOW_START_HOUR,
        DEFAULT_WINDOW_END_HOUR,
    );
    let next = next_night(
        &due,
        gmt_offset_secs,
        DEFAULT_WINDOW_START_HOUR,
        DEFAULT_WINDOW_END_HOUR,
    );
    let facts = night_facts(db, &due, &next)?;
    Ok(plan_power_event(event, now_ms, &due, &next, facts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dreamcycle::outcome::{due_night, next_night};

    const HOUR: i64 = 3_600_000;
    /// 2026-09-25 00:00 UTC.
    const SEP25: i64 = 1_790_294_400_000;

    fn wake(now: i64, done: bool, awaiting: bool) -> WakeAction {
        on_wake(
            now,
            &due_night(now, 0, 2, 6),
            done,
            awaiting,
            WAKE_CATCH_UP_MS,
        )
    }

    fn sleep(now: i64, due_done: bool, next_done: bool) -> SleepAction {
        let due = due_night(now, 0, 2, 6);
        let next = next_night(&due, 0, 2, 6);
        on_will_sleep(
            now,
            &due,
            &next,
            due_done,
            next_done,
            WAKE_CATCH_UP_MS,
            SLEEP_SUBMIT_LEAD_MS,
        )
    }

    #[test]
    fn a_missed_window_is_submitted_on_wake_within_the_allowance() {
        // Window 02–06 missed; opened at 08:00 → two hours late → submit the 25th.
        assert_eq!(
            wake(SEP25 + 8 * HOUR, false, false),
            WakeAction::SubmitNight {
                night: "20260925".into()
            }
        );
        // Exactly at the allowance still counts.
        assert_eq!(
            wake(SEP25 + 6 * HOUR + WAKE_CATCH_UP_MS, false, false),
            WakeAction::SubmitNight {
                night: "20260925".into()
            }
        );
    }

    #[test]
    fn a_window_too_far_behind_is_left_to_the_ordinary_gate() {
        assert_eq!(
            wake(SEP25 + 6 * HOUR + WAKE_CATCH_UP_MS + 1, false, false),
            WakeAction::Nothing
        );
        // Friday → Monday: Monday's own window is what is due, and it is long closed by 15:00.
        assert_eq!(wake(SEP25 + 15 * HOUR, false, false), WakeAction::Nothing);
    }

    #[test]
    fn a_wake_inside_the_window_submits_instead_of_waiting_for_idle() {
        assert_eq!(
            wake(SEP25 + 3 * HOUR, false, false),
            WakeAction::SubmitNight {
                night: "20260925".into()
            }
        );
    }

    #[test]
    fn a_done_night_does_nothing_and_a_waiting_job_is_collected_first() {
        assert_eq!(wake(SEP25 + 8 * HOUR, true, false), WakeAction::Nothing);
        // A job submitted before sleep is collected whenever the Mac wakes, however late.
        assert_eq!(wake(SEP25 + 20 * HOUR, false, true), WakeAction::Collect);
        assert_eq!(wake(SEP25 + 8 * HOUR, true, true), WakeAction::Collect);
    }

    #[test]
    fn a_lid_closed_in_the_evening_submits_the_coming_night() {
        // 22:00 on the 25th: the 26th's window opens at 02:00, four hours away.
        assert_eq!(
            sleep(SEP25 + 22 * HOUR, true, false),
            SleepAction::SubmitNight {
                night: "20260926".into()
            }
        );
        // Already done for the coming night → nothing.
        assert_eq!(sleep(SEP25 + 22 * HOUR, true, true), SleepAction::Nothing);
    }

    #[test]
    fn a_lid_closed_in_the_afternoon_does_not_cut_the_day_short() {
        assert_eq!(sleep(SEP25 + 14 * HOUR, true, false), SleepAction::Nothing);
    }

    #[test]
    fn sleeping_inside_or_just_after_a_missed_window_submits_that_night() {
        assert_eq!(
            sleep(SEP25 + 3 * HOUR, false, false),
            SleepAction::SubmitNight {
                night: "20260925".into()
            }
        );
        assert_eq!(
            sleep(SEP25 + 7 * HOUR, false, false),
            SleepAction::SubmitNight {
                night: "20260925".into()
            }
        );
    }

    // power transitions (desktop hooks)

    fn plan(event: PowerEvent, now: i64, facts: NightFacts) -> PowerEventRun {
        let due = due_night(now, 0, 2, 6);
        let next = next_night(&due, 0, 2, 6);
        plan_power_event(event, now, &due, &next, facts)
    }

    fn run(night: &str, collect: bool) -> PowerEventRun {
        PowerEventRun::Run {
            night: night.into(),
            collect,
        }
    }

    #[test]
    fn a_wake_maps_the_wake_decision_onto_one_run() {
        let none = NightFacts::default();
        assert_eq!(
            plan(PowerEvent::DidWake, SEP25 + 8 * HOUR, none),
            run("20260925", false)
        );
        let awaiting = NightFacts {
            awaiting_results: true,
            ..none
        };
        assert_eq!(
            plan(PowerEvent::DidWake, SEP25 + 20 * HOUR, awaiting),
            run("20260925", true)
        );
        let done = NightFacts {
            due_done: true,
            ..none
        };
        assert_eq!(
            plan(PowerEvent::DidWake, SEP25 + 8 * HOUR, done),
            PowerEventRun::Nothing
        );
    }

    #[test]
    fn a_sleep_submits_the_coming_night_but_never_while_results_are_owed() {
        let due_done = NightFacts {
            due_done: true,
            ..NightFacts::default()
        };
        assert_eq!(
            plan(PowerEvent::WillSleep, SEP25 + 22 * HOUR, due_done),
            run("20260926", false)
        );
        let owed = NightFacts {
            awaiting_results: true,
            ..due_done
        };
        assert_eq!(
            plan(PowerEvent::WillSleep, SEP25 + 22 * HOUR, owed),
            PowerEventRun::Nothing
        );
        assert_eq!(
            plan(PowerEvent::WillSleep, SEP25 + 14 * HOUR, due_done),
            PowerEventRun::Nothing
        );
    }

    #[test]
    fn a_power_event_run_passes_the_gate_for_an_active_user_but_not_on_a_low_battery() {
        use crate::dreamcycle::gate::{decide, RunConditions, RunDecision, SkipReason};
        let active = RunConditions {
            within_window: false,
            window_elapsed: true,
            idle_ms: 0,
            screen_locked: false,
            power_connected: false,
            battery_pct: 80,
            full_run_done_today: false,
        };
        assert_eq!(decide(&power_event_conditions(&active)), RunDecision::Full);
        let low = RunConditions {
            battery_pct: 5,
            ..active
        };
        assert_eq!(
            decide(&power_event_conditions(&low)),
            RunDecision::Skip(SkipReason::InsufficientPower)
        );
    }

    #[cfg(feature = "db")]
    mod ledger {
        use super::*;
        use crate::daemon::Db;
        use crate::dreamcycle::gate::{RunConditions, IDLE_THRESHOLD_MS};
        use crate::dreamcycle::jobs::LocalRuleClassifier;
        use crate::dreamcycle::schedule::DreamScheduler;
        use std::sync::Arc;

        fn db_at(now: i64) -> Db {
            Db::open_in_memory(Arc::new(move || now)).unwrap()
        }

        fn windows(now: i64) -> (NightWindow, NightWindow) {
            let due = due_night(now, 0, 2, 6);
            let next = next_night(&due, 0, 2, 6);
            (due, next)
        }

        #[test]
        fn an_empty_ledger_owes_nothing_and_a_wake_submits_the_missed_night() {
            let now = SEP25 + 8 * HOUR;
            let db = db_at(now);
            let (due, next) = windows(now);
            assert_eq!(night_facts(&db, &due, &next).unwrap(), NightFacts::default());
            assert_eq!(
                power_event_run(&db, PowerEvent::DidWake, now, 0).unwrap(),
                run("20260925", false)
            );
        }

        #[test]
        fn a_completed_night_is_done_and_a_wake_leaves_it_alone() {
            let now = SEP25 + 8 * HOUR;
            let db = db_at(now);
            let cond = RunConditions {
                within_window: true,
                window_elapsed: false,
                idle_ms: IDLE_THRESHOLD_MS,
                screen_locked: false,
                power_connected: true,
                battery_pct: 100,
                full_run_done_today: false,
            };
            let ran = DreamScheduler::new(&db, &LocalRuleClassifier).tick(&cond, "20260925", now);
            assert!(matches!(ran, crate::dreamcycle::run::GatedRun::Ran { .. }));
            let (due, next) = windows(now);
            let facts = night_facts(&db, &due, &next).unwrap();
            assert!(facts.due_done && !facts.next_done && !facts.awaiting_results);
            assert_eq!(
                power_event_run(&db, PowerEvent::DidWake, now, 0).unwrap(),
                PowerEventRun::Nothing
            );
        }

        #[test]
        fn only_a_cycle_handed_to_the_provider_owes_a_collect() {
            let now = SEP25 + 20 * HOUR;
            let db = db_at(now);
            db.try_freeze_full_cycle("20260925", 0, now).unwrap();
            let (due, next) = windows(now);
            // Frozen but never handed over: nothing is waiting at the provider.
            assert!(!night_facts(&db, &due, &next).unwrap().awaiting_results);
            for stage in ["submitting", "submitted", "results_ready"] {
                assert_eq!(db.test_set_dream_cycle_stage("20260925", stage).unwrap(), 1);
                assert!(
                    night_facts(&db, &due, &next).unwrap().awaiting_results,
                    "{stage}"
                );
                assert_eq!(
                    power_event_run(&db, PowerEvent::DidWake, now, 0).unwrap(),
                    run("20260925", true)
                );
                assert_eq!(
                    power_event_run(&db, PowerEvent::WillSleep, now, 0).unwrap(),
                    PowerEventRun::Nothing
                );
            }
        }
    }
}
