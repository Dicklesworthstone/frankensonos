//! Reconcile bookkeeping: player health from call outcomes, and the survey
//! schedule with its backoff.

use fsonos_core::events::{Report, Service};
use fsonos_core::reconcile::{Health, HealthBoard, OFFLINE_AFTER, Refresh};
use fsonos_types::PlayerId;
use std::time::{Duration, Instant};

fn pid(n: u8) -> PlayerId {
    PlayerId(format!("RINCON_000E58A000{n:02X}01400"))
}

#[test]
fn health_degrades_then_goes_offline_and_recovers_on_success() {
    let mut board = HealthBoard::default();
    let now = Instant::now();
    board.ok(&pid(1), now);
    assert_eq!(board.of(&pid(1)).unwrap().health, Health::Healthy);

    board.failed(&pid(1), "connection refused");
    let h = board.of(&pid(1)).unwrap();
    assert_eq!((h.health, h.consecutive_failures), (Health::Degraded, 1));
    assert_eq!(h.last_ok, Some(now), "the last success is kept");
    for _ in 1..OFFLINE_AFTER {
        board.failed(&pid(1), "connection refused");
    }
    assert_eq!(board.of(&pid(1)).unwrap().health, Health::Offline);

    board.ok(&pid(1), now + Duration::from_secs(60));
    let h = board.of(&pid(1)).unwrap();
    assert_eq!(
        (h.health, h.consecutive_failures, h.last_error.as_deref()),
        (Health::Healthy, 0, None)
    );
}

#[test]
fn a_player_the_survey_did_not_find_is_offline_at_once() {
    let mut board = HealthBoard::default();
    board.ok(&pid(2), Instant::now());
    board.missing(&pid(2), "not found by the last survey");
    assert_eq!(board.of(&pid(2)).unwrap().health, Health::Offline);
}

#[test]
fn subscription_failures_count_against_their_player() {
    let mut board = HealthBoard::default();
    board.record(&Report {
        failed: vec![(pid(3), Service::AvTransport, "HTTP 503".into())],
        ..Report::default()
    });
    let h = board.of(&pid(3)).unwrap();
    assert_eq!(h.health, Health::Degraded);
    assert!(h.last_error.as_deref().unwrap().contains("AvTransport"));
}

#[test]
fn surveys_run_on_the_interval_and_back_off_after_failures() {
    let t0 = Instant::now();
    let interval = Duration::from_mins(5);
    let mut r = Refresh::new(interval, Duration::from_mins(30), t0);
    assert!(r.due(t0), "the first survey is due right away");

    r.succeeded(t0);
    assert!(!r.due(t0 + Duration::from_secs(299)));
    assert!(r.due(t0 + interval));

    // Failures retry sooner at first (a quarter interval), then double, up
    // to the cap.
    let delays: Vec<u64> = (0..6).map(|_| r.failed_with(t0, 0).as_secs()).collect();
    assert_eq!(delays, [75, 150, 300, 600, 1200, 1800]);
    r.succeeded(t0);
    assert_eq!(r.next_at(), t0 + interval, "a success resets the backoff");
    assert_eq!(r.failed_with(t0, 0), Duration::from_secs(75));
}

#[test]
fn failure_delays_carry_up_to_ten_percent_jitter() {
    let t0 = Instant::now();
    let mut r = Refresh::new(Duration::from_mins(5), Duration::from_mins(30), t0);
    let mut steps = Vec::new();
    for n in 0..6u32 {
        let step = Duration::from_secs(75 * (1 << n)).min(Duration::from_mins(30));
        let d = r.failed(t0);
        assert!(d >= step && d <= step + step / 10, "{d:?} vs {step:?}");
        steps.push(d);
    }
    assert_eq!(r.next_at(), t0 + steps[5]);
    assert_eq!(
        Refresh::new(Duration::from_mins(5), Duration::from_mins(30), t0).failed_with(t0, 100),
        Duration::from_secs(82) + Duration::from_millis(500)
    );
}
