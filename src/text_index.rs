use std::{
    cmp,
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::{anyhow, Context, Result};

use crate::workspace::{FileRecord, Workspace};

// Magic for v2 formats (includes line offsets, TOC, etc.)
const DOCS_MAGIC_V2: &[u8; 8] = b"CSDOCS2\0";
const DOCS_MAGIC_V1: &[u8; 8] = b"CSDOCS1\0";
const PATHS_MAGIC_V2: &[u8; 8] = b"CSPATH2\0";
const PATHS_MAGIC_V1: &[u8; 8] = b"CSPATH1\0";
const GRAMS_MAGIC_V2: &[u8; 8] = b"CSGRAM2\0";
const GRAMS_MAGIC_V1: &[u8; 8] = b"CSGRAM1\0";

pub fn write(
    text_root: &Path,
    workspace: &Workspace,
    records: &[FileRecord],
    include_grams: bool,
) -> Result<()> {
    fs::create_dir_all(text_root)?;
    write_docs(&text_root.join("docs.idx"), workspace, records)?;
    write_paths(&text_root.join("paths.idx"), records)?;
    if include_grams {
        write_grams(&text_root.join("grams.idx"), workspace, records)?;
    } else {
        write_empty_grams(&text_root.join("grams.idx"))?;
    }
    Ok(())
}

pub fn read_docs(path: &Path) -> Result<Vec<FileRecord>> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut magic_buf = [0u8; 8];
    file.read_exact(&mut magic_buf)?;

    let (count, skip_line_offsets) = if &magic_buf == DOCS_MAGIC_V2 {
        (read_u32(&mut file)? as usize, true)
    } else if &magic_buf == DOCS_MAGIC_V1 {
        (read_u32(&mut file)? as usize, false)
    } else {
        return Err(anyhow!("invalid docs index magic"));
    };

    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let path = read_string(&mut file)?;
        let language = read_string(&mut file)?;
        let size = read_u64(&mut file)?;
        let mtime_ms = read_u128(&mut file)?;
        let hash = read_string(&mut file)?;
        if skip_line_offsets {
            // Skip line offset data for this record
            let line_count = read_u32(&mut file)? as usize;
            file.seek(SeekFrom::Current((line_count * 4) as i64))?;
        }
        records.push(FileRecord {
            path,
            language,
            size,
            mtime_ms,
            hash,
        });
    }
    Ok(records)
}

/// Read line byte offsets for a specific document.
/// Returns a Vec where each element is the byte offset of the start of a line in the file content.
pub fn read_doc_line_offsets(path: &Path, doc_id: usize) -> Result<Vec<u32>> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut magic_buf = [0u8; 8];
    file.read_exact(&mut magic_buf)?;
    if &magic_buf != DOCS_MAGIC_V2 {
        return Err(anyhow!(
            "line offsets unavailable: docs.idx is not v2 format"
        ));
    }
    let count = read_u32(&mut file)? as usize;
    if doc_id >= count {
        return Err(anyhow!(
            "doc_id {} out of bounds (count: {})",
            doc_id,
            count
        ));
    }

    // Skip over all doc records and their line offsets before the requested one
    for i in 0..=doc_id {
        let doc_path = read_string(&mut file)?;
        let _language = read_string(&mut file)?;
        let _size = read_u64(&mut file)?;
        let _mtime_ms = read_u128(&mut file)?;
        let _hash = read_string(&mut file)?;
        let line_count = read_u32(&mut file)? as usize;

        if i == doc_id {
            let mut offsets = Vec::with_capacity(line_count);
            for _ in 0..line_count {
                offsets.push(read_u32(&mut file)?);
            }
            return Ok(offsets);
        } else {
            file.seek(SeekFrom::Current((line_count * 4) as i64))?;
            let _ = doc_path; // suppress unused warning
        }
    }

    unreachable!()
}

/// Return candidate doc IDs for a text content search based on trigram index.
/// `mode`: "literal" or "regex".
/// For "literal", uses normal trigrams. For "regex", extracts mandatory trigrams.
pub fn candidate_ids(path: &Path, pattern: &str, mode: &str) -> Result<Option<HashSet<usize>>> {
    let query_grams = match mode {
        "literal" => {
            if pattern.as_bytes().len() < 3 {
                return Ok(None);
            }
            let grams = grams_for_bytes(pattern.as_bytes());
            if grams.is_empty() {
                return Ok(None);
            }
            grams
        }
        "regex" => {
            let grams = extract_regex_trigrams(pattern);
            if grams.is_empty() {
                return Ok(None);
            }
            grams
        }
        _ => return Ok(None),
    };

    let postings = read_selected_grams(path, &query_grams)?;
    intersect_postings(&query_grams, &postings)
}

/// Return candidate doc IDs for path-based search (files/find-path/glob).
/// Uses the path n-gram index stored in paths.idx.
pub fn candidate_path_ids(path: &Path, pattern: &str) -> Result<Option<HashSet<usize>>> {
    if pattern.is_empty() {
        return Ok(None);
    }
    // Extract path component trigrams from the pattern
    let query_grams = path_component_grams(pattern);
    if query_grams.is_empty() {
        return Ok(None);
    }
    let postings = read_path_grams(path, &query_grams)?;
    intersect_postings(&query_grams, &postings)
}

/// Intersect posting lists for a set of query grams.
fn intersect_postings(
    query_grams: &HashSet<[u8; 3]>,
    postings: &BTreeMap<[u8; 3], Vec<usize>>,
) -> Result<Option<HashSet<usize>>> {
    let mut candidate: Option<HashSet<usize>> = None;
    for gram in query_grams {
        let Some(ids) = postings.get(gram) else {
            return Ok(Some(HashSet::new()));
        };
        let current = ids.iter().copied().collect::<HashSet<_>>();
        candidate = Some(match candidate {
            Some(existing) => existing.intersection(&current).copied().collect(),
            None => current,
        });
    }
    Ok(candidate)
}

/// Extract mandatory trigrams from a regex pattern by finding contiguous literal substrings.
/// Returns the set of trigrams that MUST appear in any matching text.
fn extract_regex_trigrams(pattern: &str) -> HashSet<[u8; 3]> {
    let mut grams = HashSet::new();
    let mut current_literal = Vec::new();
    let mut escape = false;

    for ch in pattern.chars() {
        if escape {
            current_literal.push(ch as u8);
            escape = false;
            continue;
        }
        match ch {
            '\\' => {
                escape = true;
            }
            '.' | '*' | '+' | '?' | '|' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' => {
                // Regex meta character: flush current literal
                if !current_literal.is_empty() {
                    grams.extend(grams_for_bytes(&current_literal));
                    current_literal.clear();
                }
            }
            other => {
                current_literal.push(other as u8);
            }
        }
    }
    // Flush remaining
    if !current_literal.is_empty() {
        grams.extend(grams_for_bytes(&current_literal));
    }
    grams
}

// ---------------------------------------------------------------------------
// docs.idx  (v2 format: includes per-doc line-offset table)
// ---------------------------------------------------------------------------

fn write_docs(path: &Path, workspace: &Workspace, records: &[FileRecord]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(DOCS_MAGIC_V2)?;
    write_u32(&mut file, records.len() as u32)?;
    for record in records {
        write_string(&mut file, &record.path)?;
        write_string(&mut file, &record.language)?;
        write_u64(&mut file, record.size)?;
        write_u128(&mut file, record.mtime_ms)?;
        write_string(&mut file, &record.hash)?;

        // Compute and write line byte offsets
        let abs_path = workspace.abs_path(&record.path);
        let content = match fs::read(&abs_path) {
            Ok(c) => c,
            Err(_) => {
                write_u32(&mut file, 0)?; // line_count = 0
                continue;
            }
        };
        let line_offsets = compute_line_offsets(&content);
        write_u32(&mut file, line_offsets.len() as u32)?;
        for offset in &line_offsets {
            write_u32(&mut file, *offset)?;
        }
    }
    Ok(())
}

fn compute_line_offsets(content: &[u8]) -> Vec<u32> {
    let mut offsets = vec![0u32];
    for (i, &byte) in content.iter().enumerate() {
        if byte == b'\n' {
            offsets.push((i + 1) as u32);
        }
    }
    offsets
}

// ---------------------------------------------------------------------------
// paths.idx  (v2 format: includes path n-gram index for candidate search)
// ---------------------------------------------------------------------------

fn write_paths(path: &Path, records: &[FileRecord]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(PATHS_MAGIC_V2)?;
    write_u32(&mut file, records.len() as u32)?;

    // Write path records
    for record in records {
        write_string(&mut file, &record.path)?;
    }

    // Build path n-gram index from path components
    let mut index = BTreeMap::<[u8; 3], Vec<u32>>::new();
    for (doc_id, record) in records.iter().enumerate() {
        let trigrams = path_component_grams(&record.path);
        for gram in trigrams {
            index.entry(gram).or_default().push(doc_id as u32);
        }
    }

    // Write path gram index
    write_u32(&mut file, index.len() as u32)?;
    for (gram, mut ids) in index {
        ids.sort_unstable();
        ids.dedup();
        file.write_all(&gram)?;
        write_u32(&mut file, ids.len() as u32)?;
        for id in ids {
            write_u32(&mut file, id)?;
        }
    }
    Ok(())
}

/// Extract trigrams from path components.
/// Splits path by '/' and generates 3-grams from each component.
/// This allows substring matching on path segments.
fn path_component_grams(path_str: &str) -> HashSet<[u8; 3]> {
    let mut grams = HashSet::new();
    for component in path_str.split('/') {
        if component.is_empty() {
            continue;
        }
        let bytes = component.as_bytes();
        if bytes.len() < 3 {
            // For short components (< 3 chars), use the component itself as a "gram"
            // padded to 3 bytes so it's still searchable
            let mut pad = [0u8; 3];
            let len = cmp::min(bytes.len(), 3);
            pad[..len].copy_from_slice(&bytes[..len]);
            grams.insert(pad);
            continue;
        }
        grams.extend(grams_for_bytes(bytes));
    }
    grams
}

/// Read path n-gram postings from paths.idx.
fn read_path_grams(
    path: &Path,
    wanted: &HashSet<[u8; 3]>,
) -> Result<BTreeMap<[u8; 3], Vec<usize>>> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut magic_buf = [0u8; 8];
    file.read_exact(&mut magic_buf)?;

    let path_count = if &magic_buf == PATHS_MAGIC_V2 || &magic_buf == PATHS_MAGIC_V1 {
        read_u32(&mut file)? as usize
    } else {
        return Err(anyhow!("invalid paths index magic"));
    };

    // Skip path records
    for _ in 0..path_count {
        let plen = read_u32(&mut file)? as usize;
        file.seek(SeekFrom::Current(plen as i64))?;
    }

    // Read gram index (only in v2)
    let gram_count = if &magic_buf == PATHS_MAGIC_V2 {
        read_u32(&mut file)? as usize
    } else {
        return Ok(BTreeMap::new());
    };

    read_grams_posting_section(&mut file, gram_count, wanted)
}

// ---------------------------------------------------------------------------
// grams.idx  (v2 format: includes TOC at end for fast seek)
// ---------------------------------------------------------------------------

fn write_grams(path: &Path, workspace: &Workspace, records: &[FileRecord]) -> Result<()> {
    let mut index = BTreeMap::<[u8; 3], Vec<u32>>::new();
    for (doc_id, record) in records.iter().enumerate() {
        let bytes = match fs::read(workspace.abs_path(&record.path)) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        for gram in grams_for_bytes(&bytes) {
            index.entry(gram).or_default().push(doc_id as u32);
        }
    }

    let mut file = File::create(path)?;
    file.write_all(GRAMS_MAGIC_V2)?;
    write_u32(&mut file, index.len() as u32)?;

    // Write sorted gram data, recording TOC entries
    let mut toc_entries: Vec<([u8; 3], u64, u32)> = Vec::with_capacity(index.len());
    for (gram, mut ids) in index {
        ids.sort_unstable();
        ids.dedup();
        let offset = file.stream_position()?;
        file.write_all(&gram)?;
        let posting_count = ids.len() as u32;
        write_u32(&mut file, posting_count)?;
        for id in ids {
            write_u32(&mut file, id)?;
        }
        toc_entries.push((gram, offset, posting_count));
    }

    // Write TOC at end
    let toc_start = file.stream_position()?;
    write_u32(&mut file, toc_entries.len() as u32)?;
    for (gram, file_offset, posting_count) in &toc_entries {
        file.write_all(gram)?;
        write_u64(&mut file, *file_offset)?;
        write_u32(&mut file, *posting_count)?;
    }
    // Write footer: offset to TOC start
    write_u64(&mut file, toc_start)?;

    Ok(())
}

fn write_empty_grams(path: &Path) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(GRAMS_MAGIC_V2)?;
    write_u32(&mut file, 0)?;
    // Write empty TOC with footer
    let toc_start = file.stream_position()?;
    write_u32(&mut file, 0)?; // toc entry count = 0
    write_u64(&mut file, toc_start)?; // footer
    Ok(())
}

/// Seek to a specific gram using binary search on the TOC.
/// After this call, the file is positioned at the gram's [u8;3] + u32(posting_count) prefix.
fn seek_to_gram(file: &mut File, gram: [u8; 3]) -> Result<bool> {
    file.seek(SeekFrom::End(-8))?;
    let toc_start = read_u64(file)?;
    file.seek(SeekFrom::Start(toc_start))?;
    let toc_count = read_u32(file)? as usize;
    if toc_count == 0 {
        return Ok(false);
    }

    // Binary search TOC entries
    let toc_entry_size: u64 = 3 + 8 + 4; // gram(3) + offset(8) + count(4) = 15 bytes
    let toc_data_start = file.stream_position()?;
    let mut lo: i64 = 0;
    let mut hi: i64 = toc_count as i64 - 1;

    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let entry_offset = toc_data_start + (mid as u64) * toc_entry_size;
        file.seek(SeekFrom::Start(entry_offset))?;
        let mut entry_gram = [0u8; 3];
        file.read_exact(&mut entry_gram)?;
        match entry_gram.cmp(&gram) {
            cmp::Ordering::Equal => {
                let gram_offset = read_u64(file)?;
                let _posting_count = read_u32(file)?;
                file.seek(SeekFrom::Start(gram_offset))?;
                return Ok(true);
            }
            cmp::Ordering::Less => lo = mid + 1,
            cmp::Ordering::Greater => hi = mid - 1,
        }
    }
    Ok(false)
}

fn read_selected_grams(
    path: &Path,
    wanted: &HashSet<[u8; 3]>,
) -> Result<BTreeMap<[u8; 3], Vec<usize>>> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;

    let mut magic_buf = [0u8; 8];
    file.read_exact(&mut magic_buf)?;

    let has_toc = if &magic_buf == GRAMS_MAGIC_V2 {
        true
    } else if &magic_buf == GRAMS_MAGIC_V1 {
        false
    } else {
        return Err(anyhow!("invalid grams index magic"));
    };

    let gram_count = read_u32(&mut file)? as usize;

    if has_toc && wanted.len() < gram_count {
        // Use TOC for fast seek (beneficial when we want few grams from many)
        read_grams_via_toc(&mut file, wanted)
    } else {
        // Sequential scan (faster when wanting many or all grams)
        read_grams_sequential(&mut file, gram_count, wanted)
    }
}

/// Read specific grams using TOC for O(log N) seek.
fn read_grams_via_toc(
    file: &mut File,
    wanted: &HashSet<[u8; 3]>,
) -> Result<BTreeMap<[u8; 3], Vec<usize>>> {
    let mut postings = BTreeMap::new();
    for &gram in wanted {
        if !seek_to_gram(file, gram)? {
            continue;
        }
        // seek_to_gram positions at gram_offset (start of gram's [u8;3] data)
        let mut g = [0u8; 3];
        file.read_exact(&mut g)?;
        let ids_len = read_u32(file)? as usize;
        let mut ids = Vec::with_capacity(ids_len);
        for _ in 0..ids_len {
            ids.push(read_u32(file)? as usize);
        }
        postings.insert(g, ids);
    }
    Ok(postings)
}

/// Read grams by scanning the entire gram section sequentially.
fn read_grams_sequential(
    file: &mut File,
    gram_count: usize,
    wanted: &HashSet<[u8; 3]>,
) -> Result<BTreeMap<[u8; 3], Vec<usize>>> {
    read_grams_posting_section(file, gram_count, wanted)
}

/// Read a gram posting section (sequential scan) from the current file position.
fn read_grams_posting_section(
    file: &mut File,
    gram_count: usize,
    wanted: &HashSet<[u8; 3]>,
) -> Result<BTreeMap<[u8; 3], Vec<usize>>> {
    let mut postings = BTreeMap::new();
    for _ in 0..gram_count {
        let mut gram = [0u8; 3];
        file.read_exact(&mut gram)?;
        let ids_len = read_u32(file)? as usize;
        if wanted.contains(&gram) {
            let mut ids = Vec::with_capacity(ids_len);
            for _ in 0..ids_len {
                ids.push(read_u32(file)? as usize);
            }
            postings.insert(gram, ids);
        } else {
            file.seek(SeekFrom::Current((ids_len * 4) as i64))?;
        }
    }
    Ok(postings)
}

// ---------------------------------------------------------------------------
// Trigram helpers
// ---------------------------------------------------------------------------

fn grams_for_bytes(bytes: &[u8]) -> HashSet<[u8; 3]> {
    bytes
        .windows(3)
        .map(|window| [window[0], window[1], window[2]])
        .collect()
}

// ---------------------------------------------------------------------------
// Binary I/O helpers
// ---------------------------------------------------------------------------

fn write_string(file: &mut File, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    write_u32(file, bytes.len() as u32)?;
    file.write_all(bytes)?;
    Ok(())
}

fn read_string(file: &mut File) -> Result<String> {
    let len = read_u32(file)? as usize;
    let mut bytes = vec![0u8; len];
    file.read_exact(&mut bytes)?;
    Ok(String::from_utf8(bytes)?)
}

fn write_u32(file: &mut File, value: u32) -> Result<()> {
    file.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn read_u32(file: &mut File) -> Result<u32> {
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_u64(file: &mut File, value: u64) -> Result<()> {
    file.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn read_u64(file: &mut File) -> Result<u64> {
    let mut bytes = [0u8; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn write_u128(file: &mut File, value: u128) -> Result<()> {
    file.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn read_u128(file: &mut File) -> Result<u128> {
    let mut bytes = [0u8; 16];
    file.read_exact(&mut bytes)?;
    Ok(u128::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn make_test_file(path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = File::create(path)?;
        file.write_all(content.as_bytes())?;
        Ok(())
    }

    #[test]
    fn test_line_offsets() {
        let content = b"line1\nline2\n\nline4\n";
        let offsets = compute_line_offsets(content);
        assert_eq!(offsets, vec![0, 6, 12, 13, 19]);
        // line1: bytes 0-5, line2: 6-11, empty: 12, line4: 13-18
    }

    #[test]
    fn test_line_offsets_single_line() {
        let content = b"single line no newline";
        let offsets = compute_line_offsets(content);
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn test_line_offsets_empty() {
        let content = b"";
        let offsets = compute_line_offsets(content);
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn test_docs_line_offset_roundtrip() -> Result<()> {
        let dir = tempdir()?;
        let ws_root = dir.path().join("ws");
        fs::create_dir_all(&ws_root)?;
        make_test_file(&ws_root.join("a.txt"), "hello\nworld\n")?;
        make_test_file(&ws_root.join("b.txt"), "foo\nbar\nbaz\n")?;

        let workspace = Workspace {
            root: ws_root.clone(),
            git_root: None,
            head: None,
            dirty: false,
            staged_count: 0,
            worktree_count: 0,
            snapshot_id: "test".to_string(),
        };
        let records = vec![
            FileRecord {
                path: "a.txt".to_string(),
                language: "text".to_string(),
                size: 12,
                mtime_ms: 0,
                hash: "hash_a".to_string(),
            },
            FileRecord {
                path: "b.txt".to_string(),
                language: "text".to_string(),
                size: 12,
                mtime_ms: 0,
                hash: "hash_b".to_string(),
            },
        ];

        let docs_path = dir.path().join("docs.idx");
        write_docs(&docs_path, &workspace, &records)?;

        // Read line offsets for doc 0
        let offsets_a = read_doc_line_offsets(&docs_path, 0)?;
        assert_eq!(offsets_a, vec![0, 6, 12]); // "hello\n" (offset 0), "world\n" (offset 6), EOF (offset 12)

        let offsets_b = read_doc_line_offsets(&docs_path, 1)?;
        assert_eq!(offsets_b, vec![0, 4, 8, 12]); // "foo\n"(0), "bar\n"(4), "baz\n"(8), EOF(12)

        Ok(())
    }

    #[test]
    fn test_path_component_grams() {
        let grams = path_component_grams("src/text_index.rs");
        // "src" -> padded to [b's', b'r', b'c']
        // "text_index.rs" -> many trigrams including "tex", "ext", "xt_", etc.
        assert!(grams.contains(&[b's', b'r', b'c']));
        assert!(grams.contains(&[b't', b'e', b'x']));
        assert!(grams.contains(&[b'e', b'x', b't']));
        // ".rs" is 3 chars, so [b'.', b'r', b's'] is a gram from "text_index.rs"
        assert!(grams.contains(&[b'.', b'r', b's']));
        // "dex" is also a trigram from "text_index.rs"
        assert!(grams.contains(&[b'd', b'e', b'x']));
    }

    #[test]
    fn test_path_index_roundtrip() -> Result<()> {
        let dir = tempdir()?;
        let records = vec![
            FileRecord {
                path: "src/main.rs".to_string(),
                language: "rust".to_string(),
                size: 100,
                mtime_ms: 0,
                hash: "hash1".to_string(),
            },
            FileRecord {
                path: "src/lib.rs".to_string(),
                language: "rust".to_string(),
                size: 200,
                mtime_ms: 0,
                hash: "hash2".to_string(),
            },
            FileRecord {
                path: "tests/test.rs".to_string(),
                language: "rust".to_string(),
                size: 300,
                mtime_ms: 0,
                hash: "hash3".to_string(),
            },
        ];

        let path_idx = dir.path().join("paths.idx");
        write_paths(&path_idx, &records)?;

        // Search for "src" - should match docs 0 and 1
        let candidates = candidate_path_ids(&path_idx, "src")?;
        assert!(candidates.is_some());
        let candidates = candidates.unwrap();
        assert!(candidates.contains(&0));
        assert!(candidates.contains(&1));
        assert!(!candidates.contains(&2));

        // Search for "test" - should match doc 2
        let candidates = candidate_path_ids(&path_idx, "test")?;
        assert!(candidates.is_some());
        let candidates = candidates.unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(candidates.contains(&2));

        // Search for "main" - should match doc 0
        let candidates = candidate_path_ids(&path_idx, "main")?;
        assert!(candidates.is_some());
        let candidates = candidates.unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(candidates.contains(&0));

        // Search for nonexistent
        let candidates = candidate_path_ids(&path_idx, "nonexistent")?;
        assert!(candidates.is_some());
        assert!(candidates.unwrap().is_empty());

        Ok(())
    }

    #[test]
    fn test_regex_trigram_extraction() {
        let grams = extract_regex_trigrams("hello");
        // "hel", "ell", "llo"
        assert_eq!(grams.len(), 3);
        assert!(grams.contains(&[b'h', b'e', b'l']));
        assert!(grams.contains(&[b'e', b'l', b'l']));
        assert!(grams.contains(&[b'l', b'l', b'o']));

        // Pattern with regex meta chars
        let grams = extract_regex_trigrams(r"log\.\w+");
        // "log" -> "log". Only literal part is "log" (3 chars), and maybe "." after?
        // Actually: 'l', 'o', 'g', '\', '.', '\', 'w', '+'
        // With our escape handling: 'l','o','g' go to current_literal, then '\' sets escape, then '.' is escaped literal,
        // then '\' sets escape, then 'w' is escaped (becomes 'w' in current_literal), then '+' flushes.
        // Actually let me trace through more carefully:
        // ch='l' -> current_literal=[l]
        // ch='o' -> current_literal=[l,o]
        // ch='g' -> current_literal=[l,o,g]
        // ch='\' -> escape=true
        // ch='.' -> escape=false, current_literal=[l,o,g,.]
        // ch='\' -> escape=true
        // ch='w' -> escape=false, current_literal=[l,o,g,.,w]
        // ch='+' -> flush -> grams from "log.w": "log", "og.", "g.w"
        assert!(grams.contains(&[b'l', b'o', b'g']));

        // Pattern with alternation
        let grams = extract_regex_trigrams("(foo|bar)");
        // '(', flush empty, 'f','o','o' -> current, '|', flush "foo", 'b','a','r', ')', flush "bar"
        assert!(grams.contains(&[b'f', b'o', b'o']));
        assert!(grams.contains(&[b'b', b'a', b'r']));
    }

    #[test]
    fn test_regex_trigram_short_pattern() {
        let grams = extract_regex_trigrams("ab");
        assert!(grams.is_empty()); // < 3 characters

        let grams = extract_regex_trigrams(".*");
        assert!(grams.is_empty()); // no literal substrings >= 3
    }

    #[test]
    fn test_grams_index_v2_with_toc() -> Result<()> {
        let dir = tempdir()?;
        let ws_root = dir.path().join("ws");
        fs::create_dir_all(&ws_root)?;

        // Create a few files
        make_test_file(&ws_root.join("f1.txt"), "hello world")?;
        make_test_file(&ws_root.join("f2.txt"), "foo bar baz")?;

        let workspace = Workspace {
            root: ws_root.clone(),
            git_root: None,
            head: None,
            dirty: false,
            staged_count: 0,
            worktree_count: 0,
            snapshot_id: "test".to_string(),
        };
        let records = vec![
            FileRecord {
                path: "f1.txt".to_string(),
                language: "text".to_string(),
                size: 11,
                mtime_ms: 0,
                hash: "h1".to_string(),
            },
            FileRecord {
                path: "f2.txt".to_string(),
                language: "text".to_string(),
                size: 12,
                mtime_ms: 0,
                hash: "h2".to_string(),
            },
        ];

        let grams_path = dir.path().join("grams.idx");
        write_grams(&grams_path, &workspace, &records)?;

        // Read using the new TOC-based reader
        let gram = [b'h', b'e', b'l'];
        let mut wanted = HashSet::new();
        wanted.insert(gram);
        let postings = read_selected_grams(&grams_path, &wanted)?;
        assert!(postings.contains_key(&gram));
        assert_eq!(postings[&gram], vec![0]); // doc 0 has "hello"

        // Verify seek_to_gram works
        let mut file = File::open(&grams_path)?;
        let mut magic = [0u8; 8];
        file.read_exact(&mut magic)?;
        assert_eq!(&magic, GRAMS_MAGIC_V2);
        let found = seek_to_gram(&mut file, gram)?;
        assert!(found);

        Ok(())
    }

    #[test]
    fn test_read_docs_v2_and_v1_compat() -> Result<()> {
        let dir = tempdir()?;
        let ws_root = dir.path().join("ws");
        fs::create_dir_all(&ws_root)?;

        make_test_file(&ws_root.join("test.txt"), "content\n")?;

        let workspace = Workspace {
            root: ws_root.clone(),
            git_root: None,
            head: None,
            dirty: false,
            staged_count: 0,
            worktree_count: 0,
            snapshot_id: "test".to_string(),
        };
        let records = vec![FileRecord {
            path: "test.txt".to_string(),
            language: "text".to_string(),
            size: 8,
            mtime_ms: 0,
            hash: "hash".to_string(),
        }];

        // Write v2
        let docs_v2 = dir.path().join("docs_v2.idx");
        write_docs(&docs_v2, &workspace, &records)?;
        let read_v2 = read_docs(&docs_v2)?;
        assert_eq!(read_v2.len(), 1);
        assert_eq!(read_v2[0].path, "test.txt");

        // Write v1 (manually) and verify read_docs handles it
        let docs_v1 = dir.path().join("docs_v1.idx");
        {
            let mut f = File::create(&docs_v1)?;
            f.write_all(DOCS_MAGIC_V1)?;
            write_u32(&mut f, 1)?;
            write_string(&mut f, "test.txt")?;
            write_string(&mut f, "text")?;
            write_u64(&mut f, 8)?;
            write_u128(&mut f, 0)?;
            write_string(&mut f, "hash")?;
        }
        let read_v1 = read_docs(&docs_v1)?;
        assert_eq!(read_v1.len(), 1);
        assert_eq!(read_v1[0].path, "test.txt");

        Ok(())
    }
}
