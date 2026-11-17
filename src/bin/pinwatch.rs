use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use pinwatch::{
    backend,
    config::{Config, Mode},
    ipc,
    model::*,
    notify,
};
use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    version,
    about = "ASUS Astral GPU connector telemetry; read-only CLI and desktop companion"
)]
struct Args {
    /// Local fallback configuration; default: /etc/pinwatch/pinwatch.toml if present.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true, default_value = "/run/pinwatch/pinwatch.sock")]
    socket: PathBuf,
    #[arg(long, global = true, conflicts_with = "index")]
    uuid: Option<String>,
    #[arg(long, global = true)]
    index: Option<u32>,
    /// Force local simulation from a fixture, bypassing the daemon.
    #[arg(long, global = true)]
    simulate: Option<PathBuf>,
    /// PCI-local adapter override for fallback (requires a single selected GPU).
    #[arg(long, global = true)]
    adapter: Option<u32>,
    #[arg(long, global = true)]
    lock_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    nvml_library: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Enumerate GPU identities, sensor support, adapters, and queryable control ranges.
    Devices {
        #[arg(long)]
        json: bool,
    },
    /// One snapshot from the daemon, or read-only hardware access if it is absent.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Continuous readings. --json emits flushed NDJSON, no ANSI or stdout diagnostics.
    Watch {
        #[arg(long)]
        json: bool,
    },
    /// Relay read-only daemon events to the desktop session's standard D-Bus notifier.
    Notify {
        /// EXPLICIT SYNTHETIC TEST: send one real desktop popup, without touching a GPU.
        #[arg(long)]
        test: bool,
    },
}
fn output(value: &impl serde::Serialize) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}
fn display(frame: &Frame, json: bool) -> Result<()> {
    if json {
        return output(frame);
    }
    let mut stdout = std::io::stdout().lock();
    for gpu in &frame.gpus {
        writeln!(
            stdout,
            "{}  {}  {}",
            gpu.device.gpu_uuid, gpu.device.pci_address, gpu.device.model
        )?;
        writeln!(
            stdout,
            "  {} (age {} ms) | protection: {} — {}",
            gpu.freshness, gpu.sample_age_ms, gpu.protection.state, gpu.protection.reason
        )?;
        let fmt = |v: Option<f64>| v.map_or("unavailable".into(), |v| format!("{v:.2}"));
        writeln!(
            stdout,
            "  board: {} W | connector: {} W | cap: {} W",
            fmt(gpu.reading.nvml_board_power_w),
            fmt(gpu.reading.connector_total_w),
            fmt(gpu.reading.power_limit_w)
        )?;
        if let Some(channels) = &gpu.reading.channels {
            for c in channels {
                writeln!(
                    stdout,
                    "  channel {}: {:5.2} V  {:5.2} A  {:6.2} W",
                    c.channel_id, c.voltage_v, c.current_a, c.power_w
                )?;
            }
        }
        writeln!(
            stdout,
            "  power min/max: {} | current min/max: {}",
            fmt(gpu.reading.power_balance_ratio),
            fmt(gpu.reading.current_balance_ratio)
        )?;
        for error in gpu
            .reading
            .sensor_errors
            .iter()
            .chain(&gpu.reading.nvml_errors)
        {
            writeln!(stdout, "  ERROR: {error}")?;
        }
    }
    stdout.flush()?;
    Ok(())
}
fn show_devices(devices: Vec<DeviceInfo>, json: bool) -> Result<()> {
    if json {
        return output(
            &serde_json::json!({"schema_version":SCHEMA_VERSION,"kind":"devices","devices":devices}),
        );
    }
    let mut stdout = std::io::stdout().lock();
    for d in devices {
        writeln!(
            stdout,
            "[{}] {} {} {}",
            d.index, d.gpu_uuid, d.pci_address, d.model
        )?;
        writeln!(
            stdout,
            "  sensor supported: {} | adapters: {:?} | selected: {:?}",
            d.sensor_supported, d.i2c_adapters, d.selected_adapter
        )?;
        if let Some(e) = d.sensor_error {
            writeln!(stdout, "  sensor: {e}")?;
        }
        writeln!(
            stdout,
            "  controls: {} | power range {:?}..{:?} W | min clocks graphics {:?}, memory {:?} MHz",
            d.capabilities.control_status,
            d.capabilities.power_min_w,
            d.capabilities.power_max_w,
            d.capabilities.graphics_min_mhz,
            d.capabilities.memory_min_mhz
        )?;
        for error in d.capabilities.errors {
            writeln!(stdout, "  capability: {error}")?;
        }
    }
    stdout.flush()?;
    Ok(())
}
fn filter_frame(mut frame: Frame, args: &Args) -> Result<Frame> {
    let devices: Vec<_> = frame.gpus.iter().map(|g| g.device.clone()).collect();
    let selected = backend::select(&devices, args.uuid.as_deref(), args.index)?;
    frame
        .gpus
        .retain(|g| selected.iter().any(|s| s.gpu_uuid == g.device.gpu_uuid));
    frame
        .recent_events
        .retain(|e| selected.iter().any(|s| s.gpu_uuid == e.gpu_uuid));
    Ok(frame)
}
async fn run(args: Args) -> Result<()> {
    if let Command::Notify { test } = &args.command {
        ensure!(
            args.config.is_none()
                && args.simulate.is_none()
                && args.adapter.is_none()
                && args.lock_dir.is_none()
                && args.nvml_library.is_none(),
            "notify uses daemon IPC only; local monitoring options are not supported"
        );
        return notify::run(&args.socket, *test).await;
    }
    let (json, continuous) = match args.command {
        Command::Devices { json } | Command::Status { json } => (json, false),
        Command::Watch { json } => (json, true),
        _ => unreachable!(),
    };
    let mut reader = if args.simulate.is_some() {
        None
    } else {
        ipc::connect_if_running(&args.socket, continuous).await?
    };
    loop {
        if let Some(mut stream) = reader.take() {
            eprintln!("Using pinwatchd at {}", args.socket.display());
            loop {
                let frame = match ipc::next_frame(&mut stream).await {
                    Ok(frame) => filter_frame(frame, &args)?,
                    Err(error) if continuous && ipc::disconnected(&error) => {
                        eprintln!("Daemon disconnected; trying read-only local monitoring");
                        break;
                    }
                    Err(error) => return Err(error),
                };
                if matches!(args.command, Command::Devices { .. }) {
                    return show_devices(frame.gpus.into_iter().map(|g| g.device).collect(), json);
                }
                display(&frame, json)?;
                if !continuous {
                    return Ok(());
                }
            }
        }
        eprintln!(
            "Using read-only local monitoring; daemon protection is unavailable through this source"
        );
        reader = monitor_locally(&args, json, continuous).await?;
        if reader.is_none() {
            return Ok(());
        }
    }
}

async fn monitor_locally(
    args: &Args,
    json: bool,
    continuous: bool,
) -> Result<Option<tokio::io::BufReader<tokio::net::UnixStream>>> {
    let mut config = local_config(args)?;
    if config.simulate.is_none() {
        ensure!(
            config.lock_dir == std::path::Path::new("/run/pinwatch"),
            "real hardware always uses /run/pinwatch ownership locks"
        );
    }
    let mut backend = backend::open(&config)?;
    let uuid = args.uuid.as_deref().or_else(|| {
        if args.index.is_none() {
            config.gpu_uuid.as_deref()
        } else {
            None
        }
    });
    let mut devices = backend::select(&backend.devices()?, uuid, args.index)?;
    if let Some(adapter) = args.adapter {
        ensure!(
            devices.len() == 1,
            "--adapter requires exactly one selected GPU"
        );
        config.adapters.insert(devices[0].gpu_uuid.clone(), adapter);
        pinwatch::sensor::select_adapter(&devices[0].i2c_adapters, Some(adapter))?;
        devices[0].selected_adapter = Some(adapter);
        devices[0].sensor_error = None;
    }
    if matches!(args.command, Command::Devices { .. }) {
        show_devices(devices, json)?;
        return Ok(None);
    }
    let ownership_started = Instant::now();
    let _locks = loop {
        let ownership = devices
            .iter()
            .map(|d| ipc::Ownership::acquire(&config.lock_dir, &d.gpu_uuid))
            .collect::<Result<Vec<_>>>();
        match ownership {
            Ok(locks) => break locks,
            Err(error) => {
                // During shutdown/startup, the socket and GPU locks change at
                // different instants. Retry briefly without ever bypassing ownership.
                if args.simulate.is_none() {
                    if let Some(reader) = ipc::connect_if_running(&args.socket, continuous).await? {
                        return Ok(Some(reader));
                    }
                    if ownership_started.elapsed() < Duration::from_secs(6)
                        && error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
                    {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                }
                return Err(error)
                    .context("cannot acquire local GPU ownership; another poller may be running");
            }
        }
    };
    let mut sequence = 0;
    loop {
        if args.simulate.is_none()
            && let Some(reader) = ipc::connect_if_running(&args.socket, continuous).await?
        {
            return Ok(Some(reader)); // Drop local GPU locks before waiting for daemon telemetry.
        }
        sequence += 1;
        let mut gpus = vec![];
        let mut sample_times = vec![];
        for device in &devices {
            let at = Instant::now();
            sample_times.push(at);
            let reading = backend.sample(device, sequence);
            let valid = reading.valid();
            gpus.push(GpuSnapshot {
                device: device.clone(),
                sample_age_ms: (at.elapsed().as_millis() as u64).max(reading.sample_duration_ms),
                freshness: "fresh".into(),
                reading,
                protection: Protection {
                    mode: Mode::Monitor,
                    state: if valid { "monitoring" } else { "degraded" }.into(),
                    effective: false,
                    reason: "direct read-only access; no active protection".into(),
                    last_action: None,
                    last_outcome: None,
                    historical_restriction: None,
                },
            });
        }
        for (gpu, at) in gpus.iter_mut().zip(sample_times) {
            gpu.sample_age_ms = gpu.sample_age_ms.max(at.elapsed().as_millis() as u64);
        }
        let mut frame = Frame {
            schema_version: SCHEMA_VERSION,
            kind: "telemetry".into(),
            daemon_instance: "direct-read-only".into(),
            emitted_at_unix_ms: now_ms(),
            max_sample_age_ms: config.max_sample_age_ms,
            gpus,
            recent_events: vec![],
        };
        frame.refresh(0);
        display(&frame, json)?;
        if !continuous {
            return Ok(None);
        }
        let next_poll = Instant::now() + Duration::from_millis(config.poll_interval_ms);
        loop {
            tokio::time::sleep(
                Duration::from_millis(200).min(next_poll.saturating_duration_since(Instant::now())),
            )
            .await;
            if args.simulate.is_none()
                && let Some(reader) = ipc::connect_if_running(&args.socket, true).await?
            {
                return Ok(Some(reader));
            }
            if Instant::now() >= next_poll {
                break;
            }
        }
    }
}
fn local_config(args: &Args) -> Result<Config> {
    let mut config = if let Some(path) = &args.config {
        Config::read(path)
            .with_context(|| format!("read local configuration {}", path.display()))?
    } else if args.simulate.is_some() {
        // Explicit fixture runs are independent of the installed hardware configuration.
        Config::default()
    } else {
        Config::read_optional(std::path::Path::new("/etc/pinwatch/pinwatch.toml"))?
    };
    // Direct access is always read-only, even when the daemon configuration is active.
    config.mode = Mode::Monitor;
    if let Some(path) = &args.simulate {
        config.simulate = Some(path.clone());
    }
    if let Some(path) = &args.nvml_library {
        config.nvml_library = Some(path.clone());
    }
    if let Some(path) = &args.lock_dir {
        config.lock_dir = path.clone();
    }
    config.validate()?;
    Ok(config)
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tokio::select! { result=run(args)=>result.context("pinwatch"), _=tokio::signal::ctrl_c()=>Ok(()) }
}
