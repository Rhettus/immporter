//! Video metadata from QuickTime / MP4 (ISO-BMFF) files: capture time,
//! duration and frame size. Only box headers and the `moov` box are read,
//! so a multi-GB clip costs a few small reads, wherever `moov` sits.

use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, Offset, TimeZone};

use crate::readonly;

/// Largest `moov` we are willing to read (long clips have big sample tables).
const MAX_MOOV: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoMeta {
    /// Capture time as local wall-clock time.
    pub capture: Option<NaiveDateTime>,
    pub duration_secs: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

fn be32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|x| u32::from_be_bytes(x.try_into().unwrap()))
}

fn be64(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8).map(|x| u64::from_be_bytes(x.try_into().unwrap()))
}

/// Child boxes of a box body: (type, body).
fn children(buf: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut o = 0usize;
    while o + 8 <= buf.len() {
        let size = be32(buf, o).unwrap() as u64;
        let typ: [u8; 4] = buf[o + 4..o + 8].try_into().unwrap();
        let (header, size) = match size {
            1 => match be64(buf, o + 8) {
                Some(s) => (16usize, s),
                None => break,
            },
            0 => (8, (buf.len() - o) as u64),
            s => (8, s),
        };
        if size < header as u64 || o as u64 + size > buf.len() as u64 {
            break;
        }
        out.push((typ, &buf[o + header..o + size as usize]));
        o += size as usize;
    }
    out
}

/// Find and read the top-level `moov` box.
fn read_moov(path: &Path) -> Option<Vec<u8>> {
    let len = std::fs::metadata(path).ok()?.len();
    let mut off = 0u64;
    while off + 8 <= len {
        let h = readonly::read_at(path, off, 16).ok()?;
        let size = be32(&h, 0)? as u64;
        let (header, size) = match size {
            1 => (16, be64(&h, 8)?),
            0 => (8, len - off),
            s => (8, s),
        };
        if size < header {
            return None;
        }
        if &h[4..8] == b"moov" {
            if size > MAX_MOOV {
                return None;
            }
            return readonly::read_at(path, off + header, (size - header) as usize).ok();
        }
        off += size;
    }
    None
}

fn qt_epoch() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(1904, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap()
}

/// Parse `moov`: creation time (raw, as stored), duration and frame size.
fn parse_moov(moov: &[u8]) -> (Option<NaiveDateTime>, VideoMeta) {
    let mut created = None;
    let mut meta = VideoMeta::default();
    for (typ, body) in children(moov) {
        match &typ {
            b"mvhd" => {
                let v = body.first().copied().unwrap_or(0);
                let (ct, ts, dur) = if v == 1 {
                    (be64(body, 4), be32(body, 20), be64(body, 24))
                } else {
                    (be32(body, 4).map(u64::from), be32(body, 12), be32(body, 16).map(u64::from))
                };
                created = ct.filter(|&c| c > 0).map(|c| qt_epoch() + Duration::seconds(c as i64));
                if let (Some(ts), Some(d)) = (ts.filter(|&t| t > 0), dur) {
                    meta.duration_secs = Some(d as f64 / ts as f64);
                }
            }
            b"trak" => {
                for (t, tk) in children(body) {
                    if &t == b"tkhd" && tk.len() >= 8 {
                        let w = be32(tk, tk.len() - 8).unwrap() >> 16;
                        let h = be32(tk, tk.len() - 4).unwrap() >> 16;
                        if w > meta.width.unwrap_or(0) {
                            meta.width = Some(w);
                            meta.height = Some(h);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (created, meta)
}

/// QuickTime times are UTC by spec, but many cameras store local time.
/// Pick whichever reading lies closer to the file's modification time
/// (which cameras write in local time at the end of recording).
pub fn resolve_time(raw: NaiveDateTime, modified: Option<SystemTime>) -> NaiveDateTime {
    let as_utc = Local.from_utc_datetime(&raw).naive_local();
    let Some(m) = modified else { return as_utc };
    let mtime = DateTime::<Local>::from(m).naive_local();
    let offset = Local.from_utc_datetime(&raw).offset().fix().local_minus_utc();
    if offset == 0 {
        return raw;
    }
    let d_local = (mtime - raw).num_seconds().abs();
    let d_utc = (mtime - as_utc).num_seconds().abs();
    if d_utc < d_local {
        as_utc
    } else {
        raw
    }
}

pub fn read(path: &Path, modified: Option<SystemTime>) -> VideoMeta {
    let Some(moov) = read_moov(path) else { return VideoMeta::default() };
    let (raw, mut meta) = parse_moov(&moov);
    meta.capture = raw.map(|r| resolve_time(r, modified));
    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn reads_moov_at_end_and_front() {
        let end = read(&fixture("moov_end.mov"), None);
        assert_eq!((end.width, end.height), (Some(320), Some(240)));
        assert!((end.duration_secs.unwrap() - 2.0).abs() < 0.05);
        let front = read(&fixture("moov_front.mp4"), None);
        assert_eq!((front.width, front.height), (Some(640), Some(360)));
        assert!((front.duration_secs.unwrap() - 3.0).abs() < 0.05);
        let (raw, _) = parse_moov(&read_moov(&fixture("moov_front.mp4")).unwrap());
        assert_eq!(raw, Some(dt("2026-03-27 07:05:31")));
    }

    #[test]
    fn picks_utc_or_local_by_file_time() {
        let raw = dt("2026-03-27 14:05:31");
        let as_local_wall = Local.from_utc_datetime(&raw).naive_local();
        if as_local_wall == raw {
            return; // running in UTC: nothing to decide
        }
        // File written just after a camera stamped *local* time 14:05:31.
        let local_mtime: SystemTime = Local.from_local_datetime(&(raw + Duration::seconds(20))).unwrap().into();
        assert_eq!(resolve_time(raw, Some(local_mtime)), raw);
        // File written just after a camera stamped *UTC* 14:05:31.
        let utc_mtime: SystemTime = Local.from_utc_datetime(&(raw + Duration::seconds(20))).into();
        assert_eq!(resolve_time(raw, Some(utc_mtime)), as_local_wall);
    }

    #[test]
    fn non_video_files_give_nothing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.mov");
        std::fs::write(&p, b"not a movie at all").unwrap();
        assert_eq!(read(&p, None), VideoMeta::default());
    }
}
