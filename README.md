# Pinwatch

Linux monitoring and optional current-triggered protection for the supported ASUS
RTX 5090 connector sensors. A shared Rust library powers `pinwatch` (CLI and
unprivileged desktop companion) and `pinwatchd` (polling and optional controls).
Default operation is **monitor-only**. No installation, service activation, GPU
stress test, or real power/clock change is performed by building or testing.

## Supported hardware

Support follows the inspected `sus` implementation, including its locally added
current display. **Not every RTX 5090 has these sensors.** Both PCI IDs must match:

| NVIDIA device/vendor | ASUS subsystem/vendor | Reference model label |
|---|---|---|
| `0x2b8510de` | `0x89e31043` | ROG-ASTRAL-RTX5090-O32G |
| `0x2b8510de` | `0x8a2e1043` | ROG-ASTRAL-RTX5090-O32G-WHITE |
| `0x2b8510de` | `0x8a5a1043` | ROG- STRIX-RTX5090-032G-BTF |

The last label preserves the reference's spelling. No additional vendor backend
or guessed model support is implemented. Device UUID is the stable identity;
indices are convenience selectors only. PCI function is preserved from NVML.

The sensor is address `0x2b`, register `0x80`, fixed 24-byte SMBus **I²C block read**.
Six four-byte big-endian voltage/current records represent **six supply channels**.
These are not twelve independent contact measurements and no physical pin mapping
is verified. Pinwatch does not measure ground contacts or connector temperature.

## Reproducible build and editor environment

The Nix inputs and **exact `flake.lock`** are copied from Astro without refreshing
any pins. Linux targets: x86_64 and aarch64 (only x86_64 validated here).
`rust-toolchain.toml` selects **Rust 1.94.0** with clippy, rustfmt and rust-src;
it is the version authority. The flake uses
`pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml`, preserving all listed
components and adding **the same release's rust-analyzer**, rather than mixing in
a host or newer nixpkgs analyzer. Cargo's `rust-version` is an MSRV declaration,
not a separately selected compiler. Both lockfiles belong in Git.

```sh
nix develop
cargo build --locked
cargo run --locked --bin pinwatch -- --help
cargo run --locked --bin pinwatchd -- --help
# Full validation, including release binaries:
./scripts/check.sh
# Or from outside the shell:
nix develop --command ./scripts/check.sh
```

Individual checks are `cargo fmt --all -- --check`,
`cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, and
`cargo build --locked --release --bins`. Before initial files are staged, Nix's Git
source filtering can omit new files: use `nix develop path:.` during bootstrap.
A clean checkout works with plain `nix develop`. `.envrc` contains `use flake`;
optionally run `direnv allow` after reviewing it. Launch your editor from that
shell or use its direnv integration; check `command -v rustc cargo-clippy rustfmt
rust-analyzer` if editor/build tools disagree. Do not substitute host `cargo-clippy`.

Enable the Astro-style pre-push hook explicitly:

```sh
git config core.hooksPath .githooks
```

It runs `nix develop --command cargo test --locked`. Nothing pushes automatically.
No CUDA toolkit, GPU, driver headers, system libdbus development package, or other
native development dependency is required. NVML is loaded dynamically through
`nvml-wrapper`; I²C ioctl handling is in `i2cdev`; D-Bus uses pure-Rust `zbus`.
Pinwatch's own library forbids unsafe code. Checked packet lengths/errors stay at
the hardware boundary. Crate versions/checksums are fixed by `Cargo.lock`.

## GPU-free demonstration

```sh
nix develop --command cargo build --locked
./scripts/demo.sh
```

The demo creates a private directory in `/tmp`, starts an active **simulated**
daemon, and streams flushed JSON. Ctrl-C stops it without touching hardware. It
retains the private directory for inspection. The fixture starts with representative
historical normal readings, then a synthetic 9.1 A overload, a lagging reading, and
recovery. Simulation changes only the mock cap. It does not load NVML or open I²C.

For a single read-only simulated snapshot:

```sh
runtime=$(mktemp -d /tmp/pinwatch-direct.XXXXXX)
./target/debug/pinwatch --direct --simulate fixtures/overload.json \
  --lock-dir "$runtime" status --json
```

Tests exercise both binaries as subprocesses, active/dry-run mocks, IPC ownership,
slow/disconnecting clients, clean shutdown, events, and mocked notifications.
`pinwatch notify --test` is a separate, explicitly requested real-popup test; it is
never invoked by the test suite.

## CLI and ownership

```sh
# Read-only NVML enumeration; does not read I²C or require the daemon:
pinwatch --direct devices --json
# Normal usage: cached daemon telemetry, no extra sensor polling:
pinwatch devices
pinwatch --uuid GPU-your-uuid status --json
pinwatch --index 0 status
pinwatch watch
pinwatch watch --json
# Desktop-session companion (unprivileged):
pinwatch notify
```

Human output includes all six channels in volts, amps and watts, board power,
connector total, configured cap, separate power/current balance ratios, freshness
and protection status. See [example output](examples/status.txt),
[JSON snapshot](examples/telemetry.json), and [CLI help](docs/cli-help.txt).
Diagnostics are on stderr; JSON stdout has one complete, promptly flushed object
per line and no ANSI codes. `watch` exits on daemon disconnect; supervise/restart
it and mark UI data unavailable. `notify` handles its own IPC reconnection.

The daemon normally owns polling and shares cached frames with all CLI/Quickshell
clients. Direct monitoring is explicitly **read-only**, needs device permissions,
and must hold the same per-UUID exclusive advisory lock in `/run/pinwatch` as the
daemon. Real-hardware lock-directory overrides are rejected. Multiple Pinwatch
pollers cannot coexist for one UUID. Never remove live lock files/directories;
locks deliberately persist across exits. This coordinates Pinwatch processes,
not `sus`, i2cdetect, or unrelated tools: stop other sensor pollers yourself.

Adapter discovery lists only `i2c-N` entries under the GPU's PCI sysfs directory.
One adapter can be selected automatically. None gives an error; **multiple adapters
require an explicit UUID-to-adapter mapping**. The old lowest-number heuristic is
not used. An override must belong to that GPU; no arbitrary bus scanning or sensor
register writes are performed. Read-only I²C access still needs read/write open
permission for the kernel ioctl interface. Direct use after provisioning:

```sh
# Example only: choose the adapter from known hardware/reference information.
# The daemon must be stopped and /run/pinwatch must exist with safe permissions.
sudo pinwatch --direct --uuid GPU-your-uuid --adapter 1 status
```

Do not use `i2cdetect` or sweep adapters to discover this sensor. On this host,
read-only NVML discovery found adapters 1–7, so Pinwatch intentionally declined to
choose one. The original project's choice of 1 is reference evidence, not proof
that every installation uses 1.

## NVML on Arch with Nix

The loader tries `libnvidia-ml.so.1`, then Arch's
`/usr/lib/libnvidia-ml.so.1`, then `/run/opengl-driver/lib/libnvidia-ml.so.1`
for NixOS compatibility. A configured `nvml_library` (CLI `--nvml-library`)
replaces that search; use only a trusted administrator-controlled library path.
The daemon does not consult a user-provided environment override.

On this Arch host, `/usr/lib/libnvidia-ml.so.1` resolves to
`libnvidia-ml.so.620.70.06`. A Nix-built binary successfully loaded it and queried
the ASUS Astral UUID/PCI location, real power constraints (400–600 W), and minimum
clock capabilities (180/405 MHz). No GPU power/clock setter or I²C read was needed
for that check. Other hosts can have different runtime driver/dependency ABI
requirements; inspect the loader's stderr, library symlink, and `ldd` output.
Do not globally set `LD_LIBRARY_PATH=/usr/lib` inside Nix to solve this: that can
replace unrelated Nix libraries. Builds and simulated tests never need NVML.

## System service installation (explicit administrator steps)

Review the configuration and unit first. These commands are instructions, **not
steps run by development/test scripts**. Install the two release binaries at the
unit's paths, create an IPC group, and choose authorized desktop readers:

```sh
sudo groupadd --system pinwatch              # only if absent
sudo install -m755 target/release/pinwatch target/release/pinwatchd /usr/local/bin/
sudo install -d -m755 /etc/pinwatch
sudo install -m640 -o root -g root examples/pinwatch.toml /etc/pinwatch/pinwatch.toml
sudo install -m644 install/pinwatchd.service /etc/systemd/system/
sudo usermod -aG pinwatch YOUR_DESKTOP_USER   # log in again afterward
sudo modprobe i2c-dev                       # provision module loading separately if desired
```

Edit `/etc/pinwatch/pinwatch.toml`: keep `mode = "monitor"` initially and populate
`[adapters]` using the stable GPU UUID and known sensor adapter. Then, explicitly:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now pinwatchd.service
journalctl -u pinwatchd -f
```

Systemd creates `/run/pinwatch` (`root:pinwatch`, 0750) and
`/var/lib/pinwatch` (0700). The socket is 0660, ownership locks 0644, and history
0600. RuntimeDirectoryPreserve avoids replacing lock inodes between service runs.
The daemon refuses unsafe runtime directories, symlinks/non-sockets at the socket
path, a live endpoint, duplicate ownership, and invalid history. Stale socket
cleanup is done only while holding the socket lock; shutdown checks its inode.

The system unit uses root with a `CAP_SYS_ADMIN` bounding set because NVIDIA
setters may require it; `NoNewPrivileges` prevents gaining additional privileges.
`PrivateDevices=no` and the actual `char-i2c` / `char-nvidia` character-device
allowlist permit host ioctls. Those names were checked in this host's `/proc/devices`
(majors 89 and 195). `ProtectSystem=strict` leaves only systemd-managed runtime/state
paths writable, `ProtectHome=yes` hides user homes, AF_UNIX permits the IPC socket,
and `PrivateNetwork=yes` prevents network use. No copied blanket privileged/ioctl
syscall filter is used. NVML loading requires the host library path to remain visible.
If a different driver requires additional device nodes, diagnose the denied
access before extending the allowlist; do not disable all isolation blindly.

For extra monitor-only hardening, use a service override with an empty
`CapabilityBoundingSet=` and `ExecStart=` followed by the same command with
`--monitor-only`. The program performs no controls in monitor mode even without
this override. Root simplifies device permissions in the supplied example; a
non-root monitor may instead be provisioned with access to only the necessary
I²C/NVIDIA nodes and runtime/state directories. Pinwatch does not install udev
rules or alter device permissions automatically. The desktop companion needs
**only** the IPC group, never GPU device access or `CAP_SYS_ADMIN`.

For active protection, review [the exact decision process and limitations](docs/protection.md),
change `mode = "active"`, and explicitly restart the service. `--dry-run` forces
read-only proposed actions regardless of an active config. There is no automatic
restore command, shutdown reset, or IPC to raise power/unlock clocks. A system
restart may preserve restrictions; consult actual current NVML state plus the
historical event records. Other tools can also change GPU settings.

Desktop companion setup and the optional graphical-session user unit are covered
in [desktop notifications](docs/notifications.md). A conforming notification
server receives standard D-Bus messages; Quickshell needs no Pinwatch-specific API.

## Quickshell / other clients

Use `pinwatch watch --json` as a `Process` with a line parser. Parse each object,
select by `device.gpu_uuid`, display all six channels, honor nulls/freshness, and
mark data unavailable/restart on process exit. The [versioned protocol and short
consumption recipe](docs/protocol.md) cover fields, units, event replay, and bounds.
No Quickshell configuration or widget is included or changed.

## Troubleshooting and limits

- **NVML missing / initialization failed:** check the dynamic library path, driver
  installation/module, and `/dev/nvidia*` permissions. Simulations remain usable.
- **No sensor / ambiguous adapters:** check `i2c-dev`, PCI IDs and `[adapters]`.
  Unsupported models stay unsupported even if NVML can report board power.
- **Permission denied opening I²C:** check device open permissions and systemd
  DeviceAllow names. A notification companion does not need those permissions.
- **Another instance owns device/socket:** use cached IPC, stop the other poller,
  or wait for it to exit. Do not delete lock files to bypass ownership.
- **Runtime directory absent:** the system unit creates it. For manual daemon
  operation, explicitly provision the same root-owned directories and group/modes.
  A private writable runtime override is supported only for simulation.
- **Degraded / armed_unverified:** inspect read errors and control capabilities.
  Query support does not establish write permission. Dry-run is not protection;
  no check here claims real setters have been tested.
- **No desktop popup:** check companion stderr/user journal, socket group/session
  membership, session bus ownership, notification service and shell DND. It does
  not indicate whether GPU protection succeeded. Use system journal and telemetry.
- **Stale data / lost GPU / blocked driver:** no telemetry-loss cap is guessed.
  Cached age increases and protection is degraded. See the policy's failure and
  restart behavior; software is not a hardware interlock.

Hardware-independent tests verify policy and protocol behavior. Real interventions,
service hardening under a running installed unit, desktop presentation, aarch64,
and protective response under load have **not** been validated on hardware. No new
stress test, service installation, popup, or hardware setting change was performed.
See [validation record](docs/validation.md) for checks actually run.

## Design and references

`backend` owns NVML/simulation; `sensor` decodes/discovers/reads; `model` defines the
JSON contract; `policy` decides and executes via a mockable trait; `daemon` owns
polling/history; `ipc` handles bounded read-only transport; `notify` maps events to
session notifications. No speculative multi-vendor framework or extra workspace
crates are needed. The reference repositories were inspected read-only.

Primary API references used during implementation:

- [nvml-wrapper](https://docs.rs/nvml-wrapper/0.12.1/nvml_wrapper/) (also inspected the locked crate source/API docs)
- [i2cdev I2CDevice](https://docs.rs/i2cdev/0.6.2/i2cdev/core/trait.I2CDevice.html)
- [Linux I²C block transaction semantics](https://docs.kernel.org/i2c/smbus-protocol.html#i2c-block-transactions)
- [zbus](https://docs.rs/zbus/5.19.0/zbus/)
- [Freedesktop notifications](https://specifications.freedesktop.org/notification/latest-single/)
- [Quickshell NotificationServer](https://quickshell.org/docs/v0.2.1/types/Quickshell.Services.Notifications/NotificationServer/)

Hardware IDs, model mapping, register layout, and original current-triggered policy
come from Jan Provaznik's local `sus` source and its listed contributors/references.
The local current-display addition was included in the inspection. The Rust source
is a new implementation; `sus` and Astro were not modified.
