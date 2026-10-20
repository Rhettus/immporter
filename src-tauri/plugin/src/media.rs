//! `media://<gen>/<item>/<file>`: streams a source video to the web view so
//! it can decode a frame for the thumbnail. Only files of the current scan
//! session are reachable (never arbitrary paths), always opened read-only,
//! and served in ranges so only the parts the decoder needs are read.

use std::io::{Read, Seek, SeekFrom};

use crate::readonly;
use crate::scan::Session;

/// Most bytes returned for one open-ended range request.
const MAX_CHUNK: u64 = 4 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub struct MediaResponse {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

fn not_found() -> MediaResponse {
    MediaResponse { status: 404, headers: vec![], body: vec![] }
}

fn content_type(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mts" | "m2ts" => "video/mp2t",
        "avi" => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

/// Parse `bytes=a-b`, `bytes=a-` or `bytes=-n` into an inclusive range.
pub fn parse_range(header: Option<&str>, len: u64) -> Option<(u64, u64)> {
    let spec = header?.trim().strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (len.saturating_sub(n), len.checked_sub(1)?)
        }
        (s, "") => {
            let s: u64 = s.parse().ok()?;
            (s, (s + MAX_CHUNK - 1).min(len.checked_sub(1)?))
        }
        (s, e) => (s.parse().ok()?, e.parse::<u64>().ok()?.min(len.checked_sub(1)?)),
    };
    (start <= end && start < len).then_some((start, end))
}

/// Serve `path` (`/<gen>/<item>/<file>`) from the current session.
pub fn serve(session: Option<&Session>, path: &str, range: Option<&str>) -> MediaResponse {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let [gen, item, file] = parts.as_slice() else { return not_found() };
    let (Ok(gen), Ok(item), Ok(file)) = (gen.parse::<u64>(), item.parse::<usize>(), file.parse::<usize>()) else {
        return not_found();
    };
    let Some(s) = session.filter(|s| s.gen == gen) else { return not_found() };
    let Some(f) = s.items.get(item).and_then(|it| it.files.get(file)) else { return not_found() };
    if f.kind != crate::metadata::Kind::Video {
        return not_found();
    }
    let Ok(mut fh) = readonly::open_read(&f.path, false) else { return not_found() };
    let len = fh.metadata().map(|m| m.len()).unwrap_or(0);
    let (start, end) = parse_range(range, len).unwrap_or((0, (MAX_CHUNK - 1).min(len.saturating_sub(1))));
    let mut body = Vec::with_capacity((end - start + 1) as usize);
    if fh.seek(SeekFrom::Start(start)).is_err() || fh.take(end - start + 1).read_to_end(&mut body).is_err() {
        return not_found();
    }
    MediaResponse {
        status: 206,
        headers: vec![
            ("Content-Type", content_type(&f.ext).to_string()),
            ("Accept-Ranges", "bytes".into()),
            ("Content-Range", format!("bytes {start}-{}/{len}", start + body.len() as u64 - 1)),
            ("Content-Length", body.len().to_string()),
        ],
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;
    use std::sync::Arc;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(Some("bytes=0-9"), 100), Some((0, 9)));
        assert_eq!(parse_range(Some("bytes=90-"), 100), Some((90, 99)));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Some((90, 99)));
        assert_eq!(parse_range(Some("bytes=50-500"), 100), Some((50, 99)));
        assert_eq!(parse_range(Some("bytes=0-"), 10 * MAX_CHUNK), Some((0, MAX_CHUNK - 1)));
        assert_eq!(parse_range(Some("bytes=200-"), 100), None);
        assert_eq!(parse_range(Some("items=0-1"), 100), None);
        assert_eq!(parse_range(None, 100), None);
    }

    #[test]
    fn serves_only_session_videos() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("DCIM/100");
        std::fs::create_dir_all(&dir).unwrap();
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("A.MOV"), &data).unwrap();
        std::fs::write(dir.join("B.JPG"), b"jpeg").unwrap();
        let root = scan::source_root(d.path());
        let s = Session::new("v".into(), root.clone(), scan::pair(scan::walk(&root, "v")), Arc::new(|_, _| {}));
        let vid = s.items.iter().position(|i| i.files[0].name == "A.MOV").unwrap();
        let img = s.items.iter().position(|i| i.files[0].name == "B.JPG").unwrap();

        let r = serve(Some(&s), &format!("/{}/{vid}/0", s.gen), Some("bytes=10-19"));
        assert_eq!(r.status, 206);
        assert_eq!(r.body, &data[10..20]);
        assert!(r.headers.contains(&("Content-Range", "bytes 10-19/1000".into())));
        assert!(r.headers.contains(&("Content-Type", "video/quicktime".into())));

        // Wrong session, non-video, out-of-range ids, junk paths.
        assert_eq!(serve(Some(&s), &format!("/{}/{vid}/0", s.gen + 1), None).status, 404);
        assert_eq!(serve(Some(&s), &format!("/{}/{img}/0", s.gen), None).status, 404);
        assert_eq!(serve(Some(&s), &format!("/{}/999/0", s.gen), None).status, 404);
        assert_eq!(serve(Some(&s), "/../../etc/passwd", None).status, 404);
        assert_eq!(serve(None, &format!("/{}/{vid}/0", s.gen), None).status, 404);
    }
}
