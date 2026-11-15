use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Monitor,
    DryRun,
    Active,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    pub threshold_a: f64,
    pub poll_interval_ms: u64,
    pub max_sample_age_ms: u64,
    pub headroom_fraction: f64,
    pub settle_ms: u64,
    pub socket: PathBuf,
    pub lock_dir: PathBuf,
    pub state_file: PathBuf,
    pub gpu_uuid: Option<String>,
    pub adapters: BTreeMap<String, u32>,
    pub nvml_library: Option<PathBuf>,
    pub simulate: Option<PathBuf>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Monitor,
            threshold_a: 8.5,
            poll_interval_ms: 2000,
            max_sample_age_ms: 5000,
            headroom_fraction: 0.02,
            settle_ms: 4000,
            socket: "/run/pinwatch/pinwatch.sock".into(),
            lock_dir: "/run/pinwatch".into(),
            state_file: "/var/lib/pinwatch/history.json".into(),
            gpu_uuid: None,
            adapters: BTreeMap::new(),
            nvml_library: None,
            simulate: None,
        }
    }
}
impl Config {
    /// Only an absent default file falls back to defaults; unreadable/invalid files are errors.
    pub fn read_optional(path: &std::path::Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let config: Self = toml::from_str(&text)?;
                config.validate()?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.threshold_a.is_finite() && self.threshold_a > 0.0 && self.threshold_a <= 9.2,
            "threshold_a must be finite, positive, and <= 9.2 (policy ceiling, not a safety guarantee)"
        );
        ensure!(
            (100..=5000).contains(&self.poll_interval_ms),
            "poll_interval_ms must be 100..=5000"
        );
        ensure!(
            (self.poll_interval_ms..=10000).contains(&self.max_sample_age_ms),
            "max_sample_age_ms must be between poll_interval_ms and 10000"
        );
        ensure!(
            self.headroom_fraction.is_finite() && (0.0..=0.1).contains(&self.headroom_fraction),
            "headroom_fraction must be 0..=0.1"
        );
        ensure!(
            (self.poll_interval_ms..=10000).contains(&self.settle_ms),
            "settle_ms must be between poll_interval_ms and 10000"
        );
        for p in [&self.socket, &self.lock_dir, &self.state_file] {
            ensure!(
                p.is_absolute(),
                "runtime paths must be absolute: {}",
                p.display()
            );
        }
        Ok(())
    }
    pub fn read(path: &std::path::Path) -> Result<Self> {
        let config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        config.validate()?;
        Ok(config)
    }
}
