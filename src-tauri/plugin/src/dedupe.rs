//! Staged duplicate detection, against both the import catalog and the
//! destination folder:
//!   1. name + size (and the exact planned destination path)
//!   2. EXIF: same capture time / serial / image number
//!   3. BLAKE3 of the source vs. the stored or freshly computed hash
//!
//! Only a hash match marks a file as a duplicate.
//!
//! Files still new after that are, when configured, looked up in Immich
//! (see `immich.rs`) by a separate batching worker.

use std::path::Path;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use parking_lot::RwLock;

use crate::catalog::{Catalog, DestRow, NewImported};
use crate::immich::{self, ImmichSettings};
use crate::metadata::{self, PhotoMeta};
use crate::readonly;
use crate::scan::{FileStatus, Session, SourceFile};
use crate::template;

#[derive(Debug, Clone, Default)]
pub struct DedupeCfg {
    pub dest_root: Option<String>,
    pub folder_tpl: String,
    pub name_tpl: String,
    /// The destination index for `dest_root` has been fully walked.
    pub dest_ready: bool,
}

#[derive(Debug, Clone)]
pub enum Candidate {
    Catalog { hash: String, dest_path: String },
    Dest(DestRow),
}

pub struct VerifyJob {
    pub item: usize,
    pub file: usize,
    pub eval_gen: u64,
    pub cands: Vec<Candidate>,
}

pub struct ImmichJob {
    pub item: usize,
    pub file: usize,
    pub eval_gen: u64,
}

/// A configured, enabled Immich connection.
pub struct ImmichCtx {
    pub client: immich::Client,
    pub settings: ImmichSettings,
}

pub struct Deduper {
    pub catalog: Arc<Catalog>,
    pub cfg: Arc<RwLock<DedupeCfg>>,
    pub immich: RwLock<Option<Arc<ImmichCtx>>>,
}

const IMMICH_BATCH: usize = 100;

pub fn file_mtime(md: &std::fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Hash of a destination file, using the cached value when size/mtime match.
pub fn dest_hash(catalog: &Catalog, row: &DestRow) -> Option<String> {
    let md = std::fs::metadata(&row.path).ok()?;
    if md.len() != row.size {
        return None;
    }
    let mtime = file_mtime(&md);
    if let (Some(h), true) = (&row.blake3, mtime == row.mtime) {
        return Some(h.clone());
    }
    let (h, _) = readonly::hash_file(Path::new(&row.path), false).ok()?;
    let h = h.to_hex().to_string();
    catalog.dest_set_hash(&row.path, md.len(), mtime, &h);
    Some(h)
}

impl Deduper {
    pub fn new(catalog: Arc<Catalog>, cfg: DedupeCfg) -> Self {
        Self { catalog, cfg: Arc::new(RwLock::new(cfg)), immich: RwLock::new(None) }
    }

    /// Status for a file that is new locally: hand it to Immich if configured.
    fn new_status(&self, s: &Session) -> FileStatus {
        if self.immich.read().is_some() && s.immich_tx.lock().is_some() {
            FileStatus::CheckingImmich
        } else {
            FileStatus::New
        }
    }

    fn send_immich(&self, s: &Session, jobs: Vec<ImmichJob>) {
        if let Some(tx) = s.immich_tx.lock().as_ref() {
            for j in jobs {
                let _ = tx.send(j);
            }
        }
    }

    /// Stages 1 and 2 for every file of an item; queues stage 3 jobs.
    pub fn evaluate_item(&self, s: &Arc<Session>, idx: usize) {
        let cfg = self.cfg.read().clone();
        let item = &s.items[idx];
        let date = s.item_date(idx as u32);
        let (eval_gen, metas) = {
            let mut st = s.state[idx].lock();
            if !st.meta_done {
                return;
            }
            st.eval_gen += 1;
            let metas: Vec<PhotoMeta> = st.files.iter().map(|f| f.meta.clone().unwrap_or_default()).collect();
            (st.eval_gen, metas)
        };
        let all_cands: Vec<Vec<Candidate>> = item
            .files
            .iter()
            .zip(&metas)
            .map(|(f, meta)| self.candidates(&cfg, f, meta, &metas[0], &item.files[0].stem, date))
            .collect();

        let mut jobs = Vec::new();
        let mut immich_jobs = Vec::new();
        let new_status = self.new_status(s);
        {
            let mut st = s.state[idx].lock();
            if st.eval_gen != eval_gen {
                return; // a newer evaluation is running
            }
            for (fi, cands) in all_cands.into_iter().enumerate() {
                let fs = &mut st.files[fi];
                fs.existing = None;
                fs.immich = None;
                if cands.is_empty() {
                    fs.status = Some(new_status);
                    if new_status == FileStatus::CheckingImmich {
                        immich_jobs.push(ImmichJob { item: idx, file: fi, eval_gen });
                    }
                } else {
                    fs.status = Some(FileStatus::Checking);
                    jobs.push(VerifyJob { item: idx, file: fi, eval_gen, cands });
                }
            }
        }
        if let Some(tx) = s.verify_tx.lock().as_ref() {
            for j in jobs {
                let _ = tx.send(j);
            }
        }
        self.send_immich(s, immich_jobs);
        s.mark_dirty(idx as u32);
    }

    fn candidates(
        &self,
        cfg: &DedupeCfg,
        f: &SourceFile,
        meta: &PhotoMeta,
        primary_meta: &PhotoMeta,
        primary_stem: &str,
        date: Option<chrono::NaiveDateTime>,
    ) -> Vec<Candidate> {
        // Files without a capture date can't be compared by EXIF; the hash decides.
        let exif_ok = |other: &PhotoMeta| meta.capture.is_none() || other.capture.is_none() || meta.same_shot(other);
        let mut cands = Vec::new();

        for row in self.catalog.imported_by_name_size(&f.name, f.size) {
            let other = PhotoMeta {
                capture: row.capture,
                serial: row.serial.clone(),
                image_number: row.image_number,
                ..Default::default()
            };
            if exif_ok(&other) {
                cands.push(Candidate::Catalog { hash: row.blake3, dest_path: row.dest_path });
            }
        }

        let (Some(root), true) = (&cfg.dest_root, cfg.dest_ready) else { return cands };
        let mut rows = self.catalog.dest_by_name_size(root, &f.name, f.size);
        // Copies this app made under another name (e.g. `_1` after a clash with an edited file).
        for c in &cands {
            if let Candidate::Catalog { dest_path, .. } = c {
                if let Some(r) = self.catalog.dest_by_path(dest_path).filter(|r| r.size == f.size) {
                    if !rows.iter().any(|x| x.path == r.path) {
                        rows.push(r);
                    }
                }
            }
        }
        if let Some(date) = date {
            let ctx = template::Ctx {
                date,
                make: primary_meta.make.as_deref(),
                model: primary_meta.model.as_deref(),
                original: primary_stem,
                seq: 1,
            };
            let planned = Path::new(root).join(template::plan(&cfg.folder_tpl, &cfg.name_tpl, &ctx, &f.ext));
            if let Some(r) = self.catalog.dest_by_path(&planned.to_string_lossy()) {
                if r.size == f.size && !rows.iter().any(|x| x.path == r.path) {
                    rows.push(r);
                }
            }
        }
        for r in rows {
            // Stage 2 on the destination side: its EXIF (local disk, small read).
            let other = readonly::read_prefix(Path::new(&r.path), 512 * 1024)
                .map(|b| metadata::read_meta(&b, f.kind))
                .unwrap_or_default();
            if exif_ok(&other) {
                cands.push(Candidate::Dest(r));
            }
        }
        cands
    }

    pub fn evaluate_all(&self, s: &Arc<Session>) {
        for idx in 0..s.items.len() {
            if s.is_cancelled() {
                return;
            }
            self.evaluate_item(s, idx);
        }
    }

    /// Stage 3. Runs on one thread so card reads stay sequential.
    pub fn start_verifier(self: &Arc<Self>, s: &Arc<Session>) {
        let (tx, rx) = channel::<VerifyJob>();
        *s.verify_tx.lock() = Some(tx);
        let s = s.clone();
        let me = self.clone();
        std::thread::spawn(move || loop {
            if s.is_cancelled() {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(job) => me.verify(&s, job),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        });
    }

    fn verify(&self, s: &Arc<Session>, job: VerifyJob) {
        if s.state[job.item].lock().eval_gen != job.eval_gen {
            return;
        }
        let f = &s.items[job.item].files[job.file];
        let cached = s.state[job.item].lock().files[job.file].hash.clone();
        let src_hash = match cached {
            Some(h) => h,
            None => match readonly::hash_file_both(&f.path, false) {
                Ok((h, sha1, n)) if n == f.size => {
                    let mut st = s.state[job.item].lock();
                    st.files[job.file].hash = Some(h.clone());
                    st.files[job.file].sha1 = Some(sha1);
                    h
                }
                // Unreadable source: treat as new; the import will report the error.
                _ => {
                    self.set(s, &job, FileStatus::New, None);
                    return;
                }
            },
        };
        let mut result = (FileStatus::New, None);
        for c in &job.cands {
            match c {
                Candidate::Dest(row) => {
                    if dest_hash(&self.catalog, row).as_deref() == Some(src_hash.as_str()) {
                        result = (FileStatus::InDestination, Some(row.path.clone()));
                        break;
                    }
                }
                Candidate::Catalog { hash, dest_path } => {
                    if *hash == src_hash {
                        // Does the copy recorded in the import history still exist?
                        let still_there = std::fs::metadata(dest_path).is_ok_and(|m| m.is_file() && m.len() == f.size);
                        let status = if still_there { FileStatus::Duplicate } else { FileStatus::Deleted };
                        if result.0 == FileStatus::New || (result.0 == FileStatus::Deleted && still_there) {
                            result = (status, Some(dest_path.clone()));
                        }
                    }
                }
            }
        }
        if result.0 == FileStatus::InDestination {
            // Remember it so future scans hit the catalog at stage 1/2.
            let meta = s.state[job.item].lock().files[job.file].meta.clone().unwrap_or_default();
            let _ = self.catalog.record_imported(&NewImported {
                orig_name: &f.name,
                size: f.size,
                meta: &meta,
                blake3: &src_hash,
                dest_path: result.1.as_deref().unwrap_or_default(),
            });
        }
        self.set(s, &job, result.0, result.1);
    }

    fn set(&self, s: &Arc<Session>, job: &VerifyJob, status: FileStatus, existing: Option<String>) {
        // Locally new: still to be checked against Immich.
        let status = if status == FileStatus::New { self.new_status(s) } else { status };
        let mut st = s.state[job.item].lock();
        if st.eval_gen != job.eval_gen {
            return;
        }
        st.files[job.file].status = Some(status);
        st.files[job.file].existing = existing;
        drop(st);
        // New files are decided by Immich; files already in the library are
        // also looked up there, purely to show whether Immich has them too.
        if status == FileStatus::CheckingImmich || (self.immich.read().is_some() && status != FileStatus::New) {
            self.send_immich(s, vec![ImmichJob { item: job.item, file: job.file, eval_gen: job.eval_gen }]);
        }
        s.mark_dirty(job.item as u32);
    }

    // ---- Immich -----------------------------------------------------------

    /// Planned path of a file relative to the destination root (`/`-separated).
    fn planned_rel(&self, cfg: &DedupeCfg, s: &Session, item: usize, file: usize) -> Option<String> {
        let date = s.item_date(item as u32)?;
        let it = &s.items[item];
        let meta = s.state[item].lock().files[0].meta.clone().unwrap_or_default();
        let ctx = template::Ctx {
            date,
            make: meta.make.as_deref(),
            model: meta.model.as_deref(),
            original: &it.files[0].stem,
            seq: 1,
        };
        let rel = template::plan(&cfg.folder_tpl, &cfg.name_tpl, &ctx, &it.files[file].ext);
        Some(rel.to_string_lossy().replace('\\', "/"))
    }

    /// Batches Immich lookups: one search request covers many files.
    pub fn start_immich_worker(self: &Arc<Self>, s: &Arc<Session>) {
        let (tx, rx) = channel::<ImmichJob>();
        *s.immich_tx.lock() = Some(tx);
        let s = s.clone();
        let me = self.clone();
        std::thread::spawn(move || {
            let mut batch: Vec<ImmichJob> = Vec::new();
            loop {
                if s.is_cancelled() {
                    break;
                }
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(job) => {
                        batch.push(job);
                        if batch.len() < IMMICH_BATCH {
                            continue;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if !batch.is_empty() {
                    me.immich_batch(&s, std::mem::take(&mut batch));
                }
            }
        });
    }

    fn immich_batch(&self, s: &Arc<Session>, jobs: Vec<ImmichJob>) {
        let jobs: Vec<ImmichJob> =
            jobs.into_iter().filter(|j| s.state[j.item].lock().eval_gen == j.eval_gen).collect();
        if jobs.is_empty() {
            return;
        }
        let Some(ctx) = self.immich.read().clone() else {
            for j in &jobs {
                self.set_immich(s, j, None);
            }
            return;
        };
        let cfg = self.cfg.read().clone();
        let name_unpredictable = cfg.name_tpl.contains("{seq}");

        // Names to search: the card name, the name the current template would
        // give it, and any name it was actually imported under before.
        let mut planned: Vec<Option<String>> = Vec::with_capacity(jobs.len());
        let mut histories: Vec<Vec<(String, Option<String>)>> = Vec::with_capacity(jobs.len());
        let mut names: Vec<String> = Vec::new();
        for j in &jobs {
            let f = &s.items[j.item].files[j.file];
            let rel = self.planned_rel(&cfg, s, j.item, j.file);
            names.push(f.name.clone());
            if let Some(r) = &rel {
                if let Some(n) = r.rsplit('/').next().filter(|n| *n != f.name && !name_unpredictable) {
                    names.push(n.to_string());
                }
            }
            let history = self.import_history(&cfg, f);
            names.extend(history.iter().map(|(n, _)| n.clone()));
            planned.push(rel);
            histories.push(history);
        }
        names.sort();
        names.dedup();

        let assets = match ctx.client.search_names(&names) {
            Ok(a) => {
                s.emit("immich-status", serde_json::json!({ "state": "ok" }));
                a
            }
            Err(e) => {
                s.emit("immich-status", serde_json::json!({ "state": "error", "error": e }));
                for j in &jobs {
                    self.set_immich(s, j, None);
                }
                return;
            }
        };

        for ((j, rel), history) in jobs.iter().zip(planned).zip(histories) {
            let f = &s.items[j.item].files[j.file];
            let capture = s.state[j.item].lock().files[j.file].meta.as_ref().and_then(|m| m.capture);
            let planned_name = rel.as_deref().and_then(|r| r.rsplit('/').next()).filter(|_| !name_unpredictable);
            let input = immich::MatchInput {
                card_name: &f.name,
                planned_name,
                size: f.size,
                capture,
                planned_rel: rel.as_deref(),
                name_unpredictable,
                history: &history,
                is_video: f.kind == crate::metadata::Kind::Video,
            };
            let cands: Vec<&immich::Asset> = assets
                .iter()
                .filter(|a| {
                    let n = &a.original_file_name;
                    n.eq_ignore_ascii_case(&f.name)
                        || planned_name.is_some_and(|p| n.eq_ignore_ascii_case(p))
                        || history.iter().any(|(h, _)| n.eq_ignore_ascii_case(h))
                })
                .collect();
            let mut sha1 = || -> Option<String> {
                if let Some(h) = s.state[j.item].lock().files[j.file].sha1.clone() {
                    return Some(h);
                }
                let (b3, sha1, n) = readonly::hash_file_both(&f.path, false).ok()?;
                if n != f.size {
                    return None;
                }
                let mut st = s.state[j.item].lock();
                st.files[j.file].hash = Some(b3);
                st.files[j.file].sha1 = Some(sha1.clone());
                Some(sha1)
            };
            let m = immich::match_file(&input, &ctx.settings, &cands, &mut sha1);
            self.set_immich(s, j, m);
        }
    }

    /// Names this card file was imported under before (from the import
    /// history), with the path relative to the current destination root.
    fn import_history(&self, cfg: &DedupeCfg, f: &SourceFile) -> Vec<(String, Option<String>)> {
        let mut out: Vec<(String, Option<String>)> = Vec::new();
        for row in self.catalog.imported_by_name_size(&f.name, f.size) {
            let p = Path::new(&row.dest_path);
            let Some(name) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
            let rel = cfg
                .dest_root
                .as_deref()
                .and_then(|root| p.strip_prefix(root).ok())
                .map(|r| r.to_string_lossy().replace('\\', "/"));
            if !out.iter().any(|(n, r)| *n == name && *r == rel) {
                out.push((name, rel));
            }
        }
        out
    }

    fn set_immich(&self, s: &Arc<Session>, job: &ImmichJob, m: Option<immich::ImmichMatch>) {
        let mut st = s.state[job.item].lock();
        if st.eval_gen != job.eval_gen {
            return;
        }
        let fs = &mut st.files[job.file];
        match fs.status {
            Some(FileStatus::CheckingImmich) => {
                fs.status = Some(if m.is_some() { FileStatus::InImmich } else { FileStatus::New });
                fs.existing = m.as_ref().map(|m| m.original_path.clone());
            }
            // Imported before but no longer in the destination folder: being
            // in Immich is the more useful thing to show.
            Some(FileStatus::Duplicate | FileStatus::Deleted) if m.is_some() => {
                fs.status = Some(FileStatus::InImmich);
                fs.existing = m.as_ref().map(|m| m.original_path.clone());
            }
            // In the destination folder: that status stays; the match is extra info.
            _ => {}
        }
        fs.immich = m;
        drop(st);
        s.mark_dirty(job.item as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::ThumbCache;
    use crate::scan::{self, FileStatus};
    use std::sync::atomic::AtomicBool;

    fn settle(s: &Arc<Session>) -> Vec<FileStatus> {
        for _ in 0..200 {
            let v: Vec<FileStatus> = (0..s.items.len() as u32).map(|i| s.view(i).status).collect();
            if v.iter().all(|x| !matches!(x, FileStatus::Pending | FileStatus::Checking | FileStatus::CheckingImmich)) {
                return v;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("did not settle");
    }

    /// Files already in the destination are recognised with an empty catalog,
    /// and only when their content really matches.
    #[test]
    fn finds_files_already_in_destination() {
        let d = tempfile::tempdir().unwrap();
        let card = d.path().join("card/DCIM/100OMSYS");
        let lib = d.path().join("lib/2025/Somewhere");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(card.join("P1.ORF"), b"same bytes").unwrap();
        std::fs::write(card.join("P2.ORF"), b"card version").unwrap();
        std::fs::write(card.join("P3.ORF"), b"brand new").unwrap();
        std::fs::write(lib.join("P1.ORF"), b"same bytes").unwrap();
        std::fs::write(lib.join("P2.ORF"), b"card versioN").unwrap(); // same size, one byte differs

        let catalog = Arc::new(Catalog::open_in_memory().unwrap());
        let root = d.path().join("lib").to_string_lossy().to_string();
        crate::dest_index::walk(&catalog, &root, &AtomicBool::new(false), |_| {}).unwrap();
        let cfg = DedupeCfg { dest_root: Some(root), folder_tpl: "{YYYY}/{MMMM}".into(), name_tpl: String::new(), dest_ready: true };
        let deduper = Arc::new(Deduper::new(catalog.clone(), cfg));

        let src_root = scan::source_root(&d.path().join("card"));
        let session = Session::new(
            "card".into(),
            src_root.clone(),
            scan::pair(scan::walk(&src_root, "card")),
            Arc::new(|_, _| {}),
        );
        deduper.start_verifier(&session);
        let thumbs = Arc::new(ThumbCache::new(d.path().join("thumbs")).unwrap());
        let dd = deduper.clone();
        scan::start_workers(&session, thumbs, Arc::new(move |s, i| dd.evaluate_item(s, i)));
        assert_eq!(settle(&session), vec![FileStatus::InDestination, FileStatus::New, FileStatus::New]);
        session.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);

        // The match was written back to the catalog for next time.
        assert_eq!(catalog.imported_by_name_size("P1.ORF", 10).len(), 1);
    }

    /// A copy imported under a suffixed name (because an edited file held the
    /// original name) still counts as in the destination.
    #[test]
    fn finds_suffixed_copies_via_catalog() {
        let d = tempfile::tempdir().unwrap();
        let card = d.path().join("card/DCIM/100");
        let lib = d.path().join("lib/2026/October");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(card.join("IMG_1.JPG"), b"original").unwrap();
        std::fs::write(lib.join("IMG_1.JPG"), b"edited!!").unwrap(); // same size, different content
        std::fs::write(lib.join("IMG_1_1.JPG"), b"original").unwrap();

        let catalog = Arc::new(Catalog::open_in_memory().unwrap());
        catalog
            .record_imported(&NewImported {
                orig_name: "IMG_1.JPG",
                size: 8,
                meta: &PhotoMeta::default(),
                blake3: &blake3::hash(b"original").to_hex(),
                dest_path: &lib.join("IMG_1_1.JPG").to_string_lossy(),
            })
            .unwrap();
        let root = d.path().join("lib").to_string_lossy().to_string();
        crate::dest_index::walk(&catalog, &root, &AtomicBool::new(false), |_| {}).unwrap();
        let cfg = DedupeCfg { dest_root: Some(root), folder_tpl: "{YYYY}/{MMMM}".into(), name_tpl: String::new(), dest_ready: true };
        let deduper = Arc::new(Deduper::new(catalog, cfg));
        let src_root = scan::source_root(&d.path().join("card"));
        let session = Session::new("card".into(), src_root.clone(), scan::pair(scan::walk(&src_root, "card")), Arc::new(|_, _| {}));
        deduper.start_verifier(&session);
        let dd = deduper.clone();
        scan::start_workers(&session, Arc::new(ThumbCache::new(d.path().join("t")).unwrap()), Arc::new(move |s, i| dd.evaluate_item(s, i)));
        assert_eq!(settle(&session), vec![FileStatus::InDestination]);
        session.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn immich_session(d: &Path, server_url: Option<&str>) -> (Arc<Session>, Arc<Deduper>) {
        immich_session_with(d, server_url, |_| {})
    }

    fn immich_session_with(d: &Path, server_url: Option<&str>, seed: impl Fn(&Catalog)) -> (Arc<Session>, Arc<Deduper>) {
        let catalog = Arc::new(Catalog::open_in_memory().unwrap());
        seed(&catalog);
        let deduper = Arc::new(Deduper::new(catalog, DedupeCfg { folder_tpl: "{YYYY}/{MMMM}".into(), ..Default::default() }));
        let settings = ImmichSettings { enabled: true, url: server_url.unwrap_or("http://127.0.0.1:9").into(), ..Default::default() };
        let client = immich::Client::new(&settings.url, "k", false).unwrap();
        *deduper.immich.write() = Some(Arc::new(ImmichCtx { client, settings }));
        let src_root = scan::source_root(&d.join("card"));
        let session = Session::new("card".into(), src_root.clone(), scan::pair(scan::walk(&src_root, "card")), Arc::new(|_, _| {}));
        deduper.start_verifier(&session);
        deduper.start_immich_worker(&session);
        let dd = deduper.clone();
        scan::start_workers(&session, Arc::new(ThumbCache::new(d.join("t")).unwrap()), Arc::new(move |s, i| dd.evaluate_item(s, i)));
        (session, deduper)
    }

    #[test]
    fn files_found_in_immich_are_flagged() {
        let d = tempfile::tempdir().unwrap();
        let card = d.path().join("card/DCIM/100");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join("A.JPG"), b"aaaa").unwrap();
        std::fs::write(card.join("B.JPG"), b"bbbbbb").unwrap();
        let mut server = mockito::Server::new();
        // Only A is in Immich (external library: name + size match, no EXIF date on the card file).
        server
            .mock("POST", "/api/search/metadata")
            .with_body(r#"{"albums":{},"assets":{"items":[
                {"id":"a1","originalFileName":"A.JPG","originalPath":"/lib/A.JPG","libraryId":"L","checksum":"p","localDateTime":"2026-01-01T00:00:00.000Z","exifInfo":{"fileSizeInByte":4}},
                {"id":"b1","originalFileName":"B.JPG","originalPath":"/lib/B.JPG","libraryId":"L","checksum":"p","localDateTime":null,"exifInfo":{"fileSizeInByte":999}}
            ],"nextCursor":null}}"#)
            .create();
        let (session, _) = immich_session(d.path(), Some(&server.url()));
        assert_eq!(settle(&session), vec![FileStatus::InImmich, FileStatus::New]);
        assert_eq!(session.view(0).files[0].immich.as_ref().unwrap().asset_id, "a1");
        session.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn immich_unreachable_falls_back_to_new() {
        let d = tempfile::tempdir().unwrap();
        let card = d.path().join("card/DCIM/100");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join("A.JPG"), b"aaaa").unwrap();
        std::fs::write(card.join("B.JPG"), b"bbbbbb").unwrap();
        let (session, _) = immich_session(d.path(), None);
        assert_eq!(settle(&session), vec![FileStatus::New, FileStatus::New]);
        session.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Imported earlier as `20260327_070531.JPG` with a renaming template;
    /// the template has changed since, so only the import history knows the
    /// name Immich has.
    #[test]
    fn renamed_import_found_in_immich_after_template_change() {
        let d = tempfile::tempdir().unwrap();
        let card = d.path().join("card/DCIM/100");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join("A.JPG"), b"aaaa").unwrap();
        let mut server = mockito::Server::new();
        let search = server
            .mock("POST", "/api/search/metadata")
            .match_request(|r| r.utf8_lossy_body().unwrap().contains("20260327_070531.JPG"))
            .with_body(r#"{"albums":{},"assets":{"items":[
                {"id":"r1","originalFileName":"20260327_070531.JPG","originalPath":"/lib/2026/March/20260327_070531.JPG","libraryId":"L","checksum":"p","localDateTime":null,"exifInfo":{"fileSizeInByte":4}}
            ],"nextCursor":null}}"#)
            .create();
        let (session, _) = immich_session_with(d.path(), Some(&server.url()), |c| {
            c.record_imported(&NewImported {
                orig_name: "A.JPG",
                size: 4,
                meta: &PhotoMeta::default(),
                blake3: "old",
                dest_path: "/elsewhere/2026/March/20260327_070531.JPG",
            })
            .unwrap();
        });
        assert_eq!(settle(&session), vec![FileStatus::InImmich]);
        assert_eq!(session.view(0).files[0].immich.as_ref().unwrap().asset_id, "r1");
        search.assert();
        session.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// A photo already in the destination folder keeps that status, and also
    /// shows that Immich has it (e.g. after copying the library to a NAS).
    #[test]
    fn library_photos_also_report_immich() {
        let d = tempfile::tempdir().unwrap();
        let card = d.path().join("card/DCIM/100");
        let lib = d.path().join("lib/2026/October");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(card.join("A.JPG"), b"aaaa").unwrap();
        std::fs::write(lib.join("A.JPG"), b"aaaa").unwrap();
        let mut server = mockito::Server::new();
        server
            .mock("POST", "/api/search/metadata")
            .with_body(r#"{"albums":{},"assets":{"items":[
                {"id":"a1","originalFileName":"A.JPG","originalPath":"/nas/2026/October/A.JPG","libraryId":"L","checksum":"p","localDateTime":null,"exifInfo":{"fileSizeInByte":4}}
            ],"nextCursor":null}}"#)
            .create();
        let root = d.path().join("lib").to_string_lossy().to_string();
        let (session, deduper) = immich_session_with(d.path(), Some(&server.url()), |c| {
            crate::dest_index::walk(c, &root, &AtomicBool::new(false), |_| {}).unwrap();
        });
        {
            let mut cfg = deduper.cfg.write();
            cfg.dest_root = Some(root.clone());
            cfg.dest_ready = true;
        }
        deduper.evaluate_all(&session);
        assert_eq!(settle(&session), vec![FileStatus::InDestination]);
        for _ in 0..200 {
            if session.view(0).files[0].immich.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let v = session.view(0);
        assert_eq!(v.status, FileStatus::InDestination);
        assert_eq!(v.files[0].immich.as_ref().unwrap().asset_id, "a1");
        session.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Imported before, then deleted from the library, but still in Immich:
    /// shown as In Immich rather than "Imported".
    #[test]
    fn deleted_from_library_but_in_immich() {
        let d = tempfile::tempdir().unwrap();
        let card = d.path().join("card/DCIM/100");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join("A.JPG"), b"aaaa").unwrap();
        let mut server = mockito::Server::new();
        server
            .mock("POST", "/api/search/metadata")
            .with_body(r#"{"albums":{},"assets":{"items":[
                {"id":"a1","originalFileName":"A.JPG","originalPath":"/nas/A.JPG","libraryId":"L","checksum":"p","localDateTime":null,"exifInfo":{"fileSizeInByte":4}}
            ],"nextCursor":null}}"#)
            .create();
        let (session, _) = immich_session_with(d.path(), Some(&server.url()), |c| {
            c.record_imported(&NewImported {
                orig_name: "A.JPG",
                size: 4,
                meta: &PhotoMeta::default(),
                blake3: &blake3::hash(b"aaaa").to_hex(),
                dest_path: "/deleted/A.JPG",
            })
            .unwrap();
        });
        // "Imported" is final locally; the Immich answer arrives after it.
        for _ in 0..200 {
            if session.view(0).status == FileStatus::InImmich {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(session.view(0).status, FileStatus::InImmich);
        session.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Imported before: "Imported" while the recorded copy still exists,
    /// "Deleted" once it's gone (and it isn't in the library or Immich).
    #[test]
    fn imported_vs_previously_deleted() {
        let d = tempfile::tempdir().unwrap();
        let card = d.path().join("card/DCIM/100");
        let old_lib = d.path().join("old_lib");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::create_dir_all(&old_lib).unwrap();
        std::fs::write(card.join("A.JPG"), b"aaaa").unwrap();
        std::fs::write(card.join("B.JPG"), b"bbbb").unwrap();
        std::fs::write(old_lib.join("A.JPG"), b"aaaa").unwrap(); // A still exists; B was deleted
        let catalog = Arc::new(Catalog::open_in_memory().unwrap());
        for (name, data) in [("A.JPG", b"aaaa"), ("B.JPG", b"bbbb")] {
            catalog
                .record_imported(&NewImported {
                    orig_name: name,
                    size: 4,
                    meta: &PhotoMeta::default(),
                    blake3: &blake3::hash(data).to_hex(),
                    dest_path: &old_lib.join(name).to_string_lossy(),
                })
                .unwrap();
        }
        let deduper = Arc::new(Deduper::new(catalog, DedupeCfg::default()));
        let src_root = scan::source_root(&d.path().join("card"));
        let session = Session::new("card".into(), src_root.clone(), scan::pair(scan::walk(&src_root, "card")), Arc::new(|_, _| {}));
        deduper.start_verifier(&session);
        let dd = deduper.clone();
        scan::start_workers(&session, Arc::new(ThumbCache::new(d.path().join("t")).unwrap()), Arc::new(move |s, i| dd.evaluate_item(s, i)));
        let mut statuses = Vec::new();
        for _ in 0..200 {
            statuses = (0..2).map(|i| session.view(i).status).collect::<Vec<_>>();
            if !statuses.iter().any(|s| matches!(s, FileStatus::Pending | FileStatus::Checking)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(statuses, vec![FileStatus::Duplicate, FileStatus::Deleted]);
        session.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
