# Protection behavior (schema/policy v1)

Pinwatch observes six **supply channels**, not twelve individually measured large
connector contacts. The order is register order (1–6); no physical pin mapping is
asserted. It has no ground-contact or connector-temperature telemetry. Software
intervention does not certify connector condition or prevent all damage.

## Defaults and modes

`monitor` is the default and never calls control setters. `dry_run` uses the same
decisions and emits `action_proposed` events, marked simulation, without setters.
`active` explicitly permits power/clock controls. The replay backend can use
active mode but only changes in-memory mock state; every event is simulation.

Defaults: 8.5 A, 2000 ms polling, 5000 ms maximum sample age, 2% control headroom,
4000 ms settling. Configuration rejects unknown fields, nonfinite/nonpositive
thresholds, thresholds above 9.2 A, intervals outside 100–5000 ms, sample ages
outside polling interval–10000 ms, and settling outside polling interval–10000 ms.
Headroom must be 0–10%. These are conservative application policy bounds;
**9.2 A is not a universal connector-damage guarantee**. The reference discourages
thresholds above 9.2 A and intervals above 5 seconds; Pinwatch rejects them.

## Decision and confirmation

1. Require six valid channels, nonzero supply voltages, positive finite measured
   NVML board power and configured cap, no read errors, and fresh data. Zero channel
   current is allowed. Invalid/missing telemetry cannot trigger an all-clear or a
   computed zero-watt target. Ratios are informational, with null for 0/0.
2. Below 8.5 A: no new control. **Equality triggers**, with headroom making a real
   reduction possible even at equality. Zero configured headroom may result in no
   reducing action at equality; this is explicitly degraded, not successful.
3. For the highest current `I`, compute (floor to milliwatts):

   ```text
   scale = threshold_a / I
   target_w = floor(min(measured_board_power_w, current_cap_w)
                    * scale * (1 - headroom_fraction) * 1000) / 1000
   ```

   Thus measured board power drives the decision; the current cap additionally
   limits it. This deliberately reduces an already lower cap under overload rather
   than accidentally raising it when measured transient power exceeds the cap.
4. Query real NVML power constraints. If the target is within range, cap it at the
   queried maximum. Immediately before the setter, re-read constraints and the
   current cap, and clamp again. Never request an increase or a redundant write.
   After setter success, query the cap again: only a positive readback no higher
   than the target counts as success. A setter success followed by readback failure
   is a failed/uncertain intervention, not a confirmed reduction.
5. If the computed target is below the minimum, use the separately queried minimum
   graphics and memory frequencies across supported performance states. Both must
   be known and positive. Request `[0, minimum]` locked clock ranges, matching the
   reference intent. Attempt both setters even if the first fails. Report each
   driver's acknowledgement separately. A partial fallback is degraded. These
   acknowledgements do not prove that instantaneous clocks/current have settled.
   After `settle_ms`, require a fresh sample showing positive graphics and memory
   clocks at or below the requested caps. Until then, state is `waiting` and
   `effective=false`. Missing readback, clock-query errors, or an exceeded target
   latches `degraded` / `action_failed`, even if current has recovered. Do not
   repeatedly rewrite a lock the driver acknowledges without the expected effect.
   Clock-query failures do not disable otherwise valid power-limit protection.
6. Emit detection, persist/log `action_requested`, execute, then emit
   `action_applied` or `action_failed`. Clock `action_applied` means setters were
   acknowledged; a later `action_verified` records the observed response. No desktop
   delivery or acknowledgement is
   involved. If persisting the request fails, do not perform that action. All event
   stages go to stderr (journald in the service), including coalesced desktop events.

Power and clock capabilities are independent of sensor discovery. Queryable
constraints **do not prove permission or setter support**. Before a successful
intervention in this daemon run, active mode is `armed_unverified` in JSON and
`armed` in terminal output, with `effective=false`. The message says "active
monitoring; no intervention in this daemon run". This is per-run action history,
not a claim that administrative hardware checks have never been performed or
that the daemon is waiting for permission. Active mode acts automatically on an
overload. Separate checks do not populate the daemon's intervention history.
`effective=true` means a relevant control was driver-confirmed in this daemon
instance and no currently known failure exists. Clock fallback additionally needs
fresh observed clocks within the requested caps after settling. It is not a safety certification
or an assurance that current has already fallen. A fresh overload awaiting a
response is `waiting`, `effective=false`. Missing required ranges yields degraded
protection without preventing read-only samples.

## Lag, repeated samples, and incidents

No averaging or debounce delays the first action. Samples have increasing sequence
numbers and monotonic age accounting (wall-clock timestamps are for display).
Duplicate/out-of-order samples cannot cause a second write. The sensor packet has
no hardware timestamp/counter: freshness means recent acquisition, not proof that
the sensor firmware is updating plausible values. After a power action,
wait at least `settle_ms` and require board power to fall by at least 2% from the
previous action's measurement before another ordinary reduction. An increase of
more than 5% in peak current, or a newly necessary minimum-clock fallback, can
escalate immediately on a **new fresh sample**. These response/incident heuristics
are application policy, not electrical specifications.

Ten seconds of continuing overload without measured response latches a visible
ineffective-protection failure instead of endlessly multiplying the cap downward.
After emergency clocks, allow the settling period; unverifiable clocks or continued
overload then latch a failure without repeating setters. Setter failures also latch for this process.
Review the error/driver/permissions before an administrative restart; restart is
not restoration and may attempt a new intervention if overload remains.

A sample below 95% of the current threshold ends the incident and rearms ordinary
response tracking. It does not restore caps/clocks or clear a control-failure
latch. A reported cap increase after Pinwatch's confirmation (another tool or a
driver reset) latches a degraded state. NVML offers no atomic compare-and-set cap
operation: do not run competing control managers. The final read/check/write still
has a race with external software. Existing locked-clock ranges cannot be reliably
read back through the chosen API. Actual clocks are sampled; exceeding requested
caps degrades protection, but an idle reading below a cap cannot prove the lock
will survive later load, driver resets, or another control manager.

## Missing telemetry, restart, exit

- Sensor/NVML read failure: null missing fields, explicit errors and degraded state;
  continue trying the same GPU UUID. No speculative hardware change on telemetry
  loss. Existing restrictions remain. Lost GPU indices are never reassigned by the
  daemon; handles are resolved by UUID, with PCI identity checked before I2C.
- Slow polling: cached frames continue with age/freshness updates. Stale readings
  are retained only as explicitly stale historical numbers. They cannot drive
  controls. A blocked kernel ioctl cannot safely be cancelled in Rust; it can
  delay worker shutdown or other GPUs. Clients still see cache aging; systemd's
  stop timeout ultimately bounds process shutdown. No userspace guarantee can
  recover a hung driver.
- Device list/adapters are fixed for a daemon run; missing devices may recover by
  the same UUID/path, but new GPUs or re-numbered adapters require review/restart.
- Restart: query actual cap and capabilities anew. A private atomic/fsynced history
  file retains the most recent control request/outcome per GPU, including partial
  or uncertain results, separately from a 64-event replay ring. Historical state
  is explicitly historical; it does **not** assert clocks or caps remain applied.
  Current cap is independent NVML telemetry. A crash between request and outcome
  leaves a visible unknown outcome. Do not discard the history on restart. Corrupt
  history fails startup rather than silently erasing restriction information.
- SIGINT/SIGTERM: stop polling, close subscribers, remove only the socket inode
  owned by this instance. Never unlink ownership locks. Never raise a cap or unlock
  clocks on recovery, exit, crash, or restart. There is no restore IPC or CLI.
  Restoration is an external authorized administrative operation after inspection;
  Pinwatch deliberately does not prescribe a blanket 600 W reset.

The fixture's initial readings are representative of the user's historical
10-minute **standard** gpu-burn run, with mean board power 532.85 W, peak 543.56 W,
peak GPU temperature 70.79°C, peak channel current 7.76 A and zero computation
errors. The overload and recovery frames are synthetic. Neither that run nor these
unit tests validate hardware protection at 600 W, other boards, or other connectors.

## Verified hardware controls and remaining limitation

Idle hardware checks on an ASUS ROG-ASTRAL-RTX5090-O32G with NVIDIA driver
620.70.06 used the real backend/executor under the daemon's service restrictions:

| Control | Result |
|---|---|
| Power cap 600 → 550 W | Driver accepted it; repeated NVML and nvidia-smi readback confirmed 550 W |
| Graphics/memory caps 180/405 MHz | Both setters acknowledged, but observed clocks stayed around 217–225/14,001 MHz |

Equal lower/upper clock bounds produced the same discrepancy. The implementation
retains the reference's `[0, minimum]` request. LACT was running with an undervolt
profile; there is no established cause for the mismatch.

**Do not rely on minimum-clock fallback to achieve its requested limits on this
setup.** The software reports an unverified/failed clock response rather than an
effective restriction. Configured power-cap readback was verified; loaded
consumption response, real overload recovery, and electrical safety were not.
These results are specific to the tested hardware/driver configuration.

The optional [hardware control check](../examples/hardware-control-check.rs) can
reproduce setter/readback checks. It changes real hardware settings, requires an
explicit administrative invocation, and never restores higher caps or clocks.
Automated tests use simulated hardware instead.
