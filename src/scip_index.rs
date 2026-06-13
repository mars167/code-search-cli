use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::Read,
    path::Path,
};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    index, lancedb_store,
    lsp::scip_gen,
    scip,
    scip::store::{OccurrenceResult, SymbolResult},
    workspace::{ScanOptions, Workspace},
};

const ROLE_DEFINITION: i32 = 1;
const OCCURRENCES_MAGIC: &[u8; 8] = b"CSOCC1\0\0";

#[derive(Clone, Debug)]
struct PreciseOccurrenceRecord {
    path: String,
    language: String,
    symbol: String,
    name: String,
    kind: String,
    role: String,
    range: PreciseRange,
    file_hash: String,
    producer: String,
}

#[derive(Clone, Debug)]
struct PreciseRange {
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScipJsonIndex {
    documents: Vec<ScipDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScipDocument {
    #[serde(alias = "relative_path")]
    relative_path: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    occurrences: Vec<ScipOccurrence>,
    #[serde(default)]
    symbols: Vec<ScipSymbol>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScipOccurrence {
    range: Vec<usize>,
    symbol: String,
    #[serde(default, alias = "symbol_roles")]
    symbol_roles: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScipSymbol {
    symbol: String,
    #[serde(default, alias = "display_name")]
    display_name: String,
    #[serde(default)]
    kind: Value,
}

pub struct PreciseQueryOutput {
    pub results: Value,
    pub index: Value,
}

pub fn import_scip_json(workspace: &Workspace, path: impl AsRef<Path>) -> Result<Value> {
    let source_path = path.as_ref();
    let input = fs::read(source_path)
        .with_context(|| format!("failed to read SCIP JSON {}", source_path.display()))?;
    let parsed: ScipJsonIndex = serde_json::from_slice(&input)
        .with_context(|| "failed to parse SCIP JSON; binary index.scip protobuf import is not available in this build")?;

    let root = index::scip_root(workspace);
    fs::create_dir_all(&root)?;

    let mut records = Vec::new();
    for document in parsed.documents {
        let symbols = document
            .symbols
            .iter()
            .map(|symbol| (symbol.symbol.as_str(), symbol))
            .collect::<HashMap<_, _>>();
        let file_hash = current_file_hash(workspace, &document.relative_path).unwrap_or_default();
        for occurrence in document.occurrences {
            if occurrence.symbol.is_empty() || occurrence.range.is_empty() {
                continue;
            }
            let Some(range) = scip_range(&occurrence.range) else {
                continue;
            };
            let symbol_info = symbols.get(occurrence.symbol.as_str());
            let name = symbol_info
                .and_then(|info| (!info.display_name.is_empty()).then(|| info.display_name.clone()))
                .unwrap_or_else(|| display_name_from_symbol(&occurrence.symbol));
            let kind = symbol_info
                .map(|info| kind_to_string(&info.kind))
                .filter(|kind| !kind.is_empty())
                .unwrap_or_else(|| "symbol".to_string());
            records.push(PreciseOccurrenceRecord {
                path: document.relative_path.clone(),
                language: document.language.clone(),
                symbol: occurrence.symbol,
                name,
                kind,
                role: if occurrence.symbol_roles & ROLE_DEFINITION != 0 {
                    "definition".to_string()
                } else {
                    "reference".to_string()
                },
                range,
                file_hash: file_hash.clone(),
                producer: "scip".to_string(),
            });
        }
    }

    import_to_lancedb(workspace, &records)?;

    Ok(json!({
        "index": {
            "used": true,
            "fresh": true,
            "source": "scip_json",
            "path": root,
            "storageBackend": "lancedb",
            "recordCount": records.len(),
            "definitionCount": records.iter().filter(|record| record.role == "definition").count()
        }
    }))
}

/// Import a native SCIP binary protobuf index (index.scip) and build the occurrence DB.
pub fn import_native_scip(workspace: &Workspace, path: impl AsRef<Path>) -> Result<Value> {
    let source_path = path.as_ref();
    let scip_index = scip::parse_native_scip(source_path).with_context(|| {
        format!(
            "failed to parse native SCIP index {}",
            source_path.display()
        )
    })?;

    let snapshot_hash = &workspace.snapshot_id;
    let db_path = native_db_path(workspace);

    scip::build_occurrences_db(&scip_index, &db_path, snapshot_hash, &workspace.root)
        .with_context(|| "failed to build occurrence database")?;

    let occ_count: usize = scip_index
        .documents
        .iter()
        .map(|d| d.occurrences.len())
        .sum();
    let sym_count: usize = scip_index.documents.iter().map(|d| d.symbols.len()).sum();

    Ok(json!({
        "index": {
            "used": true,
            "fresh": true,
            "source": "scip_native_protobuf",
            "path": db_path,
            "recordCount": occ_count,
            "definitionCount": sym_count
        }
    }))
}

pub fn native_db_path(workspace: &Workspace) -> std::path::PathBuf {
    index::scip_root(workspace).join("occurrences.db")
}

pub fn symbols(
    workspace: &Workspace,
    opts: &ScanOptions,
    query: &str,
) -> Result<Option<PreciseQueryOutput>> {
    // Try native occurrence DB first
    if let Some(output) = query_native_symbols(workspace, opts, query)? {
        return Ok(Some(output));
    }
    // Fall back to old occurrence.idx format
    query_precise(workspace, opts, |record| {
        record.role == "definition" && record.name.contains(query)
    })
}

pub fn defs(
    workspace: &Workspace,
    opts: &ScanOptions,
    identifier: &str,
) -> Result<Option<PreciseQueryOutput>> {
    // Try native occurrence DB first
    if let Some(output) = query_native_defs(workspace, opts, identifier)? {
        return Ok(Some(output));
    }
    // Fall back to old occurrence.idx format
    query_precise(workspace, opts, |record| {
        record.role == "definition" && matches_identifier(record, identifier)
    })
}

pub fn refs(
    workspace: &Workspace,
    opts: &ScanOptions,
    identifier: &str,
) -> Result<Option<PreciseQueryOutput>> {
    // Try native occurrence DB first
    if let Some(output) = query_native_refs(workspace, opts, identifier)? {
        return Ok(Some(output));
    }
    // Fall back to old occurrence.idx format
    query_precise(workspace, opts, |record| {
        record.role != "definition" && matches_identifier(record, identifier)
    })
}

// ---------------------------------------------------------------------------
// Native occurrence DB query helpers
// ---------------------------------------------------------------------------

fn query_native_defs(
    workspace: &Workspace,
    opts: &ScanOptions,
    identifier: &str,
) -> Result<Option<PreciseQueryOutput>> {
    let db_path = native_db_path(workspace);
    if !scip::occurrence_db_fresh(&db_path, &workspace.snapshot_id, &workspace.root) {
        return Ok(None);
    }
    if !scip_gen::generation_manifests_allow_precise_use(workspace).unwrap_or(false) {
        return Ok(None);
    }
    let mut results = scip::query_defs(&db_path, identifier)?;
    filter_and_limit(workspace, &mut results, opts)?;
    if results.is_empty() {
        return Ok(Some(PreciseQueryOutput {
            results: Value::Array(Vec::new()),
            index: native_db_index_meta(&db_path, true),
        }));
    }
    let json_results: Vec<Value> = results.iter().map(scip::occurrence_to_json).collect();
    Ok(Some(PreciseQueryOutput {
        results: Value::Array(json_results),
        index: native_db_index_meta(&db_path, true),
    }))
}

fn query_native_refs(
    workspace: &Workspace,
    opts: &ScanOptions,
    identifier: &str,
) -> Result<Option<PreciseQueryOutput>> {
    let db_path = native_db_path(workspace);
    if !scip::occurrence_db_fresh(&db_path, &workspace.snapshot_id, &workspace.root) {
        return Ok(None);
    }
    if !scip_gen::generation_manifests_allow_precise_use(workspace).unwrap_or(false) {
        return Ok(None);
    }
    let mut results = scip::query_refs(&db_path, identifier)?;
    filter_and_limit(workspace, &mut results, opts)?;
    if results.is_empty() {
        return Ok(Some(PreciseQueryOutput {
            results: Value::Array(Vec::new()),
            index: native_db_index_meta(&db_path, true),
        }));
    }
    let json_results: Vec<Value> = results.iter().map(scip::occurrence_to_json).collect();
    Ok(Some(PreciseQueryOutput {
        results: Value::Array(json_results),
        index: native_db_index_meta(&db_path, true),
    }))
}

fn query_native_symbols(
    workspace: &Workspace,
    opts: &ScanOptions,
    query: &str,
) -> Result<Option<PreciseQueryOutput>> {
    let db_path = native_db_path(workspace);
    if !scip::occurrence_db_fresh(&db_path, &workspace.snapshot_id, &workspace.root) {
        return Ok(None);
    }
    if !scip_gen::generation_manifests_allow_precise_use(workspace).unwrap_or(false) {
        return Ok(None);
    }
    let mut results = scip::query_symbols(&db_path, query)?;
    filter_symbol_results(workspace, &mut results, opts)?;
    if results.is_empty() {
        return Ok(Some(PreciseQueryOutput {
            results: Value::Array(Vec::new()),
            index: native_db_index_meta(&db_path, true),
        }));
    }
    let json_results: Vec<Value> = results.iter().map(scip::symbol_to_json).collect();
    Ok(Some(PreciseQueryOutput {
        results: Value::Array(json_results),
        index: native_db_index_meta(&db_path, true),
    }))
}

fn native_db_index_meta(db_path: &std::path::Path, fresh: bool) -> Value {
    json!({
        "used": true,
        "fresh": fresh,
        "source": "scip_native",
        "fallback": false,
        "path": db_path
    })
}

fn filter_and_limit(
    workspace: &Workspace,
    results: &mut Vec<OccurrenceResult>,
    opts: &ScanOptions,
) -> Result<()> {
    let allowed_paths = allowed_scan_paths(workspace, opts)?;
    results.retain(|r| allowed_paths.contains(&r.path));
    if opts.limit > 0 && results.len() > opts.limit {
        results.truncate(opts.limit);
    }
    Ok(())
}

fn filter_symbol_results(
    workspace: &Workspace,
    results: &mut Vec<SymbolResult>,
    opts: &ScanOptions,
) -> Result<()> {
    let allowed_paths = allowed_scan_paths(workspace, opts)?;
    results.retain(|r| allowed_paths.contains(&r.path));
    if opts.limit > 0 && results.len() > opts.limit {
        results.truncate(opts.limit);
    }
    Ok(())
}

fn allowed_scan_paths(workspace: &Workspace, opts: &ScanOptions) -> Result<HashSet<String>> {
    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    Ok(workspace
        .scan_catalog(&scan_opts)?
        .into_iter()
        .map(|file| file.path)
        .collect())
}

// ---------------------------------------------------------------------------
// LEGACY: Old occurrence.idx binary format (compatibility path)
// ---------------------------------------------------------------------------

fn query_precise(
    workspace: &Workspace,
    opts: &ScanOptions,
    matches: impl Fn(&PreciseOccurrenceRecord) -> bool,
) -> Result<Option<PreciseQueryOutput>> {
    let Some((records, index_meta)) = fresh_records(workspace, opts)? else {
        return Ok(None);
    };
    let mut results = Vec::new();
    for record in records.into_iter().filter(matches) {
        results.push(record_to_json(record));
        if opts.limit > 0 && results.len() >= opts.limit {
            break;
        }
    }
    Ok(Some(PreciseQueryOutput {
        results: Value::Array(results),
        index: index_meta,
    }))
}

fn fresh_records(
    workspace: &Workspace,
    opts: &ScanOptions,
) -> Result<Option<(Vec<PreciseOccurrenceRecord>, Value)>> {
    let root = index::scip_root(workspace);

    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    let allowed_paths = workspace
        .scan_files(&scan_opts)?
        .into_iter()
        .map(|file| file.path)
        .collect::<HashSet<_>>();

    if lancedb_store::is_available(&workspace.root) {
        if let Ok(store) = lancedb_store::LanceDbStore::open_or_create(&workspace.root) {
            if let Ok(lance_records) = store.read_scip_occurrences(&workspace.snapshot_id) {
                if !lance_records.is_empty() {
                    let converted = convert_scip_occurrences(lance_records);
                    let mut fresh_rows = Vec::new();
                    for record in converted {
                        if !allowed_paths.contains(&record.path) {
                            continue;
                        }
                        let hash = match current_file_hash(workspace, &record.path) {
                            Ok(hash) => hash,
                            Err(_) => return Ok(None),
                        };
                        if hash != record.file_hash {
                            return Ok(None);
                        }
                        fresh_rows.push(record);
                    }
                    return Ok(Some((
                        fresh_rows,
                        json!({
                            "used": true,
                            "fresh": true,
                            "source": "scip_json",
                            "storageBackend": "lancedb",
                            "fallback": false,
                            "path": lancedb_store::lancedb_root(&workspace.root)
                        }),
                    )));
                }
            }
        }
    }

    let path = root.join("occurrences.idx");
    if !path.exists() {
        return Ok(None);
    }

    let records = read_occurrences(&path)?;
    let mut fresh_records = Vec::new();
    for record in records {
        if !allowed_paths.contains(&record.path) {
            continue;
        }
        let hash = match current_file_hash(workspace, &record.path) {
            Ok(hash) => hash,
            Err(_) => return Ok(None),
        };
        if hash != record.file_hash {
            return Ok(None);
        }
        fresh_records.push(record);
    }

    Ok(Some((
        fresh_records,
        json!({
            "used": true,
            "fresh": true,
            "source": "scip_json",
            "fallback": true,
            "storageBackend": "idx_binary",
            "path": path
        }),
    )))
}

fn current_file_hash(workspace: &Workspace, path: &str) -> Result<String> {
    let content = fs::read(workspace.abs_path(path))?;
    Ok(format!("blake3:{}", blake3::hash(&content).to_hex()))
}

fn import_to_lancedb(workspace: &Workspace, records: &[PreciseOccurrenceRecord]) -> Result<()> {
    let store = lancedb_store::LanceDbStore::open_or_create(&workspace.root)
        .with_context(|| "failed to open LanceDB store")?;
    store
        .ensure_tables()
        .with_context(|| "failed to ensure LanceDB tables")?;
    let scip_records: Vec<lancedb_store::ScipOccurrence> = records
        .iter()
        .map(|r| lancedb_store::ScipOccurrence {
            snapshot_id: workspace.snapshot_id.clone(),
            symbol: r.symbol.clone(),
            file_path: r.path.clone(),
            language: r.language.clone(),
            name: r.name.clone(),
            kind: r.kind.clone(),
            role: r.role.clone(),
            range_start_line: r.range.start_line,
            range_start_col: r.range.start_column,
            range_end_line: r.range.end_line,
            range_end_col: r.range.end_column,
            is_definition: r.role == "definition",
            file_hash: r.file_hash.clone(),
            enclosing_symbol: None,
            producer: r.producer.clone(),
        })
        .collect();
    store
        .write_scip_occurrences(&workspace.snapshot_id, &scip_records)
        .with_context(|| "failed to write scip occurrences")?;
    Ok(())
}

fn convert_scip_occurrences(
    records: Vec<lancedb_store::ScipOccurrence>,
) -> Vec<PreciseOccurrenceRecord> {
    records
        .into_iter()
        .map(|r| PreciseOccurrenceRecord {
            path: r.file_path,
            language: r.language,
            symbol: r.symbol,
            name: r.name,
            kind: r.kind,
            role: r.role,
            range: PreciseRange {
                start_line: r.range_start_line,
                start_column: r.range_start_col,
                end_line: r.range_end_line,
                end_column: r.range_end_col,
            },
            file_hash: r.file_hash,
            producer: r.producer,
        })
        .collect()
}

fn scip_range(range: &[usize]) -> Option<PreciseRange> {
    match range {
        [start_line, start_col, end_col] => Some(PreciseRange {
            start_line: to_one_based_u32(*start_line)?,
            start_column: to_one_based_u32(*start_col)?,
            end_line: to_one_based_u32(*start_line)?,
            end_column: to_one_based_u32(*end_col)?,
        }),
        [start_line, start_col, end_line, end_col] => Some(PreciseRange {
            start_line: to_one_based_u32(*start_line)?,
            start_column: to_one_based_u32(*start_col)?,
            end_line: to_one_based_u32(*end_line)?,
            end_column: to_one_based_u32(*end_col)?,
        }),
        _ => None,
    }
}

fn display_name_from_symbol(symbol: &str) -> String {
    symbol
        .split(|ch: char| ch == '/' || ch == '#' || ch == '.' || ch.is_whitespace())
        .rfind(|part| !part.is_empty())
        .unwrap_or(symbol)
        .trim_end_matches("().")
        .to_string()
}

fn kind_to_string(kind: &Value) -> String {
    match kind {
        Value::String(value) => value.to_ascii_lowercase(),
        Value::Number(value) => value.to_string(),
        _ => String::new(),
    }
}

fn matches_identifier(record: &PreciseOccurrenceRecord, identifier: &str) -> bool {
    record.name == identifier
        || record.symbol == identifier
        || matches_bare_method_name(&record.name, identifier)
        || matches_bare_method_name(&record.symbol, identifier)
}

fn matches_bare_method_name(value: &str, identifier: &str) -> bool {
    if identifier.is_empty() || identifier.contains('(') {
        return false;
    }
    value
        .strip_prefix(identifier)
        .is_some_and(|suffix| suffix.starts_with('('))
}

fn record_to_json(record: PreciseOccurrenceRecord) -> Value {
    json!({
        "path": record.path,
        "name": record.name,
        "symbolName": record.name,
        "kind": record.kind,
        "symbol": record.symbol,
        "role": record.role,
        "language": record.language,
        "container": Value::Null,
        "range": {
            "start": { "line": record.range.start_line, "column": record.range.start_column },
            "end": { "line": record.range.end_line, "column": record.range.end_column }
        },
        "fileHash": record.file_hash,
        "producer": record.producer,
        "reliability": "precise_fact",
        "exact": true
    })
}

fn read_occurrences(path: &Path) -> Result<Vec<PreciseOccurrenceRecord>> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    read_magic(&mut file, OCCURRENCES_MAGIC)?;
    let count = read_u32(&mut file)? as usize;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let path = read_string(&mut file)?;
        let language = read_string(&mut file)?;
        let symbol = read_string(&mut file)?;
        let name = read_string(&mut file)?;
        let kind = read_string(&mut file)?;
        let role = read_string(&mut file)?;
        let range = PreciseRange {
            start_line: read_u32(&mut file)?,
            start_column: read_u32(&mut file)?,
            end_line: read_u32(&mut file)?,
            end_column: read_u32(&mut file)?,
        };
        let file_hash = read_string(&mut file)?;
        let producer = read_string(&mut file)?;
        records.push(PreciseOccurrenceRecord {
            path,
            language,
            symbol,
            name,
            kind,
            role,
            range,
            file_hash,
            producer,
        });
    }
    Ok(records)
}

fn to_one_based_u32(value: usize) -> Option<u32> {
    value.checked_add(1)?.try_into().ok()
}

fn read_magic(file: &mut File, expected: &[u8; 8]) -> Result<()> {
    let mut actual = [0u8; 8];
    file.read_exact(&mut actual)?;
    if &actual != expected {
        return Err(anyhow!("invalid SCIP occurrence magic"));
    }
    Ok(())
}

fn read_string(file: &mut File) -> Result<String> {
    let len = read_u32(file)? as usize;
    let mut bytes = vec![0u8; len];
    file.read_exact(&mut bytes)?;
    Ok(String::from_utf8(bytes)?)
}

fn read_u32(file: &mut File) -> Result<u32> {
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}
