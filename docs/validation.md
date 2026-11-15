# Validation record

Validated on 2026-11-15, x86_64 Arch Linux. These results distinguish simulation
coverage from hardware validation.

## Pinned environment

- `flake.lock`: byte-for-byte identical to `/home/benoit/workspace/astro/flake.lock`.
- `rust-toolchain.toml`: byte-for-byte identical to Astro's file.
- Rust 1.94.0, Clippy 0.1.94, rustfmt 1.8.0 (Rust 1.94 release), and rust-analyzer
  1.94.0 all resolved from the same Nix toolchain output. No pin was refreshed.
- An initial overlay extension override dropped the requested components and
  exposed the host Clippy. This was fixed by retaining the TOML component list
  when adding rust-analyzer. All final checks below used the corrected environment.
- Cargo dependencies are captured in the tracked Cargo.lock; build/test commands
  use `--locked`. No GPU, CUDA toolkit, driver headers, or native libdbus build
  dependency was needed.

## Checks actually run

`nix develop path:. --command ./scripts/check.sh` (also run as the same sequence
inside the pinned shell):

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed |
| `cargo test --locked` | **43 tests passed**, none ignored |
| `cargo build --locked --release --bins` | Passed, both binaries |

Coverage: 21 decoding/configuration/selection/policy/schema tests; 3 daemon/CLI
subprocess tests; 2 durable-history tests; 7 IPC/ownership/backpressure/reconnection
tests; 10 mocked desktop notification tests. This includes a genuinely blocked
socket writer timing out while another client receives a snapshot, and notifier
transport draining rapid telemetry updates independently of desktop processing.

Other validation:

- Release binaries generated `examples/status.txt`, `examples/telemetry.json`,
  `examples/intervention.json`, and `docs/cli-help.txt`. JSON example timestamps
  and event identities were normalized; intervention is explicitly synthetic.
- Simulated active protection requested/read back **494.307 W** for 540 W board
  power, 9.1 A peak, 8.5 A threshold, 600 W existing cap and 2% headroom.
  Simulated dry-run retained 600 W and emitted proposals only.
- Both example service units were checked by `systemd-analyze verify`. The
  uninstalled `/usr/local/bin` paths naturally failed executable-existence checks;
  temporary copies with only those paths replaced by the built release binaries
  passed verification. No service was installed, started, or enabled. This is
  syntax/dependency validation, not a test of runtime sandbox/device access.
- Shell syntax checks passed for the check/demo scripts and pre-push hook.
- `git diff --check` passed. Build, direnv, generated test-output and runtime
  artifacts are ignored. Both lockfiles are tracked.
- The reference `sus` repository still has its original `susm/main.go` local
  modification; Astro is unchanged. No Quickshell files were accessed or changed.

## Read-only hardware verification

A Nix-built `pinwatch --direct devices --json` ran outside the development sandbox
(the sandbox itself hides `/dev/nvidia*` and `/dev/i2c-*`). It successfully loaded
`/usr/lib/libnvidia-ml.so.1`, whose host target is
`libnvidia-ml.so.620.70.06`, and identified the ASUS Astral at `0000:01:00.0` by NVML
UUID and the supported PCI IDs. It queried 400–600 W power constraints and minimum
graphics/memory capabilities of 180/405 MHz, without writing controls.

The GPU's PCI directory exposed adapters **1–7**. Pinwatch correctly reported
ambiguity and selected none. `/proc/devices` contained `i2c` (89) and `nvidia`
(195), matching the supplied service's character-device names. This proves the
runtime loader works on this host with Nix's toolchain; it does not prove all
Arch/driver ABI combinations or the installed service's sandbox will work.

## Not performed / limitations

- No new stress test or sensor poll on the real GPU, and **no real power/clock
  setter was called**. Sensor format is ported from the inspected reference and
  tested with packets/mocks. Confirmed real electrical response is not established.
- No desktop popup was sent. Notification mapping, retries and reconciliation
  use mocks; actual presentation by the user's notification server remains untested.
  The explicit `pinwatch notify --test` path is available for a user-requested check.
- No installed system/user service runtime test, permission/udev change, hook
  configuration change, or automatic deployment.
- aarch64 is declared in the Linux flake platforms but was not built/tested here.
- Kernel/driver calls can block; monotonic cache aging exposes stale data but cannot
  make hardware access hard-real-time. Concurrent external GPU control tools are
  not coordinated by NVML's non-atomic cap API. See the policy document for these
  limits and the historical-state versus current-hardware distinction.

## Direct-mode configuration follow-up

After adding automatic `/etc/pinwatch/pinwatch.toml` loading and explicit
`--config` support to direct mode, the pinned format, Clippy, test and release
build sequence passed again: **45 tests**. New tests verify configured GPU
selection, CLI UUID/index overrides, invalid adapter rejection, read-only mode
with an active configuration, absent default fallback, and explicit missing or
invalid configuration errors. CLI help was regenerated from the release binaries.
No real hardware read or system binary installation was performed for this change.
