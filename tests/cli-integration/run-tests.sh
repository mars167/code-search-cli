#!/usr/bin/env bash
set -euo pipefail
# CodeTrail CLI Full Test Suite

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CODETRAIL="${SCRIPT_DIR}/codetrail"
RESULTS_DIR="${SCRIPT_DIR}/results"
LOGS_DIR="${SCRIPT_DIR}/logs"
REPOS_DIR="${SCRIPT_DIR}/repos"
REPORT_DIR="${SCRIPT_DIR}/report"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
RESULTS_JSON="${RESULTS_DIR}/results-${TIMESTAMP}.json"
RESULTS_NDJSON="${RESULTS_DIR}/results-${TIMESTAMP}.ndjson"
LOG_FILE="${LOGS_DIR}/test-run-${TIMESTAMP}.log"
RECORDS_PY="${RESULTS_DIR}/_write_record.py"

mkdir -p "${RESULTS_DIR}" "${LOGS_DIR}" "${REPORT_DIR}"

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; NC='\033[0m'

# Python helper to write an NDJSON record
cat > "${RECORDS_PY}" << 'PYEOF'
import json, sys, os
repo = sys.argv[1]
lang = sys.argv[2]
cmd = sys.argv[3]
args = sys.argv[4]
status = sys.argv[5]
exit_code = int(sys.argv[6])
elapsed_ms = int(sys.argv[7])
result_count = int(sys.argv[8])
note = sys.argv[9]
stdout_file = sys.argv[10]
stderr_file = sys.argv[11]
timestamp = sys.argv[12]
ndjson_path = sys.argv[13]

record = {
    "repo": repo, "language": lang, "command": cmd, "args": args,
    "status": status, "exitCode": exit_code, "elapsedMs": elapsed_ms,
    "resultCount": result_count, "note": note,
    "stdout": stdout_file, "stderr": stderr_file, "timestamp": timestamp
}
with open(ndjson_path, 'a') as f:
    f.write(json.dumps(record) + '\n')
PYEOF

now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

run_test() {
    local repo="$1" cmd_name="$2" lang="$3" extra_flags="$4"
    shift 4
    local repo_path="${REPOS_DIR}/${repo}"
    local safe_name="${cmd_name// /_}"
    local stdout_file="${RESULTS_DIR}/${repo}-${safe_name}.stdout"
    local stderr_file="${RESULTS_DIR}/${repo}-${safe_name}.stderr"
    local combined_args="$*"

    local start_ms=$(now_ms)
    local exit_code=0
    "${CODETRAIL}" -p "${repo_path}" --output json ${extra_flags} $combined_args \
        > "${stdout_file}" 2> "${stderr_file}" || exit_code=$?
    local end_ms=$(now_ms)
    local elapsed=$((end_ms - start_ms))

    local status="pass"; local note=""
    if [ $exit_code -ne 0 ]; then
        status="fail"; note="exit_code=${exit_code}"
    elif grep -q '"no_match"' "${stdout_file}" 2>/dev/null; then
        status="no_match"
    fi

    local result_count=0
    [ -s "${stdout_file}" ] && result_count=$(python3 -c "
import json
try:
    d=json.load(open('${stdout_file}'))
    r=d.get('results',d.get('items',[]))
    print(len(r) if isinstance(r,list) else 0)
except: print(0)" 2>/dev/null) || result_count=0

    echo -e "${GREEN}[${status}]${NC} ${repo}/${cmd_name} (${elapsed}ms, ${result_count} results)" | tee -a "${LOG_FILE}"

    python3 "${RECORDS_PY}" "${repo}" "${lang}" "${cmd_name}" "${combined_args}" \
        "${status}" "${exit_code}" "${elapsed}" "${result_count}" "${note}" \
        "${stdout_file}" "${stderr_file}" "${TIMESTAMP}" "${RESULTS_NDJSON}"
}

run_index_build() {
    local repo="$1"
    echo -e "${YELLOW}Building index for ${repo}...${NC}" | tee -a "${LOG_FILE}"
    local start=$(now_ms)
    "${CODETRAIL}" -p "${REPOS_DIR}/${repo}" index build --force \
        > "${RESULTS_DIR}/${repo}-index_build.stdout" 2> "${RESULTS_DIR}/${repo}-index_build.stderr" || true
    echo -e "${GREEN}done: $(( $(now_ms) - start ))ms${NC}" | tee -a "${LOG_FILE}"
}

# ── Repo definitions ─────────────────────────────────────────────────────────
RLANG_go_gin="go";          RLANG_rust_ripgrep="rust";   RLANG_java_junit4="java";   RLANG_ts_express="typescript"
RSEARCH_go_gin="Context";   RSEARCH_rust_ripgrep="RegexBuilder"; RSEARCH_java_junit4="Assert"; RSEARCH_ts_express="Router"
RSYMBOL_go_gin="Engine";    RSYMBOL_rust_ripgrep="RegexBuilder"; RSYMBOL_java_junit4="Test"; RSYMBOL_ts_express="application"
RFILE_go_gin="context.go";  RFILE_rust_ripgrep="lib.rs";  RFILE_java_junit4="Assert.java"; RFILE_ts_express="application.js"
RGLOB_go_gin="**/*.go";     RGLOB_rust_ripgrep="**/*.rs"; RGLOB_java_junit4="**/*.java"; RGLOB_ts_express="**/*.js"
RGREP_go_gin="func ";       RGREP_rust_ripgrep="fn ";   RGREP_java_junit4="class "; RGREP_ts_express="function "

get_lang()   { eval echo \$RLANG_${1//-/_}; }
get_search() { eval echo \$RSEARCH_${1//-/_}; }
get_symbol() { eval echo \$RSYMBOL_${1//-/_}; }
get_file()   { eval echo \$RFILE_${1//-/_}; }
get_glob()   { eval echo \$RGLOB_${1//-/_}; }
get_grep()   { eval echo \$RGREP_${1//-/_}; }

# ── Main ─────────────────────────────────────────────────────────────────────
echo "=== CodeTrail CLI Full Test Suite ===" | tee "${LOG_FILE}"
echo "Started: $(date)" | tee -a "${LOG_FILE}"
echo "" | tee -a "${LOG_FILE}"

> "${RESULTS_NDJSON}"
TOTAL_START=$(now_ms)

for repo in go-gin rust-ripgrep java-junit4 ts-express; do
    lang=$(get_lang "$repo"); sym=$(get_symbol "$repo"); srch=$(get_search "$repo")
    file=$(get_file "$repo"); glob=$(get_glob "$repo"); grep_pat=$(get_grep "$repo")

    echo "" | tee -a "${LOG_FILE}"
    echo "══════ ${repo} (${lang}) ══════" | tee -a "${LOG_FILE}"

    run_index_build "${repo}"

    # Search
    run_test "${repo}" "find"       "${lang}" ""             find "${srch}"
    run_test "${repo}" "find_json"  "${lang}" ""             find --output json "${srch}"
    run_test "${repo}" "grep"       "${lang}" ""             grep "${grep_pat}"
    run_test "${repo}" "files"      "${lang}" ""             files "${file:0:4}"
    run_test "${repo}" "glob"       "${lang}" ""             glob "${glob}"
    run_test "${repo}" "list"       "${lang}" ""             list
    run_test "${repo}" "tree"       "${lang}" ""             tree --depth 2

    # Read
    run_test "${repo}" "read"       "${lang}" ""             read "${file}:1-20"

    # Navigation
    run_test "${repo}" "symbols"    "${lang}" ""             symbols "${sym}"
    run_test "${repo}" "defs"       "${lang}" ""             defs "${sym}"
    run_test "${repo}" "refs"       "${lang}" ""             refs "${sym}"
    run_test "${repo}" "calls"      "${lang}" ""             calls "${sym}"
    run_test "${repo}" "callers"    "${lang}" ""             callers "${sym}"
    run_test "${repo}" "find_path"  "${lang}" ""             find-path "${sym}"

    # Status & changed
    run_test "${repo}" "status"     "${lang}" ""             status
    run_test "${repo}" "changed"    "${lang}" ""             changed

    # Index subcommands
    run_test "${repo}" "index_status"  "${lang}" ""          index status
    run_test "${repo}" "index_verify"  "${lang}" ""          index verify 2>/dev/null || true

    # Query save & replay
    run_test "${repo}" "find_save_query" "${lang}" "--allow-broad" find "${srch}" --save-query "test-${repo}"
    run_test "${repo}" "query_replay"    "${lang}" ""        query replay "test-${repo}" 2>/dev/null || true

    # Hooks & completions (smoke)
    run_test "${repo}" "hooks_install"   "${lang}" ""        hooks install 2>/dev/null || true
    run_test "${repo}" "hooks_uninstall" "${lang}" ""        hooks uninstall 2>/dev/null || true
    run_test "${repo}" "completions"     "${lang}" ""        completions bash 2>/dev/null || true
done

TOTAL_END=$(now_ms); TOTAL_ELAPSED=$((TOTAL_END - TOTAL_START))

echo "" | tee -a "${LOG_FILE}"
echo "══════ Suite complete: ${TOTAL_ELAPSED}ms ($((TOTAL_ELAPSED/1000))s) ══════" | tee -a "${LOG_FILE}"

# Generate summary
python3 << PYEOF | tee -a "${LOG_FILE}"
import json
with open('${RESULTS_NDJSON}') as f:
    lines = [json.loads(l) for l in f if l.strip()]
total = len(lines)
passed = sum(1 for l in lines if l['status'] == 'pass')
failed = sum(1 for l in lines if l['status'] == 'fail')
no_match = sum(1 for l in lines if l['status'] == 'no_match')
total_time = sum(l['elapsedMs'] for l in lines)
summary = {
    'timestamp': '${TIMESTAMP}', 'totalTests': total,
    'passed': passed, 'failed': failed, 'noMatch': no_match,
    'totalElapsedMs': total_time, 'suiteElapsedMs': ${TOTAL_ELAPSED},
    'results': lines
}
with open('${RESULTS_JSON}', 'w') as f:
    json.dump(summary, f, indent=2)
print(f'Summary: {passed} pass, {failed} fail, {no_match} no_match / {total} total')
print(f'Time: {total_time}ms cmd + overhead = {${TOTAL_ELAPSED}}ms suite')
PYEOF

echo "" | tee -a "${LOG_FILE}"
echo "Results: ${RESULTS_JSON}" | tee -a "${LOG_FILE}"
echo "NDJSON:  ${RESULTS_NDJSON}" | tee -a "${LOG_FILE}"
echo "Done." | tee -a "${LOG_FILE}"
