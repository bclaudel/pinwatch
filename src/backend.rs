use crate::{config::Config, model::*, sensor};
use anyhow::{Context, Result, bail, ensure};
use nvml_wrapper::{Nvml, enum_wrappers::device::Clock, enums::device::GpuLockedClocksSetting};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Instant};

pub trait Backend: Send {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>>;
    fn sample(&mut self, device: &DeviceInfo, sequence: u64) -> Reading;
    fn set_power(&mut self, uuid: &str, target_w: f64) -> Result<f64>;
    fn lock_graphics(&mut self, uuid: &str, mhz: u32) -> Result<()>;
    fn lock_memory(&mut self, uuid: &str, mhz: u32) -> Result<()>;
}
pub fn open(config: &Config) -> Result<Box<dyn Backend>> {
    match &config.simulate {
        Some(p) => Ok(Box::new(SimBackend::load(p)?)),
        None => Ok(Box::new(Hardware::new(config.clone())?)),
    }
}
pub fn select(
    devices: &[DeviceInfo],
    uuid: Option<&str>,
    index: Option<u32>,
) -> Result<Vec<DeviceInfo>> {
    ensure!(
        uuid.is_none() || index.is_none(),
        "choose UUID or index, not both"
    );
    let found: Vec<_> = devices
        .iter()
        .filter(|d| uuid.is_none_or(|u| d.gpu_uuid == u) && index.is_none_or(|i| d.index == i))
        .cloned()
        .collect();
    ensure!(!found.is_empty(), "no matching GPU");
    Ok(found)
}
pub struct Hardware {
    nvml: Nvml,
    config: Config,
}
impl Hardware {
    pub fn new(config: Config) -> Result<Self> {
        if let Some(path) = &config.nvml_library {
            ensure!(
                path.is_absolute(),
                "explicit NVML library path must be absolute"
            );
        }
        let paths = if let Some(p) = &config.nvml_library {
            vec![p.clone()]
        } else {
            vec![
                "libnvidia-ml.so.1".into(),
                "/usr/lib/libnvidia-ml.so.1".into(),
                "/run/opengl-driver/lib/libnvidia-ml.so.1".into(),
            ]
        };
        let mut errors = vec![];
        for path in paths {
            match Nvml::builder().lib_path(path.as_os_str()).init() {
                Ok(nvml) => {
                    eprintln!("NVML loaded from {}", path.display());
                    return Ok(Self { nvml, config });
                }
                Err(e) => errors.push(format!("{}: {e}", path.display())),
            }
        }
        bail!("cannot initialize dynamic NVML: {}", errors.join("; "))
    }
}
fn capabilities(device: &nvml_wrapper::Device<'_>) -> Capabilities {
    let mut caps = Capabilities {
        control_status: "unverified: ranges do not prove setter permission/support".into(),
        ..Default::default()
    };
    match device.power_management_limit_constraints() {
        Ok(p) => {
            caps.power_min_w = Some(f64::from(p.min_limit) / 1000.0);
            caps.power_max_w = Some(f64::from(p.max_limit) / 1000.0);
        }
        Err(e) => caps.errors.push(format!("power constraints: {e}")),
    }
    // Each clock domain is queried independently; missing control support never hides sensors.
    for (clock, slot) in [
        (Clock::Graphics, &mut caps.graphics_min_mhz),
        (Clock::Memory, &mut caps.memory_min_mhz),
    ] {
        let result = (|| -> Result<u32> {
            let states = device.supported_performance_states()?;
            let mut min = None;
            for state in states {
                let (lo, hi) = device.min_max_clock_of_pstate(clock, state)?;
                ensure!(lo > 0 && hi >= lo, "invalid clock range");
                min = Some(min.map_or(lo, |m: u32| m.min(lo)));
            }
            min.context("empty performance state list")
        })();
        match result {
            Ok(v) => *slot = Some(v),
            Err(e) => caps.errors.push(format!("{clock:?} clock minimum: {e}")),
        }
    }
    caps
}
impl Backend for Hardware {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>> {
        let mut devices = vec![];
        for index in 0..self.nvml.device_count()? {
            let device = self.nvml.device_by_index(index)?;
            let pci = device.pci_info()?;
            // Preserve PCI function from NVML instead of assuming .0. Normalize the domain width.
            let function = pci
                .bus_id
                .rsplit('.')
                .next()
                .context("invalid NVML PCI address")?;
            let function: u8 = function.parse()?;
            ensure!(function <= 7, "invalid PCI function");
            let pci_address = format!(
                "{:04x}:{:02x}:{:02x}.{function}",
                pci.domain, pci.bus, pci.device
            );
            let uuid = device.uuid()?;
            let model = sensor::model(pci.pci_device_id, pci.pci_sub_system_id.unwrap_or(0));
            let (adapters, error) =
                match sensor::adapters(&Path::new("/sys/bus/pci/devices").join(&pci_address)) {
                    Ok(a) => (a, None),
                    Err(e) => (vec![], Some(e.to_string())),
                };
            let selection =
                sensor::select_adapter(&adapters, self.config.adapters.get(&uuid).copied());
            let selected_adapter = if model.is_some() {
                selection.as_ref().ok().copied()
            } else {
                None
            };
            let sensor_error = if model.is_none() {
                Some("unsupported PCI device/subsystem; no sensor access".into())
            } else {
                error.or_else(|| selection.err().map(|e| e.to_string()))
            };
            devices.push(DeviceInfo {
                gpu_uuid: uuid,
                index,
                pci_address,
                model: model.map(str::to_string).unwrap_or(device.name()?),
                sensor_supported: model.is_some(),
                i2c_adapters: adapters,
                selected_adapter,
                sensor_error,
                capabilities: capabilities(&device),
            });
        }
        Ok(devices)
    }
    fn sample(&mut self, info: &DeviceInfo, sequence: u64) -> Reading {
        let start = Instant::now();
        let mut reading = Reading::empty(sequence);
        // Resolve every time by UUID; never let a recycled NVML index select another GPU.
        match self.nvml.device_by_uuid(info.gpu_uuid.as_str()) {
            Ok(device) => {
                // Verify the PCI identity before accessing a cached adapter path.
                let identity_ok = device.pci_info().is_ok_and(|p| {
                    sensor::model(p.pci_device_id, p.pci_sub_system_id.unwrap_or(0)).is_some()
                        && format!(
                            "{:04x}:{:02x}:{:02x}.{}",
                            p.domain,
                            p.bus,
                            p.device,
                            p.bus_id.rsplit('.').next().unwrap_or("?")
                        ) == info.pci_address
                });
                let sensor_result = (|| -> Result<_> {
                    ensure!(
                        identity_ok && info.sensor_supported,
                        "GPU identity changed or unsupported"
                    );
                    let adapter = info
                        .selected_adapter
                        .context("sensor adapter unavailable; see device.sensor_error")?;
                    let current = sensor::adapters(
                        &Path::new("/sys/bus/pci/devices").join(&info.pci_address),
                    )?;
                    sensor::select_adapter(&current, Some(adapter))?;
                    sensor::read(adapter)
                })();
                match sensor_result {
                    Ok(c) => reading.set_channels(c),
                    Err(e) => reading.sensor_errors.push(e.to_string()),
                }
                match device.power_usage() {
                    Ok(p) => reading.nvml_board_power_w = Some(f64::from(p) / 1000.0),
                    Err(e) => reading.nvml_errors.push(format!("board power: {e}")),
                }
                match device.power_management_limit() {
                    Ok(p) => reading.power_limit_w = Some(f64::from(p) / 1000.0),
                    Err(e) => reading.nvml_errors.push(format!("power cap: {e}")),
                }
                match device.clock_info(Clock::Graphics) {
                    Ok(mhz) => reading.graphics_clock_mhz = Some(mhz),
                    Err(e) => reading.clock_errors.push(format!("graphics clock: {e}")),
                }
                match device.clock_info(Clock::Memory) {
                    Ok(mhz) => reading.memory_clock_mhz = Some(mhz),
                    Err(e) => reading.clock_errors.push(format!("memory clock: {e}")),
                }
            }
            Err(e) => {
                reading
                    .sensor_errors
                    .push("GPU unavailable; sensor not accessed".into());
                reading.nvml_errors.push(e.to_string());
            }
        }
        reading.sample_duration_ms = start.elapsed().as_millis() as u64;
        reading
    }
    fn set_power(&mut self, uuid: &str, target_w: f64) -> Result<f64> {
        ensure!(
            target_w.is_finite() && target_w > 0.0 && target_w <= f64::from(u32::MAX) / 1000.0,
            "invalid power target"
        );
        let mut device = self.nvml.device_by_uuid(uuid)?;
        let constraints = device.power_management_limit_constraints()?;
        // Re-read immediately before writing, including changes by other management software.
        let current = device.power_management_limit()?;
        let target = ((target_w * 1000.0).floor() as u32).min(current);
        ensure!(
            target >= constraints.min_limit && target <= constraints.max_limit,
            "target outside current power constraints"
        );
        ensure!(
            target < current,
            "cap already at or below requested target; no write"
        );
        device.set_power_management_limit(target)?;
        let actual = device.power_management_limit()?;
        ensure!(
            actual <= target,
            "driver readback {actual} mW exceeds requested {target} mW"
        );
        Ok(f64::from(actual) / 1000.0)
    }
    fn lock_graphics(&mut self, uuid: &str, mhz: u32) -> Result<()> {
        self.nvml
            .device_by_uuid(uuid)?
            .set_gpu_locked_clocks(GpuLockedClocksSetting::Numeric {
                min_clock_mhz: 0,
                max_clock_mhz: mhz,
            })?;
        Ok(())
    }
    fn lock_memory(&mut self, uuid: &str, mhz: u32) -> Result<()> {
        self.nvml
            .device_by_uuid(uuid)?
            .set_mem_locked_clocks(0, mhz)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimSample {
    pub currents_a: [f64; 6],
    #[serde(default = "default_voltage")]
    pub voltage_v: f64,
    pub board_power_w: f64,
    #[serde(default)]
    pub sensor_error: Option<String>,
    #[serde(default)]
    pub nvml_error: Option<String>,
    #[serde(default)]
    pub sample_duration_ms: u64,
}
fn default_voltage() -> f64 {
    12.0
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub samples: Vec<SimSample>,
    #[serde(default)]
    pub fail_power: bool,
    #[serde(default)]
    pub fail_graphics: bool,
    #[serde(default)]
    pub fail_memory: bool,
}
pub struct SimBackend {
    pub fixture: Fixture,
    pub cap_w: f64,
    pub writes: Vec<String>,
    pub graphics_clock_mhz: u32,
    pub memory_clock_mhz: u32,
    position: usize,
}
impl SimBackend {
    pub fn new(fixture: Fixture) -> Result<Self> {
        ensure!(!fixture.samples.is_empty(), "simulation needs samples");
        for s in &fixture.samples {
            ensure!(
                s.currents_a.iter().all(|v| v.is_finite() && *v >= 0.0)
                    && s.voltage_v.is_finite()
                    && s.voltage_v > 0.0
                    && s.board_power_w.is_finite()
                    && s.board_power_w >= 0.0,
                "invalid simulation values"
            );
        }
        Ok(Self {
            fixture,
            cap_w: 600.0,
            writes: vec![],
            graphics_clock_mhz: 1500,
            memory_clock_mhz: 14001,
            position: 0,
        })
    }
    pub fn load(path: &Path) -> Result<Self> {
        Self::new(serde_json::from_slice(&std::fs::read(path)?)?)
    }
}
impl Backend for SimBackend {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![DeviceInfo {
            gpu_uuid: "GPU-SIMULATED-ASTRAL".into(),
            index: 0,
            pci_address: "0000:01:00.0".into(),
            model: "SIMULATED ROG-ASTRAL-RTX5090-O32G".into(),
            sensor_supported: true,
            i2c_adapters: vec![1],
            selected_adapter: Some(1),
            sensor_error: None,
            capabilities: Capabilities {
                power_min_w: Some(400.0),
                power_max_w: Some(600.0),
                graphics_min_mhz: Some(210),
                memory_min_mhz: Some(405),
                control_status: "simulated".into(),
                errors: vec![],
            },
        }])
    }
    fn sample(&mut self, _: &DeviceInfo, sequence: u64) -> Reading {
        let s = &self.fixture.samples[self.position.min(self.fixture.samples.len() - 1)];
        self.position += 1;
        let mut r = Reading::empty(sequence);
        r.sample_duration_ms = s.sample_duration_ms;
        if let Some(e) = &s.sensor_error {
            r.sensor_errors.push(e.clone());
        } else {
            r.set_channels(std::array::from_fn(|i| Channel {
                channel_id: i as u8 + 1,
                voltage_v: s.voltage_v,
                current_a: s.currents_a[i],
                power_w: s.voltage_v * s.currents_a[i],
            }));
        }
        if let Some(e) = &s.nvml_error {
            r.nvml_errors.push(e.clone());
        } else {
            r.nvml_board_power_w = Some(s.board_power_w);
            r.power_limit_w = Some(self.cap_w);
            r.graphics_clock_mhz = Some(self.graphics_clock_mhz);
            r.memory_clock_mhz = Some(self.memory_clock_mhz);
        }
        r
    }
    fn set_power(&mut self, _: &str, target_w: f64) -> Result<f64> {
        self.writes.push(format!("power:{target_w}"));
        ensure!(!self.fixture.fail_power, "simulated power failure");
        ensure!(
            (400.0..=600.0).contains(&target_w) && target_w < self.cap_w,
            "invalid simulated cap"
        );
        self.cap_w = target_w;
        Ok(target_w)
    }
    fn lock_graphics(&mut self, _: &str, mhz: u32) -> Result<()> {
        self.writes.push(format!("graphics:{mhz}"));
        ensure!(!self.fixture.fail_graphics, "simulated graphics failure");
        self.graphics_clock_mhz = mhz;
        Ok(())
    }
    fn lock_memory(&mut self, _: &str, mhz: u32) -> Result<()> {
        self.writes.push(format!("memory:{mhz}"));
        ensure!(!self.fixture.fail_memory, "simulated memory failure");
        self.memory_clock_mhz = mhz;
        Ok(())
    }
}
