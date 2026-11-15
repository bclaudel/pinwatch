use pinwatch::{
    backend::{self, Backend, Fixture, SimBackend, SimSample},
    config::{Config, Mode},
    model::*,
    policy::{self, Decision, Engine},
    sensor,
};
fn setup(current: f64, board: f64) -> (Config, SimBackend, DeviceInfo, Reading) {
    let config = Config {
        mode: Mode::Active,
        ..Default::default()
    };
    let mut backend = SimBackend::new(Fixture {
        samples: vec![SimSample {
            currents_a: [current; 6],
            voltage_v: 12.0,
            board_power_w: board,
            sensor_error: None,
            nvml_error: None,
            sample_duration_ms: 0,
        }],
        fail_power: false,
        fail_graphics: false,
        fail_memory: false,
    })
    .unwrap();
    let device = backend.devices().unwrap().remove(0);
    let reading = backend.sample(&device, 1);
    (config, backend, device, reading)
}
#[test]
fn decoder_units_and_exact_lengths() {
    let packet = [0x2e, 0xe0, 0x1d, 0x4c].repeat(6); // 12 V, 7.5 A
    let channels = sensor::decode(&packet).unwrap();
    assert_eq!(
        channels[0],
        Channel {
            channel_id: 1,
            voltage_v: 12.0,
            current_a: 7.5,
            power_w: 90.0
        }
    );
    assert_eq!(channels[5].channel_id, 6);
    for length in 0..70 {
        if length != 24 {
            assert!(sensor::decode(&vec![1; length]).is_err());
        }
    }
}
#[test]
fn zero_voltage_invalid_zero_current_valid_and_ratios_explicit() {
    assert!(sensor::decode(&[0; 24]).is_err());
    let channels = sensor::decode(&[0x2e, 0xe0, 0, 0].repeat(6)).unwrap();
    let mut r = Reading::empty(1);
    r.set_channels(channels);
    assert_eq!(r.connector_total_w, Some(0.0));
    assert_eq!(r.power_balance_ratio, None);
    assert_eq!(r.current_balance_ratio, None);
    assert_eq!(ratio([0.0, 1.0].into_iter()), Some(0.0));
    assert_eq!(ratio([f64::NAN, 1.0].into_iter()), None);
}
#[test]
fn model_identity_is_strict() {
    assert_eq!(
        sensor::model(0x2b8510de, 0x89e31043),
        Some("ROG-ASTRAL-RTX5090-O32G")
    );
    assert_eq!(
        sensor::model(0x2b8510de, 0x8a2e1043),
        Some("ROG-ASTRAL-RTX5090-O32G-WHITE")
    );
    assert_eq!(
        sensor::model(0x2b8510de, 0x8a5a1043),
        Some("ROG- STRIX-RTX5090-032G-BTF")
    );
    assert!(sensor::model(0x2b8510de, 0x12341043).is_none());
    assert!(sensor::model(0x123410de, 0x89e31043).is_none());
}
#[test]
fn adapter_absent_ambiguous_and_override() {
    assert!(sensor::select_adapter(&[], None).is_err());
    assert!(sensor::select_adapter(&[1, 2], None).is_err());
    assert_eq!(sensor::select_adapter(&[2, 1], Some(2)).unwrap(), 2);
    assert!(sensor::select_adapter(&[1, 2], Some(8)).is_err());
    let dir = tempfile::tempdir().unwrap();
    for name in ["i2c-7", "i2c-2", "i2c-3x", "other"] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
    }
    assert_eq!(sensor::adapters(dir.path()).unwrap(), vec![2, 7]);
}
#[test]
fn uuid_survives_index_change() {
    let (_, _, mut device, _) = setup(7.0, 500.0);
    let uuid = device.gpu_uuid.clone();
    device.index = 9;
    assert_eq!(
        backend::select(&[device.clone()], Some(&uuid), None).unwrap()[0].index,
        9
    );
    assert!(backend::select(&[device.clone()], None, Some(0)).is_err());
    assert!(backend::select(&[device], Some(&uuid), Some(9)).is_err());
}
#[test]
fn configuration_rejects_invalid_and_unknown_settings() {
    let mut c = Config::default();
    c.validate().unwrap();
    for t in [0.0, -1.0, 9.21, f64::NAN, f64::INFINITY] {
        c.threshold_a = t;
        assert!(c.validate().is_err());
    }
    c = Config::default();
    c.poll_interval_ms = 5001;
    assert!(c.validate().is_err());
    assert!(toml::from_str::<Config>("typo = true").is_err());
}
#[test]
fn current_not_imbalance_drives_control_and_equality_triggers() {
    let (c, _, d, mut r) = setup(8.49, 540.0);
    r.power_balance_ratio = Some(0.01);
    assert_eq!(
        policy::decide(&c, &d.capabilities, &r, 0),
        Decision::BelowThreshold
    );
    r.channels.as_mut().unwrap()[0].current_a = 8.5;
    assert_eq!(
        policy::decide(&c, &d.capabilities, &r, 0),
        Decision::Act(Action::PowerLimit { target_w: 529.2 })
    );
}
#[test]
fn target_uses_board_power_caps_and_headroom() {
    let (c, _, d, mut r) = setup(9.1, 540.0);
    assert_eq!(
        policy::decide(&c, &d.capabilities, &r, 0),
        Decision::Act(Action::PowerLimit { target_w: 494.307 })
    );
    r.power_limit_w = Some(450.0);
    assert_eq!(
        policy::decide(&c, &d.capabilities, &r, 0),
        Decision::Act(Action::PowerLimit { target_w: 411.923 })
    );
    for current in [8.5, 8.6, 9.1, 12.0, 20.0] {
        for cap in [400.0, 420.0, 500.0, 600.0] {
            r.power_limit_w = Some(cap);
            r.channels.as_mut().unwrap()[0].current_a = current;
            if let Decision::Act(Action::PowerLimit { target_w }) =
                policy::decide(&c, &d.capabilities, &r, 0)
            {
                assert!(target_w < cap && target_w >= 400.0);
            }
        }
    }
}
#[test]
fn fallback_requires_both_clock_ranges_but_monitoring_does_not() {
    let (c, _, mut d, r) = setup(9.1, 400.0);
    assert!(matches!(
        policy::decide(&c, &d.capabilities, &r, 0),
        Decision::Act(Action::ClockThrottle { .. })
    ));
    d.capabilities.memory_min_mhz = None;
    assert!(matches!(
        policy::decide(&c, &d.capabilities, &r, 0),
        Decision::Unavailable(_)
    ));
    let (_, _, _, r) = setup(7.0, 400.0);
    assert_eq!(
        policy::decide(&c, &d.capabilities, &r, 0),
        Decision::BelowThreshold
    );
}
#[test]
fn stale_missing_and_nonfinite_never_trigger_a_write() {
    let (c, _, d, r) = setup(9.1, 540.0);
    assert!(matches!(
        policy::decide(&c, &d.capabilities, &r, 5001),
        Decision::Unavailable(_)
    ));
    for field in 0..5 {
        let mut bad = r.clone();
        match field {
            0 => bad.channels = None,
            1 => bad.nvml_board_power_w = None,
            2 => bad.power_limit_w = Some(f64::NAN),
            3 => bad.channels.as_mut().unwrap()[0].current_a = f64::INFINITY,
            _ => bad.sensor_errors.push("I/O error".into()),
        };
        assert!(matches!(
            policy::decide(&c, &d.capabilities, &bad, 0),
            Decision::Unavailable(_)
        ));
    }
}
#[test]
fn partial_clock_failure_attempts_both_and_records_no_success() {
    let (_, mut backend, d, _) = setup(9.1, 400.0);
    backend.fixture.fail_graphics = true;
    let outcome = policy::execute(
        &mut backend,
        &d.gpu_uuid,
        &Action::ClockThrottle {
            graphics_mhz: 210,
            memory_mhz: 405,
        },
    );
    assert!(!outcome.graphics_locked && outcome.memory_locked);
    assert_eq!(outcome.errors.len(), 1);
    assert_eq!(backend.writes.len(), 2);
}
#[test]
fn monitor_and_dryrun_never_write() {
    for mode in [Mode::Monitor, Mode::DryRun] {
        let (mut c, mut backend, d, r) = setup(9.1, 540.0);
        c.mode = mode;
        let mut engine = Engine::new(mode, None);
        let mut events = vec![];
        engine
            .tick(&c, &d, &r, 0, 0, &mut backend, &mut |e| {
                events.push(e);
                Ok(())
            })
            .unwrap();
        assert!(backend.writes.is_empty());
        assert!(!engine.protection.effective);
        assert!(events.iter().all(|e| e.kind != "action_applied"));
        assert_eq!(
            events.iter().any(|e| e.kind == "action_proposed"),
            mode == Mode::DryRun
        );
    }
}
#[test]
fn event_order_and_no_stale_ratcheting() {
    let (c, mut backend, d, mut r) = setup(9.1, 540.0);
    let mut engine = Engine::new(c.mode, None);
    let mut events = vec![];
    engine
        .tick(&c, &d, &r, 0, 0, &mut backend, &mut |e| {
            events.push(e);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        ["overload_detected", "action_requested", "action_applied"]
    );
    assert_eq!(backend.writes.len(), 1);
    engine
        .tick(&c, &d, &r, 0, 2000, &mut backend, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(backend.writes.len(), 1);
    r.sample_sequence = 2;
    r.power_limit_w = Some(backend.cap_w);
    engine
        .tick(&c, &d, &r, 0, 4000, &mut backend, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(backend.writes.len(), 1);
    r.sample_sequence = 3;
    engine
        .tick(&c, &d, &r, 0, 10000, &mut backend, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(backend.writes.len(), 1);
    assert_eq!(engine.protection.state, "degraded");
}
#[test]
fn failed_write_latches_without_claiming_protection() {
    let (c, mut backend, d, mut r) = setup(9.1, 540.0);
    backend.fixture.fail_power = true;
    let mut engine = Engine::new(c.mode, None);
    let mut events = vec![];
    engine
        .tick(&c, &d, &r, 0, 0, &mut backend, &mut |e| {
            events.push(e);
            Ok(())
        })
        .unwrap();
    assert!(!engine.protection.effective);
    assert_eq!(events.last().unwrap().kind, "action_failed");
    r.sample_sequence = 2;
    engine
        .tick(&c, &d, &r, 0, 20000, &mut backend, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(backend.writes.len(), 1);
}
#[test]
fn journal_failure_before_requested_action_blocks_hardware() {
    let (c, mut backend, d, r) = setup(9.1, 540.0);
    let mut engine = Engine::new(c.mode, None);
    assert!(
        engine
            .tick(&c, &d, &r, 0, 0, &mut backend, &mut |e| {
                if e.kind == "action_requested" {
                    anyhow::bail!("disk full");
                }
                Ok(())
            })
            .is_err()
    );
    assert!(backend.writes.is_empty());
}
#[test]
fn recovery_never_restores_cap_or_clocks() {
    let (c, mut backend, d, mut r) = setup(9.1, 540.0);
    let mut engine = Engine::new(c.mode, None);
    engine
        .tick(&c, &d, &r, 0, 0, &mut backend, &mut |_| Ok(()))
        .unwrap();
    let cap = backend.cap_w;
    r.sample_sequence = 2;
    for c in r.channels.as_mut().unwrap() {
        c.current_a = 7.0;
    }
    engine
        .tick(&c, &d, &r, 0, 2000, &mut backend, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(backend.cap_w, cap);
    assert_eq!(backend.writes.len(), 1);
}
#[test]
fn schema_round_trip_nulls_and_distinct_readings() {
    let (_, _, _, mut r) = setup(7.0, 532.85);
    assert_eq!(r.connector_total_w, Some(504.0));
    r.power_limit_w = None;
    let json = serde_json::to_value(&r).unwrap();
    assert!(json["power_limit_w"].is_null());
    assert_eq!(json["channels"].as_array().unwrap().len(), 6);
    let restored: Reading = serde_json::from_value(json).unwrap();
    assert_eq!(restored.nvml_board_power_w, Some(532.85));
}

#[test]
fn external_cap_increase_is_degraded_without_overwriting_other_tool() {
    let (c, mut backend, d, mut r) = setup(9.1, 540.0);
    let mut engine = Engine::new(c.mode, None);
    engine
        .tick(&c, &d, &r, 0, 0, &mut backend, &mut |_| Ok(()))
        .unwrap();
    r.sample_sequence = 2;
    for channel in r.channels.as_mut().unwrap() {
        channel.current_a = 7.0;
    }
    // The next read reports the former, higher cap: another tool changed it.
    engine
        .tick(&c, &d, &r, 0, 2000, &mut backend, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(engine.protection.state, "degraded");
    assert!(!engine.protection.effective);
    assert_eq!(backend.writes.len(), 1);
}

#[test]
fn sample_expiring_during_request_recording_is_not_used() {
    let (mut c, mut backend, d, r) = setup(9.1, 540.0);
    c.max_sample_age_ms = 100;
    let mut engine = Engine::new(c.mode, None);
    let mut kinds = vec![];
    engine
        .tick(&c, &d, &r, 0, 0, &mut backend, &mut |event| {
            if event.kind == "action_requested" {
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            kinds.push(event.kind);
            Ok(())
        })
        .unwrap();
    assert!(backend.writes.is_empty());
    assert_eq!(kinds.last().unwrap(), "action_failed");
}

#[test]
fn new_measured_response_can_reduce_again_but_escalation_is_immediate() {
    let (c, mut backend, d, mut r) = setup(9.1, 540.0);
    let mut engine = Engine::new(c.mode, None);
    engine
        .tick(&c, &d, &r, 0, 0, &mut backend, &mut |_| Ok(()))
        .unwrap();
    r.sample_sequence = 2;
    r.power_limit_w = Some(backend.cap_w);
    r.nvml_board_power_w = Some(480.0);
    engine
        .tick(&c, &d, &r, 0, 4000, &mut backend, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(backend.writes.len(), 2);
    r.sample_sequence = 3;
    r.power_limit_w = Some(backend.cap_w);
    r.channels.as_mut().unwrap()[0].current_a = 12.0;
    engine
        .tick(&c, &d, &r, 0, 4100, &mut backend, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(backend.writes.len(), 4); // both clock controls, without waiting another settle interval
}

#[test]
fn documented_examples_obey_v1_schema() {
    let frame: Frame = serde_json::from_str(include_str!("../examples/telemetry.json")).unwrap();
    assert_eq!(frame.schema_version, SCHEMA_VERSION);
    assert_eq!(frame.gpus.len(), 1);
    let json = serde_json::to_value(&frame).unwrap();
    let keys: std::collections::BTreeSet<_> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "schema_version",
            "kind",
            "daemon_instance",
            "emitted_at_unix_ms",
            "max_sample_age_ms",
            "gpus",
            "recent_events"
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(frame.gpus[0].reading.channels.as_ref().unwrap().len(), 6);
    let event: Event = serde_json::from_str(include_str!("../examples/intervention.json")).unwrap();
    assert_eq!(event.schema_version, SCHEMA_VERSION);
    assert!(event.simulation);
    assert_eq!(event.kind, "action_applied");
    assert_eq!(event.outcome.unwrap().power_limit_w, Some(494.307));
    let config: Config = toml::from_str(include_str!("../examples/pinwatch.toml")).unwrap();
    config.validate().unwrap();
    assert_eq!(config.mode, Mode::Monitor);
}

#[test]
fn optional_config_only_falls_back_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pinwatch.toml");
    assert_eq!(Config::read_optional(&path).unwrap().mode, Mode::Monitor);
    std::fs::write(&path, "unknown = true").unwrap();
    assert!(Config::read_optional(&path).is_err());
    std::fs::write(
        &path,
        "gpu_uuid = 'GPU-configured'\n[adapters]\nGPU-configured = 3\n",
    )
    .unwrap();
    let config = Config::read_optional(&path).unwrap();
    assert_eq!(config.gpu_uuid.as_deref(), Some("GPU-configured"));
    assert_eq!(config.adapters["GPU-configured"], 3);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(Config::read_optional(&path).is_err());
}
