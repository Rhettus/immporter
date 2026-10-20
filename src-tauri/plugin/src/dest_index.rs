//! Index of photos already in the destination library, so files that are
//! there already (imported by any tool) are recognised as duplicates.
//!
//! The walk is stat-only and incremental: unchanged files keep their cached
//! hash. Hashes are computed lazily when a candidate needs verifying, or for
//! everything on "Rebuild destination index".

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rayon::prelude::*;
use walkdir::WalkDir;

use crate::catalog::Catalog;
use crate::dedupe::{dest_hash, file_mtime};
use crate::metadata;

const BATCH: usize = 2000;

/// Walk `root`, updating `dest_files`. Calls `progress(files_seen)` per batch.
pub fn walk(catalog: &Catalog, root: &str, cancel: &AtomicBool, progress: impl Fn(u64)) -> rusqlite::Result<u64> {
    catalog.dest_begin_walk(root)?;
    let mut batch = Vec::with_capacity(BATCH);
    let mut seen = 0u64;
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'))
        .flatten()
    {
        if cancel.load(Ordering::Relaxed) {
            // Leave rows marked unseen in place; a later full walk fixes them.
            return Ok(seen);
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
        if metadata::kind_for_ext(&ext).is_none() {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        batch.push((
            path.to_string_lossy().to_string(),
            entry.file_name().to_string_lossy().to_string(),
            md.len(),
            file_mtime(&md),
        ));
        seen += 1;
        if batch.len() >= BATCH {
            catalog.dest_upsert_batch(root, &batch)?;
            batch.clear();
            progress(seen);
        }
    }
    catalog.dest_upsert_batch(root, &batch)?;
    catalog.dest_end_walk(root)?;
    progress(seen);
    Ok(seen)
}

/// Hash every unhashed file under `root`. Calls `progress(done, total)`.
pub fn hash_all(catalog: &Arc<Catalog>, root: &str, cancel: &AtomicBool, progress: impl Fn(u64, u64) + Sync) {
    let rows = catalog.dest_unhashed(root);
    let total = rows.len() as u64;
    let done = std::sync::atomic::AtomicU64::new(0);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(2).build().unwrap();
    pool.install(|| {
        rows.par_iter().for_each(|row| {
            if cancel.load(Ordering::Relaxed) || !Path::new(&row.path).exists() {
                return;
            }
            dest_hash(catalog, row);
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if d % 50 == 0 || d == total {
                progress(d, total);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_photos_only() {
        let d = tempfile::tempdir().unwrap();
        let lib = d.path().join("2026/October");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("P1.ORF"), b"raw").unwrap();
        std::fs::write(lib.join("P1.ORF.rrdata"), b"sidecar").unwrap();
        std::fs::write(lib.join(".hidden.jpg"), b"x").unwrap();
        let c = Arc::new(Catalog::open_in_memory().unwrap());
        let root = d.path().to_string_lossy().to_string();
        let n = walk(&c, &root, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(n, 1);
        let rows = c.dest_by_name_size(&root, "P1.ORF", 3);
        assert_eq!(rows.len(), 1);
        hash_all(&c, &root, &AtomicBool::new(false), |_, _| {});
        assert_eq!(
            c.dest_by_path(&rows[0].path).unwrap().blake3.unwrap(),
            blake3::hash(b"raw").to_hex().to_string()
        );
    }
}
