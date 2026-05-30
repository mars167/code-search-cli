# 性能基准测试方案

> 发布前运行本方案，与基线对比检查性能是否裂化（regression）。
> 基线采集时间: 2026-05-29 | 硬件: Mac (Apple Silicon) | 测试仓库: RuoYi (285 Java, 38K 行)

## 度量指标

| 指标 | 含义 | 工具 | 阈值策略 |
|------|------|------|----------|
| **Time (mean)** | 平均执行时间 | hyperfine | 较基线升高 >15% 为 P2 退化，>30% 为 P1 退化 |
| **Time (stddev)** | 标准差/抖动 | hyperfine | > 均值的 20% 为不稳定信号 |
| **User time** | CPU 用户态时间 | hyperfine | 辅助分析瓶颈来源 |
| **System time** | CPU 内核态时间 | hyperfine | 追踪 I/O 退化 |
| **Max RSS** | 最大物理内存 | /usr/bin/time -l | > 基线 1.5x 为内存泄漏 |
| **Exit code** | 命令退出状态 | hyperfine | 变化为回归 |

## 测试仓库选择

| 级别 | 仓库 | 文件数 | 用途 |
|------|------|--------|------|
| 小 | RuoYi | 285 Java | 日常回归 (快速) |
| 中 | (待选) | ~5,000 文件 | 扩展规模 |
| 大 | (待选) | ~20,000 文件 | 压力测试 |

默认使用 RuoYi 作为 CI 回归基准（2 分钟内可完成全部测试）。

---

## 基线 (2026-05-29)

### L0 源码事实命令 (无索引，冷启动)

| # | 命令 | 均值 | σ | 用户态 | 内核态 | 最大 RSS |
|---|------|------|---|--------|--------|----------|
| 1 | `--help` | 14ms | 2ms | 6ms | 5ms | — |
| 2 | `completions bash` | 17ms | 4ms | 6ms | 6ms | — |
| 3 | `status` | 223ms | 7ms | 80ms | 125ms | — |
| 4 | `list <dir>` | 211ms | 8ms | 75ms | 118ms | — |
| 5 | `read <file>` | 212ms | 14ms | 75ms | 118ms | — |
| 6 | `changed` | 338ms | 16ms | 113ms | 199ms | — |
| 7 | `files <pattern>` | 379ms | 9ms | 113ms | 247ms | — |
| 8 | `find-path <pat>` | 388ms | 18ms | 116ms | 253ms | — |
| 9 | `glob <pattern>` | 429ms | 39ms | 125ms | 277ms | — |
| 10 | `grep <regex>` | 434ms | 17ms | 126ms | 289ms | 9.6MB |
| 11 | `find <text>` | 461ms | 54ms | 126ms | 314ms | 6.7MB |
| 12 | `refs <id>` | 486ms | 18ms | 136ms | 330ms | — |

### L1/L2 解析器命令

| # | 命令 | 均值 | σ | 用户态 | 内核态 | 备注 |
|---|------|------|---|--------|--------|------|
| 13 | `defs <id>` | 5.2s | 55ms | 4.5s | 617ms | tree-sitter 全扫描 |
| 14 | `symbols <q>` | — | — | — | — | exit code 2 (bug) |
| 15 | `calls <id>` | 10.9s | 66ms | 9.7s | 1.1s | tree-sitter 全扫描 |
| 16 | `callers <id>` | 10.9s | 73ms | 9.7s | 1.1s | tree-sitter 全扫描 |

### 索引命令

| # | 命令 | 均值 | σ | 用户态 | 内核态 | 最大 RSS |
|---|------|------|---|--------|--------|----------|
| 17 | `index build` (cold) | 17.5s | 541ms | 3.0s | 14.3s | 44MB |
| 18 | `index build` (warm) | 17.2s | 322ms | 3.0s | 14.1s | 44MB |

### 索引加速命令 (index-backed)

| # | 命令 | 均值 | σ | 与无索引对比 |
|---|------|------|---|-------------|
| 19 | `find` (indexed) | 1.46s | 27ms | ⚠️ 比无索引慢 3x |

---

## 性能分析摘要

### 瓶颈识别

1. **全文件扫描主导延迟**: 所有 L0 命令 200-500ms，系统时间 (I/O) 占 60%+。285 个文件需遍历全部。
2. **Tree-sitter 解析未利用索引**: `defs`/`calls`/`callers` 扫描全部 285 个文件用 tree-sitter 解析，5-11s。
3. **索引查询反而更慢**: `find` 有索引时 1.46s vs 无索引 461ms。freshness 校验 + blake3 hash 计算导致额外开销。
4. **索引构建 I/O 密集**: 17s 中 14s 在内核态（文件读写 + blake3 hash），可优化。
5. **symbols 命令 bug**: exit code 2，需修复后才能纳入基准。

### 优化方向

- **预扫描文件列表缓存**: 避免每次命令都 walk 全部文件树
- **增量索引**: 当前均为全量重建，17s 不变
- **Lazy tree-sitter**: 按需解析而非全量
- **索引 freshness 快速路径**: 对未修改项目跳过 blake3 全量校验

---

## 可复用基准采集脚本

保存为 `scripts/bench.sh`：

```bash
#!/bin/bash
# code-search-cli 性能基准采集和对比脚本
# 用法:
#   ./scripts/bench.sh collect  # 采集当前基线
#   ./scripts/bench.sh compare  # 与保存的基线对比

set -euo pipefail

CS="${CS_BIN:-./target/release/code-search}"
REPO="${TEST_REPO:-/Users/mars/dev/git-ai-workspace/RuoYi}"
BASELINE_FILE="./scripts/baseline.json"
RESULTS_DIR="./scripts/bench_results"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)

mkdir -p "$RESULTS_DIR"

# ====== 测试套件定义 ======
# 格式: "group|name|command|warmup|min_runs|options"
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
  "index|index-build-cold|index build|0|3|--prepare 'rm -rf $REPO/.code-search 2>/dev/null; true'"
  "index|index-build-warm|index build|1|5|"
  "index|find-indexed|find RuoYiApplication|2|5|"
)

collect() {
  local tmpfile="$RESULTS_DIR/raw_${TIMESTAMP}.json"
  echo "[]" > "$tmpfile"

  local current_group=""
  for entry in "${TESTS[@]}"; do
    IFS='|' read -r group name cmd warmup runs extra_opts <<< "$entry"
    [[ "$group" != "$current_group" ]] && { current_group="$group"; echo ""; echo "=== [$group] ==="; }

    local full_cmd="$CS --path $REPO $cmd"
    local label="${group}/${name}"
    echo -n "  $label ... "

    local output
    if output=$(hyperfine --warmup "$warmup" --min-runs "$runs" \
         --export-json /dev/stdout \
         $extra_opts "$full_cmd" 2>/dev/null); then
      echo "$output" | jq --arg group "$group" --arg name "$name" \
        '{group: $group, name: $name, timestamp: $ts, results: .results[0]}' \
        --arg ts "$TIMESTAMP" >> "$tmpfile"
      local mean
      mean=$(echo "$output" | jq -r '.results[0].mean')
      echo "$(printf '%.0f' "$mean")ms"
    else
      echo "FAILED"
    fi
  done

  # 采集内存基线
  echo ""; echo "=== [memory] ==="
  local memfile="$RESULTS_DIR/mem_${TIMESTAMP}.json"
  echo "{}" > "$memfile"
  for cmd_label in "find:RuoyiApplication" "grep:selectUserBy" "index-build:"; do
    IFS=':' read -r cmd args <<< "$cmd_label"
    echo -n "  $cmd ... "
    local mem
    mem=$(/usr/bin/time -l $CS --path $REPO $cmd "$args" 2>&1 | \
          grep "maximum resident" | awk '{print $1}' | sed 's/^0*//')
    echo "$mem bytes ($(echo "scale=1; $mem/1048576" | bc)MB)"
    jq --arg cmd "$cmd" --arg mem "$mem" '. + {($cmd): $mem}' "$memfile" > "${memfile}.tmp"
    mv "${memfile}.tmp" "$memfile"
  done

  # 合并结果
  jq -s 'add' "$tmpfile" "$memfile" > "$BASELINE_FILE"
  cp "$BASELINE_FILE" "$RESULTS_DIR/baseline_${TIMESTAMP}.json"
  echo ""
  echo "Baseline saved to: $BASELINE_FILE"
}

compare() {
  if [[ ! -f "$BASELINE_FILE" ]]; then
    echo "No baseline found. Run 'collect' first."; exit 1
  fi

  echo "=== Performance Diff vs Baseline ==="
  echo ""

  local pass=0 fail=0 warn=0
  for entry in "${TESTS[@]}"; do
    IFS='|' read -r group name cmd warmup runs extra_opts <<< "$entry"
    local label="${group}/${name}"
    local baseline_mean
    baseline_mean=$(jq -r ".[] | select(.name==\"$name\") | .results.mean" "$BASELINE_FILE" 2>/dev/null)

    [[ -z "$baseline_mean" || "$baseline_mean" == "null" ]] && continue

    local full_cmd="$CS --path $REPO $cmd"
    local current
    current=$(hyperfine --warmup "$warmup" --min-runs 3 \
        --export-json /dev/stdout $extra_opts "$full_cmd" 2>/dev/null | \
        jq -r '.results[0].mean')
    [[ -z "$current" || "$current" == "null" ]] && { echo "  ❌ $label — failed to measure"; fail=$((fail+1)); continue; }

    local diff_pct
    diff_pct=$(echo "scale=1; ($current - $baseline_mean) / $baseline_mean * 100" | bc)

    local status="✅"
    if (( $(echo "$diff_pct > 30" | bc -l) )); then
      status="🔴"; fail=$((fail+1))
    elif (( $(echo "$diff_pct > 15" | bc -l) )); then
      status="🟡"; warn=$((warn+1))
    elif (( $(echo "$diff_pct < -30" | bc -l) )); then
      status="🟢"; pass=$((pass+1))
    else
      pass=$((pass+1))
    fi

    printf "  %s %-30s baseline=%7.0fms  current=%7.0fms  diff=%+5.1f%%\n" \
      "$status" "$label" "$baseline_mean" "$current" "$diff_pct"
  done

  echo ""
  echo "=== Summary: $pass passed, $warn warnings, $fail regressions ==="
  [[ $fail -gt 0 ]] && exit 1 || exit 0
}

# ====== 内存对比 ======
compare_memory() {
  echo ""; echo "=== Memory Diff vs Baseline ==="
  for cmd in "find" "grep" "index-build"; do
    local baseline mem diff_pct
    baseline=$(jq -r ".$cmd // 0" "$BASELINE_FILE" 2>/dev/null)
    [[ "$baseline" == "0" ]] && continue
    mem=$(/usr/bin/time -l $CS --path $REPO $cmd "${args:-test}" 2>&1 | \
          grep "maximum resident" | awk '{print $1}' | sed 's/^0*//')
    diff_pct=$(echo "scale=1; ($mem - $baseline) / $baseline * 100" | bc)
    printf "  %-15s baseline=%7.0fKB  current=%7.0fKB  diff=%+5.1f%%\n" \
      "$cmd" "$baseline" "$mem" "$diff_pct"
  done
}

# ====== Main ======
case "${1:-collect}" in
  collect)  collect ;;
  compare)  compare; compare_memory ;;
  *)        echo "Usage: $0 {collect|compare}"; exit 1 ;;
esac
```

---

## 发布前检查清单

```bash
# 1. 编译 release
cargo build --release

# 2. 采集基线（首次）或对比（后续）
./scripts/bench.sh collect   # 首次：建立基线
./scripts/bench.sh compare   # 后续：与基线对比

# 3. 检查退化阈值
#   ✅ 绿色: 变化 <15% 或 改善 >30%
#   🟡 黄色: 变化 15-30%，需评估但可发布
#   🔴 红色: 变化 >30%，必须修复再发布

# 4. 检查 exit code 变化
#   exit code != 基线 exit code → 回归

# 5. 检查内存
#   max RSS > 基线 1.5x → 内存泄漏
```

## 退化处理策略

| 级别 | 条件 | 措施 |
|------|------|------|
| P0 | `--help` > 50ms 或 崩溃 | 阻塞发布 |
| P1 | 任何命令退化 >30% | 修复后发布 |
| P2 | 命令退化 15-30% | 评估后决定 |
| P3 | 内存 >1.5x 基线 | 排查泄漏 |
| INFO | 改善 >30% | 更新基线 |
