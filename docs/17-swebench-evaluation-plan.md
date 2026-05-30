# SWE-bench 搜索效率对比评估方案

> 评估 code-search-cli 在真实 AI Agent 场景中对搜索效率的提升程度。
> 基线: mini-swe-agent 默认工具 | 实验: mini-swe-agent + code-search-cli MCP tools

## 评估目标

回答核心问题：**code-search-cli 替代 Agent 的原始 grep/find/read 搜索工具后，能否让 Agent 更快、更准地定位代码，从而提高任务解决率？**

| 维度 | 衡量内容 |
|------|----------|
| **解决率** | 相同任务集下，成功解决的 issue 数量变化 |
| **搜索效率** | 定位相关代码所需的搜索调用次数减少比例 |
| **搜索精度** | 每次搜索返回有效结果的命中率 |
| **时间效率** | 完成任务的总时间（wall clock）变化 |
| **可靠性** | 搜索结果的证据标注（reliability label）是否帮助 Agent 做更准的决策 |

---

## 架构概览

```
SWE-bench Tasks (Dockerized)
  ├── 代码仓库 + issue + 参考补丁
  └── 评估 Harness (对比 gold patch)
       │
       ▼
  Agent Runner (mini-swe-agent)
  ├── 基线组: 默认工具 (bash grep/find/ls/cat)
  └── 实验组: code-search-cli MCP tools 替代搜索工具
       │
       ▼
  Metrics
  ├── resolution_rate (解决率)
  ├── avg_search_calls (搜索调用次数)
  ├── avg_search_precision (搜索命中率)
  ├── avg_time_ms (总耗时)
  └── reliability_accuracy (可靠性标签准确率)
```

---

## 方案对比

### 基线组 (Baseline) — mini-swe-agent 默认

Agent 使用传统 shell 工具进行代码搜索：

| 操作 | 工具 | 命令示例 |
|------|------|----------|
| 文本搜索 | `grep` | `grep -rn "pattern" .` |
| 文件查找 | `find` | `find . -name "*.py"` |
| 文件读取 | `cat` / `sed` | `cat file.py` / `sed -n '10,20p' file.py` |
| 符号定位 | `grep` 多次调用 | 多次 grep + 上下文 |

**局限性**:
- 每次搜索扫描全部文件 (O(n) 文件数)
- 无索引加速，大仓库下极慢
- 无可靠性标注，Agent 需手动验证每个结果
- 正则/glob 能力有限（依赖 bash 版本）
- 需多次调用才能精确定位（先 grep 找文件，再 cat 查看，再 grep 上下文）

### 实验组 (Experiment) — code-search-cli MCP tools

Agent 使用 code-search-cli 的 12 个 MCP 工具：

| MCP Tool | 替代 | 优势 |
|----------|------|------|
| `code_search_find` | `grep -rn` | 索引加速 (gram 预过滤)，精确行号 |
| `code_search_grep` | `grep -rnP` | 正则 + 索引预过滤 |
| `code_search_files` | `find -name` | 文件名匹配 |
| `code_search_glob` | `find -path` | glob 模式 |
| `code_search_read` | `cat` / `sed -n` | 行范围读取，totalLines 信息 |
| `code_search_defs` | 多次 grep | tree-sitter/SCIP 精确定义定位 |
| `code_search_refs` | 多次 grep | 标识符引用搜索 |
| `code_search_symbols` | 多次 grep | tree-sitter 符号搜索 |
| `code_search_calls` | 手动推断 | tree-sitter 调用关系 |
| `code_search_callers` | 手动推断 | 反向调用关系 |
| `code_search_changed` | `git diff` | 变更文件列表 |
| `code_search_status` | — | 索引状态/新鲜度 |

**优势**:
- 可靠性标注 (`source_fact` / `parser_fact` / `inferred_candidate`) — Agent 无需二次验证
- 索引加速 (trigram 预过滤) — 大仓库更快
- 结构化输出 (JSON) — Agent 可程序化解析
- 精确行范围 — 减少无效读取

---

## 任务选择

从 SWE-bench Lite (300 个任务) 中选取适合代码搜索评估的任务子集：

### 筛选标准

1. **仓库规模**: 选择 3-5 个不同规模的 Python 仓库
   - 小型 (100-500 文件): `flask`, `astropy`
   - 中型 (500-2000 文件): `sympy`, `django`
   - 大型 (2000+ 文件): `matplotlib`, `sphinx`
2. **任务类型**: 优先选择需要跨文件代码追踪的任务（非单文件修复）
3. **排除**: 配置变更、文档修复、单行 typo 的任务

### 推荐任务集 (50 tasks)

从 SWE-bench Lite 中按仓库分层采样：
- `django__django-*`: 10 个任务 (大型 Django 项目，跨文件追踪)
- `sympy__sympy-*`: 10 个任务 (中型数学库，复杂调用链)
- `sphinx-doc__sphinx-*`: 10 个任务 (文档生成器，模板查找)
- `psf__requests-*`: 10 个任务 (HTTP 库，简洁但需要精确搜索)
- `matplotlib__matplotlib-*`: 10 个任务 (大型可视化库，复杂 import)

---

## 评估指标

### 核心指标

| 指标 | 定义 | 期望趋势 |
|------|------|----------|
| **Resolution Rate** | 成功解决的 issue 数 / 总任务数 | 实验组 ≥ 基线组 |
| **Avg Search Calls** | 每个任务的平均搜索工具调用次数 | 实验组 < 基线组 (更少调用) |
| **Search Precision** | 搜索返回结果中 Agent 后续实际使用的比例 | 实验组 > 基线组 (更精准) |
| **Avg Wall Time** | 每个任务的平均总耗时 (秒) | 实验组 < 基线组 (更快) |
| **First Hit Accuracy** | 第一次搜索就直接定位到目标文件的任务比例 | 实验组 > 基线组 |

### 辅助指标

| 指标 | 定义 |
|------|------|
| **Tool Call Distribution** | 各工具调用次数分布（对比基线 grep/find/cat 频次） |
| **Reliability Label Accuracy** | `precise_fact` / `inferred_candidate` 标签与实际验证一致的比例 |
| **Hallucination Rate** | Agent 基于搜索结果做出错误假设的次数 |
| **Index Usage Rate** | code-search-cli 索引生效 (`index.used=true`) 的查询比例 |

---

## 实施步骤

### Step 1: 环境准备

```bash
# 安装 SWE-bench
cd /Users/mars/dev
git clone https://github.com/swe-bench/SWE-bench.git swe-bench
cd swe-bench && pip install -e .

# 安装 mini-swe-agent (基线 agent)
pip install mini-swe-agent

# 安装 Docker (SWE-bench 使用 Docker 评估)
# macOS: Docker Desktop
# 确保虚拟磁盘空间 ≥ 120GB

# 构建 code-search-cli release 二进制
cd /Users/mars/dev/git-ai-workspace/code-search-cli
cargo build --release
```

### Step 2: 配置 Agent 工具集

**基线工具配置** (`configs/baseline.yaml`):
```yaml
tools:
  - name: bash
    description: "Run bash commands"
  - name: edit
    description: "Edit files"
  - name: submit
    description: "Submit solution"
```

Agent 在 bash 工具中使用 `grep`, `find`, `cat` 等原生命令。

**实验工具配置** (`configs/experiment_code_search.yaml`):
```yaml
tools:
  - name: bash
    description: "Run bash commands (use only for git ops, running code, etc.)"
  - name: edit
    description: "Edit files"
  - name: submit
    description: "Submit solution"
  
  # code-search-cli MCP tools
  - name: mcp__code_search_find
  - name: mcp__code_search_grep  
  - name: mcp__code_search_files
  - name: mcp__code_search_glob
  - name: mcp__code_search_read
  - name: mcp__code_search_defs
  - name: mcp__code_search_refs
  - name: mcp__code_search_symbols
  - name: mcp__code_search_calls
  - name: mcp__code_search_callers
  - name: mcp__code_search_changed
  - name: mcp__code_search_status
```

### Step 3: 任务选择脚本

```python
# select_tasks.py
from datasets import load_dataset

ds = load_dataset("princeton-nlp/SWE-bench_Lite", split="test")

# Filter by repo and task type
target_repos = ["django/django", "sympy/sympy", "sphinx-doc/sphinx", 
                "psf/requests", "matplotlib/matplotlib"]

selected = ds.filter(lambda x: x["repo"] in target_repos)
# Shuffle and take 50
selected = selected.shuffle(seed=42).select(range(50))
selected.save_to_disk("./data/swebench_search_eval_50")
```

### Step 4: 运行评估

```bash
# 基线组
python -m swebench.harness.run_evaluation \
    --dataset_name princeton-nlp/SWE-bench_Lite \
    --predictions_path ./predictions/baseline.jsonl \
    --max_workers 4 \
    --run_id baseline_search_eval \
    --instance_ids_file ./data/selected_50_ids.txt

# 实验组
python -m swebench.harness.run_evaluation \
    --dataset_name princeton-nlp/SWE-bench_Lite \
    --predictions_path ./predictions/experiment.jsonl \
    --max_workers 4 \
    --run_id experiment_code_search \
    --instance_ids_file ./data/selected_50_ids.txt
```

### Step 5: 结果分析

```bash
# 对比报告
python analyze_results.py \
    --baseline ./evaluation_results/baseline_search_eval.json \
    --experiment ./evaluation_results/experiment_code_search.json \
    --output ./reports/search_efficiency_report.md
```

---

## 预期结果与假设

### 假设

1. **搜索调用大幅减少**: code-search-cli 的结构化输出 + 索引让 Agent 用更少调用找到目标。预期搜索调用减少 40-60%。
2. **首次命中率提升**: `defs`/`refs`/`symbols` 精确语义搜索比 iterative grep 更准。预期首次命中率从 ~30% 提升到 ~60%。
3. **解决率小幅提升**: 更好的搜索 = 更好的问题理解 = 更高的修复质量。预期解决率提升 5-15%。
4. **中小仓库差异更大**: 索引对小仓库额外开销抵消加速，中等仓库 (500-2000 文件) 差异最显著。
5. **L2 推断候选被谨慎对待**: Agent 应理解 `inferred_candidate` 标签 = 需要手动验证，减少基于误判的错误。

### 风险

| 风险 | 缓解 |
|------|------|
| Mac Docker 性能差异 | 使用相同硬件，相同 Docker 配置 |
| 索引构建时间计入总时间 | 按 `index build` 独立统计，分离索引成本 |
| MCP 协议开销 | 在指标中单独追踪通信开销 |
| 任务数量不足 | 先用 50 个任务验证方法论，后续扩展到 100+ |

---

## 最小可行评估 (推荐首轮)

如果资源有限，首轮可采用最小方案：

| 参数 | 取值 |
|------|------|
| 任务数 | 20 (每个仓库 4-5 个) |
| Agent | mini-swe-agent + Claude Sonnet |
| 每组次重复 | 1 (首轮快速试跑) |
| 估时 | ~2-4 小时 |

首轮重点验证:
1. code-search-cli MCP 集成是否正常
2. Agent 是否理解并使用新工具
3. 搜索调用次数差异是否显著
4. 是否有任何任务被索引加速解决了原本无法解决的

---

## 文档相关性

- SWE-bench: https://swebench.com/
- Mini-SWE-agent: https://github.com/SWE-agent/mini-SWE-agent
- SWE-bench Verified (500 精选任务): https://huggingface.co/datasets/SWE-bench/SWE-bench_Verified
- code-search-cli MCP: `docs/15-l0-test-plan-ruoyi.md` (MCP tools reference)
- 性能基线: `docs/16-benchmark-plan.md`
