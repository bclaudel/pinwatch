# Desktop notifications

```text
pinwatchd (system service, sensor/control owner)
    -> versioned telemetry + event ring over Unix socket
pinwatch notify (desktop user, read-only socket access)
    -> org.freedesktop.Notifications on the existing session bus
notification server (Quickshell, another shell, or another conforming server)
```

The daemon neither connects to a desktop bus nor guesses users/UIDs, scrapes user
environments, launches `notify-send`, or runs desktop code as root. The CLI uses
`zbus` with a three-second D-Bus timeout. No session bus, missing notification
server, timeouts, crashes, dismissal, or DND can delay the protection worker.

## Setup

Give your desktop user membership of the service's `pinwatch` IPC group, then log
in again so the graphical session inherits it. This grants telemetry access only;
no GPU-control IPC exists. Run **as that user**:

```sh
pinwatch notify
```

The Arch package includes the user unit. Enable it as your desktop user:

```sh
systemctl --user enable --now pinwatch-notify.service
```

The example uses `PartOf`/`WantedBy=graphical-session.target`. Your desktop must
start/stop that target and provide the existing session bus to its user manager.
For a compositor that does not manage the target, launch the companion through
its normal session startup facility. This project does not edit that setup.

The companion owns `org.pinwatch.Notifier` on the user's session bus with
DoNotQueue name ownership. A second companion on that bus exits, including a
second invocation with `--test`. No shared privileged session selector is involved.

## What is notified

- Confirmed power reduction: critical urgency, GPU UUID, triggering channel/current,
  threshold, previous cap, requested and confirmed cap.
- Emergency clock restriction requested: critical urgency, both setter
  acknowledgements, with observed response pending. `action_verified` can report
  observed throttling after settling; unavailable/exceeded readback reports failure.
  A partial fallback is a failure notice, never a success.
- Failed intervention, degraded/stale telemetry, or sustained daemon disconnection:
  critical urgency with a concise reason. Fresh `armed_unverified`/armed monitoring
  is normal before an intervention and does not generate a warning by itself.
  Historical failed or uncertain actions are still reconciled on connection.
- Dry-run proposals: clearly labeled **SIMULATION**. Synthetic-backend applied
  events are also simulations; they do not represent real GPU changes.
- Monitoring/connection recovery: normal urgency, explicitly says no power caps
  or clocks were restored. There are no notification actions to restore settings
  or disable protection.

The D-Bus `Notify` request asks for critical urgency where appropriate and
`expire_timeout=0` (no expiration), supplies no interactive actions, and escapes
body markup. These express intent, **not guaranteed delivery, visibility, sound,
history retention, or persistence**. The notification server controls presentation
and DND. A Quickshell shell decides how to expose and retain received notifications.

## Deduplication, replacement, restart

Events are deduplicated by daemon-instance/sequence identity (bounded 512-ID cache).
Repeated telemetry frames cannot recreate a dismissed popup. Changes for one GPU
reuse the server's notification ID when available. Ordinary repeated intervention
notices within the same incident are limited to one per 30 seconds; escalation and distinct new failures
pass promptly. Recoveries also pass without waiting 30 seconds. The event log
retains every action regardless of popup coalescing. Pending notifications are
coalesced per GPU, not buffered unboundedly.

On IPC connection/reconnection, current state selects at most one relevant latest
event per GPU from the bounded ring, or a historical restriction record. Replayed
records are labeled **Historical**, including uncertain pre-crash requests. A new
daemon instance changes event identity; previous records are not mistaken for new
hardware changes. Longer disconnects may omit intermediate events; journald remains
the action record. A companion restart intentionally reconciles current state once
again; deduplication is in-memory, not a permanent record of user dismissals.

The companion reconnects to the daemon and retries the notification service with
1, 2, 4, 8, 16, then at most 30-second backoff. After at least five seconds of known
daemon disconnection, it queues one unavailable-protection notice. Socket receive
timeout is eight seconds, so recognizing a silent/hung endpoint adds that delay.
A server owner change clears obsolete replacement IDs and reconciles outstanding
notices once; server restart may have lost the old popup/history. An unchanged
server owner does not cause replays each poll. A missing server keeps the latest
pending notices for retry. A missing **session bus at startup** makes the process
exit; the optional user service retries. A whole desktop/bus logout ends that
session's ownership; restart the companion in the new session.

The notification channel is advisory. If you depend on intervention, monitor the
system daemon/journal as well; a desktop popup is not a protection mechanism.

## Explicit synthetic test

Only run this when you **want one real desktop popup**. Stop an existing companion
first because it owns the session name. This does not initialize NVML, read I²C,
or change hardware:

```sh
pinwatch notify --test
```

The text starts `SIMULATION — Pinwatch notification test`. Routine `cargo test`
uses mock delivery sinks (and no desktop popups). Tests cover confirmed/failure
mapping, markup escaping, duplicate frames, replacement, rate limiting, escalation,
replay, daemon disconnect/recovery, notification-server restart, unavailable
service, and pending retry.

Primary references:

- [Freedesktop Desktop Notifications 1.3](https://specifications.freedesktop.org/notification/latest-single/)
- [Quickshell 0.2.1 NotificationServer](https://quickshell.org/docs/v0.2.1/types/Quickshell.Services.Notifications/NotificationServer/)
- [zbus connection builder](https://docs.rs/zbus/5.19.0/zbus/connection/struct.Builder.html)
