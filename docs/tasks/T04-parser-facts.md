# T4 — 解析器事实

| 字段 | 值 |
|------|-----|
| 状态 | ✅ 已完成 |
| 来源 | [13-implementation-tasks.md](../13-implementation-tasks.md) |
| 相关文档 | [02-reliability-model.md](../02-reliability-model.md)、[04-rust-architecture.md](../04-rust-architecture.md) |

## 概述

`symbols` 和 `defs` 使用 tree-sitter 作为 Rust、Python、Java、TypeScript 和 JavaScript 的回退方案。

## 上下文

T1–T5 已实现、测试、提交、推送，且符合目标架构。T4 通过 tree-sitter 为五种主要语言提供符号和定义查找能力。当没有精确索引（SCIP 等）可用时，tree-sitter 回退始终将结果标记为 parser fact，且不声明 `exact`，确保 LLM Agent 知晓需要验证这些结果。
