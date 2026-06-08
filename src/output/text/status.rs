use std::io::{self, Write};

use serde_json::Value;

pub(super) fn render_text_status_like(
    command: &str,
    results: &[Value],
    out: &mut dyn Write,
) -> io::Result<()> {
    for result in results {
        match command {
            "status" => {
                let root = result.get("root").and_then(Value::as_str).unwrap_or("");
                if !root.is_empty() {
                    writeln!(out, "Workspace: {root}")?;
                }
                if let Some(head) = result.get("head").and_then(Value::as_str) {
                    writeln!(out, "Head: {head}")?;
                }
                let dirty = result
                    .get("dirty")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let staged = result
                    .get("stagedCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let worktree = result
                    .get("worktreeCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                writeln!(out, "Dirty: {dirty} (staged {staged}, worktree {worktree})")?;
            }
            "index status" | "index verify" => render_index_status_result(result, out)?,
            "index build" | "index update" => render_index_build_result(result, out)?,
            "index import-scip" => render_index_import_result(result, out)?,
            "index pack" => render_index_pack_result(result, out)?,
            "index unpack" => render_index_unpack_result(result, out)?,
            "index clean" => render_index_clean_result(result, out)?,
            _ => writeln!(out, "{}", one_line_json(result))?,
        }
    }
    Ok(())
}

fn render_index_status_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    let exists = result.get("exists").and_then(Value::as_bool);
    let fresh = result.get("fresh").and_then(Value::as_bool);
    if let Some(exists) = exists {
        writeln!(out, "Index exists: {exists}")?;
    }
    if let Some(fresh) = fresh {
        writeln!(out, "Index fresh: {fresh}")?;
    }
    if let Some(path) = result.get("path").and_then(Value::as_str) {
        writeln!(out, "Path: {path}")?;
    }
    if let Some(file_count) = result
        .pointer("/manifest/fileCount")
        .and_then(Value::as_u64)
    {
        writeln!(out, "Files: {file_count}")?;
    }
    if let Some(reason) = result.get("reason").and_then(Value::as_str) {
        writeln!(out, "Reason: {reason}")?;
    }
    Ok(())
}

fn render_index_build_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    let index = result.get("index").unwrap_or(result);
    if result.get("updated").and_then(Value::as_bool) == Some(false) {
        writeln!(out, "Index already fresh")?;
    }
    if let Some(file_count) = index.get("fileCount").and_then(Value::as_u64) {
        writeln!(out, "Indexed {file_count} files")?;
    }
    if let Some(storage) = index.get("storageBackend").and_then(Value::as_str) {
        writeln!(out, "Backend: {storage}")?;
    }
    if let Some(path) = index.get("path").and_then(Value::as_str) {
        writeln!(out, "Path: {path}")?;
    }
    if index.get("fileCount").is_none() {
        writeln!(out, "{}", one_line_json(result))?;
    }
    Ok(())
}

fn render_index_import_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    let index = result.get("index").unwrap_or(result);
    let record_count = index
        .get("recordCount")
        .or_else(|| index.get("definitionCount"))
        .and_then(Value::as_u64);
    if let Some(record_count) = record_count {
        writeln!(out, "Imported {record_count} SCIP records")?;
    } else {
        writeln!(out, "Imported SCIP index")?;
    }
    if let Some(source) = index.get("source").and_then(Value::as_str) {
        writeln!(out, "Source: {source}")?;
    }
    if let Some(path) = index.get("path").and_then(Value::as_str) {
        writeln!(out, "Path: {path}")?;
    }
    Ok(())
}

fn render_index_pack_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    let output_path = result
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("archive");
    let entry_count = result
        .get("entryCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let archive_size = result
        .get("archiveSize")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    writeln!(out, "Packed index to {output_path}")?;
    if entry_count > 0 || archive_size > 0 {
        writeln!(out, "Entries: {entry_count}, bytes: {archive_size}")?;
    }
    Ok(())
}

fn render_index_unpack_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    if let Some(snapshot_id) = result.get("remote_snapshot_id").and_then(Value::as_str) {
        writeln!(out, "Unpacked remote snapshot {snapshot_id}")?;
    } else {
        writeln!(out, "Unpacked remote snapshot")?;
    }
    if let Some(remote_dir) = result.get("remoteDir").and_then(Value::as_str) {
        writeln!(out, "Path: {remote_dir}")?;
    }
    if let Some(entry_count) = result.get("entryCount").and_then(Value::as_u64) {
        writeln!(out, "Entries: {entry_count}")?;
    }
    Ok(())
}

fn render_index_clean_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    let cleaned = result
        .get("cleaned")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    writeln!(out, "Index cleaned: {cleaned}")?;
    if let Some(path) = result.get("path").and_then(Value::as_str) {
        writeln!(out, "Path: {path}")?;
    }
    Ok(())
}

pub(super) fn is_status_like(command: &str) -> bool {
    matches!(
        command,
        "status"
            | "index status"
            | "index verify"
            | "index build"
            | "index update"
            | "index import-scip"
            | "index pack"
            | "index unpack"
            | "index clean"
    )
}

fn one_line_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}
