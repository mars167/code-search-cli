# 自动化质量看护与测试架构

> 当前设计准绳见 `docs/00-design-summary.md`。本文定义 `code-search-cli` 的测试金字塔、
> 质量门禁和自动化看护入口，目标是让每次改动都能被本地脚本和后续 CI 用同一套规则验证。

## 目标

`code-search-cli` 的质量看护不只检查 Rust 单元测试是否通过，还必须持续保护 Agent 依赖的行为：

- 命令输出 schema 稳定，字段含义不漂移。
- `source_fact`、`parser_fact`、`precise_fact`、`inferred_candidate` 等可靠性标签诚实。
- 索引只能作为 freshness 通过后的缓存，不能替代源码事实。
- Hook、watch、serve、SCIP、graph 等分层边界不被后续 MR 混淆。
- 性能、真实仓库端到端行为和 SWE-bench agent 搜索效率有自动化趋势记录。

## 测试金字塔

| 层级 | 范围 | 主要入口 | 运行频率 | 阻断策略 |
| --- | --- | --- | --- | --- |
| L0 Static Guard | 格式、编译、工作区基础卫生 | `cargo fmt --check`、`cargo check`、`git diff --check` | 每次提交前、每个 PR | 阻断 |
| L1 Unit | 纯函数、解析器、索引 manifest、freshness、路径匹配 | `cargo test` 中的模块级测试 | 每次提交前、每个 PR | 阻断 |
| L2 Contract | JSON schema、reliability、fallback、exit code | `tests/cli.rs`、snapshot/fixture 测试 | 每个 PR | 阻断 |
| L3 CLI Integration | 临时 git repo 内的真实二进制行为 | `cargo test --test cli` | 每个 PR | 阻断 |
| L4 Fixture E2E | RuoYi 等真实仓库的命令覆盖 | `scripts/quality-gate.sh cli` | PR 扩展检查、nightly、release | PR 可选阻断；release 阻断 |
| L5 Performance Regression | 启动、搜索、索引构建、内存 | `scripts/bench.sh compare` | nightly、release、性能相关 PR | 阈值阻断 |
| L6 Agent Evaluation | SWE-bench 搜索效率与解决率 | `docs/17-swebench-evaluation-plan.md` 对应 harness | weekly、release candidate | 趋势看护；release 人工判定 |

这个结构故意把高频、低成本、确定性强的检查放在底部，把耗时、波动和外部依赖较多的 agent 评估放在顶部。
默认 PR 不应该被 SWE-bench 阻断；它负责发现长期产品效果退化。

## 质量信号

| 信号 | 说明 | 失败处理 |
| --- | --- | --- |
| Schema drift | 命令响应缺字段、字段重命名、字段类型变化 | 阻断；必须更新 contract test 和文档 |
| Reliability drift | 候选结果被错误标成 exact，或 precise/parser/source 边界混淆 | 阻断；必须补反例测试 |
| Freshness bypass | stale index 被继续使用，或无法证明 freshness 时没有 fallback | 阻断；必须补 stale fixture |
| Snapshot confusion | HEAD、staged、worktree 结果混用 | 阻断；必须补 git fixture |
| Performance regression | 指标超过 `docs/16-benchmark-plan.md` 阈值 | nightly 阻断；PR 视触发范围决定 |
| Agent search regression | SWE-bench 搜索调用、token、gold file 命中变差 | 不自动阻断；进入 release 风险列表 |

## 自动化入口

统一入口是 `scripts/quality-gate.sh`。本地、agent 和 CI 都应该调用这个脚本，而不是各自拼命令。

```bash
scripts/quality-gate.sh quick
scripts/quality-gate.sh cli
scripts/quality-gate.sh bench
scripts/quality-gate.sh full
```

### quick

面向开发循环和 PR 默认检查：

- `cargo fmt --check`
- `cargo check`
- `cargo test`
- `git diff --check`

### cli

面向命令契约和真实仓库 smoke：

- `cargo build --release`
- `cargo test --test cli`
- 如果 `TEST_REPO` 或默认 RuoYi 仓库存在，则运行 L0 命令 smoke。

RuoYi smoke 只验证关键命令和输出契约，不替代 `docs/15-l0-test-plan-ruoyi.md` 的完整覆盖。

### bench

面向性能回归：

- 构建 release binary。
- 调用 `scripts/bench.sh compare`。
- 依赖 `hyperfine`、`jq`、`bc` 和已保存的 `scripts/baseline_values/`。

### full

面向合并前或 release candidate：

- 顺序执行 `quick`、`cli`、`bench`。
- 不默认运行 SWE-bench，因为它需要 Docker、模型调用和较长时间。

## CI 映射

Gitea Actions 配置位于 `.gitea/workflows/quality-gate.yml`。CI 不重新定义测试规则，只调度统一脚本：

| Job | 命令 | 触发 |
| --- | --- | --- |
| `quick` | `scripts/quality-gate.sh quick` | push、pull request |
| `cli` | `scripts/quality-gate.sh cli` | push、pull request，依赖 `quick` |
| `bench` | `scripts/quality-gate.sh bench` | 手动触发并设置 `run_bench=true` |
| `agent-eval` | SWE-bench harness | 后续 weekly、release candidate、手动 |

默认 CI 只阻断 `quick` 和 `cli`。`bench` 当前保留为手动触发，因为它依赖性能基线、机器负载和 fixture 仓库；
一旦基线稳定，可以改为 nightly 或 release 阻断。

## 产物约定

| 产物 | 位置 | 用途 |
| --- | --- | --- |
| CLI/E2E 报告 | `docs/reports/` | 人工查看和 release 归档 |
| 性能基线 | `scripts/baseline_values/` | `bench compare` 对比 |
| 性能原始结果 | `scripts/bench_results/` | 趋势排查 |
| SWE-bench 数据与报告 | `reports/swebench/` 或后续专用目录 | 搜索效率趋势 |

质量看护脚本只负责 pass/fail 和关键摘要；长报告由对应专项脚本生成。

## 分层扩展规则

新增功能时按风险选择测试层：

- 新增 CLI 参数或命令：必须有 L2/L3 contract test。
- 修改 JSON 输出：必须有 schema/reliability 断言，并同步命令文档。
- 修改索引或 freshness：必须有 stale、missing file、hash mismatch 测试。
- 修改 parser、SCIP 或 graph：必须验证 exact/candidate 边界。
- 修改性能关键路径：必须更新或运行 benchmark compare。
- 修改 Agent 工具面或 MCP：必须补 SWE-bench 搜索效率采样计划或结果。

## 当前落地边界

本轮落地 `docs/18-quality-guard-test-architecture.md` 和 `scripts/quality-gate.sh`。
SWE-bench harness、GitHub Actions、覆盖率上传和长期趋势看板不在本轮实现范围内。
