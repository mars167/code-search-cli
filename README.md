# code-search-cli

面向人类开发者和 LLM Agent 的本地优先代码搜索与跳转 CLI。

设计方案先看 [`docs/00-design-summary.md`](docs/00-design-summary.md)。其他文档是展开材料。
竞品与定位分析报告见 [`HTML`](docs/12-competitive-analysis-report.html) / [`PDF`](docs/12-competitive-analysis-report.pdf)。
实现任务拆解见 [`docs/13-implementation-tasks.md`](docs/13-implementation-tasks.md)。
Agent team / MR 执行计划见 [`docs/14-agent-team-mr-plan.md`](docs/14-agent-team-mr-plan.md)。
质量看护与测试架构见 [`docs/18-quality-guard-test-architecture.md`](docs/18-quality-guard-test-architecture.md)。
Gitea Actions 门禁配置见 [`.gitea/workflows/quality-gate.yml`](.gitea/workflows/quality-gate.yml)。

这个项目是对旧 `git-ai` 方向的 Rust 重新设计。它不是语义代码理解引擎，
而是一个让 Agent 像使用 IDE 一样高效获取代码信息的工具：搜索、跳转、读取、引用、影响分析都必须返回可验证证据。

## 产品方向

- Local first：默认在本机完成索引、搜索和跳转，不依赖远程服务。
- Git first：所有结果绑定 commit、staged 或 worktree snapshot。
- Remote 可用：远程 graph/index 服务可以作为共享和加速层，但不能替代本地验证。
- 高效准确：Agent 应像使用 IDE search/jump 一样获取信息；能精确证明的结果必须精确，不能精确证明的结果必须降级为候选。
- 证据优先：每个结果都必须指向文件、行号、列号、producer 和可靠性级别。
- 解析器感知：允许使用 tree-sitter 做声明和 symbol fallback。
- Hook 索引保留：保留基于 git hook 的索引创建、存储和更新流程，用作可验证缓存。
- 诚实推断：调用关系和关系类命令是 best-effort，必须提示 LLM 在推理前验证候选结果。

## 命令形态

```bash
code-search find <text>
code-search grep <pattern>
code-search files <pattern>
code-search find-path <pattern>
code-search glob <pattern>
code-search list <dir>
code-search tree <dir>
code-search read <file[:range]>
code-search refs <identifier>
code-search symbols <query>
code-search defs <identifier>
code-search calls <identifier>
code-search callers <identifier>
code-search changed
code-search status
code-search watch
code-search serve
code-search index build
code-search index update
code-search index status
code-search index import-scip <index.scip.json>
code-search hooks install
code-search hooks status
```

## 当前实现状态

当前 CLI 已经提供可运行的本地命令面，并已落地目标 text index 层的第一部分：`index build`
写入 `.code-search/snapshots/`、`.code-search/text/<snapshot>/{docs.idx,paths.idx,grams.idx}`、
`.code-search/working/` 和 `.code-search/staged/`，`find`/`grep` 会在 freshness 校验通过后使用
`grams.idx` 做候选预过滤。完整设计验收仍以 `snapshots/files.parquet + blobs/`、
`text/*.idx`、`scip/index.scip + occurrences.db` 和 `graph/kuzu/` 为准；JSON/JSONL 只能作为导出、
测试 fixture 或显式兼容导入格式，不能作为主存储，也不能据此把索引任务标记为完成。

- L0 源码事实：`find`、`grep`、`refs`、`files`、`find-path`、`glob`、`list`、`tree`、`read`、`changed`、`status`。
- Index/Hook 生命周期：命令入口已存在；target text `.idx` 层已开始落地，source snapshot Parquet、SCIP DB 和 Kuzu graph 尚未完成。
- Watch/Serve 状态：`watch --once`、`watch --status`、`serve --no-watch` 输出 freshness/status 契约。
- Precise + parser fallback：`symbols`、`defs`、`refs` 的 CLI 行为已存在；SCIP JSON 兼容导入写入二进制 `occurrences.idx`，native `index.scip` protobuf 与 occurrence DB 尚未完成。
- 关系候选：`calls`、`callers` 行为已存在；KuzuDB property graph backend 尚未完成。

默认输出为 JSON。所有结果都会携带 `snapshot_id`、`reliability`、`producer`、`exact` 或候选说明。

设计讨论见 `docs/`。
