//! Full pipeline on real photos: scan → dedupe → import → rescan.
//! `SAMPLE_DIR=~/Pictures/2026/August cargo test --release --test end_to_end -- --ignored --nocapture`

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri_plugin_immporter::catalog::Catalog;
use tauri_plugin_immporter::dedupe::{DedupeCfg, Deduper};
use tauri_plugin_immporter::import::{self, ImportOpts};
use tauri_plugin_immporter::preview::ThumbCache;
use tauri_plugin_immporter::scan::{self, FileStatus, Session};

fn scan(dir: &Path, deduper: &Arc<Deduper>, thumbs: &Arc<ThumbCache>) -> Arc<Session> {
    let t = Instant::now();
    let root = scan::source_root(dir);
    let s = Session::new("card".into(), root.clone(), scan::pair(scan::walk(&root, "card")), Arc::new(|_, _| {}));
    println!("walk+pair: {} items in {:?}", s.items.len(), t.elapsed());
    deduper.start_verifier(&s);
    let d = deduper.clone();
    scan::start_workers(&s, thumbs.clone(), Arc::new(move |s, i| d.evaluate_item(s, i)));
    for _ in 0..3000 {
        let v: Vec<_> = (0..s.items.len() as u32).map(|i| s.view(i)).collect();
        if v.iter().all(|x| !matches!(x.status, FileStatus::Pending | FileStatus::Checking)) {
            println!("metadata+dedupe settled in {:?}", t.elapsed());
            return s;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("scan did not settle");
}

#[test]
#[ignore]
fn import_real_photos() {
    let sample = std::env::var("SAMPLE_DIR").expect("SAMPLE_DIR");
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("card/DCIM/100OMSYS");
    std::fs::create_dir_all(&card).unwrap();
    let mut n = 0;
    for e in std::fs::read_dir(&sample).unwrap().flatten() {
        let p = e.path();
        let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if ["orf", "jpg", "dng"].contains(&ext.as_str()) && n < 12 {
            std::fs::copy(&p, card.join(p.file_name().unwrap())).unwrap();
            n += 1;
        }
    }
    let lib = tmp.path().join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    let lib_s = lib.to_string_lossy().to_string();

    let catalog = Arc::new(Catalog::open(&tmp.path().join("cat.sqlite")).unwrap());
    let cfg = DedupeCfg { dest_root: Some(lib_s.clone()), folder_tpl: "{YYYY}/{MMMM}".into(), name_tpl: "{original}".into(), dest_ready: true };
    let deduper = Arc::new(Deduper::new(catalog.clone(), cfg));
    let thumbs = Arc::new(ThumbCache::new(tmp.path().join("thumbs")).unwrap());

    let s = scan(&tmp.path().join("card"), &deduper, &thumbs);
    for i in 0..s.items.len() as u32 {
        let v = s.view(i);
        println!("{} {:?} {:?} thumb={}", v.name, v.capture, v.status, v.thumb.is_some());
        assert_eq!(v.status, FileStatus::New);
    }

    let t = Instant::now();
    let ids: Vec<u32> = (0..s.items.len() as u32).collect();
    let summary = import::run(
        catalog.clone(),
        s.clone(),
        ImportOpts { dest_root: lib_s.clone(), folder_tpl: "{YYYY}/{MMMM}".into(), name_tpl: "{original}".into(), item_ids: ids, paranoid: true },
        Arc::new(AtomicBool::new(false)),
    );
    let secs = t.elapsed().as_secs_f64();
    println!("import: {} imported, {} failed, {:.1} MB/s", summary.imported, summary.failed, summary.bytes as f64 / secs / 1e6);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.imported as usize, n);
    s.cancelled.store(true, Ordering::SeqCst);

    // Every imported file is byte-identical to its source.
    for e in walkdir::WalkDir::new(&lib).into_iter().flatten().filter(|e| e.file_type().is_file()) {
        let src = card.join(e.file_name());
        assert_eq!(std::fs::read(e.path()).unwrap(), std::fs::read(&src).unwrap(), "{:?}", e.path());
        println!("  {}", e.path().strip_prefix(&lib).unwrap().display());
    }

    // Rescan with a fresh catalog: everything is found in the destination folder.
    let catalog2 = Arc::new(Catalog::open_in_memory().unwrap());
    tauri_plugin_immporter::dest_index::walk(&catalog2, &lib_s, &AtomicBool::new(false), |_| {}).unwrap();
    let cfg = DedupeCfg { dest_root: Some(lib_s.clone()), folder_tpl: "{YYYY}/{MMMM}".into(), name_tpl: "{original}".into(), dest_ready: true };
    let deduper2 = Arc::new(Deduper::new(catalog2, cfg));
    let s2 = scan(&tmp.path().join("card"), &deduper2, &thumbs);
    for i in 0..s2.items.len() as u32 {
        assert_eq!(s2.view(i).status, FileStatus::InDestination);
    }
    s2.cancelled.store(true, Ordering::SeqCst);
}
