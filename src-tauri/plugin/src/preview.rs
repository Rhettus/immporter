//! Fast previews: find the largest JPEG embedded in (a prefix of) a file,
//! downscale it and cache the result in the app cache directory.
//!
//! Almost every RAW format stores its preview JPEG before the sensor data, so
//! reading a few MB is enough. A generic JPEG marker walk works across formats
//! (ORF, ARW, NEF, CR2, CR3, RAF, DNG, …) without format-specific parsers.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageFormat};

pub const THUMB_MAX: u32 = 480;
/// First read size; most previews sit well inside this.
pub const PREFIX_SMALL: usize = 3 * 1024 * 1024;
/// Second read size when a preview starts in the prefix but is truncated.
pub const PREFIX_LARGE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JpegHit {
    pub start: usize,
    pub end: usize,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Default)]
pub struct JpegScan {
    pub best: Option<JpegHit>,
    /// A plausible JPEG started but ran past the end of the buffer.
    pub truncated: bool,
}

enum Walk {
    Complete(JpegHit),
    Truncated,
    Invalid,
}

/// Walk JPEG markers from an SOI at `start`. Returns the full extent of the
/// JPEG and its dimensions.
fn walk_jpeg(buf: &[u8], start: usize) -> Walk {
    let mut i = start + 2;
    let (mut w, mut h) = (0u16, 0u16);
    loop {
        if i + 4 > buf.len() {
            return Walk::Truncated;
        }
        if buf[i] != 0xFF {
            return Walk::Invalid;
        }
        let marker = buf[i + 1];
        match marker {
            0xFF => {
                i += 1; // fill byte
                continue;
            }
            0xD8 | 0x00 => return Walk::Invalid,
            0xD9 => return Walk::Invalid, // EOI before SOS
            0x01 | 0xD0..=0xD7 => {
                i += 2;
                continue;
            }
            _ => {}
        }
        let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        if len < 2 {
            return Walk::Invalid;
        }
        // SOF0..SOF15 except DHT(C4), JPG(C8), DAC(CC) carry dimensions.
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if i + 9 > buf.len() {
                return Walk::Truncated;
            }
            h = u16::from_be_bytes([buf[i + 5], buf[i + 6]]);
            w = u16::from_be_bytes([buf[i + 7], buf[i + 8]]);
        }
        if marker == 0xDA {
            if w == 0 || h == 0 {
                return Walk::Invalid;
            }
            // Entropy-coded data: scan for a marker that isn't stuffing or RST.
            let mut j = i + 2 + len;
            loop {
                if j + 1 >= buf.len() {
                    return Walk::Truncated;
                }
                if buf[j] == 0xFF {
                    let m = buf[j + 1];
                    if m == 0x00 || (0xD0..=0xD7).contains(&m) || m == 0xFF {
                        j += if m == 0xFF { 1 } else { 2 };
                        continue;
                    }
                    if m == 0xD9 {
                        return Walk::Complete(JpegHit { start, end: j + 2, width: w, height: h });
                    }
                    // Another segment (progressive JPEGs have several scans).
                    i = j;
                    break;
                }
                j += 1;
            }
            continue;
        }
        i += 2 + len;
    }
}

/// Find the JPEG with the largest pixel area inside `buf`, ignoring any that
/// start at `skip_offset_zero` (used for plain JPEG files, where offset 0 is the
/// full-size image itself).
pub fn find_largest_jpeg(buf: &[u8], skip_offset_zero: bool) -> JpegScan {
    let mut scan = JpegScan::default();
    let mut i = 0usize;
    while i + 3 < buf.len() {
        let Some(pos) = memchr3(&buf[i..]) else { break };
        let start = i + pos;
        if skip_offset_zero && start == 0 {
            i = start + 3;
            continue;
        }
        match walk_jpeg(buf, start) {
            Walk::Complete(hit) => {
                let area = |h: &JpegHit| h.width as u64 * h.height as u64;
                if scan.best.as_ref().map_or(true, |b| area(&hit) > area(b)) {
                    scan.best = Some(hit.clone());
                }
                // Skip past this JPEG; nested EXIF thumbnails are smaller anyway.
                i = hit.end;
            }
            Walk::Truncated => {
                scan.truncated = true;
                i = start + 3;
            }
            Walk::Invalid => i = start + 3,
        }
    }
    scan
}

/// Locate the byte sequence FF D8 FF.
fn memchr3(hay: &[u8]) -> Option<usize> {
    hay.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF])
}

/// Decode, orient and downscale a JPEG to a cached thumbnail (JPEG bytes).
pub fn make_thumb(jpeg: &[u8], orientation: Option<u16>) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg).map_err(|e| e.to_string())?;
    let img = img.thumbnail(THUMB_MAX, THUMB_MAX);
    let img = apply_orientation(img, orientation);
    let mut out = Vec::with_capacity(64 * 1024);
    let rgb = DynamicImage::ImageRgb8(img.to_rgb8());
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut out), 82);
    enc.encode_image(&rgb).map_err(|e| e.to_string())?;
    Ok(out)
}

fn apply_orientation(img: DynamicImage, orientation: Option<u16>) -> DynamicImage {
    match orientation.unwrap_or(1) {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

/// Thumbnail cache on local disk (never on the source volume).
pub struct ThumbCache {
    dir: PathBuf,
}

impl ThumbCache {
    pub fn new(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn key(volume_id: &str, path: &Path, size: u64, mtime: i64) -> String {
        let mut h = blake3::Hasher::new();
        h.update(volume_id.as_bytes());
        h.update(path.to_string_lossy().as_bytes());
        h.update(&size.to_le_bytes());
        h.update(&mtime.to_le_bytes());
        h.finalize().to_hex()[..32].to_string()
    }

    pub fn path_for(&self, key: &str) -> Option<PathBuf> {
        // Keys are hex we generated; reject anything else (protocol handler input).
        if key.len() != 32 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(self.dir.join(&key[..2]).join(format!("{key}.jpg")))
    }

    pub fn exists(&self, key: &str) -> bool {
        self.path_for(key).is_some_and(|p| p.exists())
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path_for(key)?).ok()
    }

    pub fn get_meta(&self, key: &str) -> Option<crate::metadata::PhotoMeta> {
        let p = self.path_for(key)?.with_extension("json");
        serde_json::from_slice(&std::fs::read(p).ok()?).ok()
    }

    pub fn put_meta(&self, key: &str, meta: &crate::metadata::PhotoMeta) -> std::io::Result<()> {
        let p = self.path_for(key).ok_or_else(|| std::io::Error::other("bad key"))?.with_extension("json");
        self.write_atomic(&p, &serde_json::to_vec(meta)?)
    }

    pub fn put(&self, key: &str, data: &[u8]) -> std::io::Result<()> {
        let p = self.path_for(key).ok_or_else(|| std::io::Error::other("bad key"))?;
        self.write_atomic(&p, data)
    }

    fn write_atomic(&self, p: &Path, data: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(p.parent().unwrap())?;
        let mut tmp = p.as_os_str().to_owned();
        tmp.push(".tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(tmp, p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 100, 50]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut out), 80)
            .encode_image(&img)
            .unwrap();
        out
    }

    #[test]
    fn finds_largest_embedded_jpeg() {
        let small = tiny_jpeg(32, 24);
        let big = tiny_jpeg(640, 480);
        let mut buf = vec![0x49, 0x49, 0x2A, 0x00];
        buf.extend(std::iter::repeat(7u8).take(1000));
        buf.extend(&small);
        buf.extend(std::iter::repeat(0xFFu8).take(10));
        let big_start = buf.len();
        buf.extend(&big);
        buf.extend(std::iter::repeat(3u8).take(5000));
        let scan = find_largest_jpeg(&buf, false);
        let best = scan.best.unwrap();
        assert_eq!((best.width, best.height), (640, 480));
        assert_eq!(best.start, big_start);
        assert_eq!(&buf[best.start..best.end], &big[..]);
        assert!(!scan.truncated);
        let thumb = make_thumb(&buf[best.start..best.end], Some(6)).unwrap();
        let t = image::load_from_memory(&thumb).unwrap();
        assert_eq!((t.width(), t.height()), (360, 480)); // rotated, max 480
    }

    #[test]
    fn reports_truncation() {
        let big = tiny_jpeg(640, 480);
        let mut buf = vec![0u8; 100];
        buf.extend(&big[..big.len() / 2]);
        let scan = find_largest_jpeg(&buf, false);
        assert!(scan.best.is_none());
        assert!(scan.truncated);
    }

    #[test]
    fn cache_rejects_bad_keys() {
        let dir = tempfile::tempdir().unwrap();
        let c = ThumbCache::new(dir.path().into()).unwrap();
        assert!(c.path_for("../../etc/passwd").is_none());
        let k = ThumbCache::key("v", Path::new("/a/b.orf"), 1, 2);
        c.put(&k, b"x").unwrap();
        assert_eq!(c.get(&k).unwrap(), b"x");
    }

    /// Run against real files: `SAMPLE_DIR=~/Pictures/2026/August cargo test -- --ignored samples`
    #[test]
    #[ignore]
    fn samples() {
        let dir = std::env::var("SAMPLE_DIR").expect("SAMPLE_DIR");
        for e in std::fs::read_dir(dir).unwrap().flatten().take(20) {
            let p = e.path();
            let ext = p.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            let Some(kind) = crate::metadata::kind_for_ext(&ext) else { continue };
            let t = std::time::Instant::now();
            let buf = crate::readonly::read_prefix(&p, PREFIX_SMALL).unwrap();
            let meta = crate::metadata::read_meta(&buf, kind);
            let scan = find_largest_jpeg(&buf, kind == crate::metadata::Kind::Jpeg);
            println!("{:?} {:?} {:?} truncated={} {:?}", p.file_name().unwrap(), meta, scan.best, scan.truncated, t.elapsed());
        }
    }
}
