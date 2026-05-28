# code-search-cli

面向人类开发者和 LLM Agent 的本地优先代码搜索与跳转 CLI。

设计方案先看 [`docs/00-design-summary.md`](docs/00-design-summary.md)。其他文档是展开材料。
竞品与定位分析报告见 [`HTML`](docs/12-competitive-analysis-report.html) / [`PDF`](docs/12-competitive-analysis-report.pdf)。

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
code-search hooks install
code-search hooks status
```

设计讨论见 `docs/`。
