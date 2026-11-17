#!/usr/bin/env python3
"""Build a local pacman package from this checkout without installing anything."""
import argparse
import hashlib
import os
from pathlib import Path
import re
import shlex
import subprocess
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
# Explicit inclusion keeps runtime files, secrets, .git and target out of the source.
SOURCES = (
    "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "LICENSE", "README.md",
    "src", "tests", "fixtures", "examples", "docs", "install", "packaging",
)


def prepare() -> Path:
    package = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]
    version = package["version"]
    msrv = package["rust-version"]
    if not re.fullmatch(r"[0-9]+(?:\.[0-9]+){1,2}", msrv):
        raise ValueError(f"unsupported minimum Rust version: {msrv}")
    version = version.replace("-", "_")
    if not re.fullmatch(r"[0-9][A-Za-z0-9._+]*", version):
        raise ValueError(f"unsupported Arch package version: {version}")
    output = ROOT / "target" / "arch"
    output.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix=f"pinwatch-{version}-", dir=output))
    archive = stage / f"pinwatch-{version}.tar.gz"
    with tarfile.open(archive, "w:gz", dereference=False) as tar:
        for name in SOURCES:
            path = ROOT / name
            files = [path, *sorted(path.rglob("*"))] if path.is_dir() else [path]
            for file in files:
                # Do not follow links to data outside the checkout.
                if file.is_symlink() or not (file.is_file() or file.is_dir()):
                    raise ValueError(f"source must be a regular file or directory: {file}")
                if "__pycache__" in file.parts:
                    continue
                tar.add(file, arcname=f"pinwatch-{version}/{file.relative_to(ROOT)}", recursive=False)
    template = (ROOT / "packaging/arch/PKGBUILD.in").read_text()
    with archive.open("rb") as source:
        checksum = hashlib.file_digest(source, "sha256").hexdigest()
    template = template.replace("@VERSION@", version).replace("@MSRV@", msrv).replace("@SOURCE_SHA256@", checksum)
    (stage / "PKGBUILD").write_text(template)
    return stage


def native_environment() -> dict[str, str]:
    env = os.environ.copy()
    # direnv may have loaded the Nix dev shell. Build Arch binaries with Arch tools,
    # without Nix compiler wrappers, linker flags, or shared Cargo build artifacts.
    remove = {
        "IN_NIX_SHELL", "LD_LIBRARY_PATH", "LD_PRELOAD", "CC", "CXX", "LD", "AR",
        "AS", "NM", "STRIP", "OBJCOPY", "OBJDUMP", "RANLIB", "CPPFLAGS", "CFLAGS",
        "CXXFLAGS", "LDFLAGS", "PKG_CONFIG_PATH", "PKG_CONFIG_LIBDIR", "PKGDEST",
        "SRCDEST", "SRCPKGDEST", "BUILDDIR", "LOGDEST", "MAKEPKG_CONF", "MAKEPKG_LIBRARY",
    }
    for key in list(env):
        if key in remove or key.startswith(("NIX_", "CARGO_", "RUST")):
            del env[key]
    if os.environ.get("IN_NIX_SHELL"):
        # Nix shells export a synthetic epoch; it is not this package build date.
        env.pop("SOURCE_DATE_EPOCH", None)
    env.update(PATH="/usr/bin:/bin", RUSTC="/usr/bin/rustc", RUSTDOC="/usr/bin/rustdoc")
    return env


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prepare-only", action="store_true", help="create a source archive and checksummed PKGBUILD without building")
    args = parser.parse_args()
    if os.getuid() == 0:
        parser.error("run as your normal user; makepkg must not run as root")
    stage = prepare()
    print(f"Local source snapshot and PKGBUILD: {stage}", flush=True)
    if args.prepare_only:
        return
    subprocess.run(["/usr/bin/makepkg", "--check"], cwd=stage, env=native_environment(), check=True)
    print(f"Package built in {stage}. Install explicitly with pacman -U or paru -U.")
    for package in sorted(stage.glob("pinwatch-*.pkg.tar.*")):
        if not package.name.endswith(".sig"):
            print(f"sudo pacman -U {shlex.quote(str(package))}")


if __name__ == "__main__":
    main()
