#!/usr/bin/env bash
# The watch: this Plugin against what Yoke last published. The newest programs are installed from the module
# proxy, the Plugin is built and taken into try/ by them as anyone's would be, run under a real Core, and asked
# its snapshot. A runner may have few sensors or none; the snapshot is answered either way, with its
# sequence, and that is the Plugin admitted, reading and answering under the release.
# Usage: watch.sh [results directory]
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
results="${1:-$root/.results}"
mkdir -p "$results"
date -u +%Y-%m-%dT%H:%M:%SZ > "$results/started"
CASE=yoke-plugin-sensors:the-watch.01

finish() { # <verdict line>
  echo "$1" | tee "$results/checks.txt"
  date -u +%Y-%m-%dT%H:%M:%SZ > "$results/finished"
  [[ "$1" == pass* ]]
  exit $?
}

# Short, since the longest socket path of the instance is computed under it and bounded.
work="$(mktemp -d /tmp/w.XXX)"
trap 'kill "${running:-0}" 2>/dev/null; rm -rf "$work"' EXIT
mkdir -p "$work/bin" "$work/r" "$work/state" "$work/config/yoke"
export GOBIN="$work/bin" PATH="$work/bin:$PATH" XDG_RUNTIME_DIR="$work/r" XDG_STATE_HOME="$work/state" XDG_CONFIG_HOME="$work/config"
printf 'install_root: %s\n' "$work/apps" > "$work/config/yoke/manager.conf"

release="$(go list -m -f '{{.Version}}' github.com/yoke-project/yoke@latest)" || finish "FAIL  $CASE — the newest release of yoke cannot be read"
echo "watch: yoke $release"
echo "$release" > "$results/release"
for program in yoke yoke-compose yoke-core yoke-guardian yoke-auth; do
  go install "github.com/yoke-project/yoke/cmd/$program@$release" || finish "FAIL  $CASE — $program $release does not install"
done

cd "$root/try" || finish "FAIL  $CASE — no try/"
yoke-compose remove units.sensors > /dev/null 2>&1
rm -f components/plugins/sensors manifests/com.yoke.sensors.yaml
said="$(yoke-compose add plugin .. --yes 2>&1)" || finish "FAIL  $CASE — the Plugin is not taken under $release: $(tail -n 2 <<<"$said" | tr '\n' ' ')"

cd "$root" || exit 1
yoke app run --from try > "$work/run.log" 2>&1 &
running=$!
for _ in $(seq 1 30); do
  grep -q 'is observable' "$work/run.log" && break
  sleep 1
done
grep -q 'is observable' "$work/run.log" || finish "FAIL  $CASE — the instance did not come up under $release: $(tail -n 3 "$work/run.log" | tr '\n' ' ')"
instance="$(sed -n '1s/ runs .*//p' "$work/run.log" | head -n 1)"

# std: yoke-plugin-sensors:the-watch.01
probe() { # <instance>; says why on a failure
  yoke plugin grant com.yoke.sensors query.sensors.snapshot.answer --instance "$1" > /dev/null || { echo "the grant was refused"; return 1; }
  yoke unit restart sensors --instance "$1" > /dev/null || { echo "the unit did not restart"; return 1; }
  sleep 3
  local said
  said="$(yoke unit ask sensors sensors.snapshot --instance "$1" 2>&1)" || { echo "the question was refused: $said"; return 1; }
  grep -q '"sequence"' <<<"$said" || { echo "the answer holds no sequence: $said"; return 1; }
  grep -q '"sources"' <<<"$said" || { echo "the answer says nothing of where the sensors come from: $said"; return 1; }
}

if probe "$instance" > "$work/why"; then verdict="pass  $CASE"; else verdict="FAIL  $CASE — under $release: $(cat "$work/why")"; fi
kill -INT "$running" 2>/dev/null
for _ in $(seq 1 30); do kill -0 "$running" 2>/dev/null || break; sleep 1; done
finish "$verdict"
