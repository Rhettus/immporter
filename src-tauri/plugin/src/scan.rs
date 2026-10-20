//! Scanning a source volume: stat-only walk, RAW+JPEG pairing, then
//! background metadata + preview extraction prioritised by what is on screen.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::NaiveDateTime;
use parking_lot::Mutex;
use serde::Serialize;
use walkdir::WalkDir;

use crate::immich::ImmichMatch;
use crate::metadata::{self, DateSource, Kind, PhotoMeta};
use crate::preview::{self, ThumbCache};
use crate::readonly;

pub type EmitFn = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

const META_WORKERS: usize = 4;
const SIDECAR_PREFIX: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: PathBuf,
    pub name: String,
    pub stem: String,
    pub ext: String,
    pub size: u64,
    pub mtime: i64,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub kind: Kind,
    /// Cache key for thumbnail + metadata.
    pub key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// Metadata not read yet.
    Pending,
    New,
    /// Name/size/EXIF match; hash verification running.
    Checking,
    /// New locally; asking Immich.
    CheckingImmich,
    /// Imported before (per the import history), but that copy is gone and
    /// it isn't in the library or Immich: probably deleted on purpose.
    Deleted,
    /// Verified (by hash) as previously imported by this app; that copy
    /// still exists (e.g. in another library folder).
    Duplicate,
    /// Found in the Immich library.
    InImmich,
    /// Verified (by hash) as already present in the destination folder.
    InDestination,
}

#[derive(Debug, Clone, Default)]
pub struct FileState {
    pub meta: Option<PhotoMeta>,
    /// BLAKE3 (hex) of the source, once it has been read in full.
    pub hash: Option<String>,
    /// SHA-1 (base64) of the source, for Immich.
    pub sha1: Option<String>,
    pub immich: Option<ImmichMatch>,
    pub status: Option<FileStatus>,
    /// Where the verified copy lives (duplicates / already-in-destination).
    pub existing: Option<String>,
}

#[derive(Debug, Default)]
pub struct ItemState {
    pub files: Vec<FileState>,
    pub thumb: Option<String>,
    pub thumb_failed: bool,
    pub meta_done: bool,
    /// Bumped on each dedupe evaluation so stale verify results are ignored.
    pub eval_gen: u64,
}

pub struct Item {
    pub id: u32,
    pub files: Vec<SourceFile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileView {
    pub name: String,
    pub size: u64,
    pub kind: Kind,
    pub status: FileStatus,
    pub existing: Option<String>,
    pub immich: Option<ImmichMatch>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemView {
    pub id: u32,
    pub name: String,
    pub dir: String,
    pub size: u64,
    pub files: Vec<FileView>,
    pub capture: Option<String>,
    /// Primary file's modification time: a capture-order stand-in until EXIF is read.
    pub file_time: Option<String>,
    pub date_source: Option<DateSource>,
    pub camera: Option<String>,
    pub thumb: Option<String>,
    pub thumb_failed: bool,
    pub status: FileStatus,
    /// Kind of the primary file (`video` tiles get a frame decoded by the UI).
    pub kind: Kind,
    pub duration_secs: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

pub struct Session {
    pub gen: u64,
    pub volume_id: String,
    /// Mount point of the volume holding the source (the card), even when a
    /// subfolder of it was picked. Imports onto it are refused.
    pub mount: PathBuf,
    pub root: PathBuf,
    pub items: Vec<Item>,
    pub state: Vec<Mutex<ItemState>>,
    pub cancelled: AtomicBool,
    sched: Mutex<Sched>,
    dirty: Mutex<Vec<u32>>,
    emit: EmitFn,
    pub verify_tx: Mutex<Option<std::sync::mpsc::Sender<crate::dedupe::VerifyJob>>>,
    pub immich_tx: Mutex<Option<std::sync::mpsc::Sender<crate::dedupe::ImmichJob>>>,
}

struct Sched {
    claimed: Vec<bool>,
    cursor: usize,
    visible: (usize, usize),
}

static NEXT_GEN: AtomicU64 = AtomicU64::new(1);

fn unix_secs(t: Option<SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Where to start walking: the DCIM folder if present, else the root.
pub fn source_root(mount: &Path) -> PathBuf {
    let dcim = mount.join("DCIM");
    if dcim.is_dir() {
        dcim
    } else {
        mount.to_path_buf()
    }
}

pub const REJECTED_FOLDER: &str = "_Rejected";

#[derive(Debug, Clone, Copy)]
pub struct WalkOpts {
    /// Descend into subfolders.
    pub recursive: bool,
    /// Leave out folders named `_Rejected` (FastRawViewer's culls) below the root.
    pub skip_rejected: bool,
    /// Include videos (and their sidecars).
    pub include_videos: bool,
}

impl Default for WalkOpts {
    /// A whole card: everything below DCIM except rejected photos.
    fn default() -> Self {
        Self { recursive: true, skip_rejected: true, include_videos: true }
    }
}

/// Where cameras keep videos outside DCIM (Sony XAVC, AVCHD).
pub const VIDEO_ROOTS: &[&str] = &["PRIVATE/M4ROOT/CLIP", "PRIVATE/AVCHD/BDMV/STREAM"];

/// Everything to walk for a whole card: DCIM (or the root), plus the video
/// folders some cameras use outside DCIM.
pub fn card_roots(mount: &Path, include_videos: bool) -> Vec<PathBuf> {
    let mut roots = vec![source_root(mount)];
    if include_videos && roots[0] != mount {
        roots.extend(VIDEO_ROOTS.iter().map(|r| mount.join(r)).filter(|p| p.is_dir()));
    }
    roots
}

/// Stat-only walk of a whole card. Never opens files.
pub fn walk(root: &Path, volume_id: &str) -> Vec<SourceFile> {
    walk_with(root, volume_id, WalkOpts::default())
}

/// Stat-only walk. Never opens files.
pub fn walk_with(root: &Path, volume_id: &str, opts: WalkOpts) -> Vec<SourceFile> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .max_depth(if opts.recursive { 8 } else { 1 })
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.')
                && !(opts.skip_rejected && e.file_type().is_dir() && name.eq_ignore_ascii_case(REJECTED_FOLDER))
        })
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
        let Some(kind) = metadata::kind_for_ext(&ext) else { continue };
        if kind.is_video_group() && !opts.include_videos {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let modified = md.modified().ok();
        let mtime = unix_secs(modified);
        out.push(SourceFile {
            key: ThumbCache::key(volume_id, path, md.len(), mtime),
            path: path.to_path_buf(),
            name,
            stem,
            ext,
            size: md.len(),
            mtime,
            created: md.created().ok(),
            modified,
            kind,
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderEntry {
    pub name: String,
    pub path: String,
    /// Photos directly in this folder (not counting subfolders).
    pub photos: u32,
    pub has_children: bool,
}

/// Direct subfolders of `path`, for the source tree. Read-only.
pub fn list_folders(path: &Path) -> Vec<FolderEntry> {
    let visible_dirs = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out: Vec<FolderEntry> = visible_dirs(path)
        .into_iter()
        .map(|dir| {
            let mut photos = 0u32;
            let mut has_children = false;
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') {
                        continue;
                    }
                    match e.file_type() {
                        Ok(t) if t.is_dir() => has_children = true,
                        Ok(t) if t.is_file() => {
                            let ext = Path::new(&name).extension().map(|x| x.to_string_lossy().to_string());
                            if ext.is_some_and(|x| metadata::kind_for_ext(&x).is_some_and(|k| k != Kind::Sidecar)) {
                                photos += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            FolderEntry {
                name: dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                path: dir.to_string_lossy().to_string(),
                photos,
                has_children,
            }
        })
        .collect();
    out.sort_by_key(|f| f.name.to_lowercase());
    out
}

/// The volume a path lives on: the longest mount point that contains it.
pub fn mount_for(path: &Path, mounts: &[PathBuf]) -> PathBuf {
    mounts
        .iter()
        .filter(|m| path.starts_with(m))
        .max_by_key(|m| m.as_os_str().len())
        .cloned()
        .unwrap_or_else(|| path.to_path_buf())
}

fn primary_rank(f: &SourceFile) -> u8 {
    match f.kind {
        // ORI (OM System high-res originals) pair with an ORF; prefer the ORF.
        Kind::Raw if f.ext.eq_ignore_ascii_case("ori") => 1,
        Kind::Raw => 0,
        Kind::Jpeg => 2,
        Kind::Other => 3,
        Kind::Video => 0,
        Kind::Sidecar => 4,
    }
}

/// Group files with the same stem in the same folder into one item.
/// Cache key for a video's decoded frame. Versioned so frames saved by an
/// older decoder (e.g. black frames) are regenerated.
pub fn video_thumb_key(file_key: &str) -> String {
    blake3::hash(format!("video-frame-v2:{file_key}").as_bytes()).to_hex()[..32].to_string()
}

/// Videos never pair with photos; a THM sidecar only travels with its video.
pub fn pair(files: Vec<SourceFile>) -> Vec<Vec<SourceFile>> {
    let mut groups: BTreeMap<(PathBuf, String, bool), Vec<SourceFile>> = BTreeMap::new();
    for f in files {
        let dir = f.path.parent().map(Path::to_path_buf).unwrap_or_default();
        groups.entry((dir, f.stem.to_lowercase(), f.kind.is_video_group())).or_default().push(f);
    }
    groups
        .into_values()
        .map(|mut g| {
            g.sort_by_key(|f| (primary_rank(f), f.name.clone()));
            g
        })
        .filter(|g| g[0].kind != Kind::Sidecar)
        .collect()
}

impl Session {
    pub fn new(volume_id: String, root: PathBuf, groups: Vec<Vec<SourceFile>>, emit: EmitFn) -> Arc<Self> {
        Self::new_on(volume_id, root.clone(), root, groups, emit)
    }

    pub fn new_on(volume_id: String, mount: PathBuf, root: PathBuf, groups: Vec<Vec<SourceFile>>, emit: EmitFn) -> Arc<Self> {
        let items: Vec<Item> =
            groups.into_iter().enumerate().map(|(i, files)| Item { id: i as u32, files }).collect();
        let state = items
            .iter()
            .map(|it| Mutex::new(ItemState { files: vec![FileState::default(); it.files.len()], ..Default::default() }))
            .collect();
        let n = items.len();
        Arc::new(Self {
            gen: NEXT_GEN.fetch_add(1, Ordering::SeqCst),
            volume_id,
            mount,
            root,
            items,
            state,
            cancelled: AtomicBool::new(false),
            sched: Mutex::new(Sched { claimed: vec![false; n], cursor: 0, visible: (0, 0) }),
            dirty: Mutex::new(Vec::new()),
            emit,
            verify_tx: Mutex::new(None),
            immich_tx: Mutex::new(None),
        })
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn set_visible(&self, start: usize, end: usize) {
        self.sched.lock().visible = (start, end);
    }

    /// Next item to process: visible ones first, then in order.
    fn claim(&self) -> Option<usize> {
        let mut s = self.sched.lock();
        let (vs, ve) = s.visible;
        let ve = ve.min(s.claimed.len());
        for i in vs.min(ve)..ve {
            if !s.claimed[i] {
                s.claimed[i] = true;
                return Some(i);
            }
        }
        while s.cursor < s.claimed.len() {
            let i = s.cursor;
            s.cursor += 1;
            if !s.claimed[i] {
                s.claimed[i] = true;
                return Some(i);
            }
        }
        None
    }

    pub fn mark_dirty(&self, id: u32) {
        self.dirty.lock().push(id);
    }

    pub fn view(&self, id: u32) -> ItemView {
        let item = &self.items[id as usize];
        let st = self.state[id as usize].lock();
        let primary = &item.files[0];
        let pmeta = st.files[0].meta.as_ref();
        let date = pmeta.and_then(|m| metadata::capture_or_file_time(m, primary.created, primary.modified));
        let files: Vec<FileView> = item
            .files
            .iter()
            .zip(&st.files)
            .map(|(f, s)| FileView {
                name: f.name.clone(),
                size: f.size,
                kind: f.kind,
                status: s.status.unwrap_or(FileStatus::Pending),
                existing: s.existing.clone(),
                immich: s.immich.clone(),
            })
            .collect();
        ItemView {
            id,
            name: primary.name.clone(),
            dir: primary
                .path
                .parent()
                .and_then(|p| p.strip_prefix(&self.root).ok())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            size: item.files.iter().map(|f| f.size).sum(),
            status: item_status(item.files[0].kind, &files.iter().map(|f| f.status).collect::<Vec<_>>()),
            files,
            capture: date.map(|(d, _)| d.format("%Y-%m-%dT%H:%M:%S").to_string()),
            file_time: primary
                .modified
                .map(|t| chrono::DateTime::<chrono::Local>::from(t).naive_local().format("%Y-%m-%dT%H:%M:%S").to_string()),
            date_source: date.map(|(_, s)| s),
            camera: pmeta.and_then(|m| m.model.clone()),
            thumb: st.thumb.clone(),
            thumb_failed: st.thumb_failed,
            kind: primary.kind,
            duration_secs: pmeta.and_then(|m| m.duration_secs),
            width: pmeta.and_then(|m| m.width),
            height: pmeta.and_then(|m| m.height),
        }
    }

    /// The date used for naming an item (EXIF → created → modified).
    pub fn item_date(&self, id: u32) -> Option<NaiveDateTime> {
        let item = &self.items[id as usize];
        let st = self.state[id as usize].lock();
        let meta = st.files[0].meta.clone().unwrap_or_default();
        metadata::capture_or_file_time(&meta, item.files[0].created, item.files[0].modified).map(|(d, _)| d)
    }

    pub fn flush(&self) {
        let mut ids = std::mem::take(&mut *self.dirty.lock());
        if ids.is_empty() {
            return;
        }
        ids.sort_unstable();
        ids.dedup();
        let views: Vec<ItemView> = ids.into_iter().map(|id| self.view(id)).collect();
        (self.emit)("items-updated", serde_json::json!({ "gen": self.gen, "items": views }));
    }

    pub fn emit(&self, event: &str, payload: serde_json::Value) {
        (self.emit)(event, payload)
    }
}

/// Item status. For RAW+JPEG pairs the RAW decides until it is known to be
/// new: a RAW already in the library makes the whole pair a duplicate, so a
/// sidecar JPEG that differs (e.g. Lightroom wrote edit metadata into the
/// library copy) never triggers a re-import.
pub fn item_status(primary_kind: Kind, statuses: &[FileStatus]) -> FileStatus {
    match statuses.first() {
        Some(&p) if primary_kind == Kind::Raw && p != FileStatus::New => p,
        _ => aggregate(statuses.iter().copied()),
    }
}

/// Status from all files: pending if anything is pending, new if any file is
/// new, otherwise the "least duplicate" of its files.
pub fn aggregate(statuses: impl Iterator<Item = FileStatus>) -> FileStatus {
    let v: Vec<FileStatus> = statuses.collect();
    if v.contains(&FileStatus::Pending) {
        FileStatus::Pending
    } else if v.contains(&FileStatus::New) {
        FileStatus::New
    } else {
        v.into_iter().min().unwrap_or(FileStatus::New)
    }
}

/// Read metadata (and, for the primary file, a preview) for one item.
fn process_item(s: &Session, cache: &ThumbCache, idx: usize) {
    let item = &s.items[idx];
    let mut metas = Vec::with_capacity(item.files.len());
    let mut thumb = None;
    let mut thumb_failed = false;
    for (fi, f) in item.files.iter().enumerate() {
        let is_primary = fi == 0;
        let cached_meta = cache.get_meta(&f.key);
        let have_thumb = !is_primary || cache.exists(&f.key);
        if let (Some(m), true) = (cached_meta.clone(), have_thumb) {
            if is_primary {
                thumb = Some(f.key.clone());
            }
            metas.push(m);
            continue;
        }
        if f.kind.is_video_group() {
            // Video metadata comes from the `moov` box; the frame for the
            // thumbnail is decoded by the UI and saved via `save_video_thumb`.
            let meta = cached_meta.unwrap_or_else(|| {
                if f.kind == Kind::Video {
                    let v = crate::video::read(&f.path, f.modified);
                    PhotoMeta {
                        capture: v.capture,
                        duration_secs: v.duration_secs,
                        width: v.width,
                        height: v.height,
                        ..Default::default()
                    }
                } else {
                    PhotoMeta::default()
                }
            });
            let _ = cache.put_meta(&f.key, &meta);
            let vkey = video_thumb_key(&f.key);
            if is_primary && cache.exists(&vkey) {
                thumb = Some(vkey);
            }
            metas.push(meta);
            continue;
        }
        let prefix_len = if is_primary { preview::PREFIX_SMALL } else { SIDECAR_PREFIX };
        let buf = match readonly::read_prefix(&f.path, prefix_len) {
            Ok(b) => b,
            Err(_) => {
                metas.push(PhotoMeta::default());
                thumb_failed |= is_primary;
                continue;
            }
        };
        let meta = cached_meta.unwrap_or_else(|| metadata::read_meta(&buf, f.kind));
        let _ = cache.put_meta(&f.key, &meta);
        if is_primary {
            // Embedded previews (and plain JPEG pixels) are stored un-rotated.
            match extract_preview(f, buf).and_then(|jpeg| preview::make_thumb(&jpeg, meta.orientation).ok()) {
                Some(t) if cache.put(&f.key, &t).is_ok() => thumb = Some(f.key.clone()),
                _ => thumb_failed = true,
            }
        }
        metas.push(meta);
    }
    let mut st = s.state[idx].lock();
    for (fs, m) in st.files.iter_mut().zip(metas) {
        fs.meta = Some(m);
    }
    st.thumb = thumb;
    st.thumb_failed = thumb_failed;
    st.meta_done = true;
}

/// Return the JPEG bytes to build a thumbnail from.
fn extract_preview(f: &SourceFile, prefix: Vec<u8>) -> Option<Vec<u8>> {
    const GOOD_WIDTH: u16 = 640;
    let is_jpeg = f.kind == Kind::Jpeg;
    let good = |h: &preview::JpegHit| h.width.max(h.height) >= GOOD_WIDTH;
    let scan = preview::find_largest_jpeg(&prefix, is_jpeg);
    if let Some(h) = scan.best.as_ref().filter(|h| good(h)) {
        return Some(prefix[h.start..h.end].to_vec());
    }
    let mut fallback = scan.best.map(|h| prefix[h.start..h.end].to_vec());
    if scan.truncated && (prefix.len() as u64) < f.size {
        if let Ok(big) = readonly::read_prefix(&f.path, preview::PREFIX_LARGE) {
            let scan = preview::find_largest_jpeg(&big, is_jpeg);
            if let Some(h) = scan.best {
                if good(&h) {
                    return Some(big[h.start..h.end].to_vec());
                }
                fallback = Some(big[h.start..h.end].to_vec());
            }
        }
    }
    if is_jpeg {
        // Camera JPEGs usually only embed a 160px thumbnail: use the image itself.
        if (prefix.len() as u64) >= f.size {
            return Some(prefix);
        }
        return readonly::read_prefix(&f.path, f.size as usize).ok();
    }
    fallback
}

/// Make sure every file of an item has metadata (without building previews).
/// The importer calls this so dates never silently fall back to file times
/// just because the background workers have not reached an item yet.
pub fn ensure_meta(s: &Session, idx: usize) {
    let missing: Vec<usize> = {
        let st = s.state[idx].lock();
        (0..st.files.len()).filter(|&i| st.files[i].meta.is_none()).collect()
    };
    for fi in missing {
        let f = &s.items[idx].files[fi];
        let len = if fi == 0 { preview::PREFIX_SMALL } else { SIDECAR_PREFIX };
        let meta = readonly::read_prefix(&f.path, len).map(|b| metadata::read_meta(&b, f.kind)).unwrap_or_default();
        let mut st = s.state[idx].lock();
        if st.files[fi].meta.is_none() {
            st.files[fi].meta = Some(meta);
        }
    }
}

/// Spawn the metadata/preview workers and the UI flusher for a session.
/// `on_meta` runs after each item's metadata is loaded (dedupe evaluation).
pub fn start_workers(s: &Arc<Session>, cache: Arc<ThumbCache>, on_meta: Arc<dyn Fn(&Arc<Session>, usize) + Send + Sync>) {
    let remaining = Arc::new(AtomicU64::new(s.items.len() as u64));
    for _ in 0..META_WORKERS {
        let s = s.clone();
        let cache = cache.clone();
        let on_meta = on_meta.clone();
        let remaining = remaining.clone();
        std::thread::spawn(move || {
            while !s.is_cancelled() {
                let Some(idx) = s.claim() else { break };
                process_item(&s, &cache, idx);
                on_meta(&s, idx);
                s.mark_dirty(idx as u32);
                if remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
                    s.emit("metadata-done", serde_json::json!({ "gen": s.gen }));
                }
            }
        });
    }
    let s = s.clone();
    std::thread::spawn(move || {
        while !s.is_cancelled() {
            std::thread::sleep(Duration::from_millis(120));
            s.flush();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sf(path: &str, kind: Kind) -> SourceFile {
        let p = PathBuf::from(path);
        SourceFile {
            name: p.file_name().unwrap().to_string_lossy().into(),
            stem: p.file_stem().unwrap().to_string_lossy().into(),
            ext: p.extension().unwrap().to_string_lossy().into(),
            path: p,
            size: 1,
            mtime: 0,
            created: None,
            modified: None,
            kind,
            key: String::new(),
        }
    }

    #[test]
    fn pairs_raw_and_jpeg() {
        let groups = pair(vec![
            sf("/c/DCIM/100/P1.JPG", Kind::Jpeg),
            sf("/c/DCIM/100/P1.ORF", Kind::Raw),
            sf("/c/DCIM/100/P1.ORI", Kind::Raw),
            sf("/c/DCIM/100/P2.JPG", Kind::Jpeg),
            sf("/c/DCIM/101/P1.ORF", Kind::Raw),
        ]);
        assert_eq!(groups.len(), 3);
        let names: Vec<Vec<&str>> = groups.iter().map(|g| g.iter().map(|f| f.name.as_str()).collect()).collect();
        assert_eq!(names[0], vec!["P1.ORF", "P1.ORI", "P1.JPG"]);
        assert_eq!(names[1], vec!["P2.JPG"]);
        assert_eq!(names[2], vec!["P1.ORF"]);
    }

    #[test]
    fn aggregates_status() {
        use FileStatus::*;
        assert_eq!(aggregate([New, InDestination].into_iter()), New);
        assert_eq!(aggregate([Duplicate, InDestination].into_iter()), Duplicate);
        assert_eq!(aggregate([InDestination, InDestination].into_iter()), InDestination);
        assert_eq!(aggregate([Checking, InDestination].into_iter()), Checking);
        assert_eq!(aggregate([CheckingImmich, InImmich].into_iter()), CheckingImmich);
        assert_eq!(aggregate([InImmich, InDestination].into_iter()), InImmich);
        assert_eq!(aggregate([Duplicate, InImmich].into_iter()), Duplicate);
        assert_eq!(aggregate([Deleted, Duplicate].into_iter()), Deleted);
        assert_eq!(item_status(Kind::Raw, &[Deleted, New]), Deleted);
        // RAW+JPEG: the RAW decides once it is known to be in the library.
        assert_eq!(item_status(Kind::Raw, &[InImmich, New]), InImmich);
        assert_eq!(item_status(Kind::Raw, &[InDestination, Checking]), InDestination);
        assert_eq!(item_status(Kind::Raw, &[New, InImmich]), New);
        assert_eq!(item_status(Kind::Raw, &[CheckingImmich, New]), CheckingImmich);
        assert_eq!(item_status(Kind::Jpeg, &[InImmich, New]), New);
        assert_eq!(aggregate([Pending, New].into_iter()), Pending);
    }

    fn touch(dir: &Path, names: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        for n in names {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
    }

    fn names(files: &[SourceFile]) -> Vec<String> {
        let mut v: Vec<String> = files.iter().map(|f| f.name.clone()).collect();
        v.sort();
        v
    }

    /// FastRawViewer layout: picks moved to _Selected, culls to _Rejected.
    #[test]
    fn walk_options_for_culled_cards() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("DCIM/100OMSYS");
        touch(&dir, &["P1.ORF", "P2.ORF"]);
        touch(&dir.join("_Selected"), &["P3.ORF", "P3.JPG"]);
        touch(&dir.join("_Rejected"), &["P4.ORF"]);
        touch(&dir.join(".hidden"), &["P5.ORF"]);

        // Whole card: everything except rejects.
        let card = walk(&source_root(d.path()), "v");
        assert_eq!(names(&card), ["P1.ORF", "P2.ORF", "P3.JPG", "P3.ORF"]);
        let with_rejects = walk_with(&source_root(d.path()), "v", WalkOpts { skip_rejected: false, ..Default::default() });
        assert_eq!(names(&with_rejects), ["P1.ORF", "P2.ORF", "P3.JPG", "P3.ORF", "P4.ORF"]);

        // A picked folder without subfolders.
        let flat = walk_with(&dir, "v", WalkOpts { recursive: false, skip_rejected: true, include_videos: true });
        assert_eq!(names(&flat), ["P1.ORF", "P2.ORF"]);
        let deep = walk_with(&dir, "v", WalkOpts { recursive: true, skip_rejected: true, include_videos: true });
        assert_eq!(names(&deep), ["P1.ORF", "P2.ORF", "P3.JPG", "P3.ORF"]);

        // Picking _Selected or _Rejected directly.
        let picks = walk_with(&dir.join("_Selected"), "v", WalkOpts { recursive: false, skip_rejected: true, include_videos: true });
        assert_eq!(names(&picks), ["P3.JPG", "P3.ORF"]);
        let rejects = walk_with(&dir.join("_Rejected"), "v", WalkOpts { recursive: false, skip_rejected: true, include_videos: true });
        assert_eq!(names(&rejects), ["P4.ORF"]);
    }

    #[test]
    fn videos_are_separate_items_with_sidecars() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("DCIM/100CANON");
        touch(&dir, &["MVI_1.MOV", "MVI_1.THM", "IMG_1.JPG", "P1.ORF", "P1.MOV", "LONE.THM"]);
        touch(&d.path().join("PRIVATE/M4ROOT/CLIP"), &["C0001.MP4"]);
        let mut files = Vec::new();
        for r in card_roots(d.path(), true) {
            files.extend(walk(&r, "v"));
        }
        let groups = pair(files);
        let names: Vec<Vec<&str>> = groups.iter().map(|g| g.iter().map(|f| f.name.as_str()).collect()).collect();
        assert!(names.contains(&vec!["MVI_1.MOV", "MVI_1.THM"]));
        assert!(names.contains(&vec!["P1.ORF"]));
        assert!(names.contains(&vec!["P1.MOV"]));
        assert!(names.contains(&vec!["C0001.MP4"]));
        assert!(!names.iter().any(|g| g.contains(&"LONE.THM")));
        // Videos off: no videos, no sidecars, no PRIVATE roots.
        let roots = card_roots(d.path(), false);
        assert_eq!(roots.len(), 1);
        let off = walk_with(&roots[0], "v", WalkOpts { include_videos: false, ..Default::default() });
        assert!(off.iter().all(|f| !f.kind.is_video_group()));
        assert_eq!(off.len(), 2);
    }

    #[test]
    fn lists_folders_with_counts() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("DCIM/100OMSYS");
        touch(&dir, &["P1.ORF", "notes.txt"]);
        touch(&dir.join("_Selected"), &["P3.ORF", "P3.JPG", "._P3.ORF"]);
        touch(&dir.join(".Spotlight"), &["x"]);
        let top = list_folders(d.path());
        assert_eq!(top.len(), 1);
        assert_eq!((top[0].name.as_str(), top[0].photos, top[0].has_children), ("DCIM", 0, true));
        let inner = list_folders(&dir);
        assert_eq!(inner.len(), 1);
        assert_eq!((inner[0].name.as_str(), inner[0].photos, inner[0].has_children), ("_Selected", 2, false));
    }

    #[test]
    fn finds_mount_point() {
        let mounts = vec![PathBuf::from("/Volumes/OM SYSTEM"), PathBuf::from("/Volumes/OM"), PathBuf::from("/")];
        assert_eq!(
            mount_for(Path::new("/Volumes/OM SYSTEM/DCIM/100OMSYS/_Selected"), &mounts),
            PathBuf::from("/Volumes/OM SYSTEM")
        );
        assert_eq!(mount_for(Path::new("/Volumes/OM/DCIM"), &mounts), PathBuf::from("/Volumes/OM"));
        assert_eq!(mount_for(Path::new("relative/dir"), &mounts[..2]), PathBuf::from("relative/dir"));
    }

    #[test]
    fn walk_skips_hidden_and_non_media() {
        let d = tempfile::tempdir().unwrap();
        let dcim = d.path().join("DCIM/100OMSYS");
        std::fs::create_dir_all(&dcim).unwrap();
        for n in ["P1.ORF", "P1.JPG", "._P1.ORF", "clip.MOV", "notes.txt"] {
            std::fs::write(dcim.join(n), b"x").unwrap();
        }
        let root = source_root(d.path());
        assert!(root.ends_with("DCIM"));
        let files = walk(&root, "vol");
        let names: Vec<_> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["P1.JPG", "P1.ORF", "clip.MOV"]);
    }
}
