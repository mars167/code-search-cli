use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::{anyhow, Context, Result};

use crate::workspace::FileRecord;

const DOCS_MAGIC: &[u8; 8] = b"CSDOCS1\0";
const GRAMS_MAGIC: &[u8; 8] = b"CSGRAM1\0";

pub fn read_docs(path: &Path) -> Result<Vec<FileRecord>> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    read_magic(&mut file, DOCS_MAGIC)?;
    let count = read_u32(&mut file)? as usize;
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
    if mode != "literal" || pattern.as_bytes().len() < 3 {
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
