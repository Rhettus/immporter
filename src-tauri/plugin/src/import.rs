//! Verified import: copy → fsync → re-read and hash → atomic no-clobber
//! rename → catalog. Writes only ever target the destination; the source is
//! opened exclusively through `readonly`.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::catalog::{Catalog, NewImported};
use crate::dedupe::file_mtime;
use crate::metadata::PhotoMeta;
use crate::readonly::{self, HASH_BUF};
use crate::scan::{FileStatus, Session, SourceFile};
use crate::template;

const MAX_ATTEMPTS: u32 = 3;
const WORKERS: usize = 2;
pub const TEMP_SUFFIX: &str = ".importing";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportOpts {
    pub dest_root: String,
    pub folder_tpl: String,
    pub name_tpl: String,
    pub item_ids: Vec<u32>,
    /// Re-read the source a second time and compare hashes.
    pub paranoid: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Imported { dest: String, hash: String },
    AlreadyPresent { dest: String, hash: String },
    Skipped { reason: String },
    Failed { error: String },
}

/// Test hook: corrupt the bytes written on a given attempt.
type Fault<'a> = Option<&'a (dyn Fn(u32, &mut [u8]) + Sync)>;

pub struct FileJob<'a> {
    pub src: &'a SourceFile,
    /// Planned path relative to the destination root (before suffixes).
    pub rel: PathBuf,
    /// Hash computed earlier (during duplicate verification), if any.
    pub known_hash: Option<String>,
}

fn temp_path_for(final_path: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(final_path.file_name().unwrap_or_default());
    name.push(TEMP_SUFFIX);
    final_path.with_file_name(name)
}

fn hex(h: blake3::Hash) -> String {
    h.to_hex().to_string()
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Rename that fails with `AlreadyExists` instead of replacing `to`.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let f = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let t = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        if unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        // Some filesystems (e.g. exFAT, SMB) don't support RENAME_EXCL.
        if !matches!(err.raw_os_error(), Some(libc::ENOTSUP) | Some(libc::EINVAL)) {
            return Err(err);
        }
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let f = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let t = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        let r = unsafe { libc::renameat2(libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_NOREPLACE) };
        if r == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if !matches!(err.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EINVAL)) {
            return Err(err);
        }
    }
    // Portable fallback: a hard link fails if the target exists.
    match fs::hard_link(from, to) {
        Ok(()) => fs::remove_file(from),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            // No hard links (FAT/exFAT destinations): check, then rename.
            if fs::symlink_metadata(to).is_ok() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            fs::rename(from, to)
        }
    }
}

/// Copy `src` into a new temp file, hashing the bytes as they are read.
fn copy_hashing(src: &Path, temp: &Path, nocache: bool, attempt: u32, fault: Fault, on_bytes: &dyn Fn(u64)) -> io::Result<(String, u64)> {
    let mut input = readonly::open_read(src, nocache)?;
    let mut out = OpenOptions::new().write(true).create_new(true).open(temp)?;
    // Keep written pages out of the cache so the verify read hits the disk.
    readonly::drop_cache(&out);
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_BUF];
    let mut total = 0u64;
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        if let Some(f) = fault {
            f(attempt, &mut buf[..n]);
        }
        out.write_all(&buf[..n])?;
        total += n as u64;
        on_bytes(n as u64);
    }
    out.sync_all()?;
    drop(out);
    // The page cache may still hold the pages on Linux; drop them post-fsync.
    readonly::drop_cache(&File::open(temp)?);
    Ok((hex(hasher.finalize()), total))
}

fn copy_times(src_md: &fs::Metadata, dest: &Path) -> io::Result<()> {
    let mtime = filetime::FileTime::from_last_modification_time(src_md);
    let atime = filetime::FileTime::from_last_access_time(src_md);
    filetime::set_file_times(dest, atime, mtime)
}

/// Import one file. Returns the outcome; never touches the source except to read it.
pub fn import_file(
    catalog: &Catalog,
    session_id: i64,
    root: &Path,
    job: &FileJob,
    meta: &PhotoMeta,
    paranoid: bool,
    fault: Fault,
    on_bytes: &dyn Fn(u64),
) -> Outcome {
    let src = job.src;
    let planned = root.join(&job.rel);
    let dir = planned.parent().unwrap_or(root).to_path_buf();
    if let Err(e) = fs::create_dir_all(&dir) {
        return Outcome::Failed { error: format!("Cannot create {}: {e}", dir.display()) };
    }
    let stem = planned.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = planned.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let src_md = match fs::metadata(&src.path) {
        Ok(m) => m,
        Err(e) => return Outcome::Failed { error: format!("Cannot read source: {e}") },
    };
    if src_md.len() != src.size {
        return Outcome::Failed { error: "Source file changed size since scanning".into() };
    }
    let mut known_hash = job.known_hash.clone();
    let source_hash = |known: &mut Option<String>| -> io::Result<String> {
        if let Some(h) = known.clone() {
            return Ok(h);
        }
        let h = hex(readonly::hash_file(&src.path, paranoid)?.0);
        *known = Some(h.clone());
        Ok(h)
    };

    let mut suffix = 0u32;
    let mut journal: Option<i64> = None;
    let mut attempt = 0u32;
    let mut last_err = String::new();
    while attempt < MAX_ATTEMPTS {
        let final_path = dir.join(template::with_suffix(&stem, &ext, suffix));
        let final_str = final_path.to_string_lossy().to_string();

        // An existing file at the target: same content → already present;
        // different content → try the next suffix.
        if let Ok(existing) = fs::symlink_metadata(&final_path) {
            if existing.is_file() && existing.len() == src.size {
                let sh = match source_hash(&mut known_hash) {
                    Ok(h) => h,
                    Err(e) => return Outcome::Failed { error: format!("Cannot read source: {e}") },
                };
                if let Ok((dh, _)) = readonly::hash_file(&final_path, true) {
                    if hex(dh) == sh {
                        let _ = catalog.record_imported(&NewImported {
                            orig_name: &src.name,
                            size: src.size,
                            meta,
                            blake3: &sh,
                            dest_path: &final_str,
                        });
                        return Outcome::AlreadyPresent { dest: final_str, hash: sh };
                    }
                }
            }
            suffix += 1;
            continue;
        }

        let temp = temp_path_for(&final_path);
        let temp_str = temp.to_string_lossy().to_string();
        let jid = match journal {
            Some(id) => id,
            None => match catalog.item_start(session_id, &src.path.to_string_lossy(), &final_str, &temp_str) {
                Ok(id) => *journal.insert(id),
                Err(e) => return Outcome::Failed { error: format!("Journal error: {e}") },
            },
        };
        catalog.item_set_paths(jid, &final_str, &temp_str);
        attempt += 1;

        let result = (|| -> Result<String, String> {
            let (stream_hash, n) = match copy_hashing(&src.path, &temp, paranoid, attempt, fault, on_bytes) {
                Ok(v) => v,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err("temp-exists".into()),
                Err(e) => return Err(format!("Copy failed: {e}")),
            };
            if n != src.size {
                return Err(format!("Short read: {n} of {} bytes", src.size));
            }
            copy_times(&src_md, &temp).map_err(|e| format!("Setting file times failed: {e}"))?;
            fsync_dir(&dir).map_err(|e| format!("fsync failed: {e}"))?;
            let (dest_hash, dn) = readonly::hash_file(&temp, true).map_err(|e| format!("Verify read failed: {e}"))?;
            if dn != src.size || hex(dest_hash) != stream_hash {
                return Err("Verification failed: destination does not match source".into());
            }
            if paranoid {
                let (again, _) = readonly::hash_file(&src.path, true).map_err(|e| format!("Source re-read failed: {e}"))?;
                if hex(again) != stream_hash {
                    return Err("Verification failed: source read inconsistently (card read error?)".into());
                }
            }
            if let Some(k) = &known_hash {
                if *k != stream_hash {
                    return Err("Verification failed: source hash differs from earlier read".into());
                }
            }
            Ok(stream_hash)
        })();

        match result {
            Ok(hash) => match rename_noreplace(&temp, &final_path) {
                Ok(()) => {
                    let _ = fsync_dir(&dir);
                    let _ = catalog.record_imported(&NewImported {
                        orig_name: &src.name,
                        size: src.size,
                        meta,
                        blake3: &hash,
                        dest_path: &final_str,
                    });
                    if let Ok(md) = fs::metadata(&final_path) {
                        catalog.dest_insert_verified(
                            &root.to_string_lossy(),
                            &final_str,
                            &final_path.file_name().unwrap_or_default().to_string_lossy(),
                            md.len(),
                            file_mtime(&md),
                            &hash,
                        );
                    }
                    catalog.item_update(jid, "done", Some(&final_str), Some(&hash), None);
                    return Outcome::Imported { dest: final_str, hash };
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // Someone took the name meanwhile (e.g. a parallel worker); retry with the next suffix.
                    let _ = fs::remove_file(&temp);
                    attempt -= 1;
                    suffix += 1;
                }
                Err(e) => {
                    let _ = fs::remove_file(&temp);
                    last_err = format!("Rename failed: {e}");
                    catalog.item_update(jid, "retrying", None, None, Some(&last_err));
                }
            },
            Err(e) if e == "temp-exists" => {
                // A parallel worker is writing the same name; take the next one.
                attempt -= 1;
                suffix += 1;
            }
            Err(e) => {
                let _ = fs::remove_file(&temp);
                last_err = e;
                catalog.item_update(jid, "retrying", None, None, Some(&last_err));
            }
        }
    }
    if let Some(jid) = journal {
        catalog.item_update(jid, "failed", None, None, Some(&last_err));
    }
    Outcome::Failed { error: format!("{last_err} (after {MAX_ATTEMPTS} attempts)") }
}

/// Clean up after a crash: remove temp files of unfinished imports.
pub fn recover(catalog: &Catalog) -> usize {
    let items = catalog.interrupted_items();
    for (id, temp) in &items {
        let p = Path::new(temp);
        // Only ever delete our own temp files.
        let ours = p.file_name().is_some_and(|n| {
            let n = n.to_string_lossy();
            n.starts_with('.') && n.ends_with(TEMP_SUFFIX)
        });
        if ours {
            let _ = fs::remove_file(p);
        }
        catalog.item_update(*id, "interrupted", None, None, Some("App closed during copy"));
    }
    catalog.mark_interrupted_sessions();
    items.len()
}

#[derive(Default, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub imported: u32,
    pub already_present: u32,
    pub skipped: u32,
    pub failed: u32,
    pub failed_items: Vec<u32>,
    pub bytes: u64,
    pub seconds: f64,
    pub cancelled: bool,
}

/// Run an import for the selected items of a scan session.
pub fn run(catalog: Arc<Catalog>, s: Arc<Session>, opts: ImportOpts, cancel: Arc<AtomicBool>) -> Summary {
    let root = PathBuf::from(&opts.dest_root);
    let started = Instant::now();
    let session_id = match catalog.session_start(&opts.dest_root) {
        Ok(id) => id,
        Err(e) => {
            s.emit("import-error", serde_json::json!({ "error": e.to_string() }));
            return Summary { failed: opts.item_ids.len() as u32, failed_items: opts.item_ids.clone(), ..Default::default() };
        }
    };

    // Order by capture date so {seq} counts in shooting order.
    let mut ids: Vec<u32> = opts.item_ids.iter().copied().filter(|&i| (i as usize) < s.items.len()).collect();
    ids.sort_by_key(|&i| (s.item_date(i), i));

    struct Planned {
        item: u32,
        file: usize,
        rel: PathBuf,
        known: Option<String>,
        meta: PhotoMeta,
        skip: Option<String>,
    }
    let mut work = Vec::new();
    for (n, &id) in ids.iter().enumerate() {
        let item = &s.items[id as usize];
        crate::scan::ensure_meta(&s, id as usize);
        let date = s.item_date(id);
        let st = s.state[id as usize].lock();
        let primary_meta = st.files[0].meta.clone().unwrap_or_default();
        let Some(date) = date else {
            for fi in 0..item.files.len() {
                let skip = Some("No date available".to_string());
                work.push(Planned { item: id, file: fi, rel: PathBuf::new(), known: None, meta: PhotoMeta::default(), skip });
            }
            continue;
        };
        let ctx = template::Ctx {
            date,
            make: primary_meta.make.as_deref(),
            model: primary_meta.model.as_deref(),
            original: &item.files[0].stem,
            seq: n as u32 + 1,
        };
        for (fi, f) in item.files.iter().enumerate() {
            let fs = &st.files[fi];
            let skip = (fs.status == Some(FileStatus::InDestination)).then(|| "Already in destination".to_string());
            work.push(Planned {
                item: id,
                file: fi,
                rel: template::plan(&opts.folder_tpl, &opts.name_tpl, &ctx, &f.ext),
                known: fs.hash.clone(),
                meta: fs.meta.clone().unwrap_or_default(),
                skip,
            });
        }
    }

    let bytes_total: u64 = work.iter().filter(|w| w.skip.is_none()).map(|w| s.items[w.item as usize].files[w.file].size).sum();
    let files_total = work.len();
    let bytes_done = AtomicU64::new(0);
    let files_done = AtomicU64::new(0);
    let next = AtomicU64::new(0);
    let summary = Mutex::new(Summary::default());
    let last_emit = Mutex::new(Instant::now());

    let emit_progress = |force: bool, current: &str| {
        let mut last = last_emit.lock();
        if !force && last.elapsed().as_millis() < 200 {
            return;
        }
        *last = Instant::now();
        let secs = started.elapsed().as_secs_f64().max(0.001);
        let done = bytes_done.load(Ordering::Relaxed);
        s.emit(
            "import-progress",
            serde_json::json!({
                "filesDone": files_done.load(Ordering::Relaxed),
                "filesTotal": files_total,
                "bytesDone": done,
                "bytesTotal": bytes_total,
                "bytesPerSec": done as f64 / secs,
                "current": current,
            }),
        );
    };

    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let i = next.fetch_add(1, Ordering::SeqCst) as usize;
                let Some(w) = work.get(i) else { break };
                let f = &s.items[w.item as usize].files[w.file];
                let outcome = match &w.skip {
                    Some(reason) => Outcome::Skipped { reason: reason.clone() },
                    None => {
                        emit_progress(true, &f.name);
                        let job = FileJob { src: f, rel: w.rel.clone(), known_hash: w.known.clone() };
                        import_file(&catalog, session_id, &root, &job, &w.meta, opts.paranoid, None, &|n| {
                            bytes_done.fetch_add(n, Ordering::Relaxed);
                            emit_progress(false, &f.name);
                        })
                    }
                };
                files_done.fetch_add(1, Ordering::Relaxed);
                {
                    let mut sm = summary.lock();
                    match &outcome {
                        Outcome::Imported { .. } => sm.imported += 1,
                        Outcome::AlreadyPresent { .. } => sm.already_present += 1,
                        Outcome::Skipped { .. } => sm.skipped += 1,
                        Outcome::Failed { .. } => {
                            sm.failed += 1;
                            if !sm.failed_items.contains(&w.item) {
                                sm.failed_items.push(w.item);
                            }
                        }
                    }
                }
                s.emit(
                    "import-file",
                    serde_json::json!({ "itemId": w.item, "file": f.name, "outcome": outcome }),
                );
                emit_progress(true, &f.name);
            });
        }
    });

    let mut sm = summary.into_inner();
    sm.bytes = bytes_done.load(Ordering::Relaxed);
    sm.seconds = started.elapsed().as_secs_f64();
    sm.cancelled = cancel.load(Ordering::Relaxed);
    catalog.session_finish(
        session_id,
        if sm.cancelled { "cancelled" } else if sm.failed > 0 { "failed" } else { "done" },
    );
    sm
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::Kind;

    fn source(dir: &Path, name: &str, data: &[u8]) -> SourceFile {
        let p = dir.join(name);
        fs::write(&p, data).unwrap();
        let md = fs::metadata(&p).unwrap();
        SourceFile {
            name: name.into(),
            stem: Path::new(name).file_stem().unwrap().to_string_lossy().into(),
            ext: Path::new(name).extension().unwrap().to_string_lossy().into(),
            path: p,
            size: data.len() as u64,
            mtime: file_mtime(&md),
            created: None,
            modified: md.modified().ok(),
            kind: Kind::Raw,
            key: String::new(),
        }
    }

    fn data() -> Vec<u8> {
        (0..9_000_000u32).map(|i| (i * 7 % 256) as u8).collect()
    }

    #[test]
    fn imports_and_verifies() {
        let d = tempfile::tempdir().unwrap();
        let (card, lib) = (d.path().join("card"), d.path().join("lib"));
        fs::create_dir_all(&card).unwrap();
        let src = source(&card, "P1.ORF", &data());
        let c = Catalog::open_in_memory().unwrap();
        let sid = c.session_start("lib").unwrap();
        let job = FileJob { src: &src, rel: "2026/October/P1.ORF".into(), known_hash: None };
        let out = import_file(&c, sid, &lib, &job, &PhotoMeta::default(), true, None, &|_| {});
        let dest = lib.join("2026/October/P1.ORF");
        assert_eq!(out, Outcome::Imported { dest: dest.to_string_lossy().into(), hash: blake3::hash(&data()).to_hex().to_string() });
        assert_eq!(fs::read(&dest).unwrap(), data());
        assert_eq!(fs::metadata(&dest).unwrap().modified().unwrap(), fs::metadata(&src.path).unwrap().modified().unwrap());
        assert!(!temp_path_for(&dest).exists());
        assert_eq!(c.imported_by_name_size("P1.ORF", src.size).len(), 1);

        // Same file again: recognised, not copied twice.
        let out = import_file(&c, sid, &lib, &job, &PhotoMeta::default(), false, None, &|_| {});
        assert!(matches!(out, Outcome::AlreadyPresent { .. }));
        assert_eq!(fs::read_dir(lib.join("2026/October")).unwrap().count(), 1);
    }

    #[test]
    fn different_file_with_same_name_gets_suffix() {
        let d = tempfile::tempdir().unwrap();
        let (card, lib) = (d.path().join("card"), d.path().join("lib"));
        fs::create_dir_all(&card).unwrap();
        fs::create_dir_all(lib.join("x")).unwrap();
        fs::write(lib.join("x/P1.ORF"), b"something else").unwrap();
        let src = source(&card, "P1.ORF", b"new content");
        let c = Catalog::open_in_memory().unwrap();
        let sid = c.session_start("lib").unwrap();
        let job = FileJob { src: &src, rel: "x/P1.ORF".into(), known_hash: None };
        match import_file(&c, sid, &lib, &job, &PhotoMeta::default(), false, None, &|_| {}) {
            Outcome::Imported { dest, .. } => assert!(dest.ends_with("P1_1.ORF")),
            o => panic!("{o:?}"),
        }
        assert_eq!(fs::read(lib.join("x/P1.ORF")).unwrap(), b"something else");
    }

    #[test]
    fn corruption_is_retried_then_fails() {
        let d = tempfile::tempdir().unwrap();
        let (card, lib) = (d.path().join("card"), d.path().join("lib"));
        fs::create_dir_all(&card).unwrap();
        let src = source(&card, "P1.ORF", &data());
        let c = Catalog::open_in_memory().unwrap();
        let sid = c.session_start("lib").unwrap();
        let job = FileJob { src: &src, rel: "P1.ORF".into(), known_hash: None };

        // Corrupt the first attempt only: should succeed on retry.
        let flip_first = |attempt: u32, buf: &mut [u8]| {
            if attempt == 1 {
                buf[0] ^= 0xFF;
            }
        };
        let out = import_file(&c, sid, &lib, &job, &PhotoMeta::default(), false, Some(&flip_first), &|_| {});
        assert!(matches!(out, Outcome::Imported { .. }), "{out:?}");
        assert_eq!(fs::read(lib.join("P1.ORF")).unwrap(), data());

        // Corrupt every attempt: must fail and leave nothing behind.
        let job = FileJob { src: &src, rel: "other/P1.ORF".into(), known_hash: None };
        let flip_all = |_: u32, buf: &mut [u8]| buf[0] ^= 0xFF;
        let out = import_file(&c, sid, &lib, &job, &PhotoMeta::default(), false, Some(&flip_all), &|_| {});
        assert!(matches!(out, Outcome::Failed { .. }), "{out:?}");
        assert_eq!(fs::read_dir(lib.join("other")).unwrap().count(), 0);
    }

    #[test]
    fn wrong_known_hash_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let (card, lib) = (d.path().join("card"), d.path().join("lib"));
        fs::create_dir_all(&card).unwrap();
        let src = source(&card, "P1.ORF", b"abc");
        let c = Catalog::open_in_memory().unwrap();
        let sid = c.session_start("lib").unwrap();
        let job = FileJob { src: &src, rel: "P1.ORF".into(), known_hash: Some("deadbeef".into()) };
        let out = import_file(&c, sid, &lib, &job, &PhotoMeta::default(), false, None, &|_| {});
        assert!(matches!(out, Outcome::Failed { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_source_is_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let (card, lib) = (d.path().join("card"), d.path().join("lib"));
        fs::create_dir_all(&card).unwrap();
        let src = source(&card, "P1.ORF", &data());
        let before = fs::metadata(&src.path).unwrap();
        fs::set_permissions(&src.path, fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(&card, fs::Permissions::from_mode(0o555)).unwrap();
        let c = Catalog::open_in_memory().unwrap();
        let sid = c.session_start("lib").unwrap();
        let job = FileJob { src: &src, rel: "P1.ORF".into(), known_hash: None };
        let out = import_file(&c, sid, &lib, &job, &PhotoMeta::default(), true, None, &|_| {});
        assert!(matches!(out, Outcome::Imported { .. }));
        let after = fs::metadata(&src.path).unwrap();
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(fs::read(&src.path).unwrap(), data());
        assert_eq!(fs::read_dir(&card).unwrap().count(), 1);
        fs::set_permissions(&card, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn recovery_removes_only_our_temp_files() {
        let d = tempfile::tempdir().unwrap();
        let c = Catalog::open_in_memory().unwrap();
        let sid = c.session_start("lib").unwrap();
        let temp = d.path().join(".P1.ORF.importing");
        fs::write(&temp, b"partial").unwrap();
        let other = d.path().join("P2.ORF");
        fs::write(&other, b"keep").unwrap();
        c.item_start(sid, "/card/P1.ORF", "x", &temp.to_string_lossy()).unwrap();
        c.item_start(sid, "/card/P2.ORF", "x", &other.to_string_lossy()).unwrap();
        assert_eq!(recover(&c), 2);
        assert!(!temp.exists());
        assert!(other.exists());
        assert!(c.interrupted_items().is_empty());
    }
}
