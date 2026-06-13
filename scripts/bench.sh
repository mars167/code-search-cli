#!/bin/bash
# codetrail 性能基准采集和对比脚本
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

CS="${CS_BIN:-$ROOT/target/release/codetrail}"
REPO="${TEST_REPO:-$ROOT/../RuoYi}"
BASELINE_FILE="$SCRIPT_DIR/baseline.json"
BASELINE_VALUES_DIR="$SCRIPT_DIR/baseline_values"
RESULTS_DIR="$SCRIPT_DIR/bench_results"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)

: "${CODETRAIL_LSP_JAVA_READY_TIMEOUT_MS:=5000}"
export CODETRAIL_LSP_JAVA_READY_TIMEOUT_MS

mkdir -p "$RESULTS_DIR"

declare -a TESTS=(
  "startup|help|--help|3|10|"
  "startup|completions|completions bash|3|10|"
  "l0-source|status|status|2|5|"
  "l0-source|list|list ruoyi-admin/src/main/java/com/ruoyi/web/controller|2|5|"
  "l0-source|read|read ruoyi-admin/src/main/java/com/ruoyi/RuoYiApplication.java|2|5|"
  "l0-source|changed|changed|2|5|"
  "l0-source|files|files Controller|2|5|"
  "l0-source|find-path|find-path mapper|2|5|"
  "l0-source|glob|glob '**/*Controller.java'|2|5|"
  "l0-source|grep|grep 'selectUserBy\w+'|2|5|"
  "l0-source|find|find RuoYiApplication|2|5|"
  "l0-source|refs|refs ShiroUtils|2|5|"
  "l1-parser|defs|defs SysUserController|2|5|--ignore-failure"
  "l1-parser|symbols|symbols selectUserList|2|5|--ignore-failure"
  "l2-relation|calls|calls selectUserList|2|5|--ignore-failure"
  "l2-relation|callers|callers selectUserList|2|5|--ignore-failure"
  "index|index-build-cold|index build|0|3|--prepare 'rm -rf .codetrail 2>/dev/null; true'"
  "index|index-build-warm|index build|1|5|"
  "index|find-indexed|find RuoYiApplication|2|5|"
)

run_hyperfine_json() {
  local warmup="$1"
  local runs="$2"
  local raw_opts="$3"
  local full_cmd="$4"
  local json_file
  json_file="$(mktemp "${TMPDIR:-/tmp}/codetrail-bench-json.XXXXXX")"
  local args=(--warmup "$warmup" --min-runs "$runs" --export-json "$json_file")

  case "$raw_opts" in
    "")
      ;;
    "--ignore-failure")
      args+=(--ignore-failure)
      ;;
    "--prepare 'rm -rf .codetrail 2>/dev/null; true'")
      args+=(--prepare "rm -rf .codetrail 2>/dev/null; true")
      ;;
    *)
      echo "Unsupported hyperfine option set: $raw_opts" >&2
      rm -f "$json_file"
      return 2
      ;;
  esac

  if hyperfine "${args[@]}" "$full_cmd" >/dev/null; then
    cat "$json_file"
    rm -f "$json_file"
  else
    rm -f "$json_file"
    return 1
  fi
}

collect() {
  echo "=== Collecting baseline: $TIMESTAMP ==="
  echo "Binary: $CS"
  echo "Repo:   $REPO"
  echo ""

  cd "$REPO"

  local current_group=""
  for entry in "${TESTS[@]}"; do
    IFS='|' read -r group name cmd warmup runs raw_opts <<< "$entry"
    local extra_opts=""
    [ -n "$raw_opts" ] && extra_opts="$raw_opts"

    [ "$group" != "$current_group" ] && { current_group="$group"; echo ""; echo "== [$group] =="; }

    local full_cmd="$CS --path $REPO $cmd"
    local label="${group}/${name}"
    printf "  %-35s " "$label"

    local output status
    if output=$(run_hyperfine_json "$warmup" "$runs" "$extra_opts" "$full_cmd" 2>/dev/null); then
      local mean
      mean=$(echo "$output" | jq -r '.results[0].mean')
      local mean_ms
      mean_ms=$(awk -v seconds="$mean" 'BEGIN { printf "%.0f", seconds * 1000 }')
      printf "%8.0fms\n" "$mean_ms"
    else
      echo "FAILED"
    fi
  done

  echo ""; echo "== [memory] =="
  cd "$REPO"
  for pair in "find:RuoYiApplication" "grep:selectUserBy" "index-build:"; do
    IFS=':' read -r cmd args <<< "$pair"
    printf "  %-35s " "$cmd"
    local mem
    mem=$(/usr/bin/time -l $CS --path "$REPO" $cmd "$args" 2>&1 | \
          grep "maximum resident" | awk '{print $1}' | sed 's/^0*//')
    printf "%8.0fKB\n" "$mem"
  done
}

compare() {
  echo "=== Performance Regression Check ==="
  echo ""

  cd "$REPO"
  local pass=0 fail=0 warn=0

  for entry in "${TESTS[@]}"; do
    IFS='|' read -r group name cmd warmup runs raw_opts <<< "$entry"
    local extra_opts=""
    [ -n "$raw_opts" ] && extra_opts="$raw_opts"
    local label="${group}/${name}"

    local full_cmd="$CS --path $REPO $cmd"
    local current
    if ! current=$(run_hyperfine_json "$warmup" "$runs" "$extra_opts" "$full_cmd" 2>/dev/null | \
        jq -r '.results[0].mean'); then
      echo "  ❌ $label — failed"; fail=$((fail+1)); continue
    fi

    local bl="$BASELINE_VALUES_DIR/$name" tmp
    if [ -f "$bl" ]; then
      read -r tmp < "$bl"
    else
      echo "  ⬜ $label — no baseline"; continue
    fi

    local diff_pct
    diff_pct=$(echo "scale=1; ($current - $tmp) / $tmp * 100" | bc)
    local baseline_ms current_ms
    baseline_ms=$(awk -v seconds="$tmp" 'BEGIN { printf "%.0f", seconds * 1000 }')
    current_ms=$(awk -v seconds="$current" 'BEGIN { printf "%.0f", seconds * 1000 }')
    local status="✅"; local sev="OK"
    if (( $(echo "$diff_pct > 30" | bc -l) )); then
      status="🔴"; sev="P1"; fail=$((fail+1))
    elif (( $(echo "$diff_pct > 15" | bc -l) )); then
      status="🟡"; sev="P2"; warn=$((warn+1))
    elif (( $(echo "$diff_pct < -20" | bc -l) )); then
      status="🟢"; sev="FASTER"; pass=$((pass+1))
    else
      pass=$((pass+1))
    fi

    printf "  %s %-30s %-6s %7.0fms → %7.0fms  %+5.1f%%\n" \
      "$status" "$label" "[$sev]" "$baseline_ms" "$current_ms" "$diff_pct"
  done

  echo ""; echo "=== Summary: $pass passed, $warn warnings, $fail regressions ==="
  [ $fail -gt 0 ] && exit 1 || exit 0
}

save-baseline() {
  cd "$REPO"
  mkdir -p "$BASELINE_VALUES_DIR"
  for entry in "${TESTS[@]}"; do
    IFS='|' read -r group name cmd warmup runs raw_opts <<< "$entry"
    local extra_opts=""
    [ -n "$raw_opts" ] && extra_opts="$raw_opts"
    local full_cmd="$CS --path $REPO $cmd"
    local val
    if val=$(run_hyperfine_json "$warmup" "$runs" "$extra_opts" "$full_cmd" 2>/dev/null | \
        jq -r '.results[0].mean'); then
      echo "$val" > "$BASELINE_VALUES_DIR/$name"
    fi
  done
  echo "Baseline values saved to $BASELINE_VALUES_DIR"
}

case "${1:-collect}" in
  collect)       collect ;;
  compare)       compare ;;
  save-baseline) save-baseline ;;
  *)             echo "Usage: $0 {collect|save-baseline|compare}" ; exit 1 ;;
esac
