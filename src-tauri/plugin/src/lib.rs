pub mod catalog;
pub mod dedupe;
pub mod dest_index;
pub mod immich;
pub mod import;
pub mod media;
pub mod metadata;
pub mod preview;
pub mod readonly;
pub mod scan;
pub mod template;
pub mod video;
pub mod volumes;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::plugin::TauriPlugin;
use tauri::{AppHandle, Emitter, Manager, State, Wry};

use catalog::Catalog;
use dedupe::{DedupeCfg, Deduper, ImmichCtx};
use immich::ImmichSettings;
use preview::ThumbCache;
use scan::{ItemView, Session};

pub struct AppState {
    catalog: Arc<Catalog>,
    thumbs: Arc<ThumbCache>,
    dedupe: Arc<Deduper>,
    session: Mutex<Option<Arc<Session>>>,
    dest_cancel: Mutex<Arc<AtomicBool>>,
    import_cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// Set once the volume watcher runs: it starts with the first listing,
    /// so an app embedding the importer does not poll until it is opened.
    watching: AtomicBool,
}

/// How an app embeds the importer.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// A folder inside the app's data and cache dirs for the importer's
    /// catalog and thumbnails. `None` uses the dirs themselves, as the
    /// standalone app always has.
    pub data_subdir: Option<String>,
    /// The keychain service an older version of the app kept the Immich key
    /// under (its identifier before a rename): the key moves across once.
    pub keychain_from: Option<String>,
}

impl AppState {
    fn current(&self) -> Option<Arc<Session>> {
        self.session.lock().clone()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanResult {
    gen: u64,
    root: String,
    items: Vec<ItemView>,
}

/// Debug builds only: open a source on launch (`SDI_AUTOLOAD_SOURCE=/Volumes/CARD`),
/// so the grid can be exercised without clicking.
#[tauri::command]
fn dev_autoload_source() -> Option<String> {
    if cfg!(debug_assertions) {
        std::env::var("SDI_AUTOLOAD_SOURCE").ok()
    } else {
        None
    }
}

#[tauri::command]
fn list_volumes(app: AppHandle, state: State<'_, AppState>) -> Vec<volumes::Volume> {
    if !state.watching.swap(true, Ordering::SeqCst) {
        spawn_volume_watcher(app);
    }
    volumes::list()
}

#[tauri::command]
async fn scan_source(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
    include_subfolders: Option<bool>,
    skip_rejected: Option<bool>,
    include_videos: Option<bool>,
) -> Result<ScanResult, String> {
    if let Some(old) = state.session.lock().take() {
        old.cancelled.store(true, Ordering::SeqCst);
    }
    let picked = PathBuf::from(&path);
    if !picked.is_dir() {
        return Err(format!("{path} is not available"));
    }
    let mounts: Vec<PathBuf> = volumes::list().into_iter().map(|v| PathBuf::from(v.mount_point)).collect();
    let mount = scan::mount_for(&picked, &mounts);
    let skip_rejected = skip_rejected.unwrap_or(true);
    let include_videos = include_videos.unwrap_or(true);
    // The card itself: everything under DCIM (plus video folders some cameras
    // keep outside it). A picked folder: just that folder, plus its
    // subfolders only if asked.
    let (roots, opts) = if mounts.contains(&picked) {
        (scan::card_roots(&picked, include_videos), scan::WalkOpts { recursive: true, skip_rejected, include_videos })
    } else {
        (
            vec![picked.clone()],
            scan::WalkOpts { recursive: include_subfolders.unwrap_or(false), skip_rejected, include_videos },
        )
    };
    let root = roots[0].clone();
    let vid = mount.to_string_lossy().to_string();
    let files = tauri::async_runtime::spawn_blocking(move || {
        roots.iter().flat_map(|r| scan::walk_with(r, &vid, opts)).collect::<Vec<_>>()
    })
    .await
    .map_err(|e| e.to_string())?;
    let groups = scan::pair(files);
    let emitter = app.clone();
    let session = Session::new_on(
        path,
        mount,
        root.clone(),
        groups,
        Arc::new(move |ev, payload| {
            let _ = emitter.emit(ev, payload);
        }),
    );
    state.dedupe.start_verifier(&session);
    state.dedupe.start_immich_worker(&session);
    let dedupe = state.dedupe.clone();
    scan::start_workers(&session, state.thumbs.clone(), Arc::new(move |s, idx| dedupe.evaluate_item(s, idx)));
    let items = (0..session.items.len() as u32).map(|i| session.view(i)).collect();
    let gen = session.gen;
    *state.session.lock() = Some(session);
    Ok(ScanResult { gen, root: root.to_string_lossy().to_string(), items })
}

#[tauri::command]
fn close_source(state: State<'_, AppState>) {
    if let Some(old) = state.session.lock().take() {
        old.cancelled.store(true, Ordering::SeqCst);
    }
}

#[tauri::command]
fn set_visible(state: State<'_, AppState>, gen: u64, start: usize, end: usize) {
    if let Some(s) = state.current().filter(|s| s.gen == gen) {
        s.set_visible(start, end);
    }
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Option<serde_json::Value> {
    state.catalog.get_setting("ui").and_then(|s| serde_json::from_str(&s).ok())
}

#[tauri::command]
fn save_settings(state: State<'_, AppState>, settings: serde_json::Value) -> Result<(), String> {
    state.catalog.set_setting("ui", &settings.to_string()).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DedupeArgs {
    dest_root: Option<String>,
    folder_tpl: String,
    name_tpl: String,
}

/// Update destination + templates. A new destination triggers an index walk;
/// a template change just re-evaluates duplicates.
#[tauri::command]
fn set_dedupe_config(app: AppHandle, state: State<'_, AppState>, args: DedupeArgs) {
    let root_changed = {
        let mut cfg = state.dedupe.cfg.write();
        let changed = cfg.dest_root != args.dest_root;
        cfg.dest_root = args.dest_root.clone();
        cfg.folder_tpl = args.folder_tpl;
        cfg.name_tpl = args.name_tpl;
        if changed {
            cfg.dest_ready = false;
        }
        changed
    };
    if root_changed {
        match args.dest_root {
            Some(root) => start_dest_index(&app, &state, root, false),
            None => reevaluate(&state),
        }
    } else {
        reevaluate(&state);
    }
}

fn reevaluate(state: &AppState) {
    if let Some(s) = state.current() {
        let d = state.dedupe.clone();
        std::thread::spawn(move || d.evaluate_all(&s));
    }
}

#[tauri::command]
fn rebuild_dest_index(app: AppHandle, state: State<'_, AppState>) {
    if let Some(root) = state.dedupe.cfg.read().dest_root.clone() {
        start_dest_index(&app, &state, root, true);
    }
}

fn start_dest_index(app: &AppHandle, state: &AppState, root: String, rebuild: bool) {
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut c = state.dest_cancel.lock();
        c.store(true, Ordering::SeqCst);
        *c = cancel.clone();
    }
    state.dedupe.cfg.write().dest_ready = false;
    let app = app.clone();
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let emit = |phase: &str, done: u64, total: u64| {
            let _ = app.emit("dest-index", serde_json::json!({ "root": root, "phase": phase, "done": done, "total": total }));
        };
        if !Path::new(&root).is_dir() {
            emit("missing", 0, 0);
            return;
        }
        if rebuild {
            let _ = state.catalog.dest_clear_hashes(&root);
        }
        emit("walking", 0, 0);
        let count = match dest_index::walk(&state.catalog, &root, &cancel, |n| emit("walking", n, 0)) {
            Ok(n) => n,
            Err(e) => {
                let _ = app.emit("dest-index", serde_json::json!({ "root": root, "phase": "error", "error": e.to_string() }));
                return;
            }
        };
        if cancel.load(Ordering::SeqCst) {
            return;
        }
        {
            let mut cfg = state.dedupe.cfg.write();
            if cfg.dest_root.as_deref() != Some(root.as_str()) {
                return;
            }
            cfg.dest_ready = true;
        }
        emit("ready", count, count);
        if let Some(s) = state.current() {
            state.dedupe.evaluate_all(&s);
        }
        if rebuild {
            dest_index::hash_all(&state.catalog, &root, &cancel, |d, t| emit("hashing", d, t));
            if !cancel.load(Ordering::SeqCst) {
                emit("ready", count, count);
            }
        }
    });
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Example {
    path: String,
    from_item: bool,
}

/// Example destination path for the template editor, using the first
/// selected (or any) scanned item, else the current time.
#[tauri::command]
fn example_path(state: State<'_, AppState>, folder_tpl: String, name_tpl: String, item_id: Option<u32>) -> Example {
    let session = state.current();
    let picked = session.as_ref().and_then(|s| {
        let id = item_id.filter(|&i| (i as usize) < s.items.len()).unwrap_or(0);
        let date = s.items.get(id as usize).and_then(|_| s.item_date(id))?;
        let meta = s.state[id as usize].lock().files[0].meta.clone().unwrap_or_default();
        let item = &s.items[id as usize];
        Some((date, meta, item.files[0].stem.clone(), item.files[0].ext.clone()))
    });
    let from_item = picked.is_some();
    let (date, meta, stem, ext) = picked.unwrap_or_else(|| {
        (chrono::Local::now().naive_local(), Default::default(), "P1010001".into(), "ORF".into())
    });
    let ctx = template::Ctx { date, make: meta.make.as_deref(), model: meta.model.as_deref(), original: &stem, seq: 1 };
    Example { path: template::plan(&folder_tpl, &name_tpl, &ctx, &ext).to_string_lossy().to_string(), from_item }
}

// ---- Immich -------------------------------------------------------------

const IMMICH_SETTINGS_KEY: &str = "immich";

fn load_immich_settings(catalog: &Catalog) -> ImmichSettings {
    catalog.get_setting(IMMICH_SETTINGS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn immich_ctx(settings: &ImmichSettings, key: Option<String>) -> Option<Arc<ImmichCtx>> {
    let key = key?;
    if !settings.enabled || settings.url.trim().is_empty() {
        return None;
    }
    let client = immich::Client::new(&settings.url, &key, settings.accept_invalid_certs).ok()?;
    Some(Arc::new(ImmichCtx { client, settings: settings.clone() }))
}

/// The Immich server and key as saved, for an app embedding the importer
/// that talks to the same server for more than duplicate checks (Immage
/// uploads its exports). The key stays on the Rust side.
pub struct ImmichConnection {
    pub url: String,
    pub api_key: String,
    pub accept_invalid_certs: bool,
}

/// The saved connection, whether or not duplicate checking is on; `None`
/// without a URL or a key.
pub fn immich_connection(app: &AppHandle) -> Option<ImmichConnection> {
    let state = app.try_state::<AppState>()?;
    let settings = load_immich_settings(&state.catalog);
    if settings.url.trim().is_empty() {
        return None;
    }
    Some(ImmichConnection {
        url: settings.url,
        api_key: immich::keychain::get()?,
        accept_invalid_certs: settings.accept_invalid_certs,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImmichSettingsView {
    #[serde(flatten)]
    settings: ImmichSettings,
    has_key: bool,
    required_permission: &'static str,
}

#[tauri::command]
fn get_immich_settings(state: State<'_, AppState>) -> ImmichSettingsView {
    ImmichSettingsView {
        settings: load_immich_settings(&state.catalog),
        has_key: immich::keychain::get().is_some(),
        required_permission: immich::REQUIRED_PERMISSION,
    }
}

/// Save settings; a non-empty `api_key` replaces the stored key. The key is
/// kept in the OS keychain and never returned to the UI.
#[tauri::command]
fn save_immich_settings(state: State<'_, AppState>, mut settings: ImmichSettings, api_key: Option<String>) -> Result<(), String> {
    settings.url = immich::normalize_url(&settings.url);
    if let Some(k) = api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        immich::keychain::set(k)?;
    }
    state
        .catalog
        .set_setting(IMMICH_SETTINGS_KEY, &serde_json::to_string(&settings).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let ctx = if settings.enabled { immich_ctx(&settings, immich::keychain::get()) } else { None };
    *state.dedupe.immich.write() = ctx;
    reevaluate(&state);
    Ok(())
}

#[tauri::command]
fn forget_immich_key(state: State<'_, AppState>) -> Result<(), String> {
    immich::keychain::delete()?;
    *state.dedupe.immich.write() = None;
    reevaluate(&state);
    Ok(())
}

/// Test a connection. Uses `api_key` if given, else the stored key.
#[tauri::command]
async fn test_immich(settings: ImmichSettings, api_key: Option<String>) -> immich::TestResult {
    tauri::async_runtime::spawn_blocking(move || {
        let key = api_key.filter(|k| !k.trim().is_empty()).or_else(immich::keychain::get);
        let Some(key) = key else {
            return immich::TestResult { error: Some("Enter an API key".into()), ..Default::default() };
        };
        match immich::Client::new(&settings.url, &key, settings.accept_invalid_certs) {
            Ok(c) => c.test(),
            Err(e) => immich::TestResult { error: Some(e), ..Default::default() },
        }
    })
    .await
    .unwrap_or_default()
}

fn dest_on_source(dest: &Path, source_mount: &Path) -> Result<bool, String> {
    let dest = dest.canonicalize().map_err(|e| e.to_string())?;
    let mount = source_mount.canonicalize().map_err(|e| e.to_string())?;
    Ok(dest.starts_with(mount))
}

/// Store a video frame decoded by the UI as the item's thumbnail. The image
/// is decoded and re-encoded, so only a valid JPEG ever reaches the cache.
#[tauri::command]
fn save_video_thumb(state: State<'_, AppState>, gen: u64, item_id: u32, jpeg_base64: String) -> Result<(), String> {
    use base64::Engine;
    let s = state.current().filter(|s| s.gen == gen).ok_or("Scan has changed")?;
    let item = s.items.get(item_id as usize).ok_or("Unknown item")?;
    let f = &item.files[0];
    if f.kind != metadata::Kind::Video {
        return Err("Not a video".into());
    }
    let b64 = jpeg_base64.split_once(',').map(|(_, b)| b).unwrap_or(&jpeg_base64);
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| e.to_string())?;
    let thumb = preview::make_thumb(&bytes, None)?;
    let key = scan::video_thumb_key(&f.key);
    state.thumbs.put(&key, &thumb).map_err(|e| e.to_string())?;
    s.state[item_id as usize].lock().thumb = Some(key);
    s.mark_dirty(item_id);
    Ok(())
}

/// Subfolders of a folder, for the source tree.
#[tauri::command]
async fn list_folders(path: String) -> Vec<scan::FolderEntry> {
    tauri::async_runtime::spawn_blocking(move || scan::list_folders(Path::new(&path))).await.unwrap_or_default()
}

#[tauri::command]
fn start_import(app: AppHandle, state: State<'_, AppState>, opts: import::ImportOpts) -> Result<(), String> {
    let session = state.current().ok_or("No source selected")?;
    if !Path::new(&opts.dest_root).is_dir() {
        return Err(format!("Destination {} does not exist", opts.dest_root));
    }
    // Refuse to import onto the source card, even into another folder of it.
    if dest_on_source(Path::new(&opts.dest_root), &session.mount)? {
        return Err("The destination is on the source card. The card is never written to.".into());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut slot = state.import_cancel.lock();
        if slot.is_some() {
            return Err("An import is already running".into());
        }
        *slot = Some(cancel.clone());
    }
    let catalog = state.catalog.clone();
    std::thread::spawn(move || {
        let summary = import::run(catalog, session.clone(), opts, cancel);
        let state = app.state::<AppState>();
        *state.import_cancel.lock() = None;
        let _ = app.emit("import-done", &summary);
        // Imported files are now duplicates: refresh their status in the grid.
        state.dedupe.evaluate_all(&session);
    });
    Ok(())
}

#[tauri::command]
fn cancel_import(state: State<'_, AppState>) {
    if let Some(c) = state.import_cancel.lock().as_ref() {
        c.store(true, Ordering::SeqCst);
    }
}

fn spawn_volume_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last = volumes::list();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let now = volumes::list();
            if now != last {
                let _ = app.emit("volumes-changed", &now);
                last = now;
            }
        }
    });
}

/// The importer as a Tauri plugin: its commands (`plugin:immporter|<name>`),
/// state, and the `thumb://` and `media://` schemes its UI loads from. The
/// app registers the opener and dialog plugins its UI also uses.
pub fn init(config: Config) -> TauriPlugin<Wry> {
    tauri::plugin::Builder::new("immporter")
        .register_asynchronous_uri_scheme_protocol("thumb", |ctx, request, responder| {
            let key = request.uri().path().trim_start_matches('/').to_string();
            let thumbs = ctx.app_handle().state::<AppState>().thumbs.clone();
            std::thread::spawn(move || {
                let resp = match thumbs.get(&key) {
                    Some(bytes) => tauri::http::Response::builder()
                        .header("Content-Type", "image/jpeg")
                        .header("Cache-Control", "max-age=31536000, immutable")
                        .body(bytes),
                    None => tauri::http::Response::builder().status(404).body(Vec::new()),
                };
                responder.respond(resp.unwrap());
            });
        })
        .register_asynchronous_uri_scheme_protocol("media", |ctx, request, responder| {
            let path = request.uri().path().to_string();
            let range = request.headers().get("range").and_then(|v| v.to_str().ok()).map(String::from);
            let session = ctx.app_handle().state::<AppState>().current();
            std::thread::spawn(move || {
                let r = media::serve(session.as_deref(), &path, range.as_deref());
                let mut b = tauri::http::Response::builder().status(r.status);
                for (k, v) in r.headers {
                    b = b.header(k, v);
                }
                responder.respond(b.body(r.body).unwrap());
            });
        })
        .setup(move |app, _api| {
            let (mut data, mut cache) = (app.path().app_data_dir()?, app.path().app_cache_dir()?);
            if let Some(sub) = &config.data_subdir {
                data.push(sub);
                cache.push(sub);
            }
            // each app keeps its own Immich key
            immich::keychain::set_service(&app.config().identifier);

            std::fs::create_dir_all(&data)?;
            let catalog = Arc::new(Catalog::open(&data.join("catalog.sqlite"))?);
            let recovered = import::recover(&catalog);
            if recovered > 0 {
                eprintln!("Cleaned up {recovered} interrupted import file(s)");
            }
            let thumbs = Arc::new(ThumbCache::new(cache.join("thumbs"))?);
            let dedupe = Arc::new(Deduper::new(catalog.clone(), DedupeCfg::default()));
            // tried once, allowed or not: reading the old entry asks the
            // user, and a refusal must not ask again at every launch
            if let Some(old) = &config.keychain_from {
                let done = format!("keychain_from:{old}");
                if catalog.get_setting(&done).is_none() {
                    immich::keychain::migrate_from(old);
                    let _ = catalog.set_setting(&done, "1");
                }
            }
            let immich_settings = load_immich_settings(&catalog);
            if immich_settings.enabled {
                *dedupe.immich.write() = immich_ctx(&immich_settings, immich::keychain::get());
            }
            app.manage(AppState {
                catalog,
                thumbs,
                dedupe,
                session: Mutex::new(None),
                dest_cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
                import_cancel: Mutex::new(None),
                watching: AtomicBool::new(false),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            dev_autoload_source,
            list_volumes,
            list_folders,
            save_video_thumb,
            scan_source,
            close_source,
            set_visible,
            get_settings,
            save_settings,
            set_dedupe_config,
            rebuild_dest_index,
            example_path,
            get_immich_settings,
            save_immich_settings,
            forget_immich_key,
            test_immich,
            start_import,
            cancel_import,
        ])
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Picking a subfolder (e.g. _Selected) must not allow importing onto
    /// another folder of the same card.
    #[test]
    fn destination_anywhere_on_the_card_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let card = d.path().join("card");
        let picked = card.join("DCIM/100OMSYS/_Selected");
        std::fs::create_dir_all(&picked).unwrap();
        std::fs::create_dir_all(card.join("other")).unwrap();
        std::fs::create_dir_all(d.path().join("lib")).unwrap();
        let mount = scan::mount_for(&picked, &[card.clone()]);
        assert!(dest_on_source(&card.join("other"), &mount).unwrap());
        assert!(dest_on_source(&picked, &mount).unwrap());
        assert!(!dest_on_source(&d.path().join("lib"), &mount).unwrap());
    }
}
