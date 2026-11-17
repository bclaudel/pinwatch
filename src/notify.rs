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
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
pub fn map_event(e: &Event, historical: bool) -> Option<Notice> {
    let (summary, severity) = match (e.kind.as_str(), e.action.as_ref()) {
        ("action_applied", Some(Action::PowerLimit { .. })) => ("GPU power limit reduced", 1),
        ("action_applied", Some(Action::ClockThrottle { .. })) => {
            ("GPU emergency clock restriction requested", 2)
        }
        ("action_verified", Some(Action::ClockThrottle { .. })) => {
            ("GPU emergency clock throttling observed", 2)
        }
        ("action_failed", _) => ("GPU protective intervention failed", 3),
        ("protection_unavailable", _) => ("GPU protection unavailable", 3),
        ("action_proposed", _) => ("Simulated GPU intervention", 1),
        ("action_requested", _) if historical => ("Historical GPU action: outcome unknown", 3),
        ("monitoring_recovered", _) => ("GPU monitoring restored", 0),
        _ => return None,
    };
    let trigger = match (e.channel_id, e.current_a) {
        (Some(c), Some(a)) => format!(
            "Channel {c} reached {a:.2} A (threshold {:.2} A). ",
            e.threshold_a
        ),
        _ => String::new(),
    };
    let action = match (&e.action, &e.outcome) {
        (Some(Action::PowerLimit { target_w }), Some(o)) if o.power_limit_w.is_some() => format!(
            "Power cap: {} W → {:.3} W (requested {target_w:.3} W). ",
            e.previous_power_cap_w
                .map_or("unknown".into(), |p| format!("{p:.3}")),
            o.power_limit_w.unwrap()
        ),
        (
            Some(Action::ClockThrottle {
                graphics_mhz,
                memory_mhz,
            }),
            Some(o),
        ) => format!(
            "Graphics {graphics_mhz} MHz setter acknowledged: {}; memory {memory_mhz} MHz setter acknowledged: {}. ",
            o.graphics_locked, o.memory_locked
        ),
        (Some(action), _) => format!("Requested/proposed: {action:?}. "),
        _ => String::new(),
    };
    let errors = e
        .outcome
        .as_ref()
        .map(|o| o.errors.join("; "))
        .unwrap_or_default();
    Some(Notice {
        key: e.gpu_uuid.clone(),
        identity: e.event_id.clone(),
        incident: e.incident_id.clone(),
        summary: format!(
            "{}{}{}",
            if e.simulation { "SIMULATION — " } else { "" },
            if historical { "Historical — " } else { "" },
            summary
        ),
        body: escape(&format!(
            "{}: {trigger}{action}{} {errors}{}",
            e.gpu_uuid,
            e.detail,
            if historical {
                " Historical record; consult current telemetry. No power or clock restoration is implied."
            } else {
                ""
            }
        )),
        urgency: if severity == 0 { 1 } else { 2 },
        severity,
    })
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
        let suppressed =
            self.last_sent
                .get(&n.key)
                .is_some_and(|(at, severity, body, incident)| {
                    n.severity != 0
                        && n.incident == *incident
                        && now.saturating_sub(*at) < 30000
                        && n.severity <= *severity
                        && (n.severity < 3 || n.body == *body)
                });
        self.active.insert(n.key.clone(), n.clone());
        if !suppressed {
            self.pending.insert(n.key.clone(), n);
        }
    }
    pub fn ingest(&mut self, frame: &Frame, now: u64, reconciliation: bool) {
        self.disconnected_since = None;
        if self.disconnect_announced {
            self.disconnect_announced = false;
            self.queue(Notice { key:"daemon".into(),incident:"daemon".into(),identity:format!("reconnected:{now}"),summary:"GPU monitoring connection restored".into(),body:"Daemon connection restored; no power limits or clocks were restored. Check per-GPU protection state.".into(),urgency:1,severity:0 },now);
        }
        let new_instance = self.instance.as_ref() != Some(&frame.daemon_instance);
        self.instance = Some(frame.daemon_instance.clone());
        let replay = reconciliation || new_instance;
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
                    && let Some(n) = map_event(e, true)
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
                    && let Some(n) = map_event(e, false)
                {
                    self.queue(n, now);
                }
            }
        }
        for gpu in &frame.gpus {
            let unhealthy = gpu.freshness != "fresh"
                || gpu.protection.state == "degraded"
                || (gpu.protection.mode == Mode::Active
                    && gpu.protection.state == "armed_unverified");
            let fingerprint = if unhealthy {
                format!("{}:{}", gpu.freshness, gpu.protection.state)
            } else {
                "available".into()
            };
            let previous = self
                .health
                .insert(gpu.device.gpu_uuid.clone(), fingerprint.clone());
            if previous.as_ref() != Some(&fingerprint)
                && unhealthy
                && !self.pending.contains_key(&gpu.device.gpu_uuid)
            {
                self.queue(
                    Notice {
                        key: gpu.device.gpu_uuid.clone(),
                        incident: "health".into(),
                        identity: format!("health:{}:{fingerprint}", frame.daemon_instance),
                        summary: format!(
                            "{}GPU protection unavailable/unverified",
                            if gpu.protection.mode == Mode::DryRun {
                                "SIMULATION — "
                            } else {
                                ""
                            }
                        ),
                        body: escape(&format!(
                            "{}: {} ({}). Restrictions are not restored.",
                            gpu.device.gpu_uuid, gpu.protection.reason, gpu.freshness
                        )),
                        urgency: 2,
                        severity: 3,
                    },
                    now,
                );
            } else if previous.is_some_and(|p| p != "available")
                && !unhealthy
                && !self.pending.contains_key(&gpu.device.gpu_uuid)
            {
                self.queue(Notice { key:gpu.device.gpu_uuid.clone(), incident:"recovery".into(), identity:format!("health-recovery:{now}"),summary:"GPU monitoring restored".into(),body:escape(&format!("{}: telemetry is available again. Power limits and clocks were not restored; check current protection state.",gpu.device.gpu_uuid)),urgency:1,severity:0 },now);
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
                    && let Some(notice) = map_event(event, true)
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
            self.queue(Notice { key:"daemon".into(),incident:"daemon".into(),identity:format!("disconnect:{since}"),summary:"GPU protection connection lost".into(),body:"Pinwatch daemon unavailable for at least 5 seconds. Protection cannot be verified; existing GPU restrictions are not restored.".into(),urgency:2,severity:3 },now);
        }
    }
    pub fn delivered(&mut self, key: &str, id: u32, now: u64) {
        if let Some(n) = self.pending.remove(key) {
            self.last_sent
                .insert(key.into(), (now, n.severity, n.body, n.incident));
            self.replacements.insert(key.into(), id);
        }
    }
    pub fn server_restarted(&mut self) {
        self.replacements.clear();
        for (key, n) in &self.active {
            if n.severity > 0 {
                let mut n = n.clone();
                n.summary = format!("Reconciled — {}", n.summary);
                self.pending.insert(key.clone(), n);
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
        desktop.send(&Notice { key:"test".into(),incident:"test".into(),identity:"explicit-synthetic-test".into(), summary:"SIMULATION — Pinwatch notification test".into(),body:"Synthetic test via org.freedesktop.Notifications. No GPU was queried or changed.".into(),urgency:2,severity:1 },0).await?;
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
