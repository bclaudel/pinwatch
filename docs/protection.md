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
6. Emit detection, persist/log `action_requested`, execute, then emit
   `action_applied` or `action_failed`. No desktop delivery or acknowledgement is
   involved. If persisting the request fails, do not perform that action. All event
   stages go to stderr (journald in the service), including coalesced desktop events.

Power and clock capabilities are independent of sensor discovery. Queryable
constraints **do not prove permission or setter support**. Before a successful
intervention, active mode is `armed_unverified`, `effective=false`, and says so.
`effective=true` means a relevant control was driver-confirmed in this daemon
instance and no currently known failure exists; it is not a safety certification
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
After emergency clocks, allow the settling period; continued overload then latches
a failure without repeating setters. Setter failures also latch for this process.
Review the error/driver/permissions before an administrative restart; restart is
not restoration and may attempt a new intervention if overload remains.

A sample below 95% of the current threshold ends the incident and rearms ordinary
response tracking. It does not restore caps/clocks or clear a control-failure
latch. A reported cap increase after Pinwatch's confirmation (another tool or a
driver reset) latches a degraded state. NVML offers no atomic compare-and-set cap
operation: do not run competing control managers. The final read/check/write still
has a race with external software. Existing locked-clock ranges cannot be reliably
read back through the chosen API; later external clock changes may not be detected.

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
