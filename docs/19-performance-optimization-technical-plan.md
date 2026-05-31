# 性能优化技术方案

> 基于 `scripts/baseline_values/` 当前性能基线和源码热点分析。本文定义后续性能优化的目标架构、
> 分阶段改造方案和验收指标，目标是在不牺牲可靠性分级与 freshness 证明的前提下，让
> `code-search` 在真实 Agent 搜索场景中稳定变快。

## 背景

当前性能基线暴露出三个结构性问题：

1. 路径类和状态类命令也在做全仓内容读取与 hash。
2. 索引查询仍然做接近全量的 freshness 校验，导致 indexed search 没有体现优势。
3. parser/关系类命令每次全仓 AST 解析，未利用索引预过滤和 parser facts 缓存。

当前关键基线如下：

| 命令组 | 命令 | 当前均值 | 主要瓶颈 |
| --- | --- | ---: | --- |
| 启动 | `--help` | 17ms | 基本健康 |
| 启动 | `completions bash` | 20ms | 基本健康 |
| 路径/状态 | `status` | 301ms | Workspace::discover 中的 git status --porcelain 开销 |
| 路径/状态 | `list` | 302ms | Workspace::discover + read_dir（不涉及内容扫描） |
| 路径/状态 | `read` | 302ms | Workspace::discover + 目标文件读取与 hash（单文件，不涉及全仓） |
| 路径/状态 | `changed` | 449ms | 重复 git status 调用（Workspace::discover 中一次，search::changed 中又一次） |
| 路径/状态 | `files` | 454ms | scan_files 全仓内容读取与 hash |
| 路径/状态 | `find-path` | 426ms | scan_files 全仓内容读取与 hash |
| 路径/状态 | `glob` | 449ms | scan_files 全仓内容读取与 hash |
| 内容搜索 | `grep` | 543ms | live scan 全量读取 |
| 内容搜索 | `find` | 1859ms | live scan 全量读取与候选过滤不足 |
| 引用搜索 | `refs` | 1868ms | 文本扫描与 fallback 成本 |
| Parser | `defs` | 6848ms | 全仓 tree-sitter 解析 |
| Parser | `symbols` | 6983ms | 全仓 tree-sitter 解析 |
| 关系候选 | `callers` | 8728ms | 全仓 AST 调用采集 |
| 关系候选 | `calls` | 10989ms | 全仓 AST 调用采集 |
| 索引 | `index build` cold | 13002ms | 多次读文件、全量写索引 |
| 索引 | `index build` warm | 22374ms | 没有真正增量，warm 比 cold 更慢 |
| 索引查询 | indexed `find` | 1722ms | freshness 全量 hash + gram 顺序扫描 |

## 目标

- 路径类与状态类命令不读取完整文件内容，默认只走 metadata/catalog。
- indexed search 在索引 fresh 时显著快于 live scan。
- warm index update 在无变更时接近 no-op。
- parser/关系命令先预过滤候选文件，再解析；可缓存的 parser facts 必须缓存。
- 保持 `docs/00-design-summary.md` 中的可靠性原则：索引是可验证缓存，不是真相源。

## 非目标

- 不把 freshness 校验整体关闭。
- 不把 L2 调用关系伪装成精确结果。
- 不用 remote index 替代本地 dirty/staged/worktree 判断。
- 不在本轮引入 embedding 或语义召回。

## 瓶颈定位

| 模块 | 当前行为 | 性能影响 |
| --- | --- | --- |
| `src/workspace.rs` | `scan_files` 读取文件判断二进制，再读取完整内容计算 `blake3` | 所有依赖 scan 的命令被迫全仓 I/O |
| `src/search.rs` | `find`/`grep` fallback 会读取候选文件全文 | live scan 成本随仓库规模线性增长 |
| `src/index.rs` | freshness 对索引记录重新读文件并计算 hash | indexed query 仍接近全仓校验 |
| `src/text_index.rs` | `candidate_ids` 顺序扫描 `grams.idx` | 索引越大，单次查询越慢 |
| `src/index.rs` | `index build` 全量重建 snapshot/text index | warm build 无法利用未变更文件 |
| `src/syntax.rs` | `defs`/`symbols`/`calls`/`callers` 全仓解析 AST | parser 命令达到 7-11 秒级 |

## 总体架构

性能优化后，查询路径拆成四层：

```text
Workspace Catalog
  -> Source Proof Store
  -> Text / Path Index
  -> Parser Facts Cache
  -> Query Commands
```

- `Workspace Catalog`：只保存路径和文件 metadata，用于 `files`、`find-path`、`glob`、`list`、`status`。
- `Source Proof Store`：保存内容 hash、size、line offsets 和可选 blob，用于 L0 事实证明。
- `Text / Path Index`：保存可 seek 的 gram postings 和 path catalog，用于 `find`、`grep`、`refs` 预过滤。
- `Parser Facts Cache`：以 file hash 为 key 缓存 symbols、defs、calls，用于 parser/关系命令。

## 方案一：拆分文件扫描与内容证明

### 当前问题

`Workspace::scan_files` 同时承担路径枚举、二进制判断、语言识别、内容 hash 四个职责。结果是路径匹配类命令也会读取每个文件内容。

### 目标设计

新增两类记录：

```rust
pub struct FileCatalogRecord {
    pub path: String,
    pub language: Option<String>,
    pub size: u64,
    pub mtime_ns: Option<i128>,
    pub mode: Option<u32>,
    pub file_id: Option<String>,
}

pub struct FileProofRecord {
    pub path: String,
    pub size: u64,
    pub hash: String,
    pub line_offsets: Option<Vec<u32>>,
}
```

新增扫描 API：

```rust
impl Workspace {
    pub fn scan_catalog(&self, opts: &ScanOptions) -> Result<Vec<FileCatalogRecord>>;
    pub fn materialize_proofs(
        &self,
        records: &[FileCatalogRecord],
        opts: &ProofOptions,
    ) -> Result<Vec<FileRecord>>;
}
```

### 行为调整

| 命令 | 当前路径 | 新路径 |
| --- | --- | --- |
| `files` / `find-path` / `glob` | `scan_files` 全仓内容 hash | `scan_catalog` + path matcher（不读文件内容） |
| `list` / `tree` | `read_dir` + metadata（已不走 scan_files） | 保持现有逻辑，无需修改 |
| `status` | Workspace 字段（已不走 scan_files） | 保持现有逻辑；考虑 Workspace::discover 延迟初始化 git status |
| `changed` | 重复调用 git status | 复用 Workspace 中已有的变更信息，消除重复 git status 调用 |
| `read` | 目标文件读取 + hash（单文件，已不走 scan_files） | 保持现有逻辑；需要 proof 时只 hash 目标文件 |
| `find` / `grep` fallback | `scan_files` + 候选文件全文读取 | 只对 text candidate 文件读取内容 |
| `index build` | `scan_files` 全仓读取 | `scan_catalog` 后批量 parallel materialize proof |

### 验收指标

- `files`、`find-path`、`glob`：RuoYi 基线降到 `< 150ms`。
- `list`、`read`、`status`：RuoYi 基线降到 `< 100ms` 到 `< 150ms`。
- 二进制判断只允许读取文件头部小块，例如 8KB，不允许完整读两遍。

### 即时修复：消除 `changed` 命令的重复 git status

**当前问题**：`Workspace::discover` 已调用 `git_status()` (workspace.rs:59)，`search::changed` 又独立调用一次 (search.rs:197)，造成 `changed` 命令有双倍 git status 开销。

**修复**：不依赖方案一，直接作为独立 commit：
- 在 `Workspace` 中保留 `git_status` 结果（目前已丢弃原始结果只保存 counted values）
- `search::changed` 直接序列化 Workspace 中缓存的 changed files 列表
- 预期 `changed` 从 449ms 降到与 `status` 相近的 300ms 档位

## 方案二：freshness metadata 快路径

### 当前问题

索引查询时 freshness 对每个记录重新读取文件内容并计算 hash。对干净工作区来说，这个校验成本超过了索引查询收益。

### 目标设计

manifest 和 docs index 保存 metadata 与 content hash：

```json
{
  "snapshot_id": "commit:<sha>",
  "git_head": "<sha>",
  "scan_options_fingerprint": "<hash>",
  "files": [
    {
      "path": "src/main.rs",
      "size": 1234,
      "mtime_ns": 1770000000000000000,
      "mode": 33188,
      "file_id": "optional-platform-file-id",
      "hash": "blake3:..."
    }
  ]
}
```

freshness 判定顺序：

1. 如果 snapshot 是 commit，且 `HEAD == manifest.git_head`，并且 worktree 对该 snapshot 无需参与，直接 fresh。
2. 如果是 worktree 查询，先比较 path set、size、mtime、mode。
3. metadata 未变化时跳过 hash。
4. metadata 变化时只对变化文件重新 hash。
5. 文件缺失、metadata 异常、scan options 不一致时，标记 stale 并 fallback。

### 验收指标

- indexed `find`：RuoYi 基线 `< 300ms`。
- clean worktree indexed 查询不允许读取全仓文件内容。
- freshness 输出必须解释使用了 `metadata_fast_path`、`hash_verified` 还是 `stale_fallback`。

## 方案三：Text index 直接 seek

### 当前问题

`text_index::candidate_ids` 会顺序扫描 `grams.idx`，导致 indexed 查询仍有线性索引扫描成本。

### 目标设计

把 text index 拆成 dictionary 和 postings：

```text
grams.header
  magic
  version
  doc_count
  gram_count
  dictionary_offset
  postings_offset

dictionary
  gram: [u8; 3]
  postings_offset: u64
  postings_len: u32

postings
  sorted doc_id delta list
```

查询流程：

1. 从 pattern 提取 grams。
2. 对每个 gram 二分查 dictionary，直接 seek postings。
3. 用有序 postings 做交集。
4. 对候选文件做最终内容验证和 snippet/range 生成。

### 验收指标

- `candidate_ids` 复杂度从全索引扫描变成 `O(k log G + postings)`。
- indexed `find` 和 `grep` 必须稳定快于 live scan。
- 对短 query 或无法提取 gram 的 query，明确降级到 live scan 或 path-filtered scan。

## 方案四：真正的增量 index update

### 当前问题

warm `index build` 比 cold 更慢，说明当前 warm 路径没有复用旧索引。

### 目标设计

新增 index update 流程：

```text
load manifest
  -> scan_catalog
  -> diff added/modified/deleted/unchanged
  -> materialize proofs for added/modified only
  -> update source proof records
  -> update text/path index segment
  -> write new manifest atomically
```

初期可以采用简单 segment 模型：

```text
.code-search/text/<snapshot>/
  docs.idx
  paths.idx
  grams.base
  segments/
    000001.grams
    000002.grams
  tombstones.idx
```

### 查询与 compaction 策略

**查询**：读取 base + 所有 segments，并应用 tombstones。查询时统计 segment 数量。

**compaction 触发条件**（满足任一即触发）：
- segment 数量 >= 10
- tombstone 文件大小 >= segments 总文件大小的 20%
- 距上次 compaction 超过 30 次 update
- 手动触发：`index build --force`

**compaction 流程**：
1. 读取 base + 所有 segments + tombstones
2. 合并去重（应用 tombstones，保留最新版本）
3. 写入新 base（使用 tmp + rename 原子替换）
4. 删除所有 segments 和 tombstones
5. 更新 manifest 中的 segment_count 和 last_compaction_at

**compaction 期间的并发安全**：
- 使用 flock 文件锁保护 compaction 写入
- 查询时如果 compaction 正在进行，回退到 live scan
- compaction 失败时不删除旧 segments（base 写入是原子的）

### 并发安全

**index update 与查询的并发**：
- 不需要文件锁。update 采用 atomic rename（先写 .tmp 再 rename），查询要么读旧版本，要么读新版本，不会读到半写入文件。
- manifest 更新同样用 atomic rename。

**index build 与 index update 的并发**：
- 使用 `.code-search/.lock` 文件锁（flock），build 和 update 互斥。
- 查询不受影响（只读操作）。

### 验收指标

- no-op `index update`：`< 1s`。
- 单文件变更 update：`< 2s`。
- `index build --force` 仍允许全量重建。
- `post-commit/post-checkout/post-merge/post-rewrite` 继续维护 commit snapshot，不被 watcher 替代。
- segment 数量 >= 10 时自动触发 compaction，compaction 后查询性能不低于 base-only 查询。
- 并发 update + 查询时不会出现数据损坏或 panic。
- `index build --force` 不触发锁冲突（先等锁释放再全量重建）。

## 方案五：Parser facts 预过滤与缓存

### 当前问题

`defs`、`symbols`、`calls`、`callers` 每次全仓 parser scan，且每个文件重新创建 parser。

### 目标设计

Parser 命令分三步：

```text
query
  -> text/path index prefilter
  -> parser facts cache lookup by file hash
  -> parse missing candidate files only
```

缓存布局：

```text
.code-search/parser/<snapshot>/
  facts.db
  files.idx
```

缓存 key：

```text
language + parser_version + file_hash
```

命令策略：

| 命令 | 预过滤 |
| --- | --- |
| `defs <id>` | text index 找包含 identifier 的文件，再解析候选 |
| `symbols <query>` | path/lang filter + text index；空 query 才允许全量列出 |
| `calls <id>` | text index 找包含 callee identifier 的文件 |
| `callers <id>` | text index 找包含 callee identifier 的文件，再反查 caller |

### Parser 复用

当前每个文件执行 `Parser::new()` + `set_language()` + `parse()`。`Parser` 可安全复用：
- 按 language 维护 per-thread `Parser` pool
- 对同一 language 的文件复用 Parser 实例，仅调用 `parse()` 方法
- 预期减少 ~10% parser 初始化开销（在缓存命中前仍有收益）

> Java parser 已接入 `tree-sitter-java`，`parser_language` 支持 Java。后续 parser facts cache 落地时，需要把 Java 与现有 Rust/Python/TypeScript/JavaScript 一起纳入缓存键、parser 复用池和回归基准。

### 验收指标

- cached `defs` / `symbols`：`< 500ms`。
- prefiltered uncached `defs`：`< 2s`。
- `calls` / `callers`：候选文件数量显著小于全仓文件数。
- L2 关系结果继续标记为 candidate，不得改成 `exact=true`。

## 方案六：并行 I/O 与并行解析

### 当前问题

所有文件 I/O、hash 计算、gram 提取、tree-sitter 解析均为单线程串行执行。这些操作文件间互不依赖，天然可并行。当前 `Cargo.toml` 无并发相关依赖。

### 目标设计

引入 rayon 数据并行，在以下热点并行化：

**1. 文件扫描与 hash（workspace::scan_files / materialize_proofs）**
```text
scan_catalog 或 materialize_proofs
  -> rayon::par_iter 并行处理文件列表
  -> 每文件：fs::read + 二进制判断(仅头部8KB) + blake3 hash
  -> 按原路径排序后返回（保持确定性输出）
```

**2. Grams 索引构建（text_index::write_grams）**
```text
records.par_iter()
  -> 每文件：fs::read + grams_for_bytes
  -> 最终串行 merge 各线程的局部 BTreeMap<[u8;3], Vec<u32>>
  -> 写入时按 gram 排序（保持确定性）
```

**3. Parser 解析（syntax::collect_symbols / collect_calls）**
```text
候选文件列表.par_iter()
  -> 每文件：创建 Parser + set_language + parse + walk
  -> 收集结果到线程局部 Vec
  -> 最终串行 merge + 排序
```

**4. Freshness 校验（index::freshness）**
```text
records.par_iter()
  -> 每文件：fs::metadata 比较 size/mtime，仅在 metadata 变化时读文件 + hash
  -> 收集 fresh/stale/missing 到线程局部 vecs
```

### 并发安全约束

- **不引入异步运行时**（tokio/async-std），只用 rayon 做数据并行
- 所有并行操作对文件系统只读
- 写入操作（index build 中的 index 文件写入）在并行计算完成后串行执行
- 输出顺序保持确定性（并行计算后统一排序）
- 默认线程数 = `std::thread::available_parallelism()`，通过 `--threads` 参数可调

### 依赖添加

```toml
rayon = "1"
```

### 预期收益

| 操作 | 当前单线程耗时 | 预期并行后（8 核） |
| --- | ---: | ---: |
| scan_files (RuoYi ~2000 files) | ~400ms | ~60-80ms |
| write_grams (RuoYi) | ~3000ms | ~500ms |
| collect_symbols (RuoYi ~300 files) | ~6000ms | ~1000ms |
| freshness (RuoYi indexed, cold cache) | ~1500ms | ~250ms |

> 实际受 I/O 带宽和 CPU 核心数影响。

### 验收指标

- `index build` cold 从 13s 降到 `< 5s`（结合方案一 + 方案六）
- 并行模式 `--threads 1` 时输出结果与串行模式完全一致（确定性验证）
- 不引入新的 flaky test
## 分阶段执行计划

### Phase 1：停止不必要的全仓内容读取

**目标**：路径类、状态类、读取类命令从内容扫描路径中解耦。

**主要改动**：

- 修改 `src/workspace.rs`，引入 `scan_catalog` 和 `materialize_proofs`。
- 修改 `src/search.rs` 中 `files`、`find-path`、`glob` 的候选文件获取路径（走 `scan_catalog`）。
- 修改 `src/index.rs` build 入口，显式 materialize proof。
- 增加单元测试覆盖二进制判断只读取头部。
- 在 `materialize_proofs` 中集成 rayon 并行读取与 hash。

**验收**：

- `cargo test`
- `scripts/quality-gate.sh quick`
- `scripts/bench.sh compare`

### Phase 2：freshness metadata fast path

**目标**：indexed 查询不再对未变化文件全量 hash。

**主要改动**：

- 扩展 `docs.idx` 或 manifest 的 metadata 字段。
- 修改 `src/index.rs::freshness`。
- 为 stale、metadata unchanged、metadata changed 三类场景增加 fixture test。

**验收**：

- clean worktree indexed `find` 不触发全仓 `fs::read`。
- indexed `find` `< 300ms`。

### Phase 3：text index seek 化

**目标**：`candidate_ids` 不顺序扫描完整 `grams.idx`。

**主要改动**：

- 修改 `src/text_index.rs` index layout。
- 增加 dictionary + postings offset。
- 保留旧 index version 的错误提示或自动重建逻辑。

**验收**：

- indexed `find`、`grep` 比 live scan 快。
- index version mismatch 时返回可理解错误或触发 rebuild。

### Phase 4：增量 index update

**目标**：warm update 与变更文件数量相关，而不是全量重建。

**主要改动**：

- manifest diff added/modified/deleted/unchanged。
- changed-only proof materialization。
- segment 写入与 tombstone。
- `index status` 展示 no-op、stale、needs-update 原因。

**验收**：

- no-op update `< 1s`。
- 单文件 update `< 2s`。
- git hook 生命周期继续通过 smoke。

### Phase 5：parser facts prefilter/cache

**目标**：AST 类命令避免全仓解析。

**主要改动**：

- 新增 parser facts cache。
- `defs`、`symbols`、`calls`、`callers` 接入 text index prefilter。
- parser 按语言复用（language-level per-thread parser pool）。

**验收**：

- cached parser 查询 `< 500ms`。
- uncached parser 查询按候选文件数线性增长。
- reliability 字段保持 L1S/L2 边界。

### Phase 6：并行 I/O 与解析

**目标**：利用多核并行加速文件 I/O、hash、gram 提取和 parser 解析。

**主要改动**：
- 添加 `rayon` 依赖。
- `materialize_proofs` 并行化文件读取与 hash。
- `write_grams` 并行化 gram 提取（串行 merge）。
- `collect_symbols` / `collect_calls` 并行化 tree-sitter 解析。
- `freshness` 并行化 metadata 比较。
- 添加 `--threads` 全局参数（默认 auto）。
- 所有并行结果 merge 后统一排序，保证确定性。

**验收**：
- `index build` cold `< 5s`（结合 Phase 1）。
- 并行模式 `--threads 1` 与串行模式结果完全一致。
- 所有现有测试通过。
## 质量门禁更新

性能优化完成后，`scripts/baseline_values/` 应重采集并把以下目标作为新基线：

| 命令组 | 目标 |
| --- | ---: |
| `help` / `completions` | `< 50ms` |
| `status` / `list` / `read` | `< 150ms` |
| `changed` | `< 200ms`（消除重复 git status 后） |
| `files` / `find-path` / `glob` | `< 150ms` |
| live `find` / `grep` / `refs` | `< 500ms` |
| indexed `find` / `grep` | `< 300ms` |
| no-op `index update` | `< 1s` |
| one-file `index update` | `< 2s` |
| `index build` cold | `< 5s`（结合并行化） |
| cached `defs` / `symbols` | `< 500ms` |
| prefiltered `calls` / `callers` | `< 2s` |

CI 策略保持：

- PR 默认运行 `scripts/quality-gate.sh quick` 和 `cli`。
- 性能相关 MR 必须运行 `scripts/quality-gate.sh bench`。
- bench 在基线稳定后升级为 nightly 或 release 阻断。

## 风险与控制

| 风险 | 控制 |
| --- | --- |
| metadata fast path 漏掉文件变化 | metadata mismatch 时回退 hash；跨平台 file id 只做辅助，不做唯一依据 |
| index layout 变化破坏兼容 | index version 明确化；旧版本自动 rebuild 或输出操作指引 |
| 增量 segment 导致查询复杂化 | segment >= 10 自动 compaction；`index status` 展示 segment 数量 |
| segment 数量无限增长 | segment >= 10 自动 compaction；compaction 后归零 |
| parser cache 返回陈旧 facts | cache key 包含 file hash、language、parser version |
| 并行化引入非确定性 | 所有并行计算结果在 merge 后统一排序；`--threads 1` 可回退检测 |
| 索引更新与查询并发 | update 使用 atomic rename，查询无锁可读；build/update 之间用 flock 互斥 |
| 性能优化破坏 reliability | contract test 继续断言 `exact`、`producer`、`reliability` |
| 跨平台时间戳精度不一致 | freshness 优先使用 mtime；mtime 不可靠的平台回退 hash 校验 |

## 完成定义

性能优化不能只看单个 benchmark 变快，必须同时满足：

- 路径类命令不再依赖完整内容 hash。
- indexed 查询在 clean worktree 下明显快于 live scan。
- warm index update 不再全量重建。
- parser 命令有预过滤或缓存证据。
- `index build` cold 受益于并行 I/O，显著快于当前 13s 基线。
- segment 模型有明确的 compaction 触发策略，不会无限增长。
- 所有优化路径仍能解释 freshness 和 reliability 来源。
- `scripts/quality-gate.sh quick|cli|bench` 可作为统一验收入口。
