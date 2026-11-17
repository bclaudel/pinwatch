use pinwatch::{
    ipc::{self, Cached, Ownership, SocketGuard},
    model::*,
};
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::watch,
};
fn frame() -> Frame {
    Frame {
        schema_version: 1,
        kind: "telemetry".into(),
        daemon_instance: "test".into(),
        emitted_at_unix_ms: 0,
        max_sample_age_ms: 5000,
        gpus: vec![],
        recent_events: vec![],
    }
}
#[test]
fn ownership_exclusion_and_no_unlink() {
    let dir = tempfile::tempdir().unwrap();
    let lock = Ownership::acquire(dir.path(), "GPU-test").unwrap();
    assert!(Ownership::acquire(dir.path(), "GPU-test").is_err());
    drop(lock);
    assert!(dir.path().join("GPU-test.lock").exists());
    assert!(Ownership::acquire(dir.path(), "GPU-test").is_ok());
    assert!(Ownership::acquire(dir.path(), "../escape").is_err());
}
#[tokio::test]
async fn socket_cleanup_refuses_regular_files_symlinks_and_active_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("socket");
    std::fs::write(&path, "preserve").unwrap();
    assert!(SocketGuard::bind(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "preserve");
    std::fs::remove_file(&path).unwrap();
    symlink("nonexistent", &path).unwrap();
    assert!(SocketGuard::bind(&path).is_err());
    std::fs::remove_file(&path).unwrap();
    let (listener, guard) = SocketGuard::bind(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o660
    );
    assert!(SocketGuard::bind(&path).is_err());
    drop(listener);
    drop(guard);
    assert!(!path.exists());
}
#[tokio::test]
async fn bounded_json_reader_rejects_truncated_and_oversized() {
    assert!(
        ipc::line(&mut BufReader::new(&b"12345\n"[..]), 4)
            .await
            .is_err()
    );
    assert!(
        ipc::line(&mut BufReader::new(&b"{}"[..]), 32)
            .await
            .is_err()
    );
    assert!(
        ipc::line(&mut BufReader::new(&b""[..]), 32)
            .await
            .unwrap()
            .is_none()
    );
}
#[tokio::test]
async fn snapshot_subscribe_invalid_requests_and_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("socket");
    let (listener, guard) = SocketGuard::bind(&path).unwrap();
    let (tx, rx) = watch::channel(Some(Cached {
        frame: frame(),
        at: Instant::now(),
    }));
    let (stop_tx, stop_rx) = watch::channel(false);
    let server = tokio::spawn(ipc::serve(listener, rx, stop_rx));
    let mut reader = ipc::connect(&path, false).await.unwrap();
    assert_eq!(
        ipc::next_frame(&mut reader).await.unwrap().schema_version,
        1
    );
    assert!(ipc::line(&mut reader, 4096).await.unwrap().is_none());
    let mut sub = ipc::connect(&path, true).await.unwrap();
    assert_eq!(
        ipc::next_frame(&mut sub).await.unwrap().daemon_instance,
        "test"
    );
    let mut changed = frame();
    changed.daemon_instance = "changed".into();
    tx.send_replace(Some(Cached {
        frame: changed,
        at: Instant::now(),
    }));
    loop {
        if ipc::next_frame(&mut sub).await.unwrap().daemon_instance == "changed" {
            break;
        }
    }
    for request in [
        r#"{"schema_version":99,"op":"snapshot"}"#,
        r#"{"schema_version":1,"op":"set_power"}"#,
    ] {
        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        assert!(
            ipc::line(&mut BufReader::new(stream), 4096)
                .await
                .unwrap()
                .is_none()
        );
    }
    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(guard);
    assert!(!path.exists());
}
#[tokio::test]
async fn slow_and_disconnected_subscribers_cannot_backpressure_publisher() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("socket");
    let (listener, _guard) = SocketGuard::bind(&path).unwrap();
    let (tx, rx) = watch::channel(Some(Cached {
        frame: frame(),
        at: Instant::now(),
    }));
    let (stop_tx, stop_rx) = watch::channel(false);
    let server = tokio::spawn(ipc::serve(listener, rx, stop_rx));
    let _slow = ipc::connect(&path, true).await.unwrap();
    let disconnected = ipc::connect(&path, true).await.unwrap();
    drop(disconnected);
    let start = Instant::now();
    for i in 0..10000 {
        let mut f = frame();
        f.daemon_instance = i.to_string();
        tx.send_replace(Some(Cached {
            frame: f,
            at: Instant::now(),
        }));
    }
    assert!(start.elapsed() < Duration::from_secs(2));
    let mut snapshot = ipc::connect(&path, false).await.unwrap();
    let f = ipc::next_frame(&mut snapshot).await.unwrap();
    assert_eq!(f.daemon_instance, "9999");
    stop_tx.send(true).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn stalled_socket_writer_is_evicted_while_snapshots_continue() {
    use tokio::io::AsyncReadExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("socket");
    let (listener, _guard) = SocketGuard::bind(&path).unwrap();
    // Deliberately large but bounded frame fills the socket buffer when unread.
    let mut large = frame();
    large.daemon_instance = "x".repeat(768 * 1024);
    let (tx, rx) = watch::channel(Some(Cached {
        frame: large,
        at: Instant::now(),
    }));
    let (stop_tx, stop_rx) = watch::channel(false);
    let server = tokio::spawn(ipc::serve(listener, rx, stop_rx));
    let mut slow = ipc::connect(&path, true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    tx.send_replace(Some(Cached {
        frame: frame(),
        at: Instant::now(),
    }));
    let mut fast = ipc::connect(&path, false).await.unwrap();
    assert_eq!(
        ipc::next_frame(&mut fast).await.unwrap().daemon_instance,
        "test"
    );
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let mut buffered = vec![];
    tokio::time::timeout(Duration::from_secs(1), slow.read_to_end(&mut buffered))
        .await
        .unwrap()
        .unwrap();
    assert!(!buffered.is_empty()); // server attempted the large write, then closed it
    assert!(buffered.len() < 768 * 1024); // incomplete frame must be discarded by clients
    stop_tx.send(true).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn notifier_transport_drains_fast_frames_and_reconnects_without_desktop() {
    use pinwatch::notify::{ConnectionState, follow};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("socket");
    let (listener, guard) = SocketGuard::bind(&path).unwrap();
    let (tx, rx) = watch::channel(Some(Cached {
        frame: frame(),
        at: Instant::now(),
    }));
    let (stop_tx, stop_rx) = watch::channel(false);
    let server = tokio::spawn(ipc::serve(listener, rx, stop_rx));
    let (output, mut received) = watch::channel(ConnectionState::default());
    let transport = tokio::spawn(follow(path.clone(), output));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            received.changed().await.unwrap();
            if received.borrow().frame.is_some() {
                break;
            }
        }
    })
    .await
    .unwrap();
    for n in 1..=20 {
        let mut f = frame();
        f.emitted_at_unix_ms = n;
        f.daemon_instance = n.to_string();
        tx.send_replace(Some(Cached {
            frame: f,
            at: Instant::now(),
        }));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The desktop side deliberately did not consume anything during those updates.
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if received
                .borrow()
                .frame
                .as_ref()
                .is_some_and(|f| f.daemon_instance == "20")
            {
                break;
            }
            received.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(received.borrow().generation, 1);
    stop_tx.send(true).unwrap();
    server.await.unwrap().unwrap();
    drop(guard);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            received.changed().await.unwrap();
            if received.borrow().frame.is_none() {
                break;
            }
        }
    })
    .await
    .unwrap();
    let (listener, _guard) = SocketGuard::bind(&path).unwrap();
    let (stop_tx, stop_rx) = watch::channel(false);
    let server = tokio::spawn(ipc::serve(listener, tx.subscribe(), stop_rx));
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            received.changed().await.unwrap();
            if received.borrow().generation == 2 && received.borrow().frame.is_some() {
                break;
            }
        }
    })
    .await
    .unwrap();
    transport.abort();
    let _ = transport.await;
    stop_tx.send(true).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn automatic_connection_only_falls_back_for_absent_or_stale_sockets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.sock");
    assert!(
        ipc::connect_if_running(&path, false)
            .await
            .unwrap()
            .is_none()
    );
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(listener);
    assert!(
        ipc::connect_if_running(&path, false)
            .await
            .unwrap()
            .is_none()
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "not a socket").unwrap();
    assert!(ipc::connect_if_running(&path, false).await.is_err());
}

#[tokio::test]
async fn automatic_connection_keeps_permission_errors_visible() {
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = ipc::connect_if_running(&locked.join("daemon.sock"), false).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    assert_eq!(
        result
            .err()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
}
