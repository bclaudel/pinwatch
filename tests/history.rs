use pinwatch::{daemon::Journal, model::*};
fn event(kind: &str, simulation: bool) -> Event {
    Event {
        schema_version: 1,
        daemon_instance: String::new(),
        sequence: 0,
        event_id: String::new(),
        incident_id: "incident".into(),
        timestamp_unix_ms: now_ms(),
        gpu_uuid: "GPU-test".into(),
        kind: kind.into(),
        simulation,
        channel_id: Some(2),
        current_a: Some(9.1),
        threshold_a: 8.5,
        previous_power_cap_w: Some(600.0),
        action: Some(Action::PowerLimit { target_w: 494.307 }),
        outcome: None,
        detail: "test".into(),
    }
}
#[test]
fn restart_keeps_uncertain_request_and_bounds_recent_ring() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.json");
    let mut journal = Journal::open(&path, "first").unwrap();
    journal.emit(event("action_requested", false)).unwrap();
    assert!(Journal::open(&path, "duplicate").is_err());
    for _ in 0..70 {
        journal.emit(event("overload_detected", false)).unwrap();
    }
    assert_eq!(journal.recent().len(), 64);
    assert_eq!(
        journal.historical("GPU-test").unwrap().kind,
        "action_requested"
    );
    drop(journal);
    let mut restarted = Journal::open(&path, "second").unwrap();
    let old = restarted.historical("GPU-test").unwrap();
    assert_eq!(old.event_id, "first:1");
    assert!(old.outcome.is_none());
    restarted.emit(event("action_applied", true)).unwrap();
    assert_eq!(restarted.recent().last().unwrap().event_id, "second:1");
    assert_eq!(
        restarted.historical("GPU-test").unwrap().event_id,
        "first:1"
    );
}
#[test]
fn corrupt_or_symlinked_history_is_not_silently_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.json");
    std::fs::write(&path, "garbage").unwrap();
    assert!(Journal::open(&path, "first").is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "garbage");
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(dir.path().join("other"), &path).unwrap();
    assert!(Journal::open(&path, "first").is_err());
}
