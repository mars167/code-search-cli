# code-search-cli

面向开发者和 LLM Agent 的本地优先代码搜索 CLI。

`code-search` 的核心承诺不是“理解代码”，而是快速给出可验证的代码证据：搜索、路径定位、范围读取、定义、引用、调用候选、索引状态和 MCP 工具输出都围绕可读取的结果、分页和 caveats 组织。

## 文档

更多设计说明：

| 文档 | 内容 |
| --- | --- |
| [`docs/00-design-summary.md`](docs/00-design-summary.md) | 产品定位、文档边界、总览图 |
| [`docs/01-architecture.md`](docs/01-architecture.md) | snapshot、索引、查询、watcher、remote 架构 |
| [`docs/02-command-contract.md`](docs/02-command-contract.md) | 命令族、JSON 响应、可靠性契约 |
| [`docs/03-quality.md`](docs/03-quality.md) | 本地质量门禁、CI 映射、性能看护边界 |

命令参数以 `code-search --help` 和 `src/cli.rs` 为准；实现细节以 `src/`、`tests/` 和 `scripts/` 为准。

## Codex Skill

本仓库包含一个给 Codex/LLM Agent 使用的 skill：

```text
skills/code-search-cli/
```

它说明了 agent 应如何用 `code-search` 获取可验证的源码证据、处理 reliability 分级、重放 saved query、检查 index freshness，并验证 MCP/JSON 契约。需要随项目使用时，可以把该目录复制到本机 Codex skills 目录：

```bash
cp -R skills/code-search-cli "${CODEX_HOME:-$HOME/.codex}/skills/"
```

## 快速使用

```bash
cargo build
cargo test

cargo run -- find "Workspace"
cargo run -- grep "fn .*status"
cargo run -- read src/main.rs:1-40
cargo run -- defs Workspace
cargo run -- find "Workspace" --save-query workspace-find
cargo run -- query replay workspace-find
cargo run -- index build
cargo run -- index status
cargo run -- mcp
```

默认输出是短文本；需要机器读取时使用 `--output json` 或 `--output jsonl`。公开 JSON 只包含 `results`、`page` 和 `caveats`；修改代码前用 `read` 验证搜索、remote 或图候选结果。

## 当前实现

- CLI 命令面由 `clap` 定义，默认 `text`，支持 `json`、`compact-json`、`jsonl` 与 `text` 输出。
- L0 源码事实命令覆盖内容搜索、路径搜索、目录浏览、范围读取、git changed/status。
- 全局 scope 参数包括 `--include`、`--exclude`、`--lang`、`--changed`、`--cursor`、`--allow-broad`、`--limit`、`--context` 和 `--save-query`。
- `--save-query` 将可重放查询保存到 `.code-search/queries/`；`query replay/show/list/delete` 管理 saved query。snapshot 不匹配时，默认按当前 workspace 重放并给 caveat；`--snapshot saved` 会拒绝不匹配的重放。
- `index build` 使用 LanceDB 作为主要本地索引存储，保存 snapshot、file catalog、file proof 和 gram postings，并保留 manifest 供 pack/unpack 兼容。dirty worktree 查询会对仍 fresh 的文件使用索引，对变更文件使用 live overlay。
- `index pack/unpack` 支持 remote snapshot；remote 结果必须标记 `remote_verified` 或 `remote_unverified`，关键结果仍需 `read` 验证。
- `defs`、`refs`、`symbols` 优先使用 SCIP occurrence store；没有 precise index 时回退到 tree-sitter 或文本搜索。parser fallback 和候选关系通过 caveats 标出，调用方用结果里的 `path`/`range` 再执行 `read`。
- `calls`、`callers` 通过当前 petgraph 后端返回调用候选，可靠性始终是 `inferred_candidate`。
- `watch --once` 提供按需 reconcile；`serve` 暴露本地 query service 状态；`mcp` 通过 stdio JSON-RPC 包装同一套查询能力，并输出同一 public JSON 投影。
- `scripts/quality-gate.sh` 是本地与 CI 的统一质量入口。
