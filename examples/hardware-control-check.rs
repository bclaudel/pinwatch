//! Explicit administrative hardware check. Never invoked by automated tests.
//! No sensor polling, artificial overload, restoration, or stress workload.
use anyhow::{Context, Result, ensure};
use clap::{ArgGroup, Parser};
use nvml_wrapper::{Nvml, enum_wrappers::device::Clock};
use pinwatch::{
    backend::{Backend, Hardware, select},
    config::Config,
    model::{Action, now_ms},
    policy,
};
use std::{ffi::OsStr, io::Write, time::Duration};

#[derive(Parser)]
#[command(about="EXPLICIT REAL GPU WRITE: verify Pinwatch controls; restrictions are NOT restored",
    group(ArgGroup::new("action").required(true).args(["power_limit_w", "minimum_clocks"])))]
struct Args {
    #[arg(long)]
    uuid: String,
    #[arg(long)]
    power_limit_w: Option<f64>,
    #[arg(long)]
    minimum_clocks: bool,
}
fn main() -> Result<()> {
    let args = Args::parse();
    let status = std::fs::read_to_string("/proc/self/status")?;
    let effective = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .context("missing UID")?
        .split_whitespace()
        .nth(2)
        .context("missing effective UID")?;
    ensure!(
        effective == "0",
        "hardware checks must run as root with the daemon's service restrictions"
    );
    let mut hardware = Hardware::new(Config::default())?;
    let info = select(&hardware.devices()?, Some(&args.uuid), None)?.remove(0);
    ensure!(
        info.sensor_supported,
        "unsupported GPU; controls will not be tested"
    );
    let nvml = Nvml::builder()
        .lib_path(OsStr::new("/usr/lib/libnvidia-ml.so.1"))
        .init()?;
    let device = nvml.device_by_uuid(args.uuid.as_str())?;
    let previous_cap_w = f64::from(device.power_management_limit()?) / 1000.0;
    let action = if let Some(target_w) = args.power_limit_w {
        ensure!(
            target_w.is_finite() && target_w > 0.0 && target_w < previous_cap_w,
            "test target must reduce the existing cap"
        );
        Action::PowerLimit { target_w }
    } else {
        Action::ClockThrottle {
            graphics_mhz: info
                .capabilities
                .graphics_min_mhz
                .context("graphics minimum unavailable")?,
            memory_mhz: info
                .capabilities
                .memory_min_mhz
                .context("memory minimum unavailable")?,
        }
    };
    let before = serde_json::json!({"power_limit_w":previous_cap_w,
        "graphics_mhz":device.clock_info(Clock::Graphics).ok(),
        "memory_mhz":device.clock_info(Clock::Memory).ok()});
    let outcome = policy::execute(&mut hardware, &args.uuid, &action);
    let mut observations = vec![];
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(250));
        observations.push(serde_json::json!({"timestamp_unix_ms":now_ms(),
            "power_limit_w":device.power_management_limit().map(|p|f64::from(p)/1000.0).ok(),
            "board_power_w":device.power_usage().map(|p|f64::from(p)/1000.0).ok(),
            "graphics_mhz":device.clock_info(Clock::Graphics).ok(),
            "memory_mhz":device.clock_info(Clock::Memory).ok()}));
    }
    let observed_clock_caps = match &action {
        Action::ClockThrottle {
            graphics_mhz,
            memory_mhz,
        } => Some(observations.iter().rev().take(4).all(|o| {
            o["graphics_mhz"]
                .as_u64()
                .is_some_and(|v| v > 0 && v <= u64::from(*graphics_mhz))
                && o["memory_mhz"]
                    .as_u64()
                    .is_some_and(|v| v > 0 && v <= u64::from(*memory_mhz))
        })),
        _ => None,
    };
    let report = serde_json::json!({"kind":"explicit_hardware_control_check", "timestamp_unix_ms":now_ms(),
        "gpu_uuid":args.uuid,"before":before,"action":action,"outcome":outcome,
        "observations":observations,"observed_clock_caps":observed_clock_caps,"restored":false});
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &report)?;
    writeln!(output)?;
    output.flush()?;
    ensure!(
        outcome.errors.is_empty() && observed_clock_caps != Some(false),
        "hardware control failed/partially applied or its observed response was not verified; see JSON; no settings restored"
    );
    Ok(())
}
