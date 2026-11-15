//! Immediate current-triggered decisions; no ratio-based control and no automatic restoration.
use crate::{
    backend::Backend,
    config::{Config, Mode},
    model::*,
};
use anyhow::Result;

#[derive(Debug, PartialEq)]
pub enum Decision {
    BelowThreshold,
    Act(Action),
    Unavailable(String),
}
pub fn decide(config: &Config, caps: &Capabilities, reading: &Reading, age_ms: u64) -> Decision {
    if age_ms > config.max_sample_age_ms || !reading.valid() {
        return Decision::Unavailable(
            "missing, invalid, or stale sensor/NVML telemetry; no hardware change".into(),
        );
    }
    let maximum = reading
        .channels
        .as_ref()
        .unwrap()
        .iter()
        .map(|c| c.current_a)
        .fold(0.0_f64, f64::max);
    if maximum < config.threshold_a {
        return Decision::BelowThreshold;
    }
    let (Some(min), Some(max)) = (caps.power_min_w, caps.power_max_w) else {
        return Decision::Unavailable("power constraints unavailable".into());
    };
    if !min.is_finite() || !max.is_finite() || min <= 0.0 || max < min {
        return Decision::Unavailable("invalid power constraints".into());
    }
    let cap = reading.power_limit_w.unwrap();
    // Measured board power drives the target. Existing cap is a ceiling, not an excuse to raise it.
    let target = (reading.nvml_board_power_w.unwrap().min(cap) * config.threshold_a / maximum
        * (1.0 - config.headroom_fraction)
        * 1000.0)
        .floor()
        / 1000.0;
    if target < min {
        return match (caps.graphics_min_mhz, caps.memory_min_mhz) {
            (Some(g), Some(m)) if g > 0 && m > 0 => Decision::Act(Action::ClockThrottle {
                graphics_mhz: g,
                memory_mhz: m,
            }),
            _ => Decision::Unavailable(
                "target below minimum power limit; clock fallback unavailable".into(),
            ),
        };
    }
    if target >= cap {
        return Decision::Unavailable(
            "overload but no reducing action (zero headroom at equality)".into(),
        );
    }
    Decision::Act(Action::PowerLimit {
        target_w: target.min(max),
    })
}
/// Attempt both emergency controls even if one fails. No implicit rollback.
pub fn execute(backend: &mut dyn Backend, uuid: &str, action: &Action) -> Outcome {
    let mut out = Outcome::default();
    match action {
        Action::PowerLimit { target_w } => match backend.set_power(uuid, *target_w) {
            Ok(actual) if actual.is_finite() && actual > 0.0 && actual <= *target_w => {
                out.power_limit_w = Some(actual)
            }
            Ok(_) => out.errors.push("invalid power limit readback".into()),
            Err(e) => out.errors.push(e.to_string()),
        },
        Action::ClockThrottle {
            graphics_mhz,
            memory_mhz,
        } => {
            match backend.lock_graphics(uuid, *graphics_mhz) {
                Ok(()) => out.graphics_locked = true,
                Err(e) => out.errors.push(format!("graphics: {e}")),
            }
            match backend.lock_memory(uuid, *memory_mhz) {
                Ok(()) => out.memory_locked = true,
                Err(e) => out.errors.push(format!("memory: {e}")),
            }
        }
    }
    out
}

pub struct Engine {
    pub protection: Protection,
    incident: Option<String>,
    last_sequence: Option<u64>,
    last_attempt: Option<(u64, f64, f64)>, // monotonic elapsed, measured W, maximum A
    failed: bool,
    unavailable_reason: Option<String>,
}
impl Engine {
    pub fn new(mode: Mode, historical: Option<Event>) -> Self {
        Self {
            protection: Protection {
                mode,
                state: "initializing".into(),
                effective: false,
                reason: "no sample yet; controls unverified".into(),
                last_action: None,
                last_outcome: None,
                historical_restriction: historical,
            },
            incident: None,
            last_sequence: None,
            last_attempt: None,
            failed: false,
            unavailable_reason: None,
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn event(
        &self,
        config: &Config,
        info: &DeviceInfo,
        r: &Reading,
        kind: &str,
        detail: &str,
        action: Option<Action>,
        outcome: Option<Outcome>,
    ) -> Event {
        let peak = r
            .channels
            .as_ref()
            .and_then(|c| c.iter().max_by(|a, b| a.current_a.total_cmp(&b.current_a)));
        Event {
            schema_version: SCHEMA_VERSION,
            daemon_instance: String::new(),
            sequence: 0,
            event_id: String::new(),
            incident_id: self
                .incident
                .clone()
                .unwrap_or_else(|| format!("health-{}", info.gpu_uuid)),
            timestamp_unix_ms: now_ms(),
            gpu_uuid: info.gpu_uuid.clone(),
            kind: kind.into(),
            simulation: config.mode == Mode::DryRun || config.simulate.is_some(),
            channel_id: peak.map(|c| c.channel_id),
            current_a: peak.map(|c| c.current_a),
            threshold_a: config.threshold_a,
            previous_power_cap_w: r.power_limit_w,
            action,
            outcome,
            detail: detail.into(),
        }
    }
    fn state(&mut self, state: &str, effective: bool, reason: &str) {
        self.protection.state = state.into();
        self.protection.effective = effective;
        self.protection.reason = reason.into();
    }
    #[allow(clippy::too_many_arguments)]
    pub fn tick(
        &mut self,
        config: &Config,
        info: &DeviceInfo,
        r: &Reading,
        age_ms: u64,
        elapsed_ms: u64,
        backend: &mut dyn Backend,
        emit: &mut impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        let decision_started = std::time::Instant::now();
        let decision = decide(config, &info.capabilities, r, age_ms);
        if let Decision::Unavailable(reason) = &decision {
            if self.unavailable_reason.as_ref() != Some(reason) {
                emit(self.event(
                    config,
                    info,
                    r,
                    "protection_unavailable",
                    reason,
                    None,
                    None,
                ))?;
            }
            self.unavailable_reason = Some(reason.clone());
            self.state("degraded", false, reason);
            return Ok(());
        }
        if self.unavailable_reason.take().is_some() {
            emit(self.event(
                config,
                info,
                r,
                "monitoring_recovered",
                "Monitoring restored; power limits and clocks were not restored",
                None,
                None,
            ))?;
        }
        // A sample may be presented twice by consumers; never act twice on its sequence.
        if self
            .last_sequence
            .is_some_and(|seq| r.sample_sequence <= seq)
        {
            self.state(
                "waiting",
                false,
                "duplicate/out-of-order sample; no repeated action",
            );
            return Ok(());
        }
        self.last_sequence = Some(r.sample_sequence);
        if !self.failed
            && let Some(confirmed) = self
                .protection
                .last_outcome
                .as_ref()
                .and_then(|o| o.power_limit_w)
            && r.power_limit_w.is_some_and(|cap| cap > confirmed + 0.001)
        {
            self.failed = true;
            emit(self.event(config, info, r, "protection_unavailable",
                "configured cap increased since Pinwatch confirmation (external change or driver reset); review required",
                None, None))?;
        }
        if decision == Decision::BelowThreshold {
            let peak = r
                .channels
                .as_ref()
                .unwrap()
                .iter()
                .map(|c| c.current_a)
                .fold(0.0_f64, f64::max);
            if peak < config.threshold_a * 0.95 && self.incident.take().is_some() {
                emit(self.event(
                    config,
                    info,
                    r,
                    "overload_cleared",
                    "Current is below rearm level; restrictions remain in place",
                    None,
                    None,
                ))?;
                self.last_attempt = None;
            }
            if self.failed {
                self.state("degraded",false,"previous control failure remains latched; administrative review/restart required");
            } else if self.protection.last_outcome.is_some() {
                self.state(
                    "restricted",
                    true,
                    "below current threshold; confirmed restrictions retained",
                );
            } else if config.mode == Mode::Active {
                self.state(
                    "armed_unverified",
                    false,
                    "monitoring; control permissions/support cannot be verified without a write",
                );
            } else {
                self.state(
                    "monitoring",
                    false,
                    "read-only monitoring; no active protection",
                );
            }
            return Ok(());
        }
        let Decision::Act(action) = decision else {
            unreachable!()
        };
        if self.incident.is_none() {
            self.incident = Some(uuid::Uuid::new_v4().to_string());
            emit(self.event(
                config,
                info,
                r,
                "overload_detected",
                "channel current reached threshold",
                Some(action.clone()),
                None,
            ))?;
        }
        if config.mode != Mode::Active {
            if self.protection.last_action.as_ref() != Some(&action) && config.mode == Mode::DryRun
            {
                emit(self.event(
                    config,
                    info,
                    r,
                    "action_proposed",
                    "SIMULATION: no hardware changes",
                    Some(action.clone()),
                    None,
                ))?;
            }
            self.protection.last_action = Some(action);
            self.state(
                if config.mode == Mode::DryRun {
                    "dry_run"
                } else {
                    "overload_unprotected"
                },
                false,
                "overload observed in read-only mode",
            );
            return Ok(());
        }
        if self.failed {
            self.state(
                "degraded",
                false,
                "control failure latched; no repeated ineffective writes",
            );
            return Ok(());
        }
        if matches!(
            self.protection.last_action,
            Some(Action::ClockThrottle { .. })
        ) {
            if self
                .last_attempt
                .is_some_and(|(when, _, _)| elapsed_ms.saturating_sub(when) < config.settle_ms)
            {
                self.state(
                    "waiting",
                    false,
                    "waiting for emergency clock response; no repeated write",
                );
                return Ok(());
            }
            self.state(
                "degraded",
                false,
                "overload persists after emergency clock request; manual intervention required",
            );
            if self.unavailable_reason.is_none() {
                emit(self.event(
                    config,
                    info,
                    r,
                    "protection_unavailable",
                    "overload persists after minimum clocks",
                    None,
                    None,
                ))?;
                self.failed = true;
            }
            return Ok(());
        }
        let measured = r.nvml_board_power_w.unwrap();
        let peak = r
            .channels
            .as_ref()
            .unwrap()
            .iter()
            .map(|c| c.current_a)
            .fold(0.0_f64, f64::max);
        if let Some((when, previous_power, previous_current)) = self.last_attempt {
            let escalating =
                matches!(action, Action::ClockThrottle { .. }) || peak > previous_current * 1.05;
            if !escalating
                && (elapsed_ms.saturating_sub(when) < config.settle_ms
                    || measured >= previous_power * 0.98)
            {
                if elapsed_ms.saturating_sub(when) >= 10000 {
                    self.state(
                        "degraded",
                        false,
                        "overload persists without measured response; no downward ratcheting",
                    );
                    emit(self.event(config,info,r,"protection_unavailable","cap confirmed but overload persists without response; manual intervention required",None,None))?;
                    self.failed = true;
                } else {
                    self.state(
                        "waiting",
                        false,
                        "waiting for fresh measured response; no repeated write",
                    );
                }
                return Ok(());
            }
        }
        // The event sink persists/logs requested actions BEFORE any hardware mutation.
        emit(self.event(
            config,
            info,
            r,
            "action_requested",
            "requesting protective control",
            Some(action.clone()),
            None,
        ))?;
        if age_ms.saturating_add(decision_started.elapsed().as_millis() as u64)
            > config.max_sample_age_ms
        {
            self.state(
                "degraded",
                false,
                "sample became stale while recording request; no control executed",
            );
            emit(self.event(
                config,
                info,
                r,
                "action_failed",
                "sample became stale before execution; no control executed",
                Some(action),
                Some(Outcome {
                    errors: vec!["stale sample; request cancelled".into()],
                    ..Default::default()
                }),
            ))?;
            return Ok(());
        }
        self.last_attempt = Some((elapsed_ms, measured, peak));
        let outcome = execute(backend, &info.gpu_uuid, &action);
        self.failed = !outcome.errors.is_empty();
        self.protection.last_action = Some(action.clone());
        self.protection.last_outcome = Some(outcome.clone());
        if self.failed {
            self.state(
                "degraded",
                false,
                "intervention failed or partially applied; restrictions are not rolled back",
            );
            emit(self.event(
                config,
                info,
                r,
                "action_failed",
                "protective intervention failed or partially applied",
                Some(action),
                Some(outcome),
            ))?;
        } else {
            self.state(
                "restricted",
                true,
                "driver confirmed control; physical response not guaranteed",
            );
            emit(self.event(
                config,
                info,
                r,
                "action_applied",
                "driver confirmed control; restrictions retained",
                Some(action),
                Some(outcome),
            ))?;
        }
        Ok(())
    }
}
