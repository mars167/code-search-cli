use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use arrow::array::{StringBuilder, UInt64Builder};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::FileReader;
use arrow::ipc::writer::FileWriter;
use arrow::record_batch::RecordBatch;

use crate::workspace::FileRecord;

pub struct SnapshotFreshness {
    pub fresh_count: usize,
    pub stale_files: Vec<Value>,
    pub missing_files: Vec<Value>,
}

/// Write the legacy files.parquet path using Arrow IPC data.
pub fn write_files_parquet(path: &Path, records: &[FileRecord]) -> Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("language", DataType::Utf8, false),
        Field::new("size", DataType::UInt64, false),
        Field::new("mtime_ms", DataType::UInt64, false),
        Field::new("hash", DataType::Utf8, false),
    ]));

    let mut path_builder = StringBuilder::new();
    let mut language_builder = StringBuilder::new();
    let mut size_builder = UInt64Builder::new();
    let mut mtime_builder = UInt64Builder::new();
    let mut hash_builder = StringBuilder::new();

    for record in records {
        path_builder.append_value(&record.path);
        language_builder.append_value(&record.language);
        size_builder.append_value(record.size);
        mtime_builder.append_value(record.mtime_ms as u64);
        hash_builder.append_value(&record.hash);
    }

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(path_builder.finish()),
            Arc::new(language_builder.finish()),
            Arc::new(size_builder.finish()),
            Arc::new(mtime_builder.finish()),
            Arc::new(hash_builder.finish()),
        ],
    )?;

    let file = File::create(path)?;
    let mut writer = FileWriter::try_new(file, schema.as_ref())?;
    writer.write(&batch)?;
    writer.finish()?;

    Ok(())
}

/// Read the legacy files.parquet path back into FileRecords.
pub fn read_files_parquet(path: &Path) -> Result<Vec<FileRecord>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = FileReader::try_new(file, None)?;

    let mut records = Vec::new();
    for batch_result in reader {
        let batch = batch_result?;
        let path_col = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .context("expected string array for path")?;
        let language_col = batch
            .column(1)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .context("expected string array for language")?;
        let size_col = batch
            .column(2)
            .as_any()
            .downcast_ref::<arrow::array::UInt64Array>()
            .context("expected uint64 array for size")?;
        let mtime_col = batch
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::UInt64Array>()
            .context("expected uint64 array for mtime_ms")?;
        let hash_col = batch
            .column(4)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .context("expected string array for hash")?;

        for i in 0..batch.num_rows() {
            records.push(FileRecord {
                path: path_col.value(i).to_string(),
                language: language_col.value(i).to_string(),
                size: size_col.value(i),
                mtime_ms: mtime_col.value(i) as u128,
                mode: 0,
                hash: hash_col.value(i).to_string(),
            });
        }
    }

    Ok(records)
}

/// Write a content-addressed blob to blobs/<hash_hex>
pub fn write_blob(blobs_dir: &Path, hash_hex: &str, content: &[u8]) -> Result<()> {
    let blob_path = blobs_dir.join(hash_hex);
    if !blob_path.exists() {
        fs::write(&blob_path, content)?;
    }
    Ok(())
}

/// Build complete snapshot: files.parquet + all blobs
pub fn build_snapshot(
    snapshot_dir: &Path,
    records: &[FileRecord],
    workspace_root: &Path,
) -> Result<()> {
    fs::create_dir_all(snapshot_dir)?;
    let blobs_dir = snapshot_dir.join("blobs");
    fs::create_dir_all(&blobs_dir)?;

    write_files_parquet(&snapshot_dir.join("files.parquet"), records)?;

    for record in records {
        let file_path = workspace_root.join(&record.path);
        let content = fs::read(&file_path)
            .with_context(|| format!("failed to read {}", file_path.display()))?;
        let hash_hex = record.hash.strip_prefix("blake3:").unwrap_or(&record.hash);
        write_blob(&blobs_dir, hash_hex, &content)?;
    }

    Ok(())
}

/// Verify snapshot freshness against current files on disk
pub fn verify_snapshot(snapshot_dir: &Path, workspace_root: &Path) -> Result<SnapshotFreshness> {
    let parquet_path = snapshot_dir.join("files.parquet");
    let records = read_files_parquet(&parquet_path)?;

    let mut fresh_count = 0usize;
    let mut stale_files = Vec::new();
    let mut missing_files = Vec::new();

    for record in &records {
        let file_path = workspace_root.join(&record.path);
        if !file_path.exists() {
            missing_files.push(json!({ "path": record.path, "reason": "missing" }));
            continue;
        }
        match fs::read(&file_path) {
            Ok(content) => {
                let actual_hash = format!("blake3:{}", blake3::hash(&content).to_hex());
                if actual_hash == record.hash {
                    fresh_count += 1;
                } else {
                    stale_files.push(json!({
                        "path": record.path,
                        "reason": "file_hash_mismatch",
                        "expected": record.hash,
                        "actual": actual_hash
                    }));
                }
            }
            Err(error) => {
                stale_files.push(json!({
                    "path": record.path,
                    "reason": "read_error",
                    "message": error.to_string()
                }));
            }
        }
    }

    Ok(SnapshotFreshness {
        fresh_count,
        stale_files,
        missing_files,
    })
}
