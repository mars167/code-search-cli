use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::{anyhow, Context, Result};

use crate::workspace::{FileRecord, MAX_FILE_BYTES};

/// Maximum number of file records accepted from a remote docs index.
const MAX_INDEX_DOCS: usize = 2_000_000;
/// Maximum number of doc IDs in a single gram posting list.
const MAX_POSTING_IDS: usize = 2_000_000;
/// Maximum byte length of a string field read from an index file.
const MAX_STRING_BYTES: usize = 4096;

const DOCS_MAGIC: &[u8; 8] = b"CSDOCS1\0";
const GRAMS_MAGIC: &[u8; 8] = b"CSGRAM1\0";

pub fn write_docs(path: &Path, records: &[FileRecord]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(DOCS_MAGIC)?;
    write_u32(&mut file, records.len() as u32)?;
    for record in records {
        write_string(&mut file, &record.path)?;
        write_string(&mut file, &record.language)?;
        write_u64(&mut file, record.size)?;
        write_u128(&mut file, record.mtime_ms)?;
        write_string(&mut file, &record.hash)?;
    }
    Ok(())
}

pub fn write_grams(path: &Path, root: &Path, records: &[FileRecord]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut gram_index: BTreeMap<[u8; 3], Vec<u32>> = BTreeMap::new();
    for (doc_id, record) in records.iter().enumerate() {
        if record.size > MAX_FILE_BYTES {
            continue;
        }
        let bytes = match fs::read(root.join(&record.path)) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        for window in bytes.windows(3) {
            gram_index
                .entry([window[0], window[1], window[2]])
                .or_default()
                .push(doc_id as u32);
        }
    }
    for ids in gram_index.values_mut() {
        ids.sort_unstable();
        ids.dedup();
    }

    let mut file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(GRAMS_MAGIC)?;
    write_u32(&mut file, gram_index.len() as u32)?;
    for (gram, ids) in gram_index {
        file.write_all(&gram)?;
        write_u32(&mut file, ids.len() as u32)?;
        for id in ids {
            write_u32(&mut file, id)?;
        }
    }
    Ok(())
}

pub fn read_docs(path: &Path) -> Result<Vec<FileRecord>> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    read_magic(&mut file, DOCS_MAGIC)?;
    let count = read_u32(&mut file)? as usize;
    if count > MAX_INDEX_DOCS {
        return Err(anyhow!(
            "docs index count {} exceeds maximum {}",
            count,
            MAX_INDEX_DOCS
        ));
    }
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let path = read_string(&mut file)?;
        let language = read_string(&mut file)?;
        let size = read_u64(&mut file)?;
        let mtime_ms = read_u128(&mut file)?;
        let hash = read_string(&mut file)?;
        records.push(FileRecord {
            path,
            language,
            size,
            mtime_ms,
            mode: 0,
            hash,
        });
    }
    Ok(records)
}

pub fn candidate_ids(path: &Path, pattern: &str, mode: &str) -> Result<Option<HashSet<usize>>> {
    let Some(query_grams) = query_grams(pattern, mode) else {
        return Ok(None);
    };

    let postings = read_selected_grams(path, &query_grams)?;
    Ok(Some(intersect_postings(&query_grams, &postings)))
}

pub fn query_grams(pattern: &str, mode: &str) -> Option<HashSet<[u8; 3]>> {
    if mode != "literal" || pattern.len() < 3 {
        return None;
    }
    let query_grams = grams_for_bytes(pattern.as_bytes());
    (!query_grams.is_empty()).then_some(query_grams)
}

pub fn intersect_postings(
    query_grams: &HashSet<[u8; 3]>,
    postings: &BTreeMap<[u8; 3], Vec<usize>>,
) -> HashSet<usize> {
    let mut candidate: Option<HashSet<usize>> = None;
    for gram in query_grams {
        let Some(ids) = postings.get(gram) else {
            return HashSet::new();
        };
        let current = ids.iter().copied().collect::<HashSet<_>>();
        candidate = Some(match candidate {
            Some(existing) => existing.intersection(&current).copied().collect(),
            None => current,
        });
    }
    candidate.unwrap_or_default()
}

fn read_selected_grams(
    path: &Path,
    wanted: &HashSet<[u8; 3]>,
) -> Result<BTreeMap<[u8; 3], Vec<usize>>> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    read_magic(&mut file, GRAMS_MAGIC)?;
    let count = read_u32(&mut file)? as usize;
    let mut postings = BTreeMap::new();
    for _ in 0..count {
        let mut gram = [0u8; 3];
        file.read_exact(&mut gram)?;
        let ids_len = read_u32(&mut file)? as usize;
        if ids_len > MAX_POSTING_IDS {
            return Err(anyhow!(
                "gram posting count {} exceeds maximum {}",
                ids_len,
                MAX_POSTING_IDS
            ));
        }
        if wanted.contains(&gram) {
            let mut ids = Vec::with_capacity(ids_len);
            for _ in 0..ids_len {
                ids.push(read_u32(&mut file)? as usize);
            }
            postings.insert(gram, ids);
        } else {
            file.seek(SeekFrom::Current((ids_len * 4) as i64))?;
        }
    }
    Ok(postings)
}

fn grams_for_bytes(bytes: &[u8]) -> HashSet<[u8; 3]> {
    bytes
        .windows(3)
        .map(|window| [window[0], window[1], window[2]])
        .collect()
}

fn read_magic(file: &mut File, expected: &[u8; 8]) -> Result<()> {
    let mut actual = [0u8; 8];
    file.read_exact(&mut actual)?;
    if &actual != expected {
        return Err(anyhow!("invalid index magic"));
    }
    Ok(())
}

fn read_string(file: &mut File) -> Result<String> {
    let len = read_u32(file)? as usize;
    if len > MAX_STRING_BYTES {
        return Err(anyhow!(
            "string length {} exceeds maximum {}",
            len,
            MAX_STRING_BYTES
        ));
    }
    let mut bytes = vec![0u8; len];
    file.read_exact(&mut bytes)?;
    Ok(String::from_utf8(bytes)?)
}

fn read_u32(file: &mut File) -> Result<u32> {
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(file: &mut File) -> Result<u64> {
    let mut bytes = [0u8; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u128(file: &mut File) -> Result<u128> {
    let mut bytes = [0u8; 16];
    file.read_exact(&mut bytes)?;
    Ok(u128::from_le_bytes(bytes))
}

fn write_string(file: &mut File, value: &str) -> Result<()> {
    write_u32(file, value.len() as u32)?;
    file.write_all(value.as_bytes())?;
    Ok(())
}

fn write_u32(file: &mut File, value: u32) -> Result<()> {
    file.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u64(file: &mut File, value: u64) -> Result<()> {
    file.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u128(file: &mut File, value: u128) -> Result<()> {
    file.write_all(&value.to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn docs_and_grams_round_trip() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        fs::write(dir.path().join("b.txt"), "haystack\n").unwrap();
        let records = vec![
            FileRecord {
                path: "a.txt".to_string(),
                language: "text".to_string(),
                size: 7,
                mtime_ms: 1,
                mode: 0,
                hash: "blake3:a".to_string(),
            },
            FileRecord {
                path: "b.txt".to_string(),
                language: "text".to_string(),
                size: 9,
                mtime_ms: 2,
                mode: 0,
                hash: "blake3:b".to_string(),
            },
        ];

        let docs_path = dir.path().join("docs.idx");
        let grams_path = dir.path().join("grams.idx");
        write_docs(&docs_path, &records).unwrap();
        write_grams(&grams_path, dir.path(), &records).unwrap();

        let docs = read_docs(&docs_path).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].path, "a.txt");

        let ids = candidate_ids(&grams_path, "needle", "literal")
            .unwrap()
            .unwrap();
        assert!(ids.contains(&0));
        assert!(!ids.contains(&1));
    }
}
