#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_TEST_REPO="$ROOT/../RuoYi"
TEST_REPO="${TEST_REPO:-$DEFAULT_TEST_REPO}"
CS_BIN="${CS_BIN:-$ROOT/target/release/code-search}"

PASS=0
FAIL=0
SKIP=0
SUMMARY_PRINTED=0

usage() {
  cat <<'USAGE'
Usage: scripts/quality-gate.sh {quick|cli|bench|full}

Commands:
  quick  Run fast local quality gates: fmt, check, tests, diff whitespace.
  cli    Run CLI contract tests and a RuoYi smoke test when TEST_REPO exists.
  bench  Run performance regression checks via scripts/bench.sh compare.
  full   Run quick, cli, and bench in sequence.

Environment:
  TEST_REPO  Fixture repository path for CLI smoke and benchmarks.
  CS_BIN     code-search binary path. Defaults to target/release/code-search.
USAGE
}

note() {
  printf '\n== %s ==\n' "$1"
}

pass() {
  PASS=$((PASS + 1))
  printf '[PASS] %s\n' "$1"
}

fail() {
  FAIL=$((FAIL + 1))
  printf '[FAIL] %s\n' "$1"
}

skip() {
  SKIP=$((SKIP + 1))
  printf '[SKIP] %s\n' "$1"
}

run_step() {
  local label="$1"
  shift
  printf '%s\n' "-> $label"
  if "$@"; then
    pass "$label"
  else
    fail "$label"
    return 1
  fi
}

require_tool() {
  local tool="$1"
  if ! command -v "$tool" >/dev/null 2>&1; then
    fail "required tool missing: $tool"
    return 1
  fi
}

run_code_search_json() {
  "$CS_BIN" --path "$TEST_REPO" "$@"
}

assert_code_search() {
  local label="$1"
  local filter="$2"
  shift 2

  local output
  if ! output="$(run_code_search_json "$@")"; then
    fail "$label"
    return 1
  fi

  if jq -e "$filter" >/dev/null <<<"$output"; then
    pass "$label"
  else
    fail "$label"
    return 1
  fi
}

run_quick() {
  note "quick quality gate"
  cd "$ROOT"
  run_step "cargo fmt --check" cargo fmt --check
  run_step "cargo check" cargo check
  run_step "cargo test --lib" cargo test --lib
  run_step "git diff --check" git diff --check
}

run_ruoyi_smoke() {
  note "RuoYi L0 smoke"
  if [[ ! -d "$TEST_REPO" ]]; then
    skip "fixture repo not found: $TEST_REPO"
    return 0
  fi

  require_tool jq

  assert_code_search \
    "find RuoYiApplication returns results" \
    '.ok == true and (.results | length >= 1)' \
    find RuoYiApplication

  assert_code_search \
    "grep selectUserBy regex returns results" \
    '.ok == true and (.results | length >= 3)' \
    grep 'selectUserBy\w+'

  assert_code_search \
    "glob controller files returns results" \
    '.ok == true and (.results | length >= 10)' \
    glob '**/*Controller.java'

  assert_code_search \
    "read exact range returns verified source fact" \
    '.ok == true and .results[0].exact == true' \
    read ruoyi-admin/src/main/java/com/ruoyi/RuoYiApplication.java:12-16

  assert_code_search \
    "refs ShiroUtils returns source references" \
    '.ok == true and (.results | length >= 5)' \
    refs ShiroUtils

  assert_code_search \
    "status preserves source_fact reliability" \
    '.ok == true and .reliability.level == "source_fact"' \
    status
}

run_cli() {
  note "cli quality gate"
  cd "$ROOT"
  run_step "cargo build --release" cargo build --release --bin code-search
  run_step "cargo test --test cli" cargo test --test cli
  run_ruoyi_smoke
}

run_bench() {
  note "benchmark quality gate"
  cd "$ROOT"
  require_tool hyperfine
  require_tool jq
  require_tool bc
  # Reuse release binary if already built (e.g. from 'full' gate)
  if [[ ! -x "$CS_BIN" ]]; then
    run_step "cargo build --release" cargo build --release --bin code-search
  fi
  if [[ ! -d "$TEST_REPO" ]]; then
    skip "benchmark fixture repo not found: $TEST_REPO"
    return 0
  fi
  if [[ ! -d "$ROOT/scripts/baseline_values" ]]; then
    fail "baseline directory missing: scripts/baseline_values"
    return 1
  fi
  run_step "scripts/bench.sh compare" env CS_BIN="$CS_BIN" TEST_REPO="$TEST_REPO" "$ROOT/scripts/bench.sh" compare
}

summary() {
  SUMMARY_PRINTED=1
  printf '\n== quality gate summary ==\n'
  printf 'pass=%s fail=%s skip=%s\n' "$PASS" "$FAIL" "$SKIP"
  [[ "$FAIL" -eq 0 ]]
}

finish() {
  local status=$?
  if [[ "$SUMMARY_PRINTED" -eq 0 ]]; then
    summary || true
  fi
  exit "$status"
}

trap finish EXIT

main() {
  local command="${1:-}"
  case "$command" in
    quick)
      run_quick
      ;;
    cli)
      run_cli
      ;;
    bench)
      run_bench
      ;;
    full)
      run_quick
      run_cli
      run_bench
      ;;
    -h|--help|help)
      usage
      return 0
      ;;
    *)
      usage
      return 2
      ;;
  esac
  summary
}

main "$@"
