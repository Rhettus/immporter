//! SQLite catalog (in the app data dir): previously imported files, an index
//! of the destination library, settings and the import journal.

use std::path::Path;

use chrono::NaiveDateTime;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};

use crate::metadata::PhotoMeta;

pub struct Catalog {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct ImportedRow {
    pub id: i64,
    pub capture: Option<NaiveDateTime>,
    pub serial: Option<String>,
    pub image_number: Option<u32>,
    pub blake3: String,
    pub dest_path: String,
}

#[derive(Debug, Clone)]
pub struct DestRow {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub blake3: Option<String>,
}

pub struct NewImported<'a> {
    pub orig_name: &'a str,
    pub size: u64,
    pub meta: &'a PhotoMeta,
    pub blake3: &'a str,
    pub dest_path: &'a str,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
CREATE TABLE IF NOT EXISTS imported (
    id INTEGER PRIMARY KEY,
    orig_name TEXT NOT NULL,
    size INTEGER NOT NULL,
    capture_ts TEXT,
    camera_serial TEXT,
    image_number INTEGER,
    blake3 TEXT NOT NULL,
    dest_path TEXT NOT NULL,
    imported_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS imported_name_size ON imported(orig_name, size);
CREATE INDEX IF NOT EXISTS imported_hash ON imported(blake3);
CREATE UNIQUE INDEX IF NOT EXISTS imported_unique ON imported(blake3, dest_path);

CREATE TABLE IF NOT EXISTS dest_files (
    path TEXT PRIMARY KEY,
    root TEXT NOT NULL,
    name TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    blake3 TEXT,
    seen INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS dest_name_size ON dest_files(name, size);
CREATE INDEX IF NOT EXISTS dest_root ON dest_files(root);

CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS import_sessions (
    id INTEGER PRIMARY KEY,
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT,
    dest_root TEXT NOT NULL,
    status TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS import_items (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES import_sessions(id),
    src_path TEXT NOT NULL,
    dest_path TEXT,
    temp_path TEXT,
    status TEXT NOT NULL,
    error TEXT,
    blake3 TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS import_items_status ON import_items(status);
"#;

fn ts(d: &Option<NaiveDateTime>) -> Option<String> {
    d.map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
}

impl Catalog {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    // ---- settings -------------------------------------------------------

    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.conn
            .lock()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.lock().execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- imported -------------------------------------------------------

    pub fn imported_by_name_size(&self, name: &str, size: u64) -> Vec<ImportedRow> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, capture_ts, camera_serial, image_number, blake3, dest_path
                 FROM imported WHERE orig_name = ?1 AND size = ?2",
            )
            .unwrap();
        stmt.query_map(params![name, size as i64], |r| {
            Ok(ImportedRow {
                id: r.get(0)?,
                capture: r
                    .get::<_, Option<String>>(1)?
                    .and_then(|s| NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S").ok()),
                serial: r.get(2)?,
                image_number: r.get(3)?,
                blake3: r.get(4)?,
                dest_path: r.get(5)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    pub fn record_imported(&self, n: &NewImported) -> rusqlite::Result<()> {
        self.conn.lock().execute(
            "INSERT OR IGNORE INTO imported(orig_name, size, capture_ts, camera_serial, image_number, blake3, dest_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                n.orig_name,
                n.size as i64,
                ts(&n.meta.capture),
                n.meta.serial,
                n.meta.image_number,
                n.blake3,
                n.dest_path
            ],
        )?;
        Ok(())
    }

    // ---- destination index ---------------------------------------------

    /// Mark every row of a root unseen before a re-walk.
    pub fn dest_begin_walk(&self, root: &str) -> rusqlite::Result<()> {
        self.conn.lock().execute("UPDATE dest_files SET seen = 0 WHERE root = ?1", [root])?;
        Ok(())
    }

    /// Upsert a batch of (path, name, size, mtime). Rows whose size or mtime
    /// changed lose their cached hash.
    pub fn dest_upsert_batch(&self, root: &str, rows: &[(String, String, u64, i64)]) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO dest_files(path, root, name, size, mtime, seen) VALUES (?1, ?2, ?3, ?4, ?5, 1)
                 ON CONFLICT(path) DO UPDATE SET
                    seen = 1,
                    root = excluded.root,
                    blake3 = CASE WHEN dest_files.size = excluded.size AND dest_files.mtime = excluded.mtime
                                  THEN dest_files.blake3 ELSE NULL END,
                    size = excluded.size,
                    mtime = excluded.mtime",
            )?;
            for (path, name, size, mtime) in rows {
                stmt.execute(params![path, root, name, *size as i64, mtime])?;
            }
        }
        tx.commit()
    }

    /// Drop rows not seen during the walk (files deleted from the library).
    pub fn dest_end_walk(&self, root: &str) -> rusqlite::Result<usize> {
        self.conn.lock().execute("DELETE FROM dest_files WHERE root = ?1 AND seen = 0", [root])
    }

    pub fn dest_clear_hashes(&self, root: &str) -> rusqlite::Result<()> {
        self.conn.lock().execute("UPDATE dest_files SET blake3 = NULL WHERE root = ?1", [root])?;
        Ok(())
    }

    fn dest_row(r: &rusqlite::Row) -> rusqlite::Result<DestRow> {
        Ok(DestRow { path: r.get(0)?, size: r.get::<_, i64>(1)? as u64, mtime: r.get(2)?, blake3: r.get(3)? })
    }

    pub fn dest_by_name_size(&self, root: &str, name: &str, size: u64) -> Vec<DestRow> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT path, size, mtime, blake3 FROM dest_files WHERE root = ?1 AND name = ?2 AND size = ?3",
            )
            .unwrap();
        stmt.query_map(params![root, name, size as i64], Self::dest_row)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    pub fn dest_by_path(&self, path: &str) -> Option<DestRow> {
        self.conn
            .lock()
            .query_row("SELECT path, size, mtime, blake3 FROM dest_files WHERE path = ?1", [path], Self::dest_row)
            .optional()
            .ok()
            .flatten()
    }

    pub fn dest_by_hash(&self, root: &str, hash: &str) -> Option<DestRow> {
        self.conn
            .lock()
            .query_row(
                "SELECT path, size, mtime, blake3 FROM dest_files WHERE root = ?1 AND blake3 = ?2 LIMIT 1",
                params![root, hash],
                Self::dest_row,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn dest_unhashed(&self, root: &str) -> Vec<DestRow> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT path, size, mtime, blake3 FROM dest_files WHERE root = ?1 AND blake3 IS NULL")
            .unwrap();
        stmt.query_map([root], Self::dest_row).map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    /// Store a hash only if the file is still the size/mtime we hashed.
    pub fn dest_set_hash(&self, path: &str, size: u64, mtime: i64, hash: &str) {
        let _ = self.conn.lock().execute(
            "UPDATE dest_files SET blake3 = ?4 WHERE path = ?1 AND size = ?2 AND mtime = ?3",
            params![path, size as i64, mtime, hash],
        );
    }

    pub fn dest_insert_verified(&self, root: &str, path: &str, name: &str, size: u64, mtime: i64, hash: &str) {
        let _ = self.conn.lock().execute(
            "INSERT INTO dest_files(path, root, name, size, mtime, blake3, seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)
             ON CONFLICT(path) DO UPDATE SET root = excluded.root, name = excluded.name, size = excluded.size,
                mtime = excluded.mtime, blake3 = excluded.blake3, seen = 1",
            params![path, root, name, size as i64, mtime, hash],
        );
    }

    pub fn dest_count(&self, root: &str) -> u64 {
        self.conn
            .lock()
            .query_row("SELECT COUNT(*) FROM dest_files WHERE root = ?1", [root], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as u64
    }

    // ---- import journal --------------------------------------------------

    pub fn session_start(&self, dest_root: &str) -> rusqlite::Result<i64> {
        let conn = self.conn.lock();
        conn.execute("INSERT INTO import_sessions(dest_root, status) VALUES (?1, 'running')", [dest_root])?;
        Ok(conn.last_insert_rowid())
    }

    pub fn session_finish(&self, id: i64, status: &str) {
        let _ = self.conn.lock().execute(
            "UPDATE import_sessions SET status = ?2, finished_at = datetime('now') WHERE id = ?1",
            params![id, status],
        );
    }

    pub fn item_start(&self, session: i64, src: &str, dest: &str, temp: &str) -> rusqlite::Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO import_items(session_id, src_path, dest_path, temp_path, status) VALUES (?1, ?2, ?3, ?4, 'copying')",
            params![session, src, dest, temp],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn item_update(&self, id: i64, status: &str, dest: Option<&str>, hash: Option<&str>, error: Option<&str>) {
        let _ = self.conn.lock().execute(
            "UPDATE import_items SET status = ?2, dest_path = COALESCE(?3, dest_path), blake3 = COALESCE(?4, blake3),
                error = ?5, updated_at = datetime('now') WHERE id = ?1",
            params![id, status, dest, hash, error],
        );
    }

    pub fn item_set_paths(&self, id: i64, dest: &str, temp: &str) {
        let _ = self.conn.lock().execute(
            "UPDATE import_items SET status = 'copying', dest_path = ?2, temp_path = ?3, updated_at = datetime('now') WHERE id = ?1",
            params![id, dest, temp],
        );
    }

    /// Items left mid-copy by a crash or power loss: (item id, temp path).
    pub fn interrupted_items(&self) -> Vec<(i64, String)> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT id, temp_path FROM import_items WHERE status = 'copying' AND temp_path IS NOT NULL")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    pub fn mark_interrupted_sessions(&self) {
        let _ = self
            .conn
            .lock()
            .execute("UPDATE import_sessions SET status = 'interrupted' WHERE status = 'running'", []);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::parse_exif_datetime;

    #[test]
    fn imported_roundtrip() {
        let c = Catalog::open_in_memory().unwrap();
        let meta = PhotoMeta { capture: parse_exif_datetime("2026:10:05 10:00:00"), ..Default::default() };
        c.record_imported(&NewImported { orig_name: "P1.ORF", size: 10, meta: &meta, blake3: "abc", dest_path: "/d/P1.ORF" })
            .unwrap();
        let rows = c.imported_by_name_size("P1.ORF", 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capture, meta.capture);
        assert!(c.imported_by_name_size("P1.ORF", 11).is_empty());
    }

    #[test]
    fn dest_walk_keeps_hash_only_if_unchanged() {
        let c = Catalog::open_in_memory().unwrap();
        let r = "/lib";
        c.dest_begin_walk(r).unwrap();
        c.dest_upsert_batch(r, &[("/lib/a".into(), "a".into(), 1, 1), ("/lib/b".into(), "b".into(), 2, 2)]).unwrap();
        c.dest_end_walk(r).unwrap();
        c.dest_set_hash("/lib/a", 1, 1, "h1");
        c.dest_set_hash("/lib/b", 2, 2, "h2");
        // Second walk: a unchanged, b modified, c new; nothing deleted yet.
        c.dest_begin_walk(r).unwrap();
        c.dest_upsert_batch(r, &[("/lib/a".into(), "a".into(), 1, 1), ("/lib/b".into(), "b".into(), 2, 3)]).unwrap();
        assert_eq!(c.dest_end_walk(r).unwrap(), 0);
        assert_eq!(c.dest_by_path("/lib/a").unwrap().blake3.as_deref(), Some("h1"));
        assert_eq!(c.dest_by_path("/lib/b").unwrap().blake3, None);
        // Third walk: a deleted.
        c.dest_begin_walk(r).unwrap();
        c.dest_upsert_batch(r, &[("/lib/b".into(), "b".into(), 2, 3)]).unwrap();
        assert_eq!(c.dest_end_walk(r).unwrap(), 1);
        assert!(c.dest_by_path("/lib/a").is_none());
    }

    #[test]
    fn journal_tracks_interrupted_items() {
        let c = Catalog::open_in_memory().unwrap();
        let s = c.session_start("/lib").unwrap();
        let a = c.item_start(s, "/card/a", "/lib/a", "/lib/.a.importing").unwrap();
        let b = c.item_start(s, "/card/b", "/lib/b", "/lib/.b.importing").unwrap();
        c.item_update(a, "done", None, Some("h"), None);
        assert_eq!(c.interrupted_items(), vec![(b, "/lib/.b.importing".to_string())]);
    }
}
