//! Which folders the importer may read from and write to.
//!
//! The page asking for a scan or an import is not trusted with a path of its
//! own choosing: folders come from dialogs the Rust side opens, and every
//! command that reads or writes a folder checks it against what was granted.
//! A source grant lasts for the session and never allows writing; a
//! destination grant is kept in the catalog under a key only Rust writes.

use std::path::{Path, PathBuf};

use crate::catalog::Catalog;

pub const DEST_KEY: &str = "grants.dest";
pub const MIGRATED_KEY: &str = "grants.migrated";
pub const MAX_DESTS: usize = 16;

/// Resolve symlinks and `..`, and drop the Windows `\\?\` prefix canonicalizing
/// adds. The plain form is what the page shows and passes back, and it
/// canonicalizes to the same path again.
pub fn canon(p: &Path) -> Result<PathBuf, String> {
    p.canonicalize().map(|c| strip_verbatim(&c)).map_err(|e| format!("{} is not available: {e}", p.display()))
}

pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        // only a drive path (`C:\...`); other verbatim forms stay as they are
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

#[derive(Debug, Default)]
pub struct Grants {
    sources: Vec<PathBuf>,
    dests: Vec<PathBuf>,
    /// Debug builds only (`SDI_AUTOLOAD_SOURCE` / `SDI_AUTOLOAD_DEST`): the
    /// caller passes `None` in a release build, so there is no bypass there.
    dev_source: Option<PathBuf>,
    dev_dest: Option<PathBuf>,
}

/// The destination an earlier version kept in the page's settings, if the
/// blob names one.
fn migration_dest(ui_json: &str) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(ui_json).ok()?;
    let d = v.get("destRoot")?.as_str()?.trim();
    (!d.is_empty()).then(|| PathBuf::from(d))
}

fn parse_dests(json: &str) -> Vec<PathBuf> {
    let list: Vec<String> = serde_json::from_str(json).unwrap_or_default();
    list.into_iter().map(PathBuf::from).take(MAX_DESTS).collect()
}

fn store_dests(catalog: &Catalog, dests: &[PathBuf]) -> Result<(), String> {
    let list: Vec<String> = dests.iter().map(|d| d.to_string_lossy().to_string()).collect();
    let json = serde_json::to_string(&list).map_err(|e| e.to_string())?;
    catalog.set_setting(DEST_KEY, &json).map_err(|e| e.to_string())
}

/// Strict one-time migration: before destinations were granted, the page kept
/// its choice in the "ui" settings. Only the current destination, if it still
/// exists, carries over; recent ones need to be picked again. Runs once, so
/// nothing the page writes later into "ui" can become a grant.
fn migrate(catalog: &Catalog) {
    if catalog.get_setting(DEST_KEY).is_some() || catalog.get_setting(MIGRATED_KEY).is_some() {
        return;
    }
    let current = catalog
        .get_setting("ui")
        .and_then(|ui| migration_dest(&ui))
        .and_then(|d| canon(&d).ok())
        .filter(|c| c.is_dir());
    if let Some(c) = current {
        let _ = store_dests(catalog, &[c]);
    }
    let _ = catalog.set_setting(MIGRATED_KEY, "1");
}

impl Grants {
    pub fn load(catalog: &Catalog, dev_source: Option<PathBuf>, dev_dest: Option<PathBuf>) -> Self {
        migrate(catalog);
        let dests = catalog.get_setting(DEST_KEY).map(|j| parse_dests(&j)).unwrap_or_default();
        Grants {
            sources: Vec::new(),
            dests,
            dev_source: dev_source.and_then(|p| canon(&p).ok()),
            dev_dest: dev_dest.and_then(|p| canon(&p).ok()),
        }
    }

    /// Allow reading a folder picked in the source dialog, until the app quits.
    pub fn grant_source(&mut self, p: &Path) -> Result<PathBuf, String> {
        let c = canon(p)?;
        if !c.is_dir() {
            return Err(format!("{} is not a folder", c.display()));
        }
        if !self.sources.contains(&c) {
            self.sources.push(c.clone());
        }
        Ok(c)
    }

    /// Allow writing into a folder picked in the destination dialog, kept
    /// across launches. The most recent comes first; the oldest drop off.
    pub fn grant_dest(&mut self, catalog: &Catalog, p: &Path) -> Result<PathBuf, String> {
        let c = canon(p)?;
        if !c.is_dir() {
            return Err(format!("{} is not a folder", c.display()));
        }
        let mut next: Vec<PathBuf> = std::iter::once(c.clone()).chain(self.dests.iter().filter(|d| **d != c).cloned()).collect();
        next.truncate(MAX_DESTS);
        store_dests(catalog, &next)?;
        self.dests = next;
        Ok(c)
    }

    pub fn forget_dest(&mut self, catalog: &Catalog, p: &Path) -> Result<(), String> {
        let canonical = canon(p).ok();
        let next: Vec<PathBuf> =
            self.dests.iter().filter(|d| d.as_path() != p && Some(*d) != canonical.as_ref()).cloned().collect();
        store_dests(catalog, &next)?;
        self.dests = next;
        Ok(())
    }

    pub fn dests(&self) -> &[PathBuf] {
        &self.dests
    }

    /// The canonical path if `p` is inside a granted source, a mounted volume
    /// (`mounts`, as the volume list reports them) or the debug autoload source.
    pub fn check_source(&self, p: &Path, mounts: &[PathBuf]) -> Result<PathBuf, String> {
        let c = canon(p)?;
        let in_mount = mounts.iter().filter_map(|m| canon(m).ok()).any(|m| c.starts_with(m));
        let ok = in_mount
            || self.sources.iter().any(|s| c.starts_with(s))
            || self.dev_source.as_ref().is_some_and(|s| c.starts_with(s));
        if ok { Ok(c) } else { Err(format!("{} was not chosen as a source", p.display())) }
    }

    /// The canonical path if `p` is inside a granted destination. A source
    /// grant never counts.
    pub fn check_dest(&self, p: &Path) -> Result<PathBuf, String> {
        let c = canon(p)?;
        let ok = self.dests.iter().any(|d| c.starts_with(d)) || self.dev_dest.as_ref().is_some_and(|d| c.starts_with(d));
        if ok { Ok(c) } else { Err(format!("{} was not chosen as a destination", p.display())) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn catalog(dir: &Path) -> Catalog {
        Catalog::open(&dir.join("catalog.sqlite")).unwrap()
    }

    fn mk(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn setup() -> (tempfile::TempDir, Catalog) {
        let t = tempfile::tempdir().unwrap();
        let c = catalog(&mk(t.path(), "data"));
        (t, c)
    }

    #[test]
    fn inside_outside_and_sibling_prefix() {
        let (t, c) = setup();
        let a = mk(t.path(), "a");
        let ab = mk(t.path(), "ab");
        let sub = mk(&a, "sub");
        let mut g = Grants::load(&c, None, None);
        g.grant_dest(&c, &a).unwrap();
        assert!(g.check_dest(&a).is_ok());
        assert!(g.check_dest(&sub).is_ok());
        // /x/a granted must not allow /x/ab
        assert!(g.check_dest(&ab).is_err());
        assert!(g.check_dest(t.path()).is_err());
    }

    #[test]
    fn dotdot_escape_is_refused() {
        let (t, c) = setup();
        let a = mk(t.path(), "a");
        mk(t.path(), "other");
        let mut g = Grants::load(&c, None, None);
        g.grant_dest(&c, &a).unwrap();
        assert!(g.check_dest(&a.join("../other")).is_err());
        assert!(g.check_dest(&a.join("../a")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused() {
        let (t, c) = setup();
        let a = mk(t.path(), "a");
        let outside = mk(t.path(), "outside");
        std::os::unix::fs::symlink(&outside, a.join("link")).unwrap();
        let mut g = Grants::load(&c, None, None);
        g.grant_dest(&c, &a).unwrap();
        g.grant_source(&a).unwrap();
        assert!(g.check_dest(&a.join("link")).is_err());
        assert!(g.check_source(&a.join("link"), &[]).is_err());
    }

    #[test]
    fn nonexistent_path_is_refused() {
        let (t, c) = setup();
        let a = mk(t.path(), "a");
        let mut g = Grants::load(&c, None, None);
        g.grant_dest(&c, &a).unwrap();
        assert!(g.check_dest(&a.join("missing")).is_err());
        assert!(g.grant_dest(&c, &t.path().join("nope")).is_err());
        assert!(g.grant_source(&t.path().join("nope")).is_err());
    }

    #[test]
    fn source_grants_are_session_only_and_never_write() {
        let (t, c) = setup();
        let card = mk(t.path(), "card");
        let mut g = Grants::load(&c, None, None);
        assert!(g.check_source(&card, &[]).is_err());
        g.grant_source(&card).unwrap();
        assert!(g.check_source(&card, &[]).is_ok());
        assert!(g.check_source(&mk(&card, "DCIM"), &[]).is_ok());
        // a source grant does not authorize writing
        assert!(g.check_dest(&card).is_err());
        // and is gone after a restart
        let again = Grants::load(&c, None, None);
        assert!(again.check_source(&card, &[]).is_err());
    }

    #[test]
    fn mounted_volumes_are_readable_without_a_grant() {
        let (t, c) = setup();
        let vol = mk(t.path(), "vol");
        let g = Grants::load(&c, None, None);
        assert!(g.check_source(&mk(&vol, "DCIM"), &[vol.clone()]).is_ok());
        assert!(g.check_source(&mk(t.path(), "elsewhere"), &[vol]).is_err());
        assert!(g.check_dest(t.path()).is_err());
    }

    #[test]
    fn destinations_survive_reopening_the_catalog() {
        let t = tempfile::tempdir().unwrap();
        let data = mk(t.path(), "data");
        let lib = mk(t.path(), "lib");
        {
            let c = catalog(&data);
            let mut g = Grants::load(&c, None, None);
            g.grant_dest(&c, &lib).unwrap();
        }
        let c = catalog(&data);
        let g = Grants::load(&c, None, None);
        assert!(g.check_dest(&mk(&lib, "2026")).is_ok());
        assert_eq!(g.dests().len(), 1);
    }

    #[test]
    fn forged_ui_setting_does_not_grant() {
        let (t, c) = setup();
        let lib = mk(t.path(), "lib");
        let g = Grants::load(&c, None, None);
        // the page can write the "ui" blob, including after the migration ran
        c.set_setting("ui", &serde_json::json!({ "destRoot": lib }).to_string()).unwrap();
        assert!(g.check_dest(&lib).is_err());
        let g2 = Grants::load(&c, None, None);
        assert!(g2.check_dest(&lib).is_err());
    }

    #[test]
    fn migration_is_strict() {
        let t = tempfile::tempdir().unwrap();
        let cur = mk(t.path(), "current");
        let recent = mk(t.path(), "recent");
        let c = catalog(&mk(t.path(), "data"));
        let ui = serde_json::json!({ "destRoot": cur, "recentDests": [cur, recent] });
        c.set_setting("ui", &ui.to_string()).unwrap();
        let g = Grants::load(&c, None, None);
        assert!(g.check_dest(&cur).is_ok());
        assert!(g.check_dest(&recent).is_err());
        assert_eq!(c.get_setting(MIGRATED_KEY).as_deref(), Some("1"));
        // once migrated, later page-written settings never carry over
        let ui = serde_json::json!({ "destRoot": recent });
        c.set_setting("ui", &ui.to_string()).unwrap();
        assert!(Grants::load(&c, None, None).check_dest(&recent).is_err());
    }

    #[test]
    fn migration_skips_a_missing_destination_and_marks_done() {
        let (t, c) = setup();
        let gone = t.path().join("gone");
        c.set_setting("ui", &serde_json::json!({ "destRoot": gone }).to_string()).unwrap();
        let g = Grants::load(&c, None, None);
        assert!(g.dests().is_empty());
        assert_eq!(c.get_setting(MIGRATED_KEY).as_deref(), Some("1"));
        let g = Grants::load(&Catalog::open(&t.path().join("data/catalog.sqlite")).unwrap(), None, None);
        assert!(g.dests().is_empty());
    }

    #[test]
    fn migration_does_not_override_existing_grants() {
        let (t, c) = setup();
        let lib = mk(t.path(), "lib");
        let other = mk(t.path(), "other");
        let mut g = Grants::load(&c, None, None);
        g.grant_dest(&c, &lib).unwrap();
        c.set_setting("ui", &serde_json::json!({ "destRoot": other }).to_string()).unwrap();
        let g = Grants::load(&c, None, None);
        assert!(g.check_dest(&other).is_err());
        assert!(g.check_dest(&lib).is_ok());
    }

    #[test]
    fn migration_dest_reads_only_dest_root() {
        assert_eq!(migration_dest(r#"{"destRoot":"/x"}"#), Some(PathBuf::from("/x")));
        assert_eq!(migration_dest(r#"{"destRoot":null}"#), None);
        assert_eq!(migration_dest(r#"{"destRoot":" "}"#), None);
        assert_eq!(migration_dest("not json"), None);
    }

    #[test]
    fn list_is_capped_most_recent_first_and_deduplicated() {
        let (t, c) = setup();
        let mut g = Grants::load(&c, None, None);
        let dirs: Vec<PathBuf> = (0..MAX_DESTS + 3).map(|i| mk(t.path(), &format!("d{i}"))).collect();
        for d in &dirs {
            g.grant_dest(&c, d).unwrap();
        }
        assert_eq!(g.dests().len(), MAX_DESTS);
        assert!(g.check_dest(&dirs[0]).is_err());
        assert!(g.check_dest(dirs.last().unwrap()).is_ok());
        assert_eq!(g.dests()[0], canon(dirs.last().unwrap()).unwrap());
        g.grant_dest(&c, &dirs[5]).unwrap();
        assert_eq!(g.dests().len(), MAX_DESTS);
        assert_eq!(g.dests()[0], canon(&dirs[5]).unwrap());
        assert_eq!(g.dests().iter().filter(|d| **d == canon(&dirs[5]).unwrap()).count(), 1);
    }

    #[test]
    fn forgetting_a_destination_revokes_it() {
        let (t, c) = setup();
        let lib = mk(t.path(), "lib");
        let mut g = Grants::load(&c, None, None);
        g.grant_dest(&c, &lib).unwrap();
        g.forget_dest(&c, &lib).unwrap();
        assert!(g.check_dest(&lib).is_err());
        assert!(Grants::load(&c, None, None).check_dest(&lib).is_err());
    }

    #[test]
    fn debug_hooks_only_apply_when_given() {
        let (t, c) = setup();
        let s = mk(t.path(), "s");
        let d = mk(t.path(), "d");
        assert!(Grants::load(&c, None, None).check_source(&s, &[]).is_err());
        let g = Grants::load(&c, Some(s.clone()), Some(d.clone()));
        assert!(g.check_source(&s, &[]).is_ok());
        assert!(g.check_dest(&d).is_ok());
        assert!(g.check_dest(&s).is_err());
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\Photos")), PathBuf::from(r"C:\Photos"));
        assert_eq!(strip_verbatim(Path::new(r"\\?\UNC\srv\share\x")), PathBuf::from(r"\\srv\share\x"));
        assert_eq!(strip_verbatim(Path::new("/a/b")), PathBuf::from("/a/b"));
    }
}
