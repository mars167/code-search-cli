/// Escape single quotes in LanceDB filter strings to prevent query syntax errors.
fn escape_filter(s: &str) -> String {
    s.replace('\'', "''")
}

use anyhow::{Context, Result};
use arrow::array::Array;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use futures::StreamExt;
use lancedb::connect;
use lancedb::query::{ExecutableQuery, QueryBase};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
pub struct LanceDbStore {
    db: Arc<lancedb::Connection>,
    #[allow(dead_code)]
    root: PathBuf,
}

const READ_LIMIT: usize = 10_000_000;

use anyhow::anyhow;
use arrow::record_batch::RecordBatch;

/// Safely downcast a RecordBatch column to a specific Arrow array type.
/// Returns an error instead of panicking if the type doesn't match.
fn column_as<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    let col = batch
        .column_by_name(name)
        .ok_or_else(|| anyhow!("column '{}' not found in LanceDB batch", name))?;
    col.as_any().downcast_ref::<T>().ok_or_else(|| {
        anyhow!(
            "column '{}' has unexpected Arrow type, expected {}",
            name,
            std::any::type_name::<T>()
        )
    })
}

#[derive(Clone, Debug)]
pub struct ScipOccurrence {
    #[allow(dead_code)]
    pub snapshot_id: String,
    pub symbol: String,
    pub file_path: String,
    pub language: String,
    pub name: String,
    pub kind: String,
    pub role: String,
    pub range_start_line: u32,
    pub range_start_col: u32,
    pub range_end_line: u32,
    pub range_end_col: u32,
    pub is_definition: bool,
    pub file_hash: String,
    pub enclosing_symbol: Option<String>,
    pub producer: String,
}

pub fn lancedb_root(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".codetrail").join("index.lance")
}

static RUNTIME: LazyLock<tokio::runtime::Runtime> =
    LazyLock::new(|| tokio::runtime::Runtime::new().expect("failed to create tokio runtime"));

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    RUNTIME.block_on(f)
}

impl LanceDbStore {
    pub fn open_or_create(root: &Path) -> Result<Self> {
        let lance_path = lancedb_root(root);
        std::fs::create_dir_all(&lance_path)
            .with_context(|| format!("failed to create {:?}", lance_path))?;
        let db = block_on(connect(&lance_path.display().to_string()).execute())
            .with_context(|| format!("failed to connect to {:?}", lance_path))?;
        Ok(Self {
            db: Arc::new(db),
            root: lance_path,
        })
    }

    pub fn ensure_tables(&self) -> Result<()> {
        let existing = block_on(self.db.table_names().execute())
            .with_context(|| "failed to list table names")?;

        if !existing.iter().any(|t| t == "snapshots") {
            block_on(
                self.db
                    .create_empty_table("snapshots", snapshots_schema())
                    .execute(),
            )
            .with_context(|| "failed to create snapshots table")?;
        }

        if !existing.iter().any(|t| t == "file_catalog") {
            block_on(
                self.db
                    .create_empty_table("file_catalog", file_catalog_schema())
                    .execute(),
            )
            .with_context(|| "failed to create file_catalog table")?;
        }

        if !existing.iter().any(|t| t == "file_proofs") {
            block_on(
                self.db
                    .create_empty_table("file_proofs", file_proofs_schema())
                    .execute(),
            )
            .with_context(|| "failed to create file_proofs table")?;
        }

        if !existing.iter().any(|t| t == "gram_postings") {
            block_on(
                self.db
                    .create_empty_table("gram_postings", gram_postings_schema())
                    .execute(),
            )
            .with_context(|| "failed to create gram_postings table")?;
        }

        if !existing.iter().any(|t| t == "scip_occurrences") {
            block_on(
                self.db
                    .create_empty_table("scip_occurrences", scip_occurrences_schema())
                    .execute(),
            )
            .with_context(|| "failed to create scip_occurrences table")?;
        }

        if !existing.iter().any(|t| t == "parser_facts") {
            block_on(
                self.db
                    .create_empty_table("parser_facts", parser_facts_schema())
                    .execute(),
            )
            .with_context(|| "failed to create parser_facts table")?;
        }

        if !existing.iter().any(|t| t == "config_facts") {
            block_on(
                self.db
                    .create_empty_table("config_facts", config_facts_schema())
                    .execute(),
            )
            .with_context(|| "failed to create config_facts table")?;
        }

        if !existing.iter().any(|t| t == "config_dependency_edges") {
            block_on(
                self.db
                    .create_empty_table("config_dependency_edges", config_dependency_edges_schema())
                    .execute(),
            )
            .with_context(|| "failed to create config_dependency_edges table")?;
        }

        if !existing.iter().any(|t| t == "call_graph") {
            block_on(
                self.db
                    .create_empty_table("call_graph", call_graph_schema())
                    .execute(),
            )
            .with_context(|| "failed to create call_graph table")?;
        }

        Ok(())
    }

    pub fn delete_snapshot_rows(&self, snapshot_id: &str) -> Result<()> {
        let filter = format!("snapshot_id = '{}'", escape_filter(snapshot_id));
        for table_name in ["snapshots", "file_catalog", "file_proofs", "gram_postings"] {
            let table = block_on(self.db.open_table(table_name).execute())
                .with_context(|| format!("failed to open {table_name} table"))?;
            block_on(table.delete(&filter))
                .with_context(|| format!("failed to delete old {table_name} rows"))?;
        }
        Ok(())
    }

    // ── Write helpers ──

    pub fn write_snapshot(
        &self,
        snapshot_id: &str,
        snapshot_key: &str,
        schema_version: u32,
        tool_version: &str,
        repo_root: &str,
        head: Option<&str>,
        dirty: bool,
        source: &str,
        scan_options_json: &str,
        file_count: u32,
        created_at_epoch_ms: u64,
    ) -> Result<()> {
        use arrow::array::{BooleanArray, StringArray, UInt32Array, UInt64Array};
        use arrow::record_batch::{RecordBatch, RecordBatchIterator};

        let schema = snapshots_schema();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                std::sync::Arc::new(StringArray::from(vec![snapshot_id])),
                std::sync::Arc::new(StringArray::from(vec![snapshot_key])),
                std::sync::Arc::new(UInt32Array::from(vec![schema_version])),
                std::sync::Arc::new(StringArray::from(vec![tool_version])),
                std::sync::Arc::new(StringArray::from(vec![repo_root])),
                std::sync::Arc::new(StringArray::from(vec![head.map(|s| s.to_string())])),
                std::sync::Arc::new(BooleanArray::from(vec![dirty])),
                std::sync::Arc::new(StringArray::from(vec![source])),
                std::sync::Arc::new(StringArray::from(vec![scan_options_json])),
                std::sync::Arc::new(UInt32Array::from(vec![file_count])),
                std::sync::Arc::new(UInt64Array::from(vec![created_at_epoch_ms])),
                std::sync::Arc::new(UInt64Array::from(vec![None::<u64>])),
                std::sync::Arc::new(UInt32Array::from(vec![0u32])),
            ],
        )
        .context("failed to create snapshot RecordBatch")?;

        let batches = RecordBatchIterator::new(vec![batch].into_iter().map(Ok), schema);
        let table = block_on(self.db.open_table("snapshots").execute())
            .context("failed to open snapshots table")?;
        block_on(table.add(Box::new(batches)).execute()).context("failed to add snapshot row")?;
        Ok(())
    }

    pub fn write_file_catalog(
        &self,
        snapshot_id: &str,
        records: &[crate::workspace::FileRecord],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        use arrow::array::{BooleanArray, StringArray, UInt32Array, UInt64Array};
        use arrow::record_batch::{RecordBatch, RecordBatchIterator};

        let n = records.len();
        let mut snapshot_ids = Vec::with_capacity(n);
        let mut file_paths = Vec::with_capacity(n);
        let mut languages = Vec::with_capacity(n);
        let mut size_bytes = Vec::with_capacity(n);
        let mut mtime_ns = Vec::with_capacity(n);
        let mut modes = Vec::with_capacity(n);
        let mut is_binary = Vec::with_capacity(n);
        let mut is_ignored = Vec::with_capacity(n);

        for r in records {
            snapshot_ids.push(snapshot_id.to_string());
            file_paths.push(r.path.clone());
            languages.push(r.language.clone());
            size_bytes.push(r.size);
            mtime_ns.push((r.mtime_ms * 1_000_000) as u64);
            modes.push(r.mode);
            is_binary.push(false);
            is_ignored.push(false);
        }

        let schema = file_catalog_schema();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                std::sync::Arc::new(StringArray::from(snapshot_ids)),
                std::sync::Arc::new(StringArray::from(file_paths)),
                std::sync::Arc::new(StringArray::from(languages)),
                std::sync::Arc::new(UInt64Array::from(size_bytes)),
                std::sync::Arc::new(UInt64Array::from(mtime_ns)),
                std::sync::Arc::new(UInt32Array::from(modes)),
                std::sync::Arc::new(BooleanArray::from(is_binary)),
                std::sync::Arc::new(BooleanArray::from(is_ignored)),
            ],
        )
        .context("failed to create file_catalog RecordBatch")?;

        let batches = RecordBatchIterator::new(vec![batch].into_iter().map(Ok), schema);
        let table = block_on(self.db.open_table("file_catalog").execute())
            .context("failed to open file_catalog table")?;
        block_on(table.add(Box::new(batches)).execute())
            .context("failed to add file_catalog rows")?;
        Ok(())
    }

    pub fn write_file_proofs(
        &self,
        snapshot_id: &str,
        records: &[crate::workspace::FileRecord],
        workspace_root: Option<&Path>,
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        use arrow::array::{StringArray, UInt64Array};
        use arrow::record_batch::{RecordBatch, RecordBatchIterator};

        let n = records.len();
        let mut snapshot_ids = Vec::with_capacity(n);
        let mut file_paths = Vec::with_capacity(n);
        let mut content_hashes = Vec::with_capacity(n);
        let mut size_bytes = Vec::with_capacity(n);
        let mut line_offsets: Vec<Option<String>> = Vec::with_capacity(n);
        let mut blob_keys = Vec::with_capacity(n);

        for r in records {
            snapshot_ids.push(snapshot_id.to_string());
            file_paths.push(r.path.clone());
            content_hashes.push(r.hash.clone());
            size_bytes.push(r.size);
            line_offsets
                .push(workspace_root.and_then(|root| line_offsets_json(&root.join(&r.path)).ok()));
            blob_keys.push(r.hash.trim_start_matches("blake3:").to_string());
        }

        let schema = file_proofs_schema();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                std::sync::Arc::new(StringArray::from(snapshot_ids)),
                std::sync::Arc::new(StringArray::from(file_paths)),
                std::sync::Arc::new(StringArray::from(content_hashes)),
                std::sync::Arc::new(UInt64Array::from(size_bytes)),
                std::sync::Arc::new(StringArray::from(line_offsets)),
                std::sync::Arc::new(StringArray::from(blob_keys)),
            ],
        )
        .context("failed to create file_proofs RecordBatch")?;

        let batches = RecordBatchIterator::new(vec![batch].into_iter().map(Ok), schema);
        let table = block_on(self.db.open_table("file_proofs").execute())
            .context("failed to open file_proofs table")?;
        block_on(table.add(Box::new(batches)).execute())
            .context("failed to add file_proofs rows")?;
        Ok(())
    }

    pub fn write_gram_postings(
        &self,
        snapshot_id: &str,
        gram_index: &std::collections::BTreeMap<[u8; 3], Vec<u32>>,
    ) -> Result<()> {
        if gram_index.is_empty() {
            return Ok(());
        }

        use arrow::array::{StringArray, UInt32Array};
        use arrow::record_batch::{RecordBatch, RecordBatchIterator};

        let estimated = gram_index.values().map(|v| v.len()).sum::<usize>();
        let mut snapshot_ids = Vec::with_capacity(estimated);
        let mut grams = Vec::with_capacity(estimated);
        let mut doc_ids = Vec::with_capacity(estimated);

        for (gram, ids) in gram_index {
            let gram_hex = format!("{:02x}{:02x}{:02x}", gram[0], gram[1], gram[2]);
            for &doc_id in ids {
                snapshot_ids.push(snapshot_id.to_string());
                grams.push(gram_hex.clone());
                doc_ids.push(doc_id);
            }
        }

        let schema = gram_postings_schema();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                std::sync::Arc::new(StringArray::from(snapshot_ids)),
                std::sync::Arc::new(StringArray::from(grams)),
                std::sync::Arc::new(UInt32Array::from(doc_ids)),
            ],
        )
        .context("failed to create gram_postings RecordBatch")?;

        let batches = RecordBatchIterator::new(vec![batch].into_iter().map(Ok), schema);
        let table = block_on(self.db.open_table("gram_postings").execute())
            .context("failed to open gram_postings table")?;
        block_on(table.add(Box::new(batches)).execute())
            .context("failed to add gram_postings rows")?;
        Ok(())
    }
    pub fn write_scip_occurrences(
        &self,
        snapshot_id: &str,
        records: &[ScipOccurrence],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        use arrow::array::{BooleanArray, StringArray, UInt32Array};
        use arrow::record_batch::{RecordBatch, RecordBatchIterator};

        let n = records.len();
        let mut snapshot_ids = Vec::with_capacity(n);
        let mut symbols = Vec::with_capacity(n);
        let mut file_paths = Vec::with_capacity(n);
        let mut languages = Vec::with_capacity(n);
        let mut names = Vec::with_capacity(n);
        let mut kinds = Vec::with_capacity(n);
        let mut roles = Vec::with_capacity(n);
        let mut range_start_lines = Vec::with_capacity(n);
        let mut range_start_cols = Vec::with_capacity(n);
        let mut range_end_lines = Vec::with_capacity(n);
        let mut range_end_cols = Vec::with_capacity(n);
        let mut is_defs = Vec::with_capacity(n);
        let mut file_hashes = Vec::with_capacity(n);
        let mut enclosing_symbols = Vec::with_capacity(n);
        let mut producers = Vec::with_capacity(n);

        for r in records {
            snapshot_ids.push(snapshot_id.to_string());
            symbols.push(r.symbol.clone());
            file_paths.push(r.file_path.clone());
            languages.push(r.language.clone());
            names.push(r.name.clone());
            kinds.push(r.kind.clone());
            roles.push(r.role.clone());
            range_start_lines.push(r.range_start_line);
            range_start_cols.push(r.range_start_col);
            range_end_lines.push(r.range_end_line);
            range_end_cols.push(r.range_end_col);
            is_defs.push(r.is_definition);
            file_hashes.push(r.file_hash.clone());
            enclosing_symbols.push(r.enclosing_symbol.clone());
            producers.push(r.producer.clone());
        }

        let schema = scip_occurrences_schema();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                std::sync::Arc::new(StringArray::from(snapshot_ids)),
                std::sync::Arc::new(StringArray::from(symbols)),
                std::sync::Arc::new(StringArray::from(file_paths)),
                std::sync::Arc::new(StringArray::from(languages)),
                std::sync::Arc::new(StringArray::from(names)),
                std::sync::Arc::new(StringArray::from(kinds)),
                std::sync::Arc::new(StringArray::from(roles)),
                std::sync::Arc::new(UInt32Array::from(range_start_lines)),
                std::sync::Arc::new(UInt32Array::from(range_start_cols)),
                std::sync::Arc::new(UInt32Array::from(range_end_lines)),
                std::sync::Arc::new(UInt32Array::from(range_end_cols)),
                std::sync::Arc::new(BooleanArray::from(is_defs)),
                std::sync::Arc::new(StringArray::from(file_hashes)),
                std::sync::Arc::new(StringArray::from(enclosing_symbols)),
                std::sync::Arc::new(StringArray::from(producers)),
            ],
        )
        .context("failed to create scip_occurrences RecordBatch")?;

        let batches = RecordBatchIterator::new(vec![batch].into_iter().map(Ok), schema);
        let table = block_on(self.db.open_table("scip_occurrences").execute())
            .context("failed to open scip_occurrences table")?;
        block_on(table.add(Box::new(batches)).execute())
            .context("failed to add scip_occurrences rows")?;
        Ok(())
    }

    pub fn read_scip_occurrences(&self, snapshot_id: &str) -> Result<Vec<ScipOccurrence>> {
        let table = block_on(self.db.open_table("scip_occurrences").execute())
            .with_context(|| "failed to open scip_occurrences table")?;
        let filter = format!("snapshot_id = \'{}\'", escape_filter(snapshot_id));
        let mut stream = block_on(table.query().only_if(&filter).limit(READ_LIMIT).execute())
            .with_context(|| "failed to query scip_occurrences")?;
        let mut rows = Vec::new();
        while let Some(batch_result) = block_on(stream.next()) {
            let batch = batch_result.with_context(|| "failed to read scip_occurrences batch")?;
            let col_sid = Some(column_as::<arrow::array::StringArray>(
                &batch,
                "snapshot_id",
            )?);
            let col_sym = Some(column_as::<arrow::array::StringArray>(&batch, "symbol")?);
            let col_fp = Some(column_as::<arrow::array::StringArray>(&batch, "file_path")?);
            let col_lang = Some(column_as::<arrow::array::StringArray>(&batch, "language")?);
            let col_name = Some(column_as::<arrow::array::StringArray>(&batch, "name")?);
            let col_kind = Some(column_as::<arrow::array::StringArray>(&batch, "kind")?);
            let col_role = Some(column_as::<arrow::array::StringArray>(&batch, "role")?);
            let col_rsl = Some(column_as::<arrow::array::UInt32Array>(
                &batch,
                "range_start_line",
            )?);
            let col_rsc = Some(column_as::<arrow::array::UInt32Array>(
                &batch,
                "range_start_col",
            )?);
            let col_rel = Some(column_as::<arrow::array::UInt32Array>(
                &batch,
                "range_end_line",
            )?);
            let col_rec = Some(column_as::<arrow::array::UInt32Array>(
                &batch,
                "range_end_col",
            )?);
            let col_isdef = Some(column_as::<arrow::array::BooleanArray>(
                &batch,
                "is_definition",
            )?);
            let col_fh = Some(column_as::<arrow::array::StringArray>(&batch, "file_hash")?);
            let col_es = Some(column_as::<arrow::array::StringArray>(
                &batch,
                "enclosing_symbol",
            )?);
            let col_prod = Some(column_as::<arrow::array::StringArray>(&batch, "producer")?);

            if let (
                Some(sid),
                Some(sym),
                Some(fp),
                Some(lang),
                Some(name),
                Some(kind),
                Some(role),
                Some(rsl),
                Some(rsc),
                Some(rel),
                Some(rec),
                Some(isdef),
                Some(fh),
                Some(es),
                Some(prod),
            ) = (
                col_sid, col_sym, col_fp, col_lang, col_name, col_kind, col_role, col_rsl, col_rsc,
                col_rel, col_rec, col_isdef, col_fh, col_es, col_prod,
            ) {
                for i in 0..batch.num_rows() {
                    rows.push(ScipOccurrence {
                        snapshot_id: sid.value(i).to_string(),
                        symbol: sym.value(i).to_string(),
                        file_path: fp.value(i).to_string(),
                        language: lang.value(i).to_string(),
                        name: name.value(i).to_string(),
                        kind: kind.value(i).to_string(),
                        role: role.value(i).to_string(),
                        range_start_line: rsl.value(i),
                        range_start_col: rsc.value(i),
                        range_end_line: rel.value(i),
                        range_end_col: rec.value(i),
                        is_definition: isdef.value(i),
                        file_hash: fh.value(i).to_string(),
                        enclosing_symbol: if es.is_null(i) {
                            None
                        } else {
                            Some(es.value(i).to_string())
                        },
                        producer: prod.value(i).to_string(),
                    });
                }
            }
        }
        Ok(rows)
    }
    pub fn read_snapshot(&self, snapshot_id: &str) -> Result<Option<SnapShotRow>> {
        let table = block_on(self.db.open_table("snapshots").execute())
            .with_context(|| "failed to open snapshots table")?;
        let filter = format!("snapshot_id = \'{}\'", escape_filter(snapshot_id));
        let mut stream = block_on(table.query().only_if(&filter).limit(READ_LIMIT).execute())
            .with_context(|| "failed to query snapshots")?;
        let mut latest = None;
        while let Some(batch_result) = block_on(stream.next()) {
            let batch = batch_result.with_context(|| "failed to read snapshot batch")?;
            let col_snapshot_id = Some(column_as::<arrow::array::StringArray>(
                &batch,
                "snapshot_id",
            )?);
            let col_snapshot_key = Some(column_as::<arrow::array::StringArray>(
                &batch,
                "snapshot_key",
            )?);
            let col_schema_version = Some(column_as::<arrow::array::UInt32Array>(
                &batch,
                "schema_version",
            )?);
            let col_tool_version = Some(column_as::<arrow::array::StringArray>(
                &batch,
                "tool_version",
            )?);
            let col_repo_root = Some(column_as::<arrow::array::StringArray>(&batch, "repo_root")?);
            let col_head = Some(column_as::<arrow::array::StringArray>(&batch, "head")?);
            let col_dirty = Some(column_as::<arrow::array::BooleanArray>(&batch, "dirty")?);
            let col_source = Some(column_as::<arrow::array::StringArray>(&batch, "source")?);
            let col_scan_opts = Some(column_as::<arrow::array::StringArray>(
                &batch,
                "scan_options_json",
            )?);
            let col_file_count = Some(column_as::<arrow::array::UInt32Array>(
                &batch,
                "file_count",
            )?);
            let col_created_at = Some(column_as::<arrow::array::UInt64Array>(
                &batch,
                "created_at_epoch_ms",
            )?);

            if let (
                Some(sid),
                Some(skey),
                Some(sv),
                Some(tv),
                Some(rr),
                Some(h),
                Some(d),
                Some(s),
                Some(so),
                Some(fc),
                Some(ca),
            ) = (
                col_snapshot_id,
                col_snapshot_key,
                col_schema_version,
                col_tool_version,
                col_repo_root,
                col_head,
                col_dirty,
                col_source,
                col_scan_opts,
                col_file_count,
                col_created_at,
            ) {
                for i in 0..batch.num_rows() {
                    if sid.value(i) == snapshot_id {
                        let row = SnapShotRow {
                            snapshot_id: sid.value(i).to_string(),
                            snapshot_key: skey.value(i).to_string(),
                            schema_version: sv.value(i),
                            tool_version: tv.value(i).to_string(),
                            repo_root: rr.value(i).to_string(),
                            head: if h.is_null(i) {
                                None
                            } else {
                                Some(h.value(i).to_string())
                            },
                            dirty: d.value(i),
                            source: s.value(i).to_string(),
                            scan_options_json: so.value(i).to_string(),
                            file_count: fc.value(i),
                            created_at_epoch_ms: ca.value(i),
                        };
                        let is_newer = latest
                            .as_ref()
                            .map(|current: &SnapShotRow| {
                                row.created_at_epoch_ms >= current.created_at_epoch_ms
                            })
                            .unwrap_or(true);
                        if is_newer {
                            latest = Some(row);
                        }
                    }
                }
            }
        }
        Ok(latest)
    }

    pub fn read_file_catalog(&self, snapshot_id: &str) -> Result<Vec<FileCatalogRow>> {
        let table = block_on(self.db.open_table("file_catalog").execute())
            .with_context(|| "failed to open file_catalog table")?;
        let filter = format!("snapshot_id = \'{}\'", escape_filter(snapshot_id));
        let mut stream = block_on(table.query().only_if(&filter).limit(READ_LIMIT).execute())
            .with_context(|| "failed to query file_catalog")?;
        let mut rows = Vec::new();
        while let Some(batch_result) = block_on(stream.next()) {
            let batch = batch_result.with_context(|| "failed to read file_catalog batch")?;
            let col_sid = Some(column_as::<arrow::array::StringArray>(
                &batch,
                "snapshot_id",
            )?);
            let col_fp = Some(column_as::<arrow::array::StringArray>(&batch, "file_path")?);
            let col_lang = Some(column_as::<arrow::array::StringArray>(&batch, "language")?);
            let col_sz = Some(column_as::<arrow::array::UInt64Array>(
                &batch,
                "size_bytes",
            )?);
            let col_mt = Some(column_as::<arrow::array::UInt64Array>(&batch, "mtime_ns")?);
            let col_mode = Some(column_as::<arrow::array::UInt32Array>(&batch, "mode")?);
            if let (Some(sid), Some(fp), Some(lang), Some(sz), Some(mt), Some(mode)) =
                (col_sid, col_fp, col_lang, col_sz, col_mt, col_mode)
            {
                for i in 0..batch.num_rows() {
                    rows.push(FileCatalogRow {
                        snapshot_id: sid.value(i).to_string(),
                        file_path: fp.value(i).to_string(),
                        language: lang.value(i).to_string(),
                        size_bytes: sz.value(i),
                        mtime_ns: mt.value(i),
                        mode: mode.value(i),
                    });
                }
            }
        }
        Ok(rows)
    }

    pub fn read_file_proofs(&self, snapshot_id: &str) -> Result<Vec<FileProofRow>> {
        let table = block_on(self.db.open_table("file_proofs").execute())
            .with_context(|| "failed to open file_proofs table")?;
        let filter = format!("snapshot_id = \'{}\'", escape_filter(snapshot_id));
        let mut stream = block_on(table.query().only_if(&filter).limit(READ_LIMIT).execute())
            .with_context(|| "failed to query file_proofs")?;
        let mut rows = Vec::new();
        while let Some(batch_result) = block_on(stream.next()) {
            let batch = batch_result.with_context(|| "failed to read file_proofs batch")?;
            let col_sid = Some(column_as::<arrow::array::StringArray>(
                &batch,
                "snapshot_id",
            )?);
            let col_fp = Some(column_as::<arrow::array::StringArray>(&batch, "file_path")?);
            let col_hash = Some(column_as::<arrow::array::StringArray>(
                &batch,
                "content_hash",
            )?);
            let col_sz = Some(column_as::<arrow::array::UInt64Array>(
                &batch,
                "size_bytes",
            )?);
            let col_offsets = Some(column_as::<arrow::array::StringArray>(
                &batch,
                "line_offsets",
            )?);
            if let (Some(sid), Some(fp), Some(hash), Some(sz), Some(offsets)) =
                (col_sid, col_fp, col_hash, col_sz, col_offsets)
            {
                for i in 0..batch.num_rows() {
                    rows.push(FileProofRow {
                        snapshot_id: sid.value(i).to_string(),
                        file_path: fp.value(i).to_string(),
                        content_hash: hash.value(i).to_string(),
                        size_bytes: sz.value(i),
                        line_offsets: if offsets.is_null(i) {
                            None
                        } else {
                            Some(offsets.value(i).to_string())
                        },
                    });
                }
            }
        }
        Ok(rows)
    }

    pub fn read_gram_postings(
        &self,
        snapshot_id: &str,
        wanted: &HashSet<[u8; 3]>,
    ) -> Result<BTreeMap<[u8; 3], Vec<usize>>> {
        if wanted.is_empty() {
            return Ok(BTreeMap::new());
        }

        let table = block_on(self.db.open_table("gram_postings").execute())
            .with_context(|| "failed to open gram_postings table")?;
        let gram_values = wanted
            .iter()
            .map(|gram| format!("'{:02x}{:02x}{:02x}'", gram[0], gram[1], gram[2]))
            .collect::<Vec<_>>()
            .join(", ");
        let filter = format!(
            "snapshot_id = '{}' AND gram IN ({})",
            escape_filter(snapshot_id),
            gram_values
        );
        let mut stream = block_on(table.query().only_if(&filter).limit(READ_LIMIT).execute())
            .with_context(|| "failed to query gram_postings")?;
        let mut postings: BTreeMap<[u8; 3], Vec<usize>> = BTreeMap::new();
        while let Some(batch_result) = block_on(stream.next()) {
            let batch = batch_result.with_context(|| "failed to read gram_postings batch")?;
            let col_gram = column_as::<arrow::array::StringArray>(&batch, "gram")?;
            let col_doc = column_as::<arrow::array::UInt32Array>(&batch, "doc_id")?;
            for i in 0..batch.num_rows() {
                if col_gram.is_null(i) || col_doc.is_null(i) {
                    continue;
                }
                if let Some(gram) = decode_gram_hex(col_gram.value(i)) {
                    postings
                        .entry(gram)
                        .or_default()
                        .push(col_doc.value(i) as usize);
                }
            }
        }
        if !wanted.iter().all(|gram| postings.contains_key(gram)) {
            return Ok(BTreeMap::new());
        }
        for ids in postings.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }
        Ok(postings)
    }

    pub fn read_file_records(
        &self,
        snapshot_id: &str,
    ) -> Result<Vec<crate::workspace::FileRecord>> {
        let catalog: BTreeMap<String, FileCatalogRow> = self
            .read_file_catalog(snapshot_id)?
            .into_iter()
            .map(|row| (row.file_path.clone(), row))
            .collect();
        let proofs: HashMap<String, FileProofRow> = self
            .read_file_proofs(snapshot_id)?
            .into_iter()
            .map(|p| (p.file_path.clone(), p))
            .collect();
        Ok(catalog
            .into_iter()
            .map(|(_path, row)| {
                let proof = proofs.get(&row.file_path);
                crate::workspace::FileRecord {
                    path: row.file_path,
                    language: row.language,
                    size: row.size_bytes,
                    mtime_ms: (row.mtime_ns / 1_000_000) as u128,
                    mode: row.mode,
                    hash: proof
                        .map(|p| p.content_hash.clone())
                        .unwrap_or_else(|| "blake3:missing_proof".to_string()),
                }
            })
            .collect())
    }
}

fn decode_gram_hex(value: &str) -> Option<[u8; 3]> {
    if value.len() != 6 {
        return None;
    }
    Some([
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ])
}

fn line_offsets_json(path: &Path) -> Result<String> {
    let content = std::fs::read(path)?;
    let mut offsets = vec![0_u64];
    for (idx, byte) in content.iter().enumerate() {
        if *byte == b'\n' && idx + 1 < content.len() {
            offsets.push((idx + 1) as u64);
        }
    }
    Ok(serde_json::to_string(&offsets).unwrap_or_else(|_| "[0]".to_string()))
}

fn snapshots_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("snapshot_id", DataType::Utf8, false),
        Field::new("snapshot_key", DataType::Utf8, false),
        Field::new("schema_version", DataType::UInt32, false),
        Field::new("tool_version", DataType::Utf8, false),
        Field::new("repo_root", DataType::Utf8, false),
        Field::new("head", DataType::Utf8, true),
        Field::new("dirty", DataType::Boolean, false),
        Field::new("source", DataType::Utf8, false),
        Field::new("scan_options_json", DataType::Utf8, false),
        Field::new("file_count", DataType::UInt32, false),
        Field::new("created_at_epoch_ms", DataType::UInt64, false),
        Field::new("last_compaction_at_epoch_ms", DataType::UInt64, true),
        Field::new("segment_count", DataType::UInt32, false),
    ]))
}

fn file_catalog_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("snapshot_id", DataType::Utf8, false),
        Field::new("file_path", DataType::Utf8, false),
        Field::new("language", DataType::Utf8, false),
        Field::new("size_bytes", DataType::UInt64, false),
        Field::new("mtime_ns", DataType::UInt64, false),
        Field::new("mode", DataType::UInt32, false),
        Field::new("is_binary", DataType::Boolean, false),
        Field::new("is_ignored", DataType::Boolean, false),
    ]))
}

fn file_proofs_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("snapshot_id", DataType::Utf8, false),
        Field::new("file_path", DataType::Utf8, false),
        Field::new("content_hash", DataType::Utf8, false),
        Field::new("size_bytes", DataType::UInt64, false),
        Field::new("line_offsets", DataType::Utf8, true),
        Field::new("blob_key", DataType::Utf8, false),
    ]))
}

fn gram_postings_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("snapshot_id", DataType::Utf8, false),
        Field::new("gram", DataType::Utf8, false),
        Field::new("doc_id", DataType::UInt32, false),
    ]))
}

fn scip_occurrences_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("snapshot_id", DataType::Utf8, false),
        Field::new("symbol", DataType::Utf8, false),
        Field::new("file_path", DataType::Utf8, false),
        Field::new("language", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("role", DataType::Utf8, false),
        Field::new("range_start_line", DataType::UInt32, false),
        Field::new("range_start_col", DataType::UInt32, false),
        Field::new("range_end_line", DataType::UInt32, false),
        Field::new("range_end_col", DataType::UInt32, false),
        Field::new("is_definition", DataType::Boolean, false),
        Field::new("file_hash", DataType::Utf8, false),
        Field::new("enclosing_symbol", DataType::Utf8, true),
        Field::new("producer", DataType::Utf8, false),
    ]))
}

fn parser_facts_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("snapshot_id", DataType::Utf8, false),
        Field::new("file_path", DataType::Utf8, false),
        Field::new("language", DataType::Utf8, false),
        Field::new("parser_version", DataType::Utf8, false),
        Field::new("file_hash", DataType::Utf8, false),
        Field::new("symbols", DataType::Binary, false),
        Field::new("calls", DataType::Binary, false),
        Field::new("cached_at_epoch_ms", DataType::UInt64, false),
    ]))
}

fn config_facts_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("snapshot_id", DataType::Utf8, false),
        Field::new("file_path", DataType::Utf8, false),
        Field::new("fact_kind", DataType::Utf8, false),
        Field::new("key_path", DataType::Utf8, true),
        Field::new("name", DataType::Utf8, true),
        Field::new("value_preview", DataType::Utf8, true),
        Field::new("preview_masked", DataType::Boolean, false),
        Field::new("producer", DataType::Utf8, false),
        Field::new("reliability", DataType::Utf8, false),
        Field::new("affected_root_ids", DataType::Utf8, false),
        Field::new("dependency_edge_kind", DataType::Utf8, true),
        Field::new("dependency_edge_refs", DataType::Utf8, false),
        Field::new("caveats", DataType::Utf8, false),
        Field::new("range_start_line", DataType::UInt32, false),
        Field::new("range_start_col", DataType::UInt32, false),
        Field::new("range_end_line", DataType::UInt32, false),
        Field::new("range_end_col", DataType::UInt32, false),
        Field::new("file_hash", DataType::Utf8, false),
        Field::new("cached_at_epoch_ms", DataType::UInt64, false),
    ]))
}

fn config_dependency_edges_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("snapshot_id", DataType::Utf8, false),
        Field::new("file_path", DataType::Utf8, false),
        Field::new("edge_schema", DataType::Utf8, false),
        Field::new("edge_kind", DataType::Utf8, false),
        Field::new("from_root_id", DataType::Utf8, true),
        Field::new("to_root_id", DataType::Utf8, true),
        Field::new("via_path", DataType::Utf8, true),
        Field::new("unresolved", DataType::Boolean, false),
        Field::new("producer", DataType::Utf8, false),
        Field::new("caveats", DataType::Utf8, false),
        Field::new("cached_at_epoch_ms", DataType::UInt64, false),
    ]))
}

fn call_graph_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("snapshot_id", DataType::Utf8, false),
        Field::new("caller_file", DataType::Utf8, false),
        Field::new("caller_symbol", DataType::Utf8, false),
        Field::new("callee", DataType::Utf8, false),
        Field::new("callee_file", DataType::Utf8, true),
        Field::new("language", DataType::Utf8, false),
        Field::new("reliability", DataType::Utf8, false),
        Field::new("file_hash", DataType::Utf8, false),
    ]))
}

pub struct SnapShotRow {
    pub snapshot_id: String,
    pub snapshot_key: String,
    pub schema_version: u32,
    pub tool_version: String,
    pub repo_root: String,
    pub head: Option<String>,
    pub dirty: bool,
    pub source: String,
    pub scan_options_json: String,
    pub file_count: u32,
    pub created_at_epoch_ms: u64,
}

pub struct FileCatalogRow {
    #[allow(dead_code)]
    pub snapshot_id: String,
    pub file_path: String,
    pub language: String,
    pub size_bytes: u64,
    pub mtime_ns: u64,
    pub mode: u32,
}

pub struct FileProofRow {
    #[allow(dead_code)]
    pub snapshot_id: String,
    pub file_path: String,
    pub content_hash: String,
    #[allow(dead_code)]
    pub size_bytes: u64,
    pub line_offsets: Option<String>,
}

pub fn is_available(workspace_root: &Path) -> bool {
    let root = lancedb_root(workspace_root);
    if !root.is_dir() {
        return false;
    }
    // Verify the database is connectable (not just directory exists)
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(_) => return false,
    };
    match rt.block_on(connect(&root.display().to_string()).execute()) {
        Ok(db) => match rt.block_on(db.table_names().execute()) {
            Ok(tables) => !tables.is_empty(),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_open_or_create_idempotent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let store1 = LanceDbStore::open_or_create(root).unwrap();
        assert!(store1.root.exists());

        let store2 = LanceDbStore::open_or_create(root).unwrap();
        assert_eq!(store1.root, store2.root);
    }

    #[test]
    fn test_ensure_tables_creates_all() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let store = LanceDbStore::open_or_create(root).unwrap();
        store.ensure_tables().unwrap();

        let existing = block_on(store.db.table_names().execute()).unwrap();
        assert!(existing.iter().any(|t| t == "snapshots"));
        assert!(existing.iter().any(|t| t == "file_catalog"));
        assert!(existing.iter().any(|t| t == "file_proofs"));
        assert!(existing.iter().any(|t| t == "gram_postings"));
        assert!(existing.iter().any(|t| t == "scip_occurrences"));
        assert!(existing.iter().any(|t| t == "parser_facts"));
        assert!(existing.iter().any(|t| t == "config_facts"));
        assert!(existing.iter().any(|t| t == "config_dependency_edges"));
        assert!(existing.iter().any(|t| t == "call_graph"));
    }

    #[test]
    fn test_lancedb_directory_created() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let _store = LanceDbStore::open_or_create(root).unwrap();
        let expected = root.join(".codetrail").join("index.lance");
        assert!(expected.exists());
        assert!(expected.is_dir());
    }

    #[test]
    fn test_lancedb_root_helper() {
        let p = Path::new("/foo/bar");
        assert_eq!(
            lancedb_root(p),
            PathBuf::from("/foo/bar/.codetrail/index.lance")
        );
    }

    #[test]
    fn test_gram_postings_round_trip() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let store = LanceDbStore::open_or_create(root).unwrap();
        store.ensure_tables().unwrap();

        let mut grams = BTreeMap::new();
        grams.insert(*b"nee", vec![0u32]);
        grams.insert(*b"eed", vec![0u32, 1u32]);
        store
            .write_gram_postings("worktree:non-git", &grams)
            .unwrap();

        let wanted = HashSet::from([*b"nee", *b"eed"]);
        let postings = store
            .read_gram_postings("worktree:non-git", &wanted)
            .unwrap();

        assert_eq!(postings.get(b"nee").unwrap(), &vec![0usize]);
        assert_eq!(postings.get(b"eed").unwrap(), &vec![0usize, 1usize]);
    }

    #[test]
    fn test_file_proofs_store_line_offsets() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("sample.txt"), "one\ntwo\nthree\n").unwrap();

        let store = LanceDbStore::open_or_create(root).unwrap();
        store.ensure_tables().unwrap();
        let records = vec![crate::workspace::FileRecord {
            path: "sample.txt".to_string(),
            language: "text".to_string(),
            size: 14,
            mtime_ms: 1,
            mode: 0,
            hash: "blake3:test".to_string(),
        }];

        store
            .write_file_proofs("worktree:non-git", &records, Some(root))
            .unwrap();

        let proofs = store.read_file_proofs("worktree:non-git").unwrap();
        assert_eq!(proofs.len(), 1);
        assert_eq!(proofs[0].line_offsets.as_deref(), Some("[0,4,8]"));
    }

    #[test]
    fn test_gram_postings_missing_wanted_gram_is_empty() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let store = LanceDbStore::open_or_create(root).unwrap();
        store.ensure_tables().unwrap();

        let mut grams = BTreeMap::new();
        grams.insert(*b"nee", vec![0u32]);
        store
            .write_gram_postings("worktree:non-git", &grams)
            .unwrap();

        let wanted = HashSet::from([*b"nee", *b"abs"]);
        let postings = store
            .read_gram_postings("worktree:non-git", &wanted)
            .unwrap();

        assert!(postings.is_empty());
    }
}
