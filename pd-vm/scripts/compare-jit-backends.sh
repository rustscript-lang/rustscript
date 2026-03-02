#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  pd-vm/scripts/compare-jit-backends.sh [perf_test_name]

Defaults:
  perf_test_name = perf_jit_native_reduces_tight_loop_latency

Runs the same ignored perf test twice:
  1) PD_VM_JIT_CODEGEN=handwritten
  2) PD_VM_JIT_CODEGEN=cranelift

Requires:
  - pd-vm feature: cranelift-jit
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TEST_NAME="${1:-perf_jit_native_reduces_tight_loop_latency}"

run_backend() {
  local backend="$1"
  local log_file
  log_file="$(mktemp "${TMPDIR:-/tmp}/pd-vm-${backend}-XXXX.log")"
  local status="ok"

  if ! (
    cd "$REPO_ROOT"
    PD_VM_JIT_CODEGEN="$backend" \
      cargo test -p pd-vm --features cranelift-jit --test perf_tests -- \
      --ignored --exact "$TEST_NAME" --nocapture >"$log_file" 2>&1
  ); then
    status="fail"
  fi

  local line=""
  line="$(grep -E "latency median:" "$log_file" | tail -n 1 || true)"

  local interpreter="-"
  local jit="-"
  local unit="-"
  local speedup="-"

  if [[ -n "$line" ]]; then
    if [[ "$line" =~ interpreter=([0-9]+)([a-z]+)[[:space:]]+jit=([0-9]+)([a-z]+)[[:space:]]+speedup=([0-9]+(\.[0-9]+)?)x ]]; then
      interpreter="${BASH_REMATCH[1]}"
      jit="${BASH_REMATCH[3]}"
      speedup="${BASH_REMATCH[5]}"
      unit="${BASH_REMATCH[2]}"
      if [[ "${BASH_REMATCH[2]}" != "${BASH_REMATCH[4]}" ]]; then
        unit="${BASH_REMATCH[2]}/${BASH_REMATCH[4]}"
      fi
    fi
  fi

  if [[ "$status" != "ok" ]]; then
    {
      echo "backend '$backend' failed. Last 40 log lines:"
      tail -n 40 "$log_file"
    } >&2
  fi

  echo "log[$backend]: $log_file" >&2
  printf '%s|%s|%s|%s|%s|%s\n' "$backend" "$status" "$interpreter" "$jit" "$unit" "$speedup"
}

handwritten="$(run_backend handwritten)"
cranelift="$(run_backend cranelift)"

IFS='|' read -r hand_backend hand_status hand_interpreter hand_jit hand_unit hand_speedup <<<"$handwritten"
IFS='|' read -r crane_backend crane_status crane_interpreter crane_jit crane_unit crane_speedup <<<"$cranelift"

printf '\n%-12s %-8s %-12s %-12s %-8s %-10s\n' "backend" "status" "interpreter" "jit" "unit" "speedup"
printf '%-12s %-8s %-12s %-12s %-8s %-10s\n' "------------" "--------" "------------" "------------" "--------" "----------"
printf '%-12s %-8s %-12s %-12s %-8s %-10s\n' "$hand_backend" "$hand_status" "$hand_interpreter" "$hand_jit" "$hand_unit" "$hand_speedup"
printf '%-12s %-8s %-12s %-12s %-8s %-10s\n' "$crane_backend" "$crane_status" "$crane_interpreter" "$crane_jit" "$crane_unit" "$crane_speedup"

if [[ "$hand_status" == "ok" && "$crane_status" == "ok" \
   && "$hand_jit" =~ ^[0-9]+$ && "$crane_jit" =~ ^[0-9]+$ \
   && "$hand_unit" != "-" && "$hand_unit" == "$crane_unit" ]]; then
  ratio="$(awk -v a="$hand_jit" -v b="$crane_jit" 'BEGIN { if (a == 0) { print "n/a" } else { printf "%.3f", b / a } }')"
  echo
  echo "cranelift/handwritten jit ratio: $ratio (${hand_unit})"
fi
