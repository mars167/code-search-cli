# 设计方案汇总

> 当前设计准绳。其他 `docs/*.md` 是展开材料，不是优先阅读入口。

## 一句话定位

`code-search` 是给 Agent 用的 local-first、git-first 代码搜索与跳转工具，让 Agent 像 IDE 一样完成搜索、跳转、读取、引用和影响分析。

## 核心原则

- Local first：本地源码、本地索引、本地查询默认可用。
- Git first：所有结果绑定 `commit` / `staged` / `worktree` snapshot。
- Remote 可用：远程只做共享和加速，不能替代本地验证。
- 高效准确：能精确证明的结果必须精确；不能证明的结果必须标为候选。
- 一个工具覆盖 Agent 常用搜索：`grep`、`find-path`、`glob`、`list`、`tree`、`read`、`defs`、`refs`。

## 目标架构

```text
Source Snapshot
  -> Text Search Index
  -> SCIP / Symbol Occurrence Index
  -> Parser Facts
  -> Code Property Graph
  -> Query Service
  -> CLI / MCP / Optional Remote
```

所有索引都是 snapshot 的派生视图，不是事实源。事实源是本地源码、git object、staged blob、file hash 和 range。

## 命令面

- 内容搜索：`find`、`grep`
- 路径搜索：`files`、`find-path`、`findpath`、`glob`
- 浏览读取：`list`、`ls`、`tree`、`read`
- Git 视角：`changed`、`status`
- IDE 跳转：`defs`、`refs`、`symbols`
- 图候选：`calls`、`callers`
- 实时更新：`watch`、`serve`
- 索引：`index build/update/status/verify/clean`
- Hook：`hooks install/uninstall/status`

兼容命令只是入口名不同，输出 schema 必须统一。`grep` 不直接等于系统 grep，`find-path` 不直接等于 shell find。

## 准确性分级

- L0：源码事实，精确。
- L1P：SCIP/语言服务/编译器索引，IDE 级精确。
- L1S：tree-sitter parser fact，确定但不是语义精确。
- L2：调用链、框架桥接、启发式关系，只能是候选。

`exact=true` 只能用于 L0 或 L1P，并且必须通过 snapshot/file hash/range 验证。

## 索引策略

- 保留 git hook 自动创建和更新索引。
- 索引不是事实源，必须能用 snapshot、file hash、range 验证。
- `pre-commit` 处理 staged snapshot。
- `post-commit/post-checkout/post-merge/post-rewrite` 维护 commit snapshot 与索引一致性。
- watcher 只维护 worktree overlay，不替代 git hook。

## Watcher 策略

- `code-search watch` 启动本地文件 watcher。
- `code-search serve` 启动 query service，并默认包含 watcher。
- watcher 只更新 `worktree` overlay。
- watcher 事件进入统一 `IndexScheduler`。
- 事件丢失、overflow 或 rename 不完整时，标记 overlay stale 并触发 reconcile scan。
- watcher 不执行 `git add`，不改 staged，不维护 commit snapshot。

## 竞品借鉴

完整竞品与定位分析报告见 [`HTML`](12-competitive-analysis-report.html) / [`PDF`](12-competitive-analysis-report.pdf)。

- GitHub Code Search：学 Rust code-specific text index。
- Sourcegraph：学 text search 和 precise navigation 分层。
- CodeGraphContext：学 pluggable graph backend。
- GitNexus：学 DAG ingestion 和 Agent ops。
- CodeGraph：学 local-first graph、watcher、provenance。
- Glean：学 typed facts/schema。

## 不做什么

- 不默认做 embedding/semantic similarity。
- 不把 tree-sitter 调用链伪装成准确调用图。
- 不让 remote 覆盖本地 dirty/staged 状态。
- 不让 watcher 替代 git hook。
- 不让 Agent 在多套 shell 工具输出之间来回猜。

## 质量看护

测试架构见 `docs/18-quality-guard-test-architecture.md`。质量门禁按测试金字塔分层：

- PR 默认阻断：格式、编译、单元测试、CLI contract、git diff whitespace。
- 扩展阻断：真实仓库 L0 smoke、性能基准回归。
- 趋势看护：SWE-bench agent 搜索效率和解决率。

统一本地入口是 `scripts/quality-gate.sh quick|cli|bench|full`。
