#!/usr/bin/env bash
set -euo pipefail

assets_dir="${1:-dist}"

for tool in curl jq python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Missing required tool: $tool" >&2
    exit 1
  fi
done

: "${RELEASE_TAG:?RELEASE_TAG is required}"
: "${GITEA_TOKEN:?GITEA_TOKEN is required}"
: "${GITEA_URL:=https://git.home.arpa}"
: "${GITEA_REPOSITORY:=mars/code-search-cli}"

if [ ! -d "$assets_dir" ]; then
  echo "Assets directory does not exist: $assets_dir" >&2
  exit 1
fi

GITEA_URL="${GITEA_URL%/}"
api_base="${GITEA_URL}/api/v1"
auth_header="Authorization: token ${GITEA_TOKEN}"
target_sha="${TARGET_SHA:-}"

release_json="$(mktemp)"
status="$(
  curl -sS -o "$release_json" -w '%{http_code}' \
    -H "$auth_header" \
    "${api_base}/repos/${GITEA_REPOSITORY}/releases/tags/${RELEASE_TAG}"
)"

if [ "$status" = "404" ]; then
  payload="$(mktemp)"
  jq -n \
    --arg tag "$RELEASE_TAG" \
    --arg target "$target_sha" \
    '{
      tag_name: $tag,
      name: $tag,
      body: ("Automated release for " + $tag + "."),
      draft: false,
      prerelease: false
    } + (if $target == "" then {} else {target_commitish: $target} end)' \
    > "$payload"
  status="$(
    curl -sS -o "$release_json" -w '%{http_code}' \
      -X POST \
      -H "$auth_header" \
      -H "Content-Type: application/json" \
      -d @"$payload" \
      "${api_base}/repos/${GITEA_REPOSITORY}/releases"
  )"
fi

if [ "$status" -lt 200 ] || [ "$status" -ge 300 ]; then
  cat "$release_json"
  exit 1
fi

release_id="$(jq -r '.id // empty' "$release_json")"
if [ -z "$release_id" ]; then
  cat "$release_json"
  exit 1
fi

assets_json="$(mktemp)"
curl -fsS \
  -H "$auth_header" \
  "${api_base}/repos/${GITEA_REPOSITORY}/releases/${release_id}/assets" \
  > "$assets_json"

uploaded=0
for file in "$assets_dir"/code-search-* "$assets_dir"/SHA256SUMS; do
  [ -f "$file" ] || continue
  name="$(basename "$file")"

  while IFS= read -r asset_id; do
    [ -z "$asset_id" ] && continue
    curl -fsS \
      -X DELETE \
      -H "$auth_header" \
      "${api_base}/repos/${GITEA_REPOSITORY}/releases/${release_id}/assets/${asset_id}" \
      >/dev/null
  done < <(jq -r --arg name "$name" '.[] | select(.name == $name) | .id' "$assets_json")

  encoded_name="$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$name")"
  curl -fsS \
    -X POST \
    -H "$auth_header" \
    -F "attachment=@${file}" \
    "${api_base}/repos/${GITEA_REPOSITORY}/releases/${release_id}/assets?name=${encoded_name}" \
    >/dev/null
  uploaded=$((uploaded + 1))
done

if [ "$uploaded" -eq 0 ]; then
  echo "No release assets found in $assets_dir" >&2
  exit 1
fi

echo "Uploaded $uploaded Gitea release assets for $RELEASE_TAG."
