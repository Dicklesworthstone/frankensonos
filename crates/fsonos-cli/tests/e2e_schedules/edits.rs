use super::*;
use fsonos_api::surface::schedules::ScheduleDto;

fn cli_schedule(s: &mut Scenario, mode: &str, step: &str, action: &[&str]) -> ScheduleDto {
    let mut args = vec![mode, "--json", "schedule"];
    args.extend_from_slice(action);
    let result = s.cli(step, &args);
    assert!(result.ok(), "{result:?}");
    serde_json::from_str(&result.stdout).unwrap()
}

fn edit_volume(
    s: &mut Scenario,
    mode: &str,
    step: &str,
    observed: &ScheduleDto,
    when: &str,
    volume: u8,
) -> ScheduleDto {
    cli_schedule(
        s,
        mode,
        step,
        &[
            "edit",
            &observed.id.to_string(),
            "--expected-revision",
            &observed.revision,
            when,
            "volume",
            ROOM,
            &volume.to_string(),
        ],
    )
}

fn assert_persisted_run(
    s: &mut Scenario,
    clock: &Path,
    t0: DateTime<FixedOffset>,
    edited: &ScheduleDto,
) {
    let (daemon, api) = serve(s, clock);
    let (status, stored) = call(&api, "GET", "/schedules", None);
    assert_eq!(status, 200);
    let rows: Vec<ScheduleDto> = serde_json::from_value(stored).unwrap();
    assert_eq!(rows, [edited.clone()]);
    set_clock(clock, t0 + TimeDelta::minutes(11));
    let resumed = cli_schedule(
        s,
        "--daemon",
        "edit-resume",
        &["resume", &edited.id.to_string()],
    );
    assert!(resumed.enabled);
    let volume = || get_volume(&s.lan(), s.ip(ROOM)).ok();
    assert!(throughout(Duration::from_millis(1200), || volume() == Some(30)));
    let before_run = s.sim_handle().unwrap().soap_log().len();
    set_clock(clock, t0 + TimeDelta::minutes(20));
    assert!(eventually(Duration::from_secs(5), || volume() == Some(17)));
    assert!(throughout(Duration::from_millis(1200), || volume() == Some(17)));
    assert_eq!(
        s.sim_handle().unwrap().soap_log()[before_run..]
            .iter()
            .filter(|call| call.action == "SetVolume")
            .count(),
        1
    );
    let (status, stored) = call(&api, "GET", "/schedules", None);
    assert_eq!(status, 200);
    let row: ScheduleDto = serde_json::from_value(stored[0].clone()).unwrap();
    assert_eq!(row.id, edited.id);
    assert_eq!(row.creator, edited.creator);
    assert!(row.next.is_none(), "the edited one-shot is complete");
    assert_ne!(
        row.revision, resumed.revision,
        "a claimed occurrence invalidates old editors"
    );
    drop(daemon);
}

#[test]
fn edits_keep_identity_and_pause_state_survive_restart_and_fire_only_the_new_occurrence() {
    let mut s = Scenario::start("schedule-edit");
    s.sim(SimHousehold::standard());
    let clock = clock_file(&s);
    let t0 = now();
    set_clock(&clock, t0);
    let (daemon, api) = serve(&mut s, &clock);
    let (status, _) = call(
        &api,
        "POST",
        "/volume",
        Some(&json!({"zone":ROOM, "volume":30})),
    );
    assert_eq!(status, 200);
    let added = cli_schedule(
        &mut s,
        "--daemon",
        "edit-add",
        &["add", "in 10m", "volume", ROOM, "25"],
    );
    let id = added.id.to_string();
    let paused = cli_schedule(&mut s, "--daemon", "edit-pause", &["pause", &id]);
    assert!(!paused.revision.is_empty());
    let before = s.sim_handle().unwrap().soap_log().len();
    let edited = edit_volume(&mut s, "--daemon", "edit-update", &paused, "in 20m", 17);
    assert_eq!(
        (edited.id, &edited.creator, edited.enabled),
        (added.id, &added.creator, false)
    );
    assert_eq!(edited.volume, Some(17));
    assert_ne!(edited.revision, paused.revision);
    assert!(
        s.sim_handle().unwrap().soap_log()[before..]
            .iter()
            .all(|call| call.action.starts_with("Get") || call.action == "Browse")
    );
    let stale = s.cli(
        "edit-stale",
        &[
            "--daemon",
            "--json",
            "schedule",
            "edit",
            &id,
            "--expected-revision",
            &paused.revision,
            "in 15m",
            "volume",
            ROOM,
            "90",
        ],
    );
    assert!(!stale.ok(), "{stale:?}");
    assert!(stale.stderr.contains("CONFIG_CHANGED"), "{stale:?}");
    drop(daemon);

    // Direct mode opens the same durable store; an identical complete
    // definition remains a no-op, including its existing run cutoff.
    let unchanged = edit_volume(
        &mut s,
        "--direct",
        "edit-direct-no-op",
        &edited,
        &edited.when,
        17,
    );
    assert_eq!(unchanged.revision, edited.revision);
    assert_eq!(unchanged.last_fired, edited.last_fired);
    assert_persisted_run(&mut s, &clock, t0, &edited);
    s.finish();
}
