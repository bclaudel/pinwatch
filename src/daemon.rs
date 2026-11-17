use crate::{
    backend,
    config::Config,
    ipc::{self, Cached, Ownership, SocketGuard},
    model::*,
    policy::Engine,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct History {
    recent: VecDeque<Event>,
    restrictions: BTreeMap<String, Event>,
}
pub struct Journal {
    instance: String,
    sequence: u64,
    history: History,
    path: PathBuf,
    _owner: Ownership,
}
impl Journal {
    pub fn open(path: &Path, instance: &str) -> Result<Self> {
        let parent = path.parent().context("history path has no parent")?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("invalid history filename")?
            .replace('.', "_");
        let owner = Ownership::acquire(parent, &format!("history-{name}"))?;
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "history must be a regular file, not a symlink"
            );
            ensure!(meta.len() <= 1024 * 1024, "history file too large");
        }
        let history = match std::fs::read(path) {
            Ok(bytes) => {
                ensure!(bytes.len() <= 1024 * 1024, "history file too large");
                serde_json::from_slice(&bytes)
                    .context("invalid history; refusing to discard restriction records")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => History::default(),
            Err(e) => return Err(e.into()),
        };
        let journal = Self {
            instance: instance.into(),
            sequence: 0,
            history,
            path: path.into(),
            _owner: owner,
        };
        journal.persist()?;
        Ok(journal)
    }
    fn persist(&self) -> Result<()> {
        // Private directory + unique create_new temporary + atomic rename + fsync.
        let temporary = self
            .path
            .with_extension(format!("{}.lock.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            serde_json::to_writer(&mut f, &self.history)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            std::fs::rename(&temporary, &self.path)?;
            std::fs::File::open(self.path.parent().unwrap())?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }
    pub fn emit(&mut self, mut event: Event) -> Result<()> {
        self.sequence += 1;
        event.sequence = self.sequence;
        event.daemon_instance = self.instance.clone();
        event.event_id = format!("{}:{}", self.instance, self.sequence);
        eprintln!("{}", serde_json::to_string(&event)?); // structured journald capture
        if matches!(
            event.kind.as_str(),
            "action_requested" | "action_applied" | "action_failed"
        ) && !event.simulation
        {
            self.history
                .restrictions
                .insert(event.gpu_uuid.clone(), event.clone());
        }
        self.history.recent.push_back(event);
        while self.history.recent.len() > 64 {
            self.history.recent.pop_front();
        }
        self.persist()
    }
    pub fn recent(&self) -> Vec<Event> {
        self.history.recent.iter().cloned().collect()
    }
    pub fn historical(&self, uuid: &str) -> Option<Event> {
        self.history.restrictions.get(uuid).cloned()
    }
}
pub async fn run(config: Config) -> Result<()> {
    config.validate()?;
    if config.simulate.is_none() {
        ensure!(
            config.lock_dir == Path::new("/run/pinwatch"),
            "real hardware ownership always uses /run/pinwatch; only simulations may override lock_dir"
        );
    }
    let (listener, _socket) = SocketGuard::bind(&config.socket)?;
    let (cache_tx, cache_rx) = watch::channel(None);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let stopping = Arc::new(AtomicBool::new(false));
    let worker_stopping = stopping.clone();
    let mut worker = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut backend = backend::open(&config)?;
        let devices = backend::select(&backend.devices()?, config.gpu_uuid.as_deref(), None)?;
        let devices: Vec<_> = devices.into_iter().filter(|d| d.sensor_supported).collect();
        ensure!(!devices.is_empty(), "no supported ASUS Astral GPU found");
        // The socket is bound before GPU ownership. Give automatic local watchers
        // time to see it and release their locks; never unlink/bypass a lock.
        let ownership_started = Instant::now();
        let _owners = loop {
            if worker_stopping.load(Ordering::Relaxed) {
                return Ok(());
            }
            let result = devices
                .iter()
                .map(|d| Ownership::acquire(&config.lock_dir, &d.gpu_uuid))
                .collect::<Result<Vec<_>>>();
            match result {
                Ok(owners) => break owners,
                Err(error)
                    if ownership_started.elapsed() < Duration::from_secs(6)
                        && error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock) =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error).context("GPU polling ownership unavailable"),
            }
        };
        let instance = uuid::Uuid::new_v4().to_string();
        let mut journal = Journal::open(&config.state_file, &instance)?;
        let mut engines: Vec<_> = devices
            .iter()
            .map(|d| Engine::new(config.mode, journal.historical(&d.gpu_uuid)))
            .collect();
        let started = Instant::now();
        let mut sequence = 0;
        while !worker_stopping.load(Ordering::Relaxed) {
            let poll = Instant::now();
            sequence += 1;
            let mut snapshots = vec![];
            let mut times = vec![];
            for (device, engine) in devices.iter().zip(&mut engines) {
                let sampled = Instant::now();
                let reading = backend.sample(device, sequence);
                let age = (sampled.elapsed().as_millis() as u64).max(reading.sample_duration_ms);
                if let Err(e) = engine.tick(
                    &config,
                    device,
                    &reading,
                    age,
                    started.elapsed().as_millis() as u64,
                    backend.as_mut(),
                    &mut |e| journal.emit(e),
                ) {
                    eprintln!("event persistence failure: {e:#}; protection degraded");
                    engine.protection.state = "degraded".into();
                    engine.protection.effective = false;
                    engine.protection.reason =
                        format!("event persistence failed: {e}; no further action this poll");
                }
                times.push(sampled);
                snapshots.push(GpuSnapshot {
                    device: device.clone(),
                    reading,
                    sample_age_ms: 0,
                    freshness: "fresh".into(),
                    protection: engine.protection.clone(),
                });
            }
            for (snapshot, at) in snapshots.iter_mut().zip(times) {
                snapshot.sample_age_ms =
                    (at.elapsed().as_millis() as u64).max(snapshot.reading.sample_duration_ms);
            }
            let frame = Frame {
                schema_version: SCHEMA_VERSION,
                kind: "telemetry".into(),
                daemon_instance: instance.clone(),
                emitted_at_unix_ms: now_ms(),
                max_sample_age_ms: config.max_sample_age_ms,
                gpus: snapshots,
                recent_events: journal.recent(),
            };
            cache_tx.send_replace(Some(Cached {
                frame,
                at: Instant::now(),
            }));
            let interval = Duration::from_millis(config.poll_interval_ms);
            while poll.elapsed() < interval && !worker_stopping.load(Ordering::Relaxed) {
                std::thread::sleep(
                    Duration::from_millis(25).min(interval.saturating_sub(poll.elapsed())),
                );
            }
        }
        eprintln!("pinwatchd stopped; GPU caps and clocks left unchanged");
        Ok(())
    });
    let server = tokio::spawn(ipc::serve(listener, cache_rx, shutdown_rx));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let worker_result = tokio::select! {
        result=&mut worker => Some(result),
        _=tokio::signal::ctrl_c() => None,
        _=term.recv() => None,
    };
    stopping.store(true, Ordering::Relaxed);
    let _ = shutdown_tx.send(true);
    server.await??;
    if let Some(result) = worker_result {
        result??;
    } else {
        worker.await??;
    }
    Ok(())
}
