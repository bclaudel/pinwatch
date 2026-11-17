# Local Arch package (no AUR publication)

Build a pacman package from your current checkout, then install/upgrade/remove it
with pacman or paru. No remote Git repository, AUR submission, or custom repository
is involved. The helper never installs dependencies/the package, enables a
service, changes permissions on hardware, or invokes GPU controls.

## Build

On Arch, install prerequisites if needed:

```sh
sudo pacman -S --needed base-devel rust python
```

Run as your normal user from the checkout:

```sh
./scripts/package-arch.py
```

This snapshots the current source files, **including local edits**, into a unique
ignored `target/arch/pinwatch-VERSION-.../` directory. It generates a `PKGBUILD`
from `packaging/arch/PKGBUILD.in` with the version from Cargo.toml and the actual
SHA-256 checksum of the source archive. The archive includes source, tests,
fixtures, documentation, configuration examples, and service files; it excludes
`.git`, build/runtime directories, and symlinks. Review your source before building.
Use `--prepare-only` to create the archive/PKGBUILD without running makepkg.

The helper runs native `/usr/bin/makepkg --check`. It clears inherited Nix compiler
and linker settings and uses Arch's compiler/tools, even if direnv loaded the
project's Nix shell. This produces ordinary Arch-linked binaries, without a runtime
Nix-store dependency. No Rust version or dependency pin is refreshed: Cargo.lock
is enforced (`cargo fetch --locked`, then frozen/offline build and tests).
The **packaging compiler** is the installed Arch Rust (at least Cargo.toml's 1.94
MSRV), while reproducible **development** remains pinned through Nix and
rust-toolchain.toml. Arch system packages/makepkg configuration are not frozen by
this recipe; the package records its build environment in `.BUILDINFO`.

The build/test stage needs no NVIDIA driver, I²C device, desktop session or GPU.
The initial Cargo fetch may need network access. Missing dependencies fail the
build with makepkg's list; the helper never silently runs `pacman -S`.

## Install / upgrade

The helper prints its output directory. Inspect and install the resulting file
(substitute the actual path; keep the leading `./` for paru):

```sh
pacman -Qip ./target/arch/pinwatch-VERSION-.../pinwatch-VERSION-1-x86_64.pkg.tar.zst
sudo pacman -U ./target/arch/pinwatch-VERSION-.../pinwatch-VERSION-1-x86_64.pkg.tar.zst
# Alternative to the preceding install command:
paru -U ./target/arch/pinwatch-VERSION-.../pinwatch-VERSION-1-x86_64.pkg.tar.zst
```

For local upgrades, rerun the helper and install the new archive with `-U`.
The version follows Cargo.toml; repeated development builds can have the same
version and pacman will reinstall them. No AUR update will track your checkout.
Package extensions may differ if you customized makepkg.conf. Do not use
`pacman -Sy` for build prerequisites; maintain Arch with normal full upgrades.

Files installed:

| Path | Contents |
|---|---|
| `/usr/bin/pinwatch`, `/usr/bin/pinwatchd` | Native Arch executables |
| `/etc/pinwatch/pinwatch.toml` | Monitor-only default configuration, 0644 |
| `/usr/lib/systemd/system/pinwatchd.service` | System daemon, using `/usr/bin` |
| `/usr/lib/systemd/user/pinwatch-notify.service` | Optional desktop companion, using `/usr/bin` |
| `/usr/lib/sysusers.d/pinwatch.conf` | Definition of the `pinwatch` IPC group |
| `/usr/share/doc/pinwatch/` | README, protocol/policy docs and examples |
| `/usr/share/licenses/pinwatch/LICENSE` | MIT license |

Arch's existing systemd package hooks create the IPC group and reload unit
metadata. No package script starts/enables/restarts services, adds users to the
group, loads i2c-dev, or edits udev rules. The configuration is a pacman `backup`
file: local changes are preserved during upgrades, with `.pacnew` files when
applicable. Review those files rather than replacing your GPU mapping blindly.

After installation, configure the UUID/adapter and choose deployment mode as
explained in the README. Service activation and user group membership remain
explicit administrative steps. A running daemon keeps its old executable until
you explicitly restart it after an upgrade. Stopping it never restores caps/clocks.

## Migrating an existing manual installation

Files in `/usr/local` and administrator units in `/etc` are outside pacman's
ownership. They can shadow the packaged binaries/units, so address these before
switching. Nothing in the build helper moves or removes them automatically.

1. Stop/disable the existing services with `sudo systemctl disable --now pinwatchd`
   and, if enabled, `systemctl --user disable --now pinwatch-notify`.
2. Back up `/etc/pinwatch/pinwatch.toml`, then move the manually installed file out
   of that exact path (for example to `pinwatch.toml.before-pacman`) so it cannot
   conflict with package installation. Keep your UUID/adapter mapping for review.
3. Move the old `/usr/local/bin/pinwatch` and `/usr/local/bin/pinwatchd` aside.
   Move `/etc/systemd/system/pinwatchd.service` aside as well. If you installed
   `~/.config/systemd/user/pinwatch-notify.service`, move that aside; it overrides
   the vendor user unit too. Preserve any intentional overrides for review.
4. Install the package. Merge your GPU mapping/settings into the new config,
   deliberately choosing monitor/dry-run/active mode.
5. Reload system/user systemd metadata if needed, then explicitly enable/start
   whichever services you want. Check `command -v pinwatch` points to `/usr/bin`,
   `systemctl cat pinwatchd` shows the packaged unit, and `pinwatch status` works.

Retain `/var/lib/pinwatch` history and the `/run/pinwatch` ownership lock inodes.
Do not erase locks while any direct monitor/daemon is running. The existing
`pinwatch` group is reused. Installation does not reset hardware restrictions.

## Uninstall

Stop/disable services first, then remove the package:

```sh
systemctl --user disable --now pinwatch-notify.service  # if enabled for this user
sudo systemctl disable --now pinwatchd.service
sudo pacman -Rns pinwatch
# Or: paru -Rns pinwatch
```

Pacman removes package-owned binaries/units/docs. Modified config files can be
saved as `.pacsave` (pacman's `--nosave` option changes that). Runtime/state data,
restriction history, manually installed files, enabled units in other user sessions,
and group membership are not automatically erased. Review those separately.
Uninstalling Pinwatch does not raise GPU caps or unlock clocks. No hardware-control
restoration is performed by the package.

References: [makepkg](https://man.archlinux.org/man/makepkg.8.en),
[PKGBUILD](https://man.archlinux.org/man/PKGBUILD.5.en),
[pacman](https://man.archlinux.org/man/pacman.8.en).
