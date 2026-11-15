#!/bin/sh
# Hardware-independent active simulation; never initializes NVML or I2C.
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
runtime=$(mktemp -d /tmp/pinwatch-demo.XXXXXX)
cat > "$runtime/pinwatch.toml" <<CONFIG
mode = "active"
simulate = "$root/fixtures/overload.json"
socket = "$runtime/pinwatch.sock"
lock_dir = "$runtime"
state_file = "$runtime/history.json"
CONFIG
printf 'Simulation directory: %s\n' "$runtime" >&2
"$root/target/debug/pinwatchd" --config "$runtime/pinwatch.toml" &
daemon_pid=$!
trap 'kill -TERM "$daemon_pid" 2>/dev/null || :; wait "$daemon_pid" 2>/dev/null || :' EXIT INT TERM
attempt=0
while [ ! -S "$runtime/pinwatch.sock" ]; do
    attempt=$((attempt + 1))
    [ "$attempt" -lt 100 ] || exit 1
    sleep 0.05
done
"$root/target/debug/pinwatch" --socket "$runtime/pinwatch.sock" watch --json
