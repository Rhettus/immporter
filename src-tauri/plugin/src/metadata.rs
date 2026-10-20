//! Capture date and camera identity, read from the first few MB of a file.
//!
//! Order of preference for the capture date: EXIF DateTimeOriginal (via
//! rawler for RAW files, kamadak-exif for JPEG/HEIC/TIFF), then the file's
//! creation time, then its modification time.

use std::io::Cursor;
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDateTime};
use serde::{Deserialize, Serialize};

pub const RAW_EXTS: &[&str] = &[
    "orf", "ori", "arw", "srf", "sr2", "cr2", "cr3", "crw", "nef", "nrw", "raf", "rw2", "rwl", "dng",
    "pef", "srw", "3fr", "fff", "iiq", "erf", "mef", "mos", "mrw", "x3f", "kdc", "dcr",
];
pub const JPEG_EXTS: &[&str] = &["jpg", "jpeg"];
pub const OTHER_PHOTO_EXTS: &[&str] = &["heic", "heif", "hif", "tif", "tiff", "png", "avif"];
pub const VIDEO_EXTS: &[&str] = &["mov", "mp4", "m4v", "mts", "m2ts", "avi"];
/// Files that travel with a video of the same name (Canon/GoPro thumbnails).
pub const VIDEO_SIDECAR_EXTS: &[&str] = &["thm"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Raw,
    Jpeg,
    Other,
    Video,
    /// Imported alongside a video of the same name, never on its own.
    Sidecar,
}

impl Kind {
    pub fn is_video_group(self) -> bool {
        matches!(self, Kind::Video | Kind::Sidecar)
    }
}

pub fn kind_for_ext(ext: &str) -> Option<Kind> {
    let e = ext.to_ascii_lowercase();
    if RAW_EXTS.contains(&e.as_str()) {
        Some(Kind::Raw)
    } else if JPEG_EXTS.contains(&e.as_str()) {
        Some(Kind::Jpeg)
    } else if OTHER_PHOTO_EXTS.contains(&e.as_str()) {
        Some(Kind::Other)
    } else if VIDEO_EXTS.contains(&e.as_str()) {
        Some(Kind::Video)
    } else if VIDEO_SIDECAR_EXTS.contains(&e.as_str()) {
        Some(Kind::Sidecar)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DateSource {
    Exif,
    Created,
    Modified,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PhotoMeta {
    pub capture: Option<NaiveDateTime>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub image_number: Option<u32>,
    pub orientation: Option<u16>,
    /// Videos only.
    pub duration_secs: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl PhotoMeta {
    /// Same shot according to EXIF: identical capture time and, where both
    /// sides know them, identical camera serial and image number.
    pub fn same_shot(&self, other: &PhotoMeta) -> bool {
        fn compatible<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
        }
        self.capture.is_some()
            && self.capture == other.capture
            && compatible(&self.serial, &other.serial)
            && compatible(&self.image_number, &other.image_number)
    }
}

pub fn parse_exif_datetime(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim().trim_end_matches('\0');
    NaiveDateTime::parse_from_str(s, "%Y:%m:%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y:%m:%d %H:%M:%S%.f"))
        .ok()
        // Cameras with unset clocks write all zeros, which fails to parse above.
        .filter(|d| d.and_utc().timestamp() > 0)
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim().trim_end_matches('\0').trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Read metadata from a buffer holding (a prefix of) the file.
pub fn read_meta(buf: &[u8], kind: Kind) -> PhotoMeta {
    let mut meta = PhotoMeta::default();
    if kind == Kind::Raw {
        if let Some(m) = read_rawler(buf) {
            meta = m;
        }
    }
    if meta.capture.is_none() {
        if let Some(m) = read_kamadak(buf) {
            // Keep any identity fields rawler already found.
            meta.capture = m.capture;
            meta.make = meta.make.or(m.make);
            meta.model = meta.model.or(m.model);
            meta.serial = meta.serial.or(m.serial);
            meta.image_number = meta.image_number.or(m.image_number);
            meta.orientation = meta.orientation.or(m.orientation);
        }
    }
    meta
}

fn read_rawler(buf: &[u8]) -> Option<PhotoMeta> {
    use rawler::decoders::RawDecodeParams;
    use rawler::rawsource::RawSource;
    let src = RawSource::new_from_slice(buf);
    // rawler can panic on truncated/unknown data; never let that take a worker down.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let decoder = rawler::get_decoder(&src).ok()?;
        let md = decoder.raw_metadata(&src, &RawDecodeParams::default()).ok()?;
        let e = &md.exif;
        Some(PhotoMeta {
            capture: e
                .date_time_original
                .as_deref()
                .or(e.create_date.as_deref())
                .and_then(parse_exif_datetime),
            make: non_empty(&md.make),
            model: non_empty(&md.model),
            serial: e.serial_number.as_deref().and_then(non_empty),
            image_number: e.image_number,
            orientation: e.orientation,
            ..Default::default()
        })
    }))
    .ok()
    .flatten()
}

fn read_kamadak(buf: &[u8]) -> Option<PhotoMeta> {
    use exif::{In, Tag, Value};
    let reader = exif::Reader::new();
    let ex = reader
        .read_from_container(&mut Cursor::new(buf))
        .or_else(|_| reader.read_raw(buf.to_vec()))
        .ok()?;
    let string = |tag: Tag| -> Option<String> {
        let f = ex.get_field(tag, In::PRIMARY)?;
        match &f.value {
            Value::Ascii(v) => v.first().and_then(|b| non_empty(&String::from_utf8_lossy(b))),
            _ => non_empty(&f.display_value().to_string()),
        }
    };
    let uint = |tag: Tag| ex.get_field(tag, In::PRIMARY).and_then(|f| f.value.get_uint(0));
    Some(PhotoMeta {
        capture: string(Tag::DateTimeOriginal)
            .or_else(|| string(Tag::DateTimeDigitized))
            .or_else(|| string(Tag::DateTime))
            .and_then(|s| parse_exif_datetime(&s)),
        make: string(Tag::Make),
        model: string(Tag::Model),
        serial: string(Tag::BodySerialNumber),
        image_number: ex.get_field(Tag(exif::Context::Tiff, 0x9211), In::PRIMARY).and_then(|f| f.value.get_uint(0)),
        orientation: uint(Tag::Orientation).map(|v| v as u16),
        ..Default::default()
    })
}

/// Resolve the date used for folder/file naming.
pub fn capture_or_file_time(
    meta: &PhotoMeta,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
) -> Option<(NaiveDateTime, DateSource)> {
    let local = |t: SystemTime| DateTime::<Local>::from(t).naive_local();
    meta.capture
        .map(|c| (c, DateSource::Exif))
        .or_else(|| created.map(|t| (local(t), DateSource::Created)))
        .or_else(|| modified.map(|t| (local(t), DateSource::Modified)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exif_dates() {
        assert_eq!(
            parse_exif_datetime("2026:08:20 07:37:10").unwrap().to_string(),
            "2026-08-20 07:37:10"
        );
        assert!(parse_exif_datetime("0000:00:00 00:00:00").is_none());
        assert!(parse_exif_datetime("").is_none());
    }

    #[test]
    fn same_shot_requires_matching_identity() {
        let a = PhotoMeta {
            capture: parse_exif_datetime("2026:08:20 07:37:10"),
            serial: Some("X".into()),
            image_number: Some(5),
            ..Default::default()
        };
        let mut b = a.clone();
        assert!(a.same_shot(&b));
        b.serial = None;
        assert!(a.same_shot(&b));
        b.image_number = Some(6);
        assert!(!a.same_shot(&b));
        assert!(!PhotoMeta::default().same_shot(&PhotoMeta::default()));
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_for_ext("ORF"), Some(Kind::Raw));
        assert_eq!(kind_for_ext("ori"), Some(Kind::Raw));
        assert_eq!(kind_for_ext("JPG"), Some(Kind::Jpeg));
        assert_eq!(kind_for_ext("heic"), Some(Kind::Other));
        assert_eq!(kind_for_ext("MOV"), Some(Kind::Video));
        assert_eq!(kind_for_ext("thm"), Some(Kind::Sidecar));
        assert_eq!(kind_for_ext("xmp"), None);
    }
}
