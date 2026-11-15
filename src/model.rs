use crate::config::Mode;
use serde::{Deserialize, Serialize};
pub const SCHEMA_VERSION: u32 = 1;
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Channel {
    pub channel_id: u8,
    pub voltage_v: f64,
    pub current_a: f64,
    pub power_w: f64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub power_min_w: Option<f64>,
    pub power_max_w: Option<f64>,
    pub graphics_min_mhz: Option<u32>,
    pub memory_min_mhz: Option<u32>,
    /// Queryable ranges do not prove write permission or driver support for setters.
    pub control_status: String,
    pub errors: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub gpu_uuid: String,
    pub index: u32,
    pub pci_address: String,
    pub model: String,
    pub sensor_supported: bool,
    pub i2c_adapters: Vec<u32>,
    pub selected_adapter: Option<u32>,
    pub sensor_error: Option<String>,
    pub capabilities: Capabilities,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reading {
    pub sample_sequence: u64,
    pub sampled_at_unix_ms: u64,
    pub sample_duration_ms: u64,
    pub channels: Option<[Channel; 6]>,
    pub connector_total_w: Option<f64>,
    pub nvml_board_power_w: Option<f64>,
    pub power_limit_w: Option<f64>,
    pub power_balance_ratio: Option<f64>,
    pub current_balance_ratio: Option<f64>,
    pub sensor_errors: Vec<String>,
    pub nvml_errors: Vec<String>,
}
impl Reading {
    pub fn empty(sequence: u64) -> Self {
        Self {
            sample_sequence: sequence,
            sampled_at_unix_ms: now_ms(),
            sample_duration_ms: 0,
            channels: None,
            connector_total_w: None,
            nvml_board_power_w: None,
            power_limit_w: None,
            power_balance_ratio: None,
            current_balance_ratio: None,
            sensor_errors: vec![],
            nvml_errors: vec![],
        }
    }
    pub fn set_channels(&mut self, channels: [Channel; 6]) {
        self.connector_total_w = Some(channels.iter().map(|c| c.power_w).sum());
        self.power_balance_ratio = ratio(channels.iter().map(|c| c.power_w));
        self.current_balance_ratio = ratio(channels.iter().map(|c| c.current_a));
        self.channels = Some(channels);
    }
    pub fn valid(&self) -> bool {
        self.channels.as_ref().is_some_and(|cs| {
            cs.iter().enumerate().all(|(i, c)| {
                c.channel_id == i as u8 + 1
                    && c.voltage_v.is_finite()
                    && c.voltage_v > 0.0
                    && c.current_a.is_finite()
                    && c.current_a >= 0.0
                    && c.power_w.is_finite()
                    && c.power_w >= 0.0
            })
        }) && self
            .nvml_board_power_w
            .is_some_and(|p| p.is_finite() && p > 0.0)
            && self.power_limit_w.is_some_and(|p| p.is_finite() && p > 0.0)
            && self.sensor_errors.is_empty()
            && self.nvml_errors.is_empty()
    }
}
pub fn ratio(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (mut min, mut max) = (f64::INFINITY, 0.0_f64);
    for v in values {
        if !v.is_finite() || v < 0.0 {
            return None;
        }
        min = min.min(v);
        max = max.max(v);
    }
    (max > 0.0).then_some(min / max)
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    PowerLimit { target_w: f64 },
    ClockThrottle { graphics_mhz: u32, memory_mhz: u32 },
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Outcome {
    pub power_limit_w: Option<f64>,
    pub graphics_locked: bool,
    pub memory_locked: bool,
    pub errors: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Protection {
    pub mode: Mode,
    pub state: String,
    pub effective: bool,
    pub reason: String,
    pub last_action: Option<Action>,
    pub last_outcome: Option<Outcome>,
    pub historical_restriction: Option<Event>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuSnapshot {
    pub device: DeviceInfo,
    pub reading: Reading,
    pub sample_age_ms: u64,
    pub freshness: String,
    pub protection: Protection,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub daemon_instance: String,
    pub sequence: u64,
    pub event_id: String,
    pub incident_id: String,
    pub timestamp_unix_ms: u64,
    pub gpu_uuid: String,
    pub kind: String,
    pub simulation: bool,
    pub channel_id: Option<u8>,
    pub current_a: Option<f64>,
    pub threshold_a: f64,
    pub previous_power_cap_w: Option<f64>,
    pub action: Option<Action>,
    pub outcome: Option<Outcome>,
    pub detail: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub schema_version: u32,
    pub kind: String,
    pub daemon_instance: String,
    pub emitted_at_unix_ms: u64,
    pub max_sample_age_ms: u64,
    pub gpus: Vec<GpuSnapshot>,
    pub recent_events: Vec<Event>,
}
impl Frame {
    pub fn refresh(&mut self, age_ms: u64) {
        self.emitted_at_unix_ms = now_ms();
        for g in &mut self.gpus {
            // Age is computed from a monotonic cache clock; wall timestamps are informational.
            g.sample_age_ms = age_ms.saturating_add(g.sample_age_ms);
            g.freshness = if g.sample_age_ms > self.max_sample_age_ms {
                "stale"
            } else if g.reading.channels.is_none() {
                "unavailable"
            } else {
                "fresh"
            }
            .into();
            if g.freshness != "fresh" {
                g.protection.effective = false;
                g.protection.state = "degraded".into();
                g.protection.reason = format!(
                    "{} sensor telemetry; restrictions are not restored",
                    g.freshness
                );
            }
        }
    }
}
