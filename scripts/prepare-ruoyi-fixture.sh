#!/usr/bin/env bash
set -euo pipefail

RUOYI_REPO_URL="${RUOYI_REPO_URL:-https://git.home.arpa/mars/RuoYi.git}"
RUOYI_BRANCH="${RUOYI_BRANCH:-master}"
RUOYI_CACHE_DIR="${RUOYI_CACHE_DIR:-/cache/code-search/fixtures/RuoYi}"
RUOYI_REFRESH="${RUOYI_REFRESH:-0}"

cache_parent="$(dirname "$RUOYI_CACHE_DIR")"
lock_dir="$cache_parent/RuoYi.lock"
tmp_dir=""
lock_acquired=0

cleanup() {
  if [[ -n "$tmp_dir" ]]; then
    rm -rf "$tmp_dir"
  fi
  if [[ "$lock_acquired" -eq 1 ]]; then
    rmdir "$lock_dir" 2>/dev/null || true
  fi
}

trap cleanup EXIT

is_valid_cache() {
  [[ -d "$RUOYI_CACHE_DIR/.git" ]] &&
    git -C "$RUOYI_CACHE_DIR" rev-parse --is-inside-work-tree >/dev/null 2>&1
}

cleanup_stale_cache_artifacts() {
  # Canceled jobs can leave lock or temp directories behind.
  find "$cache_parent" -maxdepth 1 -name 'RuoYi.tmp.*' -mmin +10 -exec rm -rf {} + 2>/dev/null || true
  if [[ -d "$lock_dir" ]]; then
    find "$lock_dir" -maxdepth 0 -mmin +10 -exec rm -rf {} + 2>/dev/null || true
  fi
}

acquire_lock() {
  local attempt
  for attempt in $(seq 1 120); do
    cleanup_stale_cache_artifacts
    if mkdir "$lock_dir" 2>/dev/null; then
      lock_acquired=1
      return 0
    fi
    sleep 1
  done

  printf 'Timed out waiting for RuoYi fixture cache lock: %s\n' "$lock_dir" >&2
  return 1
}

clone_cache() {
  rm -rf "$RUOYI_CACHE_DIR"
  tmp_dir="$(mktemp -d "$cache_parent/RuoYi.tmp.XXXXXX")"
  git clone --depth 1 --single-branch --branch "$RUOYI_BRANCH" "$RUOYI_REPO_URL" "$tmp_dir"
  mv "$tmp_dir" "$RUOYI_CACHE_DIR"
  tmp_dir=""
}

mkdir -p "$cache_parent"
cleanup_stale_cache_artifacts

if [[ "$RUOYI_REFRESH" == "1" ]] || ! is_valid_cache; then
  acquire_lock
  if [[ "$RUOYI_REFRESH" == "1" ]] || ! is_valid_cache; then
    clone_cache
  fi
fi

fixture_head="$(git -C "$RUOYI_CACHE_DIR" rev-parse --short HEAD)"
fixture_shallow="$(git -C "$RUOYI_CACHE_DIR" rev-parse --is-shallow-repository)"
printf 'RuoYi fixture ready: path=%s head=%s shallow=%s\n' \
  "$RUOYI_CACHE_DIR" "$fixture_head" "$fixture_shallow"
