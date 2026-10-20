//! The only module allowed to open files on a source volume (SD card).
//!
//! Every function here opens files strictly read-only. Nothing in this module
//! (or anywhere else in the crate) may create, modify, rename or delete a source
//! file. `tests::no_write_opens_outside_import` enforces that write-capable
//! opens only happen in `import.rs`.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

pub const HASH_BUF: usize = 4 * 1024 * 1024;

/// Open a file read-only. On unix the open refuses to follow a final symlink
/// and, on Linux, tries not to update atime. `nocache` asks the OS to not serve
/// / retain the data in the page cache where supported (macOS `F_NOCACHE`,
/// Linux `POSIX_FADV_DONTNEED`); this is best effort.
pub fn open_read(path: &Path, nocache: bool) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[allow(unused_mut)]
        let mut flags = libc::O_NOFOLLOW;
        #[cfg(target_os = "linux")]
        {
            flags |= libc::O_NOATIME;
        }
        opts.custom_flags(flags);
    }
    let file = match opts.open(path) {
        Ok(f) => f,
        // O_NOATIME is only permitted for the file owner; retry without it.
        #[cfg(target_os = "linux")]
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?
        }
        Err(e) => return Err(e),
    };
    if nocache {
        drop_cache(&file);
    }
    Ok(file)
}

/// Best-effort: stop the OS from caching this file's pages, so a following read
/// comes from the physical medium. Used on both the write and verify side of
/// an import so the verify read is not served from RAM.
pub fn drop_cache(file: &File) {
    #[cfg(target_os = "macos")]
    unsafe {
        use std::os::unix::io::AsRawFd;
        libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::io::AsRawFd;
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = file;
}

/// Read at most `n` bytes from the start of a file.
pub fn read_prefix(path: &Path, n: usize) -> io::Result<Vec<u8>> {
    let file = open_read(path, false)?;
    let mut buf = Vec::with_capacity(n.min(file.metadata().map(|m| m.len() as usize).unwrap_or(n)));
    file.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Read up to `len` bytes at `offset`.
pub fn read_at(path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut file = open_read(path, false)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity(len);
    file.take(len as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Hash a whole file with BLAKE3. Returns the hash and the number of bytes read.
pub fn hash_file(path: &Path, nocache: bool) -> io::Result<(blake3::Hash, u64)> {
    let mut file = open_read(path, nocache)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_BUF];
    let mut total = 0u64;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hasher.finalize(), total))
}

/// One pass over a file computing BLAKE3 (hex, used for import verification)
/// and SHA-1 (base64, as Immich stores it). Returns (blake3, sha1, bytes).
pub fn hash_file_both(path: &Path, nocache: bool) -> io::Result<(String, String, u64)> {
    use base64::Engine;
    use sha1::Digest;
    let mut file = open_read(path, nocache)?;
    let mut b3 = blake3::Hasher::new();
    let mut s1 = sha1::Sha1::new();
    let mut buf = vec![0u8; HASH_BUF];
    let mut total = 0u64;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        b3.update(&buf[..n]);
        s1.update(&buf[..n]);
        total += n as u64;
    }
    let sha1 = base64::engine::general_purpose::STANDARD.encode(s1.finalize());
    Ok((b3.finalize().to_hex().to_string(), sha1, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_blake3() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.bin");
        let data: Vec<u8> = (0..10_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let (h, n) = hash_file(&p, true).unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(h, blake3::hash(&data));
        assert_eq!(read_prefix(&p, 10).unwrap(), &data[..10]);
        let (b3, s1, n2) = hash_file_both(&p, false).unwrap();
        assert_eq!((b3, n2), (h.to_hex().to_string(), n));
        assert_eq!(hash_file_both(&dir.path().join("none"), false).is_err(), true);
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(hash_file_both(&p, false).unwrap().1, "qZk+NkcGgWq6PiVxeFDCbJzQ2J0="); // sha1("abc")
        let _ = s1;
    }

    #[test]
    fn works_on_read_only_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ro.bin");
        std::fs::write(&p, b"hello").unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&p, perm).unwrap();
        assert_eq!(hash_file(&p, false).unwrap().0, blake3::hash(b"hello"));
    }

    /// Guard rail: write-capable file APIs may only be used by the import
    /// pipeline (which writes to the destination) and the catalog/cache code
    /// (which write to the app data dir). Source paths only ever flow through
    /// this module.
    #[test]
    fn no_write_opens_outside_allowed_modules() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let allowed = ["import.rs", "preview.rs"];
        let needles = [
            ".write(true)",
            ".append(true)",
            "File::create",
            "fs::write",
            "fs::remove_file",
            "fs::rename",
            "fs::copy",
            "set_permissions",
            "set_file_times",
            "set_file_mtime",
        ];
        for entry in std::fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if allowed.contains(&name.as_str()) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Test modules may create fixtures.
            let text = text.split("#[cfg(test)]").next().unwrap();
            for n in needles {
                assert!(!text.contains(n), "{name} uses write API `{n}`");
            }
        }
    }
}
