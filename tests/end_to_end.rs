use pinwatch::{
    config::{Config, Mode},
    ipc,
    model::Frame,
};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn launch(dir: &Path, mode: Mode) -> (Process, PathBuf) {
    let socket = dir.join("pinwatch.sock");
    let config = Config {
        mode,
        poll_interval_ms: 100,
        settle_ms: 100,
        simulate: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/overload.json")),
        socket: socket.clone(),
        lock_dir: dir.into(),
        state_file: dir.join("history.json"),
        ..Default::default()
    };
    let path = dir.join("pinwatch.toml");
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_pinwatchd"))
        .arg("--config")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    (Process(child), socket)
}
async fn ready(socket: &Path) {
    for _ in 0..100 {
        if socket.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon did not create socket");
}
async fn terminate(process: &mut Process, socket: &Path) {
    assert!(
        Command::new("kill")
            .arg("-TERM")
            .arg(process.0.id().to_string())
            .status()
            .unwrap()
            .success()
    );
    for _ in 0..100 {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success());
            assert!(!socket.exists());
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon failed to stop");
}
#[tokio::test]
async fn daemon_cli_simulated_intervention_and_clean_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let (mut process, socket) = launch(dir.path(), Mode::Active);
    ready(&socket).await;
    let mut sub = ipc::connect(&socket, true).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let f = ipc::next_frame(&mut sub).await.unwrap();
            if f.recent_events.iter().any(|e| e.kind == "action_applied") {
                break f;
            }
        }
    })
    .await
    .unwrap();
    let event = frame
        .recent_events
        .iter()
        .find(|e| e.kind == "action_applied")
        .unwrap();
    assert!(event.simulation);
    assert_eq!(event.outcome.as_ref().unwrap().power_limit_w, Some(494.307));
    let output = Command::new(env!("CARGO_BIN_EXE_pinwatch"))
        .arg("--socket")
        .arg(&socket)
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let status: Frame = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status.gpus[0].device.gpu_uuid, "GPU-SIMULATED-ASTRAL");
    assert_eq!(status.gpus[0].reading.channels.as_ref().unwrap().len(), 6);
    assert!(!output.stdout.contains(&0x1b));
    let duplicate = Command::new(env!("CARGO_BIN_EXE_pinwatchd"))
        .arg("--config")
        .arg(dir.path().join("pinwatch.toml"))
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(socket.exists());
    let conflict = Command::new(env!("CARGO_BIN_EXE_pinwatch"))
        .arg("--simulate")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/overload.json"))
        .arg("--lock-dir")
        .arg(dir.path())
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    terminate(&mut process, &socket).await;
}
#[tokio::test]
async fn dryrun_proposes_but_never_applies_and_watch_is_ndjson() {
    let dir = tempfile::tempdir().unwrap();
    let (mut process, socket) = launch(dir.path(), Mode::DryRun);
    ready(&socket).await;
    let mut reader = ipc::connect(&socket, true).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let f = ipc::next_frame(&mut reader).await.unwrap();
            if f.recent_events.iter().any(|e| e.kind == "action_proposed") {
                break f;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(frame.gpus[0].reading.power_limit_w, Some(600.0));
    assert!(
        !frame
            .recent_events
            .iter()
            .any(|e| e.kind == "action_applied")
    );
    let mut watch = Process(
        Command::new(env!("CARGO_BIN_EXE_pinwatch"))
            .arg("--socket")
            .arg(&socket)
            .args(["watch", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = watch.0.stdout.take().unwrap();
    let lines = tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        std::io::BufReader::new(stdout)
            .lines()
            .take(2)
            .map(|l| serde_json::from_str::<Frame>(&l.unwrap()).unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), lines)
            .await
            .unwrap()
            .unwrap()
            .len(),
        2
    );
    terminate(&mut process, &socket).await;
}
#[test]
fn local_simulation_devices_and_help() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pinwatch"))
        .arg("--simulate")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/overload.json"))
        .arg("--lock-dir")
        .arg(dir.path())
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let frame: Frame = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(frame.gpus[0].reading.nvml_board_power_w, Some(532.85));
    for binary in [
        env!("CARGO_BIN_EXE_pinwatch"),
        env!("CARGO_BIN_EXE_pinwatchd"),
    ] {
        let output = Command::new(binary).arg("--help").output().unwrap();
        assert!(output.status.success());
    }
}

#[test]
fn fallback_reads_config_and_cli_selection_overrides_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pinwatch.toml");
    let mut config = Config {
        mode: Mode::Active,
        gpu_uuid: Some("GPU-SIMULATED-ASTRAL".into()),
        simulate: Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/overload.json")),
        lock_dir: dir.path().into(),
        ..Default::default()
    };
    config.adapters.insert("GPU-SIMULATED-ASTRAL".into(), 1);
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_pinwatch"))
            .args(["--socket"])
            .arg(dir.path().join("missing.sock"))
            .arg("--config")
            .arg(&path)
            .args(extra)
            .args(["status", "--json"])
            .output()
            .unwrap()
    };
    let output = run(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let frame: Frame = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(frame.gpus[0].protection.mode, Mode::Monitor);
    assert_eq!(frame.gpus[0].reading.power_limit_w, Some(600.0));
    assert!(frame.recent_events.is_empty());
    assert!(!run(&["--uuid", "GPU-MISSING"]).status.success());
    assert!(!run(&["--adapter", "99"]).status.success());
    config.gpu_uuid = Some("GPU-MISSING".into());
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    assert!(!run(&[]).status.success());
    assert!(run(&["--index", "0"]).status.success());
    assert!(
        run(&["--uuid", "GPU-SIMULATED-ASTRAL", "--adapter", "1"])
            .status
            .success()
    );
    std::fs::write(&path, "invalid = true").unwrap();
    assert!(!run(&[]).status.success());
    std::fs::remove_file(&path).unwrap();
    assert!(!run(&[]).status.success()); // explicit missing configuration never silently falls back
}

#[tokio::test]
async fn automatic_status_prefers_daemon_without_loading_local_config() {
    let dir = tempfile::tempdir().unwrap();
    let (mut daemon, socket) = launch(dir.path(), Mode::Monitor);
    ready(&socket).await;
    // A broken local fallback config must not prevent cached daemon monitoring.
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "bad = true").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pinwatch"))
        .arg("--socket")
        .arg(&socket)
        .arg("--config")
        .arg(&bad)
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let frame: Frame = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(frame.daemon_instance, "direct-read-only");
    // Local overrides do not create a second sensor poller when IPC is available.
    let output = Command::new(env!("CARGO_BIN_EXE_pinwatch"))
        .arg("--socket")
        .arg(&socket)
        .args(["--adapter", "99", "devices", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    terminate(&mut daemon, &socket).await;
}

#[tokio::test]
async fn automatic_watch_hands_off_between_local_and_restarted_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let (mut daemon, socket) = launch(dir.path(), Mode::Monitor);
    ready(&socket).await;
    let mut watcher = Process(
        Command::new(env!("CARGO_BIN_EXE_pinwatch"))
            .arg("--socket")
            .arg(&socket)
            .arg("--config")
            .arg(dir.path().join("pinwatch.toml"))
            .args(["watch", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = watcher.0.stdout.take().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let parser = tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(stdout);
        loop {
            let mut line = vec![];
            if reader.read_until(b'\n', &mut line).unwrap() == 0 || !line.ends_with(b"\n") {
                break; // teardown may kill the subprocess partway through its final line
            }
            let frame: Frame = serde_json::from_slice(&line).unwrap();
            if tx.blocking_send(frame).is_err() {
                break;
            }
        }
    });
    let first = tokio::time::timeout(Duration::from_secs(4), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first.daemon_instance, "direct-read-only");
    terminate(&mut daemon, &socket).await;
    let local = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let frame = rx.recv().await.unwrap();
            if frame.daemon_instance == "direct-read-only" {
                break frame;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(local.gpus[0].protection.mode, Mode::Monitor);
    assert!(!local.gpus[0].protection.effective);
    assert_eq!(local.gpus[0].reading.power_limit_w, Some(600.0));
    let (mut restarted, _) = launch(dir.path(), Mode::Monitor);
    let recovered = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let frame = rx.recv().await.unwrap();
            if frame.daemon_instance != "direct-read-only" {
                break frame;
            }
        }
    })
    .await
    .unwrap();
    assert_ne!(recovered.daemon_instance, first.daemon_instance);
    assert!(watcher.0.try_wait().unwrap().is_none());
    drop(watcher);
    drop(rx);
    parser.await.unwrap();
    terminate(&mut restarted, &socket).await;
}

#[tokio::test]
async fn incompatible_and_malformed_daemons_do_not_trigger_hardware_fallback() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for response in ["not json\n", "{\"schema_version\":99}\n"] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            reader
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        });
        let command = Command::new(env!("CARGO_BIN_EXE_pinwatch"))
            .arg("--socket")
            .arg(&socket)
            .arg("--config")
            .arg(dir.path().join("must-not-be-read.toml"))
            .args(["status", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = tokio::task::spawn_blocking(move || command.wait_with_output().unwrap())
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("read local configuration"));
        server.await.unwrap();
    }
}
