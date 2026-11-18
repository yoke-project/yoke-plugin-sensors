#!/usr/bin/env bash
# Runs the application on a local Core, grants the Plugin what its Manifest declares, and opens the panel
# attached to the channel `panel`. Closing the panel, or an interrupt, stops everything.
#
# Usage: run.sh [--follow [--seconds N]]
#   --follow      in place of the panel, prints the snapshot, every reading and every event of the unit as
#                 one line each
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
plugin=com.yoke.sensors unit=sensors
manifest="$here/../plugin/$plugin.yaml"
follow="" follow_args=()
while (( $# )); do
  case "$1" in
    --follow) follow=1 ;;
    --seconds) follow_args+=(--seconds "$2"); shift ;;
    *) echo "run.sh: unknown argument $1" >&2; exit 2 ;;
  esac
  shift
done

# The Plugin as the application carries it, built from ../plugin whenever it changed.
cargo build --release --quiet --manifest-path "$here/../plugin/Cargo.toml"
mkdir -p "$here/components/plugins" "$here/manifests"
if ! cmp -s "$here/../plugin/target/release/$unit" "$here/components/plugins/$unit"; then
  cp "$here/../plugin/target/release/$unit" "$here/components/plugins/$unit"
  cp "$manifest" "$here/manifests/"
fi
yoke-compose set "units.$unit.digest" "sha256:$(sha256sum "$here/components/plugins/$unit" | cut -d' ' -f1)" --dir "$here" > /dev/null
if [[ -n "$follow" ]]; then
  cargo build --release --quiet --manifest-path "$here/follow/Cargo.toml"
else
  cargo build --release --quiet --manifest-path "$here/ui/Cargo.toml"
fi

log="$(mktemp)"
yoke app run --from "$here" > "$log" 2>&1 &
run=$!
trap 'kill -INT "$run" 2>/dev/null; wait "$run" 2>/dev/null; rm -f "$log"' EXIT
until grep -q 'is observable' "$log"; do
  kill -0 "$run" 2>/dev/null || { cat "$log" >&2; exit 1; }
  sleep 0.3
done
instance="$(sed -n '1s/ runs .*//p' "$log")"
export YOKE_INSTANCE="$instance"

# A run's instance starts with nothing granted; a grant reaches the unit at its next admission.
for capability in $(sed -n 's/^  - name: //p' "$manifest"); do
  yoke plugin grant "$plugin" "$capability" > /dev/null
done
yoke unit restart "$unit" > /dev/null
sleep 1
if [[ -z "$follow" ]]; then
  echo "run.sh: $instance runs; the panel attaches to its channel panel" >&2
  "$here/ui/target/release/sensors-ui" --instance "$instance" --channel panel --unit "$unit"
  exit
fi
# The stream, subscribed first, then the snapshot: the history wants a sensor, so it is not asked here.
section() { awk -v s="$1:" '$0 == s {on=1; next} /^[a-z]/ {on=0} on && /^  - id: / {print $3}' "$manifest"; }
for s in $(section streams); do follow_args+=(--stream "$s"); done
follow_args+=(--ask sensors.snapshot)
echo "run.sh: $instance runs; a sensor's history is asked with: echo '{\"id\":\"<id>\"}' | YOKE_INSTANCE=$instance yoke unit ask $unit sensors.history --input -" >&2
"$here/follow/target/release/follow" --instance "$instance" --channel panel --unit "$unit" "${follow_args[@]}"
