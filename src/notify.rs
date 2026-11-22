//! Unprivileged session companion. Notifications are never on the protection execution path.
use crate::{config::Mode, ipc, model::*};
use anyhow::{Context, Result};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::watch;
use zbus::zvariant::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub key: String,
    pub identity: String,
    pub incident: String,
    pub summary: String,
    pub body: String,
    pub urgency: u8,
    pub severity: u8,
    failure_detail: Option<String>,
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn gpu_label(device: &DeviceInfo, multiple: bool) -> String {
    let model = if device.model.contains("ASTRAL-RTX5090") {
        "Astral RTX 5090"
    } else {
        &device.model
    };
    if multiple {
        format!("{model} (GPU {})", device.index)
    } else {
        model.into()
    }
}

fn power_change(previous: Option<f64>, target: f64) -> String {
    previous.map_or_else(
        || format!("{target:.0} W"),
        |w| format!("{w:.0} → {target:.0} W"),
    )
}

pub fn map_event(e: &Event, historical: bool, label: &str) -> Option<Notice> {
    let trigger = match (e.channel_id, e.current_a) {
        (Some(c), Some(a)) => format!(
            "\nChannel {c} reached {a:.2} A (limit {:.2} A).",
            e.threshold_a
        ),
        _ => String::new(),
    };
    let (summary, message, severity) = match (e.kind.as_str(), e.action.as_ref()) {
        ("action_applied", Some(Action::PowerLimit { .. })) => {
            let watts = e.outcome.as_ref()?.power_limit_w?;
            (
                "GPU power limit reduced",
                format!("{}.{trigger}", power_change(e.previous_power_cap_w, watts)),
                1,
            )
        }
        ("action_applied", Some(Action::ClockThrottle { .. })) => (
            "GPU clock restriction requested",
            "Driver accepted the request. Checking clock speeds.".into(),
            2,
        ),
        ("action_verified", Some(Action::ClockThrottle { .. })) => (
            "GPU clock throttling confirmed",
            "Graphics and memory clocks reached their targets.".into(),
            2,
        ),
        ("action_failed", action) => {
            let reason = if e.detail.starts_with("sample became stale") {
                "Readings went stale before the action."
            } else {
                match action {
                    Some(Action::PowerLimit { .. }) => "Couldn’t verify the requested power limit.",
                    Some(Action::ClockThrottle { .. }) => {
                        "Couldn’t verify the requested clock restrictions."
                    }
                    None => "Couldn’t complete the protection action.",
                }
            };
            (
                "GPU protection action failed",
                format!("{reason} See Pinwatch logs."),
                3,
            )
        }
        ("action_proposed", Some(Action::PowerLimit { target_w })) => (
            "GPU power limit reduction",
            e.previous_power_cap_w.map_or_else(
                || format!("Would set the cap to {target_w:.0} W."),
                |w| format!("Would reduce the cap from {w:.0} to {target_w:.0} W."),
            ),
            1,
        ),
        (
            "action_proposed",
            Some(Action::ClockThrottle {
                graphics_mhz,
                memory_mhz,
            }),
        ) => (
            "GPU clock restriction",
            format!("Would cap graphics at {graphics_mhz} MHz and memory at {memory_mhz} MHz."),
            1,
        ),
        ("action_requested", _) if historical => (
            "Previous GPU action unconfirmed",
            "Check the current GPU settings.".into(),
            3,
        ),
        _ => return None,
    };
    let prefix = match (
        e.simulation || e.kind == "action_proposed",
        historical && e.kind != "action_requested",
    ) {
        (true, true) => "Simulation · Earlier · ",
        (true, false) => "Simulation · ",
        (false, true) => "Earlier · ",
        (false, false) => "",
    };
    Some(Notice {
        key: e.gpu_uuid.clone(),
        identity: e.event_id.clone(),
        incident: e.incident_id.clone(),
        summary: format!("{prefix}{summary}"),
        body: escape(&format!("{label} · {message}")),
        urgency: 2,
        severity,
        failure_detail: (e.kind == "action_failed").then(|| {
            format!(
                "{:?}: {} {:?}",
                e.action,
                e.detail,
                e.outcome.as_ref().map(|o| &o.errors)
            )
        }),
    })
}

fn health_message(gpu: &GpuSnapshot) -> &'static str {
    let reason = gpu.protection.reason.as_str();
    if gpu.freshness == "stale" || reason.contains("sample became stale") {
        "GPU readings are stale."
    } else if gpu.reading.channels.is_none() {
        "Connector readings are unavailable."
    } else if gpu.reading.nvml_board_power_w.is_none() || gpu.reading.power_limit_w.is_none() {
        "GPU power readings are unavailable."
    } else if reason.contains("overload persists") {
        "Current is still above the limit. See Pinwatch logs."
    } else if reason.contains("clock fallback unavailable") {
        "The required clock restrictions are unavailable. See Pinwatch logs."
    } else if reason.contains("power constraints") {
        "GPU power limit range is unavailable."
    } else {
        "Check Pinwatch logs for the cause."
    }
}
#[derive(Default)]
pub struct Manager {
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    pub pending: HashMap<String, Notice>,
    pub active: HashMap<String, Notice>,
    last_sent: HashMap<String, (u64, u8, String, String)>,
    pub replacements: HashMap<String, u32>,
    disconnected_since: Option<u64>,
    disconnect_announced: bool,
    health: HashMap<String, String>,
    instance: Option<String>,
    unhealthy_since: HashMap<String, u64>,
    health_announced: HashSet<String>,
}
impl Manager {
    fn remember(&mut self, id: &str) -> bool {
        if !self.seen.insert(id.into()) {
            return false;
        }
        self.seen_order.push_back(id.into());
        while self.seen_order.len() > 512 {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
    pub fn queue(&mut self, n: Notice, now: u64) {
        // New failures/escalations bypass rate limiting; repeated interventions within 30s coalesce.
        let signature = n.failure_detail.as_ref().unwrap_or(&n.body);
        let suppressed = self.last_sent.get(&n.key).is_some_and(
            |(at, severity, previous_signature, incident)| {
                n.severity != 0
                    && n.incident == *incident
                    && now.saturating_sub(*at) < 30000
                    && n.severity <= *severity
                    && (n.severity < 3 || signature == previous_signature)
            },
        );
        self.active.insert(n.key.clone(), n.clone());
        if !suppressed {
            self.pending.insert(n.key.clone(), n);
        }
    }
    pub fn ingest(&mut self, frame: &Frame, now: u64, reconciliation: bool) {
        self.disconnected_since = None;
        if self.disconnect_announced {
            self.disconnect_announced = false;
            self.queue(
                Notice {
                    key: "daemon".into(),
                    incident: "daemon".into(),
                    identity: format!("reconnected:{now}"),
                    summary: "Pinwatch reconnected".into(),
                    body: "GPU monitoring connection restored. Existing limits remain in place."
                        .into(),
                    urgency: 1,
                    severity: 0,
                    failure_detail: None,
                },
                now,
            );
        }
        let new_instance = self.instance.as_ref() != Some(&frame.daemon_instance);
        self.instance = Some(frame.daemon_instance.clone());
        let replay = reconciliation || new_instance;
        let label = |gpu: &GpuSnapshot| gpu_label(&gpu.device, frame.gpus.len() > 1);
        if replay {
            // At most one relevant recent event per GPU, not a backlog of obsolete popups.
            for gpu in &frame.gpus {
                let event = frame
                    .recent_events
                    .iter()
                    .rev()
                    .find(|e| {
                        e.gpu_uuid == gpu.device.gpu_uuid
                            && match gpu.protection.state.as_str() {
                                "degraded" => matches!(
                                    e.kind.as_str(),
                                    "action_failed" | "protection_unavailable"
                                ),
                                "restricted" | "waiting" => {
                                    matches!(e.kind.as_str(), "action_applied" | "action_verified")
                                }
                                "dry_run" => e.kind == "action_proposed",
                                _ => false,
                            }
                    })
                    .or(gpu.protection.historical_restriction.as_ref());
                if let Some(e) = event
                    && self.remember(&e.event_id)
                    && e.kind != "protection_unavailable"
                    && let Some(n) = map_event(e, true, &label(gpu))
                {
                    self.queue(n, now);
                }
            }
            for e in &frame.recent_events {
                self.remember(&e.event_id);
            }
        } else {
            for e in &frame.recent_events {
                if self.remember(&e.event_id)
                    // Health notices follow current state, not obsolete event history.
                    && !matches!(e.kind.as_str(), "protection_unavailable" | "monitoring_recovered")
                {
                    let device_label = frame
                        .gpus
                        .iter()
                        .find(|gpu| gpu.device.gpu_uuid == e.gpu_uuid)
                        .map_or_else(|| e.gpu_uuid.clone(), &label);
                    if let Some(n) = map_event(e, false, &device_label) {
                        self.queue(n, now);
                    }
                }
            }
        }
        for gpu in &frame.gpus {
            // Armed means active monitoring with no intervention yet. It is not
            // a failure; setter/readback failures arrive as events or degraded state.
            let unhealthy = gpu.freshness != "fresh" || gpu.protection.state == "degraded";
            let fingerprint = if unhealthy {
                format!("{}:{}", gpu.freshness, gpu.protection.state)
            } else {
                "available".into()
            };
            let previous = self
                .health
                .insert(gpu.device.gpu_uuid.clone(), fingerprint.clone());
            let uuid = &gpu.device.gpu_uuid;
            let since = if unhealthy {
                *self.unhealthy_since.entry(uuid.clone()).or_insert(now)
            } else {
                self.unhealthy_since.remove(uuid);
                // A health warning that never reached the desktop is now obsolete.
                if self
                    .pending
                    .get(uuid)
                    .is_some_and(|n| n.incident == "health")
                {
                    self.pending.remove(uuid);
                }
                now
            };
            // Notification-only grace covers short resume/driver transitions. Hardware
            // monitoring, intervention failures and the event log remain immediate.
            if unhealthy
                && now.saturating_sub(since) >= 3000
                && !self.health_announced.contains(uuid)
                && !self.pending.contains_key(uuid)
            {
                self.queue(
                    Notice {
                        key: gpu.device.gpu_uuid.clone(),
                        incident: "health".into(),
                        identity: format!("health:{}:{fingerprint}", frame.daemon_instance),
                        summary: format!(
                            "{}GPU protection unavailable",
                            if gpu.protection.mode == Mode::DryRun
                                || gpu.device.model.starts_with("SIMULATED ")
                            {
                                "Simulation · "
                            } else {
                                ""
                            }
                        ),
                        body: escape(&format!("{} · {}", label(gpu), health_message(gpu))),
                        urgency: 2,
                        severity: 3,
                        failure_detail: None,
                    },
                    now,
                );
            } else if previous.is_some_and(|p| p != "available")
                && !unhealthy
                && self.health_announced.remove(uuid)
                && !self.pending.contains_key(&gpu.device.gpu_uuid)
            {
                self.queue(
                    Notice {
                        key: gpu.device.gpu_uuid.clone(),
                        incident: "recovery".into(),
                        identity: format!("health-recovery:{now}"),
                        summary: "GPU monitoring restored".into(),
                        body: escape(&format!(
                            "{} · Readings are back. Existing limits remain in place.",
                            label(gpu)
                        )),
                        urgency: 1,
                        severity: 0,
                        failure_detail: None,
                    },
                    now,
                );
            }
            if !unhealthy {
                let restriction = frame
                    .recent_events
                    .iter()
                    .rev()
                    .find(|e| {
                        e.gpu_uuid == gpu.device.gpu_uuid
                            && matches!(e.kind.as_str(), "action_applied" | "action_verified")
                    })
                    .or(gpu.protection.historical_restriction.as_ref());
                if let Some(event) = restriction
                    && let Some(notice) = map_event(event, true, &label(gpu))
                {
                    self.active.insert(gpu.device.gpu_uuid.clone(), notice);
                } else {
                    self.active.remove(&gpu.device.gpu_uuid);
                }
            }
        }
    }
    pub fn disconnected(&mut self, now: u64) {
        let since = *self.disconnected_since.get_or_insert(now);
        if now.saturating_sub(since) >= 5000 && !self.disconnect_announced {
            self.disconnect_announced = true;
            self.queue(
                Notice {
                    key: "daemon".into(),
                    incident: "daemon".into(),
                    identity: format!("disconnect:{since}"),
                    summary: "Pinwatch disconnected".into(),
                    body: "GPU protection status is unknown.".into(),
                    urgency: 2,
                    severity: 3,
                    failure_detail: None,
                },
                now,
            );
        }
    }
    pub fn delivered(&mut self, key: &str, id: u32, now: u64) {
        if let Some(n) = self.pending.remove(key) {
            if n.severity == 3 {
                self.health_announced.insert(key.into());
            }
            self.last_sent.insert(
                key.into(),
                (
                    now,
                    n.severity,
                    n.failure_detail.unwrap_or(n.body),
                    n.incident,
                ),
            );
            self.replacements.insert(key.into(), id);
        }
    }
    pub fn server_restarted(&mut self) {
        self.replacements.clear();
        for (key, n) in &self.active {
            if n.severity > 0 {
                self.pending.insert(key.clone(), n.clone());
            }
        }
    }
}
pub trait NotificationSink {
    fn send(
        &mut self,
        notice: &Notice,
        replaces: u32,
    ) -> impl std::future::Future<Output = Result<u32>> + Send;
}
pub async fn deliver_pending(
    manager: &mut Manager,
    sink: &mut impl NotificationSink,
    now: u64,
) -> Result<()> {
    let mut keys: Vec<_> = manager.pending.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let notice = manager.pending[&key].clone();
        let id = sink
            .send(&notice, *manager.replacements.get(&key).unwrap_or(&0))
            .await?;
        manager.delivered(&key, id, now);
    }
    Ok(())
}
pub struct Desktop {
    connection: zbus::Connection,
}
impl Desktop {
    pub async fn connect() -> Result<Self> {
        let connection = tokio::time::timeout(
            Duration::from_secs(3),
            zbus::connection::Builder::session()?
                .name("org.pinwatch.Notifier")?
                .method_timeout(Duration::from_secs(3))
                .build(),
        )
        .await??;
        Ok(Self { connection })
    }
    async fn owner(&self) -> Result<String> {
        let proxy = zbus::fdo::DBusProxy::new(&self.connection).await?;
        Ok(proxy
            .get_name_owner("org.freedesktop.Notifications".try_into()?)
            .await?
            .to_string())
    }
}
impl NotificationSink for Desktop {
    async fn send(&mut self, notice: &Notice, replaces: u32) -> Result<u32> {
        let proxy = zbus::Proxy::new(
            &self.connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await?;
        let mut hints = HashMap::new();
        hints.insert("urgency", Value::U8(notice.urgency));
        let actions: Vec<&str> = vec![];
        let id: u32 = tokio::time::timeout(
            Duration::from_secs(3),
            proxy.call(
                "Notify",
                &(
                    "Pinwatch",
                    replaces,
                    "dialog-warning",
                    notice.summary.as_str(),
                    notice.body.as_str(),
                    actions,
                    hints,
                    0_i32,
                ),
            ),
        )
        .await??;
        Ok(id)
    }
}
pub fn backoff_ms(attempt: u32) -> u64 {
    (1000_u64.saturating_mul(1_u64 << attempt.min(5))).min(30000)
}
/// Latest-value transport keeps draining IPC while desktop delivery is slow/unavailable.
#[derive(Clone, Default)]
pub struct ConnectionState {
    pub generation: u64,
    pub frame: Option<Frame>,
}
pub async fn follow(socket: PathBuf, output: watch::Sender<ConnectionState>) {
    let mut generation = 0;
    let mut attempt: u32 = 0;
    loop {
        let result = async {
            let mut reader = ipc::connect(&socket, true).await?;
            let first = ipc::next_frame(&mut reader).await?;
            generation += 1;
            attempt = 0;
            output.send_replace(ConnectionState {
                generation,
                frame: Some(first),
            });
            loop {
                let frame = ipc::next_frame(&mut reader).await?;
                if output.is_closed() {
                    return Ok::<(), anyhow::Error>(());
                }
                output.send_replace(ConnectionState {
                    generation,
                    frame: Some(frame),
                });
            }
        }
        .await;
        if output.is_closed() {
            return;
        }
        if let Err(error) = result {
            eprintln!("notifier IPC: {error}");
        }
        output.send_replace(ConnectionState {
            generation,
            frame: None,
        });
        tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
        attempt = attempt.saturating_add(1);
    }
}
struct AbortTransport(tokio::task::JoinHandle<()>);
impl Drop for AbortTransport {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub async fn run(socket: &Path, test: bool) -> Result<()> {
    // Name acquisition is DoNotQueue: exactly one companion per session/user bus.
    let mut desktop = Desktop::connect().await.context(
        "cannot own session notifier name; no session bus or another companion is running",
    )?;
    if test {
        desktop
            .send(
                &Notice {
                    key: "test".into(),
                    incident: "test".into(),
                    identity: "explicit-synthetic-test".into(),
                    summary: "Pinwatch notification test".into(),
                    body: "Test alert from Pinwatch.".into(),
                    urgency: 2,
                    severity: 1,
                    failure_detail: None,
                },
                0,
            )
            .await?;
        return Ok(());
    }
    let start = Instant::now();
    let mut manager = Manager::default();
    let (output, mut received) = watch::channel(ConnectionState::default());
    let _transport = AbortTransport(tokio::spawn(follow(socket.into(), output)));
    let mut generation = 0;
    let mut delivery_attempt: u32 = 0;
    let mut delivery_at = 0;
    let mut owner: Option<String> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tick.tick() => {},
            changed = received.changed() => { changed.context("notifier transport stopped")?; },
        }
        let now = start.elapsed().as_millis() as u64;
        let state = received.borrow_and_update().clone();
        if let Some(frame) = state.frame {
            manager.ingest(&frame, now, state.generation != generation);
            generation = state.generation;
        } else {
            manager.disconnected(now);
        }
        if now >= delivery_at {
            match desktop.owner().await {
                Ok(new_owner) => {
                    if owner.as_ref() != Some(&new_owner) {
                        if owner.is_some() {
                            manager.server_restarted();
                        }
                        owner = Some(new_owner);
                    }
                    match deliver_pending(&mut manager, &mut desktop, now).await {
                        Ok(()) => {
                            delivery_attempt = 0;
                            delivery_at = now + 1000;
                        }
                        Err(e) => {
                            eprintln!("desktop notification unavailable: {e}");
                            delivery_at = now + backoff_ms(delivery_attempt);
                            delivery_attempt = delivery_attempt.saturating_add(1);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("notification service unavailable: {e}");
                    delivery_at = now + backoff_ms(delivery_attempt);
                    delivery_attempt = delivery_attempt.saturating_add(1);
                }
            }
        }
    }
}
