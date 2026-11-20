use anyhow::Result;
use pinwatch::{
    config::Mode,
    model::*,
    notify::{self, Manager, Notice, NotificationSink},
};
fn event(kind: &str, seq: u64) -> Event {
    Event {
        schema_version: 1,
        daemon_instance: "daemon-a".into(),
        sequence: seq,
        event_id: format!("daemon-a:{seq}"),
        incident_id: "incident-a".into(),
        timestamp_unix_ms: 1000,
        gpu_uuid: "GPU-TEST".into(),
        kind: kind.into(),
        simulation: false,
        channel_id: Some(2),
        current_a: Some(9.1),
        threshold_a: 8.5,
        previous_power_cap_w: Some(600.0),
        action: Some(Action::PowerLimit { target_w: 494.307 }),
        outcome: Some(Outcome {
            power_limit_w: Some(494.307),
            ..Default::default()
        }),
        detail: "driver confirmed".into(),
    }
}
fn frame(events: Vec<Event>, state: &str) -> Frame {
    Frame {
        schema_version: 1,
        kind: "telemetry".into(),
        daemon_instance: "daemon-a".into(),
        emitted_at_unix_ms: 1000,
        max_sample_age_ms: 5000,
        recent_events: events,
        gpus: vec![GpuSnapshot {
            device: DeviceInfo {
                gpu_uuid: "GPU-TEST".into(),
                index: 0,
                pci_address: "0000:01:00.0".into(),
                model: "simulation".into(),
                sensor_supported: true,
                i2c_adapters: vec![1],
                selected_adapter: Some(1),
                sensor_error: None,
                capabilities: Capabilities::default(),
            },
            reading: Reading::empty(1),
            sample_age_ms: 0,
            freshness: "fresh".into(),
            protection: Protection {
                mode: Mode::Active,
                state: state.into(),
                effective: state == "restricted",
                reason: state.into(),
                last_action: None,
                last_outcome: None,
                historical_restriction: None,
            },
        }],
    }
}
#[derive(Default)]
struct MockSink {
    unavailable: bool,
    sent: Vec<(Notice, u32)>,
}
impl NotificationSink for MockSink {
    async fn send(&mut self, n: &Notice, replaces: u32) -> Result<u32> {
        if self.unavailable {
            anyhow::bail!("service absent");
        }
        self.sent.push((n.clone(), replaces));
        Ok(42)
    }
}
#[test]
fn mappings_do_not_announce_unconfirmed_actions() {
    assert!(notify::map_event(&event("action_requested", 1), false).is_none());
    assert!(notify::map_event(&event("overload_detected", 1), false).is_none());
    let notice = notify::map_event(&event("action_applied", 2), false).unwrap();
    assert!(notice.summary.contains("reduced"));
    assert!(notice.body.contains("494.307"));
    assert_eq!(notice.urgency, 2);
    let mut e = event("action_proposed", 3);
    e.simulation = true;
    assert!(
        notify::map_event(&e, false)
            .unwrap()
            .summary
            .starts_with("SIMULATION")
    );
    let mut e = event("action_failed", 4);
    e.outcome
        .as_mut()
        .unwrap()
        .errors
        .push("permission denied".into());
    assert!(
        notify::map_event(&e, false)
            .unwrap()
            .body
            .contains("permission denied")
    );
    let mut e = event("action_applied", 5);
    e.action = Some(Action::ClockThrottle {
        graphics_mhz: 210,
        memory_mhz: 405,
    });
    assert_eq!(notify::map_event(&e, false).unwrap().severity, 2);
}
#[tokio::test]
async fn dedup_replacement_rate_limit_and_escalation() {
    let mut manager = Manager::default();
    let mut sink = MockSink::default();
    manager.ingest(&frame(vec![], "restricted"), 0, true);
    let e = event("action_applied", 1);
    let f = frame(vec![e.clone()], "restricted");
    manager.ingest(&f, 1000, false);
    notify::deliver_pending(&mut manager, &mut sink, 1000)
        .await
        .unwrap();
    assert_eq!(sink.sent.len(), 1);
    for t in [2000, 3000, 4000] {
        manager.ingest(&f, t, false);
        notify::deliver_pending(&mut manager, &mut sink, t)
            .await
            .unwrap();
    }
    assert_eq!(sink.sent.len(), 1);
    manager.ingest(
        &frame(vec![event("action_applied", 2)], "restricted"),
        5000,
        false,
    );
    assert!(manager.pending.is_empty());
    let mut escalation = event("action_applied", 3);
    escalation.action = Some(Action::ClockThrottle {
        graphics_mhz: 210,
        memory_mhz: 405,
    });
    manager.ingest(&frame(vec![escalation], "restricted"), 6000, false);
    notify::deliver_pending(&mut manager, &mut sink, 6000)
        .await
        .unwrap();
    assert_eq!(sink.sent.len(), 2);
    assert_eq!(sink.sent[1].1, 42);
    manager.ingest(
        &frame(vec![event("action_failed", 4)], "degraded"),
        7000,
        false,
    );
    notify::deliver_pending(&mut manager, &mut sink, 7000)
        .await
        .unwrap();
    assert_eq!(sink.sent.len(), 3);
}
#[tokio::test]
async fn replay_only_latest_relevant_and_reconnect_deduplicates() {
    let mut manager = Manager::default();
    let mut sink = MockSink::default();
    let f = frame(
        vec![
            event("action_applied", 1),
            event("action_applied", 2),
            event("action_applied", 3),
        ],
        "restricted",
    );
    manager.ingest(&f, 0, true);
    notify::deliver_pending(&mut manager, &mut sink, 0)
        .await
        .unwrap();
    assert_eq!(sink.sent.len(), 1);
    assert!(sink.sent[0].0.summary.contains("Historical"));
    assert_eq!(sink.sent[0].0.identity, "daemon-a:3");
    manager.ingest(&f, 1000, true);
    assert!(manager.pending.is_empty());
    manager.server_restarted();
    notify::deliver_pending(&mut manager, &mut sink, 2000)
        .await
        .unwrap();
    assert_eq!(sink.sent.len(), 2);
    assert_eq!(sink.sent[1].1, 0);
    manager.ingest(&f, 3000, false);
    assert!(manager.pending.is_empty());
}
#[tokio::test]
async fn unavailable_notification_service_retains_one_pending_and_recovers() {
    let mut manager = Manager::default();
    let mut sink = MockSink {
        unavailable: true,
        ..Default::default()
    };
    manager.ingest(&frame(vec![event("action_failed", 1)], "degraded"), 0, true);
    assert!(
        notify::deliver_pending(&mut manager, &mut sink, 0)
            .await
            .is_err()
    );
    assert_eq!(manager.pending.len(), 1);
    manager.ingest(
        &frame(vec![event("action_failed", 1)], "degraded"),
        1000,
        false,
    );
    assert_eq!(manager.pending.len(), 1);
    sink.unavailable = false;
    notify::deliver_pending(&mut manager, &mut sink, 2000)
        .await
        .unwrap();
    assert_eq!(sink.sent.len(), 1);
    assert!(manager.pending.is_empty());
}
#[test]
fn sustained_disconnect_single_notice_then_recovery() {
    let mut m = Manager::default();
    m.disconnected(0);
    m.disconnected(4999);
    assert!(m.pending.is_empty());
    m.disconnected(5000);
    assert_eq!(m.pending.len(), 1);
    m.delivered("daemon", 5, 5000);
    m.disconnected(10000);
    assert!(m.pending.is_empty());
    m.ingest(&frame(vec![], "restricted"), 12000, true);
    assert!(
        m.pending["daemon"]
            .body
            .contains("no power limits or clocks were restored")
    );
    assert_eq!(
        (0..7).map(notify::backoff_ms).collect::<Vec<_>>(),
        [1000, 2000, 4000, 8000, 16000, 30000, 30000]
    );
}
#[test]
fn stale_cached_sample_is_not_an_all_clear() {
    let mut f = frame(vec![], "restricted");
    f.refresh(5001);
    assert_eq!(f.gpus[0].freshness, "stale");
    assert!(!f.gpus[0].protection.effective);
    let mut m = Manager::default();
    m.ingest(&f, 0, true);
    assert!(m.pending.is_empty());
    m.ingest(&f, 3000, false);
    assert_eq!(m.pending["GPU-TEST"].urgency, 2);
}
#[test]
fn daemon_restart_exposes_persistent_uncertain_action() {
    let mut f = frame(vec![], "armed_unverified");
    f.gpus[0].protection.historical_restriction = Some(event("action_requested", 1));
    let mut m = Manager::default();
    m.ingest(&f, 0, true);
    assert!(m.pending["GPU-TEST"].summary.contains("outcome unknown"));
}
#[test]
fn markup_is_escaped_and_no_restore_actions_exist() {
    let mut e = event("action_failed", 1);
    e.detail = "<b>unsafe & wrong</b>".into();
    let n = notify::map_event(&e, false).unwrap();
    assert!(n.body.contains("&lt;b&gt;unsafe &amp; wrong&lt;/b&gt;"));
}

#[tokio::test]
async fn armed_startup_is_quiet_and_does_not_suppress_intervention_or_new_incident() {
    let mut manager = Manager::default();
    let mut sink = MockSink::default();
    manager.ingest(&frame(vec![], "armed_unverified"), 0, true);
    manager.ingest(&frame(vec![], "armed_unverified"), 500, false);
    assert!(manager.pending.is_empty());
    manager.server_restarted();
    assert!(manager.pending.is_empty());
    notify::deliver_pending(&mut manager, &mut sink, 0)
        .await
        .unwrap();
    assert!(sink.sent.is_empty());
    manager.ingest(
        &frame(vec![event("action_applied", 1)], "restricted"),
        1000,
        false,
    );
    notify::deliver_pending(&mut manager, &mut sink, 1000)
        .await
        .unwrap();
    assert_eq!(sink.sent.len(), 1);
    assert!(sink.sent[0].0.summary.contains("reduced"));
    let mut next = event("action_applied", 2);
    next.incident_id = "new-incident".into();
    manager.ingest(&frame(vec![next], "restricted"), 2000, false);
    assert_eq!(manager.pending.len(), 1);
}

#[tokio::test]
async fn reason_wording_change_does_not_repeat_a_failure_popup() {
    let mut manager = Manager::default();
    let mut sink = MockSink::default();
    let mut f = frame(vec![event("action_failed", 1)], "degraded");
    manager.ingest(&f, 0, true);
    notify::deliver_pending(&mut manager, &mut sink, 0)
        .await
        .unwrap();
    f.gpus[0].protection.reason = "failure remains latched".into();
    manager.ingest(&f, 1000, false);
    assert!(manager.pending.is_empty());
}

#[test]
fn clock_notifications_distinguish_acknowledgement_from_observed_response() {
    let mut e = event("action_applied", 1);
    e.action = Some(Action::ClockThrottle {
        graphics_mhz: 180,
        memory_mhz: 405,
    });
    let requested = notify::map_event(&e, false).unwrap();
    assert!(requested.summary.contains("requested"));
    assert!(requested.body.contains("setter acknowledged"));
    e.kind = "action_verified".into();
    assert!(
        notify::map_event(&e, false)
            .unwrap()
            .summary
            .contains("observed")
    );
    e.kind = "action_failed".into();
    assert!(
        notify::map_event(&e, false)
            .unwrap()
            .summary
            .contains("failed")
    );
}

#[tokio::test]
async fn resume_health_events_do_not_replay_obsolete_popups() {
    let mut m = Manager::default();
    let mut sink = MockSink::default();
    m.ingest(&frame(vec![], "armed_unverified"), 0, true);
    let events = vec![
        event("protection_unavailable", 1),
        event("monitoring_recovered", 2),
    ];
    m.ingest(&frame(events, "armed_unverified"), 1000, false);
    notify::deliver_pending(&mut m, &mut sink, 1000)
        .await
        .unwrap();
    assert!(sink.sent.is_empty());
    m.ingest(&frame(vec![], "degraded"), 2000, false);
    m.ingest(&frame(vec![], "armed_unverified"), 2028, false);
    assert!(m.pending.is_empty());
    m.server_restarted();
    assert!(m.pending.is_empty());
}

#[tokio::test]
async fn sustained_health_failure_warns_and_only_delivered_warnings_recover() {
    let mut m = Manager::default();
    let mut sink = MockSink::default();
    m.ingest(&frame(vec![], "degraded"), 0, true);
    m.ingest(&frame(vec![], "degraded"), 3000, false);
    assert_eq!(m.pending["GPU-TEST"].urgency, 2);
    // An unavailable desktop must not receive obsolete warnings or recovery.
    m.ingest(&frame(vec![], "armed_unverified"), 4000, false);
    assert!(m.pending.is_empty());
    m.ingest(&frame(vec![], "degraded"), 5000, false);
    m.ingest(&frame(vec![], "degraded"), 8000, false);
    notify::deliver_pending(&mut m, &mut sink, 8000)
        .await
        .unwrap();
    m.ingest(&frame(vec![], "armed_unverified"), 9000, false);
    notify::deliver_pending(&mut m, &mut sink, 9000)
        .await
        .unwrap();
    assert_eq!(sink.sent.len(), 2);
    assert!(sink.sent[1].0.summary.contains("restored"));
    m.ingest(&frame(vec![], "armed_unverified"), 10000, false);
    assert!(m.pending.is_empty());
}

#[test]
fn intervention_failure_bypasses_health_grace() {
    let mut m = Manager::default();
    m.ingest(&frame(vec![], "armed_unverified"), 0, true);
    m.ingest(
        &frame(vec![event("action_failed", 1)], "degraded"),
        1,
        false,
    );
    assert!(
        m.pending["GPU-TEST"]
            .summary
            .contains("intervention failed")
    );
    m.ingest(&frame(vec![], "armed_unverified"), 2, false);
    assert!(
        m.pending["GPU-TEST"]
            .summary
            .contains("intervention failed")
    );
}
