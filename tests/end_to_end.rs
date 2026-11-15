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
        .args(["--direct", "--simulate"])
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
fn direct_simulation_devices_and_help() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pinwatch"))
        .args(["--direct", "--simulate"])
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
