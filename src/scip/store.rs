use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::scip_proto::proto;

const OCCURRENCE_DB_SCHEMA_VERSION: &str = "2";

/// A single occurrence result from the store.
#[derive(Clone, Debug)]
pub struct OccurrenceResult {
    pub symbol_key: String,
    pub path: String,
    pub language: String,
    pub symbol: String,
    pub name: String,
    pub kind: String,
    pub role: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub file_hash: String,
}

/// A symbol result from the store.
#[derive(Clone, Debug)]
pub struct SymbolResult {
    pub symbol_key: String,
    pub symbol: String,
    pub name: String,
    pub kind: String,
    pub language: String,
    pub path: String,
    pub role: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// Build the occurrence database from a native SCIP Index.
pub fn build_occurrences_db(
    scip_index: &proto::Index,
    db_path: &Path,
    snapshot: &str,
    root: &Path,
) -> Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Remove existing DB
    if db_path.exists() {
        std::fs::remove_file(db_path)?;
    }

    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open occurrence DB at {}", db_path.display()))?;

    create_schema(&conn)?;

    let mut inserted_symbols = 0usize;
    let mut inserted_occurrences = 0usize;

    let tx = conn.unchecked_transaction()?;

    for document in &scip_index.documents {
        let language = document.language.clone();
        let path = document.relative_path.clone();

        // Build symbol lookup: symbol string -> (display_name, kind)
        let symbols: HashMap<&str, (&str, &str)> = document
            .symbols
            .iter()
            .map(|sym| {
                let kind = kind_name(sym);
                (sym.symbol.as_str(), (sym.display_name.as_str(), kind))
            })
            .collect();

        for occurrence in &document.occurrences {
            if occurrence.symbol.is_empty() || occurrence.range.is_empty() {
                continue;
            }

            let range = match occurrence.range.len() {
                3 => (
                    occurrence.range[0],
                    occurrence.range[1],
                    occurrence.range[0],
                    occurrence.range[2],
                ),
                4 => (
                    occurrence.range[0],
                    occurrence.range[1],
                    occurrence.range[2],
                    occurrence.range[3],
                ),
                _ => continue,
            };

            let role = if occurrence.symbol_roles & 0x1 != 0 {
                "definition"
            } else {
                "reference"
            };

            let default_name = display_name_from_symbol(&occurrence.symbol);
            let (metadata_name, kind) = symbols
                .get(occurrence.symbol.as_str())
                .copied()
                .unwrap_or((default_name.as_str(), "symbol"));

            let display_name = if metadata_name.is_empty() {
                display_name_from_symbol(&occurrence.symbol)
            } else {
                metadata_name.to_string()
            };

            let symbol_key = symbol_key_for_document(&path, &occurrence.symbol);
            let symbol_id = upsert_symbol(
                &tx,
                &symbol_key,
                &occurrence.symbol,
                &display_name,
                kind,
                &language,
            )?;

            // Insert occurrence with 1-based positions
            tx.execute(
                "INSERT INTO occurrences \
                 (symbol_id, symbol, file_path, start_line, start_column, end_line, end_column, role, language, file_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    symbol_id,
                    occurrence.symbol,
                    path,
                    (range.0 + 1) as u32,
                    (range.1 + 1) as u32,
                    (range.2 + 1) as u32,
                    (range.3 + 1) as u32,
                    role,
                    language,
                    "", // file_hash is not in SCIP proto; verify from workspace instead
                ],
            )?;

            inserted_occurrences += 1;
        }
        inserted_symbols += document.symbols.len();
    }

    // Store per-file hashes for freshness validation
    {
        let mut seen_paths: HashSet<&str> = HashSet::new();
        for document in &scip_index.documents {
            if seen_paths.insert(&document.relative_path) {
                let abs_path = root.join(&document.relative_path);
                let hash = match fs::read(&abs_path) {
                    Ok(content) => format!("blake3:{}", blake3::hash(&content).to_hex()),
                    Err(_) => String::new(),
                };
                tx.execute(
                    "INSERT OR REPLACE INTO file_hashes (file_path, hash) VALUES (?1, ?2)",
                    params![document.relative_path, hash],
                )?;
            }
        }
    }

    // Store snapshot hash for freshness tracking
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params!["snapshot_hash", snapshot],
    )?;

    tx.commit()?;

    log_index_meta(db_path, snapshot, inserted_symbols, inserted_occurrences)?;

    Ok(())
}

/// Check if the occurrence DB is fresh for the given snapshot hash.
/// Verifies every stored file hash against current disk contents.
pub fn occurrence_db_fresh(db_path: &Path, snapshot: &str, root: &Path) -> bool {
    if !db_path.exists() {
        return false;
    }
    let Ok(conn) = Connection::open(db_path) else {
        return false;
    };
    schema_is_current(&conn).unwrap_or(false)
        && check_snapshot_hash(&conn, snapshot).unwrap_or(false)
        && all_file_hashes_match(&conn, root).unwrap_or(false)
}

/// Delete the occurrence DB (force rebuild).
pub fn invalidate_db(db_path: &Path) -> Result<()> {
    if db_path.exists() {
        std::fs::remove_file(db_path)?;
    }
    Ok(())
}

/// Query definitions for a given identifier name.
pub fn query_defs(db_path: &Path, identifier: &str) -> Result<Vec<OccurrenceResult>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open(db_path)?;
    let method_like = method_name_like_pattern(identifier);

    let mut stmt = conn.prepare(
        "SELECT s.symbol_key, o.file_path, o.language, o.symbol, s.name, s.kind, o.role, \
                o.start_line, o.start_column, o.end_line, o.end_column, o.file_hash \
         FROM occurrences o \
         JOIN symbols s ON o.symbol_id = s.id \
         WHERE o.role = 'definition' AND (\
            s.name = ?1 OR s.symbol = ?1 OR s.symbol_key = ?1 \
            OR (?2 IS NOT NULL AND s.name LIKE ?2 ESCAPE '\\') \
            OR (?2 IS NOT NULL AND s.symbol LIKE ?2 ESCAPE '\\')\
         ) \
         ORDER BY o.file_path, o.start_line, o.start_column",
    )?;

    let mut results = stmt
        .query_map(params![identifier, method_like], map_occurrence_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    dedup_occurrence_results(&mut results);

    Ok(results)
}

/// Query references for a given identifier name.
pub fn query_refs(db_path: &Path, identifier: &str) -> Result<Vec<OccurrenceResult>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open(db_path)?;
    let method_like = method_name_like_pattern(identifier);

    let mut stmt = conn.prepare(
        "SELECT s.symbol_key, o.file_path, o.language, o.symbol, s.name, s.kind, o.role, \
                o.start_line, o.start_column, o.end_line, o.end_column, o.file_hash \
         FROM occurrences o \
         JOIN symbols s ON o.symbol_id = s.id \
         WHERE o.role = 'reference' AND (\
            s.name = ?1 OR s.symbol = ?1 OR s.symbol_key = ?1 \
            OR (?2 IS NOT NULL AND s.name LIKE ?2 ESCAPE '\\') \
            OR (?2 IS NOT NULL AND s.symbol LIKE ?2 ESCAPE '\\')\
         ) \
         ORDER BY o.file_path, o.start_line, o.start_column",
    )?;

    let mut results = stmt
        .query_map(params![identifier, method_like], map_occurrence_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    dedup_occurrence_results(&mut results);

    Ok(results)
}

pub fn query_refs_by_symbol_key(db_path: &Path, symbol_key: &str) -> Result<Vec<OccurrenceResult>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open(db_path)?;

    let mut stmt = conn.prepare(
        "SELECT s.symbol_key, o.file_path, o.language, o.symbol, s.name, s.kind, o.role, \
                o.start_line, o.start_column, o.end_line, o.end_column, o.file_hash \
         FROM occurrences o \
         JOIN symbols s ON o.symbol_id = s.id \
         WHERE o.role = 'reference' AND s.symbol_key = ?1 \
         ORDER BY o.file_path, o.start_line",
    )?;

    let results = stmt
        .query_map(params![symbol_key], map_occurrence_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(results)
}

/// Query symbols with name containing the query string.
pub fn query_symbols(db_path: &Path, query: &str) -> Result<Vec<SymbolResult>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open(db_path)?;

    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.symbol_key, s.symbol, s.name, s.kind, s.language, \
                o.file_path, o.role, o.start_line, o.start_column, o.end_line, o.end_column \
         FROM symbols s \
         JOIN occurrences o ON o.symbol_id = s.id \
         WHERE o.role = 'definition' AND (s.name LIKE ?1 OR s.symbol LIKE ?1 OR s.symbol_key LIKE ?1) \
         ORDER BY s.kind, s.name, s.symbol_key, o.file_path",
    )?;

    let like_pattern = format!("%{}%", query);
    let results = stmt
        .query_map(params![like_pattern], |row| {
            Ok(SymbolResult {
                symbol_key: row.get(0)?,
                symbol: row.get(1)?,
                name: row.get(2)?,
                kind: row.get(3)?,
                language: row.get(4)?,
                path: row.get(5)?,
                role: row.get(6)?,
                start_line: row.get(7)?,
                start_column: row.get(8)?,
                end_line: row.get(9)?,
                end_column: row.get(10)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(results)
}

/// Convert an OccurrenceResult to JSON.
pub fn occurrence_to_json(result: &OccurrenceResult) -> Value {
    json!({
        "path": result.path,
        "name": result.name,
        "symbolName": result.name,
        "symbol": result.symbol,
        "kind": result.kind,
        "role": result.role,
        "language": result.language,
        "container": Value::Null,
        "range": {
            "start": { "line": result.start_line, "column": result.start_column },
            "end": { "line": result.end_line, "column": result.end_column }
        },
        "fileHash": result.file_hash,
        "producer": "scip",
        "reliability": "precise_fact",
        "exact": true
    })
}

/// Convert a SymbolResult to JSON.
pub fn symbol_to_json(result: &SymbolResult) -> Value {
    json!({
        "name": result.name,
        "symbolName": result.name,
        "symbol": result.symbol,
        "kind": result.kind,
        "language": result.language,
        "path": result.path,
        "role": result.role,
        "container": Value::Null,
        "range": {
            "start": { "line": result.start_line, "column": result.start_column },
            "end": { "line": result.end_line, "column": result.end_column }
        },
        "producer": "scip",
        "reliability": "precise_fact",
        "exact": true
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS symbols (
            id INTEGER PRIMARY KEY,
            symbol_key TEXT NOT NULL UNIQUE,
            symbol TEXT NOT NULL,
            name TEXT NOT NULL,
            kind TEXT NOT NULL DEFAULT '',
            language TEXT NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS occurrences (
            id INTEGER PRIMARY KEY,
            symbol_id INTEGER NOT NULL REFERENCES symbols(id),
            symbol TEXT NOT NULL,
            file_path TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            start_column INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            end_column INTEGER NOT NULL,
            role TEXT NOT NULL,
            language TEXT NOT NULL DEFAULT '',
            file_hash TEXT NOT NULL DEFAULT ''
        );

        CREATE INDEX IF NOT EXISTS idx_occurrences_symbol_id ON occurrences(symbol_id);
        CREATE INDEX IF NOT EXISTS idx_occurrences_role ON occurrences(role);
        CREATE INDEX IF NOT EXISTS idx_occurrences_symbol ON occurrences(symbol);
        CREATE INDEX IF NOT EXISTS idx_symbols_symbol_key ON symbols(symbol_key);
        CREATE INDEX IF NOT EXISTS idx_symbols_symbol ON symbols(symbol);
        CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);

        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS file_hashes (
            file_path TEXT PRIMARY KEY,
            hash TEXT NOT NULL
        );",
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![OCCURRENCE_DB_SCHEMA_VERSION],
    )?;
    Ok(())
}

fn check_snapshot_hash(conn: &Connection, snapshot: &str) -> Result<bool> {
    let stored: String = conn.query_row(
        "SELECT value FROM meta WHERE key = 'snapshot_hash'",
        [],
        |row| row.get(0),
    )?;
    Ok(stored == snapshot)
}

fn all_file_hashes_match(conn: &Connection, root: &Path) -> Result<bool> {
    let mut stmt = conn.prepare("SELECT file_path, hash FROM file_hashes ORDER BY file_path")?;
    let entries: Vec<(String, String)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if entries.is_empty() {
        return Ok(false);
    }
    for (file_path, stored_hash) in &entries {
        let abs_path = root.join(file_path);
        let current_hash = if abs_path.exists() {
            let content = fs::read(&abs_path)
                .with_context(|| format!("failed to read {} for freshness check", file_path))?;
            format!("blake3:{}", blake3::hash(&content).to_hex())
        } else {
            String::new()
        };
        if &current_hash != stored_hash {
            return Ok(false);
        }
    }
    Ok(true)
}

fn schema_is_current(conn: &Connection) -> Result<bool> {
    let schema_version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if schema_version.as_deref() != Some(OCCURRENCE_DB_SCHEMA_VERSION) {
        return Ok(false);
    }

    let symbol_columns = table_columns(conn, "symbols")?;
    let occurrence_columns = table_columns(conn, "occurrences")?;
    Ok(symbol_columns.contains("symbol_key")
        && symbol_columns.contains("symbol")
        && occurrence_columns.contains("symbol_id")
        && occurrence_columns.contains("symbol"))
}

fn table_columns(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<HashSet<_>, _>>()?;
    Ok(columns)
}

fn upsert_symbol(
    tx: &rusqlite::Transaction<'_>,
    symbol_key: &str,
    symbol: &str,
    name: &str,
    kind: &str,
    language: &str,
) -> Result<i64> {
    let existing = tx
        .prepare("SELECT id, name, kind, language FROM symbols WHERE symbol_key = ?1")?
        .query_row(params![symbol_key], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .optional()?;

    if let Some((id, current_name, current_kind, current_language)) = existing {
        let next_name = better_name(symbol, &current_name, name);
        let next_kind = better_kind(&current_kind, kind);
        let next_language = better_language(&current_language, language);
        if next_name != current_name
            || next_kind != current_kind
            || next_language != current_language
        {
            tx.execute(
                "UPDATE symbols SET name = ?1, kind = ?2, language = ?3 WHERE id = ?4",
                params![next_name, next_kind, next_language, id],
            )?;
        }
        return Ok(id);
    }

    tx.execute(
        "INSERT INTO symbols (symbol_key, symbol, name, kind, language) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![symbol_key, symbol, name, kind, language],
    )?;
    Ok(tx.last_insert_rowid())
}

fn better_name(symbol: &str, current: &str, candidate: &str) -> String {
    let fallback = display_name_from_symbol(symbol);
    if !candidate.is_empty() && (current.is_empty() || current == fallback) {
        candidate.to_string()
    } else {
        current.to_string()
    }
}

fn better_kind(current: &str, candidate: &str) -> String {
    if !candidate.is_empty() && (current.is_empty() || current == "symbol") {
        candidate.to_string()
    } else {
        current.to_string()
    }
}

fn better_language(current: &str, candidate: &str) -> String {
    if !candidate.is_empty() && current.is_empty() {
        candidate.to_string()
    } else {
        current.to_string()
    }
}

fn log_index_meta(
    db_path: &Path,
    snapshot: &str,
    symbol_count: usize,
    occurrence_count: usize,
) -> Result<()> {
    let manifest_path = db_path.with_file_name("manifest.json");
    let value = json!({
        "source": "scip_native",
        "snapshot": snapshot,
        "dbPath": db_path.to_string_lossy(),
        "symbolCount": symbol_count,
        "occurrenceCount": occurrence_count,
    });
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}

fn map_occurrence_row(
    row: &rusqlite::Row<'_>,
) -> std::result::Result<OccurrenceResult, rusqlite::Error> {
    Ok(OccurrenceResult {
        symbol_key: row.get(0)?,
        path: row.get(1)?,
        language: row.get(2)?,
        symbol: row.get(3)?,
        name: row.get(4)?,
        kind: row.get(5)?,
        role: row.get(6)?,
        start_line: row.get(7)?,
        start_column: row.get(8)?,
        end_line: row.get(9)?,
        end_column: row.get(10)?,
        file_hash: row.get(11)?,
    })
}

fn dedup_occurrence_results(results: &mut Vec<OccurrenceResult>) {
    results.dedup_by(|a, b| {
        a.path == b.path
            && a.name == b.name
            && a.role == b.role
            && a.start_line == b.start_line
            && a.start_column == b.start_column
            && a.end_line == b.end_line
            && a.end_column == b.end_column
    });
}

fn symbol_key_for_document(path: &str, symbol: &str) -> String {
    if symbol.starts_with("local ") {
        format!("{path}:{symbol}")
    } else {
        symbol.to_string()
    }
}

fn kind_name(sym: &proto::SymbolInformation) -> &str {
    match proto::symbol_information::Kind::try_from(sym.kind) {
        Ok(proto::symbol_information::Kind::Function) => "function",
        Ok(proto::symbol_information::Kind::Method) => "method",
        Ok(proto::symbol_information::Kind::Struct) => "struct",
        Ok(proto::symbol_information::Kind::Class) => "class",
        Ok(proto::symbol_information::Kind::Interface) => "interface",
        Ok(proto::symbol_information::Kind::Enum) => "enum",
        Ok(proto::symbol_information::Kind::Trait) => "trait",
        Ok(proto::symbol_information::Kind::TypeAlias) => "type_alias",
        Ok(proto::symbol_information::Kind::Module) => "module",
        Ok(proto::symbol_information::Kind::Constant) => "constant",
        Ok(proto::symbol_information::Kind::Variable) => "variable",
        Ok(proto::symbol_information::Kind::Field) => "field",
        Ok(proto::symbol_information::Kind::TypeParameter) => "type_parameter",
        Ok(proto::symbol_information::Kind::Parameter) => "parameter",
        Ok(proto::symbol_information::Kind::Property) => "property",
        Ok(proto::symbol_information::Kind::Constructor) => "constructor",
        _ => "symbol",
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

fn method_name_like_pattern(identifier: &str) -> Option<String> {
    if identifier.is_empty() || identifier.contains('(') {
        return None;
    }
    Some(format!("{}(%", escape_sql_like(identifier)))
}

fn escape_sql_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scip_proto::proto;
    use tempfile::tempdir;

    fn build_test_index() -> proto::Index {
        proto::Index {
            metadata: Some(proto::Metadata {
                version: proto::ProtocolVersion::UnspecifiedProtocolVersion as i32,
                tool_info: Some(proto::ToolInfo {
                    name: "test".to_string(),
                    version: "0.1.0".to_string(),
                    arguments: vec![],
                }),
                project_root: "file:///test".to_string(),
                text_document_encoding: proto::TextEncoding::Utf8 as i32,
            }),
            documents: vec![proto::Document {
                language: "rust".to_string(),
                relative_path: "src/lib.rs".to_string(),
                occurrences: vec![
                    proto::Occurrence {
                        range: vec![0, 3, 0, 9],
                        symbol: "local 1".to_string(),
                        symbol_roles: 1,
                        ..Default::default()
                    },
                    proto::Occurrence {
                        range: vec![1, 12, 1, 18],
                        symbol: "local 1".to_string(),
                        symbol_roles: 0,
                        ..Default::default()
                    },
                ],
                symbols: vec![proto::SymbolInformation {
                    symbol: "local 1".to_string(),
                    kind: proto::symbol_information::Kind::Function as i32,
                    display_name: "needle".to_string(),
                    ..Default::default()
                }],
                position_encoding: proto::PositionEncoding::Utf8CodeUnitOffsetFromLineStart as i32,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn build_and_query_full_cycle() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("occurrences.db");

        let index = build_test_index();
        build_occurrences_db(&index, &db_path, "snapshot-v1", dir.path()).unwrap();

        assert!(occurrence_db_fresh(&db_path, "snapshot-v1", dir.path()));
        assert!(!occurrence_db_fresh(&db_path, "snapshot-v2", dir.path()));

        // defs
        let defs = query_defs(&db_path, "needle").unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "needle");
        assert_eq!(defs[0].symbol, "local 1");
        assert_eq!(defs[0].role, "definition");
        assert_eq!(defs[0].path, "src/lib.rs");
        assert_eq!(defs[0].start_line, 1);
        assert_eq!(defs[0].start_column, 4);
        assert_eq!(query_defs(&db_path, "local 1").unwrap().len(), 1);

        // refs
        let refs = query_refs(&db_path, "needle").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].role, "reference");
        assert_eq!(refs[0].start_line, 2);
        assert_eq!(query_refs(&db_path, "local 1").unwrap().len(), 1);

        // symbols
        let symbols = query_symbols(&db_path, "needle").unwrap();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].symbol, "local 1");
        assert_eq!(symbols[0].name, "needle");

        // unknown identifier
        let no_defs = query_defs(&db_path, "nonexistent").unwrap();
        assert!(no_defs.is_empty());

        // JSON output
        let json = occurrence_to_json(&defs[0]);
        assert_eq!(json["reliability"], "precise_fact");
        assert_eq!(json["exact"], true);
        assert_eq!(json["producer"], "scip");
    }

    #[test]
    fn bare_method_name_matches_signature_display_names() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("occurrences.db");
        let index = proto::Index {
            documents: vec![proto::Document {
                language: "java".to_string(),
                relative_path: "src/UserService.java".to_string(),
                occurrences: vec![
                    proto::Occurrence {
                        range: vec![2, 16, 2, 30],
                        symbol: "semanticdb maven . . UserService#selectUserById().".to_string(),
                        symbol_roles: 1,
                        ..Default::default()
                    },
                    proto::Occurrence {
                        range: vec![8, 22, 8, 36],
                        symbol: "semanticdb maven . . UserService#selectUserById().".to_string(),
                        symbol_roles: 0,
                        ..Default::default()
                    },
                ],
                symbols: vec![proto::SymbolInformation {
                    symbol: "semanticdb maven . . UserService#selectUserById().".to_string(),
                    kind: proto::symbol_information::Kind::Method as i32,
                    display_name: "selectUserById(Long)".to_string(),
                    ..Default::default()
                }],
                position_encoding: proto::PositionEncoding::Utf8CodeUnitOffsetFromLineStart as i32,
                ..Default::default()
            }],
            ..Default::default()
        };

        build_occurrences_db(&index, &db_path, "snapshot-v1", dir.path()).unwrap();

        let defs = query_defs(&db_path, "selectUserById").unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "selectUserById(Long)");

        let refs = query_refs(&db_path, "selectUserById").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].role, "reference");

        assert_eq!(
            query_defs(&db_path, "selectUserById(Long)").unwrap().len(),
            1
        );
        assert!(query_defs(&db_path, "selectUser").unwrap().is_empty());
    }

    #[test]
    fn freshness_detects_hash_mismatch() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("occurrences.db");

        let index = build_test_index();
        build_occurrences_db(&index, &db_path, "commit:abc123", dir.path()).unwrap();

        assert!(occurrence_db_fresh(&db_path, "commit:abc123", dir.path()));
        assert!(!occurrence_db_fresh(
            &db_path,
            "worktree:abc123",
            dir.path()
        ));

        let nonexistent = dir.path().join("nonexistent.db");
        assert!(!occurrence_db_fresh(&nonexistent, "any", dir.path()));
    }

    #[test]
    fn freshness_checks_every_stored_file_hash() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("occurrences.db");

        let mut documents = Vec::new();
        for index in 0..8 {
            let relative_path = format!("src/file_{index}.rs");
            let absolute_path = dir.path().join(&relative_path);
            std::fs::create_dir_all(absolute_path.parent().unwrap()).unwrap();
            std::fs::write(&absolute_path, format!("fn item_{index}() {{}}\n")).unwrap();
            documents.push(proto::Document {
                language: "rust".to_string(),
                relative_path,
                occurrences: vec![proto::Occurrence {
                    range: vec![0, 3, 0, 9],
                    symbol: format!("local {index}"),
                    symbol_roles: 1,
                    ..Default::default()
                }],
                symbols: vec![proto::SymbolInformation {
                    symbol: format!("local {index}"),
                    kind: proto::symbol_information::Kind::Function as i32,
                    display_name: format!("item_{index}"),
                    ..Default::default()
                }],
                position_encoding: proto::PositionEncoding::Utf8CodeUnitOffsetFromLineStart as i32,
                ..Default::default()
            });
        }

        let index = proto::Index {
            documents,
            ..Default::default()
        };
        build_occurrences_db(&index, &db_path, "snapshot-v1", dir.path()).unwrap();
        assert!(occurrence_db_fresh(&db_path, "snapshot-v1", dir.path()));

        std::fs::write(dir.path().join("src/file_7.rs"), "fn changed() {}\n").unwrap();

        assert!(!occurrence_db_fresh(&db_path, "snapshot-v1", dir.path()));
    }

    #[test]
    fn freshness_rejects_old_schema_without_scoped_symbol_identity() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("occurrences.db");
        let source_path = dir.path().join("src/lib.rs");
        std::fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        std::fs::write(&source_path, "fn needle() {}\n").unwrap();
        let source_hash = format!(
            "blake3:{}",
            blake3::hash(&std::fs::read(&source_path).unwrap()).to_hex()
        );

        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE symbols (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT '',
                language TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE occurrences (
                id INTEGER PRIMARY KEY,
                symbol_id INTEGER NOT NULL REFERENCES symbols(id),
                symbol TEXT NOT NULL,
                file_path TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                start_column INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                end_column INTEGER NOT NULL,
                role TEXT NOT NULL,
                language TEXT NOT NULL DEFAULT '',
                file_hash TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE file_hashes (
                file_path TEXT PRIMARY KEY,
                hash TEXT NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('snapshot_hash', 'snapshot-v1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_hashes (file_path, hash) VALUES ('src/lib.rs', ?1)",
            params![source_hash],
        )
        .unwrap();

        assert!(!occurrence_db_fresh(&db_path, "snapshot-v1", dir.path()));
    }

    #[test]
    fn local_symbols_are_scoped_by_document_path() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("occurrences.db");
        for (path, body) in [
            ("src/alpha.rs", "fn alpha() {}\n"),
            ("src/beta.rs", "fn beta() {}\n"),
        ] {
            let absolute_path = dir.path().join(path);
            std::fs::create_dir_all(absolute_path.parent().unwrap()).unwrap();
            std::fs::write(absolute_path, body).unwrap();
        }

        let index = proto::Index {
            documents: vec![
                proto::Document {
                    language: "rust".to_string(),
                    relative_path: "src/alpha.rs".to_string(),
                    occurrences: vec![proto::Occurrence {
                        range: vec![0, 3, 0, 8],
                        symbol: "local 1".to_string(),
                        symbol_roles: 1,
                        ..Default::default()
                    }],
                    symbols: vec![proto::SymbolInformation {
                        symbol: "local 1".to_string(),
                        kind: proto::symbol_information::Kind::Function as i32,
                        display_name: "alpha".to_string(),
                        ..Default::default()
                    }],
                    position_encoding: proto::PositionEncoding::Utf8CodeUnitOffsetFromLineStart
                        as i32,
                    ..Default::default()
                },
                proto::Document {
                    language: "rust".to_string(),
                    relative_path: "src/beta.rs".to_string(),
                    occurrences: vec![proto::Occurrence {
                        range: vec![0, 3, 0, 7],
                        symbol: "local 1".to_string(),
                        symbol_roles: 1,
                        ..Default::default()
                    }],
                    symbols: vec![proto::SymbolInformation {
                        symbol: "local 1".to_string(),
                        kind: proto::symbol_information::Kind::Function as i32,
                        display_name: "beta".to_string(),
                        ..Default::default()
                    }],
                    position_encoding: proto::PositionEncoding::Utf8CodeUnitOffsetFromLineStart
                        as i32,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        build_occurrences_db(&index, &db_path, "snapshot-v1", dir.path()).unwrap();

        let alpha_defs = query_defs(&db_path, "alpha").unwrap();
        assert_eq!(alpha_defs.len(), 1);
        assert_eq!(alpha_defs[0].path, "src/alpha.rs");
        assert_eq!(alpha_defs[0].name, "alpha");

        let beta_defs = query_defs(&db_path, "beta").unwrap();
        assert_eq!(beta_defs.len(), 1);
        assert_eq!(beta_defs[0].path, "src/beta.rs");
        assert_eq!(beta_defs[0].name, "beta");
    }

    #[test]
    fn defs_returns_empty_for_missing_db() {
        let dir = tempdir().unwrap();
        let nonexistent = dir.path().join("no-such.db");
        assert!(query_defs(&nonexistent, "anything").unwrap().is_empty());
        assert!(query_refs(&nonexistent, "anything").unwrap().is_empty());
        assert!(query_symbols(&nonexistent, "anything").unwrap().is_empty());
    }
}
